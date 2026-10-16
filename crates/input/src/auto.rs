//! Automatic input injector discovery and fallback orchestration.

use crate::device::UinputDeviceOpener;
use crate::error::InputError;
use crate::uinput::{UinputConfig, UinputInjector};
use crate::{InputInjector, MockInputInjector};
use linux_quest_protocol::InputEvent;

/// Automatically chooses between `UinputInjector` and `MockInputInjector`.
///
/// If `/dev/uinput` is accessible and properly configured, it initializes kernel-level
/// virtual devices. Otherwise, it logs an informative diagnostic message and gracefully falls back
/// to `MockInputInjector`.
pub struct AutoInputInjector {
    inner: Box<dyn InputInjector>,
    is_uinput: bool,
}

impl Default for AutoInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoInputInjector {
    /// Creates an `AutoInputInjector`, attempting to open real `/dev/uinput` devices
    /// and falling back to `MockInputInjector` if unavailable or permission is denied.
    pub fn new() -> Self {
        match UinputInjector::open() {
            Ok(injector) => {
                tracing::info!(
                    "Initialized Linux uinput virtual input devices (mouse, absolute pointer, keyboard)"
                );
                Self {
                    inner: Box::new(injector),
                    is_uinput: true,
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Virtual input device (/dev/uinput) is unavailable; falling back to MockInputInjector. See README for udev permissions setup."
                );
                Self {
                    inner: Box::new(MockInputInjector::new()),
                    is_uinput: false,
                }
            }
        }
    }

    /// Creates an `AutoInputInjector` with a custom device opener and configuration.
    pub fn new_with_opener(opener: &dyn UinputDeviceOpener, config: &UinputConfig) -> Self {
        match UinputInjector::new_with_opener(opener, config) {
            Ok(injector) => Self {
                inner: Box::new(injector),
                is_uinput: true,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to create UinputInjector with opener; falling back to MockInputInjector"
                );
                Self {
                    inner: Box::new(MockInputInjector::new()),
                    is_uinput: false,
                }
            }
        }
    }

    /// Returns `true` if this injector is currently backed by real or mock `UinputInjector` devices.
    pub fn is_uinput(&self) -> bool {
        self.is_uinput
    }

    /// Returns a reference to the active inner injector.
    pub fn inner(&self) -> &dyn InputInjector {
        &*self.inner
    }

    /// Returns a mutable reference to the active inner injector.
    pub fn inner_mut(&mut self) -> &mut dyn InputInjector {
        &mut *self.inner
    }

    /// Releases all held keys and mouse buttons.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        self.inner.release_all()
    }
}

impl InputInjector for AutoInputInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        self.inner.inject_event(event)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        self.inner.release_all()
    }
}

/// Convenience factory creating an `InputInjector` trait object wrapped in `AutoInputInjector`.
pub fn create_auto_injector() -> Box<dyn InputInjector> {
    Box::new(AutoInputInjector::new())
}
