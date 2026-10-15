//! Quest client application runtime coordinating OpenXR, Vulkan, decoder, and transport.

use std::time::Instant;

use linux_quest_protocol::packet::Packet;

use crate::config::ClientConfig;
use crate::decoder::{create_decoder, DecoderStats, HardwareVideoDecoder};
use crate::error::ClientResult;
use crate::graphics::VulkanContext;
use crate::hud::DebugHud;
use crate::openxr::{FrameLoopEngine, FramePacingMetrics, OpenXrContext};
use crate::receiver::ClientReceiver;
use crate::spatial::{ScreenTransform, SpatialLayoutStore, SpatialScreenLayout, Vec3};

/// High-performance client runtime managing the VR spatial desktop session.
pub struct QuestClientRuntime {
    config: ClientConfig,
    decoder: Box<dyn HardwareVideoDecoder>,
    openxr_context: OpenXrContext,
    vulkan_context: VulkanContext,
    receiver: ClientReceiver,
    frame_loop: FrameLoopEngine,
    hud: DebugHud,
    screen_transform: ScreenTransform,
    layout_store: Option<SpatialLayoutStore>,
    is_running: bool,
}

impl QuestClientRuntime {
    /// Initializes the Quest client streaming runtime.
    pub fn new(config: ClientConfig) -> ClientResult<Self> {
        config.validate()?;

        let mut vulkan_context = VulkanContext::new_simulated();

        // Try live OpenXR context, falling back to simulated for CI and desktop testing
        let mut openxr_context = match OpenXrContext::try_new(&config, &mut vulkan_context) {
            Ok(ctx) => ctx,
            Err(e) => {
                #[cfg(target_os = "android")]
                if !config.force_mock_decoder {
                    return Err(e);
                }
                tracing::info!(error = ?e, "Using simulated OpenXR context");
                OpenXrContext::new_simulated(&config)
            }
        };

        let (screen_transform, layout_store) = if let Some(path) = &config.spatial_layout_path {
            let store = SpatialLayoutStore::new(path);
            let mut layout = store.load_or_default();
            let tf = layout.get_or_create(0).clone();
            (tf, Some(store))
        } else {
            let mut tf = ScreenTransform::for_display(0);
            tf.display_mode = config.display_mode;
            tf.distance = config.quad_distance;
            tf.quad_size = config.quad_size;
            tf.curve_radius = config.cylinder_radius;
            tf.curve_angle = config.cylinder_central_angle;
            tf.aspect_ratio = config.cylinder_aspect_ratio;
            tf.clamp();
            (tf, None)
        };

        openxr_context.set_screen_transform(screen_transform.clone());

        #[cfg(target_os = "android")]
        let surface_ptr = openxr_context
            .surface_window()
            .map(|win| win.ptr().as_ptr() as *mut std::ffi::c_void);
        #[cfg(not(target_os = "android"))]
        let surface_ptr = None;

        let decoder = create_decoder(
            &config,
            config.stream_width,
            config.stream_height,
            surface_ptr,
        )?;

        let receiver = ClientReceiver::new(&config);
        let frame_loop = FrameLoopEngine::new();
        let hud = DebugHud::new(512, 256);

        Ok(Self {
            config,
            decoder,
            openxr_context,
            vulkan_context,
            receiver,
            frame_loop,
            hud,
            screen_transform,
            layout_store,
            is_running: true,
        })
    }

    /// Processes a single frame iteration of the VR display presentation loop at current time.
    pub fn step_frame(&mut self) -> ClientResult<FramePacingMetrics> {
        self.step_frame_at(Instant::now())
    }

    /// Processes a single frame iteration with an explicit timestamp (for testing/simulation).
    pub fn step_frame_at(&mut self, now: Instant) -> ClientResult<FramePacingMetrics> {
        let render_start = Instant::now();

        // 1. Poll OpenXR events
        self.openxr_context.poll_events()?;

        // If running in live mode, ensure the OpenXR session has transitioned to running
        if !self.openxr_context.is_simulated() && !self.openxr_context.is_session_running() {
            return Ok(FramePacingMetrics::default());
        }

        // 2. Pace rendering loop to target headset refresh rate (72Hz, 90Hz, 120Hz)
        let pacing = self.frame_loop.wait_frame_pacing(&self.openxr_context)?;
        let display_time = openxr::Time::from_nanos(pacing.predicted_display_time);
        let monotonic_display_ns = self
            .openxr_context
            .convert_time_to_monotonic_ns(display_time);

        // If OpenXR requests not to render (should_render == false), end frame with empty layers
        if !pacing.should_render {
            self.openxr_context.end_frame_empty(display_time)?;
            return Ok(pacing);
        }

        // 3. Pop completed video frames whose playout delay has elapsed at the current moment
        let playout_instant = if self.openxr_context.is_simulated() {
            now
        } else {
            Instant::now()
        };

        if let Ok(ready_frames) = self.receiver.pop_ready_frames(playout_instant) {
            for frame in ready_frames {
                if let Err(e) = self.decoder.queue_input_buffer(
                    &frame.bitstream,
                    frame.meta.pts_us,
                    frame.meta.is_keyframe,
                ) {
                    tracing::warn!(
                        error = ?e,
                        frame_id = frame.meta.frame_id,
                        "Failed to queue video frame into decoder"
                    );
                }
            }
        }

        // 4. Dequeue and release hardware-decoded video frames direct to OpenXR surface
        let mut latest_frame = None;
        while let Ok(Some(frame)) = self.decoder.dequeue_output_buffer(0) {
            if let Some(prev) = latest_frame.replace(frame) {
                // Drop older frame without rendering to prevent backlog and multiple frames stamped at same display time
                let _ = self.decoder.release_output_buffer(prev.buffer_index, false);
            }
        }
        if let Some(frame) = latest_frame {
            let _ = self
                .decoder
                .release_output_buffer_at_time(frame.buffer_index, monotonic_display_ns);
        }

        // 5. Update debug HUD telemetry
        let decoder_stats = self.decoder.stats();
        self.hud.update(
            &self.config,
            &pacing,
            &decoder_stats,
            self.receiver.packets_received(),
            self.receiver.fec_recovered_count(),
            self.receiver.packets_lost_count(),
        );

        // 6. Submit composition layers to OpenXR compositor for timewarp display
        let hud_rgba = if self.config.enable_hud {
            Some(self.hud.render_to_rgba())
        } else {
            None
        };

        let present_res = self.openxr_context.render_and_present_layers(
            display_time,
            &self.config,
            &self.vulkan_context,
            hud_rgba.as_deref(),
        );

        if let Err(e) = present_res {
            tracing::warn!(error = ?e, "Layer presentation failed");
            return Err(e);
        }

        // 7. Record render duration
        let render_duration = render_start.elapsed().as_micros() as u64;
        self.frame_loop.record_render_duration(render_duration);

        Ok(pacing)
    }

    /// Ingests an incoming network or USB packet into the jitter buffer.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        self.receiver.ingest_packet(packet, now);
    }

    /// Requests a dynamic display refresh rate change.
    pub fn set_target_refresh_rate(&mut self, target_rate: f32) {
        self.config.target_refresh_rate = target_rate;
        let _ = self.openxr_context.request_refresh_rate(target_rate);
    }

    /// Returns the current HUD telemetry text.
    pub fn hud_text(&self) -> String {
        self.hud.formatted_text()
    }

    /// Returns the decoder statistics.
    pub fn decoder_stats(&self) -> DecoderStats {
        self.decoder.stats()
    }

    /// Whether the client runtime is actively running.
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Whether the OpenXR session has transitioned to running.
    pub fn is_session_running(&self) -> bool {
        self.openxr_context.is_session_running()
    }

    /// Returns a reference to the Vulkan context.
    pub fn vulkan_context(&self) -> &VulkanContext {
        &self.vulkan_context
    }

    /// Polls OpenXR runtime events and updates the session state machine.
    pub fn poll_events(&mut self) -> ClientResult<()> {
        self.openxr_context.poll_events()
    }

    /// Whether the client runtime or OpenXR session has requested termination.
    pub fn should_exit(&self) -> bool {
        !self.is_running || self.openxr_context.should_exit()
    }

    /// Returns a reference to the active spatial screen transform.
    pub fn screen_transform(&self) -> &ScreenTransform {
        &self.screen_transform
    }

    /// Returns a mutable reference to the active spatial screen transform.
    pub fn screen_transform_mut(&mut self) -> &mut ScreenTransform {
        &mut self.screen_transform
    }

    /// Sets the active spatial screen transform and propagates it to OpenXR context.
    pub fn set_screen_transform(&mut self, transform: ScreenTransform) {
        self.screen_transform = transform.clone();
        self.openxr_context.set_screen_transform(transform);
    }

    /// Translates the virtual screen by `delta` in meters.
    pub fn translate_screen(&mut self, delta: Vec3) {
        self.screen_transform.translate(delta);
        self.openxr_context
            .set_screen_transform(self.screen_transform.clone());
    }

    /// Rotates the virtual screen by incremental Euler angles (pitch, yaw, roll) in radians.
    pub fn rotate_screen(&mut self, pitch: f32, yaw: f32, roll: f32) {
        self.screen_transform.rotate(pitch, yaw, roll);
        self.openxr_context
            .set_screen_transform(self.screen_transform.clone());
    }

    /// Sets the viewing distance of the virtual screen in meters.
    pub fn set_screen_distance(&mut self, distance: f32) {
        self.screen_transform.set_distance(distance);
        self.openxr_context
            .set_screen_transform(self.screen_transform.clone());
    }

    /// Pushes or pulls the virtual screen relative to the user by `delta_distance` meters.
    pub fn push_pull_screen(&mut self, delta_distance: f32) {
        self.screen_transform.push_pull(delta_distance);
        self.openxr_context
            .set_screen_transform(self.screen_transform.clone());
    }

    /// Sets the curved cylinder curvature radius and central arc angle.
    pub fn set_screen_curve(&mut self, radius: f32, angle: f32) {
        self.screen_transform.set_curve(radius, angle);
        self.openxr_context
            .set_screen_transform(self.screen_transform.clone());
    }

    /// Returns whether Quest Passthrough (`XR_FB_passthrough`) is supported.
    pub fn is_passthrough_supported(&self) -> bool {
        self.openxr_context.is_passthrough_supported()
    }

    /// Returns whether passthrough is currently active/enabled.
    pub fn is_passthrough_enabled(&self) -> bool {
        self.openxr_context.is_passthrough_enabled()
    }

    /// Enables or disables Quest Passthrough.
    pub fn set_passthrough_enabled(&mut self, enabled: bool) -> ClientResult<()> {
        self.openxr_context.set_passthrough_enabled(enabled)
    }

    /// Toggles Quest Passthrough on/off and returns the new enabled state.
    pub fn toggle_passthrough(&mut self) -> ClientResult<bool> {
        let current = self.is_passthrough_enabled();
        let new_state = !current;
        self.set_passthrough_enabled(new_state)?;
        Ok(new_state)
    }

    /// Returns the active OpenXR environment blend mode.
    pub fn passthrough_blend_mode(&self) -> openxr::EnvironmentBlendMode {
        self.openxr_context.passthrough().blend_mode()
    }

    /// Saves the current spatial screen layout to disk at the given path.
    pub fn save_spatial_layout(&self, path: impl AsRef<std::path::Path>) -> ClientResult<()> {
        let store = SpatialLayoutStore::new(path.as_ref());
        let mut layout = SpatialScreenLayout::default();
        layout.insert(0, self.screen_transform.clone());
        store.save(&layout)
    }

    /// Restores a spatial screen layout from disk at the given path for the specified display ID.
    pub fn load_spatial_layout(
        &mut self,
        path: impl AsRef<std::path::Path>,
        display_id: u32,
    ) -> ClientResult<()> {
        let store = SpatialLayoutStore::new(path.as_ref());
        let layout = store.load()?;
        if let Some(transform) = layout.get(display_id) {
            self.set_screen_transform(transform.clone());
        }
        Ok(())
    }

    /// Returns the active layout store if one was configured.
    pub fn layout_store(&self) -> Option<&SpatialLayoutStore> {
        self.layout_store.as_ref()
    }

    /// Signals the runtime to gracefully shut down.
    pub fn stop(&mut self) {
        self.is_running = false;
        let _ = self.decoder.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};
    use std::time::Duration;

    #[test]
    fn test_client_runtime_lifecycle_and_packet_flow() {
        let config = ClientConfig::default();
        let mut runtime = QuestClientRuntime::new(config).unwrap();
        assert!(runtime.is_running());

        // Step a frame
        let pacing = runtime.step_frame().unwrap();
        assert!(pacing.predicted_display_time > 0);

        // Feed a complete video frame packet
        let meta = VideoChunkMeta {
            frame_id: 1,
            chunk_index: 0,
            total_chunks: 1,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 1920,
            height: 1080,
            fps: 90,
            pts_us: 50_000,
        };
        let chunk = VideoChunk::new(meta, Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]));
        let chunk_bytes = chunk.serialize().unwrap();
        let header = linux_quest_protocol::packet::PacketHeader::new(
            linux_quest_protocol::packet::PacketType::VideoFrameChunk,
            1,
            1,
            50_000,
            &chunk_bytes,
        );
        let packet = Packet::new(header, chunk_bytes);

        let arrival = Instant::now();
        runtime.ingest_packet(packet, arrival);

        // After jitter playout delay (+10ms), step frame to ingest into decoder and render
        let playout_time = arrival + Duration::from_millis(10);
        let _ = runtime.step_frame_at(playout_time).unwrap();

        let stats = runtime.decoder_stats();
        assert_eq!(stats.frames_queued, 1);
        assert_eq!(stats.frames_rendered, 1);

        let hud_text = runtime.hud_text();
        assert!(hud_text.contains("FPS:"));

        runtime.stop();
        assert!(!runtime.is_running());
    }

    #[test]
    fn test_client_runtime_refresh_rate_switch() {
        let config = ClientConfig::default();
        let mut runtime = QuestClientRuntime::new(config).unwrap();

        runtime.set_target_refresh_rate(120.0);
        assert_eq!(
            runtime
                .openxr_context
                .refresh_rate_manager()
                .unwrap()
                .current_rate(),
            120.0
        );
    }
}
