//! Meta Quest client clipboard integration.
//!
//! Provides the `ClipboardManager` trait, Android JNI implementation (`AndroidClipboardManager`),
//! mock backend (`MockClipboardManager`), and `ClientClipboardSync` engine integration.

pub mod android_jni;
pub mod manager;
pub mod mock;
pub mod sync;

pub use android_jni::AndroidClipboardManager;
pub use manager::ClipboardManager;
pub use mock::MockClipboardManager;
pub use sync::{
    ClientClipboardSync, DEFAULT_CLIENT_CLIPBOARD_DEBOUNCE_DURATION,
    DEFAULT_CLIENT_CLIPBOARD_POLL_INTERVAL,
};
