//! The headset's way to the host: the USB tunnel, Wi-Fi (UDP), or both at once.
//!
//! [`ClientTransport`] runs what the [`TransportMode`] asks for:
//!
//! - `AdbReverseTcp`: the USB tunnel ([`crate::usb_tunnel`]) carries video in and input out.
//! - `Udp`: video arrives as UDP datagrams on port 48440, and input leaves through the UDP input
//!   sender ([`crate::input_sender`]), whose keepalives are what the host latches onto.
//! - `Auto`: both, all the time. The tunnel is retried every half second, so plugging the cable
//!   in brings it up; the Wi-Fi path runs once the host's LAN address is known (from the
//!   configuration, or later from discovery through [`WifiHandle::set_host`]).
//!
//! # Where input goes
//!
//! One input queue feeds both paths, and each event is drained by exactly one of them, so
//! nothing reaches the host twice. [`choose_input_path`] picks the path: the tunnel while it
//! is open and not stale, unless the host is sending video over Wi-Fi (an older host, or one
//! that found the tunnel quiet); Wi-Fi otherwise. Both paths keep sending keepalives whichever
//! one carries the input, so the host always knows both are there and can move video at once.
//!
//! The host decides where video goes: over the tunnel while it carries data, otherwise to the
//! Wi-Fi peer (see the host's `link_status`). Packets from both paths go to the same channel;
//! the host sends each frame over one path only.
//!
//! # Switching
//!
//! When the input path changes, every stream seen so far gets a keyframe request over the new
//! path, so decoding restarts at once instead of after loss recovery notices the gap. The
//! host also starts a new path with a keyframe; it ignores a second request that close behind.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use linux_quest_protocol::{InputEvent, Packet};
use parking_lot::RwLock;

use crate::android::{accept_udp_source, udp_config_warning, udp_source_filter, DropWarnLimiter};
use crate::config::{ClientConfig, TransportMode, USB_TUNNEL_ADDR};
use crate::input_sender::{
    host_targets, input_sender_config, CombinedInputSource, InputEventQueue, InputSenderConfig,
    InputSenderControl, InputSenderThread, InputSource, KeyframeRequestFlags, SharedInputEvents,
};
use crate::usb_tunnel::{
    TunnelHealth, TunnelInput, TunnelMonitor, TunnelReconnect, UsbTunnel, UsbTunnelConfig,
};

/// The UDP port the host sends video to.
pub const VIDEO_PORT: u16 = 48440;

/// Where the headset receives UDP video.
pub const DEFAULT_VIDEO_BIND: SocketAddr =
    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), VIDEO_PORT);

/// Silence after which an open tunnel no longer carries input, when its host sends a heartbeat
/// every second. The tunnel itself is reopened later, after
/// [`TUNNEL_IDLE_TIMEOUT`](linux_quest_transport::TUNNEL_IDLE_TIMEOUT).
pub const TUNNEL_STALE_AFTER: Duration = Duration::from_millis(1500);

/// Requested `SO_RCVBUF` for the UDP video socket (the kernel may cap it at `rmem_max`).
const UDP_RECEIVE_BUFFER_BYTES: libc::c_int = 4 * 1024 * 1024;

/// How long the UDP receiver waits for the video port to be free: the other presentation mode
/// (immersive or Home panel) may still be releasing it.
const VIDEO_BIND_WAIT: Duration = Duration::from_secs(5);

/// A way to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPath {
    /// The USB tunnel.
    Usb,
    /// UDP over Wi-Fi.
    Wifi,
}

impl InputPath {
    fn code(path: Option<Self>) -> u8 {
        match path {
            None => 0,
            Some(Self::Usb) => 1,
            Some(Self::Wifi) => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Usb),
            2 => Some(Self::Wifi),
            _ => None,
        }
    }
}

impl std::fmt::Display for InputPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Usb => "USB",
            Self::Wifi => "Wi-Fi",
        })
    }
}

/// The path input should take, or `None` when neither can carry it.
///
/// - `tunnel`: the USB tunnel's health, `None` when this mode runs no tunnel. It counts while
///   it is connected, unless its host heartbeats and it has been quiet for
///   [`TUNNEL_STALE_AFTER`].
/// - `wifi_ready`: the Wi-Fi path runs and knows the host's address.
/// - `last_media`: the path the newest packet from the host came over.
///
/// The tunnel is preferred, but input follows video to Wi-Fi when the host sends video there
/// although the tunnel is open: the host is the one that knows which path reaches it.
pub fn choose_input_path(
    tunnel: Option<TunnelHealth>,
    wifi_ready: bool,
    last_media: Option<InputPath>,
) -> Option<InputPath> {
    let connected = tunnel.is_some_and(|t| t.connected);
    let live =
        tunnel.is_some_and(|t| t.connected && !(t.heartbeats && t.quiet >= TUNNEL_STALE_AFTER));
    if live && !(wifi_ready && last_media == Some(InputPath::Wifi)) {
        Some(InputPath::Usb)
    } else if wifi_ready {
        Some(InputPath::Wifi)
    } else if connected {
        Some(InputPath::Usb)
    } else {
        None
    }
}

/// What a [`ClientTransport`] runs.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub mode: TransportMode,
    /// The USB tunnel, or `None` for none.
    pub tunnel: Option<UsbTunnelConfig>,
    /// The Wi-Fi path, or `None` for none.
    pub wifi: Option<WifiConfig>,
}

/// The Wi-Fi (UDP) path.
#[derive(Debug, Clone)]
pub struct WifiConfig {
    /// Local address video datagrams arrive on ([`DEFAULT_VIDEO_BIND`]).
    pub video_bind: SocketAddr,
    /// Source IPs video is accepted from; `None` accepts any.
    pub sources: Option<Vec<IpAddr>>,
    /// The input sender. Its targets are the host's input addresses, best first; empty until
    /// the host is known.
    pub sender: InputSenderConfig,
}

impl TransportConfig {
    /// What `config` asks for, with the production ports and timing.
    pub fn for_client(config: &ClientConfig) -> Self {
        let mode = config.transport_mode;
        let tunnel = match mode {
            TransportMode::AdbReverseTcp => Some(UsbTunnelConfig::new(config.host_addr)),
            TransportMode::Auto => Some(UsbTunnelConfig::new(USB_TUNNEL_ADDR)),
            TransportMode::Udp => None,
        };
        let wifi = mode.uses_wifi().then(|| {
            if let Some(message) = udp_config_warning(config) {
                tracing::warn!("{message}");
            }
            let mut sender =
                input_sender_config(config).unwrap_or_else(|| InputSenderConfig::new(Vec::new()));
            sender.targets = prefer_local_networks(sender.targets, &local_ipv4_networks());
            WifiConfig {
                video_bind: DEFAULT_VIDEO_BIND,
                sources: udp_source_filter(config),
                sender,
            }
        });
        Self { mode, tunnel, wifi }
    }
}

/// Where input goes and which streams have been seen, shared by the paths' threads.
struct PathState {
    tunnel: OnceLock<TunnelMonitor>,
    wifi_ready: AtomicBool,
    /// [`InputPath::code`] of the path the newest packet came over.
    last_media: AtomicU8,
    /// [`InputPath::code`] of the path input went over last.
    current: AtomicU8,
    /// Streams (bit per id below 32) that delivered packets on either path.
    seen_streams: AtomicU32,
    keyframes: KeyframeRequestFlags,
    /// Wakes the UDP sender, so a request queued on a switch to Wi-Fi goes out at once.
    wifi_sender: OnceLock<InputSenderControl>,
}

impl PathState {
    fn new(keyframes: KeyframeRequestFlags) -> Self {
        Self {
            tunnel: OnceLock::new(),
            wifi_ready: AtomicBool::new(false),
            last_media: AtomicU8::new(0),
            current: AtomicU8::new(0),
            seen_streams: AtomicU32::new(0),
            keyframes,
            wifi_sender: OnceLock::new(),
        }
    }

    fn media_arrived(&self, path: InputPath, packet: &Packet) {
        self.last_media
            .store(InputPath::code(Some(path)), Ordering::Release);
        if let Some(bit) = 1u32.checked_shl(u32::from(packet.header.stream_id)) {
            self.seen_streams.fetch_or(bit, Ordering::AcqRel);
        }
    }

    /// The path input takes now; logs and asks for keyframes when it changed.
    fn input_path(&self) -> Option<InputPath> {
        let chosen = choose_input_path(
            self.tunnel.get().map(TunnelMonitor::health),
            self.wifi_ready.load(Ordering::Acquire),
            InputPath::from_code(self.last_media.load(Ordering::Acquire)),
        );
        let previous = self.current.swap(InputPath::code(chosen), Ordering::AcqRel);
        if previous != InputPath::code(chosen) {
            self.switched(InputPath::from_code(previous), chosen);
        }
        chosen
    }

    fn switched(&self, from: Option<InputPath>, to: Option<InputPath>) {
        match (from, to) {
            (_, None) => tracing::info!("no path to the host: input waits"),
            (None, Some(to)) => tracing::info!(path = %to, "input goes to the host over {to}"),
            (Some(from), Some(to)) => {
                // Stream 0 always: it is the one every host sends.
                let streams = self.seen_streams.load(Ordering::Acquire) | 1;
                for stream_id in (0..32u16).filter(|id| streams & (1 << id) != 0) {
                    self.keyframes.raise(stream_id);
                }
                tracing::info!(
                    %from,
                    %to,
                    streams = format!("{streams:#b}"),
                    "switched from {from} to {to}; asking the host for keyframes"
                );
                if let Some(sender) = self.wifi_sender.get() {
                    sender.notify();
                }
            }
        }
    }
}

/// `targets`, those on one of `networks` (address, netmask) first, otherwise in their order.
///
/// A host on several networks (LAN, VPN, container bridges) advertises an address on each, and
/// discovery cannot tell which one the headset reaches; the one on the headset's own network
/// is the likely one.
pub fn prefer_local_networks(
    targets: Vec<SocketAddr>,
    networks: &[(Ipv4Addr, Ipv4Addr)],
) -> Vec<SocketAddr> {
    let on_local_network = |target: &SocketAddr| match target.ip().to_canonical() {
        IpAddr::V4(ip) => networks.iter().any(|(net, mask)| {
            let mask = u32::from(*mask);
            mask != 0 && u32::from(ip) & mask == u32::from(*net) & mask
        }),
        IpAddr::V6(_) => false,
    };
    let (mut local, other): (Vec<_>, Vec<_>) = targets.into_iter().partition(on_local_network);
    local.extend(other);
    local
}

/// The IPv4 networks of this device's interfaces that are up, loopback excluded, as
/// (address, netmask). Empty if they cannot be read.
#[cfg(unix)]
pub fn local_ipv4_networks() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut networks = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: on success `head` is a list owned by us until `freeifaddrs` below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return networks;
    }
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list `getifaddrs` returned, still alive.
        let ifa = unsafe { &*cursor };
        cursor = ifa.ifa_next;
        let flags = ifa.ifa_flags;
        let up = flags & libc::IFF_UP as libc::c_uint != 0;
        let loopback = flags & libc::IFF_LOOPBACK as libc::c_uint != 0;
        if !up || loopback || ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: non-null address pointers of a live node; an AF_INET address and its netmask
        // are `sockaddr_in`s.
        unsafe {
            if i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            networks.push((
                Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
                Ipv4Addr::from(u32::from_be(mask.sin_addr.s_addr)),
            ));
        }
    }
    // SAFETY: `head` came from a successful `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(head) };
    networks
}

#[cfg(not(unix))]
pub fn local_ipv4_networks() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    Vec::new()
}

/// The share of the input queue one path sends: everything while it is the input path,
/// nothing (so only keepalives) otherwise.
struct RoutedInput {
    inner: Arc<Mutex<CombinedInputSource>>,
    paths: Arc<PathState>,
    path: InputPath,
}

impl InputSource for RoutedInput {
    fn poll(&mut self) -> Vec<InputEvent> {
        let path = self.paths.input_path();
        if path != Some(self.path) {
            // While the tunnel carries input, nothing comes back over Wi-Fi to show which of the
            // host's addresses works, and the sender would rotate through all of them, leaving
            // the right one quiet for seconds. Holding it on its current address (the first,
            // which is on this device's own network, unless Wi-Fi use moved it to one that
            // works) keeps the host's Wi-Fi peer fresh, so video can move there the moment the
            // cable comes out.
            if self.path == InputPath::Wifi && path == Some(InputPath::Usb) {
                if let Some(sender) = self.paths.wifi_sender.get() {
                    sender.ack();
                }
            }
            return Vec::new();
        }
        self.inner
            .lock()
            .map(|mut source| source.poll())
            .unwrap_or_default()
    }
}

/// Wakes the input senders after input was queued. A no-op when no UDP sender runs: the
/// tunnel's writer polls on its own timer.
#[derive(Clone, Default)]
pub struct InputWaker(Option<InputSenderControl>);

impl InputWaker {
    pub fn wake(&self) {
        if let Some(control) = &self.0 {
            control.notify();
        }
    }
}

/// Points the Wi-Fi path at a host found after start-up. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct WifiHandle {
    control: InputSenderControl,
    sources: Arc<RwLock<Option<Vec<IpAddr>>>>,
    paths: Arc<PathState>,
}

impl WifiHandle {
    /// Sends input and keepalives to `host` (and its other addresses `extra`) on `input_port`,
    /// and accepts video from those addresses.
    pub fn set_host(&self, host: IpAddr, extra: &[IpAddr], input_port: u16) {
        let targets = prefer_local_networks(
            host_targets(host, extra, input_port),
            &local_ipv4_networks(),
        );
        let mut sources: Vec<IpAddr> = Vec::new();
        for ip in std::iter::once(host).chain(extra.iter().copied()) {
            let ip = ip.to_canonical();
            if !sources.contains(&ip) {
                sources.push(ip);
            }
        }
        tracing::info!(?targets, "Wi-Fi path: host address set");
        let ready = !targets.is_empty();
        self.control.retarget(targets);
        *self.sources.write() = Some(sources);
        self.paths.wifi_ready.store(ready, Ordering::Release);
    }

    /// Whether the Wi-Fi path knows where the host is.
    pub fn has_host(&self) -> bool {
        self.paths.wifi_ready.load(Ordering::Acquire)
    }
}

/// The running transport. Stops everything when dropped.
pub struct ClientTransport {
    paths: Arc<PathState>,
    keyboard: InputEventQueue,
    pointer: SharedInputEvents,
    tunnel: Option<UsbTunnel>,
    sender: Option<InputSenderThread>,
    receiver: Option<UdpReceiver>,
    wifi: Option<WifiHandle>,
}

impl ClientTransport {
    /// Starts the paths `config` asks for. Every packet from the host goes to `packets`; a full
    /// channel drops it.
    pub fn spawn(config: TransportConfig, packets: SyncSender<Packet>) -> std::io::Result<Self> {
        Self::spawn_with_source(config, CombinedInputSource::new(), packets)
    }

    /// [`Self::spawn`], sending the input queued on `source` (and its keyframe requests).
    pub fn spawn_with_source(
        config: TransportConfig,
        source: CombinedInputSource,
        packets: SyncSender<Packet>,
    ) -> std::io::Result<Self> {
        let keyframes = source.requester();
        let keyboard = source.keyboard_queue();
        let pointer = source.pointer_events();
        let inner = Arc::new(Mutex::new(source));
        let paths = Arc::new(PathState::new(keyframes.clone()));
        tracing::info!(
            mode = ?config.mode,
            usb = config.tunnel.is_some(),
            wifi = config.wifi.is_some(),
            "starting the transport to the host"
        );

        let (sender, receiver, wifi) = match config.wifi {
            Some(wifi) => {
                paths
                    .wifi_ready
                    .store(!wifi.sender.targets.is_empty(), Ordering::Release);
                let routed = RoutedInput {
                    inner: inner.clone(),
                    paths: paths.clone(),
                    path: InputPath::Wifi,
                };
                tracing::info!(targets = ?wifi.sender.targets, "starting UDP input/keepalive sender");
                let sender = InputSenderThread::spawn(wifi.sender, Box::new(routed))?;
                let control = sender.control().clone();
                let _ = paths.wifi_sender.set(control.clone());
                let sources = Arc::new(RwLock::new(wifi.sources));
                let receiver = UdpReceiver::spawn(
                    wifi.video_bind,
                    sources.clone(),
                    control.clone(),
                    paths.clone(),
                    packets.clone(),
                )?;
                let handle = WifiHandle {
                    control,
                    sources,
                    paths: paths.clone(),
                };
                (Some(sender), Some(receiver), Some(handle))
            }
            None => (None, None, None),
        };

        let tunnel = match config.tunnel {
            Some(tunnel_config) => {
                let routed: Arc<Mutex<dyn InputSource>> = Arc::new(Mutex::new(RoutedInput {
                    inner: inner.clone(),
                    paths: paths.clone(),
                    path: InputPath::Usb,
                }));
                let input = TunnelInput {
                    source: routed,
                    keyframes: keyframes.clone(),
                };
                let media = paths.clone();
                let tx = packets;
                let tunnel = UsbTunnel::spawn(tunnel_config, Some(input), move |packet| {
                    media.media_arrived(InputPath::Usb, &packet);
                    let _ = tx.try_send(packet);
                })?;
                let _ = paths.tunnel.set(tunnel.monitor());
                Some(tunnel)
            }
            None => None,
        };

        Ok(Self {
            paths,
            keyboard,
            pointer,
            tunnel,
            sender,
            receiver,
            wifi,
        })
    }

    /// Raise a stream's flag to ask the host for a keyframe, then [`InputWaker::wake`].
    pub fn keyframe_flags(&self) -> KeyframeRequestFlags {
        self.paths.keyframes.clone()
    }

    /// Queue for keyboard and IME events.
    pub fn keyboard_queue(&self) -> InputEventQueue {
        self.keyboard.clone()
    }

    /// Queue for pointer events (and network feedback).
    pub fn pointer_events(&self) -> SharedInputEvents {
        self.pointer.clone()
    }

    /// Call after queueing input, so it leaves at once.
    pub fn waker(&self) -> InputWaker {
        InputWaker(self.sender.as_ref().map(|s| s.control().clone()))
    }

    /// The Wi-Fi path's handle, if this mode runs one.
    pub fn wifi(&self) -> Option<WifiHandle> {
        self.wifi.clone()
    }

    /// Makes the tunnel reconnect (after a pause), if this mode runs one.
    pub fn tunnel_reconnect(&self) -> Option<TunnelReconnect> {
        self.tunnel.as_ref().map(UsbTunnel::reconnect_handle)
    }

    /// The path input takes now.
    pub fn input_path(&self) -> Option<InputPath> {
        self.paths.input_path()
    }

    /// The address the UDP video socket is bound to, once it is.
    pub fn video_addr(&self) -> Option<SocketAddr> {
        self.receiver.as_ref().and_then(UdpReceiver::local_addr)
    }

    /// Stops every path and waits for their threads (as dropping does).
    pub fn stop(self) {}
}

impl Drop for ClientTransport {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            receiver.stop();
        }
        if let Some(tunnel) = self.tunnel.take() {
            tunnel.stop();
        }
        if let Some(sender) = self.sender.take() {
            sender.stop();
        }
    }
}

/// The thread that receives UDP video.
struct UdpReceiver {
    running: Arc<AtomicBool>,
    local: Arc<Mutex<Option<SocketAddr>>>,
    thread: Option<JoinHandle<()>>,
}

impl UdpReceiver {
    fn spawn(
        bind: SocketAddr,
        sources: Arc<RwLock<Option<Vec<IpAddr>>>>,
        ack: InputSenderControl,
        paths: Arc<PathState>,
        packets: SyncSender<Packet>,
    ) -> std::io::Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let local = Arc::new(Mutex::new(None));
        let thread = {
            let running = running.clone();
            let local = local.clone();
            std::thread::Builder::new()
                .name("lq-udp-video".into())
                .spawn(move || receive(bind, &running, &local, &sources, &ack, &paths, &packets))?
        };
        Ok(Self {
            running,
            local,
            thread: Some(thread),
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        *self.local.lock().ok()?
    }

    fn stop(mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn receive(
    bind: SocketAddr,
    running: &AtomicBool,
    local: &Mutex<Option<SocketAddr>>,
    sources: &RwLock<Option<Vec<IpAddr>>>,
    ack: &InputSenderControl,
    paths: &PathState,
    packets: &SyncSender<Packet>,
) {
    let bind_until = Instant::now() + VIDEO_BIND_WAIT;
    let socket = loop {
        if !running.load(Ordering::Acquire) {
            return;
        }
        match std::net::UdpSocket::bind(bind) {
            Ok(socket) => break socket,
            Err(e) if e.kind() == ErrorKind::AddrInUse && Instant::now() < bind_until => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = ?e, %bind, "could not bind the UDP video socket");
                return;
            }
        }
    };
    if let Ok(mut local) = local.lock() {
        *local = socket.local_addr().ok();
    }
    // The default receive buffer holds ~10 ms of an 80 Mbit/s stream; any stall of this thread
    // then drops datagrams in the kernel without a trace.
    set_receive_buffer(&socket, UDP_RECEIVE_BUFFER_BYTES);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = vec![0u8; 65536];
    let mut drop_warn = DropWarnLimiter::default();
    while running.load(Ordering::Acquire) {
        match socket.recv_from(&mut buf) {
            Ok((len, source)) => {
                let accepted = accept_udp_source(sources.read().as_deref(), source);
                if !accepted {
                    if drop_warn.should_warn(source.ip(), Instant::now()) {
                        tracing::warn!(
                            %source,
                            allowed = ?*sources.read(),
                            "Dropping UDP datagram from a source that is not the host \
                             (further drops from this source are not logged for 60s)"
                        );
                    }
                    continue;
                }
                match Packet::from_bytes(&buf[..len]) {
                    Ok(packet) => {
                        ack.ack();
                        paths.media_arrived(InputPath::Wifi, &packet);
                        let _ = packets.try_send(packet);
                    }
                    Err(e) => tracing::trace!(error = ?e, "Failed to parse incoming packet"),
                }
            }
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => tracing::trace!(error = ?e, "UDP socket recv error"),
        }
    }
}

/// Best-effort `SO_RCVBUF`; logs the size the kernel actually granted.
#[cfg(unix)]
fn set_receive_buffer(socket: &std::net::UdpSocket, bytes: libc::c_int) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    // SAFETY: valid socket fd and a c_int option value of the right size.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &bytes as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    let mut granted: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: as above, with out-pointers to locals of the declared size.
    unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &mut granted as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        );
    }
    tracing::info!(
        requested = bytes,
        granted,
        ok = set == 0,
        "UDP video socket receive buffer"
    );
}

#[cfg(not(unix))]
fn set_receive_buffer(_socket: &std::net::UdpSocket, _bytes: libc::c_int) {}

#[cfg(test)]
mod tests;
