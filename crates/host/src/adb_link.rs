//! Runs the ADB bridge inside the daemon and feeds its tunnel events into the [`LinkTracker`].
//!
//! The supervisor owns one [`AdbBridge`] and drives it with a poll loop. While a Quest is
//! attached and authorized it keeps `adb reverse tcp:<control> tcp:<control>` in place, so the
//! headset can reach the control listener at `127.0.0.1:<control>` over USB. Every tunnel
//! up/down event is forwarded to [`LinkTracker::adb_event`], which is what lets a loopback
//! control connection be classified as USB.
//!
//! # Degradation
//!
//! Nothing here is fatal. When `adb` is missing, the adb server cannot be reached, no (or more
//! than one) Quest is attached, or the device is unauthorized, the supervisor logs ONE message
//! when the situation changes (not one per poll) and keeps polling. A missing or broken `adb`
//! is retried at [`AdbSupervisorConfig::retry_interval`]; otherwise polling runs at the bridge's
//! `poll_interval`.
//!
//! # Shutdown
//!
//! On the shutdown broadcast the supervisor stops polling, removes the reverse mapping on a
//! best-effort basis (bounded by [`REMOVE_TIMEOUT`]) and tells the tracker the tunnel is down.
//! The task has finished when the returned [`JoinHandle`] resolves.
//!
//! [`LinkTracker`]: crate::link_status::LinkTracker
//! [`LinkTracker::adb_event`]: crate::link_status::LinkTracker::adb_event

use std::time::Duration;

use linux_quest_transport::{
    AdbBridge, AdbBridgeConfig, AdbBridgeEvent, AdbBridgeState, AdbCommandRunner, AdbDevice,
    AdbDeviceState, AdbError,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::link_status::SharedLinkTracker;

/// Upper bound for removing the reverse mapping during shutdown.
pub const REMOVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Daemon-side settings for the ADB supervisor.
#[derive(Debug, Clone)]
pub struct AdbSupervisorConfig {
    /// Bridge settings (ports, poll interval, serial filter).
    pub bridge: AdbBridgeConfig,
    /// How long to wait before retrying when `adb` is missing or failing.
    pub retry_interval: Duration,
}

impl AdbSupervisorConfig {
    /// Daemon defaults: reverse the control port, poll every 2 s, retry a broken `adb` every 15 s.
    /// Naming a serial trusts it and skips the "looks like a Quest" model check.
    pub fn new(target_serial: Option<String>) -> Self {
        Self {
            bridge: AdbBridgeConfig {
                poll_interval: Duration::from_secs(2),
                require_quest_filter: target_serial.is_none(),
                target_serial,
                ..AdbBridgeConfig::default()
            },
            retry_interval: Duration::from_secs(15),
        }
    }
}

impl Default for AdbSupervisorConfig {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Why the tunnel is not up. Logged once per change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    AdbMissing,
    AdbFailing(String),
    NoDevice,
    NoQuest(Vec<String>),
    Unauthorized(Vec<String>),
    MultipleDevices(Vec<String>),
    TunnelFailed(String),
}

impl Problem {
    fn from_error(e: &AdbError) -> Self {
        match e {
            AdbError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => Self::AdbMissing,
            other => Self::AdbFailing(other.to_string()),
        }
    }

    /// Missing or failing `adb` is retried slowly; device-level problems at the poll interval.
    fn is_adb_level(&self) -> bool {
        matches!(
            self,
            Self::AdbMissing | Self::AdbFailing(_) | Self::TunnelFailed(_)
        )
    }

    fn log(&self, port: u16) {
        match self {
            Self::AdbMissing => warn!(
                "`adb` was not found on PATH; USB connections are disabled until it is installed \
                 (Wi-Fi is unaffected). Install Android platform-tools, or pass --no-adb to \
                 silence this."
            ),
            Self::AdbFailing(e) => warn!(
                "`adb` is not usable ({e}); USB connections are unavailable. Will keep retrying."
            ),
            Self::NoDevice => {
                info!("No ADB device attached; waiting for a Quest to be plugged in over USB")
            }
            Self::NoQuest(serials) => info!(
                "ADB device(s) attached but none looks like a Quest ({}); \
                 pass --adb-serial to use one anyway",
                serials.join(", ")
            ),
            Self::Unauthorized(serials) => warn!(
                "ADB device(s) {} unauthorized: put the headset on and accept the USB debugging \
                 prompt. Waiting.",
                serials.join(", ")
            ),
            Self::MultipleDevices(serials) => warn!(
                "Several Quest devices are attached over ADB ({}); not guessing. \
                 Unplug the extras or pass --adb-serial <serial>.",
                serials.join(", ")
            ),
            Self::TunnelFailed(e) => {
                warn!("`adb reverse tcp:{port} tcp:{port}` failed: {e}. Will keep retrying.")
            }
        }
    }
}

struct Supervisor<R: AdbCommandRunner> {
    runner: R,
    bridge: AdbBridge<R>,
    config: AdbSupervisorConfig,
    link: SharedLinkTracker,
    problem: Option<Problem>,
}

impl<R: AdbCommandRunner + Clone> Supervisor<R> {
    fn new(runner: R, config: AdbSupervisorConfig, link: SharedLinkTracker) -> Self {
        Self {
            bridge: AdbBridge::new(runner.clone(), config.bridge.clone()),
            runner,
            config,
            link,
            problem: None,
        }
    }

    fn port(&self) -> u16 {
        self.config.bridge.remote_port
    }

    /// Logs `problem` unless it is the same as the last one reported.
    fn report(&mut self, problem: Problem) {
        if self.problem.as_ref() != Some(&problem) {
            problem.log(self.port());
            self.problem = Some(problem);
        }
    }

    fn is_candidate(&self, dev: &AdbDevice) -> bool {
        let serial_ok = self
            .config
            .bridge
            .target_serial
            .as_ref()
            .is_none_or(|s| *s == dev.serial);
        let quest_ok = !self.config.bridge.require_quest_filter || dev.is_quest();
        serial_ok && quest_ok
    }

    /// One iteration. Returns how long to wait before the next one.
    async fn step(&mut self) -> Duration {
        let poll = self.config.bridge.poll_interval;
        let retry = self.config.retry_interval;

        if matches!(self.bridge.state(), AdbBridgeState::Idle) {
            // Look before connecting, so the reason for staying idle can be reported and the
            // bridge never picks one of several headsets arbitrarily.
            let devices = match self.runner.list_devices().await {
                Ok(d) => d,
                Err(e) => {
                    self.report(Problem::from_error(&e));
                    return retry;
                }
            };
            let ready: Vec<&AdbDevice> = devices
                .iter()
                .filter(|d| d.state == AdbDeviceState::Device && self.is_candidate(d))
                .collect();
            let problem = match ready.len() {
                1 => None,
                0 => Some(diagnose_no_ready(&devices, |d| self.is_candidate(d))),
                _ => Some(Problem::MultipleDevices(
                    ready.iter().map(|d| d.serial.clone()).collect(),
                )),
            };
            if let Some(problem) = problem {
                self.report(problem);
                return poll;
            }
        }

        match self.bridge.poll_step().await {
            Ok(Some(evt)) => self.handle_event(evt),
            Ok(None) => {}
            Err(e) => {
                self.report(Problem::from_error(&e));
                return retry;
            }
        }

        if self.problem.as_ref().is_some_and(Problem::is_adb_level) {
            retry
        } else {
            poll
        }
    }

    fn handle_event(&mut self, evt: AdbBridgeEvent) {
        self.link.lock().adb_event(&evt);
        let port = self.port();
        match &evt {
            AdbBridgeEvent::ReversePortEstablished { serial, .. } => {
                self.problem = None;
                info!("ADB reverse tunnel up for {serial}: device tcp:{port} -> host tcp:{port}");
            }
            AdbBridgeEvent::ReconnectionSuccessful { serial } => {
                self.problem = None;
                info!("ADB reverse tunnel re-established for {serial}");
            }
            AdbBridgeEvent::CableBumpDetected { serial } => {
                warn!(
                    "ADB device {serial} went away; USB tunnel is down, waiting for it to return"
                );
            }
            AdbBridgeEvent::DeviceDisconnected(serial) => {
                info!("ADB device {serial} did not return; back to waiting for a Quest");
            }
            AdbBridgeEvent::Error(msg) => self.report(Problem::TunnelFailed(msg.clone())),
            AdbBridgeEvent::DeviceConnected(_) => {}
        }
    }

    /// Best-effort removal of the reverse mapping, then marks the tunnel down.
    async fn shutdown(&mut self) {
        let serial = match self.bridge.state() {
            AdbBridgeState::Connected { serial, .. }
            | AdbBridgeState::Reconnecting { serial, .. } => Some(serial.clone()),
            AdbBridgeState::Idle => None,
        };
        if let Some(serial) = serial {
            let port = self.port();
            match tokio::time::timeout(
                REMOVE_TIMEOUT,
                self.runner.reverse_remove(Some(&serial), port),
            )
            .await
            {
                Ok(Ok(())) => info!("Removed ADB reverse tcp:{port} for {serial}"),
                Ok(Err(e)) => warn!("Could not remove ADB reverse tcp:{port} for {serial}: {e}"),
                Err(_) => warn!("Timed out removing ADB reverse tcp:{port} for {serial}"),
            }
            self.link
                .lock()
                .adb_event(&AdbBridgeEvent::DeviceDisconnected(serial));
        }
    }
}

fn diagnose_no_ready(devices: &[AdbDevice], is_candidate: impl Fn(&AdbDevice) -> bool) -> Problem {
    let unauthorized: Vec<String> = devices
        .iter()
        .filter(|d| d.state == AdbDeviceState::Unauthorized && is_candidate(d))
        .map(|d| d.serial.clone())
        .collect();
    if !unauthorized.is_empty() {
        return Problem::Unauthorized(unauthorized);
    }
    let others: Vec<String> = devices
        .iter()
        .filter(|d| d.state == AdbDeviceState::Device && !is_candidate(d))
        .map(|d| d.serial.clone())
        .collect();
    if others.is_empty() {
        Problem::NoDevice
    } else {
        Problem::NoQuest(others)
    }
}

/// Starts the ADB supervisor task. It stops when `shutdown` fires (or its sender is dropped)
/// and removes the reverse mapping before the returned handle resolves.
pub fn spawn_adb_supervisor<R>(
    runner: R,
    config: AdbSupervisorConfig,
    link: SharedLinkTracker,
    mut shutdown: broadcast::Receiver<()>,
) -> JoinHandle<()>
where
    R: AdbCommandRunner + Clone + 'static,
{
    tokio::spawn(async move {
        let mut sup = Supervisor::new(runner, config, link);
        loop {
            // A step is never cancelled half way: it would lose track of a reverse rule it
            // had just created. Each adb command has its own timeout, so a step is bounded.
            let delay = sup.step().await;
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        sup.shutdown().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link_status::{new_shared_link_tracker, LinkTransport};
    use linux_quest_transport::MockAdbRunner;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Instant;

    fn quest(serial: &str) -> AdbDevice {
        AdbDevice {
            serial: serial.to_string(),
            state: AdbDeviceState::Device,
            product: Some("eureka".into()),
            model: Some("Quest_3".into()),
            device_name: Some("eureka".into()),
            transport_id: Some("1".into()),
        }
    }

    fn config() -> AdbSupervisorConfig {
        AdbSupervisorConfig {
            bridge: AdbBridgeConfig {
                poll_interval: Duration::from_millis(100),
                max_reconnect_attempts: 3,
                ..AdbSupervisorConfig::default().bridge
            },
            retry_interval: Duration::from_secs(10),
        }
    }

    fn usb_up(link: &SharedLinkTracker) -> bool {
        let mut t = link.lock();
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let now = Instant::now();
        t.control_accepted(peer, now);
        let status = t.status(now);
        t.control_closed(peer);
        status.transport == Some(LinkTransport::UsbAdb)
    }

    struct Harness {
        runner: MockAdbRunner,
        link: SharedLinkTracker,
        tx: broadcast::Sender<()>,
        task: JoinHandle<()>,
    }

    fn start() -> Harness {
        let runner = MockAdbRunner::new();
        let link = new_shared_link_tracker(Duration::from_secs(5));
        let (tx, rx) = broadcast::channel(1);
        let task = spawn_adb_supervisor(Arc::new(runner.clone()), config(), link.clone(), rx);
        Harness {
            runner,
            link,
            tx,
            task,
        }
    }

    async fn settle() {
        // Lets the supervisor run a few polls on the paused clock.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn attach_brings_tunnel_up_and_detach_takes_it_down() {
        let h = start();
        settle().await;
        assert!(!usb_up(&h.link), "no device: no USB");

        h.runner.add_device(quest("Q1"));
        settle().await;
        assert_eq!(h.runner.get_rules("Q1").len(), 1);
        assert_eq!(h.runner.get_rules("Q1")[0].remote_port, 48444);
        assert!(usb_up(&h.link));

        h.runner.remove_device("Q1");
        settle().await;
        assert!(!usb_up(&h.link), "detach: tunnel down");

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn flap_re_establishes_the_tunnel() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        settle().await;
        assert!(usb_up(&h.link));

        // Unplug and replug within the reconnect window.
        h.runner.remove_device("Q1");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!usb_up(&h.link));
        h.runner.add_device(quest("Q1"));
        settle().await;
        assert!(usb_up(&h.link));
        assert_eq!(h.runner.get_rules("Q1").len(), 1);

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn long_detach_gives_up_then_new_attach_recovers() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        settle().await;
        h.runner.remove_device("Q1");
        // Longer than max_reconnect_attempts polls: back to Idle.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!usb_up(&h.link));
        h.runner.add_device(quest("Q2"));
        settle().await;
        assert_eq!(h.runner.get_rules("Q2").len(), 1);
        assert!(usb_up(&h.link));

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn missing_binary_never_crashes_and_recovers_when_installed() {
        let h = start();
        h.runner.set_binary_missing(true);
        h.runner.add_device(quest("Q1"));
        // Several retry intervals: still running, no tunnel.
        tokio::time::sleep(Duration::from_secs(45)).await;
        assert!(
            !h.task.is_finished(),
            "supervisor must survive a missing adb"
        );
        assert!(!usb_up(&h.link));

        h.runner.set_binary_missing(false);
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert!(usb_up(&h.link), "tunnel comes up once adb appears");

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_device_waits_then_connects_once_accepted() {
        let h = start();
        let mut dev = quest("Q1");
        dev.state = AdbDeviceState::Unauthorized;
        h.runner.add_device(dev);
        settle().await;
        assert!(h.runner.get_rules("Q1").is_empty());
        assert!(!usb_up(&h.link));

        h.runner.set_device_state("Q1", AdbDeviceState::Device);
        settle().await;
        assert!(usb_up(&h.link));

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_quests_are_not_guessed() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        h.runner.add_device(quest("Q2"));
        settle().await;
        assert!(h.runner.get_rules("Q1").is_empty());
        assert!(h.runner.get_rules("Q2").is_empty());
        assert!(!usb_up(&h.link));

        h.runner.remove_device("Q2");
        settle().await;
        assert!(usb_up(&h.link));

        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_serial_selects_one_of_several() {
        let runner = MockAdbRunner::new();
        runner.add_device(quest("Q1"));
        runner.add_device(quest("Q2"));
        let link = new_shared_link_tracker(Duration::from_secs(5));
        let (tx, rx) = broadcast::channel(1);
        let mut cfg = AdbSupervisorConfig::new(Some("Q2".into()));
        cfg.bridge.poll_interval = Duration::from_millis(100);
        let task = spawn_adb_supervisor(Arc::new(runner.clone()), cfg, link.clone(), rx);
        settle().await;
        assert!(runner.get_rules("Q1").is_empty());
        assert_eq!(runner.get_rules("Q2").len(), 1);
        let _ = tx.send(());
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn non_quest_device_is_ignored() {
        let h = start();
        let mut phone = quest("PHONE");
        phone.product = Some("sunfish".into());
        phone.model = Some("Pixel_4a".into());
        phone.device_name = Some("sunfish".into());
        h.runner.add_device(phone);
        settle().await;
        assert!(h.runner.get_rules("PHONE").is_empty());
        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_removes_the_reverse_mapping_and_marks_link_down() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        settle().await;
        assert_eq!(h.runner.get_rules("Q1").len(), 1);
        assert!(usb_up(&h.link));

        let _ = h.tx.send(());
        h.task.await.unwrap();
        assert!(h.runner.get_rules("Q1").is_empty(), "reverse removed");
        assert!(!usb_up(&h.link), "tracker told the tunnel is down");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_without_device_or_with_missing_binary_is_clean() {
        let h = start();
        settle().await;
        let _ = h.tx.send(());
        h.task.await.unwrap();

        let h = start();
        h.runner.set_binary_missing(true);
        settle().await;
        let _ = h.tx.send(());
        h.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_shutdown_sender_also_stops_the_task() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        settle().await;
        drop(h.tx);
        h.task.await.unwrap();
        assert!(h.runner.get_rules("Q1").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn transient_adb_failure_is_retried() {
        let h = start();
        h.runner.add_device(quest("Q1"));
        // The next adb call (the first device listing) fails once; the supervisor must back
        // off to the retry interval and then recover.
        h.runner.set_fail_next(Some("adb server error".into()));
        settle().await;
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert!(usb_up(&h.link), "recovers after a transient adb failure");
        let _ = h.tx.send(());
        h.task.await.unwrap();
    }
}
