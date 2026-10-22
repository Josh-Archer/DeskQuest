//! Loopback tunnels whose host end goes silent, drops the connection or never heartbeats.

use super::*;
use crate::input_sender::CombinedInputSource;
use linux_quest_protocol::InputEvent;
use std::net::TcpListener;
use std::sync::mpsc;

fn quick(host_addr: SocketAddr) -> UsbTunnelConfig {
    UsbTunnelConfig {
        host_addr,
        connect_timeout: Duration::from_millis(200),
        retry_interval: Duration::from_millis(20),
        poll_interval: Duration::from_millis(10),
        idle_timeout: Duration::from_millis(300),
        input_interval: Duration::from_millis(2),
        keepalive_interval: Duration::from_millis(20),
    }
}

/// The host end: accepts and negotiates tunnel connections like the daemon does.
struct FakeHost {
    listener: TcpListener,
}

impl FakeHost {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self { listener }
    }

    fn addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// The next negotiated connection, or `None` if none arrives within `timeout`.
    fn accept(&self, timeout: Duration) -> Option<TcpStream> {
        let deadline = Instant::now() + timeout;
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut hello = [0u8; linux_quest_transport::USB_TUNNEL_HELLO.len()];
        stream.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, linux_quest_transport::USB_TUNNEL_HELLO);
        stream
            .write_all(linux_quest_transport::USB_TUNNEL_ACCEPT)
            .unwrap();
        Some(stream)
    }
}

fn send(stream: &mut TcpStream, frame: &TunnelFrame) {
    stream.write_all(&encode_frame(frame).unwrap()).unwrap();
}

fn heartbeat() -> TunnelFrame {
    TunnelFrame::Control(ControlMessage::Heartbeat {
        sequence: 1,
        timestamp_us: 0,
    })
}

fn video(stream_id: u16, sequence: u32) -> TunnelFrame {
    let payload = b"video";
    let header = PacketHeader::new(PacketType::VideoFrameChunk, stream_id, sequence, 0, payload);
    let packet = Packet::new(header, bytes::Bytes::from_static(payload));
    TunnelFrame::Packet(packet.to_bytes().to_vec())
}

/// Reads input batches from the headset until `done` holds for the events seen so far.
fn read_input_until(
    stream: &mut TcpStream,
    timeout: Duration,
    mut done: impl FnMut(&[InputEvent]) -> bool,
) -> Vec<InputEvent> {
    let deadline = Instant::now() + timeout;
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut decoder = TunnelFrameDecoder::new();
    let mut events = Vec::new();
    let mut batches = 0;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("read failed: {e}"),
        }
        while let Some(frame) = decoder.next_frame() {
            let TunnelFrame::Packet(bytes) = frame.unwrap() else {
                continue;
            };
            let packet = Packet::from_bytes(&bytes).unwrap();
            assert_eq!(packet.header.packet_type, PacketType::InputEvent);
            batches += 1;
            events.extend(InputBatch::decode(&packet.payload).unwrap().events);
        }
        if batches > 0 && done(&events) {
            return events;
        }
    }
    panic!("the headset did not send the expected input in time; got {events:?}");
}

struct Headset {
    tunnel: UsbTunnel,
    packets: mpsc::Receiver<Packet>,
}

impl Headset {
    fn start(config: UsbTunnelConfig) -> Self {
        let source = CombinedInputSource::new();
        let keyframes = source.requester();
        let input = TunnelInput {
            source: Arc::new(Mutex::new(source)),
            keyframes,
        };
        let (tx, packets) = mpsc::channel();
        let tunnel = UsbTunnel::spawn(config, Some(input), move |packet| {
            let _ = tx.send(packet);
        })
        .unwrap();
        Self { tunnel, packets }
    }

    /// Waits until the headset has negotiated `count` connections; the host end finishes
    /// its half of the greeting a moment before the headset counts the connection.
    fn wait_for_connections(&self, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.tunnel.connections() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(self.tunnel.connections(), count);
    }

    fn next_packet(&self) -> Packet {
        self.packets
            .recv_timeout(Duration::from_secs(2))
            .expect("no packet reached the headset")
    }
}

#[test]
fn a_host_that_goes_silent_is_detected_and_video_resumes_on_a_new_connection() {
    let host = FakeHost::bind();
    let headset = Headset::start(quick(host.addr()));

    let mut first = host
        .accept(Duration::from_secs(2))
        .expect("first connection");
    send(&mut first, &heartbeat());
    send(&mut first, &video(2, 1));
    // From here the host end sends nothing but keeps the connection open, as a wedged
    // `adb reverse` stream does.
    let silent_since = Instant::now();
    assert_eq!(headset.next_packet().header.stream_id, 2);
    // The headset keeps sending keepalives while the connection works.
    read_input_until(&mut first, Duration::from_secs(1), |_| true);

    let mut second = host
        .accept(Duration::from_secs(3))
        .expect("the headset did not reconnect after the host went silent");
    let detected_after = silent_since.elapsed();
    assert!(
        detected_after >= Duration::from_millis(250),
        "reconnected after {detected_after:?}, before the idle timeout"
    );
    headset.wait_for_connections(2);

    // The new connection asks for keyframes on every stream seen so far, so video resumes.
    let events = read_input_until(&mut second, Duration::from_secs(2), |events| {
        events.contains(&InputEvent::KeyframeRequest)
            && events.contains(&InputEvent::KeyframeRequestFor { stream_id: 2 })
    });
    assert!(!events.contains(&InputEvent::KeyframeRequestFor { stream_id: 1 }));
    send(&mut second, &video(2, 2));
    let packet = headset.next_packet();
    assert_eq!(packet.header.sequence, 2);
    drop(first);
}

#[test]
fn a_host_that_drops_the_connection_is_reconnected() {
    let host = FakeHost::bind();
    let headset = Headset::start(quick(host.addr()));

    let mut first = host
        .accept(Duration::from_secs(2))
        .expect("first connection");
    send(&mut first, &video(0, 1));
    assert_eq!(headset.next_packet().header.sequence, 1);
    drop(first);

    let mut second = host
        .accept(Duration::from_secs(2))
        .expect("the headset did not reconnect after the host closed the connection");
    send(&mut second, &video(0, 2));
    assert_eq!(headset.next_packet().header.sequence, 2);
    read_input_until(&mut second, Duration::from_secs(2), |events| {
        events.contains(&InputEvent::KeyframeRequest)
    });
}

#[test]
fn an_older_host_without_heartbeats_is_not_timed_out_but_resume_reconnects() {
    let host = FakeHost::bind();
    let config = quick(host.addr());
    let idle = config.idle_timeout;
    let headset = Headset::start(config);

    let mut first = host
        .accept(Duration::from_secs(2))
        .expect("first connection");
    send(&mut first, &video(0, 1));
    headset.next_packet();
    // A static screen on an older host: nothing is sent, and that is no reason to reconnect.
    assert!(
        host.accept(idle * 4).is_none(),
        "reconnected to a host that never promised heartbeats"
    );
    headset.wait_for_connections(1);

    // The app resumed after a pause: replace the connection even though it looks open.
    headset.tunnel.reconnect_handle().request();
    let mut second = host
        .accept(Duration::from_secs(2))
        .expect("the headset did not reconnect on request");
    headset.wait_for_connections(2);
    send(&mut second, &video(0, 2));
    assert_eq!(headset.next_packet().header.sequence, 2);
    drop(first);
}

#[test]
fn stopping_ends_the_thread_while_the_host_is_unreachable() {
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = unused.local_addr().unwrap();
    drop(unused);
    let tunnel = UsbTunnel::spawn(quick(addr), None, |_| {}).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    tunnel.stop();
    assert!(started.elapsed() < Duration::from_secs(1));
}
