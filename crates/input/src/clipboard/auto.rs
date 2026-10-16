//! Automatic clipboard backend discovery and fallback orchestration.

use crate::clipboard::backend::ClipboardBackend;
use crate::clipboard::mock::MockClipboardBackend;
use crate::clipboard::wayland::WaylandClipboardBackend;
use crate::clipboard::x11::X11ClipboardBackend;
use crate::error::InputError;

/// Which backend was automatically resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBackendType {
    Wayland,
    X11,
    Mock,
}

/// Automatically selects between Wayland (`wl-clipboard`), X11 (`xclip`/`xsel`),
/// and `MockClipboardBackend`.
pub struct AutoClipboardBackend {
    inner: Box<dyn ClipboardBackend>,
    backend_type: ResolvedBackendType,
}

impl Default for AutoClipboardBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoClipboardBackend {
    /// Detects the current desktop session environment (`WAYLAND_DISPLAY` or `DISPLAY`)
    /// and initializes the appropriate backend. Falls back gracefully to `MockClipboardBackend`
    /// if neither is usable or external tools are missing.
    pub fn new() -> Self {
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY")
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let has_x11 = std::env::var_os("DISPLAY")
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        if has_wayland {
            let wayland = WaylandClipboardBackend::new();
            if wayland.is_available() {
                tracing::info!(
                    "Auto-detected Wayland display; using Wayland clipboard backend (wl-clipboard)"
                );
                return Self {
                    inner: Box::new(wayland),
                    backend_type: ResolvedBackendType::Wayland,
                };
            }
        }

        if has_x11 {
            let x11 = X11ClipboardBackend::new();
            if x11.is_available() {
                tracing::info!(
                    "Auto-detected X11 display; using X11 clipboard backend (xclip/xsel)"
                );
                return Self {
                    inner: Box::new(x11),
                    backend_type: ResolvedBackendType::X11,
                };
            }
        }

        tracing::warn!(
            wayland_present = has_wayland,
            x11_present = has_x11,
            "No functional clipboard tools found on system (wl-copy/wl-paste or xclip/xsel); falling back to MockClipboardBackend"
        );

        Self {
            inner: Box::new(MockClipboardBackend::new()),
            backend_type: ResolvedBackendType::Mock,
        }
    }

    /// Wraps an explicit backend.
    pub fn with_backend(
        backend: Box<dyn ClipboardBackend>,
        backend_type: ResolvedBackendType,
    ) -> Self {
        Self {
            inner: backend,
            backend_type,
        }
    }

    /// Returns the resolved backend category.
    pub fn backend_type(&self) -> ResolvedBackendType {
        self.backend_type
    }

    /// Returns a reference to the active inner backend.
    pub fn inner(&self) -> &dyn ClipboardBackend {
        &*self.inner
    }

    /// Returns a mutable reference to the active inner backend.
    pub fn inner_mut(&mut self) -> &mut dyn ClipboardBackend {
        &mut *self.inner
    }
}

impl ClipboardBackend for AutoClipboardBackend {
    fn get_text(&self) -> Result<String, InputError> {
        self.inner.get_text()
    }

    fn set_text(&mut self, text: &str) -> Result<(), InputError> {
        self.inner.set_text(text)
    }

    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_clipboard_creation() {
        let backend = AutoClipboardBackend::new();
        assert!(backend.is_available());
        assert!(!backend.name().is_empty());
    }

    #[test]
    fn test_auto_clipboard_with_mock() {
        let mock = Box::new(MockClipboardBackend::with_contents("auto-mock"));
        let mut auto = AutoClipboardBackend::with_backend(mock, ResolvedBackendType::Mock);
        assert_eq!(auto.backend_type(), ResolvedBackendType::Mock);
        assert_eq!(auto.get_text().unwrap(), "auto-mock");

        auto.set_text("updated").unwrap();
        assert_eq!(auto.get_text().unwrap(), "updated");
    }
}
