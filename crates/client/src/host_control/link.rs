//! The headset's control connection to the host for the settings channel.
//!
//! A background thread keeps one TCP connection to the host's control port. After connecting
//! it waits for the host's `HeadsetStatusRequest`. An older host never sends one: the thread
//! then closes the connection and leaves that address alone (see [`LinkConfig`]), because an
//! older host counts a control connection through the USB tunnel as a USB link and pauses UDP
//! video while it is open.
//!
//! The render loop drains [`HostCommand`]s with [`HostLink::try_command`] and publishes its
//! status with [`HostLink::publish`]; the newest status is sent, older unsent ones are dropped.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use linux_quest_protocol::{HeadsetSettings, HeadsetStatus, ScreenPlacement};
use linux_quest_transport::{
    encode_control_frame, ControlFrameReader, ControlMessage, DEFAULT_CONTROL_PORT,
    SETTINGS_TUNNEL_PORT,
};
use parking_lot::Mutex;

use crate::config::{ClientConfig, TransportMode};

/// What the host asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum HostCommand {
    /// Send a status report now.
    ReportStatus,
    SetLayout(Vec<ScreenPlacement>),
    SetSettings(HeadsetSettings),
}

/// Timing of the connection thread.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// Addresses to try, in order.
    pub candidates: Vec<SocketAddr>,
    pub connect_timeout: Duration,
    /// How long to wait for the host's status request before taking it for an older host.
    pub greeting_timeout: Duration,
    /// Pause between connection rounds.
    pub retry_interval: Duration,
    /// How long an older host's network address is left alone. Its loopback (USB tunnel)
    /// address is never tried again in this run: each try would pause its video.
    pub old_host_backoff: Duration,
    /// Socket read timeout, which is also how often unsent status is checked.
    pub poll_interval: Duration,
}

impl LinkConfig {
    pub fn new(candidates: Vec<SocketAddr>) -> Self {
        Self {
            candidates,
            connect_timeout: Duration::from_secs(1),
            greeting_timeout: Duration::from_millis(1500),
            retry_interval: Duration::from_secs(2),
            old_host_backoff: Duration::from_secs(300),
            poll_interval: Duration::from_millis(50),
        }
    }
}

/// Where the host's control port can be reached, best first.
///
/// - USB mode: the settings channel's own USB tunnel ([`SETTINGS_TUNNEL_PORT`]).
/// - Wi-Fi and Auto modes: the host's LAN address (when known), then that tunnel, which works
///   when a firewall blocks TCP on the LAN and the cable is plugged in.
///
/// The tunnel is not the control port's own (`adb reverse tcp:48444`): only a host that speaks
/// the settings channel sets up the settings tunnel, so an older host is never reached through
/// the cable. An older host counted a loopback control connection as a USB link and paused
/// Wi-Fi video, and could stall on it for good.
pub fn control_candidates(config: &ClientConfig) -> Vec<SocketAddr> {
    let tunnel = SocketAddr::from(([127, 0, 0, 1], SETTINGS_TUNNEL_PORT));
    match config.transport_mode {
        TransportMode::AdbReverseTcp if config.host_addr.ip().is_loopback() => {
            vec![SocketAddr::new(config.host_addr.ip(), SETTINGS_TUNNEL_PORT)]
        }
        TransportMode::AdbReverseTcp => vec![config.host_addr],
        TransportMode::Udp | TransportMode::Auto => {
            let mut out = Vec::new();
            let ip = config.host_addr.ip();
            if !ip.is_unspecified() && !ip.is_loopback() {
                out.push(SocketAddr::new(ip, DEFAULT_CONTROL_PORT));
            }
            out.push(tunnel);
            out
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// The newest status not yet sent.
    pending: Mutex<Option<HeadsetStatus>>,
    /// A host that speaks the settings channel is connected.
    connected: AtomicBool,
    running: AtomicBool,
}

/// Handle of the connection thread. Stops the thread when dropped.
pub struct HostLink {
    shared: Arc<Shared>,
    commands: mpsc::Receiver<HostCommand>,
    thread: Option<JoinHandle<()>>,
}

impl HostLink {
    /// Starts the connection thread.
    pub fn spawn(config: LinkConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.running.store(true, Ordering::SeqCst);
        let (tx, commands) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lq-host-control".into())
            .spawn(move || run(config, thread_shared, tx))?;
        Ok(Self {
            shared,
            commands,
            thread: Some(thread),
        })
    }

    /// The next request from the host, without waiting.
    pub fn try_command(&self) -> Option<HostCommand> {
        self.commands.try_recv().ok()
    }

    /// Queues `status` to be sent (replacing any unsent one).
    pub fn publish(&self, status: HeadsetStatus) {
        *self.shared.pending.lock() = Some(status);
    }

    /// True while a host that understands the settings channel is connected.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
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

impl Drop for HostLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How a connection ended.
#[derive(Debug, PartialEq)]
enum Ended {
    /// The host never asked for a status: it predates the settings channel.
    OldHost,
    Closed,
    Stopped,
}

fn run(config: LinkConfig, shared: Arc<Shared>, commands: mpsc::Sender<HostCommand>) {
    let mut avoid: HashMap<SocketAddr, Option<Instant>> = HashMap::new();
    while shared.running.load(Ordering::SeqCst) {
        for &addr in &config.candidates {
            if !shared.running.load(Ordering::SeqCst) {
                return;
            }
            match avoid.get(&addr) {
                Some(None) => continue,
                Some(Some(until)) if Instant::now() < *until => continue,
                _ => {}
            }
            let Ok(stream) = TcpStream::connect_timeout(&addr, config.connect_timeout) else {
                continue;
            };
            tracing::info!(host = %addr, "control connection open; waiting for the host's status request");
            let ended = session(stream, &config, &shared, &commands);
            shared.connected.store(false, Ordering::SeqCst);
            match ended {
                Ended::OldHost => {
                    let until = (!addr.ip().is_loopback())
                        .then(|| Instant::now() + config.old_host_backoff);
                    tracing::warn!(
                        host = %addr,
                        "the host did not ask for the headset's status: it predates the \
                         settings channel; not using this address for it"
                    );
                    avoid.insert(addr, until);
                }
                Ended::Closed => tracing::info!(host = %addr, "control connection closed"),
                Ended::Stopped => return,
            }
            // Start over from the best address.
            break;
        }
        sleep_while_running(&shared, config.retry_interval);
    }
}

fn sleep_while_running(shared: &Shared, total: Duration) {
    let deadline = Instant::now() + total;
    while shared.running.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn session(
    mut stream: TcpStream,
    config: &LinkConfig,
    shared: &Shared,
    commands: &mpsc::Sender<HostCommand>,
) -> Ended {
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(config.poll_interval)).is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .is_err()
    {
        return Ended::Closed;
    }
    // Speak first: the host tells USB tunnel clients (which open with a hello) from plain
    // control clients by their first bytes, and drops a connection that stays silent. A
    // heartbeat is a message every host version accepts.
    let hello = ControlMessage::Heartbeat {
        sequence: 0,
        timestamp_us: 0,
    };
    match encode_control_frame(&hello) {
        Ok(frame) if stream.write_all(&frame).is_ok() => {}
        _ => return Ended::Closed,
    }
    let greeting_deadline = Instant::now() + config.greeting_timeout;
    let mut greeted = false;
    let mut reader = ControlFrameReader::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        if !shared.running.load(Ordering::SeqCst) {
            return Ended::Stopped;
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ended::Closed,
            Ok(n) => reader.push(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Ended::Closed,
        }
        while let Some(message) = reader.next_message() {
            let command = match message {
                Ok(ControlMessage::HeadsetStatusRequest) => {
                    if !greeted {
                        tracing::info!("host speaks the settings channel");
                    }
                    greeted = true;
                    shared.connected.store(true, Ordering::SeqCst);
                    Some(HostCommand::ReportStatus)
                }
                Ok(ControlMessage::SetHeadsetLayout(screens)) if greeted => {
                    Some(HostCommand::SetLayout(screens))
                }
                Ok(ControlMessage::SetHeadsetSettings(settings)) if greeted => {
                    Some(HostCommand::SetSettings(settings))
                }
                Ok(ControlMessage::Disconnect { .. }) => return Ended::Closed,
                // Clipboard, heartbeats and anything else are not for this link.
                Ok(_) => None,
                Err(e) if e.is_recoverable() => None,
                Err(_) => return Ended::Closed,
            };
            if let Some(command) = command {
                if commands.send(command).is_err() {
                    return Ended::Stopped;
                }
            }
        }
        if !greeted {
            if Instant::now() >= greeting_deadline {
                return Ended::OldHost;
            }
            continue;
        }
        let pending = shared.pending.lock().take();
        if let Some(status) = pending {
            let frame = match encode_control_frame(&ControlMessage::HeadsetStatus(Box::new(status)))
            {
                Ok(frame) => frame,
                Err(e) => {
                    tracing::warn!(error = %e, "could not encode the headset status");
                    continue;
                }
            };
            if stream.write_all(&frame).is_err() {
                return Ended::Closed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn quick(candidates: Vec<SocketAddr>) -> LinkConfig {
        LinkConfig {
            connect_timeout: Duration::from_millis(200),
            greeting_timeout: Duration::from_millis(150),
            retry_interval: Duration::from_millis(20),
            old_host_backoff: Duration::from_secs(60),
            poll_interval: Duration::from_millis(10),
            candidates,
        }
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn candidates_prefer_the_lan_and_fall_back_to_the_tunnel() {
        let lan: SocketAddr = "192.168.1.20:48444".parse().unwrap();
        let tunnel: SocketAddr = "127.0.0.1:48445".parse().unwrap();
        let udp = ClientConfig {
            transport_mode: TransportMode::Udp,
            host_addr: lan,
            ..Default::default()
        };
        assert_eq!(control_candidates(&udp), vec![lan, tunnel]);
        let unset = ClientConfig {
            host_addr: "0.0.0.0:48444".parse().unwrap(),
            ..udp.clone()
        };
        assert_eq!(control_candidates(&unset), vec![tunnel]);
        let usb = ClientConfig {
            transport_mode: TransportMode::AdbReverseTcp,
            ..Default::default()
        };
        assert_eq!(
            control_candidates(&usb),
            vec![tunnel],
            "never the control port's own tunnel, which an older host also has"
        );
        // Auto: the LAN host once it is known, and the settings tunnel for the cable.
        let auto = ClientConfig::default();
        assert_eq!(auto.transport_mode, TransportMode::Auto);
        assert_eq!(control_candidates(&auto), vec![tunnel]);
        let auto_lan = ClientConfig {
            host_addr: lan,
            ..Default::default()
        };
        assert_eq!(control_candidates(&auto_lan), vec![lan, tunnel]);
    }

    #[test]
    fn talks_to_a_host_that_greets_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let link = HostLink::spawn(quick(vec![addr])).unwrap();
        let (mut host, _) = listener.accept().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        host.write_all(&encode_control_frame(&ControlMessage::HeadsetStatusRequest).unwrap())
            .unwrap();
        assert_eq!(wait_for(|| link.try_command()), HostCommand::ReportStatus);
        assert!(link.is_connected());

        let layout = vec![ScreenPlacement::facing_user(
            1,
            -0.4,
            0.0,
            1.2,
            1.4,
            1.6,
            0.9,
            linux_quest_protocol::ScreenCurve::Flat,
        )];
        host.write_all(
            &encode_control_frame(&ControlMessage::SetHeadsetLayout(layout.clone())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_for(|| link.try_command()),
            HostCommand::SetLayout(layout)
        );

        let status = crate::host_control::tests::sample_status();
        link.publish(status.clone());
        let mut reader = ControlFrameReader::new();
        let mut buf = [0u8; 4096];
        let mut next = || {
            wait_for(|| {
                if let Some(m) = reader.next_message() {
                    return Some(m.unwrap());
                }
                let n = host.read(&mut buf).ok()?;
                reader.push(&buf[..n]);
                reader.next_message().map(Result::unwrap)
            })
        };
        // The link spoke first, with a heartbeat.
        assert!(matches!(next(), ControlMessage::Heartbeat { .. }));
        assert_eq!(next(), ControlMessage::HeadsetStatus(Box::new(status)));

        drop(host);
        wait_for(|| (!link.is_connected()).then_some(()));
        link.stop();
    }

    #[test]
    fn an_old_host_that_never_greets_is_dropped_and_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let link = HostLink::spawn(quick(vec![addr])).unwrap();

        let (mut first, _) = wait_for(|| listener.accept().ok());
        first.set_nonblocking(false).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // The link speaks first (a heartbeat), then closes it after the greeting timeout.
        let mut sent = Vec::new();
        first.read_to_end(&mut sent).unwrap();
        let mut reader = ControlFrameReader::new();
        reader.push(&sent);
        assert!(matches!(
            reader.next_message(),
            Some(Ok(ControlMessage::Heartbeat { .. }))
        ));
        assert!(reader.next_message().is_none(), "nothing else");
        assert!(!link.is_connected());

        // A loopback address of an old host is never tried again.
        std::thread::sleep(Duration::from_millis(200));
        assert!(listener.accept().is_err(), "no second connection");
        link.stop();
    }

    #[test]
    fn skips_unknown_and_unrelated_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let link = HostLink::spawn(quick(vec![addr])).unwrap();
        let (mut host, _) = listener.accept().unwrap();
        // A message from a future version, a heartbeat, then the greeting.
        let mut future = 4u32.to_be_bytes().to_vec();
        future.extend(77_777u32.to_le_bytes());
        host.write_all(&future).unwrap();
        host.write_all(
            &encode_control_frame(&ControlMessage::Heartbeat {
                sequence: 1,
                timestamp_us: 1,
            })
            .unwrap(),
        )
        .unwrap();
        host.write_all(&encode_control_frame(&ControlMessage::HeadsetStatusRequest).unwrap())
            .unwrap();
        assert_eq!(wait_for(|| link.try_command()), HostCommand::ReportStatus);
        link.stop();
    }
}
