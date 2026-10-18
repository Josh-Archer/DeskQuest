//! Host-side clipboard synchronization service.
//!
//! Connects the Linux clipboard backend (Wayland or X11) to the reliable control channel,
//! handling debounced change detection, echo suppression, and last-writer conflict resolution.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use linux_quest_input::{
    AutoClipboardBackend, ClipboardBackend, ClipboardWatcher, ClipboardWatcherConfig,
    DEFAULT_CLIPBOARD_DEBOUNCE_DURATION, DEFAULT_CLIPBOARD_POLL_INTERVAL,
};
use linux_quest_protocol::{ClipboardMessage, ClipboardSyncEngine, SyncAction};
use linux_quest_transport::{ControlChannel, ControlMessage, TransportError};

use crate::link_status::SharedLinkTracker;
use crate::HostError;

/// Idle time before the first TCP keepalive probe on an accepted control socket.
pub const CONTROL_KEEPALIVE_IDLE: Duration = Duration::from_secs(5);
/// Interval between TCP keepalive probes.
pub const CONTROL_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// Unanswered probes before the kernel resets the connection (about 9s of silence in total).
pub const CONTROL_KEEPALIVE_RETRIES: u32 = 2;

/// Enables short-interval TCP keepalive so a headset that vanishes without a FIN
/// (Wi-Fi drop, power off) is detected within seconds instead of the kernel default of hours.
pub fn enable_control_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    let ka = socket2::TcpKeepalive::new()
        .with_time(CONTROL_KEEPALIVE_IDLE)
        .with_interval(CONTROL_KEEPALIVE_INTERVAL);
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    let ka = ka.with_retries(CONTROL_KEEPALIVE_RETRIES);
    socket2::SockRef::from(stream).set_tcp_keepalive(&ka)
}

/// Disables Nagle on the control socket. Control messages are small and sent one at a time,
/// so Nagle plus the peer's delayed ACK can hold a reply back for tens of milliseconds.
pub fn enable_control_nodelay(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)
}

/// Host origin ID for clipboard synchronization.
pub const HOST_CLIPBOARD_ORIGIN_ID: u64 = 1;

/// Host service managing bidirectional clipboard synchronization.
pub struct HostClipboardService<B: ClipboardBackend + 'static = AutoClipboardBackend> {
    backend: Arc<Mutex<B>>,
    sync_engine: Arc<Mutex<ClipboardSyncEngine>>,
    watcher: ClipboardWatcher<B>,
    running: Arc<AtomicBool>,
    outgoing_tx: mpsc::UnboundedSender<ControlMessage>,
    incoming_rx: mpsc::UnboundedReceiver<ClipboardMessage>,
    listener: Option<TcpListener>,
    peer_writers: Arc<Mutex<Vec<mpsc::UnboundedSender<ControlMessage>>>>,
    link: Option<SharedLinkTracker>,
}

impl<B: ClipboardBackend + 'static> HostClipboardService<B> {
    /// Creates a new `HostClipboardService` with explicit backend and channels.
    pub fn new(
        backend: Arc<Mutex<B>>,
        sync_engine: Arc<Mutex<ClipboardSyncEngine>>,
        outgoing_tx: mpsc::UnboundedSender<ControlMessage>,
        incoming_rx: mpsc::UnboundedReceiver<ClipboardMessage>,
    ) -> Self {
        let watcher = ClipboardWatcher::with_config(
            backend.clone(),
            ClipboardWatcherConfig {
                poll_interval: DEFAULT_CLIPBOARD_POLL_INTERVAL,
                debounce_duration: DEFAULT_CLIPBOARD_DEBOUNCE_DURATION,
            },
        );

        Self {
            backend,
            sync_engine,
            watcher,
            running: Arc::new(AtomicBool::new(false)),
            outgoing_tx,
            incoming_rx,
            listener: None,
            peer_writers: Arc::new(Mutex::new(Vec::new())),
            link: None,
        }
    }

    /// Reports accepted and closed control connections to the shared link tracker.
    pub fn with_link_tracker(mut self, link: SharedLinkTracker) -> Self {
        self.link = Some(link);
        self
    }

    /// Customizes the watcher polling and debouncing configuration.
    pub fn with_watcher_config(mut self, config: ClipboardWatcherConfig) -> Self {
        self.watcher = ClipboardWatcher::with_config(self.backend.clone(), config);
        self
    }

    /// Returns a handle to the `running` atomic flag.
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    /// Returns a reference to the active clipboard backend.
    pub fn backend(&self) -> Arc<Mutex<B>> {
        self.backend.clone()
    }

    /// Returns a reference to the synchronization engine.
    pub fn sync_engine(&self) -> Arc<Mutex<ClipboardSyncEngine>> {
        self.sync_engine.clone()
    }

    /// Checks the local clipboard for changes.
    ///
    /// If changed and stabilized, processes the change via `ClipboardSyncEngine`,
    /// sends messages to the outgoing channel and connected peers, and returns them.
    pub fn step_local(&mut self) -> Result<Option<Vec<ClipboardMessage>>, HostError> {
        if let Some(changed_text) = self.watcher.poll_once()? {
            let messages_opt = {
                let mut engine = self.sync_engine.lock();
                engine.process_local_change(&changed_text)?
            };

            if let Some(messages) = messages_opt {
                for msg in &messages {
                    let ctrl_msg = ControlMessage::Clipboard(msg.clone());
                    let _ = self.outgoing_tx.send(ctrl_msg.clone());

                    // Broadcast to active network peers
                    let mut peers = self.peer_writers.lock();
                    peers.retain(|peer_tx| peer_tx.send(ctrl_msg.clone()).is_ok());
                }
                return Ok(Some(messages));
            }
        }

        Ok(None)
    }

    /// Processes an incoming `ClipboardMessage` from a remote client.
    ///
    /// Returns `Ok(true)` if the message was accepted, won conflict resolution,
    /// and was applied to the host clipboard backend.
    pub fn handle_incoming(&mut self, msg: ClipboardMessage) -> Result<bool, HostError> {
        let action = {
            let mut engine = self.sync_engine.lock();
            engine.process_incoming_message(msg)?
        };

        match action {
            SyncAction::ApplyToLocal(text) => {
                {
                    let mut backend = self.backend.lock();
                    backend.set_text(&text)?;
                }
                self.watcher.suppress_next(&text);
                Ok(true)
            }
            SyncAction::Reassembling
            | SyncAction::EchoSuppressed
            | SyncAction::ConflictLost
            | SyncAction::Ignored => Ok(false),
        }
    }

    /// Runs the clipboard service until `running` flag is cleared.
    pub async fn run(&mut self) -> Result<(), HostError> {
        self.running.store(true, Ordering::SeqCst);
        let poll_interval = self.watcher.poll_interval();

        loop {
            if !self.running.load(Ordering::Relaxed) {
                break;
            }

            // Step local backend change detection
            if let Err(e) = self.step_local() {
                tracing::trace!(error = ?e, "Error during local clipboard poll step");
            }

            // Process any incoming messages queued from channel
            while let Ok(msg) = self.incoming_rx.try_recv() {
                if let Err(e) = self.handle_incoming(msg) {
                    tracing::warn!(error = ?e, "Error handling incoming clipboard message");
                }
            }

            // If a TCP listener is configured, accept incoming control channel connections
            if let Some(ref listener) = self.listener {
                // Non-blocking accept check
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        tracing::info!(peer = %peer_addr, "New control channel connection for clipboard sync");
                        if let Err(e) = enable_control_keepalive(&stream) {
                            tracing::warn!(error = ?e, "Failed to enable TCP keepalive on control socket");
                        }
                        if let Err(e) = enable_control_nodelay(&stream) {
                            tracing::warn!(error = ?e, "Failed to set TCP_NODELAY on control socket");
                        }
                        if let Some(link) = &self.link {
                            link.lock().control_accepted(peer_addr, Instant::now());
                        }
                        self.spawn_peer_handler(stream, peer_addr);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        tracing::trace!(error = ?e, "TCP listener accept error");
                    }
                }
            } else {
                tokio::time::sleep(poll_interval).await;
            }
        }

        Ok(())
    }

    fn spawn_peer_handler(&self, stream: TcpStream, peer_addr: SocketAddr) {
        let (reader, writer) = stream.into_split();
        let mut ctrl_reader = ControlChannel::new(reader, tokio::io::sink());
        let mut ctrl_writer = ControlChannel::new(tokio::io::empty(), writer);

        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ControlMessage>();
        self.peer_writers.lock().push(peer_tx);

        let sync_engine = self.sync_engine.clone();
        let backend = self.backend.clone();
        let running = self.running.clone();

        // Task sending outgoing messages to peer
        let running_writer = running.clone();
        tokio::spawn(async move {
            while running_writer.load(Ordering::Relaxed) {
                match peer_rx.recv().await {
                    Some(msg) => {
                        if ctrl_writer.send_message(&msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });

        // Task receiving incoming messages from peer; reports the close when it ends
        let link = self.link.clone();
        tokio::spawn(async move {
            while running.load(Ordering::Relaxed) {
                let received = ctrl_reader.recv_message().await;
                if let (Ok(_), Some(link)) = (&received, &link) {
                    link.lock().control_activity(Instant::now());
                }
                match received {
                    Ok(ControlMessage::Clipboard(clip_msg)) => {
                        let action = {
                            let mut engine = sync_engine.lock();
                            engine.process_incoming_message(clip_msg)
                        };

                        if let Ok(SyncAction::ApplyToLocal(text)) = action {
                            let mut b = backend.lock();
                            let _ = b.set_text(&text);
                        }
                    }
                    Ok(ControlMessage::Disconnect { .. }) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if let Some(link) = link {
                link.lock().control_closed(peer_addr);
            }
        });
    }
}

impl HostClipboardService<AutoClipboardBackend> {
    /// Initializes `HostClipboardService` with `AutoClipboardBackend`.
    pub fn new_auto() -> (
        Self,
        mpsc::UnboundedSender<ClipboardMessage>,
        mpsc::UnboundedReceiver<ControlMessage>,
    ) {
        let backend = Arc::new(Mutex::new(AutoClipboardBackend::new()));
        let sync_engine = Arc::new(Mutex::new(ClipboardSyncEngine::new(
            HOST_CLIPBOARD_ORIGIN_ID,
        )));

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let service = Self::new(backend, sync_engine, outgoing_tx, incoming_rx);
        (service, incoming_tx, outgoing_rx)
    }

    /// Address the control listener is bound to, or `None` if no listener was bound.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().and_then(|l| l.local_addr().ok())
    }

    /// Binds a TCP listener to `bind_addr` for accepting client control channel connections.
    pub async fn bind_auto(bind_addr: SocketAddr) -> Result<Self, HostError> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|e| HostError::Transport(TransportError::Io(e)))?;

        let (mut service, _, _) = Self::new_auto();
        service.listener = Some(listener);
        Ok(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_input::MockClipboardBackend;

    #[tokio::test]
    async fn test_control_keepalive_is_enabled_on_accepted_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        enable_control_keepalive(&stream).unwrap();
        let sock = socket2::SockRef::from(&stream);
        assert!(sock.keepalive().unwrap());
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            assert_eq!(sock.tcp_keepalive_time().unwrap(), CONTROL_KEEPALIVE_IDLE);
            assert_eq!(
                sock.tcp_keepalive_interval().unwrap(),
                CONTROL_KEEPALIVE_INTERVAL
            );
            assert_eq!(
                sock.tcp_keepalive_retries().unwrap(),
                CONTROL_KEEPALIVE_RETRIES
            );
        }
    }

    #[tokio::test]
    async fn test_control_nodelay_is_enabled_on_accepted_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        assert!(
            !stream.nodelay().unwrap(),
            "accepted sockets start with Nagle on"
        );
        enable_control_nodelay(&stream).unwrap();
        assert!(stream.nodelay().unwrap());
    }

    #[test]
    fn test_host_clipboard_service_local_change_emits_message() {
        let mock_backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let sync_engine = Arc::new(Mutex::new(ClipboardSyncEngine::new(
            HOST_CLIPBOARD_ORIGIN_ID,
        )));

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let mut service =
            HostClipboardService::new(mock_backend.clone(), sync_engine, outgoing_tx, incoming_rx)
                .with_watcher_config(ClipboardWatcherConfig {
                    poll_interval: Duration::from_millis(5),
                    debounce_duration: Duration::ZERO,
                });

        // Set mock text
        mock_backend.lock().set_text("Linux desktop text").unwrap();

        let messages = service.step_local().unwrap().expect("Should emit message");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].origin_id, HOST_CLIPBOARD_ORIGIN_ID);
        assert_eq!(messages[0].text, "Linux desktop text");

        let ctrl_msg = outgoing_rx.try_recv().unwrap();
        match ctrl_msg {
            ControlMessage::Clipboard(m) => {
                assert_eq!(m.text, "Linux desktop text");
            }
            other => panic!("Expected Clipboard message, got {:?}", other),
        }
    }

    #[test]
    fn test_host_clipboard_service_incoming_message_applies_and_suppresses_echo() {
        let mock_backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let sync_engine = Arc::new(Mutex::new(ClipboardSyncEngine::new(
            HOST_CLIPBOARD_ORIGIN_ID,
        )));

        let (outgoing_tx, _outgoing_rx) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let mut service =
            HostClipboardService::new(mock_backend.clone(), sync_engine, outgoing_tx, incoming_rx)
                .with_watcher_config(ClipboardWatcherConfig {
                    poll_interval: Duration::from_millis(5),
                    debounce_duration: Duration::ZERO,
                });

        // Client sends update with version 1 from origin 2
        let client_msg = ClipboardMessage::single(2, 1, "From Quest client".to_string()).unwrap();
        let applied = service.handle_incoming(client_msg).unwrap();
        assert!(applied);

        // Verify backend was updated
        assert_eq!(mock_backend.lock().get_text().unwrap(), "From Quest client");

        // Step local poll: must NOT emit an echo
        let echo = service.step_local().unwrap();
        assert!(
            echo.is_none(),
            "Echo was not suppressed: host re-emitted client's write!"
        );
    }

    #[tokio::test]
    async fn test_host_clipboard_bind_auto() {
        let service = HostClipboardService::bind_auto("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        assert!(service.listener.is_some());
    }
}
