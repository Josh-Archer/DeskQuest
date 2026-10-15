//! Quest client input capture and mapping module.
//!
//! Provides translation of Android `KeyEvent` and `MotionEvent` data (from Bluetooth
//! keyboard, mouse, or Quest controllers) into Linux desktop protocol input events.

pub mod capture;
pub mod coalescer;
pub mod keycodes;
pub mod source;

pub use capture::ClientInputCapture;
pub use coalescer::MouseCoalescer;
pub use keycodes::{android_keycode_to_evdev, android_meta_to_modifiers};
pub use source::{
    AndroidKeyEvent, AndroidMotionEvent, CapturedEvent, ControllerInputEvent, InputSource,
    MockInputSource,
};
