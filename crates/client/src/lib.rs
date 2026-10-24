//! linux-quest-client
//!
//! Meta Quest 3 OpenXR client and hardware video decoder runtime.

pub mod android;
pub mod audio;
pub mod clipboard;
pub mod config;
pub mod decoder;
pub mod discovery;
pub mod error;
pub mod graphics;
pub mod host_control;
pub mod hud;
pub mod input;
pub mod input_sender;
pub mod openxr;
pub mod receiver;
pub mod runtime;
pub mod settings;
pub mod spatial;
pub mod storage;
pub mod streams;
pub mod transport;
pub mod usb_tunnel;

#[cfg(target_os = "android")]
pub use audio::AAudioSink;
pub use audio::{
    create_default_audio_sink, AudioDecoder, AudioDecoderError, AudioJitterBuffer,
    AudioPlayoutItem, AudioReceiver, AudioReceiverConfig, AudioReceiverStats, AudioSink,
    AudioSinkConfig, AudioSinkError, AudioStepSummary, AvSyncConfig, AvSyncMetrics, AvSynchronizer,
    ClientAudioPipeline, DecodedAudioFrame, DriftCorrectionMode, MockAudioDecoder, MockAudioSink,
    OpusAudioDecoder,
};
pub use clipboard::{
    AndroidClipboardManager, ClientClipboardSync, ClipboardManager, MockClipboardManager,
    DEFAULT_CLIENT_CLIPBOARD_DEBOUNCE_DURATION, DEFAULT_CLIENT_CLIPBOARD_POLL_INTERVAL,
};
pub use config::{ClientConfig, ClientVideoCodec, DisplayMode, TransportMode};
pub use decoder::{create_decoder, DecodedFrame, DecoderStats, HardwareVideoDecoder};
pub use discovery::scheduler::{DiscoveryScheduler, DEFAULT_RESCAN_PERIOD, MIN_RESCAN_PERIOD};
pub use discovery::{
    apply_selection, parse_manual_host, AndroidMulticastLock, ClientDiscovery, MockMulticastLock,
    MulticastLock,
};
pub use error::{ClientError, ClientResult};
pub use graphics::{ImageLayoutTransition, SwapchainTexture, VulkanContext};
pub use hud::{DebugHud, HudSnapshot, ShortcutAction, ShortcutDetector};
pub use input::{
    android_keycode_to_evdev, android_meta_to_modifiers, AndroidKeyEvent, AndroidMotionEvent,
    CapturedEvent, ClientInputCapture, ControllerInputEvent, InputSource, MockInputSource,
    MouseCoalescer,
};
pub use openxr::{
    build_cylinder_layer, build_cylinder_layer_from_transform, build_hud_layer,
    build_passthrough_layer, build_quad_layer, build_quad_layer_from_transform,
    CompositionLayerPassthroughFB, DesktopLayerConfig, FrameLoopEngine, FramePacingMetrics,
    HandTrackingBackend, InteractionBackend, MockHandTrackingBackend, MockInteractionBackend,
    MockPassthroughBackend, OpenXrContext, OpenXrHandTrackingBackend, OpenXrInteractionBackend,
    OpenXrPassthroughBackend, PassthroughBackend, PassthroughState, RefreshRateManager,
    QUEST_REFRESH_RATES,
};
pub use receiver::ClientReceiver;
pub use runtime::QuestClientRuntime;
pub use settings::{
    BitrateMode, ClientSettings, LayoutPreset as SettingsLayoutPreset, SettingsAction,
    SettingsPanel, SettingsStore,
};
pub use spatial::{
    ray_intersects_cylinder, ray_intersects_quad, ArrangementBook, ArrangementBookStore,
    ControllerInputState, CylinderLayerParams, GrabSession, GrabStateMachine, HandGestureDetector,
    HandGestureState, HandJoint, HandJointData, HandTrackingData, Handedness, HostPlacement,
    InteractionEvent, InteractionInput, InteractionSource, InteractionState, LayoutPreset,
    PanelArrangement, QuadLayerParams, Quat, Ray, RayHit, ScreenPose, ScreenTransform,
    SpatialLayoutStore, SpatialScreenLayout, Vec3, CURRENT_LAYOUT_VERSION,
    DEFAULT_GRAB_RELEASE_THRESHOLD_M, DEFAULT_GRAB_START_THRESHOLD_M,
    DEFAULT_PINCH_RELEASE_THRESHOLD_M, DEFAULT_PINCH_START_THRESHOLD_M, HAND_JOINT_COUNT,
};

pub fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_version() {
        assert_eq!(client_version(), "0.1.0");
    }
}
