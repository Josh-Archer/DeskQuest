//! Clipboard backend abstraction and host implementations.

use crate::error::InputError;

/// Trait defining the host clipboard access interface.
pub trait ClipboardBackend: Send + Sync {
    /// Reads the current text from the host clipboard.
    fn get_text(&self) -> Result<String, InputError>;

    /// Sets the text on the host clipboard.
    fn set_text(&mut self, text: &str) -> Result<(), InputError>;

    /// Returns `true` if this backend is currently available on the host system.
    fn is_available(&self) -> bool {
        true
    }

    /// Returns the descriptive name of the backend.
    fn name(&self) -> &'static str;
}

impl<T: ?Sized + ClipboardBackend> ClipboardBackend for Box<T> {
    fn get_text(&self) -> Result<String, InputError> {
        (**self).get_text()
    }

    fn set_text(&mut self, text: &str) -> Result<(), InputError> {
        (**self).set_text(text)
    }

    fn is_available(&self) -> bool {
        (**self).is_available()
    }

    fn name(&self) -> &'static str {
        (**self).name()
    }
}
