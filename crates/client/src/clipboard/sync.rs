//! Client clipboard synchronization engine integration.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::clipboard::manager::ClipboardManager;
use crate::error::ClientError;
use linux_quest_protocol::{ClipboardMessage, ClipboardSyncEngine, SyncAction};

/// Default poll interval for client clipboard (250ms).
pub const DEFAULT_CLIENT_CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default debounce duration for client clipboard (100ms).
pub const DEFAULT_CLIENT_CLIPBOARD_DEBOUNCE_DURATION: Duration = Duration::from_millis(100);

/// Synchronizes the Quest client clipboard with the Linux host streaming session.
pub struct ClientClipboardSync<C: ClipboardManager> {
    clipboard: Arc<Mutex<C>>,
    sync_engine: ClipboardSyncEngine,
    poll_interval: Duration,
    debounce_duration: Duration,
    last_confirmed_text: Option<String>,
    pending_candidate: Option<(String, Instant)>,
    suppressed_text: Option<String>,
}

impl<C: ClipboardManager> ClientClipboardSync<C> {
    /// Creates a new client clipboard synchronizer for the given origin ID (typically 2 for Quest client).
    pub fn new(clipboard: Arc<Mutex<C>>, origin_id: u64) -> Self {
        Self {
            clipboard,
            sync_engine: ClipboardSyncEngine::new(origin_id),
            poll_interval: DEFAULT_CLIENT_CLIPBOARD_POLL_INTERVAL,
            debounce_duration: DEFAULT_CLIENT_CLIPBOARD_DEBOUNCE_DURATION,
            last_confirmed_text: Some(String::new()),
            pending_candidate: None,
            suppressed_text: None,
        }
    }

    /// Sets custom timing for polling and debouncing.
    pub fn with_timing(mut self, poll_interval: Duration, debounce_duration: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.debounce_duration = debounce_duration;
        self
    }

    /// Suppresses emission of the next occurrence of `text` locally to prevent echo loops.
    pub fn suppress_next(&mut self, text: &str) {
        self.suppressed_text = Some(text.to_string());
        self.last_confirmed_text = Some(text.to_string());
        self.pending_candidate = None;
        self.sync_engine.mark_suppression(text);
    }

    /// Polls the local client clipboard. If a stabilized change is detected,
    /// evaluates it through `ClipboardSyncEngine` and returns outgoing `ClipboardMessage`s.
    pub fn poll_local(&mut self) -> Result<Option<Vec<ClipboardMessage>>, ClientError> {
        let current_text = {
            let clipboard = self.clipboard.lock();
            clipboard.get_text()?
        };

        // Suppressed text check
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

        if self.debounce_duration.is_zero() {
            self.last_confirmed_text = Some(current_text.clone());
            self.pending_candidate = None;
            let msgs = self
                .sync_engine
                .process_local_change(&current_text)
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            return Ok(msgs);
        }

        let now = Instant::now();
        if let Some((ref candidate_text, first_seen)) = self.pending_candidate {
            if candidate_text == &current_text {
                if now.duration_since(first_seen) >= self.debounce_duration {
                    self.last_confirmed_text = Some(current_text.clone());
                    self.pending_candidate = None;
                    let msgs = self
                        .sync_engine
                        .process_local_change(&current_text)
                        .map_err(|e| ClientError::Protocol(e.to_string()))?;
                    return Ok(msgs);
                }
                return Ok(None);
            }
        }

        self.pending_candidate = Some((current_text, now));
        Ok(None)
    }

    /// Ingests an incoming `ClipboardMessage` from the host over the reliable control channel.
    ///
    /// Returns `Ok(true)` if the message won conflict resolution and was written to the client clipboard.
    pub fn handle_incoming(&mut self, msg: ClipboardMessage) -> Result<bool, ClientError> {
        match self
            .sync_engine
            .process_incoming_message(msg)
            .map_err(|e| ClientError::Protocol(e.to_string()))?
        {
            SyncAction::ApplyToLocal(text) => {
                {
                    let mut clipboard = self.clipboard.lock();
                    clipboard.set_text(&text)?;
                }
                self.suppress_next(&text);
                Ok(true)
            }
            SyncAction::Reassembling
            | SyncAction::EchoSuppressed
            | SyncAction::ConflictLost
            | SyncAction::Ignored => Ok(false),
        }
    }

    /// Returns a reference to the inner sync engine.
    pub fn sync_engine(&self) -> &ClipboardSyncEngine {
        &self.sync_engine
    }

    /// Returns a reference to the client clipboard manager.
    pub fn clipboard(&self) -> Arc<Mutex<C>> {
        self.clipboard.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::mock::MockClipboardManager;

    #[test]
    fn test_client_sync_local_change_and_echo_suppression() {
        let mock = Arc::new(Mutex::new(MockClipboardManager::new()));
        let mut client_sync = ClientClipboardSync::new(mock.clone(), 2)
            .with_timing(Duration::from_millis(10), Duration::ZERO);

        // Initially empty
        assert!(client_sync.poll_local().unwrap().is_none());

        // Client copies text locally
        mock.lock().set_text("Quest locally copied").unwrap();
        let outgoing = client_sync.poll_local().unwrap().unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].origin_id, 2);
        assert_eq!(outgoing[0].text, "Quest locally copied");

        // Host writes new clipboard text over control channel
        let host_msg = ClipboardMessage::single(1, 10, "Host desktop text".to_string()).unwrap();
        let applied = client_sync.handle_incoming(host_msg).unwrap();
        assert!(applied);
        assert_eq!(mock.lock().get_text().unwrap(), "Host desktop text");

        // Subsequent poll must NOT re-emit host text (echo loop broken)
        let echo = client_sync.poll_local().unwrap();
        assert!(echo.is_none());
    }
}
