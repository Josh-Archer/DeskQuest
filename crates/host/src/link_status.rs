//! Host-side view of the Quest link: whether a headset is connected and over which transport.
//!
//! The inputs are real events only:
//! - the control TCP listener accepting and closing a connection,
//! - the input UDP receiver seeing a packet from a peer (with a liveness timeout),
//! - the ADB bridge reporting that the reverse tunnel was established or dropped,
//! - a control connection negotiating the USB tunnel, sending on it, and closing.
//!
//! A headset in the client's `Auto` mode is reachable both ways at once: it keeps a USB tunnel
//! open whenever the cable is plugged in and sends UDP input keepalives over Wi-Fi all the
//! time. Video goes over the tunnel while it carries data, and moves to the Wi-Fi peer as soon
//! as the tunnel closes or goes quiet for [`TUNNEL_VIDEO_LIVENESS`].
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

/// How recently a USB tunnel peer must have sent something for video to stay on the tunnel.
/// The headset sends an input keepalive over the tunnel every 100 ms. After an unplug adb
/// usually closes the tunnel at once, but not always; a tunnel that carried nothing for this
/// long loses video to a live Wi-Fi peer. It stays open (and gets video back when it carries
/// data again) until the control service drops it.
pub const TUNNEL_VIDEO_LIVENESS: Duration = Duration::from_secs(1);

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
    /// Open connections that negotiated the USB tunnel, oldest first, with the last time each
    /// one carried data. Each is also in `control_peers`.
    tunnel_peers: Vec<(SocketAddr, Instant)>,
    tunnel_liveness: Duration,
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
            tunnel_peers: Vec::new(),
            tunnel_liveness: TUNNEL_VIDEO_LIVENESS,
        }
    }

    /// Changes how long a quiet USB tunnel keeps video (tests).
    pub fn with_tunnel_liveness(mut self, liveness: Duration) -> Self {
        self.tunnel_liveness = liveness;
        self
    }

    /// The control connection from `peer` negotiated the USB tunnel (video, input, settings).
    pub fn tunnel_opened(&mut self, peer: SocketAddr, now: Instant) {
        self.tunnel_peers.retain(|(p, _)| *p != peer);
        self.tunnel_peers.push((peer, now));
        self.last_activity = Some(now);
    }

    /// The USB tunnel from `peer` delivered a frame.
    pub fn tunnel_activity(&mut self, peer: SocketAddr, now: Instant) {
        if let Some((_, seen)) = self.tunnel_peers.iter_mut().find(|(p, _)| *p == peer) {
            *seen = now;
        }
        self.last_activity = Some(now);
    }

    /// The USB tunnel from `peer` closed. Its control connection is closed separately
    /// ([`Self::control_closed`]).
    pub fn tunnel_closed(&mut self, peer: SocketAddr) {
        self.tunnel_peers.retain(|(p, _)| *p != peer);
    }

    /// The newest USB tunnel that carried data within [`TUNNEL_VIDEO_LIVENESS`], if the ADB
    /// reverse tunnel is up.
    fn live_tunnel(&self, now: Instant) -> Option<SocketAddr> {
        if !self.adb_reverse_established {
            return None;
        }
        self.tunnel_peers
            .iter()
            .rev()
            .find(|(_, seen)| now.saturating_duration_since(*seen) < self.tunnel_liveness)
            .map(|(peer, _)| *peer)
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
    /// A USB tunnel that carried data within [`TUNNEL_VIDEO_LIVENESS`] is the link. Otherwise
    /// the peer is the newest open control connection, or, if there is none, the input peer
    /// while its packets are within the liveness timeout. A control connection through the USB
    /// tunnel (a quiet tunnel, or the settings channel) gives way to live input from the
    /// network. A peer only counts as connected when it can be classified: loopback needs an
    /// established ADB reverse tunnel (USB), and any other address is Wi-Fi.
    pub fn status(&self, now: Instant) -> LinkStatus {
        let input_alive = self
            .input_last_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) < self.liveness);

        let live_input = self.input_peer.filter(|_| input_alive);
        // The cable is the lower-latency path: a tunnel that is carrying data wins over Wi-Fi
        // input from the same headset.
        let peer = self
            .live_tunnel(now)
            .or_else(|| match self.control_peers.last() {
                // The headset streams over the network (live UDP input from a LAN address) and only
                // its control connection comes through the USB tunnel, for example because a
                // firewall blocks TCP on the LAN, or the tunnel went quiet after an unplug. Video
                // goes where the input comes from, so the network peer is the link.
                Some(control)
                    if is_loopback(control.ip())
                        && live_input.is_some_and(|input| !is_loopback(input.ip())) =>
                {
                    live_input
                }
                Some(peer) => Some(*peer),
                None => live_input,
            });

        let transport = peer.and_then(|p| classify(p, self.adb_reverse_established));

        LinkStatus {
            connected: transport.is_some(),
            transport,
            peer,
            last_seen: self.last_activity,
        }
    }
}

impl LinkTracker {
    /// Destination selected for video at `now`; unspecified selects the negotiated USB tunnel.
    ///
    /// Video flows to a peer whose last activity, control or input keepalive, is
    /// within the liveness timeout. When the headset goes silent this returns `None` after
    /// [`DEFAULT_INPUT_LIVENESS`] (or whatever liveness the tracker was built with); a new input
    /// source address is picked up on the next call, so the sender follows a re-latch.
    pub fn video_peer(&self, now: Instant) -> Option<IpAddr> {
        let status = self.status(now);
        if !status.connected {
            return None;
        }
        let seen = status.last_seen?;
        if now.saturating_duration_since(seen) >= self.liveness {
            return None;
        }
        match status.transport {
            // Unspecified is an internal route marker consumed by HybridSink. Real destinations
            // on 127.0.0.0/8 remain regular UDP destinations.
            Some(LinkTransport::UsbAdb) => Some(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            Some(LinkTransport::Wifi) => status.peer.map(|peer| peer.ip().to_canonical()),
            None => None,
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

    #[test]
    fn video_peer_is_the_live_network_peer() {
        let now = Instant::now();
        let mut t = LinkTracker::new(LIVENESS);
        assert_eq!(t.video_peer(now), None, "no events, no video");

        t.input_packet(addr("192.168.1.20:50000"), now);
        assert_eq!(t.video_peer(now), Some("192.168.1.20".parse().unwrap()));

        // Silence past the liveness timeout stops video.
        assert_eq!(t.video_peer(now + LIVENESS), None);
        assert!(t.video_peer(now + Duration::from_secs(4)).is_some());

        // A new sender address is followed.
        let later = now + Duration::from_secs(6);
        t.input_packet(addr("192.168.1.31:40000"), later);
        assert_eq!(t.video_peer(later), Some("192.168.1.31".parse().unwrap()));
    }

    #[test]
    fn a_control_connection_over_the_tunnel_does_not_take_video_off_the_network() {
        // UDP mode with the control channel over `adb reverse` (TCP blocked on the LAN).
        let now = Instant::now();
        let mut t = usb_tracker(now);
        t.input_packet(addr("192.168.1.20:48442"), now);
        let status = t.status(now);
        assert_eq!(status.transport, Some(LinkTransport::Wifi));
        assert_eq!(status.peer, Some(addr("192.168.1.20:48442")));
        assert_eq!(t.video_peer(now), Some("192.168.1.20".parse().unwrap()));

        // Once the input goes quiet only the cable is left: USB, and the stale link sends no
        // video either way.
        let later = now + LIVENESS;
        assert_eq!(t.status(later).transport, Some(LinkTransport::UsbAdb));
        assert_eq!(t.video_peer(later), None);

        // A LAN control connection is still the link, as before.
        let mut lan = LinkTracker::new(LIVENESS);
        lan.control_accepted(addr("192.168.1.21:50000"), now);
        lan.input_packet(addr("192.168.1.20:48442"), now);
        assert_eq!(lan.status(now).peer, Some(addr("192.168.1.21:50000")));
    }

    const USB_ROUTE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

    fn wifi_ip() -> IpAddr {
        "192.168.1.20".parse().unwrap()
    }

    /// A client in `Auto` mode with the cable plugged in: a USB tunnel plus the UDP input
    /// keepalives it sends over Wi-Fi all the time.
    fn auto_headset(now: Instant) -> LinkTracker {
        let mut t = LinkTracker::new(LIVENESS);
        t.adb_event(&AdbBridgeEvent::ReversePortEstablished {
            serial: "QUEST".into(),
            port: 48444,
        });
        t.control_accepted(addr("127.0.0.1:50000"), now);
        t.tunnel_opened(addr("127.0.0.1:50000"), now);
        t.input_packet(addr("192.168.1.20:40000"), now);
        t
    }

    #[test]
    fn a_live_tunnel_beats_wifi_input_and_video_moves_when_it_closes() {
        let t0 = Instant::now();
        let mut t = auto_headset(t0);
        assert_eq!(t.status(t0).transport, Some(LinkTransport::UsbAdb));
        assert_eq!(t.video_peer(t0), Some(USB_ROUTE));

        // Unplugged: adb closes the tunnel, the Wi-Fi keepalives go on.
        let t1 = t0 + Duration::from_millis(200);
        t.input_packet(addr("192.168.1.20:40000"), t1);
        t.tunnel_closed(addr("127.0.0.1:50000"));
        t.control_closed(addr("127.0.0.1:50000"));
        assert_eq!(t.status(t1).transport, Some(LinkTransport::Wifi));
        assert_eq!(t.video_peer(t1), Some(wifi_ip()));

        // Plugged back in: the new tunnel takes video back.
        let t2 = t1 + Duration::from_secs(3);
        t.input_packet(addr("192.168.1.20:40000"), t2);
        t.control_accepted(addr("127.0.0.1:50100"), t2);
        assert_eq!(
            t.video_peer(t2),
            Some(wifi_ip()),
            "a control connection that has not negotiated the tunnel is not the tunnel"
        );
        t.tunnel_opened(addr("127.0.0.1:50100"), t2);
        assert_eq!(t.status(t2).transport, Some(LinkTransport::UsbAdb));
        assert_eq!(t.video_peer(t2), Some(USB_ROUTE));
    }

    #[test]
    fn a_quiet_tunnel_loses_video_to_wifi_and_gets_it_back() {
        let t0 = Instant::now();
        let mut t = auto_headset(t0);
        let tunnel = addr("127.0.0.1:50000");
        let just_before = t0 + TUNNEL_VIDEO_LIVENESS - Duration::from_millis(1);
        assert_eq!(t.video_peer(just_before), Some(USB_ROUTE));

        // Keepalives keep coming over Wi-Fi only: the tunnel is open but carries nothing.
        let t1 = t0 + TUNNEL_VIDEO_LIVENESS;
        t.input_packet(addr("192.168.1.20:40000"), t1);
        assert_eq!(t.video_peer(t1), Some(wifi_ip()));
        assert_eq!(t.status(t1).transport, Some(LinkTransport::Wifi));

        // The tunnel carries data again.
        let t2 = t1 + Duration::from_millis(100);
        t.tunnel_activity(tunnel, t2);
        assert_eq!(t.video_peer(t2), Some(USB_ROUTE));
    }

    #[test]
    fn a_tunnel_counts_only_while_adb_reverse_is_up() {
        let now = Instant::now();
        let mut t = auto_headset(now);
        t.adb_event(&AdbBridgeEvent::DeviceDisconnected("QUEST".into()));
        assert_eq!(t.video_peer(now), Some(wifi_ip()));
        t.adb_event(&AdbBridgeEvent::ReconnectionSuccessful {
            serial: "QUEST".into(),
        });
        assert_eq!(t.video_peer(now), Some(USB_ROUTE));
    }

    #[test]
    fn a_usb_only_headset_keeps_video_on_a_quiet_tunnel() {
        // Without Wi-Fi input there is nowhere else to send video, as before.
        let t0 = Instant::now();
        let mut t = usb_tracker(t0);
        t.tunnel_opened(addr("127.0.0.1:50000"), t0);
        let later = t0 + TUNNEL_VIDEO_LIVENESS * 2;
        assert_eq!(t.status(later).transport, Some(LinkTransport::UsbAdb));
        assert_eq!(t.video_peer(later), Some(USB_ROUTE));
    }

    #[test]
    fn video_peer_selects_usb_tunnel_and_folds_mapped_addresses() {
        let now = Instant::now();
        assert_eq!(
            usb_tracker(now).video_peer(now),
            Some(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            "the USB tunnel uses the explicit sink route marker"
        );

        let mut t = LinkTracker::new(LIVENESS);
        t.control_accepted(addr("[::ffff:192.168.1.20]:50000"), now);
        assert_eq!(t.video_peer(now), Some("192.168.1.20".parse().unwrap()));
    }
}
