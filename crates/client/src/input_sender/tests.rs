//! Loopback tests against the real host-side `InputChannelReceiver`.

use std::sync::Mutex;

use tokio::sync::mpsc;

use linux_quest_protocol::{ElementState, KeyModifiers};
use linux_quest_transport::InputChannelReceiver;

use super::*;

/// One datagram as the host receiver saw it.
struct Got {
    events: Vec<InputEvent>,
    seq: u64,
    peer: Option<SocketAddr>,
    at: Instant,
}

/// A real `InputChannelReceiver` on loopback, draining into a channel.
struct Host {
    addr: SocketAddr,
    rx: mpsc::UnboundedReceiver<Got>,
    task: JoinHandle<()>,
}

impl Host {
    async fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let mut receiver = InputChannelReceiver::new(socket, None, true);
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Ok(events) = receiver.recv_events().await {
                let got = Got {
                    events,
                    seq: receiver.highest_sequence_seen(),
                    peer: receiver.remote_addr(),
                    at: Instant::now(),
                };
                if tx.send(got).is_err() {
                    break;
                }
            }
        });
        Self { addr, rx, task }
    }

    async fn next(&mut self) -> Got {
        tokio::time::timeout(Duration::from_secs(5), self.rx.recv())
            .await
            .expect("timed out waiting for a datagram")
            .expect("host receiver ended")
    }

    /// Next datagram that carries events (skips keepalives).
    async fn next_with_events(&mut self) -> Got {
        loop {
            let g = self.next().await;
            if !g.events.is_empty() {
                return g;
            }
        }
    }

    fn drain(&mut self) -> usize {
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Test source fed through a shared queue.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<VecDeque<Vec<InputEvent>>>>);

impl Script {
    fn push(&self, events: Vec<InputEvent>) {
        self.0.lock().unwrap().push_back(events);
    }
}

impl InputSource for Script {
    fn poll(&mut self) -> Vec<InputEvent> {
        self.0.lock().unwrap().pop_front().unwrap_or_default()
    }
}

fn fast(targets: Vec<SocketAddr>) -> InputSenderConfig {
    InputSenderConfig {
        keepalive_interval: Duration::from_millis(20),
        poll_interval: Duration::from_millis(5),
        ack_timeout: Duration::from_millis(100),
        ..InputSenderConfig::new(targets)
    }
}

fn mouse(dx: i32) -> InputEvent {
    InputEvent::MouseMoveRelative { dx, dy: 0 }
}

fn key(code: u32, state: ElementState) -> InputEvent {
    InputEvent::KeyboardKey {
        keycode: code,
        state,
        modifiers: KeyModifiers::empty(),
    }
}

#[tokio::test]
async fn keepalives_latch_the_peer_with_consecutive_sequences() {
    let mut host = Host::start().await;
    let sender = InputSender::spawn(fast(vec![host.addr]), Box::new(KeepaliveOnlySource));

    let mut seqs = Vec::new();
    let mut peer = None;
    for _ in 0..4 {
        let g = host.next().await;
        assert!(g.events.is_empty(), "idle datagrams are empty keepalives");
        seqs.push(g.seq);
        peer = g.peer;
    }
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    let peer = peer.expect("host latched a peer");
    assert_eq!(peer.ip(), IpAddr::from([127, 0, 0, 1]));
    assert_ne!(peer.port(), host.addr.port());
    assert!(sender.control().stats().keepalives_sent >= 4);
    sender.stop().await;
}

#[tokio::test]
async fn events_arrive_in_order_with_sequence_numbers() {
    let mut host = Host::start().await;
    let script = Script::default();
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(10),
        ..fast(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await; // initial keepalive latches the peer

    let mut last_seq = 0;
    for dx in 1..=5 {
        script.push(vec![mouse(dx)]);
        sender.control().notify();
        let g = host.next_with_events().await;
        assert_eq!(g.events, vec![mouse(dx)]);
        assert!(g.seq > last_seq, "sequence must increase");
        last_seq = g.seq;
    }
    assert_eq!(sender.control().stats().last_sequence, last_seq);
    sender.stop().await;
}

#[tokio::test]
async fn key_events_are_delivered_once_despite_redundancy() {
    let mut host = Host::start().await;
    let script = Script::default();
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(10),
        ..fast(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await;

    script.push(vec![key(30, ElementState::Pressed)]);
    sender.control().notify();
    let down = host.next_with_events().await;
    script.push(vec![key(30, ElementState::Released)]);
    sender.control().notify();
    let up = host.next_with_events().await;
    assert_eq!(down.events, vec![key(30, ElementState::Pressed)]);
    assert_eq!(up.events, vec![key(30, ElementState::Released)]);
    sender.stop().await;
}

#[tokio::test]
async fn state_change_is_sent_within_one_tick() {
    let mut host = Host::start().await;
    let script = Script::default();
    // Keepalive far away: only the change path can deliver this in time.
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(30),
        poll_interval: Duration::from_secs(30),
        ..fast(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await;

    script.push(vec![mouse(7)]);
    sender.control().notify();
    let g = tokio::time::timeout(Duration::from_millis(100), host.next())
        .await
        .expect("change sent immediately after notify");
    assert_eq!(g.events, vec![mouse(7)]);
    sender.stop().await;

    // Polling path: no notify, one poll tick.
    let mut host = Host::start().await;
    let script = Script::default();
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(30),
        poll_interval: Duration::from_millis(5),
        ..fast(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await;
    script.push(vec![mouse(9)]);
    let g = tokio::time::timeout(Duration::from_millis(200), host.next())
        .await
        .expect("change picked up by the poll tick");
    assert_eq!(g.events, vec![mouse(9)]);
    sender.stop().await;
}

#[tokio::test]
async fn burst_of_events_is_split_into_bounded_datagrams() {
    let mut host = Host::start().await;
    let script = Script::default();
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(30),
        ..fast(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await;
    let burst: Vec<InputEvent> = (0..(MAX_EVENTS_PER_DATAGRAM as i32 + 8))
        .map(mouse)
        .collect();
    script.push(burst.clone());
    sender.control().notify();
    let a = host.next_with_events().await;
    let b = host.next_with_events().await;
    assert_eq!(a.events.len(), MAX_EVENTS_PER_DATAGRAM);
    let mut all = a.events;
    all.extend(b.events);
    assert_eq!(all, burst);
    sender.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_keepalives_hold_the_link_across_more_than_five_seconds() {
    let mut host = Host::start().await;
    let sender = InputSender::spawn(
        InputSenderConfig::new(vec![host.addr]),
        Box::new(KeepaliveOnlySource),
    );

    let mut times = vec![host.next().await.at];
    let mut peer = None;
    let deadline = times[0] + Duration::from_secs(6);
    while *times.last().unwrap() < deadline {
        let g = host.next().await;
        times.push(g.at);
        peer = g.peer;
    }
    assert!(peer.is_some(), "peer stays latched");
    let max_gap = times.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        max_gap < Duration::from_millis(250),
        "keepalive gap {max_gap:?} must stay far below the 5 s host timeout"
    );
    assert!(times.len() >= 55, "~10 Hz for 6 s, got {}", times.len());
    sender.stop().await;
}

#[tokio::test]
async fn retarget_moves_traffic_to_the_new_host() {
    let mut a = Host::start().await;
    let mut b = Host::start().await;
    let sender = InputSender::spawn(fast(vec![a.addr]), Box::new(KeepaliveOnlySource));
    a.next().await;
    let seq_before = sender.control().stats().last_sequence;

    sender.control().retarget(vec![b.addr]);
    let g = b.next().await;
    assert!(g.seq >= 1);
    // Let anything in flight to A land, then A must go quiet.
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.drain();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(a.drain(), 0, "old host must stop receiving");
    assert!(
        sender.control().stats().last_sequence > seq_before,
        "same socket and sequence continue across a same-family retarget"
    );
    sender.stop().await;
}

#[tokio::test]
async fn falls_back_to_the_next_address_when_unacked_and_sticks_when_acked() {
    // A bound-then-dropped port: sends succeed, nothing answers.
    let dead = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap()
    };
    let mut live = Host::start().await;
    let sender = InputSender::spawn(fast(vec![dead, live.addr]), Box::new(KeepaliveOnlySource));

    live.next().await; // reached after the ack timeout rotated past `dead`
    assert_eq!(sender.control().stats().target_index, 1);

    // Acks from now on keep us there well past several ack windows.
    let ctl = sender.control().clone();
    let acker = tokio::spawn(async move {
        loop {
            ctl.ack();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    live.drain();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sender.control().stats().target_index, 1);
    assert!(live.drain() > 10, "still streaming to the acked address");
    acker.abort();
    sender.stop().await;
}

/// Sending to the IPv4 limited broadcast address without SO_BROADCAST fails with EACCES.
#[tokio::test(start_paused = true)]
async fn send_errors_back_off_instead_of_spinning() {
    let bad = SocketAddr::from(([255, 255, 255, 255], 48442));
    let sender = InputSender::spawn(
        InputSenderConfig::new(vec![bad]),
        Box::new(KeepaliveOnlySource),
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    let stats = sender.control().stats();
    assert!(stats.failing, "{stats:?}");
    assert_eq!(stats.datagrams_sent, 0);
    // 100, 200, 400, 800, 1600, then 2000 ms steps: about 8 attempts in 10 s, not thousands.
    assert!(
        (3..=12).contains(&stats.send_errors),
        "attempts must be bounded by the backoff: {stats:?}"
    );
    sender.stop().await;
}

#[tokio::test]
async fn recovers_after_a_retarget_from_an_unreachable_address() {
    let mut host = Host::start().await;
    let bad = SocketAddr::from(([255, 255, 255, 255], 48442));
    let sender = InputSender::spawn(fast(vec![bad]), Box::new(KeepaliveOnlySource));
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(sender.control().stats().send_errors >= 1);

    sender.control().retarget(vec![host.addr]);
    host.next().await;
    assert!(!sender.control().stats().failing);
    sender.stop().await;
}

#[tokio::test]
async fn shutdown_stops_the_task_and_the_traffic() {
    let mut host = Host::start().await;
    let sender = InputSender::spawn(fast(vec![host.addr]), Box::new(KeepaliveOnlySource));
    host.next().await;

    tokio::time::timeout(Duration::from_secs(2), sender.stop())
        .await
        .expect("task ends after shutdown");
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.drain();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(host.drain(), 0, "no datagrams after shutdown");
}

#[test]
fn thread_variant_starts_and_stops_cleanly() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let addr = socket.local_addr().unwrap();
    let thread = InputSenderThread::spawn(fast(vec![addr]), Box::new(KeepaliveOnlySource)).unwrap();
    let mut buf = [0u8; 2048];
    let (len, _) = socket.recv_from(&mut buf).unwrap();
    assert!(linux_quest_protocol::InputBatch::decode(&buf[..len]).is_ok());
    thread.stop();
    drop(rt);
}

#[test]
fn host_targets_are_bounded_deduplicated_and_canonical() {
    let host = IpAddr::from([192, 168, 1, 20]);
    let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
    let extras: Vec<IpAddr> = (1..=10u8).map(|i| IpAddr::from([10, 0, 0, i])).collect();
    let t = host_targets(host, &[mapped, extras[0], extras[0]], 48442);
    assert_eq!(
        t,
        vec![
            SocketAddr::from(([192, 168, 1, 20], 48442)),
            SocketAddr::from(([10, 0, 0, 1], 48442)),
        ]
    );
    assert_eq!(host_targets(host, &extras, 1234).len(), MAX_TARGETS);
    assert!(host_targets(IpAddr::from([0, 0, 0, 0]), &[], 48442).is_empty());
}

#[test]
fn sender_config_follows_transport_mode_and_input_port() {
    let udp = ClientConfig {
        transport_mode: TransportMode::Udp,
        host_addr: SocketAddr::from(([192, 168, 1, 20], 48444)),
        extra_host_ips: vec![IpAddr::from([10, 0, 0, 7])],
        input_port: 5555,
        ..Default::default()
    };
    let cfg = input_sender_config(&udp).unwrap();
    assert_eq!(
        cfg.targets,
        vec![
            SocketAddr::from(([192, 168, 1, 20], 5555)),
            SocketAddr::from(([10, 0, 0, 7], 5555)),
        ]
    );
    assert_eq!(
        input_sender_config(&ClientConfig::default()).map(|_| ()),
        None,
        "adb reverse default: no UDP sender"
    );
    let adb = ClientConfig {
        transport_mode: TransportMode::AdbReverseTcp,
        ..udp.clone()
    };
    assert!(input_sender_config(&adb).is_none());
    let unset = ClientConfig {
        host_addr: SocketAddr::from(([0, 0, 0, 0], 48444)),
        ..udp
    };
    assert!(input_sender_config(&unset).is_none());
    assert_eq!(ClientConfig::default().input_port, 48442);
}

#[cfg(unix)]
#[tokio::test]
async fn tos_is_applied_to_the_socket() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    set_tos(&socket, false, DEFAULT_TOS).unwrap();
    use std::os::fd::AsRawFd;
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid fd, and `value`/`len` are correctly sized out-parameters.
    let rc = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(value, DEFAULT_TOS as libc::c_int);
}

/// Reports notify-to-datagram latency on loopback. Run with
/// `cargo test -p linux-quest-client --release -- --ignored --nocapture input_sender_latency`.
#[tokio::test]
#[ignore = "timing measurement, prints a report"]
async fn input_sender_latency() {
    let mut host = Host::start().await;
    let script = Script::default();
    let cfg = InputSenderConfig {
        keepalive_interval: Duration::from_secs(30),
        poll_interval: Duration::from_secs(30),
        ..InputSenderConfig::new(vec![host.addr])
    };
    let sender = InputSender::spawn(cfg, Box::new(script.clone()));
    host.next().await;

    let mut samples = Vec::new();
    for i in 0..500 {
        script.push(vec![mouse(i)]);
        let t0 = Instant::now();
        sender.control().notify();
        let g = host.next_with_events().await;
        samples.push(g.at.duration_since(t0));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    samples.sort();
    let pct = |p: usize| samples[samples.len() * p / 100];
    println!(
        "notify -> host-receiver latency over {} samples: p50 {:?}, p90 {:?}, p99 {:?}, max {:?}",
        samples.len(),
        pct(50),
        pct(90),
        pct(99),
        samples.last().unwrap()
    );
    sender.stop().await;
}

#[test]
fn keyframe_requests_are_per_stream() {
    let mut source = KeyframeRequestSource::new();
    let flags = source.requester();
    assert!(source.poll().is_empty());
    flags.raise(2);
    flags.raise(0);
    flags.raise(2);
    flags.raise(40); // no such stream bit: ignored
    assert_eq!(
        source.poll(),
        vec![
            InputEvent::KeyframeRequest,
            InputEvent::KeyframeRequestFor { stream_id: 2 },
        ]
    );
    assert!(source.poll().is_empty());
}
