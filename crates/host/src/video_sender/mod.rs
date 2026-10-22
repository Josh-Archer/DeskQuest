//! Host-side low-latency UDP video sender.
//!
//! Frames flow `FrameSource -> encoder -> packetizer (+FEC) -> pacer -> UDP socket` to
//! `<latched peer IP>:48440`, the port the Quest client listens on. There is no handshake and
//! no retransmission:
//!
//! * The destination is the headset IP the host already learned from its input and control
//!   channels ([`crate::link_status::LinkTracker::video_peer`]). Sending starts when a live
//!   network peer exists, follows it if the peer address changes, and stops when the link has
//!   been silent for the liveness timeout (5 s by default).
//! * Loss is handled by Reed-Solomon FEC only ([`packetizer`]); the client's jitter buffer
//!   recovers up to `m` lost datagrams per block of `k` source datagrams.
//! * A frame's datagrams are spread over a fraction of the frame interval (half by default)
//!   instead of leaving as one burst, using [`PacketPacer`].
//! * At most one frame waits to be encoded. A newer capture replaces it; the older one is
//!   dropped before it was ever encoded, so the encoder's reference chain is untouched. A frame
//!   that was already encoded but cannot be finished in time (a newer frame is waiting and a
//!   whole frame interval has passed) is abandoned and an IDR is requested, because the client
//!   can no longer decode from the frames it did receive.
//!
//! The pieces that touch hardware, [`FrameSource`], [`VideoEncoder`] and [`DatagramSink`], are
//! traits, so the whole pipeline runs in tests with synthetic frames over loopback sockets.
//!
//! One sender is one video stream. To show several screens the daemon runs one sender per
//! stream ([`multi`]); each has its own capture, encoder, packetizer and frame ids, and stamps
//! its [`VideoSenderConfig::stream_id`] on every packet.

pub mod daemon;
pub mod keyframe;
pub mod multi;
pub mod packetizer;
pub mod sink;

#[cfg(test)]
mod tests;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use linux_quest_capture::{CaptureError, RawFrame};
use linux_quest_encoder::VideoEncoder;
use linux_quest_transport::PacketPacer;
use parking_lot::Mutex;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, info_span, warn, Instrument, Span};

use crate::congestion::StreamRate;
use crate::link_status::SharedLinkTracker;

pub use keyframe::KeyframeRequests;
pub use packetizer::{
    Datagram, FecConfig, FramePlan, Packetizer, VideoSendError, DEFAULT_MAX_PAYLOAD,
    DEFAULT_VIDEO_PORT, MAX_DATAGRAM_SIZE,
};
pub use sink::{DatagramSink, Dscp, HybridSink, UdpSink, DEFAULT_SEND_BUFFER_BYTES};

/// Shortest pacing wait worth sleeping for. Timers wake up to about 1 ms late, so gaps below
/// this are sent as a micro-burst and the schedule catches up on the next real wait.
const MIN_PACING_WAIT: Duration = Duration::from_millis(1);

/// Longest a single FEC block's datagrams are spread over. The client learns a block's size and
/// parity count only when the parity packets arrive (the last datagrams of the block) and its
/// jitter buffer releases packets about 4 ms (the client default) after they arrive. If a block
/// were paced over longer than that, missing packets would be released before the parity that
/// could rebuild them. So the pacing rate is raised, if needed, until one block fits in this.
const MAX_BLOCK_SPAN: Duration = Duration::from_millis(2);

/// Shortest time between two live encoder bitrate changes. Rate-control reconfiguration is not
/// free, and the controller already limits how fast it moves.
const MIN_BITRATE_APPLY_INTERVAL: Duration = Duration::from_millis(100);

/// Width of the window the achieved video rate is averaged over.
const RATE_WINDOW: Duration = Duration::from_millis(500);

/// Consecutive capture failures after which capturing stops (with one error log).
const MAX_CONSECUTIVE_CAPTURE_ERRORS: u32 = 50;

/// A source of raw frames, normally the screen capture.
#[async_trait]
pub trait FrameSource: Send + 'static {
    /// Waits for the next frame. An `Err` is transient until it repeats
    /// [`MAX_CONSECUTIVE_CAPTURE_ERRORS`] times in a row.
    async fn next_frame(&mut self) -> Result<RawFrame, CaptureError>;
}

/// Tells the sender where the headset is, and whether it is still there.
pub trait PeerProvider: Send + Sync + 'static {
    /// The IP video should go to at `now`, or `None` to send nothing.
    fn video_peer(&self, now: Instant) -> Option<IpAddr>;
}

impl PeerProvider for SharedLinkTracker {
    fn video_peer(&self, now: Instant) -> Option<IpAddr> {
        self.lock().video_peer(now)
    }
}

#[derive(Debug, Clone)]
pub struct VideoSenderConfig {
    /// UDP port on the headset (the client binds `0.0.0.0:48440`).
    pub video_port: u16,
    /// Stream (screen) this sender carries; stamped on every data and parity packet. Equal to
    /// the display id. Stream 0 is the primary capture.
    pub stream_id: u16,
    /// Bitstream bytes per datagram (clamped to [`packetizer::MAX_PAYLOAD_LIMIT`]).
    pub max_payload: usize,
    pub fec: FecConfig,
    /// A frame's datagrams are spread over this fraction of the frame interval. `0.0` sends
    /// each frame as fast as the socket takes it. Values above `1.0` are treated as `1.0`.
    pub pacing_fraction: f64,
    /// Ceiling for the pacing rate. A keyframe too large to fit its budget at this rate takes
    /// longer than the fraction asked for.
    pub max_pacing_mbps: u32,
    /// Fallback frame rate if the encoder reports none.
    pub fallback_fps: u32,
    /// Keyframes requested because the local socket refused datagrams, or by the client, are
    /// at least this far apart, so neither can trigger an IDR storm.
    pub min_keyframe_interval: Duration,
    /// Set by the host input receiver when the client asks for a keyframe; the send loop takes
    /// the flag for its own `stream_id` and forces an IDR. `None` ignores client requests.
    pub keyframe_requests: Option<Arc<KeyframeRequests>>,
    /// Target bitrate and FEC ratio set live by the congestion controller. The sender applies
    /// them before each frame and reports the rate it achieved. `None` keeps the encoder and
    /// FEC settings fixed.
    pub rate: Option<Arc<StreamRate>>,
}

impl Default for VideoSenderConfig {
    fn default() -> Self {
        Self {
            video_port: DEFAULT_VIDEO_PORT,
            stream_id: 0,
            max_payload: DEFAULT_MAX_PAYLOAD,
            fec: FecConfig::default(),
            pacing_fraction: 0.5,
            max_pacing_mbps: 800,
            fallback_fps: 60,
            min_keyframe_interval: Duration::from_millis(250),
            keyframe_requests: None,
            rate: None,
        }
    }
}

/// Counters, readable while the sender runs.
#[derive(Debug, Default)]
pub struct VideoSenderStats {
    /// Frames handed over by the capture source.
    pub frames_captured: AtomicU64,
    /// Frames replaced in the one-deep pending slot before they were encoded.
    pub frames_dropped_pending: AtomicU64,
    /// Frames discarded because no live peer existed.
    pub frames_skipped_no_peer: AtomicU64,
    /// Frames whose datagrams were all handed to the socket.
    pub frames_sent: AtomicU64,
    /// Encoded frames abandoned part-way because a newer frame was waiting.
    pub frames_abandoned: AtomicU64,
    pub encode_errors: AtomicU64,
    pub datagrams_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Datagrams dropped because the socket buffer was full.
    pub send_would_block: AtomicU64,
    /// Datagrams the socket refused for any other reason.
    pub send_errors: AtomicU64,
    pub keyframes_requested: AtomicU64,
}

impl VideoSenderStats {
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// What happened to one frame.
#[derive(Debug, Clone)]
pub struct FrameReport {
    pub frame_id: u64,
    pub is_keyframe: bool,
    /// Datagrams handed to the socket.
    pub datagrams: usize,
    pub parity_datagrams: usize,
    pub bytes: usize,
    /// Datagrams the socket did not accept.
    pub send_failures: usize,
    /// The frame was cut short because a newer frame was waiting.
    pub abandoned: bool,
    /// From the frame entering [`FramePipeline::send_frame`] to its last datagram leaving.
    pub latency: Duration,
    /// How long the frame's datagrams were meant to be spread over (zero if unpaced).
    pub pacing_budget: Duration,
}

/// Encode, packetize, pace and send. Owns everything that is single-threaded per stream.
pub struct FramePipeline<E: VideoEncoder, K: DatagramSink> {
    encoder: E,
    sink: K,
    packetizer: Packetizer,
    pacer: PacketPacer,
    cfg: VideoSenderConfig,
    stats: Arc<VideoSenderStats>,
    epoch: Instant,
    last_keyframe_request: Option<Instant>,
    /// An IDR was requested and no keyframe has been planned since.
    keyframe_pending: bool,
    applied_kbps: Option<u32>,
    last_bitrate_apply: Option<Instant>,
    rate_window: RateWindow,
}

/// Measures the video payload rate (FEC parity excluded) over fixed windows.
#[derive(Debug)]
struct RateWindow {
    started: Option<Instant>,
    payload_bytes: u64,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started: None,
            payload_bytes: 0,
        }
    }

    /// Adds `payload_bytes` sent at `now`; returns kbit/s when a window just closed.
    fn record(&mut self, payload_bytes: u64, now: Instant) -> Option<u32> {
        let started = *self.started.get_or_insert(now);
        self.payload_bytes += payload_bytes;
        let elapsed = now.saturating_duration_since(started);
        if elapsed < RATE_WINDOW {
            return None;
        }
        let kbps = (self.payload_bytes * 8) as f64 / elapsed.as_secs_f64() / 1000.0;
        self.started = Some(now);
        self.payload_bytes = 0;
        Some(kbps.round().min(f64::from(u32::MAX)) as u32)
    }
}

/// Why an IDR is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeReason {
    /// A decoder is about to start (first peer, re-latched peer, resumed link).
    NewPeer,
    /// An already-encoded frame was dropped, so the client cannot decode what follows it.
    Abandoned,
    /// The local socket refused datagrams. Rate limited, so congestion cannot cause IDR storms.
    SendFailure,
    /// The client lost video and asked for one ([`InputEvent::KeyframeRequest`], or
    /// [`InputEvent::KeyframeRequestFor`] for a stream other than 0, on the input channel).
    /// Rate limited like `SendFailure`.
    ///
    /// [`InputEvent::KeyframeRequest`]: linux_quest_protocol::InputEvent::KeyframeRequest
    /// [`InputEvent::KeyframeRequestFor`]: linux_quest_protocol::InputEvent::KeyframeRequestFor
    ClientRequest,
}

impl<E: VideoEncoder, K: DatagramSink> FramePipeline<E, K> {
    pub fn new(encoder: E, sink: K, cfg: VideoSenderConfig, stats: Arc<VideoSenderStats>) -> Self {
        let packetizer = Packetizer::new(cfg.stream_id, cfg.max_payload, cfg.fec);
        Self {
            encoder,
            sink,
            packetizer,
            pacer: PacketPacer::new(cfg.max_pacing_mbps),
            cfg,
            stats,
            epoch: Instant::now(),
            last_keyframe_request: None,
            keyframe_pending: false,
            applied_kbps: None,
            last_bitrate_apply: None,
            rate_window: RateWindow::new(),
        }
    }

    /// Applies the congestion controller's current target to the encoder and packetizer.
    fn apply_rate(&mut self, rate: &StreamRate, now: Instant) {
        let target = rate.target_kbps();
        let due = self
            .last_bitrate_apply
            .is_none_or(|t| now.saturating_duration_since(t) >= MIN_BITRATE_APPLY_INTERVAL);
        if target > 0 && self.applied_kbps != Some(target) && due {
            if self.encoder.set_bitrate(target) {
                debug!(
                    stream = self.cfg.stream_id,
                    kbps = target,
                    "encoder bitrate changed"
                );
            } else if self.applied_kbps.is_some() {
                // Remember the attempt so an encoder that cannot retune is asked once per
                // change, not on every frame.
                debug!(
                    stream = self.cfg.stream_id,
                    kbps = target,
                    "encoder cannot retune live"
                );
            }
            self.applied_kbps = Some(target);
            self.last_bitrate_apply = Some(now);
        }
        self.packetizer.set_parity_ratio(rate.fec_ratio());
    }

    pub fn encoder(&self) -> &E {
        &self.encoder
    }

    pub fn sink(&self) -> &K {
        &self.sink
    }

    /// Asks the encoder for an IDR, unless one is already requested and not yet produced.
    /// [`KeyframeReason::SendFailure`] requests are also spaced by `min_keyframe_interval`.
    pub fn request_keyframe(&mut self, now: Instant, reason: KeyframeReason) {
        if self.keyframe_pending {
            return;
        }
        if matches!(
            reason,
            KeyframeReason::SendFailure | KeyframeReason::ClientRequest
        ) {
            if let Some(last) = self.last_keyframe_request {
                if now.saturating_duration_since(last) < self.cfg.min_keyframe_interval {
                    return;
                }
            }
        }
        self.keyframe_pending = true;
        self.last_keyframe_request = Some(now);
        self.stats
            .keyframes_requested
            .fetch_add(1, Ordering::Relaxed);
        self.encoder.request_keyframe();
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64 + 1
    }

    fn pacing_budget(&self, fps: u32) -> Duration {
        if self.cfg.pacing_fraction <= 0.0 {
            return Duration::ZERO;
        }
        let fps = if fps == 0 { self.cfg.fallback_fps } else { fps }.max(1);
        Duration::from_secs_f64(self.cfg.pacing_fraction.min(1.0) / f64::from(fps))
    }

    /// Encodes `raw` and sends it to `dest`. Returns `None` if the encoder produced no data.
    ///
    /// `newer_frame_waiting` is polled between datagrams once a full frame interval has passed;
    /// when it returns true the rest of the frame is abandoned and an IDR is requested.
    pub async fn send_frame(
        &mut self,
        raw: &RawFrame,
        dest: SocketAddr,
        newer_frame_waiting: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<FrameReport>, VideoSendError> {
        let frame_in = Instant::now();
        if let Some(rate) = self.cfg.rate.clone() {
            self.apply_rate(&rate, frame_in);
        }
        let encoded = self.encoder.encode(raw).await?;
        let Some(mut plan) = self.packetizer.plan(&encoded)? else {
            return Ok(None);
        };
        if plan.is_keyframe {
            self.keyframe_pending = false;
        }

        let fps = if plan.fps == 0 {
            self.cfg.fallback_fps
        } else {
            plan.fps
        }
        .max(1);
        let frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let budget = self.pacing_budget(plan.fps);
        let paced = !budget.is_zero();
        if paced {
            // Mbps is bits per microsecond. Take the rate that spreads the frame over its
            // budget, or the one that fits the largest block into `MAX_BLOCK_SPAN`, whichever
            // is faster.
            let over_budget =
                (plan.wire_bytes as u64 * 8).div_ceil((budget.as_micros() as u64).max(1));
            let per_block = (plan.max_block_wire_bytes as u64 * 8)
                .div_ceil((MAX_BLOCK_SPAN.as_micros() as u64).max(1));
            let mbps = over_budget.max(per_block);
            self.pacer
                .set_target_mbps(mbps.clamp(1, u64::from(self.cfg.max_pacing_mbps.max(1))) as u32);
        }

        let mut report = FrameReport {
            frame_id: plan.frame_id,
            is_keyframe: plan.is_keyframe,
            datagrams: 0,
            parity_datagrams: 0,
            bytes: 0,
            send_failures: 0,
            abandoned: false,
            latency: Duration::ZERO,
            pacing_budget: budget,
        };

        // `cursor` is the virtual time the next datagram is due; see `PacketPacer::reserve`.
        let mut cursor = Instant::now();
        let started = cursor;
        'blocks: while let Some(block) = self.packetizer.next_block(&mut plan, self.now_us())? {
            // A block's parity may take a while to compute. If that left the schedule behind by
            // more than a late timer wake-up explains, pacing restarts from the present so the
            // block is spread out instead of sent as one catch-up burst. Smaller lags are kept,
            // so a late wake-up does not push every later block back.
            let now = Instant::now();
            if now > cursor + MIN_PACING_WAIT {
                cursor = now;
            }
            for datagram in block {
                if paced {
                    let now = Instant::now();
                    if now > cursor + budget {
                        // Stalled (descheduled for most of a frame): do not burst to catch up.
                        cursor = now;
                    }
                    cursor += self.pacer.reserve(datagram.bytes.len(), cursor);
                    if cursor > now + MIN_PACING_WAIT {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(cursor)).await;
                    }
                }
                if started.elapsed() >= frame_interval && newer_frame_waiting() {
                    report.abandoned = true;
                    break 'blocks;
                }
                match self.sink.send(&datagram.bytes, dest) {
                    Ok(()) => {
                        report.datagrams += 1;
                        report.bytes += datagram.bytes.len();
                        report.parity_datagrams += usize::from(datagram.is_parity);
                    }
                    Err(e) => {
                        report.send_failures += 1;
                        if e.kind() == io::ErrorKind::WouldBlock {
                            self.stats.send_would_block.fetch_add(1, Ordering::Relaxed);
                        } else if self.stats.send_errors.fetch_add(1, Ordering::Relaxed) == 0 {
                            warn!(error = %e, %dest, "UDP video send failed");
                        }
                    }
                }
            }
        }

        self.stats
            .datagrams_sent
            .fetch_add(report.datagrams as u64, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(report.bytes as u64, Ordering::Relaxed);
        if report.abandoned {
            self.stats.frames_abandoned.fetch_add(1, Ordering::Relaxed);
            self.request_keyframe(Instant::now(), KeyframeReason::Abandoned);
        } else {
            self.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
            if report.send_failures > 0 {
                // Some of the frame never left the host; FEC may or may not cover it, and
                // the encoder cannot know. Resynchronize the decoder with an IDR.
                self.request_keyframe(Instant::now(), KeyframeReason::SendFailure);
            }
        }
        if let Some(rate) = &self.cfg.rate {
            let source = (report.datagrams - report.parity_datagrams) as u64;
            let payload = if report.datagrams == 0 {
                0
            } else {
                report.bytes as u64 * source / report.datagrams as u64
            };
            if let Some(kbps) = self.rate_window.record(payload, Instant::now()) {
                rate.set_actual_kbps(kbps);
            }
        }
        report.latency = frame_in.elapsed();
        Ok(Some(report))
    }
}

/// One-deep, newest-wins hand-off between capture and the sender.
pub struct FrameSlot {
    pending: Mutex<Option<RawFrame>>,
    notify: Notify,
    stats: Arc<VideoSenderStats>,
}

impl FrameSlot {
    pub fn new(stats: Arc<VideoSenderStats>) -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            stats,
        }
    }

    /// Stores `frame`, dropping the frame that was still waiting. Returns whether one was dropped.
    pub fn put(&self, frame: RawFrame) -> bool {
        let replaced = self.pending.lock().replace(frame).is_some();
        if replaced {
            self.stats
                .frames_dropped_pending
                .fetch_add(1, Ordering::Relaxed);
        }
        self.notify.notify_one();
        replaced
    }

    pub fn has_pending(&self) -> bool {
        self.pending.lock().is_some()
    }

    /// Waits for and takes the pending frame.
    pub async fn take(&self) -> RawFrame {
        loop {
            if let Some(frame) = self.pending.lock().take() {
                return frame;
            }
            self.notify.notified().await;
        }
    }
}

/// Running sender. Dropping it does not stop the tasks; call [`VideoSenderHandle::stop`].
pub struct VideoSenderHandle {
    stats: Arc<VideoSenderStats>,
    video_port: u16,
    stop_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl VideoSenderHandle {
    pub fn stats(&self) -> &Arc<VideoSenderStats> {
        &self.stats
    }

    /// The headset UDP port video is sent to.
    pub fn video_port(&self) -> u16 {
        self.video_port
    }

    /// Stops capture and sending and waits for both tasks.
    pub async fn stop(self) {
        let _ = self.stop_tx.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Starts the capture task and the sender task. `peers` decides when and where to send.
pub fn spawn_video_sender<S, E, K, P>(
    source: S,
    encoder: E,
    sink: K,
    peers: P,
    cfg: VideoSenderConfig,
) -> VideoSenderHandle
where
    S: FrameSource,
    E: VideoEncoder + 'static,
    K: DatagramSink,
    P: PeerProvider,
{
    let stats = Arc::new(VideoSenderStats::default());
    let slot = Arc::new(FrameSlot::new(stats.clone()));
    let (stop_tx, stop_rx) = watch::channel(false);
    let video_port = cfg.video_port;
    // Log lines of an extra stream carry `video_stream{stream_id=N}`; stream 0, the only stream
    // of a single-screen daemon, logs exactly as before.
    let span = if cfg.stream_id == 0 {
        Span::none()
    } else {
        info_span!("video_stream", stream_id = cfg.stream_id)
    };

    let capture = tokio::spawn(
        capture_loop(source, slot.clone(), stats.clone(), stop_rx.clone()).instrument(span.clone()),
    );
    let pipeline = FramePipeline::new(encoder, sink, cfg, stats.clone());
    let sender =
        tokio::spawn(send_loop(pipeline, slot, peers, stats.clone(), stop_rx).instrument(span));

    VideoSenderHandle {
        stats,
        video_port,
        stop_tx,
        tasks: vec![capture, sender],
    }
}

async fn capture_loop<S: FrameSource>(
    mut source: S,
    slot: Arc<FrameSlot>,
    stats: Arc<VideoSenderStats>,
    mut stop: watch::Receiver<bool>,
) {
    let mut consecutive_errors = 0u32;
    loop {
        let result = tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => break,
            result = source.next_frame() => result,
        };
        match result {
            Ok(frame) => {
                consecutive_errors = 0;
                stats.frames_captured.fetch_add(1, Ordering::Relaxed);
                slot.put(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 {
                    warn!(error = %e, "Video capture failed; retrying");
                }
                if consecutive_errors >= MAX_CONSECUTIVE_CAPTURE_ERRORS {
                    error!(
                        error = %e,
                        "Video capture failed {consecutive_errors} times in a row; video sending stopped"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn send_loop<E, K, P>(
    mut pipeline: FramePipeline<E, K>,
    slot: Arc<FrameSlot>,
    peers: P,
    stats: Arc<VideoSenderStats>,
    mut stop: watch::Receiver<bool>,
) where
    E: VideoEncoder + 'static,
    K: DatagramSink,
    P: PeerProvider,
{
    let video_port = pipeline.cfg.video_port;
    let stream_id = pipeline.cfg.stream_id;
    let mut active: Option<IpAddr> = None;
    let mut encode_failures = 0u64;
    loop {
        let raw = tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => break,
            raw = slot.take() => raw,
        };
        let now = Instant::now();
        let Some(ip) = peers.video_peer(now) else {
            if active.take().is_some() {
                info!("Headset went silent; UDP video sending paused");
            }
            stats.frames_skipped_no_peer.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if active != Some(ip) {
            match active {
                None => info!(peer = %ip, port = video_port, "Starting UDP video to headset"),
                Some(old) => {
                    info!(old = %old, new = %ip, "Headset address changed; retargeting UDP video")
                }
            }
            active = Some(ip);
            // A new decoder needs an IDR to start from.
            pipeline.request_keyframe(now, KeyframeReason::NewPeer);
        }
        let client_asked = pipeline
            .cfg
            .keyframe_requests
            .as_ref()
            .is_some_and(|requests| requests.take(stream_id));
        if client_asked {
            debug!(stream_id, "Client requested a keyframe");
            pipeline.request_keyframe(now, KeyframeReason::ClientRequest);
        }

        let dest = SocketAddr::new(ip, video_port);
        // Shutdown cancels a frame mid-send; nothing is left half-updated by that.
        let newer_frame_waiting = || slot.has_pending();
        let result = tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => break,
            result = pipeline.send_frame(&raw, dest, &newer_frame_waiting) => result,
        };
        if let Err(e) = result {
            stats.encode_errors.fetch_add(1, Ordering::Relaxed);
            encode_failures += 1;
            if encode_failures == 1 || encode_failures.is_multiple_of(100) {
                warn!(error = %e, failures = encode_failures, "Video frame could not be sent");
            } else {
                debug!(error = %e, "Video frame could not be sent");
            }
        }
    }
}
