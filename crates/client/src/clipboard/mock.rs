//! In-memory mock clipboard manager for unit tests.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::clipboard::manager::ClipboardManager;
use crate::error::ClientError;

/// In-memory mock clipboard manager.
#[derive(Debug, Clone, Default)]
pub struct MockClipboardManager {
    contents: Arc<Mutex<String>>,
    history: Arc<Mutex<Vec<String>>>,
    available: Arc<Mutex<bool>>,
    error_on_get: Arc<Mutex<Option<String>>>,
    error_on_set: Arc<Mutex<Option<String>>>,
}

impl MockClipboardManager {
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

impl ClipboardManager for MockClipboardManager {
    fn get_text(&self) -> Result<String, ClientError> {
        if let Some(ref err) = *self.error_on_get.lock() {
            return Err(ClientError::Android(err.clone()));
        }
        Ok(self.contents.lock().clone())
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClientError> {
        if let Some(ref err) = *self.error_on_set.lock() {
            return Err(ClientError::Android(err.clone()));
        }
        *self.contents.lock() = text.to_string();
        self.history.lock().push(text.to_string());
        Ok(())
    }

    fn is_available(&self) -> bool {
        *self.available.lock()
    }

    fn name(&self) -> &'static str {
        "mock-client"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_mock_clipboard_roundtrip() {
        let mut mock = MockClipboardManager::new();
        assert_eq!(mock.get_text().unwrap(), "");

        mock.set_text("Quest 3 VR clipboard").unwrap();
        assert_eq!(mock.get_text().unwrap(), "Quest 3 VR clipboard");
        assert_eq!(mock.history(), vec!["Quest 3 VR clipboard"]);
    }

    #[test]
    fn test_client_mock_clipboard_errors() {
        let mut mock = MockClipboardManager::new();
        mock.set_error_on_get(Some("JNI call failed".to_string()));
        assert!(mock.get_text().is_err());

        mock.set_error_on_set(Some("Write failed".to_string()));
        assert!(mock.set_text("test").is_err());
    }
}
