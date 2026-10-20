//! Quest client application runtime coordinating OpenXR, Vulkan, decoder, and transport.

use std::time::Instant;

use linux_quest_protocol::packet::Packet;

use crate::audio::{AudioReceiver, AudioReceiverConfig};
use crate::config::ClientConfig;
use crate::decoder::{create_decoder, DecoderStats, HardwareVideoDecoder};
use crate::error::ClientResult;
use crate::graphics::VulkanContext;
use crate::hud::DebugHud;
use crate::openxr::{FrameLoopEngine, FramePacingMetrics, OpenXrContext};
use crate::receiver::ClientReceiver;
use crate::spatial::{
    GrabStateMachine, HandGestureDetector, Handedness, InteractionEvent, InteractionInput,
    InteractionState, ScreenTransform, SpatialLayoutStore, SpatialScreenLayout, Vec3,
};

/// High-performance client runtime managing the VR spatial desktop session.
pub struct QuestClientRuntime {
    config: ClientConfig,
    decoder: Box<dyn HardwareVideoDecoder>,
    openxr_context: OpenXrContext,
    vulkan_context: VulkanContext,
    receiver: ClientReceiver,
    audio_receiver: AudioReceiver,
    frame_loop: FrameLoopEngine,
    hud: DebugHud,
    screen_transform: ScreenTransform,
    layout_store: Option<SpatialLayoutStore>,
    active_layout: SpatialScreenLayout,
    interaction_state_machine: GrabStateMachine,
    gesture_detector: HandGestureDetector,
    is_running: bool,
    swapchains_ready: bool,
    loss_recovery: crate::receiver::loss_recovery::LossRecovery,
    overlay_scene: crate::openxr::overlay::OverlayScene,
    keyframe_requester: Option<Box<dyn Fn() + Send>>,
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

        let (screen_transform, layout_store, active_layout) =
            if let Some(path) = &config.spatial_layout_path {
                let store = SpatialLayoutStore::new(path);
                let mut layout = store.load_or_default();
                let tf = layout.get_or_create(0).clone();
                (tf, Some(store), layout)
            } else {
                let mut tf = ScreenTransform::for_display(0);
                tf.display_mode = config.display_mode;
                tf.distance = config.quad_distance;
                tf.quad_size = config.quad_size;
                tf.curve_radius = config.cylinder_radius;
                tf.curve_angle = config.cylinder_central_angle;
                tf.aspect_ratio = config.cylinder_aspect_ratio;
                tf.clamp();
                let mut layout = SpatialScreenLayout::default();
                layout.insert(0, tf.clone());
                (tf, None, layout)
            };

        openxr_context.set_screen_transform(screen_transform.clone());

        // On Horizon OS the Android surface swapchain (the decoder's output surface) can only be
        // created once the session is running, so a live context defers it: until
        // `create_swapchains` runs, a mock decoder stands in and `step_frame` does nothing.
        let swapchains_ready = openxr_context.swapchains_created();
        let decoder = if swapchains_ready {
            Self::build_decoder(&config, &openxr_context)?
        } else {
            let placeholder = ClientConfig {
                force_mock_decoder: true,
                ..config.clone()
            };
            create_decoder(
                &placeholder,
                config.stream_width,
                config.stream_height,
                None,
            )?
        };

        let receiver = ClientReceiver::new(&config);
        let audio_receiver = AudioReceiver::new(AudioReceiverConfig::default());
        let frame_loop = FrameLoopEngine::new();
        let hud = DebugHud::new(512, 256);
        let interaction_state_machine = GrabStateMachine::new();
        let gesture_detector = HandGestureDetector::new();

        Ok(Self {
            config,
            decoder,
            openxr_context,
            vulkan_context,
            receiver,
            audio_receiver,
            frame_loop,
            hud,
            screen_transform,
            layout_store,
            active_layout,
            interaction_state_machine,
            gesture_detector,
            is_running: true,
            swapchains_ready,
            loss_recovery: crate::receiver::loss_recovery::LossRecovery::new(),
            overlay_scene: Default::default(),
            keyframe_requester: None,
        })
    }

    /// Called (rate limited) whenever video is lost and a keyframe is needed. On Android this
    /// sends [`linux_quest_protocol::InputEvent::KeyframeRequest`] on the UDP input channel.
    pub fn set_keyframe_requester(&mut self, request: Box<dyn Fn() + Send>) {
        self.keyframe_requester = Some(request);
    }

    /// Loss-recovery state between the reassembler and the decoder.
    pub fn loss_recovery(&self) -> &crate::receiver::loss_recovery::LossRecovery {
        &self.loss_recovery
    }

    fn build_decoder(
        config: &ClientConfig,
        openxr_context: &OpenXrContext,
    ) -> ClientResult<Box<dyn HardwareVideoDecoder>> {
        #[cfg(target_os = "android")]
        let surface_ptr = openxr_context
            .surface_window()
            .map(|win| win.ptr().as_ptr() as *mut std::ffi::c_void);
        #[cfg(not(target_os = "android"))]
        let surface_ptr = {
            let _ = openxr_context;
            None
        };
        create_decoder(
            config,
            config.stream_width,
            config.stream_height,
            surface_ptr,
        )
    }

    /// Whether the swapchains (and the decoder bound to their surface) exist.
    pub fn swapchains_ready(&self) -> bool {
        self.swapchains_ready
    }

    /// Creates the swapchains and the real decoder. Call only once the OpenXR session is
    /// running; returns an error (retry on the next loop iteration) otherwise.
    pub fn create_swapchains(&mut self) -> ClientResult<()> {
        if self.swapchains_ready {
            return Ok(());
        }
        self.openxr_context.create_swapchains(&self.config)?;
        self.decoder = Self::build_decoder(&self.config, &self.openxr_context)?;
        // A fresh decoder can only start from a keyframe.
        self.loss_recovery = crate::receiver::loss_recovery::LossRecovery::new();
        self.swapchains_ready = true;
        Ok(())
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
        if !self.swapchains_ready {
            return Ok(FramePacingMetrics::default());
        }

        // 2. Pace rendering loop to target headset refresh rate (72Hz, 90Hz, 120Hz)
        let pacing = self.frame_loop.wait_frame_pacing(&self.openxr_context)?;
        let display_time = openxr::Time::from_nanos(pacing.predicted_display_time);
        let monotonic_display_ns = self
            .openxr_context
            .convert_time_to_monotonic_ns(display_time);

        // 2a. Poll controller actions and skeletal hand tracking
        let _ = self.openxr_context.poll_interaction(display_time);

        let dt_seconds = 1.0 / self.config.target_refresh_rate.max(1.0);

        let right_ctrl = self
            .openxr_context
            .interaction()
            .controller_state(Handedness::Right);
        let left_ctrl = self
            .openxr_context
            .interaction()
            .controller_state(Handedness::Left);

        let right_joints = self
            .openxr_context
            .locate_hand_joints(Handedness::Right, display_time)
            .ok()
            .flatten();
        let left_joints = self
            .openxr_context
            .locate_hand_joints(Handedness::Left, display_time)
            .ok()
            .flatten();

        let right_gesture = right_joints
            .as_ref()
            .map(|j| self.gesture_detector.update(Handedness::Right, j));
        let left_gesture = left_joints
            .as_ref()
            .map(|j| self.gesture_detector.update(Handedness::Left, j));

        let active_source = match self.interaction_state_machine.state() {
            InteractionState::Grabbing(session) => Some(session.source),
            InteractionState::Hovering { source, .. } => Some(*source),
            InteractionState::Idle => None,
        };

        let candidate_inputs: [Option<InteractionInput>; 4] = [
            right_ctrl
                .as_ref()
                .map(|c| InteractionInput::from_controller(Handedness::Right, c)),
            right_gesture
                .as_ref()
                .map(|g| InteractionInput::from_hand(Handedness::Right, g)),
            left_ctrl
                .as_ref()
                .map(|c| InteractionInput::from_controller(Handedness::Left, c)),
            left_gesture
                .as_ref()
                .map(|g| InteractionInput::from_hand(Handedness::Left, g)),
        ];

        // One pointer per hand: the controller when it has a pose, else the tracked hand. A
        // controller state exists even with no controller held, so inputs without a ray are
        // ignored (otherwise the right controller would always win and hands never could).
        let pointing: Vec<InteractionInput> = [
            (&candidate_inputs[0], &candidate_inputs[1]),
            (&candidate_inputs[2], &candidate_inputs[3]),
        ]
        .into_iter()
        .filter_map(|(ctrl, hand)| {
            ctrl.as_ref()
                .filter(|i| i.aim_ray.is_some())
                .or(hand.as_ref().filter(|i| i.aim_ray.is_some()))
                .cloned()
        })
        .collect();
        let hits: Vec<Option<crate::spatial::RayHit>> = pointing
            .iter()
            .map(|i| {
                i.aim_ray
                    .as_ref()
                    .and_then(|r| self.screen_transform.intersect_ray(r))
            })
            .collect();

        // An input that points at the screen and squeezes wins (it starts or takes over the
        // grab); otherwise a grab or hover in progress keeps its source.
        let squeezing_on_screen = pointing
            .iter()
            .zip(&hits)
            .find(|(i, hit)| i.is_grabbing && hit.is_some())
            .map(|(i, _)| *i);
        let grabbing = matches!(
            self.interaction_state_machine.state(),
            InteractionState::Grabbing(_)
        );
        let selected_input = match active_source {
            // The grabbing hand or controller lost tracking (no ray, no pose): treat that as a
            // release. Otherwise the grab would never see one and would stick forever.
            Some(src) if grabbing => {
                pointing
                    .iter()
                    .find(|inp| inp.source == src)
                    .cloned()
                    .or(Some(InteractionInput {
                        source: src,
                        aim_ray: None,
                        grab_pose: None,
                        is_grabbing: false,
                        is_selecting: false,
                        thumbstick: (0.0, 0.0),
                    }))
            }
            Some(src) => squeezing_on_screen
                .or_else(|| pointing.iter().find(|inp| inp.source == src).copied()),
            None => squeezing_on_screen.or_else(|| {
                pointing
                    .iter()
                    .zip(&hits)
                    .find(|(_, hit)| hit.is_some())
                    .map(|(i, _)| *i)
                    .or_else(|| pointing.first().cloned())
            }),
        };

        if let Some(input) = selected_input {
            // The other hand or controller joins a grab in progress for two-handed scaling
            let secondary_input = match self.interaction_state_machine.state() {
                InteractionState::Grabbing(session) => candidate_inputs
                    .iter()
                    .filter_map(|opt| opt.as_ref())
                    .find(|inp| {
                        inp.is_grabbing
                            && inp.grab_pose.is_some()
                            && inp.source != session.source
                            && inp.source.handedness() != session.source.handedness()
                    })
                    .copied(),
                _ => None,
            };
            if let Some(event) =
                self.step_interaction_two(&input, secondary_input.as_ref(), dt_seconds)
            {
                log_interaction_event(&event);
            }
        }

        let interaction_state = self.interaction_state_machine.state();
        self.overlay_scene = crate::openxr::overlay::OverlayScene {
            pointers: pointing
                .iter()
                .zip(&hits)
                .filter_map(|(i, hit)| {
                    let ray = i.aim_ray.as_ref()?;
                    Some(crate::openxr::overlay::PointerVisual {
                        origin: ray.origin,
                        direction: ray.direction,
                        length: hit
                            .as_ref()
                            .map_or(crate::openxr::overlay::RAY_MISS_LENGTH_M, |h| h.distance),
                        hit: hit.is_some(),
                    })
                })
                .collect(),
            border: !matches!(interaction_state, InteractionState::Idle),
            border_active: matches!(interaction_state, InteractionState::Grabbing(_)),
        };

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
            use crate::receiver::loss_recovery::Admit;
            for frame in ready_frames {
                if self
                    .loss_recovery
                    .admit(frame.meta.frame_id, frame.meta.is_keyframe)
                    == Admit::Skip
                {
                    continue;
                }
                if let Err(e) = self.decoder.queue_input_buffer(
                    &frame.bitstream,
                    frame.meta.pts_us,
                    frame.meta.is_keyframe,
                ) {
                    tracing::warn!(
                        error = ?e,
                        frame_id = frame.meta.frame_id,
                        "Failed to queue video frame into decoder; waiting for a keyframe"
                    );
                    self.loss_recovery.decoder_refused();
                }
            }
        }
        if self.loss_recovery.take_request(playout_instant) {
            if let Some(request) = &self.keyframe_requester {
                request();
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
            &self.overlay_scene,
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

    /// Ingests an incoming network or USB packet into the appropriate jitter buffer (video or audio).
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        if packet.header.packet_type == linux_quest_protocol::packet::PacketType::AudioFrame {
            let _ = self.audio_receiver.ingest_packet(packet, now);
        } else {
            self.receiver.ingest_packet(packet, now);
        }
    }

    /// Returns a reference to the client audio receiver.
    pub fn audio_receiver(&self) -> &AudioReceiver {
        &self.audio_receiver
    }

    /// Returns a mutable reference to the client audio receiver.
    pub fn audio_receiver_mut(&mut self) -> &mut AudioReceiver {
        &mut self.audio_receiver
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

    /// Returns the current lifecycle state of screen interaction (Idle, Hovering, or Grabbing).
    pub fn interaction_state(&self) -> &InteractionState {
        self.interaction_state_machine.state()
    }

    /// Returns a reference to the active screen grab state machine.
    pub fn interaction_state_machine(&self) -> &GrabStateMachine {
        &self.interaction_state_machine
    }

    /// Returns a mutable reference to the active screen grab state machine.
    pub fn interaction_state_machine_mut(&mut self) -> &mut GrabStateMachine {
        &mut self.interaction_state_machine
    }

    /// Returns a reference to the skeletal hand gesture detector.
    pub fn gesture_detector(&self) -> &HandGestureDetector {
        &self.gesture_detector
    }

    /// Returns a mutable reference to the skeletal hand gesture detector.
    pub fn gesture_detector_mut(&mut self) -> &mut HandGestureDetector {
        &mut self.gesture_detector
    }

    /// Returns a reference to the active spatial screen layout.
    pub fn active_layout(&self) -> &SpatialScreenLayout {
        &self.active_layout
    }

    /// Returns a reference to the underlying OpenXR context.
    pub fn openxr_context(&self) -> &OpenXrContext {
        &self.openxr_context
    }

    /// Returns a mutable reference to the underlying OpenXR context.
    pub fn openxr_context_mut(&mut self) -> &mut OpenXrContext {
        &mut self.openxr_context
    }

    /// Configures or replaces the active spatial layout store.
    pub fn set_layout_store(&mut self, store: SpatialLayoutStore) {
        self.layout_store = Some(store);
    }

    /// Steps the interaction state machine with the provided input, updating the active screen transform
    /// and persisting layout changes to disk when manipulation ends.
    pub fn step_interaction(
        &mut self,
        input: &InteractionInput,
        dt_seconds: f32,
    ) -> Option<InteractionEvent> {
        self.step_interaction_two(input, None, dt_seconds)
    }

    /// Like `step_interaction`, with an optional grabbing input from the other hand or controller
    /// that enables two-handed scaling of the screen during an active grab.
    pub fn step_interaction_two(
        &mut self,
        input: &InteractionInput,
        secondary: Option<&InteractionInput>,
        dt_seconds: f32,
    ) -> Option<InteractionEvent> {
        let screens = vec![(0, &self.screen_transform)];
        let event = self
            .interaction_state_machine
            .update_two(input, secondary, screens, dt_seconds);

        if let Some(ref ev) = event {
            match ev {
                InteractionEvent::GrabUpdated {
                    display_id,
                    transform,
                    ..
                } => {
                    self.screen_transform = transform.clone();
                    self.active_layout.insert(*display_id, transform.clone());
                    self.openxr_context.set_screen_transform(transform.clone());
                }
                InteractionEvent::GrabEnded {
                    display_id,
                    final_transform,
                    ..
                } => {
                    self.screen_transform = final_transform.clone();
                    self.active_layout
                        .insert(*display_id, final_transform.clone());
                    self.openxr_context
                        .set_screen_transform(final_transform.clone());

                    // Persist spatial layout to disk via layout store
                    if let Some(store) = &self.layout_store {
                        if let Err(e) = store.save(&self.active_layout) {
                            tracing::warn!(error = ?e, "Failed to persist spatial layout after grab manipulation");
                        } else {
                            tracing::info!(
                                display_id = *display_id,
                                "Persisted spatial layout after grab manipulation"
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        event
    }
    /// Signals the runtime to gracefully shut down.
    pub fn stop(&mut self) {
        self.is_running = false;
        let _ = self.decoder.flush();
    }
}

/// Hover and grab transitions are rare and useful when checking input on a headset.
fn log_interaction_event(event: &InteractionEvent) {
    match event {
        InteractionEvent::GrabUpdated { .. } | InteractionEvent::HoverUpdated { .. } => {}
        other => tracing::info!(event = ?other, "screen interaction"),
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
