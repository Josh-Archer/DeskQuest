//! A loopback host with a USB tunnel that can be unplugged and a real UDP input receiver: input
//! and video follow the cable both ways, and no input event arrives twice.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, Receiver};

use linux_quest_protocol::{InputBatch, PacketHeader, PacketType};
use linux_quest_transport::{
    encode_frame, ControlMessage, InputChannelReceiver, TunnelFrame, TunnelFrameDecoder,
};

use super::*;

const WAIT: Duration = Duration::from_secs(5);

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}

fn health(connected: bool, quiet_ms: u64, heartbeats: bool) -> Option<TunnelHealth> {
    Some(TunnelHealth {
        connected,
        quiet: Duration::from_millis(quiet_ms),
        heartbeats,
    })
}

#[test]
fn the_tunnel_carries_input_while_it_is_live_and_wifi_otherwise() {
    use InputPath::{Usb, Wifi};
    // Auto, cable in: the tunnel, unless the host streams over Wi-Fi.
    assert_eq!(
        choose_input_path(health(true, 10, true), true, None),
        Some(Usb)
    );
    assert_eq!(
        choose_input_path(health(true, 10, true), true, Some(Usb)),
        Some(Usb)
    );
    assert_eq!(
        choose_input_path(health(true, 10, true), true, Some(Wifi)),
        Some(Wifi),
        "input follows the host's video to Wi-Fi"
    );
    // Cable out, or a tunnel gone quiet although its host heartbeats.
    assert_eq!(
        choose_input_path(health(false, 0, false), true, Some(Usb)),
        Some(Wifi)
    );
    assert_eq!(
        choose_input_path(
            health(true, TUNNEL_STALE_AFTER.as_millis() as u64, true),
            true,
            Some(Usb)
        ),
        Some(Wifi)
    );
    // An older host never heartbeats: a quiet tunnel may just be a static screen.
    assert_eq!(
        choose_input_path(health(true, 60_000, false), true, Some(Usb)),
        Some(Usb)
    );
    // No Wi-Fi host known: the tunnel, even a quiet one, or nothing.
    assert_eq!(
        choose_input_path(health(true, 60_000, true), false, Some(Wifi)),
        Some(Usb)
    );
    assert_eq!(
        choose_input_path(health(false, 0, false), false, None),
        None
    );
    // Single-path modes.
    assert_eq!(choose_input_path(None, true, None), Some(Wifi));
    assert_eq!(choose_input_path(None, false, None), None);
    assert_eq!(
        choose_input_path(health(true, 0, false), false, None),
        Some(Usb)
    );
}

/// The host: a control port that serves USB tunnel connections while "plugged in" (and drops
/// them otherwise, as `adb` does without the cable), and a UDP input port.
struct FakeHost {
    tunnel_addr: SocketAddr,
    udp_addr: SocketAddr,
    plugged: Arc<AtomicBool>,
    tunnel: Arc<Mutex<Option<TcpStream>>>,
    connections: Arc<AtomicU64>,
    tunnel_events: Arc<Mutex<Vec<InputEvent>>>,
    wifi_events: Arc<Mutex<Vec<InputEvent>>>,
    wifi_datagrams: Arc<AtomicU64>,
    video_out: std::net::UdpSocket,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl FakeHost {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let tunnel_addr = listener.local_addr().unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        let udp_addr = udp.local_addr().unwrap();
        let mut host = Self {
            tunnel_addr,
            udp_addr,
            plugged: Arc::new(AtomicBool::new(true)),
            tunnel: Arc::default(),
            connections: Arc::default(),
            tunnel_events: Arc::default(),
            wifi_events: Arc::default(),
            wifi_datagrams: Arc::default(),
            video_out: std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
            stop: Arc::default(),
            threads: Vec::new(),
        };
        host.threads.push(host.spawn_tunnel_listener(listener));
        host.threads.push(host.spawn_input_receiver(udp));
        host
    }

    fn spawn_tunnel_listener(&self, listener: TcpListener) -> JoinHandle<()> {
        let (plugged, slot, connections, events, stop) = (
            self.plugged.clone(),
            self.tunnel.clone(),
            self.connections.clone(),
            self.tunnel_events.clone(),
            self.stop.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                if !plugged.load(Ordering::SeqCst) {
                    continue; // dropped: the headset's negotiation fails, as without a cable
                }
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let mut hello = [0u8; linux_quest_transport::USB_TUNNEL_HELLO.len()];
                if stream.read_exact(&mut hello).is_err() {
                    continue;
                }
                assert_eq!(&hello, linux_quest_transport::USB_TUNNEL_HELLO);
                stream
                    .write_all(linux_quest_transport::USB_TUNNEL_ACCEPT)
                    .unwrap();
                *slot.lock().unwrap() = Some(stream.try_clone().unwrap());
                connections.fetch_add(1, Ordering::SeqCst);
                let (events, stop) = (events.clone(), stop.clone());
                std::thread::spawn(move || read_tunnel_input(stream, &events, &stop));
            }
        })
    }

    fn spawn_input_receiver(&self, socket: std::net::UdpSocket) -> JoinHandle<()> {
        let (events, datagrams, stop) = (
            self.wifi_events.clone(),
            self.wifi_datagrams.clone(),
            self.stop.clone(),
        );
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let socket = tokio::net::UdpSocket::from_std(socket).unwrap();
                let mut receiver = InputChannelReceiver::new(socket, None, true);
                while !stop.load(Ordering::SeqCst) {
                    let got =
                        tokio::time::timeout(Duration::from_millis(50), receiver.recv_events())
                            .await;
                    if let Ok(Ok(got)) = got {
                        datagrams.fetch_add(1, Ordering::SeqCst);
                        events.lock().unwrap().extend(got);
                    }
                }
            });
        })
    }

    fn tunnel_open(&self) -> bool {
        self.tunnel.lock().unwrap().is_some()
    }

    fn send_tunnel(&self, frame: &TunnelFrame) {
        let mut slot = self.tunnel.lock().unwrap();
        let stream = slot.as_mut().expect("a tunnel is open");
        stream.write_all(&encode_frame(frame).unwrap()).unwrap();
    }

    fn send_wifi_video(&self, to: SocketAddr, stream_id: u16, sequence: u32) {
        self.video_out
            .send_to(&video(stream_id, sequence).to_bytes(), to)
            .unwrap();
    }

    /// Pulls the cable: the open tunnel closes and new ones are refused.
    fn unplug(&self) {
        self.plugged.store(false, Ordering::SeqCst);
        if let Some(stream) = self.tunnel.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn plug(&self) {
        self.plugged.store(true, Ordering::SeqCst);
    }

    fn markers_over_usb(&self) -> Vec<i32> {
        markers(&self.tunnel_events.lock().unwrap())
    }

    fn markers_over_wifi(&self) -> Vec<i32> {
        markers(&self.wifi_events.lock().unwrap())
    }

    fn keyframe_requests_over_usb(&self) -> usize {
        keyframe_requests(&self.tunnel_events.lock().unwrap())
    }

    fn keyframe_requests_over_wifi(&self) -> usize {
        keyframe_requests(&self.wifi_events.lock().unwrap())
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.unplug();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn read_tunnel_input(mut stream: TcpStream, events: &Mutex<Vec<InputEvent>>, stop: &AtomicBool) {
    let mut decoder = TunnelFrameDecoder::new();
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::SeqCst) {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => decoder.push(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => return,
        }
        while let Some(frame) = decoder.next_frame() {
            if let Ok(TunnelFrame::Packet(bytes)) = frame {
                let packet = Packet::from_bytes(&bytes).unwrap();
                assert_eq!(packet.header.packet_type, PacketType::InputEvent);
                let batch = InputBatch::decode(&packet.payload).unwrap();
                events.lock().unwrap().extend(batch.events);
            }
        }
    }
}

fn video(stream_id: u16, sequence: u32) -> Packet {
    let payload = b"video";
    let header = PacketHeader::new(PacketType::VideoFrameChunk, stream_id, sequence, 0, payload);
    Packet::new(header, bytes::Bytes::from_static(payload))
}

fn tunnel_video(stream_id: u16, sequence: u32) -> TunnelFrame {
    TunnelFrame::Packet(video(stream_id, sequence).to_bytes().to_vec())
}

fn heartbeat() -> TunnelFrame {
    TunnelFrame::Control(ControlMessage::Heartbeat {
        sequence: 1,
        timestamp_us: 0,
    })
}

/// A recognisable input event.
fn marker(n: i32) -> InputEvent {
    InputEvent::MouseMoveRelative { dx: n, dy: 0 }
}

fn markers(events: &[InputEvent]) -> Vec<i32> {
    events
        .iter()
        .filter_map(|e| match e {
            InputEvent::MouseMoveRelative { dx, .. } => Some(*dx),
            _ => None,
        })
        .collect()
}

fn keyframe_requests(events: &[InputEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, InputEvent::KeyframeRequest))
        .count()
}

fn quick_tunnel(host_addr: SocketAddr) -> UsbTunnelConfig {
    UsbTunnelConfig {
        host_addr,
        connect_timeout: Duration::from_millis(200),
        retry_interval: Duration::from_millis(20),
        poll_interval: Duration::from_millis(10),
        idle_timeout: Duration::from_secs(5),
        input_interval: Duration::from_millis(2),
        keepalive_interval: Duration::from_millis(20),
    }
}

fn quick_wifi(targets: Vec<SocketAddr>, sources: Option<Vec<IpAddr>>) -> WifiConfig {
    WifiConfig {
        video_bind: "127.0.0.1:0".parse().unwrap(),
        sources,
        sender: InputSenderConfig {
            keepalive_interval: Duration::from_millis(20),
            poll_interval: Duration::from_millis(2),
            ..InputSenderConfig::new(targets)
        },
    }
}

struct Headset {
    transport: ClientTransport,
    packets: Receiver<Packet>,
}

impl Headset {
    fn start(config: TransportConfig) -> Self {
        let (tx, packets) = mpsc::sync_channel(1024);
        let transport = ClientTransport::spawn(config, tx).unwrap();
        Self { transport, packets }
    }

    fn auto(host: &FakeHost) -> Self {
        Self::start(TransportConfig {
            mode: TransportMode::Auto,
            tunnel: Some(quick_tunnel(host.tunnel_addr)),
            wifi: Some(quick_wifi(vec![host.udp_addr], None)),
        })
    }

    fn send_markers(&self, range: std::ops::RangeInclusive<i32>) {
        let events = self.transport.pointer_events();
        let waker = self.transport.waker();
        for n in range {
            events.push(marker(n));
            waker.wake();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn video_addr(&self) -> SocketAddr {
        let mut addr = None;
        assert!(wait_until(WAIT, || {
            addr = self.transport.video_addr();
            addr.is_some()
        }));
        addr.unwrap()
    }

    /// Waits for a packet from the host.
    fn next_packet(&self) -> Packet {
        self.packets.recv_timeout(WAIT).expect("no packet arrived")
    }
}

fn range(from: i32, to: i32) -> Vec<i32> {
    (from..=to).collect()
}

#[test]
fn input_and_video_follow_the_cable_both_ways_without_duplicates() {
    let host = FakeHost::start();
    let headset = Headset::auto(&host);

    // Cable in: the tunnel carries the input, Wi-Fi only keepalives.
    assert!(wait_until(WAIT, || host.tunnel_open()));
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Usb)));
    assert!(
        wait_until(WAIT, || host.wifi_datagrams.load(Ordering::SeqCst) > 3),
        "Wi-Fi keepalives run beside the tunnel, so the host can switch at once"
    );
    headset.send_markers(1..=20);
    assert!(wait_until(WAIT, || host.markers_over_usb() == range(1, 20)));
    host.send_tunnel(&tunnel_video(0, 1));
    assert_eq!(headset.next_packet().header.sequence, 1);
    assert!(host.markers_over_wifi().is_empty());

    // Unplugged: input moves to Wi-Fi at once, asking for a keyframe there, and Wi-Fi video
    // is taken in.
    let unplugged = Instant::now();
    host.unplug();
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Wifi)));
    assert!(wait_until(WAIT, || host.keyframe_requests_over_wifi() >= 1));
    let switch = unplugged.elapsed();
    assert!(
        switch < Duration::from_millis(500),
        "switching to Wi-Fi took {switch:?}"
    );
    headset.send_markers(21..=40);
    assert!(wait_until(WAIT, || host.markers_over_wifi() == range(21, 40)));
    host.send_wifi_video(headset.video_addr(), 0, 2);
    assert_eq!(headset.next_packet().header.sequence, 2);

    // Plugged back in: the tunnel reopens, but input stays on Wi-Fi while video still comes
    // that way...
    host.plug();
    assert!(wait_until(WAIT, || host.tunnel_open()));
    assert!(wait_until(WAIT, || host.connections.load(Ordering::SeqCst) == 2));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(headset.transport.input_path(), Some(InputPath::Wifi));
    // ...until the host moves video to the tunnel. Then input follows, with a keyframe request.
    let usb_requests = host.keyframe_requests_over_usb();
    host.send_tunnel(&tunnel_video(0, 3));
    assert_eq!(headset.next_packet().header.sequence, 3);
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Usb)));
    assert!(wait_until(WAIT, || host.keyframe_requests_over_usb() > usb_requests));
    headset.send_markers(41..=60);
    assert!(wait_until(WAIT, || host.markers_over_usb().len() == 40));

    // Every event arrived exactly once, in order on each path.
    std::thread::sleep(Duration::from_millis(100));
    let usb = host.markers_over_usb();
    let wifi = host.markers_over_wifi();
    assert_eq!(usb, [range(1, 20), range(41, 60)].concat());
    assert_eq!(wifi, range(21, 40));
}

#[test]
fn a_tunnel_that_goes_quiet_hands_input_to_wifi_until_it_speaks_again() {
    let host = FakeHost::start();
    let headset = Headset::auto(&host);
    assert!(wait_until(WAIT, || host.tunnel_open()));
    // The host heartbeats, so silence means trouble.
    host.send_tunnel(&heartbeat());
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Usb)));

    // Silence (an unplug adb has not noticed): after `TUNNEL_STALE_AFTER`, input goes over
    // Wi-Fi although the tunnel is still open.
    let quiet_since = Instant::now();
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Wifi)));
    assert!(quiet_since.elapsed() >= TUNNEL_STALE_AFTER - Duration::from_millis(100));
    assert!(host.tunnel_open(), "the tunnel was not reopened yet");
    headset.send_markers(1..=10);
    assert!(wait_until(WAIT, || host.markers_over_wifi() == range(1, 10)));

    // The tunnel speaks again: input goes back to it.
    host.send_tunnel(&heartbeat());
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Usb)));
    headset.send_markers(11..=20);
    assert!(wait_until(WAIT, || host.markers_over_usb() == range(11, 20)));
    assert_eq!(host.markers_over_wifi(), range(1, 10));
}

#[test]
fn the_wifi_path_waits_for_the_host_address_from_discovery() {
    let host = FakeHost::start();
    host.unplug();
    let headset = Headset::start(TransportConfig {
        mode: TransportMode::Auto,
        tunnel: Some(quick_tunnel(host.tunnel_addr)),
        wifi: Some(quick_wifi(Vec::new(), Some(Vec::new()))),
    });
    let wifi = headset.transport.wifi().unwrap();
    assert!(!wifi.has_host());
    assert_eq!(
        headset.transport.input_path(),
        None,
        "nowhere to send input yet"
    );
    headset.send_markers(1..=5);

    // Video from a host the headset does not know yet is refused.
    let video_addr = headset.video_addr();
    host.send_wifi_video(video_addr, 0, 1);
    assert!(headset
        .packets
        .recv_timeout(Duration::from_millis(200))
        .is_err());

    // Discovery finds the host: keepalives start, the waiting input goes out, video comes in.
    wifi.set_host(IpAddr::from([127, 0, 0, 1]), &[], host.udp_addr.port());
    assert!(wifi.has_host());
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Wifi)));
    assert!(wait_until(WAIT, || host.markers_over_wifi() == range(1, 5)));
    host.send_wifi_video(video_addr, 0, 2);
    assert_eq!(headset.next_packet().header.sequence, 2);
}

#[test]
fn single_path_modes_run_only_their_path() {
    let host = FakeHost::start();

    let udp = Headset::start(TransportConfig {
        mode: TransportMode::Udp,
        tunnel: None,
        wifi: Some(quick_wifi(vec![host.udp_addr], None)),
    });
    udp.send_markers(1..=5);
    assert!(wait_until(WAIT, || host.markers_over_wifi() == range(1, 5)));
    assert_eq!(host.connections.load(Ordering::SeqCst), 0, "no tunnel");
    assert!(udp.transport.tunnel_reconnect().is_none());
    drop(udp);

    let wifi_datagrams = host.wifi_datagrams.load(Ordering::SeqCst);
    let usb = Headset::start(TransportConfig {
        mode: TransportMode::AdbReverseTcp,
        tunnel: Some(quick_tunnel(host.tunnel_addr)),
        wifi: None,
    });
    assert!(usb.transport.wifi().is_none() && usb.transport.video_addr().is_none());
    usb.send_markers(6..=10);
    assert!(wait_until(WAIT, || host.markers_over_usb() == range(6, 10)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(host.wifi_datagrams.load(Ordering::SeqCst), wifi_datagrams);
}

#[test]
fn the_production_config_matches_the_mode() {
    let auto = TransportConfig::for_client(&ClientConfig::default());
    assert_eq!(auto.mode, TransportMode::Auto);
    assert_eq!(auto.tunnel.unwrap().host_addr, USB_TUNNEL_ADDR);
    let wifi = auto.wifi.unwrap();
    assert_eq!(wifi.video_bind, DEFAULT_VIDEO_BIND);
    assert!(
        wifi.sender.targets.is_empty(),
        "the LAN host is not known yet"
    );
    assert_eq!(wifi.sources, Some(vec![IpAddr::from([127, 0, 0, 1])]));

    let lan: SocketAddr = "192.168.1.20:48444".parse().unwrap();
    let auto_lan = TransportConfig::for_client(&ClientConfig {
        host_addr: lan,
        ..Default::default()
    });
    assert_eq!(
        auto_lan.tunnel.unwrap().host_addr,
        USB_TUNNEL_ADDR,
        "the tunnel never dials the LAN host"
    );
    let wifi = auto_lan.wifi.unwrap();
    assert_eq!(
        wifi.sender.targets,
        vec!["192.168.1.20:48442".parse().unwrap()]
    );
    assert_eq!(wifi.sources, Some(vec![lan.ip()]));

    let udp = TransportConfig::for_client(&ClientConfig {
        host_addr: lan,
        transport_mode: TransportMode::Udp,
        ..Default::default()
    });
    assert!(udp.tunnel.is_none() && udp.wifi.is_some());

    let usb = TransportConfig::for_client(&ClientConfig {
        transport_mode: TransportMode::AdbReverseTcp,
        ..Default::default()
    });
    assert!(usb.wifi.is_none());
    assert_eq!(
        usb.tunnel.unwrap().host_addr,
        ClientConfig::default().host_addr
    );
}

#[test]
fn host_addresses_on_this_devices_network_come_first() {
    let addr = |s: &str| -> SocketAddr { s.parse().unwrap() };
    // The PC from the hardware test: k3s bridge, LAN, Tailscale, Docker.
    let targets = vec![
        addr("10.42.7.1:48442"),
        addr("192.168.91.175:48442"),
        addr("100.76.80.115:48442"),
        addr("172.17.0.1:48442"),
    ];
    let wifi = [(
        "192.168.211.189".parse().unwrap(),
        "255.255.0.0".parse().unwrap(),
    )];
    assert_eq!(
        prefer_local_networks(targets.clone(), &wifi),
        vec![
            addr("192.168.91.175:48442"),
            addr("10.42.7.1:48442"),
            addr("100.76.80.115:48442"),
            addr("172.17.0.1:48442"),
        ]
    );
    // Nothing local: the order is kept. A zero netmask matches nothing.
    assert_eq!(prefer_local_networks(targets.clone(), &[]), targets);
    let any = [("0.0.0.0".parse().unwrap(), "0.0.0.0".parse().unwrap())];
    assert_eq!(prefer_local_networks(targets.clone(), &any), targets);
    // Mapped IPv4 targets count; IPv6 ones never match.
    let mapped = vec![addr("[::1]:1"), addr("[::ffff:192.168.0.9]:1")];
    assert_eq!(
        prefer_local_networks(mapped, &wifi),
        vec![addr("[::ffff:192.168.0.9]:1"), addr("[::1]:1")]
    );
    assert!(local_ipv4_networks()
        .iter()
        .all(|(ip, _)| !ip.is_loopback()));
}

#[test]
fn wifi_keepalives_stay_on_one_address_while_the_cable_carries_input() {
    let host = FakeHost::start();
    let second = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    second
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut wifi = quick_wifi(vec![host.udp_addr, second.local_addr().unwrap()], None);
    wifi.sender.ack_timeout = Duration::from_millis(200);
    let headset = Headset::start(TransportConfig {
        mode: TransportMode::Auto,
        tunnel: Some(quick_tunnel(host.tunnel_addr)),
        wifi: Some(wifi),
    });
    let reached_second = |within: Duration| {
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + within;
        let mut count = 0;
        while Instant::now() < deadline {
            if second.recv_from(&mut buf).is_ok() {
                count += 1;
            }
        }
        count
    };

    // Cable in: no video over Wi-Fi to confirm an address, yet the keepalives stay on the
    // first one instead of rotating.
    assert!(wait_until(WAIT, || headset.transport.input_path()
        == Some(InputPath::Usb)));
    let before = host.wifi_datagrams.load(Ordering::SeqCst);
    assert_eq!(reached_second(Duration::from_secs(1)), 0);
    assert!(host.wifi_datagrams.load(Ordering::SeqCst) > before + 20);

    // Cable out and no video over Wi-Fi either: the sender tries the next address, as before.
    host.unplug();
    assert!(reached_second(Duration::from_secs(2)) > 0);
}
