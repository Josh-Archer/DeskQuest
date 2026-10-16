//! Client clipboard trait interface for Meta Quest.

use crate::error::ClientError;

/// Trait defining the clipboard manager on the Quest client.
pub trait ClipboardManager: Send + Sync {
    /// Reads text from the device clipboard.
    fn get_text(&self) -> Result<String, ClientError>;

    /// Writes text to the device clipboard.
    fn set_text(&mut self, text: &str) -> Result<(), ClientError>;

    /// Returns `true` if this clipboard manager is available on the current target.
    fn is_available(&self) -> bool {
        true
    }

    /// Descriptive name of the clipboard implementation.
    fn name(&self) -> &'static str;
}

impl<T: ?Sized + ClipboardManager> ClipboardManager for Box<T> {
    fn get_text(&self) -> Result<String, ClientError> {
        (**self).get_text()
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClientError> {
        (**self).set_text(text)
    }

    fn is_available(&self) -> bool {
        (**self).is_available()
    }

    fn name(&self) -> &'static str {
        (**self).name()
    }
}
