//! Loopback tests for the QUIC control channel. Compiled only with `--features quic`.
//!
//! quinn starts an incoming handshake only while `QuicControlServer::accept` is polled, so every
//! dial here runs concurrently with `accept`. The client sends its first control message before
//! the host's accept can return, because quinn only exposes the control stream once data is
//! written on it (the real client also opens with ClientHello).
#![cfg(feature = "quic")]

use linux_quest_protocol::{
    chunk_clipboard_text, ClientHandshake, ClipboardReassembler, ServerHandshake, VideoCodec,
    DEFAULT_CLIPBOARD_CHUNK_SIZE, MAX_CLIPBOARD_TEXT_SIZE,
};
use linux_quest_transport::quic::{QUIC_KEEP_ALIVE_INTERVAL, QUIC_MAX_IDLE_TIMEOUT};
use linux_quest_transport::{
    AuthorizedClients, ClientIdentity, ControlChannel, ControlClientSession, ControlError,
    ControlMessage, Fingerprint, HostIdentity, QuicControlServer, QuicControlSession,
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Harness {
    server: QuicControlServer,
    fingerprint: Fingerprint,
    addr: SocketAddr,
    client: ClientIdentity,
}

fn harness() -> Harness {
    let identity = HostIdentity::generate().unwrap();
    let client = ClientIdentity::generate().unwrap();
    let authorized: AuthorizedClients = [client.fingerprint()].into_iter().collect();
    let server =
        QuicControlServer::bind("127.0.0.1:0".parse().unwrap(), &identity, authorized).unwrap();
    let addr = server.local_addr().unwrap();
    Harness {
        server,
        fingerprint: identity.fingerprint(),
        addr,
        client,
    }
}

/// Dials, sends `first`, and accepts on the host. Returns both sessions after the host has read
/// `first`.
async fn open_session(
    h: &Harness,
    first: &ControlMessage,
) -> (QuicControlSession, QuicControlSession) {
    let dial = async {
        let mut client = QuicControlSession::connect(h.addr, h.fingerprint, &h.client)
            .await
            .expect("connect should succeed with the pinned fingerprint");
        client.control.send_message(first).await.unwrap();
        client
    };
    let (client, server) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(dial, h.server.accept())
    })
    .await
    .expect("session setup timed out");
    let mut server = server.unwrap();
    assert_eq!(&server.control.recv_message().await.unwrap(), first);
    (client, server)
}

fn heartbeat(sequence: u64) -> ControlMessage {
    ControlMessage::Heartbeat {
        sequence,
        timestamp_us: sequence * 1_000,
    }
}

#[tokio::test]
async fn handshake_round_trip_over_quic() {
    let h = harness();
    let client_hello = ControlMessage::ClientHello(ClientHandshake {
        client_version: 1,
        device_model: "Meta Quest 3".to_string(),
        supported_codecs: vec![VideoCodec::Av1, VideoCodec::Hevc],
        requested_displays: 1,
        max_bandwidth_mbps: 150,
        preferred_refresh_rate: 72,
    });
    let (mut client, mut server) = open_session(&h, &client_hello).await;

    let server_hello = ControlMessage::ServerHello(ServerHandshake {
        server_version: 1,
        selected_codec: VideoCodec::Av1,
        negotiated_bitrate_mbps: 120,
        displays: vec![],
    });
    server.control.send_message(&server_hello).await.unwrap();
    assert_eq!(client.control.recv_message().await.unwrap(), server_hello);
}

#[tokio::test]
async fn large_clipboard_payload_round_trip_over_quic() {
    let h = harness();
    let (mut client, mut server) = open_session(&h, &heartbeat(0)).await;

    // Fill up to the protocol's clipboard limit (4 MiB) with multi-byte UTF-8.
    let unit = "linux-quest clipboard \u{65e5}\u{672c}\u{8a9e} ";
    let mut text = String::new();
    while text.len() + unit.len() <= MAX_CLIPBOARD_TEXT_SIZE {
        text.push_str(unit);
    }
    let chunks = chunk_clipboard_text(1, 7, &text, DEFAULT_CLIPBOARD_CHUNK_SIZE).unwrap();
    assert!(chunks.len() > 1, "payload should be chunked");

    // Send and receive concurrently: the stream's flow-control window is far smaller than 4 MiB.
    let sender = tokio::spawn(async move {
        for chunk in chunks {
            client
                .control
                .send_message(&ControlMessage::Clipboard(chunk))
                .await
                .unwrap();
        }
        client
    });

    let receive = async {
        let mut reassembler = ClipboardReassembler::new(4);
        loop {
            match server.control.recv_message().await.unwrap() {
                ControlMessage::Clipboard(chunk) => {
                    let result = reassembler.ingest_chunk(chunk).unwrap();
                    if let Some(assembled) = result.into_assembled() {
                        return assembled;
                    }
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    };
    let assembled = tokio::time::timeout(TEST_TIMEOUT, receive)
        .await
        .expect("clipboard transfer timed out");

    assert_eq!(assembled.version, 7);
    assert_eq!(assembled.text.len(), text.len());
    assert!(
        assembled.text == text,
        "clipboard text corrupted in transit"
    );
    let _client = sender.await.unwrap();
}

#[tokio::test]
async fn connect_rejects_certificate_that_does_not_match_pin() {
    let h = harness();
    let wrong_pin = Fingerprint::of_der(b"not the host certificate");
    let (dial, _accept) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            QuicControlSession::connect(h.addr, wrong_pin, &h.client),
            h.server.accept()
        )
    })
    .await
    .expect("handshake should fail promptly, not hang");

    let err = match dial {
        Ok(_) => panic!("connection with a wrong pin must fail"),
        Err(err) => err,
    };
    // The failure must come from certificate verification, not from a silent timeout.
    let text = err.to_string();
    assert!(
        text.contains("pinned fingerprint"),
        "expected a pin mismatch, got: {text}"
    );
}

#[tokio::test]
async fn tcp_fallback_does_not_bypass_a_quic_pin_failure() {
    let h = harness();
    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let wrong_pin = Fingerprint::of_der(b"not the host certificate");

    let (dial, _accept) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            ControlClientSession::connect_with_tcp_fallback(h.addr, tcp_addr, wrong_pin, &h.client),
            h.server.accept()
        )
    })
    .await
    .expect("pin failure should be surfaced promptly");
    let err = match dial {
        Ok(_) => panic!("a wrong pin must fail rather than fall back"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("pinned fingerprint"),
        "expected a pinned fingerprint failure, got: {err}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), tcp_listener.accept())
            .await
            .is_err(),
        "authentication failure must not fall back to TCP"
    );
}

#[tokio::test]
async fn reconnect_after_host_drops_session() {
    let h = harness();
    let (mut client, server) = open_session(&h, &heartbeat(1)).await;

    // Host ends the session. The client learns about it as a clean close, not as a hang.
    server.close("host restart");
    let reason = tokio::time::timeout(TEST_TIMEOUT, client.closed())
        .await
        .expect("client should observe the close");
    assert!(
        matches!(reason, quinn::ConnectionError::ApplicationClosed(_)),
        "expected application close, got {reason:?}"
    );
    let err = client.control.recv_message().await.unwrap_err();
    assert!(
        matches!(err, ControlError::ConnectionClosed),
        "expected ConnectionClosed, got {err:?}"
    );

    // Dial again with the same pin. The new session must be fully usable.
    let redial = async {
        client.reconnect().await.expect("reconnect should succeed");
        client.control.send_message(&heartbeat(2)).await.unwrap();
    };
    let (_, server2) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(redial, h.server.accept())
    })
    .await
    .expect("reconnect timed out");
    let mut server2 = server2.unwrap();
    assert_eq!(server2.control.recv_message().await.unwrap(), heartbeat(2));
    server2.control.send_message(&heartbeat(3)).await.unwrap();
    assert_eq!(client.control.recv_message().await.unwrap(), heartbeat(3));
}

#[tokio::test]
async fn tcp_fallback_round_trip_when_quic_is_unavailable() {
    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let quic_addr = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let identity = ClientIdentity::generate().unwrap();
    let host_identity = HostIdentity::generate().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = tcp_listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut control = ControlChannel::new(reader, writer);
        assert_eq!(control.recv_message().await.unwrap(), heartbeat(29));
        control.send_message(&heartbeat(30)).await.unwrap();
    });

    let mut client = ControlClientSession::connect_with_tcp_fallback(
        quic_addr,
        tcp_addr,
        host_identity.fingerprint(),
        &identity,
    )
    .await
    .expect("unavailable QUIC should use the legacy TCP control listener");
    assert!(!client.is_quic());
    client.send_message(&heartbeat(29)).await.unwrap();
    assert_eq!(client.recv_message().await.unwrap(), heartbeat(30));
    server.await.unwrap();
}

#[tokio::test]
async fn client_socket_rebind_migrates_existing_connection() {
    let h = harness();
    let (mut client, mut server) = open_session(&h, &heartbeat(1)).await;
    let stable_before = client.connection().stable_id();
    let server_view_before = server.remote_address();

    // Move the client to a new UDP socket, as a Wi-Fi change would.
    let new_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let new_port = new_socket.local_addr().unwrap().port();
    client.rebind_socket(new_socket).unwrap();

    // The same connection and stream must keep working with no reconnect.
    client.control.send_message(&heartbeat(2)).await.unwrap();
    assert_eq!(server.control.recv_message().await.unwrap(), heartbeat(2));
    server.control.send_message(&heartbeat(3)).await.unwrap();
    assert_eq!(client.control.recv_message().await.unwrap(), heartbeat(3));
    assert_eq!(client.connection().stable_id(), stable_before);

    // The host should observe the new client address once quinn validates the path.
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut seq = 4;
    while server.remote_address() == server_view_before {
        assert!(
            Instant::now() < deadline,
            "host never saw the migrated address"
        );
        client.control.send_message(&heartbeat(seq)).await.unwrap();
        assert_eq!(server.control.recv_message().await.unwrap(), heartbeat(seq));
        seq += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.remote_address().port(), new_port);
}

#[tokio::test]
async fn idle_connection_survives_longer_than_idle_timeout() {
    // Keepalive PINGs must keep a silent but healthy connection alive past the idle timeout.
    assert!(QUIC_KEEP_ALIVE_INTERVAL < QUIC_MAX_IDLE_TIMEOUT);
    let h = harness();
    let (mut client, mut server) = open_session(&h, &heartbeat(1)).await;

    tokio::time::sleep(QUIC_MAX_IDLE_TIMEOUT + Duration::from_secs(2)).await;

    client.control.send_message(&heartbeat(2)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(TEST_TIMEOUT, server.control.recv_message())
            .await
            .expect("idle connection was dropped")
            .unwrap(),
        heartbeat(2)
    );
}

/// Sequential heartbeat round trips on one task. Each sample is client send, host receive, host
/// send ack, client receive, so both transports are measured the same way.
async fn heartbeat_round_trips<R1, W1, R2, W2>(
    client: &mut ControlChannel<R1, W1>,
    server: &mut ControlChannel<R2, W2>,
    count: u64,
) -> Vec<Duration>
where
    R1: AsyncRead + Unpin,
    W1: AsyncWrite + Unpin,
    R2: AsyncRead + Unpin,
    W2: AsyncWrite + Unpin,
{
    let mut samples = Vec::with_capacity(count as usize);
    for seq in 0..count {
        let started = Instant::now();
        client.send_message(&heartbeat(seq)).await.unwrap();
        match server.recv_message().await.unwrap() {
            ControlMessage::Heartbeat { sequence, .. } => {
                server
                    .send_message(&ControlMessage::HeartbeatAck {
                        sequence,
                        timestamp_us: 0,
                    })
                    .await
                    .unwrap();
            }
            other => panic!("unexpected message: {other:?}"),
        }
        match client.recv_message().await.unwrap() {
            ControlMessage::HeartbeatAck { sequence, .. } => assert_eq!(sequence, seq),
            other => panic!("unexpected message: {other:?}"),
        }
        samples.push(started.elapsed());
    }
    samples
}

fn summarize(name: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let pct = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{name:<6} n={} mean={:?} p50={:?} p90={:?} p99={:?} max={:?}",
        samples.len(),
        mean,
        pct(0.50),
        pct(0.90),
        pct(0.99),
        samples[samples.len() - 1],
    );
}

/// TCP control channel as `ControlChannel::create_connected_pair` builds it, with TCP_NODELAY on,
/// which matches the host's accepted control sockets today.
async fn tcp_pair_as_shipped() -> (
    ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
    ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
) {
    ControlChannel::create_connected_pair().await.unwrap()
}

/// Direct TCP pair with `TCP_NODELAY` set on both ends, as a socket setup sanity comparator.
async fn tcp_pair_nodelay() -> (
    ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
    ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, accepted) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
    let client = client.unwrap();
    let (server, _) = accepted.unwrap();
    client.set_nodelay(true).unwrap();
    server.set_nodelay(true).unwrap();
    let (cr, cw) = client.into_split();
    let (sr, sw) = server.into_split();
    (ControlChannel::new(cr, cw), ControlChannel::new(sr, sw))
}

/// Benchmark, not a correctness test. Run with:
/// `cargo test -p linux-quest-transport --release --features quic --test quic_control_loopback -- --ignored --nocapture quic_vs_tcp`
#[tokio::test]
#[ignore = "benchmark: run in --release with --ignored --nocapture"]
async fn quic_vs_tcp_control_round_trip_latency() {
    const WARMUP: u64 = 500;
    const SAMPLES: u64 = 5_000;
    // The shipped TCP path takes roughly 90 ms per round trip (Nagle + delayed ACK), so it gets a
    // smaller sample to keep the run short. Its percentiles are still stable at this size.
    const SHIPPED_SAMPLES: u64 = 200;

    let (mut tcp_client, mut tcp_server) = tcp_pair_as_shipped().await;
    heartbeat_round_trips(&mut tcp_client, &mut tcp_server, 5).await;
    let tcp_shipped =
        heartbeat_round_trips(&mut tcp_client, &mut tcp_server, SHIPPED_SAMPLES).await;

    let (mut nd_client, mut nd_server) = tcp_pair_nodelay().await;
    heartbeat_round_trips(&mut nd_client, &mut nd_server, WARMUP).await;
    let tcp_nodelay = heartbeat_round_trips(&mut nd_client, &mut nd_server, SAMPLES).await;

    let h = harness();
    let (mut quic_client, mut quic_server) = open_session(&h, &heartbeat(0)).await;
    heartbeat_round_trips(&mut quic_client.control, &mut quic_server.control, WARMUP).await;
    let quic =
        heartbeat_round_trips(&mut quic_client.control, &mut quic_server.control, SAMPLES).await;

    summarize("tcp*", tcp_shipped);
    summarize("tcp-nd", tcp_nodelay);
    summarize("quic", quic);
    println!("* tcp = ControlChannel::create_connected_pair (TCP_NODELAY on); tcp-nd = direct TCP pair with TCP_NODELAY on both ends");
}
