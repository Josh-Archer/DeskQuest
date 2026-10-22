//! Host-side clipboard synchronization service.
//!
//! Connects the Linux clipboard backend (Wayland or X11) to the reliable control channel,
//! handling debounced change detection, echo suppression, and last-writer conflict resolution.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};

use linux_quest_input::{
    AutoClipboardBackend, ClipboardBackend, ClipboardWatcher, ClipboardWatcherConfig,
    DEFAULT_CLIPBOARD_DEBOUNCE_DURATION, DEFAULT_CLIPBOARD_POLL_INTERVAL,
};
use linux_quest_protocol::Packet;
use linux_quest_protocol::{ClipboardMessage, ClipboardSyncEngine, SyncAction};
use linux_quest_transport::{
    negotiate_server, recv_frame, send_frame, ControlMessage, TransportError, TunnelFrame,
};

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
    tunnel_packet_handler: Option<Arc<dyn Fn(Packet) + Send + Sync>>,
    tunnel_video: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    tunnel_keyframe_request: Option<Arc<dyn Fn() + Send + Sync>>,
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
            tunnel_packet_handler: None,
            tunnel_video: None,
            tunnel_keyframe_request: None,
        }
    }

    /// Reports accepted and closed control connections to the shared link tracker.
    pub fn with_link_tracker(mut self, link: SharedLinkTracker) -> Self {
        self.link = Some(link);
        self
    }

    /// Routes packet frames from negotiated USB tunnel peers to the host's packet handler.
    pub fn with_tunnel_packet_handler(
        mut self,
        handler: Arc<dyn Fn(Packet) + Send + Sync>,
    ) -> Self {
        self.tunnel_packet_handler = Some(handler);
        self
    }

    /// Adds the outbound packet stream used by negotiated USB tunnel peers.
    pub fn with_tunnel_video(mut self, video: tokio::sync::broadcast::Sender<Vec<u8>>) -> Self {
        self.tunnel_video = Some(video);
        self
    }

    pub fn with_tunnel_keyframe_request(mut self, request: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.tunnel_keyframe_request = Some(request);
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
                        if let Err(e) = stream.set_nodelay(true) {
                            tracing::warn!(error = ?e, "Failed to set TCP_NODELAY on control socket");
                        }
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
        let running = self.running.clone();
        let link = self.link.clone();
        let sync_engine = self.sync_engine.clone();
        let backend = self.backend.clone();
        let packet_handler = self.tunnel_packet_handler.clone();
        let keyframe_request = self.tunnel_keyframe_request.clone();
        let mut video_rx = self.tunnel_video.as_ref().map(|tx| tx.subscribe());
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ControlMessage>();
        self.peer_writers.lock().push(peer_tx);
        tokio::spawn(async move {
            let mut probe = [0u8; linux_quest_transport::USB_TUNNEL_HELLO.len()];
            let probe_result = tokio::time::timeout(
                linux_quest_transport::tcp_tunnel::TUNNEL_NEGOTIATION_TIMEOUT,
                async {
                    loop {
                        match stream.peek(&mut probe).await {
                            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof),
                            Ok(n) if n == probe.len() => return Ok(()),
                            Ok(_) => tokio::task::yield_now().await,
                            Err(error) => return Err(error.kind()),
                        }
                    }
                },
            )
            .await;
            if !matches!(probe_result, Ok(Ok(()))) {
                if let Some(link) = link {
                    link.lock().control_closed(peer_addr);
                }
                return;
            }
            let is_tunnel = probe == *linux_quest_transport::USB_TUNNEL_HELLO;
            let (reader, writer) = stream.into_split();
            let mut writer = writer;
            if is_tunnel {
                let mut reader = reader;
                let negotiated = tokio::time::timeout(
                    linux_quest_transport::tcp_tunnel::TUNNEL_NEGOTIATION_TIMEOUT,
                    negotiate_server(&mut reader, &mut writer),
                )
                .await;
                if !matches!(negotiated, Ok(Ok(()))) {
                    if let Some(link) = link {
                        link.lock().control_closed(peer_addr);
                    }
                    return;
                }
                let (incoming_tx, mut incoming_rx) = mpsc::channel(32);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(reader);
                    loop {
                        let result = recv_frame(&mut reader).await;
                        let failed = result.is_err();
                        if incoming_tx.send(result).await.is_err() || failed {
                            break;
                        }
                    }
                });
                while running.load(Ordering::Relaxed) {
                    let received = tokio::select! {
                        outgoing = peer_rx.recv() => match outgoing {
                            Some(message) => {
                                if send_frame(&mut writer, &TunnelFrame::Control(message)).await.is_err() { break; }
                                continue;
                            }
                            None => break,
                        },
                        video = async {
                            match video_rx.as_mut() {
                                Some(rx) => Some(rx.recv().await),
                                None => std::future::pending().await,
                            }
                        } => {
                            match video {
                                Some(result) => {
                                    let (packet, closed) = handle_video_result(result, keyframe_request.as_ref());
                                    if closed { break; }
                                    if let Some(bytes) = packet {
                                        if send_frame(&mut writer, &TunnelFrame::Packet(bytes)).await.is_err() { break; }
                                    }
                                }
                                None => break,
                            }
                            continue;
                        },
                        incoming = incoming_rx.recv() => incoming,
                    };
                    let Some(received) = received else { break };
                    if let (Ok(_), Some(link)) = (&received, &link) {
                        link.lock().control_activity(Instant::now());
                    }
                    match received {
                        Ok(TunnelFrame::Control(ControlMessage::Clipboard(clip_msg))) => {
                            apply_clipboard_message(&sync_engine, &backend, clip_msg);
                        }
                        Ok(TunnelFrame::Control(ControlMessage::Disconnect { .. })) => break,
                        Ok(TunnelFrame::Packet(bytes)) => {
                            if let (Some(handler), Ok(packet)) =
                                (&packet_handler, Packet::from_bytes(&bytes))
                            {
                                handler(packet);
                            }
                        }
                        Ok(TunnelFrame::Control(_)) => {}
                        Err(_) => break,
                    }
                }
                if let Some(link) = link {
                    link.lock().control_closed(peer_addr);
                }
            } else {
                run_legacy_control_peer(
                    reader,
                    writer,
                    peer_rx,
                    sync_engine,
                    backend,
                    link,
                    peer_addr,
                    running,
                )
                .await;
            }
        });
    }
}

fn handle_video_result(
    result: Result<Vec<u8>, broadcast::error::RecvError>,
    keyframe_request: Option<&Arc<dyn Fn() + Send + Sync>>,
) -> (Option<Vec<u8>>, bool) {
    match result {
        Ok(bytes) => (Some(bytes), false),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(skipped, "USB video tunnel lagged; requesting keyframe");
            if let Some(request) = keyframe_request {
                request();
            }
            (None, false)
        }
        Err(broadcast::error::RecvError::Closed) => (None, true),
    }
}

fn apply_clipboard_message<B: ClipboardBackend>(
    sync_engine: &Arc<Mutex<ClipboardSyncEngine>>,
    backend: &Arc<Mutex<B>>,
    message: ClipboardMessage,
) {
    let action = sync_engine.lock().process_incoming_message(message);
    if let Ok(SyncAction::ApplyToLocal(text)) = action {
        let _ = backend.lock().set_text(&text);
    }
}

#[allow(clippy::too_many_arguments)] // Legacy and tunnel peers share the same service state.
async fn run_legacy_control_peer<B: ClipboardBackend + 'static>(
    reader: tokio::net::tcp::OwnedReadHalf,
    writer: tokio::net::tcp::OwnedWriteHalf,
    mut outgoing: mpsc::UnboundedReceiver<ControlMessage>,
    sync_engine: Arc<Mutex<ClipboardSyncEngine>>,
    backend: Arc<Mutex<B>>,
    link: Option<SharedLinkTracker>,
    peer_addr: SocketAddr,
    running: Arc<AtomicBool>,
) {
    let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut channel = linux_quest_transport::ControlChannel::new(reader, tokio::io::sink());
        loop {
            let message = channel.recv_message().await;
            let failed = message.is_err();
            if incoming_tx.send(message).await.is_err() || failed {
                break;
            }
        }
    });
    let mut channel = linux_quest_transport::ControlChannel::new(tokio::io::empty(), writer);
    while running.load(Ordering::Relaxed) {
        tokio::select! {
            message = outgoing.recv() => match message {
                Some(message) => if channel.send_message(&message).await.is_err() { break; },
                None => break,
            },
            message = incoming_rx.recv() => match message {
                Some(Ok(message)) => {
                    if let Some(link) = &link { link.lock().control_activity(Instant::now()); }
                    match message {
                        ControlMessage::Clipboard(clip) => apply_clipboard_message(&sync_engine, &backend, clip),
                        ControlMessage::Disconnect { .. } => break,
                        _ => {},
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
    }
    if let Some(link) = link {
        link.lock().control_closed(peer_addr);
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
    use bytes::Bytes;
    use linux_quest_input::MockClipboardBackend;
    use linux_quest_protocol::{PacketHeader, PacketType};
    use linux_quest_transport::{negotiate_client, recv_frame, ControlMessage, TunnelFrame};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[tokio::test]
    async fn negotiated_usb_peer_routes_packets_and_video_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let sync = Arc::new(Mutex::new(ClipboardSyncEngine::new(7)));
        let (outgoing, _outgoing_rx) = mpsc::unbounded_channel::<ControlMessage>();
        let (_incoming_tx, incoming) = mpsc::unbounded_channel::<ClipboardMessage>();
        let mut service = HostClipboardService::new(backend, sync, outgoing, incoming);
        service.listener = Some(listener);
        let packets = Arc::new(AtomicUsize::new(0));
        let packet_count = packets.clone();
        let (video_tx, _) = tokio::sync::broadcast::channel(8);
        let link =
            crate::link_status::new_shared_link_tracker(crate::link_status::DEFAULT_INPUT_LIVENESS);
        let running = service.running_flag();
        service = service
            .with_link_tracker(link.clone())
            .with_tunnel_packet_handler(Arc::new(move |_| {
                packet_count.fetch_add(1, Ordering::Relaxed);
            }))
            .with_tunnel_video(video_tx.clone());
        let task = tokio::spawn(async move { service.run().await.unwrap() });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        negotiate_client(&mut reader, &mut writer).await.unwrap();
        // Video is sent while an input frame is only partially available on the TCP stream.
        // The independent tunnel reader must preserve the partial length prefix across this.
        let video = linux_quest_protocol::Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, 11, b"video"),
            Bytes::from_static(b"video"),
        );
        video_tx.send(video.to_bytes().to_vec()).unwrap();
        let TunnelFrame::Packet(bytes) =
            tokio::time::timeout(Duration::from_secs(1), recv_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected video packet")
        };
        assert_eq!(
            linux_quest_protocol::Packet::from_bytes(&bytes)
                .unwrap()
                .payload,
            Bytes::from_static(b"video")
        );

        let packet = linux_quest_protocol::Packet::new(
            PacketHeader::new(PacketType::InputEvent, 0, 1, 10, b"input"),
            Bytes::from_static(b"input"),
        );
        let body = bincode::serialize(&TunnelFrame::Packet(packet.to_bytes().to_vec())).unwrap();
        let prefix = (body.len() as u32).to_be_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut writer, &prefix[..2])
            .await
            .unwrap();
        let video2 = linux_quest_protocol::Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 3, 12, b"video2"),
            Bytes::from_static(b"video2"),
        );
        video_tx.send(video2.to_bytes().to_vec()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), recv_frame(&mut reader))
            .await
            .unwrap()
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut writer, &prefix[2..])
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut writer, &body)
            .await
            .unwrap();
        assert!(link
            .lock()
            .status(std::time::Instant::now())
            .last_seen
            .is_some());
        tokio::time::timeout(Duration::from_secs(1), async {
            while packets.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        running.store(false, Ordering::SeqCst);
        task.abort();
    }

    #[tokio::test]
    async fn plain_control_client_still_works_without_tunnel_hello() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let sync = Arc::new(Mutex::new(ClipboardSyncEngine::new(7)));
        let (outgoing, _outgoing_rx) = mpsc::unbounded_channel::<ControlMessage>();
        let (_incoming_tx, incoming) = mpsc::unbounded_channel::<ClipboardMessage>();
        let link =
            crate::link_status::new_shared_link_tracker(crate::link_status::DEFAULT_INPUT_LIVENESS);
        let mut service = HostClipboardService::new(backend, sync, outgoing.clone(), incoming)
            .with_link_tracker(link.clone());
        service.listener = Some(listener);
        let running = service.running_flag();
        let task = tokio::spawn(async move { service.run().await.unwrap() });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut legacy = linux_quest_transport::ControlChannel::new(reader, writer);
        let outbound = ControlMessage::Heartbeat {
            sequence: 8,
            timestamp_us: 19,
        };
        legacy.send_message(&outbound).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if link
                    .lock()
                    .status(Instant::now())
                    .last_seen
                    .is_some_and(|seen| seen.elapsed() < Duration::from_millis(100))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok(),);

        running.store(false, Ordering::SeqCst);
        task.abort();
    }

    #[tokio::test]
    async fn idle_control_connection_is_closed_after_negotiation_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Arc::new(Mutex::new(MockClipboardBackend::new()));
        let sync = Arc::new(Mutex::new(ClipboardSyncEngine::new(7)));
        let (outgoing, _outgoing_rx) = mpsc::unbounded_channel::<ControlMessage>();
        let (_incoming_tx, incoming) = mpsc::unbounded_channel::<ClipboardMessage>();
        let mut service = HostClipboardService::new(backend, sync, outgoing, incoming);
        service.listener = Some(listener);
        let running = service.running_flag();
        let task = tokio::spawn(async move { service.run().await.unwrap() });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut one = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(
                linux_quest_transport::tcp_tunnel::TUNNEL_NEGOTIATION_TIMEOUT
                    + Duration::from_secs(1),
                tokio::io::AsyncReadExt::read(&mut client, &mut one),
            )
            .await
            .unwrap()
            .unwrap(),
            0,
        );
        running.store(false, Ordering::SeqCst);
        task.abort();
    }

    #[test]
    fn lagged_video_requests_keyframe_and_closed_sender_stops_reader() {
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let (packet, closed) =
            handle_video_result(Err(broadcast::error::RecvError::Lagged(3)), Some(&callback));
        assert!(packet.is_none());
        assert!(!closed);
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        let (packet, closed) =
            handle_video_result(Err(broadcast::error::RecvError::Closed), Some(&callback));
        assert!(packet.is_none());
        assert!(closed);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
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
