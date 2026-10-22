//! Hardware-free tests of the whole sender: synthetic frames in, real UDP loopback sockets,
//! and the same jitter buffer and reassembler types the Quest client uses on the other end.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle as ThreadHandle;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use linux_quest_capture::{CaptureError, PixelFormat, RawFrame};
use linux_quest_client::config::ClientConfig;
use linux_quest_client::receiver::ClientReceiver;
use linux_quest_encoder::{
    EncodedFrame, EncoderConfig, EncoderError, MockVideoEncoder, VideoEncoder,
};
use linux_quest_protocol::{
    AssembledFrame, Packet, PacketHeader, PacketType, VideoChunk, VideoChunkMeta, VideoCodec,
    HEADER_SIZE,
};
use parking_lot::Mutex;

use super::*;
use crate::link_status::{new_shared_link_tracker, SharedLinkTracker};

/// The tests below depend on wall-clock timing (jitter buffer release, pacing, liveness), so
/// they take turns instead of competing for CPU with each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------------

fn raw_frame(width: u32, height: u32, data: Bytes, pts_us: u64) -> RawFrame {
    RawFrame {
        display_id: 0,
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Bgra8,
        pts_us,
        dma_buf: None,
        data,
    }
}

/// Synthetic screen: `count` frames, one per `interval`, then it waits forever (like a screen
/// that stopped changing). The first `transient_errors` calls fail.
struct TestSource {
    remaining: u32,
    interval: Duration,
    counter: u64,
    data_len: usize,
    transient_errors: u32,
    produced: Arc<Mutex<Vec<(u64, Bytes)>>>,
}

impl TestSource {
    fn new(count: u32, interval: Duration, data_len: usize) -> Self {
        Self {
            remaining: count,
            interval,
            counter: 0,
            data_len,
            transient_errors: 0,
            produced: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl FrameSource for TestSource {
    async fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
        if self.transient_errors > 0 {
            self.transient_errors -= 1;
            return Err(CaptureError::CaptureFailed("transient".into()));
        }
        if self.remaining == 0 {
            std::future::pending::<()>().await;
        }
        if !self.interval.is_zero() {
            tokio::time::sleep(self.interval).await;
        }
        self.remaining -= 1;
        self.counter += 1;
        let n = self.counter;
        let data: Bytes = (0..self.data_len)
            .map(|i| (i as u64).wrapping_mul(37).wrapping_add(n * 11) as u8)
            .collect::<Vec<u8>>()
            .into();
        let pts = n * 5_000;
        self.produced.lock().push((pts, data.clone()));
        Ok(raw_frame(640, 360, data, pts))
    }
}

/// Deterministic bitstream for frame `frame_id`.
fn pattern(frame_id: u64, len: usize) -> Bytes {
    (0..len)
        .map(|i| {
            (i as u64)
                .wrapping_mul(131)
                .wrapping_add(frame_id * 7)
                .wrapping_add((i as u64 >> 8) * 13) as u8
        })
        .collect::<Vec<u8>>()
        .into()
}

fn chunked(
    frame_id: u64,
    bitstream: &Bytes,
    chunk: usize,
    keyframe: bool,
    fps: u32,
    pts_us: u64,
    (width, height): (u32, u32),
) -> EncodedFrame {
    let total = bitstream.len().div_ceil(chunk).max(1);
    let chunks = (0..total)
        .map(|i| {
            let start = (i * chunk).min(bitstream.len());
            let end = (start + chunk).min(bitstream.len());
            VideoChunk::new(
                VideoChunkMeta {
                    frame_id,
                    chunk_index: i as u16,
                    total_chunks: total as u16,
                    codec: VideoCodec::Hevc,
                    is_keyframe: keyframe,
                    is_intra_refresh: false,
                    width,
                    height,
                    fps,
                    pts_us,
                },
                bitstream.slice(start..end),
            )
        })
        .collect();
    EncodedFrame {
        frame_id,
        is_keyframe: keyframe,
        is_intra_refresh: false,
        picture_type: if keyframe { 3 } else { 0 },
        chunks,
        encode_duration_us: 0,
    }
}

/// Encoder that emits frames of scripted sizes (cycled), keyframe on the first frame and after
/// each `request_keyframe`.
struct ScriptedEncoder {
    sizes: Vec<usize>,
    chunk: usize,
    fps: u32,
    counter: u64,
    force_key: bool,
    delay: Duration,
    key_requests: Arc<AtomicU64>,
    encoded: Arc<AtomicU64>,
    produced: Arc<Mutex<HashMap<u64, Bytes>>>,
    keyframes: Arc<Mutex<Vec<u64>>>,
    pts_seen: Arc<Mutex<Vec<u64>>>,
}

impl ScriptedEncoder {
    fn new(sizes: Vec<usize>, fps: u32) -> Self {
        Self {
            sizes,
            chunk: 1200,
            fps,
            counter: 0,
            force_key: true,
            delay: Duration::ZERO,
            key_requests: Arc::new(AtomicU64::new(0)),
            encoded: Arc::new(AtomicU64::new(0)),
            produced: Arc::new(Mutex::new(HashMap::new())),
            keyframes: Arc::new(Mutex::new(Vec::new())),
            pts_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl VideoEncoder for ScriptedEncoder {
    async fn init(&mut self, _config: EncoderConfig) -> Result<(), EncoderError> {
        Ok(())
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.pts_seen.lock().push(frame.pts_us);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.counter += 1;
        self.encoded.fetch_add(1, Ordering::Relaxed);
        let id = self.counter;
        let size = self.sizes[(id as usize - 1) % self.sizes.len()];
        let bitstream = pattern(id, size);
        self.produced.lock().insert(id, bitstream.clone());
        let keyframe = std::mem::take(&mut self.force_key);
        if keyframe {
            self.keyframes.lock().push(id);
        }
        Ok(chunked(
            id,
            &bitstream,
            self.chunk,
            keyframe,
            self.fps,
            frame.pts_us,
            (frame.width, frame.height),
        ))
    }

    fn request_keyframe(&mut self) {
        self.key_requests.fetch_add(1, Ordering::Relaxed);
        self.force_key = true;
    }

    fn invalidate_reference_picture(&mut self, _pts_us: u64) {}
}

/// Wraps any encoder to count keyframe requests and encodes, optionally delaying each encode.
struct CountingEncoder<E: VideoEncoder> {
    inner: E,
    key_requests: Arc<AtomicU64>,
    encoded: Arc<AtomicU64>,
    delay: Duration,
}

impl<E: VideoEncoder> CountingEncoder<E> {
    fn new(inner: E) -> Self {
        Self {
            inner,
            key_requests: Arc::new(AtomicU64::new(0)),
            encoded: Arc::new(AtomicU64::new(0)),
            delay: Duration::ZERO,
        }
    }
}

#[async_trait]
impl<E: VideoEncoder> VideoEncoder for CountingEncoder<E> {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.inner.init(config).await
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.encoded.fetch_add(1, Ordering::Relaxed);
        self.inner.encode(frame).await
    }

    fn request_keyframe(&mut self) {
        self.key_requests.fetch_add(1, Ordering::Relaxed);
        self.inner.request_keyframe();
    }

    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.inner.invalidate_reference_picture(pts_us);
    }
}

/// Peer provider the test steers by hand.
#[derive(Clone)]
struct ManualPeer(Arc<Mutex<Option<IpAddr>>>);

impl ManualPeer {
    fn at(ip: &str) -> Self {
        Self(Arc::new(Mutex::new(Some(ip.parse().unwrap()))))
    }
}

impl PeerProvider for ManualPeer {
    fn video_peer(&self, _now: Instant) -> Option<IpAddr> {
        *self.0.lock()
    }
}

/// Records every datagram instead of sending it.
#[derive(Clone, Default)]
struct RecordingSink {
    sent: Arc<Mutex<Vec<(Instant, SocketAddr, usize)>>>,
}

impl DatagramSink for RecordingSink {
    fn send(&mut self, datagram: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        self.sent
            .lock()
            .push((Instant::now(), dest, datagram.len()));
        Ok(())
    }
}

/// Sink whose socket buffer is always full.
struct FullSink;

impl DatagramSink for FullSink {
    fn send(&mut self, _datagram: &[u8], _dest: SocketAddr) -> std::io::Result<()> {
        Err(std::io::ErrorKind::WouldBlock.into())
    }
}

// ---------------------------------------------------------------------------------------------
// Client-side receiver and a lossy relay, on real sockets
// ---------------------------------------------------------------------------------------------

/// The client's own receive path (`ClientReceiver`: adaptive jitter buffer with FEC recovery,
/// then the frame reassembler) behind a UDP socket.
struct LoopbackReceiver {
    addr: SocketAddr,
    frames: Arc<Mutex<Vec<(Instant, AssembledFrame)>>>,
    fec_recovered: Arc<AtomicU64>,
    packets_lost: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<ThreadHandle<()>>,
}

impl LoopbackReceiver {
    /// Receiver with the client's default configuration (4 ms jitter-buffer playout delay).
    fn spawn() -> Self {
        Self::spawn_with_depth(ClientConfig::default().jitter_buffer_initial_depth_ms)
    }

    /// Receiver whose jitter buffer holds packets for `depth_ms` before playout.
    fn spawn_with_depth(depth_ms: u64) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let fec_recovered = Arc::new(AtomicU64::new(0));
        let packets_lost = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let (f, r, l, s) = (
            frames.clone(),
            fec_recovered.clone(),
            packets_lost.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            let mut receiver = ClientReceiver::new(&ClientConfig {
                jitter_buffer_initial_depth_ms: depth_ms,
                ..ClientConfig::default()
            });
            let mut buf = vec![0u8; 65536];
            let mut quiet_since: Option<Instant> = None;
            loop {
                if let Ok((n, _)) = socket.recv_from(&mut buf) {
                    quiet_since = None;
                    if let Ok(packet) = Packet::from_bytes(&buf[..n]) {
                        receiver.ingest_packet(packet, Instant::now());
                    }
                }
                let now = Instant::now();
                let ready = receiver.pop_ready_frames(now).unwrap();
                if !ready.is_empty() {
                    quiet_since = None;
                    f.lock().extend(ready.into_iter().map(|frame| (now, frame)));
                }
                r.store(receiver.fec_recovered_count(), Ordering::Relaxed);
                l.store(receiver.packets_lost_count(), Ordering::Relaxed);
                if s.load(Ordering::Relaxed) {
                    // After stop, keep draining until nothing arrives for a while so FEC hold
                    // timeouts and jitter delays have expired.
                    let since = *quiet_since.get_or_insert(now);
                    if now.duration_since(since) > Duration::from_millis(150) {
                        break;
                    }
                }
            }
        });
        Self {
            addr,
            frames,
            fec_recovered,
            packets_lost,
            stop,
            thread: Some(thread),
        }
    }

    /// Stops after draining and returns the delivered frames in arrival order.
    fn finish(mut self) -> Vec<(Instant, AssembledFrame)> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        std::mem::take(&mut *self.frames.lock())
    }
}

/// UDP relay between sender and receiver that drops datagrams chosen by `should_drop`.
struct LossyRelay {
    addr: SocketAddr,
    dropped: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<ThreadHandle<()>>,
}

impl LossyRelay {
    fn spawn(
        to: SocketAddr,
        mut should_drop: impl FnMut(u64, &[u8]) -> bool + Send + 'static,
    ) -> Self {
        let input = UdpSocket::bind("127.0.0.1:0").unwrap();
        input
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let addr = input.local_addr().unwrap();
        let output = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dropped = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (d, s) = (dropped.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let mut index = 0u64;
            while !s.load(Ordering::Relaxed) {
                if let Ok((n, _)) = input.recv_from(&mut buf) {
                    if should_drop(index, &buf[..n]) {
                        d.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let _ = output.send_to(&buf[..n], to);
                    }
                    index += 1;
                }
            }
        });
        Self {
            addr,
            dropped,
            stop,
            thread: Some(thread),
        }
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

/// Header facts the loss predicates need.
struct DatagramInfo {
    packet_type: PacketType,
    fec_index: u16,
    frame_id: Option<u64>,
}

fn inspect(datagram: &[u8]) -> DatagramInfo {
    let mut slice = datagram;
    let header = PacketHeader::decode(&mut slice).expect("sender emits valid headers");
    let frame_id = (header.packet_type == PacketType::VideoFrameChunk).then(|| {
        VideoChunk::deserialize(&datagram[HEADER_SIZE..])
            .unwrap()
            .meta
            .frame_id
    });
    DatagramInfo {
        packet_type: header.packet_type,
        fec_index: header.fec_index(),
        frame_id,
    }
}

/// xorshift, so loss patterns are reproducible.
struct Rng(u64);

impl Rng {
    fn chance(&mut self, p: f64) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

fn config_to(port: u16) -> VideoSenderConfig {
    VideoSenderConfig {
        video_port: port,
        ..VideoSenderConfig::default()
    }
}

fn udp_sink() -> UdpSink {
    UdpSink::new(DEFAULT_SEND_BUFFER_BYTES, Dscp::Af41).unwrap()
}

async fn until_async(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

// ---------------------------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_encoder_frames_arrive_bit_exact_over_udp_loopback() {
    let _serial = SERIAL.lock().await;
    let receiver = LoopbackReceiver::spawn();
    // 800 kB of "pixels" -> the mock encoder emits 4000 bytes -> 4 chunks per frame.
    let source = TestSource::new(20, Duration::from_millis(5), 800_000);
    let produced = source.produced.clone();
    let encoder = CountingEncoder::new(MockVideoEncoder::new(EncoderConfig {
        codec: VideoCodec::Hevc,
        width: 640,
        height: 360,
        fps: 200,
        ..EncoderConfig::default()
    }));

    let handle = spawn_video_sender(
        source,
        encoder,
        udp_sink(),
        ManualPeer::at("127.0.0.1"),
        config_to(receiver.addr.port()),
    );
    let stats = handle.stats().clone();
    assert!(
        until_async(Duration::from_secs(10), || VideoSenderStats::get(
            &stats.frames_sent
        ) >= 20)
        .await
    );
    handle.stop().await;
    let frames = receiver.finish();

    assert_eq!(frames.len(), 20, "every frame must be reconstructed");
    let produced = produced.lock();
    for (i, (_, frame)) in frames.iter().enumerate() {
        let (pts, data) = &produced[i];
        let expected = data.slice(..(data.len() / 200).clamp(64, 4096));
        assert_eq!(frame.bitstream, expected, "frame {} bit-exact", i + 1);
        assert_eq!(frame.meta.frame_id, i as u64 + 1);
        assert_eq!(
            frame.meta.pts_us, *pts,
            "capture timestamp is carried in the meta"
        );
        assert_eq!(frame.meta.codec, VideoCodec::Hevc);
        assert_eq!((frame.meta.width, frame.meta.height), (640, 360));
        assert_eq!(frame.meta.is_keyframe, i == 0);
    }
    assert_eq!(VideoSenderStats::get(&stats.frames_dropped_pending), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_of_every_size_arrive_bit_exact_across_fec_blocks() {
    let _serial = SERIAL.lock().await;
    let receiver = LoopbackReceiver::spawn();
    // 1 byte, below one chunk, exactly one chunk, one over, a few blocks, and 9 blocks.
    let sizes = vec![1, 100, 1200, 1201, 30_000, 24_000, 200_000, 2400];
    let source = TestSource::new(sizes.len() as u32, Duration::from_millis(15), 64);
    let encoder = ScriptedEncoder::new(sizes.clone(), 60);
    let produced = encoder.produced.clone();

    let handle = spawn_video_sender(
        source,
        encoder,
        udp_sink(),
        ManualPeer::at("127.0.0.1"),
        config_to(receiver.addr.port()),
    );
    let stats = handle.stats().clone();
    let n = sizes.len() as u64;
    assert!(
        until_async(Duration::from_secs(20), || VideoSenderStats::get(
            &stats.frames_sent
        ) >= n)
        .await
    );
    handle.stop().await;
    let frames = receiver.finish();

    assert_eq!(frames.len(), sizes.len());
    let produced = produced.lock();
    for (_, frame) in &frames {
        assert_eq!(
            frame.bitstream,
            produced[&frame.meta.frame_id],
            "frame {} ({} bytes)",
            frame.meta.frame_id,
            frame.bitstream.len()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Loss
// ---------------------------------------------------------------------------------------------

/// Playout delay of the receiver in the loss tests. The client default is 4 ms; a loaded CI
/// machine can delay a thread by more than the 2 ms margin the sender leaves, which would turn
/// a protocol test into a scheduler test. The sender-side guarantee that keeps a block inside
/// the 4 ms default is pinned separately by
/// `a_single_fec_block_is_never_paced_over_more_than_a_few_milliseconds`.
const RELAY_PLAYOUT_MS: u64 = 15;

/// 20 chunks of 1200 bytes -> one FEC block of k=20 with m=2 parity.
const BLOCK_FRAME: usize = 20 * 1200;

async fn run_through_relay(
    frames: u32,
    interval: Duration,
    playout_ms: u64,
    should_drop: impl FnMut(u64, &[u8]) -> bool + Send + 'static,
) -> (
    Vec<(Instant, AssembledFrame)>,
    ScriptedEncoderProbe,
    u64,
    u64,
    u64,
) {
    let receiver = LoopbackReceiver::spawn_with_depth(playout_ms);
    let relay = LossyRelay::spawn(receiver.addr, should_drop);
    let source = TestSource::new(frames, interval, 64);
    // Leave room for scheduler jitter: these tests validate FEC/reassembly, not the sender's
    // separate deadline-abandonment behavior.
    let encoder = ScriptedEncoder::new(vec![BLOCK_FRAME], 30);
    let probe = ScriptedEncoderProbe {
        produced: encoder.produced.clone(),
    };

    let handle = spawn_video_sender(
        source,
        encoder,
        udp_sink(),
        ManualPeer::at("127.0.0.1"),
        config_to(relay.addr.port()),
    );
    let stats = handle.stats().clone();
    assert!(
        until_async(Duration::from_secs(45), || VideoSenderStats::get(
            &stats.frames_sent
        ) >= u64::from(frames))
        .await
    );
    handle.stop().await;
    // Let the relay and the receiver run dry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let dropped = relay.dropped.load(Ordering::Relaxed);
    relay.finish();
    let recovered_probe = receiver.fec_recovered.clone();
    let lost_probe = receiver.packets_lost.clone();
    let delivered = receiver.finish();
    (
        delivered,
        probe,
        dropped,
        recovered_probe.load(Ordering::Relaxed),
        lost_probe.load(Ordering::Relaxed),
    )
}

struct ScriptedEncoderProbe {
    produced: Arc<Mutex<HashMap<u64, Bytes>>>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fec_recovers_up_to_its_design_limit() {
    let _serial = SERIAL.lock().await;
    // Drop the first five source datagrams of every block: exactly m = 5 losses per block.
    // Frame 1 is spared: the client's jitter buffer has no playout cursor until it has seen
    // a packet, so loss at the very start of a stream is outside the FEC contract.
    let (frames, probe, dropped, recovered, _) =
        run_through_relay(30, Duration::from_millis(100), RELAY_PLAYOUT_MS, |_, d| {
            let info = inspect(d);
            info.frame_id.is_some_and(|f| f > 1) && info.fec_index < 5
        })
        .await;

    assert_eq!(dropped, 5 * 29, "five datagrams of each of frames 2..=30");
    assert_eq!(frames.len(), 30, "m losses per block are fully recovered");
    let produced = probe.produced.lock();
    for (_, frame) in &frames {
        assert_eq!(frame.bitstream, produced[&frame.meta.frame_id], "bit-exact");
    }
    assert!(
        recovered >= 5 * 29,
        "FEC rebuilt the dropped datagrams: {recovered}"
    );
}

/// Measures, rather than assumes, what the client's default 4 ms playout delay does to FEC
/// recovery when the lost datagrams are the first of their block. The client learns a block's
/// parity count only from its parity packets, which trail the sources by up to the block span
/// (about 2 ms), so recovery has little slack. The threshold is loose because this is a
/// wall-clock property of the client's jitter buffer, not something the sender can guarantee.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fec_recovery_with_the_client_default_playout_delay() {
    let _serial = SERIAL.lock().await;
    let default_ms = ClientConfig::default().jitter_buffer_initial_depth_ms;
    let (frames, probe, _, _, _) =
        run_through_relay(60, Duration::from_millis(100), default_ms, |_, d| {
            let info = inspect(d);
            info.frame_id.is_some_and(|f| f > 1) && info.fec_index < 2
        })
        .await;
    eprintln!(
        "default {default_ms} ms playout: {}/60 frames recovered with m losses at the start of their block",
        frames.len()
    );
    assert!(
        frames.len() >= 48,
        "at least 80% of frames recover, got {}/60",
        frames.len()
    );
    let produced = probe.produced.lock();
    for (_, frame) in &frames {
        assert_eq!(frame.bitstream, produced[&frame.meta.frame_id], "bit-exact");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_beyond_the_fec_limit_are_dropped_without_stalling_later_frames() {
    let _serial = SERIAL.lock().await;
    // Frames 3, 6, 9, ... lose m + 1 = 6 sources of their block (m = 5 with the defaults):
    // unrecoverable.
    let (frames, probe, dropped, _, lost) =
        run_through_relay(30, Duration::from_millis(15), RELAY_PLAYOUT_MS, |_, d| {
            let info = inspect(d);
            info.packet_type == PacketType::VideoFrameChunk
                && info.fec_index < 6
                && info.frame_id.is_some_and(|f| f % 3 == 0)
        })
        .await;

    assert_eq!(
        dropped,
        10 * 6,
        "six sources of each of the ten damaged frames"
    );
    let ids: Vec<u64> = frames.iter().map(|(_, f)| f.meta.frame_id).collect();
    let expected: Vec<u64> = (1..=30).filter(|f| f % 3 != 0).collect();
    assert_eq!(
        ids, expected,
        "every recoverable frame arrives, damaged ones do not"
    );
    let produced = probe.produced.lock();
    for (_, frame) in &frames {
        assert_eq!(frame.bitstream, produced[&frame.meta.frame_id], "bit-exact");
    }
    assert!(
        lost >= 60,
        "the client counted the unrecoverable datagrams: {lost}"
    );

    // A damaged frame must not hold up the ones behind it: arrival gaps stay near the frame
    // interval (5 ms) plus the FEC hold (10 ms) and jitter delay, nowhere near a stall.
    for pair in frames.windows(2) {
        let gap = pair[1].0.duration_since(pair[0].0);
        assert!(
            gap < Duration::from_millis(150),
            "delivery stalled for {gap:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_loss_is_mostly_recovered_and_never_corrupts_a_frame() {
    let _serial = SERIAL.lock().await;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // 3% independent loss: a block (25 datagrams) fails only with 6 or more losses, ~0%.
    let (frames, probe, dropped, recovered, _) = run_through_relay(
        60,
        Duration::from_millis(15),
        RELAY_PLAYOUT_MS,
        move |_, _| rng.chance(0.03),
    )
    .await;

    assert!(
        dropped > 10,
        "the relay really dropped datagrams: {dropped}"
    );
    assert!(recovered > 0, "FEC recovered something");
    assert!(
        frames.len() >= 50,
        "at 3% loss with 10% parity nearly every frame survives, got {}/60",
        frames.len()
    );
    let produced = probe.produced.lock();
    for (_, frame) in &frames {
        assert_eq!(frame.bitstream, produced[&frame.meta.frame_id], "bit-exact");
    }
}

// ---------------------------------------------------------------------------------------------
// Burst loss inside one frame, through the client's receiver on a simulated clock
// ---------------------------------------------------------------------------------------------

/// Feeds the datagrams of one frame to the client's `ClientReceiver` on a simulated clock.
/// Datagram `i` arrives `gap * i` after the start and the receiver is polled every 50 us; lost
/// datagrams never arrive. The clock is simulated, so the result does not depend on scheduler
/// timing. Returns the frames the client delivered.
fn receive_on_simulated_clock(
    datagrams: &[Datagram],
    gap: Duration,
    lost: impl Fn(usize) -> bool,
) -> Vec<AssembledFrame> {
    let start = Instant::now();
    let mut receiver = ClientReceiver::new(&ClientConfig::default());
    let mut delivered = Vec::new();
    let mut next = 0;
    let mut t = Duration::ZERO;
    let end = gap * datagrams.len() as u32 + Duration::from_millis(60);
    while t <= end {
        while next < datagrams.len() && gap * next as u32 <= t {
            if !lost(next) {
                let packet = Packet::from_bytes(&datagrams[next].bytes).unwrap();
                receiver.ingest_packet(packet, start + gap * next as u32);
            }
            next += 1;
        }
        delivered.extend(receiver.pop_ready_frames(start + t).unwrap());
        t += Duration::from_micros(50);
    }
    delivered
}

#[test]
fn a_burst_of_m_datagrams_inside_one_block_is_recovered_bit_exact_and_m_plus_one_is_not() {
    // 60 chunks of 1200 bytes: three blocks of 20 sources and 5 parity, 75 datagrams in all.
    let bitstream = pattern(1, 60 * 1200);
    let encoded = chunked(1, &bitstream, 1200, true, 60, 16_666, (640, 360));
    let datagrams = Packetizer::new(0, 1200, FecConfig::default())
        .packetize(&encoded, 1_000)
        .unwrap();
    assert_eq!(datagrams.len(), 75);
    // The sender spreads a block over at most 2 ms (`MAX_BLOCK_SPAN`): 25 datagrams, 80 us apart.
    let gap = Duration::from_micros(80);

    // Five consecutive lost datagrams: the first block's first sources, and one in the middle
    // of the second block. Both fall within the 5 parity of their block.
    for first in [0usize, 35] {
        let frames =
            receive_on_simulated_clock(&datagrams, gap, |i| (first..first + 5).contains(&i));
        assert_eq!(frames.len(), 1, "burst at {first}: frame delivered");
        assert_eq!(
            frames[0].bitstream, bitstream,
            "burst at {first}: bit-exact"
        );
    }

    // Six in a row exceed one block's parity: the frame is not delivered, and nothing is
    // delivered wrongly.
    let frames = receive_on_simulated_clock(&datagrams, gap, |i| (0..6).contains(&i));
    assert!(
        frames.is_empty(),
        "six losses in one block are unrecoverable"
    );

    let frames = receive_on_simulated_clock(&datagrams, gap, |_| false);
    assert_eq!(frames.len(), 1, "no loss: delivered");
}

// ---------------------------------------------------------------------------------------------
// Peer latching
// ---------------------------------------------------------------------------------------------

fn tracker_with_liveness(ms: u64) -> SharedLinkTracker {
    new_shared_link_tracker(Duration::from_millis(ms))
}

fn peer(addr: &str) -> SocketAddr {
    addr.parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_the_latched_peer_retargets_and_stops_when_silent() {
    let _serial = SERIAL.lock().await;
    let link = tracker_with_liveness(300);
    let sink = RecordingSink::default();
    let sent = sink.sent.clone();
    let source = TestSource::new(100_000, Duration::from_millis(10), 64);
    let encoder = ScriptedEncoder::new(vec![2400], 100);
    let key_requests = encoder.key_requests.clone();
    let config = VideoSenderConfig {
        pacing_fraction: 0.0,
        ..config_to(48440)
    };
    let handle = spawn_video_sender(source, encoder, sink, link.clone(), config);

    // Nothing is sent before a peer has latched.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sent.lock().is_empty(), "no peer, no video");
    assert!(VideoSenderStats::get(&handle.stats().frames_skipped_no_peer) > 0);

    // A headset sends its first input packet: video starts, to <its IP>:48440.
    link.lock()
        .input_packet(peer("192.0.2.10:50000"), Instant::now());
    assert!(until_async(Duration::from_secs(5), || !sent.lock().is_empty()).await);
    assert!(sent
        .lock()
        .iter()
        .all(|(_, dest, _)| *dest == peer("192.0.2.10:48440")));
    assert_eq!(
        key_requests.load(Ordering::Relaxed),
        1,
        "IDR for the new decoder"
    );

    // The input source re-latches to another address: video follows, with a fresh IDR.
    let mark = sent.lock().len();
    link.lock()
        .input_packet(peer("198.51.100.7:51000"), Instant::now());
    assert!(
        until_async(Duration::from_secs(5), || {
            sent.lock()[mark..]
                .iter()
                .any(|(_, dest, _)| *dest == peer("198.51.100.7:48440"))
        })
        .await
    );
    assert_eq!(
        key_requests.load(Ordering::Relaxed),
        2,
        "IDR after the retarget"
    );
    // Keep the new peer alive while checking the old one gets nothing further.
    link.lock()
        .input_packet(peer("198.51.100.7:51000"), Instant::now());
    let settled = sent.lock().len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    link.lock()
        .input_packet(peer("198.51.100.7:51000"), Instant::now());
    assert!(sent.lock()[settled..]
        .iter()
        .all(|(_, dest, _)| *dest == peer("198.51.100.7:48440")));

    // The link goes silent: within the liveness window (300 ms) plus one frame, sending stops.
    tokio::time::sleep(Duration::from_millis(450)).await;
    let stopped_at = sent.lock().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sent.lock().len(),
        stopped_at,
        "silent link: nothing is sent"
    );

    // And it resumes, with an IDR, when the headset comes back.
    link.lock()
        .input_packet(peer("198.51.100.7:51000"), Instant::now());
    assert!(until_async(Duration::from_secs(5), || sent.lock().len() > stopped_at).await);
    assert_eq!(key_requests.load(Ordering::Relaxed), 3, "IDR on resume");

    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_keyframe_requests_force_an_idr_and_are_rate_limited() {
    let _serial = SERIAL.lock().await;
    let link = tracker_with_liveness(60_000);
    let sink = RecordingSink::default();
    let source = TestSource::new(100_000, Duration::from_millis(10), 64);
    let encoder = ScriptedEncoder::new(vec![2400], 100);
    let key_requests = encoder.key_requests.clone();
    let keyframes = encoder.keyframes.clone();
    let flags = KeyframeRequests::new();
    let config = VideoSenderConfig {
        pacing_fraction: 0.0,
        keyframe_requests: Some(flags.clone()),
        ..config_to(48441)
    };
    let handle = spawn_video_sender(source, encoder, sink, link.clone(), config);
    let stats = handle.stats().clone();

    // The peer latches and a few frames flow; the only IDR so far is the new decoder's.
    link.lock()
        .input_packet(peer("192.0.2.10:50000"), Instant::now());
    assert!(
        until_async(Duration::from_secs(5), || {
            VideoSenderStats::get(&stats.frames_sent) >= 3
        })
        .await
    );
    assert_eq!(key_requests.load(Ordering::Relaxed), 1);
    assert_eq!(VideoSenderStats::get(&stats.keyframes_requested), 1);

    // Past the minimum interval since that IDR, a client request is honoured and consumed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    flags.request(0);
    assert!(
        until_async(Duration::from_secs(5), || {
            key_requests.load(Ordering::Relaxed) == 2
        })
        .await
    );
    assert!(!flags.is_requested(0), "the sender clears the flag");
    assert_eq!(VideoSenderStats::get(&stats.keyframes_requested), 2);
    assert!(
        until_async(Duration::from_secs(5), || keyframes.lock().len() == 2).await,
        "the encoder emitted the IDR"
    );

    // Asking again straight away is consumed but does not force another IDR.
    flags.request(0);
    assert!(until_async(Duration::from_secs(5), || !flags.is_requested(0)).await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        key_requests.load(Ordering::Relaxed),
        2,
        "inside min_keyframe_interval: rate limited"
    );
    assert_eq!(keyframes.lock().len(), 2);

    // Once the interval has passed, the next request goes through again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    flags.request(0);
    assert!(
        until_async(Duration::from_secs(5), || {
            key_requests.load(Ordering::Relaxed) == 3
        })
        .await
    );
    assert!(!flags.is_requested(0));
    assert_eq!(VideoSenderStats::get(&stats.keyframes_requested), 3);

    handle.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Several streams to one headset
// ---------------------------------------------------------------------------------------------

/// What a headset with several screens does on its one video port: datagrams are routed by
/// `PacketHeader::stream_id` to one client receive path (jitter buffer, FEC, reassembler) per
/// stream, because each stream has its own sequence numbers and frame ids.
struct DemuxReceiver {
    addr: SocketAddr,
    frames: Arc<Mutex<HashMap<u16, Vec<AssembledFrame>>>>,
    /// `(source datagrams, parity datagrams)` seen per stream id.
    datagrams: Arc<Mutex<HashMap<u16, (u64, u64)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<ThreadHandle<()>>,
}

impl DemuxReceiver {
    fn spawn() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let frames = Arc::new(Mutex::new(HashMap::<u16, Vec<AssembledFrame>>::new()));
        let datagrams = Arc::new(Mutex::new(HashMap::<u16, (u64, u64)>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, d, s) = (frames.clone(), datagrams.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let config = ClientConfig::default();
            let mut receivers: HashMap<u16, ClientReceiver> = HashMap::new();
            let mut buf = vec![0u8; 65536];
            let mut quiet_since: Option<Instant> = None;
            loop {
                if let Ok((n, _)) = socket.recv_from(&mut buf) {
                    quiet_since = None;
                    if let Ok(packet) = Packet::from_bytes(&buf[..n]) {
                        let stream_id = packet.header.stream_id;
                        {
                            let mut counts = d.lock();
                            let entry = counts.entry(stream_id).or_default();
                            match packet.header.packet_type {
                                PacketType::FecParity => entry.1 += 1,
                                _ => entry.0 += 1,
                            }
                        }
                        receivers
                            .entry(stream_id)
                            .or_insert_with(|| ClientReceiver::new(&config))
                            .ingest_packet(packet, Instant::now());
                    }
                }
                let now = Instant::now();
                for (stream_id, receiver) in receivers.iter_mut() {
                    let ready = receiver.pop_ready_frames(now).unwrap();
                    if !ready.is_empty() {
                        quiet_since = None;
                        f.lock().entry(*stream_id).or_default().extend(ready);
                    }
                }
                if s.load(Ordering::Relaxed) {
                    let since = *quiet_since.get_or_insert(now);
                    if now.duration_since(since) > Duration::from_millis(150) {
                        break;
                    }
                }
            }
        });
        Self {
            addr,
            frames,
            datagrams,
            stop,
            thread: Some(thread),
        }
    }

    /// Stops after draining; returns the frames delivered per stream id, in arrival order.
    fn finish(mut self) -> HashMap<u16, Vec<AssembledFrame>> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        std::mem::take(&mut *self.frames.lock())
    }
}

/// What a test can read from a stream's [`ScriptedEncoder`] after the encoder moved into a sender.
struct StreamProbe {
    key_requests: Arc<AtomicU64>,
    keyframes: Arc<Mutex<Vec<u64>>>,
    produced: Arc<Mutex<HashMap<u64, Bytes>>>,
}

/// One stream, wired like the daemon wires each of its streams: own source, encoder, packetizer
/// and socket; shared peer and keyframe flags.
fn spawn_stream(
    stream_id: u16,
    frame_bytes: usize,
    port: u16,
    peers: ManualPeer,
    requests: &Arc<KeyframeRequests>,
) -> (VideoSenderHandle, StreamProbe) {
    let encoder = ScriptedEncoder::new(vec![frame_bytes], 100);
    let probe = StreamProbe {
        key_requests: encoder.key_requests.clone(),
        keyframes: encoder.keyframes.clone(),
        produced: encoder.produced.clone(),
    };
    let handle = spawn_video_sender(
        TestSource::new(100_000, Duration::from_millis(10), 64),
        encoder,
        udp_sink(),
        peers,
        VideoSenderConfig {
            video_port: port,
            stream_id,
            pacing_fraction: 0.0,
            min_keyframe_interval: Duration::from_millis(50),
            keyframe_requests: Some(requests.clone()),
            ..VideoSenderConfig::default()
        },
    );
    (handle, probe)
}

/// The client sends `events` on the input channel and the host input receiver takes the batch,
/// which sets the keyframe flags exactly as in the daemon.
async fn client_asks<I: linux_quest_input::InputInjector>(
    client: &mut linux_quest_transport::InputChannelSender,
    input_service: &mut crate::input_receiver::HostInputReceiver<I>,
    events: Vec<linux_quest_protocol::InputEvent>,
) {
    client.send_events(events, 100_000).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), input_service.step())
        .await
        .expect("the input batch arrives")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_streams_share_the_headset_port_with_independent_frame_ids_and_keyframes() {
    use crate::input_receiver::HostInputReceiver;
    use linux_quest_input::MockInputInjector;
    use linux_quest_protocol::InputEvent;
    use linux_quest_transport::{InputChannelReceiver, InputChannelSender};

    let _serial = SERIAL.lock().await;
    let receiver = DemuxReceiver::spawn();
    let port = receiver.addr.port();
    let peers = ManualPeer::at("127.0.0.1");
    let requests = KeyframeRequests::new();

    // The client's input channel into the host, as in the daemon: its keyframe requests reach
    // the senders through the shared flags.
    let client_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let host_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (client_addr, host_addr) = (
        client_socket.local_addr().unwrap(),
        host_socket.local_addr().unwrap(),
    );
    let mut client = InputChannelSender::new(client_socket, Some(host_addr), 4);
    let mut input_service = HostInputReceiver::new(
        InputChannelReceiver::new(host_socket, Some(client_addr), true),
        Arc::new(std::sync::Mutex::new(MockInputInjector::new())),
    )
    .with_keyframe_requests(requests.clone());
    // Stream 0 runs alone for a while, so its frame ids are well ahead when stream 1 starts.
    let (stream0, probe0) = spawn_stream(0, 2400, port, peers.clone(), &requests);
    let stats0 = stream0.stats().clone();
    assert!(
        until_async(Duration::from_secs(5), || {
            VideoSenderStats::get(&stats0.frames_sent) >= 8
        })
        .await
    );
    let (stream1, probe1) = spawn_stream(1, 3600, port, peers, &requests);
    let stats1 = stream1.stats().clone();
    assert!(
        until_async(Duration::from_secs(5), || {
            VideoSenderStats::get(&stats1.frames_sent) >= 8
                && VideoSenderStats::get(&stats0.frames_sent) >= 16
        })
        .await
    );

    // Each stream's decoder start asked for exactly one IDR, its own.
    assert_eq!(probe0.key_requests.load(Ordering::Relaxed), 1);
    assert_eq!(probe1.key_requests.load(Ordering::Relaxed), 1);
    assert_eq!(probe0.keyframes.lock().clone(), vec![1]);
    assert_eq!(probe1.keyframes.lock().clone(), vec![1]);

    // The client lost stream 1 only (wire tag 10): only stream 1 emits another keyframe.
    tokio::time::sleep(Duration::from_millis(100)).await;
    client_asks(
        &mut client,
        &mut input_service,
        vec![InputEvent::KeyframeRequestFor { stream_id: 1 }],
    )
    .await;
    assert!(
        until_async(Duration::from_secs(5), || probe1.keyframes.lock().len()
            == 2)
        .await,
        "stream 1 emitted the IDR"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(probe1.key_requests.load(Ordering::Relaxed), 2);
    assert_eq!(
        probe0.key_requests.load(Ordering::Relaxed),
        1,
        "stream 0 was not asked"
    );
    assert_eq!(probe0.keyframes.lock().len(), 1, "stream 0 emitted no IDR");
    assert!(!requests.is_requested(0) && !requests.is_requested(1));

    // The payload-less request (wire tag 8) still means stream 0, and only stream 0.
    client_asks(
        &mut client,
        &mut input_service,
        vec![InputEvent::KeyframeRequest],
    )
    .await;
    assert!(
        until_async(Duration::from_secs(5), || probe0.keyframes.lock().len()
            == 2)
        .await,
        "stream 0 emitted the IDR"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(probe0.key_requests.load(Ordering::Relaxed), 2);
    assert_eq!(probe1.key_requests.load(Ordering::Relaxed), 2);
    assert_eq!(probe1.keyframes.lock().len(), 2);

    // A request for a stream nobody sends is harmless and wakes nobody.
    client_asks(
        &mut client,
        &mut input_service,
        vec![InputEvent::KeyframeRequestFor { stream_id: 2 }],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(probe0.key_requests.load(Ordering::Relaxed), 2);
    assert_eq!(probe1.key_requests.load(Ordering::Relaxed), 2);

    stream0.stop().await;
    stream1.stop().await;
    let datagrams = receiver.datagrams.lock().clone();
    let frames = receiver.finish();

    let mut ids_seen: Vec<u16> = frames.keys().copied().collect();
    ids_seen.sort_unstable();
    assert_eq!(
        ids_seen,
        vec![0, 1],
        "packets carry stream ids 0 and 1 only"
    );
    for (stream_id, probe, frame_bytes) in [(0u16, &probe0, 2400usize), (1, &probe1, 3600)] {
        let delivered = &frames[&stream_id];
        assert!(delivered.len() >= 8, "stream {stream_id} delivered frames");

        // Frame ids belong to the stream: each counts 1, 2, 3, ... on its own, although
        // stream 1 started after stream 0 had already sent 8 frames.
        let ids: Vec<u64> = delivered.iter().map(|f| f.meta.frame_id).collect();
        let expected: Vec<u64> = (1..=ids.len() as u64).collect();
        assert_eq!(ids, expected, "stream {stream_id}: independent frame ids");

        // Bit-exact against what this stream's own encoder produced, so no datagram crossed
        // over from the other stream.
        let produced = probe.produced.lock();
        for frame in delivered {
            assert_eq!(frame.bitstream.len(), frame_bytes);
            assert_eq!(frame.bitstream, produced[&frame.meta.frame_id]);
        }

        // The IDRs on the wire are exactly the ones this stream's encoder was asked for.
        let key_ids: Vec<u64> = delivered
            .iter()
            .filter(|f| f.meta.is_keyframe)
            .map(|f| f.meta.frame_id)
            .collect();
        assert_eq!(key_ids, *probe.keyframes.lock(), "stream {stream_id} IDRs");
        assert_eq!(key_ids.len(), 2);

        // Source and FEC parity datagrams are both stamped with the stream id.
        let (sources, parity) = datagrams[&stream_id];
        assert!(
            sources > 0 && parity > 0,
            "stream {stream_id}: {sources}+{parity}"
        );
    }
    assert!(
        frames[&0].len() > frames[&1].len(),
        "stream 0 started earlier and ran longer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usb_tunnel_peer_uses_tunnel_destination() {
    let _serial = SERIAL.lock().await;
    let link = tracker_with_liveness(5_000);
    link.lock().adb_event(
        &linux_quest_transport::AdbBridgeEvent::ReversePortEstablished {
            serial: "QUEST".into(),
            port: 48444,
        },
    );
    link.lock()
        .input_packet(peer("127.0.0.1:50000"), Instant::now());

    let sink = RecordingSink::default();
    let sent = sink.sent.clone();
    let handle = spawn_video_sender(
        TestSource::new(100_000, Duration::from_millis(5), 64),
        ScriptedEncoder::new(vec![1200], 200),
        sink,
        link,
        config_to(48440),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let sent = sent.lock();
        assert!(!sent.is_empty(), "USB peer should receive video");
        assert!(sent
            .iter()
            .all(|(_, destination, _)| destination.ip().is_unspecified()));
    }
    handle.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Latency rules
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn pending_slot_holds_one_frame_and_the_newest_wins() {
    let stats = Arc::new(VideoSenderStats::default());
    let slot = FrameSlot::new(stats.clone());
    let frame = |pts| raw_frame(8, 8, Bytes::from_static(&[0; 4]), pts);

    assert!(!slot.put(frame(1)));
    assert!(slot.put(frame(2)), "second frame replaces the first");
    assert!(slot.put(frame(3)));
    assert!(slot.has_pending());
    assert_eq!(slot.take().await.pts_us, 3, "only the newest survives");
    assert!(!slot.has_pending());
    assert_eq!(VideoSenderStats::get(&stats.frames_dropped_pending), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_encoder_never_builds_a_backlog() {
    let _serial = SERIAL.lock().await;
    // 10 frames arrive at once; each encode takes 40 ms.
    let source = TestSource::new(10, Duration::ZERO, 64);
    let mut encoder = ScriptedEncoder::new(vec![2400], 60);
    encoder.delay = Duration::from_millis(40);
    let encoded = encoder.encoded.clone();
    let pts_seen = encoder.pts_seen.clone();
    let sink = RecordingSink::default();

    let handle = spawn_video_sender(
        source,
        encoder,
        sink.clone(),
        ManualPeer::at("192.0.2.1"),
        VideoSenderConfig {
            pacing_fraction: 0.0,
            ..config_to(48440)
        },
    );
    let stats = handle.stats().clone();
    assert!(
        until_async(Duration::from_secs(5), || {
            VideoSenderStats::get(&stats.frames_captured) == 10
                && pts_seen.lock().last() == Some(&(10 * 5_000))
                && VideoSenderStats::get(&stats.frames_sent) == pts_seen.lock().len() as u64
        })
        .await
    );
    handle.stop().await;

    let encoded = encoded.load(Ordering::Relaxed);
    let dropped = VideoSenderStats::get(&stats.frames_dropped_pending);
    assert!(
        encoded <= 3,
        "stale frames were not encoded: {encoded} of 10"
    );
    assert_eq!(
        encoded + dropped,
        10,
        "every frame is either encoded or dropped before encode"
    );
    // The newest captured frame is always the one that gets through, and none that were
    // skipped was encoded late.
    let seen = pts_seen.lock().clone();
    assert_eq!(seen.last(), Some(&(10 * 5_000)));
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "frames stay in order: {seen:?}"
    );
    assert_eq!(VideoSenderStats::get(&stats.frames_abandoned), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encoded_frame_that_cannot_finish_is_abandoned_and_an_idr_requested() {
    let _serial = SERIAL.lock().await;
    // Frames of ~240 kB at a 2 Mbps pacing ceiling would take seconds; 100 fps means a newer
    // frame is waiting after 10 ms.
    let source = TestSource::new(4, Duration::from_millis(2), 64);
    let encoder = ScriptedEncoder::new(vec![200_000], 100);
    let key_requests = encoder.key_requests.clone();
    let sink = RecordingSink::default();
    let sent = sink.sent.clone();

    let handle = spawn_video_sender(
        source,
        encoder,
        sink,
        ManualPeer::at("192.0.2.1"),
        VideoSenderConfig {
            max_pacing_mbps: 2,
            ..config_to(48440)
        },
    );
    let stats = handle.stats().clone();
    assert!(
        until_async(Duration::from_secs(10), || VideoSenderStats::get(
            &stats.frames_abandoned
        ) >= 1)
        .await
    );
    handle.stop().await;

    assert!(
        key_requests.load(Ordering::Relaxed) >= 2,
        "start-of-stream IDR plus one after the abandoned frame"
    );
    // The abandoned frame was cut well short of its ~200 datagrams.
    assert!(sent.lock().len() < 150);
}

#[tokio::test]
async fn pacing_spreads_a_frames_datagrams_over_its_budget() {
    let _serial = SERIAL.lock().await;
    let frame_id_size = 160 * 1200; // 160 chunks -> 8 FEC blocks, ~225 kB on the wire
    let raw = raw_frame(640, 360, Bytes::new(), 0);
    let stats = Arc::new(VideoSenderStats::default());
    let dest = peer("192.0.2.1:48440");

    let run = |fraction: f64| {
        let stats = stats.clone();
        let raw = &raw;
        async move {
            let sink = RecordingSink::default();
            let sent = sink.sent.clone();
            let mut pipeline = FramePipeline::new(
                ScriptedEncoder::new(vec![frame_id_size], 60),
                sink,
                VideoSenderConfig {
                    pacing_fraction: fraction,
                    ..VideoSenderConfig::default()
                },
                stats,
            );
            let report = pipeline
                .send_frame(raw, dest, &|| false)
                .await
                .unwrap()
                .unwrap();
            let times: Vec<Instant> = sent.lock().iter().map(|(t, _, _)| *t).collect();
            (report, times)
        }
    };

    let (paced, paced_times) = run(0.5).await;
    let (unpaced, unpaced_times) = run(0.0).await;

    let budget = Duration::from_secs_f64(0.5 / 60.0);
    assert_eq!(paced.pacing_budget, budget);
    assert_eq!(unpaced.pacing_budget, Duration::ZERO);
    assert_eq!(paced.datagrams, unpaced.datagrams);
    assert_eq!(
        paced.datagrams,
        160 + 8 * 5,
        "160 sources + 8 blocks x 5 parity"
    );

    let span = |t: &[Instant]| *t.last().unwrap() - *t.first().unwrap();
    let paced_span = span(&paced_times);
    assert!(
        paced_span >= budget.mul_f64(0.6),
        "datagrams were spread out: {paced_span:?} for a {budget:?} budget"
    );
    // Wall-clock scheduling can overshoot the ideal pacing budget when this timing-sensitive
    // suite runs beside other crates' tests. Keep a generous ceiling to catch unbounded waits.
    assert!(
        paced_span <= budget + Duration::from_millis(100),
        "...but not by an unbounded amount: {paced_span:?}"
    );
    // The first half of the datagrams must not leave in one burst.
    let half = paced_times.len() / 2;
    assert!(
        paced_times[half] - paced_times[0] >= budget.mul_f64(0.25),
        "no front-loaded burst"
    );
    // The same frame without pacing sends the same datagrams (compared above) and never waits.
    assert_eq!(unpaced_times.len(), paced_times.len());
}

#[tokio::test]
async fn a_single_fec_block_is_never_paced_over_more_than_a_few_milliseconds() {
    let _serial = SERIAL.lock().await;
    // One block (20 sources + 5 parity) at 60 fps would get an 8.3 ms budget, but the client
    // releases packets ~4 ms after arrival and learns the block's size from its parity packets,
    // so the block must leave within `MAX_BLOCK_SPAN` (2 ms).
    let raw = raw_frame(640, 360, Bytes::new(), 0);
    let sink = RecordingSink::default();
    let sent = sink.sent.clone();
    let mut pipeline = FramePipeline::new(
        ScriptedEncoder::new(vec![20 * 1200], 60),
        sink,
        VideoSenderConfig::default(),
        Arc::new(VideoSenderStats::default()),
    );
    let report = pipeline
        .send_frame(&raw, peer("192.0.2.1:48440"), &|| false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.datagrams, 25);
    let times: Vec<Instant> = sent.lock().iter().map(|(t, _, _)| *t).collect();
    let span = *times.last().unwrap() - *times.first().unwrap();
    // The budget would be 8.3 ms; the block cap keeps it near 2 ms (pacing resolution is ~1 ms).
    assert!(report.pacing_budget > Duration::from_millis(8));
    assert!(
        span < Duration::from_millis(6),
        "block span capped near 2 ms: {span:?}"
    );
}

#[tokio::test]
async fn socket_backpressure_drops_datagrams_instead_of_blocking_and_requests_an_idr() {
    let stats = Arc::new(VideoSenderStats::default());
    let encoder = ScriptedEncoder::new(vec![24_000], 60);
    let key_requests = encoder.key_requests.clone();
    let mut pipeline = FramePipeline::new(
        encoder,
        FullSink,
        VideoSenderConfig {
            pacing_fraction: 0.0,
            ..VideoSenderConfig::default()
        },
        stats.clone(),
    );
    let raw = raw_frame(64, 64, Bytes::new(), 0);
    let dest = peer("192.0.2.1:48440");
    let start = Instant::now();
    let report = pipeline
        .send_frame(&raw, dest, &|| false)
        .await
        .unwrap()
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(500), "never blocks");
    assert_eq!(report.datagrams, 0);
    assert_eq!(report.send_failures, 25);
    assert_eq!(VideoSenderStats::get(&stats.send_would_block), 25);
    assert_eq!(key_requests.load(Ordering::Relaxed), 1);

    // A second failing frame right away does not ask again (inside the minimum interval).
    pipeline
        .send_frame(&raw, dest, &|| false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key_requests.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_capture_errors_do_not_stop_the_stream() {
    let _serial = SERIAL.lock().await;
    let mut source = TestSource::new(5, Duration::from_millis(5), 64);
    source.transient_errors = 3;
    let sink = RecordingSink::default();
    let handle = spawn_video_sender(
        source,
        ScriptedEncoder::new(vec![1200], 200),
        sink.clone(),
        ManualPeer::at("192.0.2.1"),
        config_to(48440),
    );
    let stats = handle.stats().clone();
    assert!(
        until_async(Duration::from_secs(5), || VideoSenderStats::get(
            &stats.frames_sent
        ) >= 5)
        .await
    );
    handle.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Benchmark (ignored by default)
// ---------------------------------------------------------------------------------------------

/// Sender-side software latency on loopback. Run with:
/// `cargo test -p linux-quest-host --release video_sender::tests::bench -- --ignored --nocapture`
///
/// It measures from a frame entering [`FramePipeline::send_frame`] to its last datagram being
/// accepted by the kernel: bitstream chunking, CRC, Reed-Solomon parity, pacing and the send
/// syscalls, with a stub encoder. It does NOT include screen capture, GPU encode, the Wi-Fi
/// hop, or client-side receive and decode.
#[tokio::test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
async fn bench_sender_latency() {
    struct Workload {
        name: &'static str,
        fps: u32,
        bytes: usize,
        dims: (u32, u32),
    }
    let workloads = [
        Workload {
            name: "1080p60 P-frame (30 Mbps)",
            fps: 60,
            bytes: 62_500,
            dims: (1920, 1080),
        },
        Workload {
            name: "1080p60 IDR     (250 kB) ",
            fps: 60,
            bytes: 250_000,
            dims: (1920, 1080),
        },
        Workload {
            name: "4K60    P-frame (100 Mbps)",
            fps: 60,
            bytes: 208_000,
            dims: (3840, 2160),
        },
        Workload {
            name: "4K60    IDR     (800 kB) ",
            fps: 60,
            bytes: 800_000,
            dims: (3840, 2160),
        },
    ];

    // Drain the loopback socket so the kernel buffer never fills.
    let drain = UdpSocket::bind("127.0.0.1:0").unwrap();
    drain
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let dest = drain.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let drained = Arc::new(AtomicU64::new(0));
    let (s, d) = (stop.clone(), drained.clone());
    let drain_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; 2048];
        while !s.load(Ordering::Relaxed) {
            if drain.recv_from(&mut buf).is_ok() {
                d.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    println!();
    println!("sender-side latency, frame-in to last datagram out, UDP loopback, release build");
    println!("(stub encoder: excludes capture, GPU encode, Wi-Fi, client receive and decode)");
    println!(
        "{:<26} {:<10} {:>9} {:>9} {:>9} {:>10} {:>9}",
        "workload", "pacing", "p50 ms", "p99 ms", "max ms", "dgrams/fr", "kB/frame"
    );

    let raw = raw_frame(64, 64, Bytes::new(), 0);
    for w in &workloads {
        // Cost of the stub itself, so it can be read off the numbers.
        let mut stub = ScriptedEncoder::new(vec![w.bytes], w.fps);
        let mut stub_costs = Vec::new();
        for _ in 0..200 {
            let t = Instant::now();
            let _ = stub.encode(&raw).await.unwrap();
            stub_costs.push(t.elapsed());
        }
        stub_costs.sort();
        let stub_p50 = stub_costs[stub_costs.len() / 2];

        for (label, fraction, frames) in [("unpaced", 0.0, 600usize), ("0.5 frame", 0.5, 240)] {
            let stats = Arc::new(VideoSenderStats::default());
            let mut pipeline = FramePipeline::new(
                ScriptedEncoder::new(vec![w.bytes], w.fps),
                udp_sink(),
                VideoSenderConfig {
                    pacing_fraction: fraction,
                    ..VideoSenderConfig::default()
                },
                stats.clone(),
            );
            let frame = raw_frame(w.dims.0, w.dims.1, Bytes::new(), 0);
            let mut latencies = Vec::with_capacity(frames);
            let mut datagrams = 0usize;
            let mut bytes = 0usize;
            for _ in 0..frames {
                let report = pipeline
                    .send_frame(&frame, dest, &|| false)
                    .await
                    .unwrap()
                    .unwrap();
                latencies.push(report.latency);
                datagrams += report.datagrams;
                bytes += report.bytes;
            }
            latencies.sort();
            let pct = |p: f64| {
                latencies[((latencies.len() as f64 * p) as usize).min(latencies.len() - 1)]
            };
            println!(
                "{:<26} {:<10} {:>9.2} {:>9.2} {:>9.2} {:>10.1} {:>9.1}",
                w.name,
                label,
                pct(0.50).as_secs_f64() * 1e3,
                pct(0.99).as_secs_f64() * 1e3,
                latencies.last().unwrap().as_secs_f64() * 1e3,
                datagrams as f64 / frames as f64,
                bytes as f64 / frames as f64 / 1e3,
            );
            let refused = VideoSenderStats::get(&stats.send_would_block)
                + VideoSenderStats::get(&stats.send_errors);
            if refused > 0 {
                println!("{:<26} ({refused} datagrams refused by the socket)", "");
            }
        }
        println!("{:<26} (stub encoder p50 {:?})", "", stub_p50);
    }

    stop.store(true, Ordering::Relaxed);
    drain_thread.join().unwrap();
    println!(
        "datagrams drained by the loopback receiver: {}",
        drained.load(Ordering::Relaxed)
    );
}

// ---------------------------------------------------------------------------------------------
// Adaptive bitrate
// ---------------------------------------------------------------------------------------------

/// Records the bitrates the sender asks the encoder to switch to.
struct BitrateProbe<E: VideoEncoder> {
    inner: E,
    applied: Arc<Mutex<Vec<u32>>>,
    supports_retune: bool,
}

#[async_trait]
impl<E: VideoEncoder> VideoEncoder for BitrateProbe<E> {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.inner.init(config).await
    }
    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.inner.encode(frame).await
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }
    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.inner.invalidate_reference_picture(pts_us);
    }
    fn set_bitrate(&mut self, kbps: u32) -> bool {
        self.applied.lock().push(kbps);
        self.supports_retune
    }
}

#[tokio::test]
async fn the_sender_applies_live_rate_changes_to_the_encoder_and_fec() {
    let _serial = SERIAL.lock().await;
    let rc = crate::congestion::RateCoordinator::new(30_000, true);
    let rate = rc.register_stream(0);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let sink = RecordingSink::default();
    let sent = sink.sent.clone();
    let mut pipeline = FramePipeline::new(
        BitrateProbe {
            inner: ScriptedEncoder::new(vec![20 * 1200], 60),
            applied: applied.clone(),
            supports_retune: true,
        },
        sink,
        VideoSenderConfig {
            pacing_fraction: 0.0,
            rate: Some(rate.clone()),
            ..VideoSenderConfig::default()
        },
        Arc::new(VideoSenderStats::default()),
    );
    let raw = raw_frame(640, 360, Bytes::new(), 0);
    let dest = peer("192.0.2.1:48440");

    let first = pipeline
        .send_frame(&raw, dest, &|| false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        *applied.lock(),
        vec![30_000],
        "initial target is applied once"
    );
    assert_eq!(first.parity_datagrams, 5, "base FEC ratio of 25 %");
    pipeline.send_frame(&raw, dest, &|| false).await.unwrap();
    assert_eq!(
        applied.lock().len(),
        1,
        "an unchanged target is not re-applied"
    );

    // The client reports heavy loss: lower bitrate, more parity.
    rc.on_feedback(
        &linux_quest_protocol::NetworkFeedback {
            loss_permille: 150,
            delay_trend_us_per_s: 0,
            jitter_us: 0,
            received_kbps: 12_000,
            focused_stream: linux_quest_protocol::NO_FOCUSED_STREAM,
        },
        Instant::now(),
    );
    tokio::time::sleep(MIN_BITRATE_APPLY_INTERVAL + Duration::from_millis(20)).await;
    let report = pipeline
        .send_frame(&raw, dest, &|| false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*applied.lock(), vec![30_000, 8_400]);
    assert!(
        report.parity_datagrams > 5,
        "loss raised parity to {}",
        report.parity_datagrams
    );
    assert_eq!(sent.lock().len(), 25 * 2 + report.datagrams);
}

#[tokio::test]
async fn an_encoder_that_cannot_retune_is_not_asked_again_every_frame() {
    let _serial = SERIAL.lock().await;
    let rc = crate::congestion::RateCoordinator::new(10_000, true);
    let rate = rc.register_stream(0);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = FramePipeline::new(
        BitrateProbe {
            inner: ScriptedEncoder::new(vec![1200], 60),
            applied: applied.clone(),
            supports_retune: false,
        },
        RecordingSink::default(),
        VideoSenderConfig {
            pacing_fraction: 0.0,
            rate: Some(rate),
            ..VideoSenderConfig::default()
        },
        Arc::new(VideoSenderStats::default()),
    );
    let raw = raw_frame(64, 64, Bytes::new(), 0);
    let dest = peer("192.0.2.1:48440");
    for _ in 0..5 {
        pipeline.send_frame(&raw, dest, &|| false).await.unwrap();
    }
    assert_eq!(applied.lock().len(), 1);
}

#[test]
fn achieved_rate_is_measured_over_windows_without_counting_parity() {
    let mut w = RateWindow::new();
    let t0 = Instant::now();
    assert_eq!(w.record(62_500, t0), None);
    assert_eq!(w.record(62_500, t0 + Duration::from_millis(250)), None);
    // 187 500 B in 0.5 s = 3 Mbit/s.
    assert_eq!(
        w.record(62_500, t0 + Duration::from_millis(500)),
        Some(3_000)
    );
    assert_eq!(w.record(0, t0 + Duration::from_millis(600)), None);
}

#[tokio::test]
async fn the_sender_publishes_its_achieved_video_rate() {
    let _serial = SERIAL.lock().await;
    let rc = crate::congestion::RateCoordinator::new(30_000, true);
    let rate = rc.register_stream(0);
    let mut pipeline = FramePipeline::new(
        ScriptedEncoder::new(vec![20 * 1200], 60),
        RecordingSink::default(),
        VideoSenderConfig {
            pacing_fraction: 0.0,
            rate: Some(rate.clone()),
            ..VideoSenderConfig::default()
        },
        Arc::new(VideoSenderStats::default()),
    );
    let raw = raw_frame(64, 64, Bytes::new(), 0);
    let dest = peer("192.0.2.1:48440");
    let end = Instant::now() + RATE_WINDOW + Duration::from_millis(50);
    while Instant::now() < end {
        pipeline.send_frame(&raw, dest, &|| false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    pipeline.send_frame(&raw, dest, &|| false).await.unwrap();
    // ~60 fps x 24 kB payload is ~11.5 Mbit/s; allow wide timer slack.
    let kbps = rate.actual_kbps();
    assert!((4_000..=16_000).contains(&kbps), "measured {kbps} kbit/s");
}

/// Records every `init` an encoder gets.
struct InitProbe<E: VideoEncoder> {
    inner: E,
    inits: Arc<Mutex<Vec<(u32, u32, u32)>>>,
    keyframes: Arc<AtomicU64>,
}

#[async_trait]
impl<E: VideoEncoder> VideoEncoder for InitProbe<E> {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.inits
            .lock()
            .push((config.width, config.height, config.bitrate_kbps));
        self.inner.init(config).await
    }
    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.inner.encode(frame).await
    }
    fn request_keyframe(&mut self) {
        self.keyframes.fetch_add(1, Ordering::Relaxed);
        self.inner.request_keyframe();
    }
    fn invalidate_reference_picture(&mut self, pts_us: u64) {
        self.inner.invalidate_reference_picture(pts_us);
    }
    fn set_bitrate(&mut self, kbps: u32) -> bool {
        self.inner.set_bitrate(kbps)
    }
}

#[tokio::test]
async fn a_new_capture_size_recreates_the_encoder_at_that_size() {
    let inits = Arc::new(Mutex::new(Vec::new()));
    let keyframes = Arc::new(AtomicU64::new(0));
    let base = EncoderConfig {
        width: 3840,
        height: 2160,
        bitrate_kbps: 30_000,
        ..EncoderConfig::default()
    };
    let mut pipeline = FramePipeline::new(
        InitProbe {
            inner: ScriptedEncoder::new(vec![1200; 8], 60),
            inits: inits.clone(),
            keyframes: keyframes.clone(),
        },
        RecordingSink::default(),
        VideoSenderConfig {
            pacing_fraction: 0.0,
            encoder_config: Some(base),
            ..VideoSenderConfig::default()
        },
        Arc::new(VideoSenderStats::default()),
    );
    let dest = peer("192.0.2.1:48440");

    // The size it was set up for: nothing to do.
    let same = raw_frame(3840, 2160, Bytes::new(), 0);
    pipeline.send_frame(&same, dest, &|| false).await.unwrap();
    assert!(inits.lock().is_empty());

    // The user picked a 1080p monitor (an odd size is rounded down to even).
    let smaller = raw_frame(1921, 1080, Bytes::new(), 16_666);
    pipeline
        .send_frame(&smaller, dest, &|| false)
        .await
        .unwrap();
    pipeline
        .send_frame(&smaller, dest, &|| false)
        .await
        .unwrap();
    assert_eq!(
        *inits.lock(),
        vec![(1920, 1080, 30_000)],
        "once, at the new size"
    );
    assert_eq!(
        keyframes.load(Ordering::Relaxed),
        1,
        "the new encoder starts with an IDR"
    );

    // Without a known encoder setup, frames go through untouched.
    let mut plain = FramePipeline::new(
        InitProbe {
            inner: ScriptedEncoder::new(vec![1200; 2], 60),
            inits: inits.clone(),
            keyframes,
        },
        RecordingSink::default(),
        VideoSenderConfig {
            pacing_fraction: 0.0,
            ..VideoSenderConfig::default()
        },
        Arc::new(VideoSenderStats::default()),
    );
    plain.send_frame(&smaller, dest, &|| false).await.unwrap();
    assert_eq!(inits.lock().len(), 1);
}
