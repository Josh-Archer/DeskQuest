//! Automatic input injector discovery and fallback orchestration.

use crate::device::UinputDeviceOpener;
use crate::error::InputError;
use crate::uinput::{UinputConfig, UinputInjector};
use crate::{
    BackendDiagnostic, BackendKind, InputBackend, InputInjector, MockInputInjector, XTestInjector,
};
use linux_quest_protocol::InputEvent;

/// Automatically chooses between `UinputInjector` and `MockInputInjector`.
///
/// If `/dev/uinput` is accessible and properly configured, it initializes kernel-level
/// virtual devices. Otherwise, it logs an informative diagnostic message and gracefully falls back
/// to `MockInputInjector`.
pub struct AutoInputInjector {
    inner: Box<dyn InputInjector>,
    diagnostic: BackendDiagnostic,
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
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let display = std::env::var_os("DISPLAY").is_some();
        let mut skipped = Vec::new();
        if wayland {
            skipped.push((
                BackendKind::Portal,
                "RemoteDesktop/libei session is not available in this build".into(),
            ));
            skipped.push((
                BackendKind::Wlroots,
                "virtual input protocol client is not available in this build".into(),
            ));
            if display {
                skipped.push((
                    BackendKind::XTest,
                    "Wayland session takes precedence over the Xwayland DISPLAY".into(),
                ));
            } else {
                skipped.push((BackendKind::XTest, "DISPLAY is not set".into()));
            }
        } else if display {
            match XTestInjector::new() {
                Ok(injector) => {
                    tracing::info!("Using X11 XTEST input backend");
                    return Self {
                        inner: Box::new(injector),
                        diagnostic: BackendDiagnostic {
                            active: BackendKind::XTest,
                            reason: "X11 session detected and xdotool XTEST probe succeeded".into(),
                            skipped: vec![
                                (
                                    BackendKind::Portal,
                                    "RemoteDesktop portal is not needed in an X11 session".into(),
                                ),
                                (
                                    BackendKind::Wlroots,
                                    "wlroots protocols are not available in an X11 session".into(),
                                ),
                            ],
                        },
                    };
                }
                Err(e) => skipped.push((BackendKind::XTest, format!("XTEST probe failed: {e}"))),
            }
            skipped.push((
                BackendKind::Portal,
                "RemoteDesktop portal is not needed in an X11 session".into(),
            ));
            skipped.push((
                BackendKind::Wlroots,
                "wlroots protocols are not available in an X11 session".into(),
            ));
        } else {
            skipped.push((
                BackendKind::Portal,
                "RemoteDesktop portal unavailable: no portal backend is built".into(),
            ));
            skipped.push((
                BackendKind::Wlroots,
                "wlroots backend unavailable: no Wayland session detected".into(),
            ));
            skipped.push((BackendKind::XTest, "DISPLAY is not set".into()));
        }
        match UinputInjector::open() {
            Ok(injector) => {
                tracing::info!(
                    "Initialized Linux uinput virtual input devices (mouse, absolute pointer, keyboard)"
                );
                Self {
                    inner: Box::new(injector),
                    diagnostic: BackendDiagnostic {
                        active: BackendKind::Uinput,
                        reason: "opened /dev/uinput".into(),
                        skipped,
                    },
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Virtual input device (/dev/uinput) is unavailable; falling back to MockInputInjector. See README for udev permissions setup."
                );
                Self {
                    inner: Box::new(MockInputInjector::new()),
                    diagnostic: BackendDiagnostic {
                        active: BackendKind::Mock,
                        reason: format!("uinput unavailable: {e}"),
                        skipped: {
                            let mut reasons = skipped;
                            reasons.push((
                                BackendKind::Uinput,
                                format!("could not open /dev/uinput: {e}"),
                            ));
                            reasons
                        },
                    },
                }
            }
        }
    }

    /// Creates an `AutoInputInjector` with a custom device opener and configuration.
    pub fn new_with_opener(opener: &dyn UinputDeviceOpener, config: &UinputConfig) -> Self {
        match UinputInjector::new_with_opener(opener, config) {
            Ok(injector) => Self {
                inner: Box::new(injector),
                diagnostic: BackendDiagnostic {
                    active: BackendKind::Uinput,
                    reason: "uinput created with the supplied device opener".into(),
                    skipped: Vec::new(),
                },
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to create UinputInjector with opener; falling back to MockInputInjector"
                );
                Self {
                    inner: Box::new(MockInputInjector::new()),
                    diagnostic: BackendDiagnostic {
                        active: BackendKind::Mock,
                        reason: format!("uinput creation failed: {e}"),
                        skipped: vec![(BackendKind::Uinput, format!("{e}"))],
                    },
                }
            }
        }
    }

    /// Returns `true` if this injector is currently backed by real or mock `UinputInjector` devices.
    pub fn is_uinput(&self) -> bool {
        self.diagnostic.active == BackendKind::Uinput
    }

    pub fn diagnostic(&self) -> &BackendDiagnostic {
        &self.diagnostic
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

impl InputBackend for AutoInputInjector {
    fn backend_kind(&self) -> BackendKind {
        self.diagnostic.active
    }
    fn diagnostic(&self) -> BackendDiagnostic {
        self.diagnostic.clone()
    }
}

/// Convenience factory creating an `InputInjector` trait object wrapped in `AutoInputInjector`.
pub fn create_auto_injector() -> Box<dyn InputInjector> {
    Box::new(AutoInputInjector::new())
}
