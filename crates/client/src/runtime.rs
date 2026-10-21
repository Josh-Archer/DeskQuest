//! Quest client application runtime coordinating OpenXR, Vulkan, decoder, and transport.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use linux_quest_protocol::packet::Packet;
use linux_quest_protocol::InputEvent;

use crate::audio::{AudioReceiver, AudioReceiverConfig};
use crate::config::ClientConfig;
use crate::decoder::{create_decoder, DecoderStats, HardwareVideoDecoder};
use crate::error::ClientResult;
use crate::graphics::VulkanContext;
use crate::hud::{DebugHud, ShortcutAction, ShortcutDetector};
use crate::openxr::{FrameLoopEngine, FramePacingMetrics, OpenXrContext};
use crate::receiver::loss_recovery::{Admit, LossRecovery};
use crate::receiver::ClientReceiver;
use crate::settings::{ClientSettings, SettingsAction, SettingsPanel, SettingsStore};
use crate::spatial::{
    Corner, GrabStateMachine, HandGestureDetector, Handedness, InteractionEvent, InteractionInput,
    InteractionState, PointerInput, ScreenTransform, SpatialLayoutStore, SpatialScreenLayout, Vec3,
};

mod host_control;
pub use host_control::MAX_LAYOUT_DISPLAY_ID;

/// Video streams beyond the primary one (stream ids 1 to this) that get a screen of their own.
pub const MAX_EXTRA_STREAMS: u16 = 3;

/// How long to wait before retrying a screen or decoder that could not be created.
const EXTRA_SCREEN_RETRY: Duration = Duration::from_secs(5);

/// A video stream other than the primary one, shown on its own screen.
struct ExtraStream {
    receiver: ClientReceiver,
    loss_recovery: LossRecovery,
    /// Bound to the stream's screen, created when the first keyframe gives its size.
    decoder: Option<Box<dyn HardwareVideoDecoder>>,
    transform: ScreenTransform,
    /// After a failed screen or decoder creation, when to try again.
    retry_at: Option<Instant>,
    /// Whether a decoded frame has been shown on its screen yet (logged once).
    shown: bool,
}

/// Where stream `stream_id`'s screen starts, in screens beside the primary: 1 to the right,
/// 2 to the left, 3 further right.
fn extra_screen_slot(stream_id: u16) -> i32 {
    let n = i32::from(stream_id);
    if n % 2 == 1 {
        (n + 1) / 2
    } else {
        -(n / 2)
    }
}

/// The screens a pointer can hit or grab: the primary one and every extra stream's screen
/// that is showing.
fn visible_screens<'a>(
    primary: &'a ScreenTransform,
    extra: &'a BTreeMap<u16, ExtraStream>,
) -> Vec<(u32, &'a ScreenTransform)> {
    std::iter::once((0, primary))
        .chain(
            extra
                .iter()
                .filter(|(_, s)| s.decoder.is_some())
                .map(|(&id, s)| (u32::from(id), &s.transform)),
        )
        .collect()
}

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
    loss_recovery: LossRecovery,
    overlay_scene: crate::openxr::overlay::OverlayScene,
    keyframe_requester: Option<Box<dyn Fn(u16) + Send>>,
    extra_streams: BTreeMap<u16, ExtraStream>,
    pointer_input: PointerInput,
    pointer_event_sink: Option<Box<dyn Fn(InputEvent) + Send>>,
    pointer_queue_latency: Option<Box<dyn Fn() -> u64 + Send>>,
    settings_panel: SettingsPanel,
    settings_store: Option<SettingsStore>,
    shortcuts: ShortcutDetector,
    settings_action_sink: Option<Box<dyn Fn(SettingsAction) + Send>>,
    /// Headset model reported to the host.
    device_name: String,
    /// Something the host sees changed since the last status (see `host_control`).
    host_status_dirty: bool,
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
            Self::placeholder_decoder(&config)?
        };

        let receiver = ClientReceiver::new(&config);
        let audio_receiver = AudioReceiver::new(AudioReceiverConfig::default());
        let frame_loop = FrameLoopEngine::new();
        let hud = DebugHud::new(512, 256);
        let interaction_state_machine = GrabStateMachine::new();
        let gesture_detector = HandGestureDetector::new();
        let pointer_input = PointerInput::new(config.pointer_bindings);
        let settings_panel = SettingsPanel::new(ClientSettings::from_config(&config), 512, 256);

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
            loss_recovery: LossRecovery::new(),
            overlay_scene: Default::default(),
            keyframe_requester: None,
            extra_streams: BTreeMap::new(),
            pointer_input,
            pointer_event_sink: None,
            pointer_queue_latency: None,
            settings_panel,
            settings_store: None,
            shortcuts: ShortcutDetector::default(),
            settings_action_sink: None,
            device_name: "Meta Quest".to_string(),
            host_status_dirty: false,
        })
    }

    /// Called (rate limited) with the stream id whenever a stream's video is lost and needs a
    /// keyframe. On Android this sends a keyframe request on the UDP input channel.
    pub fn set_keyframe_requester(&mut self, request: Box<dyn Fn(u16) + Send>) {
        self.keyframe_requester = Some(request);
    }

    /// Installs the low-latency output used for pointer events generated by spatial interactions.
    pub fn set_pointer_event_sink(&mut self, sink: Box<dyn Fn(InputEvent) + Send>) {
        self.pointer_event_sink = Some(sink);
    }

    /// Installs a low-latency metric for time spent waiting in the client input sender queue.
    pub fn set_pointer_queue_latency_source(&mut self, source: Box<dyn Fn() -> u64 + Send>) {
        self.pointer_queue_latency = Some(source);
    }

    /// Loss-recovery state between the reassembler and the decoder.
    pub fn loss_recovery(&self) -> &LossRecovery {
        &self.loss_recovery
    }

    /// Stream ids of the extra video streams seen so far, and whether each has a screen yet.
    pub fn extra_streams(&self) -> Vec<(u16, bool)> {
        self.extra_streams
            .iter()
            .map(|(&id, s)| (id, s.decoder.is_some()))
            .collect()
    }

    /// Placement of extra stream `stream_id`'s screen.
    pub fn extra_screen_transform(&self, stream_id: u16) -> Option<&ScreenTransform> {
        self.extra_streams.get(&stream_id).map(|s| &s.transform)
    }

    /// Creates stream `stream_id`'s screen (if needed) and a decoder bound to it.
    fn open_extra_screen(
        config: &ClientConfig,
        openxr_context: &mut OpenXrContext,
        stream_id: u16,
        (width, height): (u32, u32),
        transform: &ScreenTransform,
    ) -> ClientResult<Box<dyn HardwareVideoDecoder>> {
        openxr_context.create_extra_screen(stream_id, (width, height), transform.clone())?;
        #[cfg(target_os = "android")]
        let surface_ptr = openxr_context
            .extra_surface_window(stream_id)
            .map(|win| win.ptr().as_ptr() as *mut std::ffi::c_void);
        #[cfg(not(target_os = "android"))]
        let surface_ptr = None;
        create_decoder(config, width, height, surface_ptr)
    }

    /// Feeds each extra stream's ready frames to its decoder (creating its screen on the first
    /// keyframe), asks for keyframes it needs, and shows its newest decoded frame.
    fn step_extra_streams(&mut self, now: Instant, display_ns: i64) {
        for (&id, stream) in self.extra_streams.iter_mut() {
            let frames = stream.receiver.pop_ready_frames(now).unwrap_or_default();
            for frame in frames {
                if stream
                    .loss_recovery
                    .admit(frame.meta.frame_id, frame.meta.is_keyframe)
                    == Admit::Skip
                {
                    continue;
                }
                if stream.decoder.is_none() {
                    if stream.retry_at.is_some_and(|t| now < t) {
                        stream.loss_recovery.decoder_refused();
                        continue;
                    }
                    let size = (frame.meta.width, frame.meta.height);
                    match Self::open_extra_screen(
                        &self.config,
                        &mut self.openxr_context,
                        id,
                        size,
                        &stream.transform,
                    ) {
                        Ok(decoder) => {
                            tracing::info!(
                                stream_id = id,
                                width = size.0,
                                height = size.1,
                                "showing video stream on its own screen"
                            );
                            stream.decoder = Some(decoder);
                            stream.retry_at = None;
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = ?e,
                                stream_id = id,
                                "could not create a screen for this video stream; retrying later"
                            );
                            stream.retry_at = Some(now + EXTRA_SCREEN_RETRY);
                            stream.loss_recovery.decoder_refused();
                            continue;
                        }
                    }
                }
                if let Some(decoder) = stream.decoder.as_mut() {
                    if let Err(e) = decoder.queue_input_buffer(
                        &frame.bitstream,
                        frame.meta.pts_us,
                        frame.meta.is_keyframe,
                    ) {
                        tracing::warn!(
                            error = ?e,
                            stream_id = id,
                            frame_id = frame.meta.frame_id,
                            "Failed to queue video frame into decoder; waiting for a keyframe"
                        );
                        stream.loss_recovery.decoder_refused();
                    }
                }
            }
            if stream.loss_recovery.take_request(now) {
                if let Some(request) = &self.keyframe_requester {
                    request(id);
                }
            }
            if let Some(decoder) = stream.decoder.as_mut() {
                let mut latest = None;
                while let Ok(Some(frame)) = decoder.dequeue_output_buffer(0) {
                    if let Some(prev) = latest.replace(frame) {
                        let _ = decoder.release_output_buffer(prev.buffer_index, false);
                    }
                }
                if let Some(frame) = latest {
                    let shown = decoder
                        .release_output_buffer_at_time(frame.buffer_index, display_ns)
                        .is_ok();
                    if shown && !stream.shown {
                        stream.shown = true;
                        tracing::info!(stream_id = id, "first decoded frame shown on its screen");
                    }
                }
            }
        }
    }

    /// Applies a moved or resized screen, primary or extra.
    fn apply_screen_transform(&mut self, display_id: u32, transform: ScreenTransform) {
        self.active_layout.insert(display_id, transform.clone());
        match u16::try_from(display_id) {
            Ok(0) => {
                self.screen_transform = transform.clone();
                self.openxr_context.set_screen_transform(transform);
            }
            Ok(id) => {
                if let Some(stream) = self.extra_streams.get_mut(&id) {
                    stream.transform = transform.clone();
                    self.openxr_context
                        .set_extra_screen_transform(id, transform);
                }
            }
            Err(_) => {}
        }
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
        let (width, height) = openxr_context.desktop_size();
        create_decoder(config, width, height, surface_ptr)
    }

    /// A decoder bound to no surface, standing in while the real one cannot exist.
    fn placeholder_decoder(config: &ClientConfig) -> ClientResult<Box<dyn HardwareVideoDecoder>> {
        let placeholder = ClientConfig {
            force_mock_decoder: true,
            ..config.clone()
        };
        create_decoder(
            &placeholder,
            config.stream_width,
            config.stream_height,
            None,
        )
    }

    /// Sizes the primary screen to what its stream decodes to: a new surface swapchain and a
    /// decoder bound to it. The swapchain is made before any video arrives, at the configured
    /// size, which a 4K desktop does not match.
    fn resize_primary_screen(&mut self, size: (u32, u32)) {
        let from = self.openxr_context.desktop_size();
        // The old decoder renders into the old surface: release it first.
        match Self::placeholder_decoder(&self.config) {
            Ok(placeholder) => self.decoder = placeholder,
            Err(e) => {
                tracing::warn!(error = ?e, "cannot resize the primary screen");
                return;
            }
        }
        let resized = self.openxr_context.resize_desktop_surface(size);
        if let Err(e) = &resized {
            tracing::warn!(error = ?e, "could not resize the primary screen; keeping its size");
        }
        match Self::build_decoder(&self.config, &self.openxr_context) {
            Ok(decoder) => {
                self.decoder = decoder;
                if resized.is_ok() {
                    tracing::info!(
                        from = ?from,
                        to = ?self.openxr_context.desktop_size(),
                        "primary screen sized to its stream"
                    );
                }
            }
            Err(e) => tracing::warn!(error = ?e, "no decoder for the primary screen"),
        }
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
        self.loss_recovery = LossRecovery::new();
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
        let hits: Vec<Option<crate::spatial::RayHit>> = {
            let screens = visible_screens(&self.screen_transform, &self.extra_streams);
            pointing
                .iter()
                .map(|i| {
                    i.aim_ray.as_ref().and_then(|r| {
                        ScreenTransform::find_closest_window_hit(r, screens.iter().copied())
                            .map(|(_, hit)| hit)
                    })
                })
                .collect()
        };

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
        // A hand or controller that stops pointing (put down, out of view, lost tracking) must
        // be reported as such: otherwise the hover or grab it started never ends, and no other
        // input can take over. Picking up the controllers after a hand hovered did exactly that.
        let stopped_pointing = |source| InteractionInput {
            source,
            aim_ray: None,
            grab_pose: None,
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };
        let selected_input = match active_source {
            // For a grab, that is a release.
            Some(src) if grabbing => pointing
                .iter()
                .find(|inp| inp.source == src)
                .copied()
                .or(Some(stopped_pointing(src))),
            // For a hover, it ends it, so the next frame picks whatever points now.
            Some(src) => squeezing_on_screen
                .or_else(|| pointing.iter().find(|inp| inp.source == src).copied())
                .or(Some(stopped_pointing(src))),
            None => squeezing_on_screen.or_else(|| {
                pointing
                    .iter()
                    .zip(&hits)
                    .find(|(_, hit)| hit.is_some())
                    .map(|(i, _)| *i)
                    .or_else(|| pointing.first().cloned())
            }),
        };

        let mut pointer_latency_us = 0;
        if let Some(input) = selected_input {
            // Each position carries its stream's display id; the host rebases it onto that
            // monitor.
            let screen_refs = visible_screens(&self.screen_transform, &self.extra_streams);
            let secondary_down = match input.source {
                crate::spatial::InteractionSource::Controller(Handedness::Right) => {
                    right_ctrl.as_ref().is_some_and(|c| c.thumbstick_clicked)
                }
                crate::spatial::InteractionSource::Controller(Handedness::Left) => {
                    left_ctrl.as_ref().is_some_and(|c| c.thumbstick_clicked)
                }
                crate::spatial::InteractionSource::Hand(Handedness::Right) => {
                    right_gesture.as_ref().is_some_and(|g| g.is_grabbing)
                }
                crate::spatial::InteractionSource::Hand(Handedness::Left) => {
                    left_gesture.as_ref().is_some_and(|g| g.is_grabbing)
                }
            };
            let started = Instant::now();
            let pointer_events = self.pointer_input.update(
                &input,
                secondary_down,
                (
                    input.thumbstick.0 * dt_seconds,
                    input.thumbstick.1 * dt_seconds,
                ),
                &screen_refs,
                now,
            );
            pointer_latency_us = started.elapsed().as_micros() as u64;
            if let Some(sink) = &self.pointer_event_sink {
                for event in pointer_events {
                    sink(event);
                }
            }

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
            let mut manipulation_input = input;
            let continuing_hand_grab = matches!(
                self.interaction_state_machine.state(),
                InteractionState::Grabbing(session) if session.source == input.source
            );
            manipulation_input.is_grabbing &= input.source.is_controller()
                || self.interaction_state_machine.hovered_corner().is_some()
                || continuing_hand_grab;
            let mut manipulation_secondary = secondary_input;
            if let Some(secondary) = manipulation_secondary.as_mut() {
                secondary.is_grabbing &= secondary.source.is_controller()
                    || self.interaction_state_machine.hovered_corner().is_some();
            }
            if let Some(event) = self.step_interaction_two(
                &manipulation_input,
                manipulation_secondary.as_ref(),
                dt_seconds,
            ) {
                log_interaction_event(&event);
            }
        }
        if let Some(queue_latency) = &self.pointer_queue_latency {
            pointer_latency_us = pointer_latency_us.max(queue_latency());
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
            chrome: Default::default(),
        };
        let hover_uv = match interaction_state {
            InteractionState::Hovering { hit, .. } => Some(hit.uv),
            _ => None,
        };
        let resizing = self.interaction_state_machine.resizing().map(|c| match c {
            Corner::TopLeft => (0.0, 0.0),
            Corner::TopRight => (1.0, 0.0),
            Corner::BottomLeft => (0.0, 1.0),
            Corner::BottomRight => (1.0, 1.0),
        });
        let moving =
            matches!(interaction_state, InteractionState::Grabbing(_)) && resizing.is_none();
        let handle_display = match interaction_state {
            InteractionState::Hovering { display_id, .. } => *display_id,
            InteractionState::Grabbing(session) => session.display_id,
            InteractionState::Idle => 0,
        };
        let handle_screen = u16::try_from(handle_display)
            .ok()
            .and_then(|id| self.extra_streams.get(&id))
            .map_or(&self.screen_transform, |s| &s.transform);
        self.overlay_scene.chrome =
            crate::openxr::overlay::window_chrome(handle_screen, hover_uv, moving, resizing);

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
                if self
                    .loss_recovery
                    .admit(frame.meta.frame_id, frame.meta.is_keyframe)
                    == Admit::Skip
                {
                    continue;
                }
                let size = (frame.meta.width, frame.meta.height);
                if frame.meta.is_keyframe
                    && size.0 > 0
                    && size.1 > 0
                    && size != self.openxr_context.desktop_size()
                {
                    self.resize_primary_screen(size);
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
                request(0);
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
        self.step_extra_streams(playout_instant, monotonic_display_ns);

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
        self.hud.set_input_latency_us(pointer_latency_us);
        // Tell the host how the network is doing so it can adapt the bitrate.
        if let Some(feedback) = self.receiver.poll_feedback(Instant::now()) {
            self.hud.set_received_kbps(feedback.received_kbps);
            if let Some(sink) = &self.pointer_event_sink {
                sink(InputEvent::NetworkFeedback(feedback));
            }
        }

        // 6. Submit composition layers to OpenXR compositor for timewarp display
        // The settings panel takes over the head-locked overlay while it is open.
        let panel_open = self.settings_panel.is_visible();
        let hud_rgba = if panel_open {
            Some(self.settings_panel.render_to_rgba())
        } else if self.config.enable_hud {
            Some(self.hud.render_to_rgba())
        } else {
            None
        };
        let mut layer_config = std::borrow::Cow::Borrowed(&self.config);
        if panel_open && !self.config.enable_hud {
            layer_config.to_mut().enable_hud = true;
        }

        let present_res = self.openxr_context.render_and_present_layers(
            display_time,
            &layer_config,
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
    /// Video packets go to their stream's receiver by `stream_id`: 0 is the primary screen,
    /// and each of the ids 1 to [`MAX_EXTRA_STREAMS`] gets a screen of its own, placed beside
    /// the primary one (or where the saved layout has it).
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        let stream_id = packet.header.stream_id;
        if packet.header.packet_type == linux_quest_protocol::packet::PacketType::AudioFrame {
            let _ = self.audio_receiver.ingest_packet(packet, now);
        } else if stream_id == 0 {
            self.receiver.ingest_packet(packet, now);
        } else if stream_id <= MAX_EXTRA_STREAMS {
            let (config, primary, layout) =
                (&self.config, &self.screen_transform, &self.active_layout);
            self.extra_streams
                .entry(stream_id)
                .or_insert_with(|| {
                    tracing::info!(stream_id, "new video stream; it gets its own screen");
                    ExtraStream {
                        receiver: ClientReceiver::new(config),
                        loss_recovery: LossRecovery::new(),
                        decoder: None,
                        transform: layout
                            .get(u32::from(stream_id))
                            .cloned()
                            .unwrap_or_else(|| primary.beside(extra_screen_slot(stream_id))),
                        retry_at: None,
                        shown: false,
                    }
                })
                .receiver
                .ingest_packet(packet, now);
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

    /// Loads persisted settings (if any), applies them to the runtime, and persists later changes.
    pub fn set_settings_store(&mut self, store: SettingsStore) {
        if let Some(saved) = store.load() {
            saved.apply_to(&mut self.config);
            self.pointer_input = PointerInput::new(self.config.pointer_bindings);
            self.settings_panel = SettingsPanel::new(saved, 512, 256);
            let _ = self
                .openxr_context
                .request_refresh_rate(self.config.target_refresh_rate);
        }
        self.settings_store = Some(store);
    }

    /// Receives the actions that need the session layer (reconnect, host control requests).
    pub fn set_settings_action_sink(&mut self, sink: Box<dyn Fn(SettingsAction) + Send>) {
        self.settings_action_sink = Some(sink);
    }

    pub fn settings_panel(&self) -> &SettingsPanel {
        &self.settings_panel
    }

    /// Feeds the thumbstick-click state each frame; both clicks toggle the HUD, the left alone
    /// toggles the settings panel.
    pub fn step_shortcuts(&mut self, left_click: bool, right_click: bool) {
        match self.shortcuts.update(left_click, right_click) {
            Some(ShortcutAction::ToggleHud) => {
                let enabled = !self.config.enable_hud;
                self.config.enable_hud = enabled;
                self.settings_panel.sync_hud(enabled);
                self.persist_settings();
                self.host_status_dirty = true;
            }
            Some(ShortcutAction::TogglePanel) => {
                self.settings_panel.toggle();
            }
            None => {}
        }
    }

    /// Moves the panel selection (thumbstick up/down).
    pub fn panel_navigate(&mut self, dir: i32) {
        self.settings_panel.navigate(dir);
    }

    /// Steps the selected panel row (thumbstick left/right or trigger).
    pub fn panel_adjust(&mut self, dir: i32) {
        let actions = self.settings_panel.adjust(dir);
        self.run_settings_actions(actions);
    }

    /// Clicks the panel at the pointer's UV position.
    pub fn panel_click(&mut self, u: f32, v: f32) {
        let actions = self.settings_panel.click_at(u, v);
        self.run_settings_actions(actions);
    }

    fn run_settings_actions(&mut self, actions: Vec<SettingsAction>) {
        for action in actions {
            match &action {
                SettingsAction::SetRefreshRate(rate) => self.set_target_refresh_rate(*rate),
                SettingsAction::SetPassthrough(on) => {
                    let _ = self.set_passthrough_enabled(*on);
                }
                SettingsAction::SetHud(on) => self.config.enable_hud = *on,
                _ => {}
            }
            if let Some(sink) = &self.settings_action_sink {
                sink(action);
            }
            self.host_status_dirty = true;
        }
        self.persist_settings();
    }

    fn persist_settings(&mut self) {
        if !self.settings_panel.take_dirty() {
            return;
        }
        if let Some(store) = &self.settings_store {
            if let Err(e) = store.save(self.settings_panel.settings()) {
                tracing::warn!(error = ?e, "Failed to persist client settings");
            }
        }
    }

    /// Returns the current HUD telemetry text.
    pub fn hud_text(&self) -> String {
        self.hud.formatted_text()
    }

    /// Returns the decoder statistics.
    pub fn decoder_stats(&self) -> DecoderStats {
        self.decoder.stats()
    }

    /// Machine-readable primary-stream counters for hardware validation. Surface releases
    /// are submission counts, not proof of photons or distinct compositor presentations.
    pub fn validation_metrics(&self) -> serde_json::Value {
        let d = self.decoder.stats();
        let h = self.hud.snapshot();
        let p = self.frame_loop.last_metrics();
        serde_json::json!({
            "loop_frames": self.frame_loop.frame_counter(),
            "hud_fps": h.fps,
            "target_hz": h.target_fps,
            "hud_estimate_ms": h.motion_to_photon_ms,
            "wait_us": p.wait_frame_duration_us,
            "render_us": p.render_duration_us,
            "queued": d.frames_queued,
            "decoded": d.frames_decoded,
            "surface_releases": d.frames_rendered,
            "decoder_dropped": d.frames_dropped,
            "decode_last_us": d.last_latency_us,
            "decoder": d.decoder_name,
            "packets": h.packets_received,
            "packets_lost": h.packets_lost,
            "fec_recovered": h.fec_recovered,
            "simulated": self.openxr_context.is_simulated(),
        })
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
        for (&id, stream) in &self.extra_streams {
            layout.insert(u32::from(id), stream.transform.clone());
        }
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
        let screens = visible_screens(&self.screen_transform, &self.extra_streams);
        let event = self
            .interaction_state_machine
            .update_two(input, secondary, screens, dt_seconds);

        if let Some(ref ev) = event {
            match ev {
                InteractionEvent::GrabUpdated {
                    display_id,
                    transform,
                    ..
                } => self.apply_screen_transform(*display_id, transform.clone()),
                InteractionEvent::GrabEnded {
                    display_id,
                    final_transform,
                    ..
                } => {
                    self.apply_screen_transform(*display_id, final_transform.clone());
                    self.host_status_dirty = true;

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
        for stream in self.extra_streams.values_mut() {
            if let Some(decoder) = stream.decoder.as_mut() {
                let _ = decoder.flush();
            }
        }
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
            0,
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

    #[test]
    fn shortcuts_toggle_hud_and_panel_and_panel_actions_reach_sink_and_store() {
        use crate::settings::PanelRow;
        use std::sync::{Arc, Mutex};

        let path = std::env::temp_dir().join(format!("lq-rt-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
        runtime.set_settings_store(SettingsStore::new(&path));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        runtime.set_settings_action_sink(Box::new(move |a| sink_seen.lock().unwrap().push(a)));

        // Both sticks toggle the HUD off and persist it.
        assert!(runtime.config.enable_hud);
        runtime.step_shortcuts(true, true);
        runtime.step_shortcuts(false, false);
        assert!(!runtime.config.enable_hud);
        assert!(!SettingsStore::new(&path).load().unwrap().hud_enabled);

        // Left stick opens the panel; adding a monitor reaches the sink and the store.
        runtime.step_shortcuts(true, false);
        runtime.step_shortcuts(false, false);
        assert!(runtime.settings_panel().is_visible());
        while runtime.settings_panel().selected_row() != PanelRow::Monitors {
            runtime.panel_navigate(1);
        }
        runtime.panel_adjust(1);
        let actions = seen.lock().unwrap().clone();
        assert!(matches!(
            &actions[..],
            [SettingsAction::Display(
                linux_quest_protocol::DisplayConfigMessage::SetMonitorCount { count: 2, .. }
            )]
        ));
        assert_eq!(SettingsStore::new(&path).load().unwrap().monitor_count, 2);

        // The panel renders while open even though the HUD is off.
        runtime.step_frame().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    fn video_packet(stream_id: u16, frame_id: u64, is_keyframe: bool) -> Packet {
        let meta = VideoChunkMeta {
            frame_id,
            chunk_index: 0,
            total_chunks: 1,
            codec: VideoCodec::Av1,
            is_keyframe,
            is_intra_refresh: false,
            width: 1280,
            height: 720,
            fps: 60,
            pts_us: frame_id * 16_666,
        };
        let bytes = VideoChunk::new(meta, Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]))
            .serialize()
            .unwrap();
        let header = linux_quest_protocol::packet::PacketHeader::new(
            linux_quest_protocol::packet::PacketType::VideoFrameChunk,
            stream_id,
            frame_id as u32,
            frame_id * 16_666,
            &bytes,
        );
        Packet::new(header, bytes)
    }

    #[test]
    fn extra_screens_start_beside_the_primary_alternating_sides() {
        assert_eq!(extra_screen_slot(1), 1);
        assert_eq!(extra_screen_slot(2), -1);
        assert_eq!(extra_screen_slot(3), 2);
    }

    #[test]
    fn each_extra_stream_gets_its_own_screen_and_keyframe_requests() {
        let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
        let requested = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = requested.clone();
        runtime.set_keyframe_requester(Box::new(move |id| log.lock().unwrap().push(id)));
        runtime.step_frame().unwrap();
        // With no video yet, the primary stream asks for its first keyframe.
        assert_eq!(std::mem::take(&mut *requested.lock().unwrap()), vec![0]);

        let arrival = Instant::now();
        runtime.ingest_packet(video_packet(0, 1, true), arrival);
        runtime.ingest_packet(video_packet(1, 1, true), arrival);
        // Stream 2 starts mid-GOP: no screen until a keyframe, and it asks for one.
        runtime.ingest_packet(video_packet(2, 5, false), arrival);
        // Beyond the supported streams: ignored.
        runtime.ingest_packet(video_packet(MAX_EXTRA_STREAMS + 1, 1, true), arrival);
        runtime
            .step_frame_at(arrival + Duration::from_millis(10))
            .unwrap();

        assert_eq!(runtime.extra_streams(), vec![(1, true), (2, false)]);
        assert_eq!(*requested.lock().unwrap(), vec![2]);
        assert_eq!(runtime.decoder_stats().frames_queued, 1);

        let primary = runtime.screen_transform().position;
        assert!(runtime.extra_screen_transform(1).unwrap().position.x > primary.x + 0.5);
        assert!(runtime.extra_screen_transform(2).unwrap().position.x < primary.x - 0.5);

        // The keyframe arrives: stream 2 gets its screen too.
        runtime.ingest_packet(
            video_packet(2, 6, true),
            arrival + Duration::from_millis(20),
        );
        runtime
            .step_frame_at(arrival + Duration::from_millis(40))
            .unwrap();
        assert_eq!(runtime.extra_streams(), vec![(1, true), (2, true)]);
    }

    #[test]
    fn a_pointer_aimed_at_a_side_screen_hovers_that_screen() {
        let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
        let arrival = Instant::now();
        runtime.ingest_packet(video_packet(1, 1, true), arrival);
        runtime
            .step_frame_at(arrival + Duration::from_millis(10))
            .unwrap();
        let primary = runtime.screen_transform().clone();
        let viewer = primary.position + Vec3::new(0.0, 0.0, primary.distance);
        let target = runtime.extra_screen_transform(1).unwrap().position;
        let input = InteractionInput {
            source: crate::spatial::InteractionSource::Controller(Handedness::Right),
            aim_ray: Some(crate::spatial::Ray::new(viewer, target - viewer)),
            grab_pose: None,
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };
        runtime.step_interaction(&input, 1.0 / 90.0);
        assert!(
            matches!(
                runtime.interaction_state(),
                InteractionState::Hovering { display_id: 1, .. }
            ),
            "{:?}",
            runtime.interaction_state()
        );
    }

    #[test]
    fn a_hover_ends_when_its_hand_stops_pointing_so_a_controller_can_take_over() {
        use crate::openxr::{MockHandTrackingBackend, MockInteractionBackend};
        use crate::spatial::{ControllerInputState, InteractionSource, Quat, ScreenPose};
        let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
        let at_screen = Vec3::new(0.0, runtime.screen_transform().position.y, -0.4);

        // The left hand points at the screen first, as when putting the headset on.
        let mut hands = MockHandTrackingBackend::new(true);
        hands.simulate_open_hand(Handedness::Left, at_screen, Vec3::new(0.0, 0.0, -1.0));
        runtime
            .openxr_context_mut()
            .set_hand_tracking_backend(Box::new(hands));
        runtime.step_frame().unwrap();
        assert!(matches!(
            runtime.interaction_state(),
            InteractionState::Hovering {
                source: InteractionSource::Hand(Handedness::Left),
                ..
            }
        ));

        // Then the controllers are picked up: the hand is gone, a controller points.
        runtime
            .openxr_context_mut()
            .set_hand_tracking_backend(Box::new(MockHandTrackingBackend::new(true)));
        let mut controllers = MockInteractionBackend::new();
        controllers.set_controller_state(
            Handedness::Right,
            Some(ControllerInputState {
                aim_pose: Some(ScreenPose::new(at_screen, Quat::IDENTITY)),
                grip_pose: Some(ScreenPose::new(at_screen, Quat::IDENTITY)),
                ..Default::default()
            }),
        );
        runtime
            .openxr_context_mut()
            .set_interaction_backend(Box::new(controllers));
        runtime.step_frame().unwrap();
        runtime.step_frame().unwrap();
        assert!(
            matches!(
                runtime.interaction_state(),
                InteractionState::Hovering {
                    source: InteractionSource::Controller(Handedness::Right),
                    ..
                }
            ),
            "{:?}",
            runtime.interaction_state()
        );
    }

    #[test]
    fn the_primary_screen_takes_its_streams_size() {
        let mut runtime = QuestClientRuntime::new(ClientConfig::default()).unwrap();
        assert_eq!(runtime.openxr_context().desktop_size(), (1920, 1080));
        let arrival = Instant::now();
        let mut packet = video_packet(0, 1, true);
        let mut chunk = VideoChunk::deserialize(&packet.payload).unwrap();
        chunk.meta.width = 3840;
        chunk.meta.height = 2160;
        let bytes = chunk.serialize().unwrap();
        packet.header = linux_quest_protocol::packet::PacketHeader::new(
            packet.header.packet_type,
            0,
            1,
            16_666,
            &bytes,
        );
        packet.payload = bytes;
        runtime.ingest_packet(packet, arrival);
        runtime
            .step_frame_at(arrival + Duration::from_millis(10))
            .unwrap();
        assert_eq!(runtime.openxr_context().desktop_size(), (3840, 2160));
        assert_eq!(
            runtime.decoder_stats().frames_queued,
            1,
            "the keyframe goes to the new decoder"
        );
    }
}
