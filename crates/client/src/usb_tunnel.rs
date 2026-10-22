//! The headset end of the USB tunnel (`adb reverse tcp:48444`): video in, input out.
//!
//! A background thread keeps one negotiated connection to the host (see
//! `linux_quest_transport::tcp_tunnel`) and starts a new one when:
//!
//! - the host closes the connection, or a read or write fails;
//! - the tunnel goes quiet: the host sends a heartbeat every
//!   [`TUNNEL_HEARTBEAT_INTERVAL`](linux_quest_transport::TUNNEL_HEARTBEAT_INTERVAL), so
//!   [`UsbTunnelConfig::idle_timeout`] without a byte means the connection is dead. Hosts from
//!   before heartbeats send nothing on a static screen, so the timeout applies only once the
//!   host has sent a heartbeat on this connection;
//! - the app asks with [`TunnelReconnect::request`]. It does so when it resumes after a pause:
//!   after the headset sleeps, `adb reverse` can leave the old connection open on both ends
//!   while it carries nothing, and an older host never heartbeats.
//!
//! After a reconnect the thread asks the host for a keyframe on every stream it has received:
//! the frames sent into the dead connection are lost, and the host does not otherwise know the
//! decoder needs a fresh start.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use linux_quest_protocol::{InputBatch, Packet, PacketHeader, PacketType};
use linux_quest_transport::{
    encode_frame, negotiate_client_blocking, ControlMessage, TunnelError, TunnelFrame,
    TunnelFrameDecoder,
};

use crate::input_sender::{InputSource, KeyframeRequestFlags};

/// Timing of the tunnel thread.
#[derive(Debug, Clone)]
pub struct UsbTunnelConfig {
    /// The headset end of the `adb reverse` tunnel to the host's control port.
    pub host_addr: SocketAddr,
    pub connect_timeout: Duration,
    /// Pause after a failed connection attempt.
    pub retry_interval: Duration,
    /// Socket read timeout: how often reconnect requests and silence are checked.
    pub poll_interval: Duration,
    /// Silence after which a host that sends heartbeats is taken for gone. Also the write
    /// timeout of the input writer.
    pub idle_timeout: Duration,
    /// How often the input writer polls its source.
    pub input_interval: Duration,
    /// An input batch is sent at least this often, so the host knows the headset is there.
    pub keepalive_interval: Duration,
}

impl UsbTunnelConfig {
    pub fn new(host_addr: SocketAddr) -> Self {
        Self {
            host_addr,
            connect_timeout: Duration::from_secs(1),
            retry_interval: Duration::from_millis(500),
            poll_interval: Duration::from_millis(100),
            idle_timeout: linux_quest_transport::TUNNEL_IDLE_TIMEOUT,
            input_interval: Duration::from_millis(4),
            keepalive_interval: Duration::from_millis(100),
        }
    }
}

/// Asks the tunnel thread to drop its connection and open a new one.
#[derive(Debug, Clone, Default)]
pub struct TunnelReconnect(Arc<AtomicU64>);

impl TunnelReconnect {
    /// Connections opened before this call are replaced. Cheap; call from any thread.
    pub fn request(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    fn generation(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Input forwarded to the host over the tunnel, plus the flags its keyframe requests come from.
pub struct TunnelInput {
    pub source: Arc<Mutex<dyn InputSource>>,
    /// The flags `source` turns into keyframe requests, raised after a reconnect.
    pub keyframes: KeyframeRequestFlags,
}

#[derive(Debug, Default)]
struct Shared {
    running: AtomicBool,
    /// Successful negotiations so far.
    connections: AtomicU64,
    /// Streams (bit per id below 32) that have delivered packets.
    seen_streams: AtomicU32,
}

/// Handle of the tunnel thread. Stops the thread when dropped.
pub struct UsbTunnel {
    shared: Arc<Shared>,
    reconnect: TunnelReconnect,
    thread: Option<JoinHandle<()>>,
}

impl UsbTunnel {
    /// Starts the tunnel thread. `on_packet` gets every packet the host sends, on that thread.
    pub fn spawn(
        config: UsbTunnelConfig,
        input: Option<TunnelInput>,
        on_packet: impl FnMut(Packet) + Send + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.running.store(true, Ordering::SeqCst);
        let reconnect = TunnelReconnect::default();
        let thread = {
            let shared = shared.clone();
            let reconnect = reconnect.clone();
            std::thread::Builder::new()
                .name("lq-usb-tunnel".into())
                .spawn(move || run(config, shared, reconnect, input, on_packet))?
        };
        Ok(Self {
            shared,
            reconnect,
            thread: Some(thread),
        })
    }

    /// A handle that makes the thread reconnect (see [`TunnelReconnect::request`]).
    pub fn reconnect_handle(&self) -> TunnelReconnect {
        self.reconnect.clone()
    }

    /// Connections negotiated so far, the first one included.
    pub fn connections(&self) -> u64 {
        self.shared.connections.load(Ordering::Acquire)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for UsbTunnel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Why a connection ended.
#[derive(Debug)]
enum Ended {
    Closed,
    ReadFailed(std::io::Error),
    WriteFailed,
    Silent(Duration),
    Requested,
    Corrupt(TunnelError),
    Stopped,
}

fn run(
    config: UsbTunnelConfig,
    shared: Arc<Shared>,
    reconnect: TunnelReconnect,
    input: Option<TunnelInput>,
    mut on_packet: impl FnMut(Packet),
) {
    let mut failures = 0u32;
    while shared.running.load(Ordering::SeqCst) {
        // A request made while this connection is being opened applies to it too: it may have
        // been opened through a tunnel that was already dead.
        let generation = reconnect.generation();
        let stream = match connect(&config) {
            Ok(stream) => stream,
            Err(error) => {
                if failures == 0 {
                    tracing::info!(host = %config.host_addr, %error, "USB tunnel: host not reachable; retrying");
                } else {
                    tracing::debug!(host = %config.host_addr, %error, "USB tunnel: connect failed");
                }
                failures = failures.saturating_add(1);
                sleep_while_running(&shared, config.retry_interval);
                continue;
            }
        };
        failures = 0;
        let opened = Instant::now();
        let previous = shared.connections.fetch_add(1, Ordering::AcqRel);
        tracing::info!(host = %config.host_addr, reconnects = previous, "USB tunnel connected");
        if previous > 0 {
            if let Some(input) = &input {
                request_keyframes(
                    &input.keyframes,
                    shared.seen_streams.load(Ordering::Acquire),
                );
            }
        }
        let ended = session(
            &stream,
            &config,
            &shared,
            &reconnect,
            generation,
            input.as_ref(),
            &mut on_packet,
        );
        match &ended {
            Ended::Stopped => {}
            Ended::Closed => tracing::info!("USB tunnel closed by the host; reconnecting"),
            Ended::Requested => tracing::info!("USB tunnel: reconnect requested; reconnecting"),
            Ended::Silent(quiet) => tracing::warn!(
                quiet_ms = quiet.as_millis() as u64,
                "USB tunnel: no data from the host, which sends a heartbeat every second; \
                 the tunnel is dead, reconnecting"
            ),
            Ended::WriteFailed => {
                tracing::warn!("USB tunnel: sending input failed; reconnecting")
            }
            Ended::ReadFailed(error) => {
                tracing::warn!(%error, "USB tunnel: read failed; reconnecting")
            }
            Ended::Corrupt(error) => {
                tracing::warn!(%error, "USB tunnel: stream out of step; reconnecting")
            }
        }
        match ended {
            Ended::Stopped => return,
            Ended::Requested => {}
            // A host that drops every connection at once is not hammered.
            _ if opened.elapsed() < Duration::from_secs(1) => {
                sleep_while_running(&shared, config.retry_interval)
            }
            _ => {}
        }
    }
}

fn connect(config: &UsbTunnelConfig) -> Result<TcpStream, TunnelError> {
    let mut stream = TcpStream::connect_timeout(&config.host_addr, config.connect_timeout)?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(
        linux_quest_transport::tcp_tunnel::TUNNEL_NEGOTIATION_TIMEOUT,
    ))?;
    stream.set_write_timeout(Some(config.idle_timeout))?;
    negotiate_client_blocking(&mut stream)?;
    stream.set_read_timeout(Some(config.poll_interval))?;
    Ok(stream)
}

fn request_keyframes(flags: &KeyframeRequestFlags, seen_streams: u32) {
    // Stream 0 always: a host that has not been heard from yet still starts with it.
    let streams = seen_streams | 1;
    for stream_id in (0..32u16).filter(|id| streams & (1 << id) != 0) {
        flags.raise(stream_id);
    }
    tracing::info!(
        streams = format!("{streams:#b}"),
        "USB tunnel: asking the host for keyframes after reconnecting"
    );
}

fn session(
    stream: &TcpStream,
    config: &UsbTunnelConfig,
    shared: &Shared,
    reconnect: &TunnelReconnect,
    generation: u64,
    input: Option<&TunnelInput>,
    on_packet: &mut impl FnMut(Packet),
) -> Ended {
    let writer_failed = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::new(AtomicBool::new(false));
    let writer = input.and_then(|input| {
        let stream = stream.try_clone().ok()?;
        let source = input.source.clone();
        let config = config.clone();
        let failed = writer_failed.clone();
        let stop = writer_stop.clone();
        std::thread::Builder::new()
            .name("lq-usb-input".into())
            .spawn(move || write_input(stream, source, &config, &stop, &failed))
            .ok()
    });

    let ended = read_frames(
        stream,
        config,
        shared,
        reconnect,
        generation,
        &writer_failed,
        on_packet,
    );

    // Shutting down both directions wakes a writer blocked in a send, so it is gone before the
    // next connection reuses its input source.
    writer_stop.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(Shutdown::Both);
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    ended
}

fn read_frames(
    mut stream: &TcpStream,
    config: &UsbTunnelConfig,
    shared: &Shared,
    reconnect: &TunnelReconnect,
    generation: u64,
    writer_failed: &AtomicBool,
    on_packet: &mut impl FnMut(Packet),
) -> Ended {
    let mut decoder = TunnelFrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_rx = Instant::now();
    let mut host_heartbeats = false;
    loop {
        if !shared.running.load(Ordering::SeqCst) {
            return Ended::Stopped;
        }
        if reconnect.generation() != generation {
            return Ended::Requested;
        }
        if writer_failed.load(Ordering::SeqCst) {
            return Ended::WriteFailed;
        }
        let quiet = last_rx.elapsed();
        if host_heartbeats && quiet >= config.idle_timeout {
            return Ended::Silent(quiet);
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ended::Closed,
            Ok(n) => {
                last_rx = Instant::now();
                decoder.push(&buf[..n]);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Ended::ReadFailed(e),
        }
        while let Some(frame) = decoder.next_frame() {
            match frame {
                Ok(TunnelFrame::Packet(bytes)) => match Packet::from_bytes(&bytes) {
                    Ok(packet) => {
                        if let Some(bit) = 1u32.checked_shl(u32::from(packet.header.stream_id)) {
                            shared.seen_streams.fetch_or(bit, Ordering::AcqRel);
                        }
                        on_packet(packet);
                    }
                    Err(error) => tracing::trace!(?error, "USB tunnel: invalid packet"),
                },
                Ok(TunnelFrame::Control(ControlMessage::Heartbeat { .. })) => {
                    if !host_heartbeats {
                        tracing::info!(
                            "USB tunnel: the host sends heartbeats; a silent tunnel will be \
                             reopened"
                        );
                        host_heartbeats = true;
                    }
                }
                Ok(TunnelFrame::Control(message)) => {
                    tracing::trace!(?message, "USB tunnel: control message");
                }
                Err(error) if error.is_recoverable() => {
                    tracing::debug!(%error, "USB tunnel: skipped a frame");
                }
                Err(error) => return Ended::Corrupt(error),
            }
        }
    }
}

/// Sends queued input, and a keepalive batch when there is none, until stopped or a write fails.
fn write_input(
    mut stream: TcpStream,
    source: Arc<Mutex<dyn InputSource>>,
    config: &UsbTunnelConfig,
    stop: &AtomicBool,
    failed: &AtomicBool,
) {
    let mut sequence = 0u32;
    let mut last_sent = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let events = source
            .lock()
            .map(|mut source| source.poll())
            .unwrap_or_default();
        if !events.is_empty() || last_sent.elapsed() >= config.keepalive_interval {
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            let payload = InputBatch::new(u64::from(sequence), now_us)
                .with_events(events)
                .encode();
            let header = PacketHeader::new(PacketType::InputEvent, 0, sequence, now_us, &payload);
            let packet = Packet::new(header, payload);
            let wire = match encode_frame(&TunnelFrame::Packet(packet.to_bytes().to_vec())) {
                Ok(wire) => wire,
                Err(error) => {
                    tracing::warn!(%error, "USB tunnel: could not encode input");
                    failed.store(true, Ordering::SeqCst);
                    return;
                }
            };
            if stream.write_all(&wire).is_err() {
                if !stop.load(Ordering::SeqCst) {
                    failed.store(true, Ordering::SeqCst);
                }
                return;
            }
            sequence = sequence.wrapping_add(1);
            last_sent = Instant::now();
        }
        std::thread::sleep(config.input_interval);
    }
}

fn sleep_while_running(shared: &Shared, total: Duration) {
    let deadline = Instant::now() + total;
    while shared.running.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod tests;
