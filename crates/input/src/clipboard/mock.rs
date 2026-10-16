//! Mock in-memory clipboard backend for headless testing.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::clipboard::backend::ClipboardBackend;
use crate::error::InputError;

/// In-memory mock clipboard backend.
#[derive(Debug, Clone, Default)]
pub struct MockClipboardBackend {
    contents: Arc<Mutex<String>>,
    history: Arc<Mutex<Vec<String>>>,
    available: Arc<Mutex<bool>>,
    error_on_get: Arc<Mutex<Option<String>>>,
    error_on_set: Arc<Mutex<Option<String>>>,
}

impl MockClipboardBackend {
    pub fn new() -> Self {
        Self {
            contents: Arc::new(Mutex::new(String::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            available: Arc::new(Mutex::new(true)),
            error_on_get: Arc::new(Mutex::new(None)),
            error_on_set: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_contents(contents: impl Into<String>) -> Self {
        let text = contents.into();
        Self {
            contents: Arc::new(Mutex::new(text.clone())),
            history: Arc::new(Mutex::new(vec![text])),
            available: Arc::new(Mutex::new(true)),
            error_on_get: Arc::new(Mutex::new(None)),
            error_on_set: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_available(&self, available: bool) {
        *self.available.lock() = available;
    }

    pub fn set_error_on_get(&self, error: Option<String>) {
        *self.error_on_get.lock() = error;
    }

    pub fn set_error_on_set(&self, error: Option<String>) {
        *self.error_on_set.lock() = error;
    }

    pub fn history(&self) -> Vec<String> {
        self.history.lock().clone()
    }
}

impl ClipboardBackend for MockClipboardBackend {
    fn get_text(&self) -> Result<String, InputError> {
        if let Some(ref err) = *self.error_on_get.lock() {
            return Err(InputError::ClipboardError(err.clone()));
        }
        Ok(self.contents.lock().clone())
    }

    fn set_text(&mut self, text: &str) -> Result<(), InputError> {
        if let Some(ref err) = *self.error_on_set.lock() {
            return Err(InputError::ClipboardError(err.clone()));
        }
        *self.contents.lock() = text.to_string();
        self.history.lock().push(text.to_string());
        Ok(())
    }

    fn is_available(&self) -> bool {
        *self.available.lock()
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_clipboard_roundtrip() {
        let mut mock = MockClipboardBackend::new();
        assert_eq!(mock.get_text().unwrap(), "");

        mock.set_text("Copied to mock").unwrap();
        assert_eq!(mock.get_text().unwrap(), "Copied to mock");
        assert_eq!(mock.history(), vec!["Copied to mock"]);
    }

    #[test]
    fn test_mock_clipboard_error_injection() {
        let mut mock = MockClipboardBackend::new();
        mock.set_error_on_get(Some("Simulated read error".to_string()));
        assert!(mock.get_text().is_err());

        mock.set_error_on_set(Some("Simulated write error".to_string()));
        assert!(mock.set_text("test").is_err());
    }
}
