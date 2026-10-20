//! Low-latency UDP input and keepalive sender (Quest client -> host).
//!
//! The client sends [`InputBatch`](linux_quest_protocol::InputBatch) datagrams to the host's
//! input port (UDP `48442`, [`DEFAULT_INPUT_PORT`]) using the existing
//! [`InputChannelSender`] encoder, which owns sequence numbers and the redundant key history.
//! The wire format and the host are unchanged.
//!
//! # Why this exists
//!
//! The host latches the first valid input sender as its peer and streams video to that peer's
//! IP (UDP `48440`). The host's link liveness also rides on these datagrams (5 s timeout). So
//! this sender must keep talking even when nobody is touching a controller: when idle it sends
//! an empty batch (a keepalive) every [`DEFAULT_KEEPALIVE_INTERVAL`] (100 ms).
//!
//! # Local socket
//!
//! The socket is bound to an **ephemeral** local port, not 48440. The host replies by IP to
//! 48440 (the receiver socket), never to the input datagram's source port, so the source port
//! does not matter for video. A fresh port per process is also what we want: the host
//! re-latches a new `ip:port` only after the old peer has been silent for 5 s, and resets its
//! sequence cursor when it does. Reusing a fixed port after an app restart would instead look
//! like the same peer with a sequence that went backwards, and the host would drop those
//! batches as stale.
//!
//! # Targets and "ack"
//!
//! There is no input-level ack on the wire. The only evidence that the host heard us is video
//! arriving on the receive socket, so the receiver calls [`InputSenderControl::ack`] for each
//! accepted datagram. The sender targets the first address; if no ack arrives within
//! [`DEFAULT_ACK_TIMEOUT`] (or a send fails) it moves to the next of at most
//! [`MAX_TARGETS`] addresses (`host_addr` then `extra_host_ips`), and stays on one that is acked.
//!
//! # Not available over `adb reverse`
//!
//! `adb reverse` forwards TCP only. In `TransportMode::AdbReverseTcp` this sender is not
//! started and the client sends no input at all; carrying input over USB needs the TCP control
//! channel, which is out of scope here.
//!
//! # Name clash
//!
//! [`InputSource`] here is a different trait from [`crate::input::InputSource`] (which polls raw
//! Android events). It is deliberately not re-exported at the crate root.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use linux_quest_protocol::InputEvent;
#[allow(unused_imports)] // referenced by the module docs
use linux_quest_transport::DEFAULT_INPUT_PORT;
use linux_quest_transport::{InputChannelSender, TransportError, DEFAULT_KEY_REDUNDANCY};

use crate::config::{ClientConfig, TransportMode};

/// Idle heartbeat period. Well inside the host's 5 s liveness window, short enough that a
/// lost datagram or two never matters.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(100);
/// How often a source that [`InputSource::needs_polling`] is polled without a wake-up.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(4);
/// Time without an ack before moving to the next target address.
pub const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_millis(1500);
/// First retry delay after a send error.
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
/// Retry delay cap.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(2);
/// DSCP EF (46) in the TOS byte: lowest-latency Wi-Fi (WMM voice) queue where honoured.
pub const DEFAULT_TOS: u8 = 0xB8;
/// Most targets kept (host_addr plus extra_host_ips); extras beyond this are ignored.
pub const MAX_TARGETS: usize = 5;
/// Events per datagram (events are at most ~11 bytes, so this stays far under 1 KiB).
pub const MAX_EVENTS_PER_DATAGRAM: usize = 32;
/// Events buffered while sends are failing; oldest are dropped first.
pub const MAX_PENDING_EVENTS: usize = 256;

/// Thread-safe queue carrying XR pointer events to the existing UDP input sender.
#[derive(Debug, Clone, Default)]
pub struct SharedInputEvents(Arc<SharedInputEventsInner>);

#[derive(Debug, Default)]
struct SharedInputEventsInner {
    events: Mutex<VecDeque<(Instant, InputEvent)>>,
    last_queue_latency_us: AtomicU64,
}

impl SharedInputEvents {
    pub fn push(&self, event: InputEvent) {
        if let Ok(mut events) = self.0.events.lock() {
            if events.len() == MAX_PENDING_EVENTS {
                events.pop_front();
            }
            events.push_back((Instant::now(), event));
        }
    }

    pub fn last_queue_latency_us(&self) -> u64 {
        self.0.last_queue_latency_us.load(Ordering::Relaxed)
    }

    fn drain(&self) -> Vec<InputEvent> {
        let Ok(mut queue) = self.0.events.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut events = Vec::with_capacity(queue.len());
        let mut max_latency = 0;
        while let Some((queued_at, event)) = queue.pop_front() {
            max_latency = max_latency.max(now.duration_since(queued_at).as_micros() as u64);
            events.push(event);
        }
        self.0
            .last_queue_latency_us
            .store(max_latency, Ordering::Relaxed);
        events
    }
}

/// Sender source combining queued pointer events with keyframe requests.
pub struct PointerInputSource {
    pub events: SharedInputEvents,
    pub keyframe: KeyframeRequestFlags,
}

impl InputSource for PointerInputSource {
    fn poll(&mut self) -> Vec<InputEvent> {
        let mut events = self.events.drain();
        events.extend(self.keyframe.take_events());
        events
    }
}

/// Supplies input state to the sender. Implemented by the Android runtime.
pub trait InputSource: Send {
    /// Returns the events produced since the last call (empty when nothing changed). Called
    /// from the sender thread whenever it wakes, so it must not block.
    fn poll(&mut self) -> Vec<InputEvent>;

    /// Whether the sender should poll on a timer ([`InputSenderConfig::poll_interval`]).
    /// A source that is pushed to (and calls [`InputSenderControl::notify`]) can return
    /// `false` to avoid timer wake-ups.
    fn needs_polling(&self) -> bool {
        true
    }
}

/// An [`InputSource`] with no input: the sender only emits keepalives. Enough for the host to
/// latch the headset and stream video.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeepaliveOnlySource;

impl InputSource for KeepaliveOnlySource {
    fn poll(&mut self) -> Vec<InputEvent> {
        Vec::new()
    }
    fn needs_polling(&self) -> bool {
        false
    }
}

/// Streams waiting for a keyframe request, one bit per stream id (ids 0..32).
#[derive(Debug, Default, Clone)]
pub struct KeyframeRequestFlags(Arc<AtomicU32>);

impl KeyframeRequestFlags {
    /// Marks `stream_id` as needing a keyframe. Ids of 32 and above are ignored.
    pub fn raise(&self, stream_id: u16) {
        if let Some(bit) = 1u32.checked_shl(u32::from(stream_id)) {
            self.0.fetch_or(bit, Ordering::AcqRel);
        }
    }

    /// Takes the pending streams as request events: [`InputEvent::KeyframeRequest`] for
    /// stream 0 (what single-stream hosts understand) and
    /// [`InputEvent::KeyframeRequestFor`] for the others.
    fn take_events(&self) -> Vec<InputEvent> {
        let pending = self.0.swap(0, Ordering::AcqRel);
        (0..32u16)
            .filter(|id| pending & (1 << id) != 0)
            .map(|id| match id {
                0 => InputEvent::KeyframeRequest,
                stream_id => InputEvent::KeyframeRequestFor { stream_id },
            })
            .collect()
    }
}

/// An [`InputSource`] that only emits keepalives, plus a keyframe request for each stream
/// raised on [`KeyframeRequestSource::requester`]'s flags (then call
/// [`InputSenderControl::notify`] so it goes out at once).
#[derive(Debug, Default, Clone)]
pub struct KeyframeRequestSource {
    pending: KeyframeRequestFlags,
}

impl KeyframeRequestSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// The flags to raise when a stream needs a keyframe.
    pub fn requester(&self) -> KeyframeRequestFlags {
        self.pending.clone()
    }
}

impl InputSource for KeyframeRequestSource {
    fn poll(&mut self) -> Vec<InputEvent> {
        self.pending.take_events()
    }
    fn needs_polling(&self) -> bool {
        false
    }
}

/// Sender tuning. [`InputSenderConfig::new`] gives the production defaults.
#[derive(Debug, Clone)]
pub struct InputSenderConfig {
    /// Candidate host input addresses, primary first. May be empty and set later by
    /// [`InputSenderControl::retarget`].
    pub targets: Vec<SocketAddr>,
    pub keepalive_interval: Duration,
    pub poll_interval: Duration,
    pub ack_timeout: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// IP TOS / traffic class byte, best effort. `None` leaves the OS default.
    pub tos: Option<u8>,
}

impl InputSenderConfig {
    pub fn new(targets: Vec<SocketAddr>) -> Self {
        Self {
            targets: normalize_targets(targets),
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            poll_interval: DEFAULT_POLL_INTERVAL,
            ack_timeout: DEFAULT_ACK_TIMEOUT,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
            tos: Some(DEFAULT_TOS),
        }
    }
}

fn normalize_targets(targets: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::with_capacity(targets.len().min(MAX_TARGETS));
    for t in targets {
        let t = SocketAddr::new(t.ip().to_canonical(), t.port());
        if !t.ip().is_unspecified() && t.port() != 0 && !out.contains(&t) {
            out.push(t);
        }
        if out.len() == MAX_TARGETS {
            break;
        }
    }
    out
}

/// Input addresses for a host: `host` first, then `extra`, deduplicated and bounded, all on
/// `port`. Unspecified addresses are dropped.
pub fn host_targets(host: IpAddr, extra: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    normalize_targets(
        std::iter::once(host)
            .chain(extra.iter().copied())
            .map(|ip| SocketAddr::new(ip, port))
            .collect(),
    )
}

/// The sender configuration for a client config, or `None` when no sender should run: any mode
/// other than `Udp` (`adb reverse` cannot carry UDP), or no usable host address.
pub fn input_sender_config(config: &ClientConfig) -> Option<InputSenderConfig> {
    // An unspecified host IP means "unset" (same convention as the video source filter).
    if config.transport_mode != TransportMode::Udp || config.host_addr.ip().is_unspecified() {
        return None;
    }
    let targets = host_targets(
        config.host_addr.ip(),
        &config.extra_host_ips,
        config.input_port,
    );
    (!targets.is_empty()).then(|| InputSenderConfig::new(targets))
}

/// Counters readable from any thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputSenderStats {
    pub datagrams_sent: u64,
    pub keepalives_sent: u64,
    pub send_errors: u64,
    /// Sequence number of the last datagram sent.
    pub last_sequence: u64,
    /// Index into the current target list that datagrams go to.
    pub target_index: usize,
    /// `true` while sends are failing and the sender is backing off.
    pub failing: bool,
}

#[derive(Default)]
struct Shared {
    notify: Notify,
    acked: AtomicBool,
    datagrams: AtomicU64,
    keepalives: AtomicU64,
    send_errors: AtomicU64,
    last_seq: AtomicU64,
    target_index: AtomicUsize,
    failing: AtomicBool,
}

/// Cheap, cloneable handle to a running sender.
#[derive(Clone)]
pub struct InputSenderControl {
    shared: Arc<Shared>,
    targets: Arc<watch::Sender<Vec<SocketAddr>>>,
    shutdown: Arc<watch::Sender<bool>>,
}

impl InputSenderControl {
    fn new(targets: Vec<SocketAddr>) -> Self {
        Self {
            shared: Arc::new(Shared::default()),
            targets: Arc::new(watch::channel(targets).0),
            shutdown: Arc::new(watch::channel(false).0),
        }
    }

    /// Tells the sender the source has new state: it polls and sends right away.
    pub fn notify(&self) {
        self.shared.notify.notify_one();
    }

    /// Records that the host was heard from (call for each accepted video datagram). Keeps the
    /// sender on its current target address.
    pub fn ack(&self) {
        self.shared.acked.store(true, Ordering::Relaxed);
    }

    /// Points the sender at a new host (after discovery or manual entry). Keepalive goes out
    /// immediately. Same address family keeps the socket, sequence and key history; a family
    /// change rebinds, which the host treats as a new peer after its 5 s relatch window.
    pub fn retarget(&self, targets: Vec<SocketAddr>) {
        self.targets.send_replace(normalize_targets(targets));
    }

    /// Asks the sender to stop.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn stats(&self) -> InputSenderStats {
        let s = &self.shared;
        InputSenderStats {
            datagrams_sent: s.datagrams.load(Ordering::Relaxed),
            keepalives_sent: s.keepalives.load(Ordering::Relaxed),
            send_errors: s.send_errors.load(Ordering::Relaxed),
            last_sequence: s.last_seq.load(Ordering::Relaxed),
            target_index: s.target_index.load(Ordering::Relaxed),
            failing: s.failing.load(Ordering::Relaxed),
        }
    }
}

/// A sender running as a task on the caller's tokio runtime.
pub struct InputSender {
    control: InputSenderControl,
    task: JoinHandle<()>,
}

impl InputSender {
    /// Spawns the send loop. Must be called inside a tokio runtime.
    pub fn spawn(config: InputSenderConfig, source: Box<dyn InputSource>) -> Self {
        let control = InputSenderControl::new(config.targets.clone());
        let task = tokio::spawn(run(config, source, control.clone()));
        Self { control, task }
    }

    pub fn control(&self) -> &InputSenderControl {
        &self.control
    }

    /// Stops the loop and waits for it to end.
    pub async fn stop(self) {
        self.control.shutdown();
        let _ = self.task.await;
    }
}

/// A sender running on its own OS thread with a private current-thread runtime. This is what
/// the Android entry point uses, since it has no tokio runtime of its own.
pub struct InputSenderThread {
    control: InputSenderControl,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputSenderThread {
    pub fn spawn(config: InputSenderConfig, source: Box<dyn InputSource>) -> io::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let control = InputSenderControl::new(config.targets.clone());
        let ctl = control.clone();
        let thread = std::thread::Builder::new()
            .name("lq-input-tx".into())
            .spawn(move || rt.block_on(run(config, source, ctl)))?;
        Ok(Self {
            control,
            thread: Some(thread),
        })
    }

    pub fn control(&self) -> &InputSenderControl {
        &self.control
    }

    /// Stops the loop and joins the thread.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.control.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for InputSenderThread {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

struct Link {
    sender: InputChannelSender,
    v6: bool,
}

async fn bind_link(v6: bool, tos: Option<u8>) -> io::Result<Link> {
    let addr: SocketAddr = if v6 {
        ([0u16; 8], 0).into()
    } else {
        ([0u8; 4], 0).into()
    };
    let socket = UdpSocket::bind(addr).await?;
    if let Some(tos) = tos {
        if let Err(e) = set_tos(&socket, v6, tos) {
            tracing::debug!(error = %e, "could not set IP TOS on input socket (best effort)");
        }
    }
    Ok(Link {
        sender: InputChannelSender::new(socket, None, DEFAULT_KEY_REDUNDANCY),
        v6,
    })
}

#[cfg(unix)]
fn set_tos(socket: &UdpSocket, v6: bool, tos: u8) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let value: libc::c_int = tos.into();
    let (level, name) = if v6 {
        (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
    } else {
        (libc::IPPROTO_IP, libc::IP_TOS)
    };
    // SAFETY: `value` outlives the call and the length matches its type; the fd is owned by
    // `socket`, which is borrowed for the duration.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn set_tos(_socket: &UdpSocket, _v6: bool, _tos: u8) -> io::Result<()> {
    Ok(())
}

/// Send-failure bookkeeping: bounded exponential backoff, one warn per state change.
struct Health {
    failing: bool,
    delay: Duration,
    retry_at: Option<Instant>,
}

impl Health {
    fn on_error(&mut self, cfg: &InputSenderConfig, now: Instant, what: &str) {
        self.delay = if self.failing {
            (self.delay * 2).min(cfg.backoff_max)
        } else {
            cfg.backoff_initial
        };
        self.retry_at = Some(now + self.delay);
        if !self.failing {
            self.failing = true;
            tracing::warn!(error = what, "input sender: sends failing, backing off");
        }
    }

    fn on_ok(&mut self) {
        self.retry_at = None;
        if self.failing {
            self.failing = false;
            tracing::info!("input sender: sends recovered");
        }
    }
}

async fn run(cfg: InputSenderConfig, mut source: Box<dyn InputSource>, ctl: InputSenderControl) {
    let shared = ctl.shared.clone();
    let mut targets_rx = ctl.targets.subscribe();
    let mut shutdown_rx = ctl.shutdown.subscribe();
    let mut targets: Vec<SocketAddr> = targets_rx.borrow_and_update().clone();
    let epoch = Instant::now();
    let mut link: Option<Link> = None;
    let mut pending: VecDeque<InputEvent> = VecDeque::new();
    let mut last_send: Option<Instant> = None;
    let mut ack_window = epoch;
    let mut idx = 0usize;
    let mut health = Health {
        failing: false,
        delay: cfg.backoff_initial,
        retry_at: None,
    };

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let now = Instant::now();
        let backing_off = health.retry_at.is_some_and(|t| now < t);

        if !backing_off {
            pending.extend(source.poll());
            while pending.len() > MAX_PENDING_EVENTS {
                pending.pop_front();
            }

            // Ack bookkeeping: stay on an acked target, otherwise try the next one.
            if now.duration_since(ack_window) >= cfg.ack_timeout {
                if !shared.acked.swap(false, Ordering::Relaxed) && targets.len() > 1 {
                    idx = (idx + 1) % targets.len();
                    tracing::debug!(
                        target_index = idx,
                        "input sender: no ack, trying next host address"
                    );
                }
                ack_window = now;
            }

            let due = last_send.is_none_or(|t| now.duration_since(t) >= cfg.keepalive_interval);
            if !pending.is_empty() || due {
                let mut state = SendState {
                    link: &mut link,
                    idx: &mut idx,
                    pending: &mut pending,
                    last_send: &mut last_send,
                    health: &mut health,
                };
                send_pending(&cfg, &shared, &targets, epoch, &mut state).await;
            }
        }

        // Sleep until the next thing that could need doing.
        let now = Instant::now();
        let mut wake = last_send.map_or(now, |t| t + cfg.keepalive_interval);
        if source.needs_polling() {
            wake = wake.min(now + cfg.poll_interval);
        }
        if let Some(t) = health.retry_at.filter(|t| *t > now) {
            wake = t;
        }
        tokio::select! {
            biased;
            r = shutdown_rx.changed() => {
                if r.is_err() || *shutdown_rx.borrow() { break; }
            }
            r = targets_rx.changed() => {
                if r.is_err() { break; }
                targets = targets_rx.borrow_and_update().clone();
                idx = 0;
                ack_window = Instant::now();
                shared.acked.store(false, Ordering::Relaxed);
                last_send = None; // keepalive to the new host at once
                health.retry_at = None;
                // A different address family needs a different socket.
                if let (Some(l), Some(first)) = (&link, targets.first()) {
                    if l.v6 != first.is_ipv6() {
                        link = None;
                    }
                }
            }
            _ = shared.notify.notified() => {}
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
}

struct SendState<'a> {
    link: &'a mut Option<Link>,
    idx: &'a mut usize,
    pending: &'a mut VecDeque<InputEvent>,
    last_send: &'a mut Option<Instant>,
    health: &'a mut Health,
}

async fn send_pending(
    cfg: &InputSenderConfig,
    shared: &Shared,
    targets: &[SocketAddr],
    epoch: Instant,
    st: &mut SendState<'_>,
) {
    let now = Instant::now();
    if targets.is_empty() {
        // Nothing to talk to yet: drop input, and pace the idle loop.
        st.pending.clear();
        *st.last_send = Some(now);
        return;
    }
    if st.link.is_none() {
        match bind_link(targets[0].is_ipv6(), cfg.tos).await {
            Ok(l) => *st.link = Some(l),
            Err(e) => {
                shared.send_errors.fetch_add(1, Ordering::Relaxed);
                st.health.on_error(cfg, now, &e.to_string());
                shared.failing.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
    let Some(l) = st.link.as_mut() else { return };
    let candidates: Vec<SocketAddr> = targets
        .iter()
        .copied()
        .filter(|a| a.is_ipv6() == l.v6)
        .collect();
    if candidates.is_empty() {
        st.pending.clear();
        *st.last_send = Some(now);
        return;
    }

    loop {
        let slot = *st.idx % candidates.len();
        shared.target_index.store(slot, Ordering::Relaxed);
        l.sender.set_remote_addr(candidates[slot]);
        let n = st.pending.len().min(MAX_EVENTS_PER_DATAGRAM);
        let chunk: Vec<InputEvent> = st.pending.iter().take(n).cloned().collect();
        let ts = Instant::now().duration_since(epoch).as_micros() as u64;
        match l.sender.send_events(chunk, ts).await {
            Ok(seq) => {
                st.pending.drain(..n);
                *st.last_send = Some(Instant::now());
                shared.last_seq.store(seq, Ordering::Relaxed);
                shared.datagrams.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    shared.keepalives.fetch_add(1, Ordering::Relaxed);
                }
                st.health.on_ok();
                shared.failing.store(false, Ordering::Relaxed);
                if st.pending.is_empty() {
                    break;
                }
            }
            Err(TransportError::Io(e)) => {
                shared.send_errors.fetch_add(1, Ordering::Relaxed);
                st.health.on_error(cfg, Instant::now(), &e.to_string());
                shared.failing.store(true, Ordering::Relaxed);
                // The address may be what is unreachable: try the next one after the backoff.
                *st.idx = (*st.idx + 1) % candidates.len();
                break;
            }
            Err(e) => {
                // Not a network problem (e.g. oversized batch): drop this chunk, keep going.
                tracing::warn!(error = %e, "input sender: dropping unsendable batch");
                st.pending.drain(..n);
                if st.pending.is_empty() {
                    *st.last_send = Some(Instant::now());
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
