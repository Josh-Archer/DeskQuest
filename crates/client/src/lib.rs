//! linux-quest-client
//!
//! Meta Quest 3 OpenXR client and hardware video decoder runtime.

pub mod android;
pub mod clipboard;
pub mod config;
pub mod decoder;
pub mod error;
pub mod graphics;
pub mod hud;
pub mod input;
pub mod openxr;
pub mod receiver;
pub mod runtime;

pub use clipboard::{
    AndroidClipboardManager, ClientClipboardSync, ClipboardManager, MockClipboardManager,
    DEFAULT_CLIENT_CLIPBOARD_DEBOUNCE_DURATION, DEFAULT_CLIENT_CLIPBOARD_POLL_INTERVAL,
};
pub use config::{ClientConfig, ClientVideoCodec, DisplayMode, TransportMode};
pub use decoder::{create_decoder, DecodedFrame, DecoderStats, HardwareVideoDecoder};
pub use error::{ClientError, ClientResult};
pub use graphics::{ImageLayoutTransition, SwapchainTexture, VulkanContext};
pub use hud::{DebugHud, HudSnapshot};
pub use input::{
    android_keycode_to_evdev, android_meta_to_modifiers, AndroidKeyEvent, AndroidMotionEvent,
    CapturedEvent, ClientInputCapture, ControllerInputEvent, InputSource, MockInputSource,
    MouseCoalescer,
};
pub use openxr::{
    build_cylinder_layer, build_hud_layer, build_quad_layer, DesktopLayerConfig, FrameLoopEngine,
    FramePacingMetrics, OpenXrContext, RefreshRateManager, QUEST_REFRESH_RATES,
};
pub use receiver::ClientReceiver;
pub use runtime::QuestClientRuntime;

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
