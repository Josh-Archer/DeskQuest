use bytes::Bytes;
use linux_quest_capture::SyntheticCapture;
use linux_quest_encoder::{EncoderConfig, MockVideoEncoder};
use linux_quest_host::HostStreamSession;
use linux_quest_input::MockInputInjector;
use linux_quest_protocol::{
    ElementState, FrameReassembler, InputEvent, KeyModifiers, Packet, PacketHeader, PacketType,
    VideoChunk, VideoCodec,
};
use linux_quest_transport::{LoopbackEndpoint, TransportEndpoint};

#[tokio::test]
async fn test_full_pipeline_multi_frame_streaming_and_reassembly() {
    let capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let encoder = MockVideoEncoder::new(EncoderConfig {
        codec: VideoCodec::Av1,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 50_000,
        intra_refresh_period: 5,
        max_chunk_size: 1024,
    });

    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
    session
        .init(EncoderConfig {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 50_000,
            intra_refresh_period: 5,
            max_chunk_size: 1024,
        })
        .await
        .expect("Session init failed");

    let mut reassembler = FrameReassembler::new(8);

    // Stream 10 consecutive frames
    for expected_frame_id in 1..=10 {
        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Frame step failed");
        assert_eq!(frame_id, expected_frame_id);
    }

    // Client receives all packets and reassembles them
    let mut completed_frames = 0;
    while let Ok(packet) = ep_client.recv_packet().await {
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);

        let chunk = VideoChunk::deserialize(&packet.payload).expect("Chunk deserialization failed");
        if let Some(frame) = reassembler.ingest_chunk(chunk).expect("Reassembly failed") {
            completed_frames += 1;
            assert_eq!(frame.meta.frame_id, completed_frames);
            assert_eq!(frame.meta.codec, VideoCodec::Av1);
            if completed_frames == 1 {
                assert!(frame.meta.is_keyframe);
            }
        }

        if completed_frames == 10 {
            break;
        }
    }

    assert_eq!(completed_frames, 10);
}

#[tokio::test]
async fn test_bidirectional_control_and_input_flow() {
    let capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let encoder = MockVideoEncoder::new(EncoderConfig::default());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
    session
        .init(EncoderConfig::default())
        .await
        .expect("Session init failed");

    // 1. Client sends Ping, Host responds with Pong
    let ping_payload = Bytes::from_static(b"latency-probe-timestamp");
    let ping_packet = Packet::new(
        PacketHeader::new(PacketType::Ping, 0, 1, 555_000, &ping_payload),
        ping_payload.clone(),
    );

    session
        .handle_incoming_packet(ping_packet)
        .await
        .expect("Handle ping failed");

    let pong_response = ep_client.recv_packet().await.expect("Recv pong failed");
    assert_eq!(pong_response.header.packet_type, PacketType::Pong);
    assert_eq!(pong_response.payload, ping_payload);

    // 2. Client sends Input Events (Key press and Mouse Move)
    let key_event = InputEvent::KeyboardKey {
        keycode: 30, // KEY_A
        state: ElementState::Pressed,
        modifiers: KeyModifiers::empty(),
    };
    let key_payload = Bytes::from(bincode::serialize(&key_event).unwrap());
    let key_packet = Packet::new(
        PacketHeader::new(PacketType::InputEvent, 0, 2, 600_000, &key_payload),
        key_payload,
    );

    session
        .handle_incoming_packet(key_packet)
        .await
        .expect("Handle key event failed");

    let mouse_event = InputEvent::MouseMoveRelative { dx: 15, dy: -20 };
    let mouse_payload = Bytes::from(bincode::serialize(&mouse_event).unwrap());
    let mouse_packet = Packet::new(
        PacketHeader::new(PacketType::InputEvent, 0, 3, 601_000, &mouse_payload),
        mouse_payload,
    );

    session
        .handle_incoming_packet(mouse_packet)
        .await
        .expect("Handle mouse event failed");

    // 3. Client sends Reference Picture Invalidation (RPI) feedback carrying frame pts_us
    let rpi_pts_us = 602_000u64;
    let rpi_payload = Bytes::copy_from_slice(&rpi_pts_us.to_be_bytes());
    let rpi_packet = Packet::new(
        PacketHeader::new(
            PacketType::ReferencePictureInvalidation,
            0,
            4,
            602_000,
            &rpi_payload,
        ),
        rpi_payload,
    );

    session
        .handle_incoming_packet(rpi_packet)
        .await
        .expect("Handle RPI failed");

    assert_eq!(session.encoder().last_rpi_pts(), Some(602_000));
}

#[tokio::test]
async fn test_auto_hardware_pipeline_stream() {
    let capture = linux_quest_capture::AutoCapture::new(0, 1920, 1080, 90);
    let encoder_config = linux_quest_encoder::EncoderConfig {
        codec: VideoCodec::Av1,
        width: 1920,
        height: 1080,
        fps: 90,
        bitrate_kbps: 100_000,
        intra_refresh_period: 30,
        max_chunk_size: 1400,
    };
    let encoder = linux_quest_encoder::AutoVideoEncoder::new(encoder_config.clone());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
    session
        .init(encoder_config)
        .await
        .expect("Session init failed");

    let frame_id = session
        .step_stream_frame()
        .await
        .expect("Hardware stream frame failed");
    assert_eq!(frame_id, 1);

    let packet = ep_client.recv_packet().await.expect("Client recv failed");
    assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    assert_ne!(packet.header.flags & linux_quest_protocol::FLAG_KEYFRAME, 0);

    let chunk = VideoChunk::deserialize(&packet.payload).expect("Chunk deserialization failed");
    assert_eq!(chunk.meta.frame_id, 1);
    assert!(chunk.meta.is_keyframe);
    assert_eq!(chunk.meta.codec, VideoCodec::Av1);
    assert!(!chunk.payload.is_empty());

    if linux_quest_encoder::NvencEncoder::is_available() {
        let obu_type = (chunk.payload[0] >> 3) & 0x0f;
        assert!(
            obu_type == 1 || obu_type == 2,
            "Hardware AV1 stream must start with Sequence Header or Temporal Delimiter OBU, got {obu_type}"
        );
    }
}

#[tokio::test]
async fn test_display_config_packet_lifecycle() {
    use linux_quest_display::backend::mock::MockDisplayBackend;
    use linux_quest_host::MultiDisplayHost;
    use linux_quest_protocol::{DisplayConfigMessage, PacketHeader};
    use std::sync::Arc;

    let capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let encoder_cfg = EncoderConfig {
        codec: VideoCodec::Av1,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 50_000,
        intra_refresh_period: 5,
        max_chunk_size: 1024,
    };
    let encoder = MockVideoEncoder::new(encoder_cfg);
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let display_host = Arc::new(MultiDisplayHost::new(Box::new(MockDisplayBackend::new())));

    let mut session =
        HostStreamSession::new(capture, encoder, ep_host, input, 0).with_display_host(display_host);

    // 1. Client requests 2 monitors at 2560x1440@90Hz with 120 DPI
    let req = DisplayConfigMessage::SetMonitorCount {
        count: 2,
        width: 2560,
        height: 1440,
        refresh_rate: 90,
        dpi: 120,
        layout_mode: 0, // Horizontal
    };
    let req_bytes = bincode::serialize(&req).unwrap();
    let req_packet = Packet::new(
        PacketHeader::new(PacketType::DisplayConfig, 0, 1, 1000, &req_bytes),
        req_bytes.into(),
    );

    session.handle_incoming_packet(req_packet).await.unwrap();

    // Client receives response packet
    let resp_packet = ep_client.recv_packet().await.unwrap();
    assert_eq!(resp_packet.header.packet_type, PacketType::DisplayConfig);

    let resp_msg: DisplayConfigMessage = bincode::deserialize(&resp_packet.payload).unwrap();
    match resp_msg {
        DisplayConfigMessage::ActiveMonitors(monitors) => {
            assert_eq!(monitors.len(), 2);
            assert_eq!(monitors[0].display_id, 1);
            assert_eq!(monitors[0].width, 2560);
            assert_eq!(monitors[0].height, 1440);
            assert_eq!(monitors[0].refresh_rate, 90);
            assert_eq!(monitors[0].dpi, 120);
            assert_eq!(monitors[1].display_id, 2);
            assert_eq!(monitors[1].width, 2560);
            assert_eq!(monitors[1].height, 1440);
            assert_eq!(monitors[1].dpi, 120);
        }
        other => panic!("Expected ActiveMonitors response, got {other:?}"),
    }

    // 2. Client requests invalid monitor count (4 monitors > 3 max)
    let invalid_req = DisplayConfigMessage::SetMonitorCount {
        count: 4,
        width: 1920,
        height: 1080,
        refresh_rate: 60,
        dpi: 96,
        layout_mode: 0,
    };
    let invalid_bytes = bincode::serialize(&invalid_req).unwrap();
    let invalid_packet = Packet::new(
        PacketHeader::new(PacketType::DisplayConfig, 0, 2, 2000, &invalid_bytes),
        invalid_bytes.into(),
    );

    // Session does NOT return error or crash; it catches error and replies with Error message
    session
        .handle_incoming_packet(invalid_packet)
        .await
        .unwrap();

    let err_packet = ep_client.recv_packet().await.unwrap();
    assert_eq!(err_packet.header.packet_type, PacketType::DisplayConfig);

    let err_msg: DisplayConfigMessage = bincode::deserialize(&err_packet.payload).unwrap();
    match err_msg {
        DisplayConfigMessage::Error(err_text) => {
            assert!(err_text.contains("Maximum virtual monitors exceeded"));
        }
        other => panic!("Expected Error response message, got {other:?}"),
    }
}

#[tokio::test]
async fn test_daemon_ipc_single_owner_lock() {
    use linux_quest_display::backend::mock::MockDisplayBackend;
    use linux_quest_host::{
        daemon_pid_path, daemon_runtime_dir, daemon_socket_path, run_ipc_server, MultiDisplayHost,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let test_dir = std::env::temp_dir().join(format!("lqst-ipc-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);
    let old_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    std::env::set_var("XDG_RUNTIME_DIR", &test_dir);

    let host = Arc::new(MultiDisplayHost::new(Box::new(MockDisplayBackend::new())));

    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let host_clone = host.clone();

    let server_task = tokio::spawn(async move { run_ipc_server(host_clone, shutdown_rx).await });

    let socket_path = daemon_socket_path();
    let pid_path = daemon_pid_path();

    // Poll until server binds socket
    let mut bound = false;
    for _ in 0..40 {
        if socket_path.exists() && pid_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
    }
    assert!(bound, "Daemon server failed to bind socket in time");

    // Verify socket and pid files exist and have mode 0600, directory has mode 0700
    let runtime_dir = daemon_runtime_dir();

    assert!(socket_path.exists(), "Socket path must exist");
    assert!(pid_path.exists(), "PID lock file must exist");
    let dir_perms = std::fs::metadata(&runtime_dir).unwrap().permissions();
    assert_eq!(
        dir_perms.mode() & 0o777,
        0o700,
        "Runtime directory must have 0700 permissions"
    );
    let sock_perms = std::fs::metadata(&socket_path).unwrap().permissions();
    assert_eq!(
        sock_perms.mode() & 0o777,
        0o600,
        "Socket file must have 0600 permissions"
    );
    let pid_perms = std::fs::metadata(&pid_path).unwrap().permissions();
    assert_eq!(
        pid_perms.mode() & 0o777,
        0o600,
        "PID file must have 0600 permissions"
    );

    // Attempt to start a second daemon server on the same socket
    let (_shutdown_tx2, shutdown_rx2) = tokio::sync::broadcast::channel(1);
    let second_res = run_ipc_server(host.clone(), shutdown_rx2).await;
    assert!(
        second_res.is_err(),
        "Second daemon instance must be rejected"
    );
    let err_str = second_res.unwrap_err().to_string();
    assert!(
        err_str.contains("holds the PID file lock"),
        "Expected PID lock error, got: {err_str}"
    );

    // Assert that the active socket was NOT unlinked by the rejected second instance
    assert!(
        socket_path.exists(),
        "Socket must remain active after rejection of duplicate instance"
    );

    // Shutdown first server
    let _ = shutdown_tx.send(());
    let _ = server_task.await;

    let _ = std::fs::remove_dir_all(&test_dir);
    if let Some(old) = old_xdg {
        std::env::set_var("XDG_RUNTIME_DIR", old);
    } else {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}
