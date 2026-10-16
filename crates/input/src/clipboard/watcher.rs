//! Clipboard watcher and poller with debouncing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::clipboard::backend::ClipboardBackend;
use crate::error::InputError;

/// Default interval for polling host clipboard changes (200ms).
pub const DEFAULT_CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Default debounce stabilization duration before emitting a clipboard change (100ms).
pub const DEFAULT_CLIPBOARD_DEBOUNCE_DURATION: Duration = Duration::from_millis(100);

/// Configuration for `ClipboardWatcher`.
#[derive(Debug, Clone)]
pub struct ClipboardWatcherConfig {
    pub poll_interval: Duration,
    pub debounce_duration: Duration,
}

impl Default for ClipboardWatcherConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_CLIPBOARD_POLL_INTERVAL,
            debounce_duration: DEFAULT_CLIPBOARD_DEBOUNCE_DURATION,
        }
    }
}

/// Watches and polls a `ClipboardBackend` for changes with debouncing and suppression.
pub struct ClipboardWatcher<B: ClipboardBackend> {
    backend: Arc<Mutex<B>>,
    poll_interval: Duration,
    debounce_duration: Duration,
    last_confirmed_text: Option<String>,
    pending_candidate: Option<(String, Instant)>,
    suppressed_text: Option<String>,
}

impl<B: ClipboardBackend + 'static> ClipboardWatcher<B> {
    /// Creates a new watcher wrapping the given backend with default timing.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self::with_config(backend, ClipboardWatcherConfig::default())
    }

    /// Creates a new watcher wrapping the given backend with custom configuration.
    pub fn with_config(backend: Arc<Mutex<B>>, config: ClipboardWatcherConfig) -> Self {
        Self {
            backend,
            poll_interval: config.poll_interval,
            debounce_duration: config.debounce_duration,
            last_confirmed_text: Some(String::new()),
            pending_candidate: None,
            suppressed_text: None,
        }
    }

    /// Suppresses emission of the next occurrence of `text` (used when updating clipboard from remote).
    pub fn suppress_next(&mut self, text: &str) {
        self.suppressed_text = Some(text.to_string());
        self.last_confirmed_text = Some(text.to_string());
        self.pending_candidate = None;
    }

    /// Explicitly updates the known clipboard text baseline without emitting an event.
    pub fn set_known_text(&mut self, text: &str) {
        self.last_confirmed_text = Some(text.to_string());
        self.pending_candidate = None;
    }

    /// Polls the clipboard backend once.
    ///
    /// Returns `Ok(Some(text))` if a change has stabilized past `debounce_duration`,
    /// or `Ok(None)` if no change, still debouncing, or suppressed.
    pub fn poll_once(&mut self) -> Result<Option<String>, InputError> {
        let current_text = {
            let backend = self.backend.lock();
            backend.get_text()?
        };

        // Suppressed text check (e.g. echo from remote write)
        if let Some(ref suppressed) = self.suppressed_text {
            if suppressed == &current_text {
                self.suppressed_text = None;
                self.last_confirmed_text = Some(current_text);
                self.pending_candidate = None;
                return Ok(None);
            }
        }

        // Unchanged text check
        if let Some(ref confirmed) = self.last_confirmed_text {
            if confirmed == &current_text {
                self.pending_candidate = None;
                return Ok(None);
            }
        }

        // Text changed relative to last confirmed text
        if self.debounce_duration.is_zero() {
            self.last_confirmed_text = Some(current_text.clone());
            self.pending_candidate = None;
            return Ok(Some(current_text));
        }

        let now = Instant::now();
        if let Some((ref candidate_text, first_seen)) = self.pending_candidate {
            if candidate_text == &current_text {
                if now.duration_since(first_seen) >= self.debounce_duration {
                    self.last_confirmed_text = Some(current_text.clone());
                    self.pending_candidate = None;
                    return Ok(Some(current_text));
                }
                // Still waiting for debounce window to elapse
                return Ok(None);
            }
        }

        // New candidate seen
        self.pending_candidate = Some((current_text, now));
        Ok(None)
    }

    /// Poll interval getter.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Debounce duration getter.
    pub fn debounce_duration(&self) -> Duration {
        self.debounce_duration
    }

    /// Starts a background polling task that sends stabilized changed text down an `mpsc` channel.
    pub fn spawn_watch_loop(mut self, running: Arc<AtomicBool>) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let poll_interval = self.poll_interval;

        tokio::spawn(async move {
            while running.load(Ordering::Relaxed) {
                match self.poll_once() {
                    Ok(Some(changed_text)) => {
                        if tx.send(changed_text).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::trace!(error = ?e, "ClipboardWatcher error during poll");
                    }
                }
                tokio::time::sleep(poll_interval).await;
            }
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::mock::MockClipboardBackend;

    #[test]
    fn test_clipboard_watcher_immediate_when_no_debounce() {
        let mock = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let mut watcher = ClipboardWatcher::with_config(
            mock.clone(),
            ClipboardWatcherConfig {
                poll_interval: Duration::from_millis(10),
                debounce_duration: Duration::ZERO,
            },
        );

        // Initially empty
        assert_eq!(watcher.poll_once().unwrap(), None);

        // Change mock text
        mock.lock().set_text("First change").unwrap();
        assert_eq!(
            watcher.poll_once().unwrap(),
            Some("First change".to_string())
        );

        // Next poll without change returns None
        assert_eq!(watcher.poll_once().unwrap(), None);
    }

    #[test]
    fn test_clipboard_watcher_debounce_window() {
        let mock = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let debounce = Duration::from_millis(50);
        let mut watcher = ClipboardWatcher::with_config(
            mock.clone(),
            ClipboardWatcherConfig {
                poll_interval: Duration::from_millis(5),
                debounce_duration: debounce,
            },
        );

        mock.lock().set_text("Pending text").unwrap();

        // First poll detects candidate
        assert_eq!(watcher.poll_once().unwrap(), None);

        // Wait for debounce window
        std::thread::sleep(debounce + Duration::from_millis(10));

        // Next poll confirms candidate
        assert_eq!(
            watcher.poll_once().unwrap(),
            Some("Pending text".to_string())
        );
    }

    #[test]
    fn test_clipboard_watcher_suppress_next() {
        let mock = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let mut watcher = ClipboardWatcher::with_config(
            mock.clone(),
            ClipboardWatcherConfig {
                poll_interval: Duration::from_millis(10),
                debounce_duration: Duration::ZERO,
            },
        );

        // Suppress "Remote copied text"
        watcher.suppress_next("Remote copied text");
        mock.lock().set_text("Remote copied text").unwrap();

        // Must NOT emit
        assert_eq!(watcher.poll_once().unwrap(), None);

        // Now a local change occurs
        mock.lock().set_text("Local text").unwrap();
        assert_eq!(watcher.poll_once().unwrap(), Some("Local text".to_string()));
    }
}
