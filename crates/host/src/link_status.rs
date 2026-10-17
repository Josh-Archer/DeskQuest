//! Host-side view of the Quest link: whether a headset is connected and over which transport.
//!
//! The inputs are real events only:
//! - the control TCP listener accepting and closing a connection,
//! - the input UDP receiver seeing a packet from a peer (with a liveness timeout),
//! - the ADB bridge reporting that the reverse tunnel was established or dropped.
//!
//! [`LinkTracker`] records those events. [`LinkStatus`] is the derived view for one
//! instant. Every time-dependent method takes `now` explicitly so the model is pure
//! and liveness can be tested without sleeping.
//!
//! # Caveats
//!
//! - With several clients connected, only the newest open control connection is reported as the
//!   peer (the link is single-client). Older connections stay tracked so that closing the newest
//!   one falls back to the previous one.
//! - Loopback means USB only by convention: the ADB reverse tunnel makes the headset appear as a
//!   local peer. Any local process can also connect from loopback, so the transport label is
//!   informational and must NOT be used as an authentication or authorization signal.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_quest_protocol::ClientConnectionMode;
use linux_quest_transport::AdbBridgeEvent;
use parking_lot::Mutex;

/// How long input-channel silence is tolerated before the input path counts as dead.
pub const DEFAULT_INPUT_LIVENESS: Duration = Duration::from_secs(5);

/// Transport a connected client is reached over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkTransport {
    /// Client reached the host through the ADB reverse tunnel (loopback peer).
    UsbAdb,
    /// Client reached the host over the network (non-loopback peer).
    Wifi,
}

/// Derived link state at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkStatus {
    /// True only when a peer is live AND its transport could be classified.
    pub connected: bool,
    pub transport: Option<LinkTransport>,
    pub peer: Option<SocketAddr>,
    /// Most recent activity from the client (control accept or input packet).
    pub last_seen: Option<Instant>,
}

impl LinkStatus {
    /// Maps the link onto the telemetry enum used by the dashboard wire format.
    pub fn connection_mode(&self) -> ClientConnectionMode {
        match self.transport {
            Some(LinkTransport::UsbAdb) => ClientConnectionMode::UsbAdb,
            Some(LinkTransport::Wifi) => ClientConnectionMode::Wifi,
            None => ClientConnectionMode::Disconnected,
        }
    }
}

/// Records link events from the daemon components and derives [`LinkStatus`].
#[derive(Debug)]
pub struct LinkTracker {
    liveness: Duration,
    /// Open control connections, oldest first. The newest one is reported as the peer.
    control_peers: Vec<SocketAddr>,
    input_peer: Option<SocketAddr>,
    input_last_seen: Option<Instant>,
    last_activity: Option<Instant>,
    adb_reverse_established: bool,
}

/// Shared handle the daemon hands to the IPC server, the TUI and the transport services.
pub type SharedLinkTracker = Arc<Mutex<LinkTracker>>;

/// Creates a shared tracker with no events recorded (disconnected).
pub fn new_shared_link_tracker(liveness: Duration) -> SharedLinkTracker {
    Arc::new(Mutex::new(LinkTracker::new(liveness)))
}

impl LinkTracker {
    pub fn new(liveness: Duration) -> Self {
        Self {
            liveness,
            control_peers: Vec::new(),
            input_peer: None,
            input_last_seen: None,
            last_activity: None,
            adb_reverse_established: false,
        }
    }

    /// The control TCP listener accepted a connection from `peer`.
    pub fn control_accepted(&mut self, peer: SocketAddr, now: Instant) {
        self.control_peers.push(peer);
        self.last_activity = Some(now);
    }

    /// Some connected client sent a control message (liveness refresh).
    pub fn control_activity(&mut self, now: Instant) {
        self.last_activity = Some(now);
    }

    /// The control connection from `peer` closed (EOF, error or Disconnect).
    pub fn control_closed(&mut self, peer: SocketAddr) {
        if let Some(idx) = self.control_peers.iter().position(|p| *p == peer) {
            self.control_peers.remove(idx);
        }
    }

    /// The input UDP receiver accepted a packet from `peer`.
    pub fn input_packet(&mut self, peer: SocketAddr, now: Instant) {
        self.input_peer = Some(peer);
        self.input_last_seen = Some(now);
        self.last_activity = Some(now);
    }

    /// Applies an ADB bridge event. Only reverse-tunnel state is tracked here.
    pub fn adb_event(&mut self, event: &AdbBridgeEvent) {
        match event {
            AdbBridgeEvent::ReversePortEstablished { .. }
            | AdbBridgeEvent::ReconnectionSuccessful { .. } => {
                self.adb_reverse_established = true;
            }
            AdbBridgeEvent::CableBumpDetected { .. }
            | AdbBridgeEvent::DeviceDisconnected(_)
            | AdbBridgeEvent::Error(_) => {
                self.adb_reverse_established = false;
            }
            AdbBridgeEvent::DeviceConnected(_) => {}
        }
    }

    /// Derives the link status at `now`.
    ///
    /// The peer is the newest open control connection. If there is none, the input peer is
    /// used while its packets are within the liveness timeout. A peer only counts as connected
    /// when it can be classified: loopback needs an established ADB reverse tunnel (USB), and
    /// any other address is Wi-Fi.
    pub fn status(&self, now: Instant) -> LinkStatus {
        let input_alive = self
            .input_last_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) < self.liveness);

        let peer = match self.control_peers.last() {
            Some(peer) => Some(*peer),
            None if input_alive => self.input_peer,
            None => None,
        };

        let transport = peer.and_then(|p| classify(p, self.adb_reverse_established));

        LinkStatus {
            connected: transport.is_some(),
            transport,
            peer,
            last_seen: self.last_activity,
        }
    }
}

fn classify(peer: SocketAddr, adb_reverse_established: bool) -> Option<LinkTransport> {
    if is_loopback(peer.ip()) {
        adb_reverse_established.then_some(LinkTransport::UsbAdb)
    } else {
        Some(LinkTransport::Wifi)
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_transport::AdbDevice;

    const LIVENESS: Duration = Duration::from_secs(5);

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn usb_tracker(now: Instant) -> LinkTracker {
        let mut t = LinkTracker::new(LIVENESS);
        t.adb_event(&AdbBridgeEvent::ReversePortEstablished {
            serial: "QUEST".into(),
            port: 48444,
        });
        t.control_accepted(addr("127.0.0.1:50000"), now);
        t
    }

    #[test]
    fn loopback_peer_with_reverse_tunnel_is_usb_adb() {
        let now = Instant::now();
        let status = usb_tracker(now).status(now);

        assert!(status.connected);
        assert_eq!(status.transport, Some(LinkTransport::UsbAdb));
        assert_eq!(status.peer, Some(addr("127.0.0.1:50000")));
        assert_eq!(status.connection_mode(), ClientConnectionMode::UsbAdb);
    }

    #[test]
    fn ipv4_mapped_loopback_peer_is_usb_adb() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.adb_event(&AdbBridgeEvent::ReversePortEstablished {
            serial: "QUEST".into(),
            port: 48444,
        });
        t.control_accepted(addr("[::ffff:127.0.0.1]:50000"), now);

        assert_eq!(t.status(now).transport, Some(LinkTransport::UsbAdb));
    }

    #[test]
    fn ipv6_loopback_peer_is_usb_adb_only_with_reverse_tunnel() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("[::1]:50000"), now);
        assert!(!t.status(now).connected);

        t.adb_event(&AdbBridgeEvent::ReversePortEstablished {
            serial: "QUEST".into(),
            port: 48444,
        });
        assert_eq!(t.status(now).transport, Some(LinkTransport::UsbAdb));
    }

    #[test]
    fn newest_control_connection_wins() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("192.168.1.20:50000"), now);
        t.control_accepted(addr("192.168.1.21:50001"), now);
        assert_eq!(t.status(now).peer, Some(addr("192.168.1.21:50001")));
    }

    #[test]
    fn loopback_peer_without_reverse_tunnel_is_not_connected() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("127.0.0.1:50000"), now);

        let status = t.status(now);
        assert!(!status.connected);
        assert_eq!(status.transport, None);
        assert_eq!(status.connection_mode(), ClientConnectionMode::Disconnected);
    }

    #[test]
    fn lan_peer_is_wifi() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("192.168.1.20:50000"), now);

        let status = t.status(now);
        assert!(status.connected);
        assert_eq!(status.transport, Some(LinkTransport::Wifi));
        assert_eq!(status.connection_mode(), ClientConnectionMode::Wifi);
    }

    #[test]
    fn input_silence_past_liveness_timeout_is_disconnected() {
        let t0 = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.input_packet(addr("192.168.1.20:48442"), t0);

        let just_before = t0 + LIVENESS - Duration::from_millis(1);
        assert!(t.status(just_before).connected);
        assert_eq!(t.status(just_before).transport, Some(LinkTransport::Wifi));

        let at_timeout = t0 + LIVENESS;
        let status = t.status(at_timeout);
        assert!(!status.connected);
        assert_eq!(status.transport, None);
        assert_eq!(status.peer, None);
    }

    #[test]
    fn fresh_input_packets_keep_link_alive_without_control_channel() {
        let t0 = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.input_packet(addr("192.168.1.20:48442"), t0);
        t.input_packet(addr("192.168.1.20:48442"), t0 + Duration::from_secs(4));

        let status = t.status(t0 + Duration::from_secs(6));
        assert!(status.connected);
        assert_eq!(status.transport, Some(LinkTransport::Wifi));
        assert_eq!(status.last_seen, Some(t0 + Duration::from_secs(4)));
    }

    #[test]
    fn control_disconnect_event_is_disconnected() {
        let now = Instant::now();
        let mut t = usb_tracker(now);
        assert!(t.status(now).connected);

        t.control_closed(addr("127.0.0.1:50000"));
        let status = t.status(now);
        assert!(!status.connected);
        assert_eq!(status.transport, None);
        assert_eq!(status.peer, None);
    }

    #[test]
    fn closing_one_of_two_control_connections_keeps_the_other() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("192.168.1.20:50000"), now);
        t.control_accepted(addr("192.168.1.21:50001"), now);

        t.control_closed(addr("192.168.1.21:50001"));
        assert_eq!(t.status(now).peer, Some(addr("192.168.1.20:50000")));

        t.control_closed(addr("192.168.1.20:50000"));
        assert!(!t.status(now).connected);
    }

    #[test]
    fn dropped_reverse_tunnel_disconnects_usb_client() {
        let now = Instant::now();
        let mut t = usb_tracker(now);

        t.adb_event(&AdbBridgeEvent::CableBumpDetected {
            serial: "QUEST".into(),
        });
        assert!(!t.status(now).connected);

        t.adb_event(&AdbBridgeEvent::ReconnectionSuccessful {
            serial: "QUEST".into(),
        });
        assert!(t.status(now).connected);
    }

    #[test]
    fn device_connected_alone_does_not_establish_reverse_tunnel() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.adb_event(&AdbBridgeEvent::DeviceConnected(AdbDevice {
            serial: "QUEST".into(),
            state: linux_quest_transport::AdbDeviceState::Device,
            product: None,
            model: None,
            device_name: None,
            transport_id: None,
        }));
        t.control_accepted(addr("127.0.0.1:50000"), now);

        assert!(!t.status(now).connected);
    }

    #[test]
    fn control_activity_refreshes_last_seen() {
        let t0 = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("192.168.1.20:50000"), t0);
        let later = t0 + Duration::from_secs(30);
        t.control_activity(later);
        assert_eq!(t.status(later).last_seen, Some(later));
        assert!(t.status(later).connected);
    }

    #[test]
    fn no_events_is_disconnected() {
        let t = LinkTracker::new(LIVENESS);
        let status = t.status(Instant::now());

        assert_eq!(status, LinkStatus::default());
        assert_eq!(status.connection_mode(), ClientConnectionMode::Disconnected);
    }
}
