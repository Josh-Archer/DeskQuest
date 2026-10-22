//! The host's side of the headset settings channel: the last status each headset reported, and
//! a way to send it a layout or settings.
//!
//! The control service ([`crate::HostClipboardService`]) feeds this hub: it asks every new
//! control connection for its status, and a connection that answers is a headset that
//! understands the layout and settings messages. The daemon's IPC server reads the hub and
//! sends through it. See `linux_quest_protocol::headset` for the message flow.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use linux_quest_protocol::{HeadsetSettings, HeadsetSettingsPatch, HeadsetStatus, ScreenPlacement};
use linux_quest_transport::ControlMessage;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

/// How long [`HeadsetHub::apply_layout`] and [`HeadsetHub::apply_settings`] wait for the
/// headset to report the result.
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Why a request could not be carried to the headset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeadsetError {
    #[error(
        "no headset is connected on the control channel (the headset app must be running, and \
         reach the host on TCP 48444 over the LAN or the USB tunnel)"
    )]
    NotConnected,
    #[error("the headset did not report back within {0:?}")]
    NoAnswer(Duration),
    #[error("invalid {0}")]
    Invalid(String),
}

/// A control connection that answered the status request.
struct Peer {
    id: u64,
    addr: SocketAddr,
    tx: mpsc::UnboundedSender<ControlMessage>,
    status: HeadsetStatus,
    updated: Instant,
}

/// Shared between the control service (which feeds it) and the IPC server (which reads it).
pub struct HeadsetHub {
    peers: Mutex<Vec<Peer>>,
    /// Bumped on every status update or disconnect, so a request can wait for the answer.
    generation: watch::Sender<u64>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for HeadsetHub {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadsetHub {
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(Vec::new()),
            generation: watch::channel(0).0,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// An id for a new control connection.
    pub fn next_peer_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The message a new control connection is greeted with.
    pub fn greeting() -> ControlMessage {
        ControlMessage::HeadsetStatusRequest
    }

    /// Records `status` from connection `id`, whose writer is `tx`.
    pub fn status_received(
        &self,
        id: u64,
        addr: SocketAddr,
        tx: &mpsc::UnboundedSender<ControlMessage>,
        status: HeadsetStatus,
    ) {
        let now = Instant::now();
        {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == id) {
                Some(peer) => {
                    peer.status = status;
                    peer.updated = now;
                }
                None => {
                    tracing::info!(peer = %addr, device = %status.device, "headset reports its layout and settings");
                    peers.push(Peer {
                        id,
                        addr,
                        tx: tx.clone(),
                        status,
                        updated: now,
                    });
                }
            }
        }
        self.bump();
    }

    /// Connection `id` closed.
    pub fn peer_closed(&self, id: u64) {
        let removed = {
            let mut peers = self.peers.lock();
            let before = peers.len();
            peers.retain(|p| p.id != id);
            before != peers.len()
        };
        if removed {
            tracing::info!("headset settings channel closed");
            self.bump();
        }
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// The newest status from the most recently updated headset, with where it came from.
    pub fn status(&self) -> Option<(HeadsetStatus, SocketAddr, Instant)> {
        let peers = self.peers.lock();
        peers
            .iter()
            .max_by_key(|p| p.updated)
            .map(|p| (p.status.clone(), p.addr, p.updated))
    }

    /// True when some headset has answered and is still connected.
    pub fn is_connected(&self) -> bool {
        !self.peers.lock().is_empty()
    }

    /// Sends `msg` to every connected headset; dropped writers are forgotten.
    fn send(&self, msg: ControlMessage) -> Result<(), HeadsetError> {
        let mut peers = self.peers.lock();
        peers.retain(|p| p.tx.send(msg.clone()).is_ok());
        if peers.is_empty() {
            Err(HeadsetError::NotConnected)
        } else {
            Ok(())
        }
    }

    /// Sends `msg` and waits for the next status report (the headset answers every request
    /// with one), returning it.
    async fn request(
        &self,
        msg: ControlMessage,
        timeout: Duration,
    ) -> Result<HeadsetStatus, HeadsetError> {
        let mut updates = self.generation.subscribe();
        updates.mark_unchanged();
        self.send(msg)?;
        match tokio::time::timeout(timeout, updates.changed()).await {
            Ok(Ok(())) => self
                .status()
                .map(|(status, _, _)| status)
                .ok_or(HeadsetError::NotConnected),
            _ => Err(HeadsetError::NoAnswer(timeout)),
        }
    }

    /// Changes only the settings `patch` carries. The other fields are filled from the
    /// headset's own last report, never from host-side values, so a one-field change cannot
    /// reset, for example, the headset's bitrate.
    pub async fn apply_settings_patch(
        &self,
        patch: HeadsetSettingsPatch,
        timeout: Duration,
    ) -> Result<HeadsetStatus, HeadsetError> {
        let (current, _, _) = self.status().ok_or(HeadsetError::NotConnected)?;
        if patch.is_empty() {
            return Ok(current);
        }
        self.apply_settings(patch.applied_to(&current.settings), timeout)
            .await
    }

    /// Asks the headset to place `screens`, and returns the layout it then reports.
    pub async fn apply_layout(
        &self,
        screens: Vec<ScreenPlacement>,
        timeout: Duration,
    ) -> Result<HeadsetStatus, HeadsetError> {
        if let Some(bad) = screens.iter().find(|s| !s.is_valid()) {
            return Err(HeadsetError::Invalid(format!(
                "placement for display {}: non-finite or non-positive values",
                bad.display_id
            )));
        }
        self.request(ControlMessage::SetHeadsetLayout(screens), timeout)
            .await
    }

    /// Asks the headset to apply `settings`, and returns the settings it then reports.
    pub async fn apply_settings(
        &self,
        settings: HeadsetSettings,
        timeout: Duration,
    ) -> Result<HeadsetStatus, HeadsetError> {
        if !settings.refresh_rate_hz.is_finite() || settings.refresh_rate_hz <= 0.0 {
            return Err(HeadsetError::Invalid("refresh rate".into()));
        }
        if settings.bitrate_mbps == 0 {
            return Err(HeadsetError::Invalid("bitrate".into()));
        }
        self.request(ControlMessage::SetHeadsetSettings(settings), timeout)
            .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use linux_quest_protocol::{HeadsetCodec, HeadsetStats, ScreenCurve};

    pub(crate) fn status(device: &str) -> HeadsetStatus {
        HeadsetStatus {
            device: device.into(),
            app_version: "test".into(),
            settings: HeadsetSettings {
                bitrate_mbps: 80,
                adaptive_bitrate: true,
                codec: HeadsetCodec::Av1,
                refresh_rate_hz: 90.0,
                curve: ScreenCurve::Curved { radius_m: 3.0 },
                hud: true,
                passthrough: true,
            },
            screens: vec![ScreenPlacement::facing_user(
                0,
                0.0,
                0.0,
                1.2,
                1.4,
                1.8,
                1.0,
                ScreenCurve::Curved { radius_m: 3.0 },
            )],
            showing: vec![0],
            refresh_rates_hz: vec![72.0, 90.0, 120.0],
            stats: HeadsetStats::default(),
        }
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:50000".parse().unwrap()
    }

    #[tokio::test]
    async fn without_a_headset_requests_fail_fast() {
        let hub = HeadsetHub::new();
        assert!(hub.status().is_none());
        let err = hub
            .apply_settings(status("q").settings, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err, HeadsetError::NotConnected);
    }

    #[tokio::test]
    async fn a_request_returns_the_status_the_headset_reports_next() {
        let hub = std::sync::Arc::new(HeadsetHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = hub.next_peer_id();
        hub.status_received(id, addr(), &tx, status("Quest 3"));
        assert!(hub.is_connected());

        // A fake headset: applies the settings it is sent and reports them.
        let fake = {
            let hub = hub.clone();
            tokio::spawn(async move {
                let Some(ControlMessage::SetHeadsetSettings(settings)) = rx.recv().await else {
                    panic!("expected settings");
                };
                let mut next = status("Quest 3");
                next.settings = settings;
                // The headset clamps what it cannot do.
                next.settings.refresh_rate_hz = 120.0;
                hub.status_received(id, addr(), &tx, next);
            })
        };
        let mut wanted = status("Quest 3").settings;
        wanted.hud = false;
        wanted.refresh_rate_hz = 144.0;
        let applied = hub
            .apply_settings(wanted, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!applied.settings.hud);
        assert_eq!(applied.settings.refresh_rate_hz, 120.0);
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn a_silent_headset_times_out_and_a_closed_one_is_forgotten() {
        let hub = HeadsetHub::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = hub.next_peer_id();
        hub.status_received(id, addr(), &tx, status("q"));
        let err = hub
            .apply_layout(status("q").screens, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, HeadsetError::NoAnswer(_)));

        drop(rx);
        // The writer is gone: the next send notices and the hub is empty.
        let err = hub
            .apply_layout(status("q").screens, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(err, HeadsetError::NotConnected);
        assert!(!hub.is_connected());
        hub.peer_closed(id);
    }

    #[tokio::test]
    async fn invalid_requests_are_refused_before_sending() {
        let hub = HeadsetHub::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.status_received(hub.next_peer_id(), addr(), &tx, status("q"));
        let mut screens = status("q").screens;
        screens[0].width_m = f32::NAN;
        assert!(matches!(
            hub.apply_layout(screens, Duration::from_millis(20)).await,
            Err(HeadsetError::Invalid(_))
        ));
        let mut settings = status("q").settings;
        settings.bitrate_mbps = 0;
        assert!(matches!(
            hub.apply_settings(settings, Duration::from_millis(20))
                .await,
            Err(HeadsetError::Invalid(_))
        ));
        assert!(rx.try_recv().is_err(), "nothing was sent");
    }

    #[test]
    fn the_newest_status_wins_and_closing_removes_it() {
        let hub = HeadsetHub::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (a, b) = (hub.next_peer_id(), hub.next_peer_id());
        hub.status_received(a, addr(), &tx, status("first"));
        hub.status_received(b, addr(), &tx, status("second"));
        assert_eq!(hub.status().unwrap().0.device, "second");
        hub.peer_closed(b);
        assert_eq!(hub.status().unwrap().0.device, "first");
        hub.peer_closed(a);
        assert!(hub.status().is_none());
    }
}
