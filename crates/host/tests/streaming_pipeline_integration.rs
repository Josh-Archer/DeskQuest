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
    use linux_quest_host::link_status::{new_shared_link_tracker, DEFAULT_INPUT_LIVENESS};
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

    let server_task = tokio::spawn(async move {
        run_ipc_server(
            host_clone,
            new_shared_link_tracker(DEFAULT_INPUT_LIVENESS),
            shutdown_rx,
        )
        .await
    });

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
    let second_res = run_ipc_server(
        host.clone(),
        new_shared_link_tracker(DEFAULT_INPUT_LIVENESS),
        shutdown_rx2,
    )
    .await;
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

#[tokio::test]
async fn test_audio_pipeline_streaming_opus_2_5ms() {
    use linux_quest_capture::SineAudioSource;
    use linux_quest_encoder::{AudioApplication, AudioEncoderConfig, AutoAudioEncoder};
    use linux_quest_host::HostAudioPipeline;
    use linux_quest_protocol::{AudioCodec, AudioPacket, PacketType};
    use linux_quest_transport::LoopbackEndpoint;

    let source = SineAudioSource::new_default(); // 48kHz, stereo, 2.5ms (120 samples), F32Le
    let config = AudioEncoderConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48000,
        channels: 2,
        frame_duration_us: 2500,
        bitrate_bps: 96_000,
        fec_enabled: true,
        dtx_enabled: false,
        application: AudioApplication::RestrictedLowDelay,
    };
    let encoder = AutoAudioEncoder::new(config.clone());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();

    let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);
    pipeline
        .init(config)
        .await
        .expect("Audio pipeline init failed");

    // Stream 10 frames of 2.5ms Opus audio
    for expected_id in 1..=10 {
        let frame_id = pipeline
            .step_stream_audio_frame()
            .await
            .expect("Step audio failed");
        assert_eq!(frame_id, expected_id);
    }

    for expected_id in 1..=10 {
        let pkt = ep_client
            .recv_packet()
            .await
            .expect("Client recv audio failed");
        assert_eq!(pkt.header.packet_type, PacketType::AudioFrame);
        assert_eq!(pkt.header.stream_id, 1);
        assert_eq!(pkt.header.sequence, expected_id as u32);
        let expected_pts = (expected_id - 1) * 2500;
        assert_eq!(pkt.header.timestamp_us, expected_pts);

        let audio = AudioPacket::deserialize(&pkt.payload).expect("Deserialize audio failed");
        assert_eq!(audio.meta.frame_id, expected_id);
        assert_eq!(audio.meta.sequence, expected_id as u32);
        assert_eq!(audio.meta.sample_rate, 48000);
        assert_eq!(audio.meta.channels, 2);
        assert_eq!(audio.meta.samples_per_channel, 120);
        assert_eq!(audio.meta.codec, AudioCodec::Opus);
        assert_eq!(audio.meta.pts_us, expected_pts);
        assert_eq!(audio.duration_us(), 2500);
        assert!(!audio.payload.is_empty());
    }
}

#[tokio::test]
async fn test_audio_pipeline_streaming_5ms_frames() {
    use linux_quest_capture::{AudioSampleFormat, SineAudioSource};
    use linux_quest_encoder::{AudioApplication, AudioEncoderConfig, AutoAudioEncoder};
    use linux_quest_host::HostAudioPipeline;
    use linux_quest_protocol::{AudioCodec, AudioPacket, PacketType};
    use linux_quest_transport::LoopbackEndpoint;

    let source = SineAudioSource::new(48000, 2, AudioSampleFormat::I16Le, 240); // 5ms = 240 samples @ 48kHz
    let config = AudioEncoderConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48000,
        channels: 2,
        frame_duration_us: 5000,
        bitrate_bps: 128_000,
        fec_enabled: true,
        dtx_enabled: false,
        application: AudioApplication::RestrictedLowDelay,
    };
    let encoder = AutoAudioEncoder::new(config.clone());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();

    let mut pipeline = HostAudioPipeline::new(source, encoder, ep_host, 1);
    pipeline
        .init(config)
        .await
        .expect("Audio pipeline init failed");

    for expected_id in 1..=5 {
        let frame_id = pipeline
            .step_stream_audio_frame()
            .await
            .expect("Step audio failed");
        assert_eq!(frame_id, expected_id);

        let pkt = ep_client
            .recv_packet()
            .await
            .expect("Client recv audio failed");
        assert_eq!(pkt.header.packet_type, PacketType::AudioFrame);
        let expected_pts = (expected_id - 1) * 5000;
        assert_eq!(pkt.header.timestamp_us, expected_pts);

        let audio = AudioPacket::deserialize(&pkt.payload).expect("Deserialize audio failed");
        assert_eq!(audio.meta.frame_id, expected_id);
        assert_eq!(audio.meta.samples_per_channel, 240);
        assert_eq!(audio.duration_us(), 5000);
    }
}

#[tokio::test]
async fn test_synchronized_audio_and_video_pipeline() {
    use linux_quest_capture::{SineAudioSource, SyntheticCapture};
    use linux_quest_encoder::{
        AudioEncoderConfig, AutoAudioEncoder, EncoderConfig, MockVideoEncoder,
    };
    use linux_quest_host::{HostAudioPipeline, HostStreamSession};
    use linux_quest_input::MockInputInjector;
    use linux_quest_protocol::{AudioPacket, PacketType, VideoChunk};
    use linux_quest_transport::LoopbackEndpoint;

    // Both video and audio are packetized and sent to the client.
    // Video: 60Hz (~16,666us interval).
    // Audio: 2.5ms (2,500us interval).
    let (video_tx, mut client_video_rx) = LoopbackEndpoint::create_pair();
    let (audio_tx, mut client_audio_rx) = LoopbackEndpoint::create_pair();

    let video_capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let video_encoder = MockVideoEncoder::new(EncoderConfig::default());
    let mut video_session = HostStreamSession::new(
        video_capture,
        video_encoder,
        video_tx,
        MockInputInjector::new(),
        0,
    );
    video_session.init(EncoderConfig::default()).await.unwrap();

    let audio_source = SineAudioSource::new_default();
    let audio_config = AudioEncoderConfig::default();
    let audio_encoder = AutoAudioEncoder::new(audio_config.clone());
    let mut audio_pipeline = HostAudioPipeline::new(audio_source, audio_encoder, audio_tx, 1);
    audio_pipeline.init(audio_config).await.unwrap();

    // Stream 1 video frame and 6 audio frames (covering ~15ms of video presentation)
    let vid_fid = video_session.step_stream_frame().await.unwrap();
    assert_eq!(vid_fid, 1);

    for _ in 0..6 {
        audio_pipeline.step_stream_audio_frame().await.unwrap();
    }

    // Client verifies video packet
    let vid_pkt = client_video_rx.recv_packet().await.unwrap();
    assert_eq!(vid_pkt.header.packet_type, PacketType::VideoFrameChunk);
    let video_chunk = VideoChunk::deserialize(&vid_pkt.payload).unwrap();
    let video_pts = video_chunk.meta.pts_us;

    // Client verifies audio packets and A/V sync clock
    for i in 0..6 {
        let aud_pkt = client_audio_rx.recv_packet().await.unwrap();
        assert_eq!(aud_pkt.header.packet_type, PacketType::AudioFrame);
        let audio_pkt = AudioPacket::deserialize(&aud_pkt.payload).unwrap();
        let audio_pts = audio_pkt.meta.pts_us;

        // Audio timestamps progress at 2500us steps
        assert_eq!(audio_pts, i * 2500);

        // Skew between audio and video timestamp is measurable and bounded
        let diff = (audio_pts as i64) - (video_pts as i64);
        assert!(diff.abs() < 20_000); // within 20ms
    }
}

#[tokio::test]
async fn test_audio_pipeline_over_tcp_and_udp() {
    use linux_quest_capture::SineAudioSource;
    use linux_quest_encoder::{AudioEncoderConfig, AutoAudioEncoder};
    use linux_quest_host::HostAudioPipeline;
    use linux_quest_protocol::{AudioPacket, PacketType};
    use linux_quest_transport::{PacedUdpEndpoint, TcpEndpoint};

    // 1. Over TCP endpoint
    let (tcp_host, mut tcp_client) = TcpEndpoint::create_connected_pair().await.unwrap();
    let source_tcp = SineAudioSource::new_default();
    let mut tcp_pipeline = HostAudioPipeline::new(
        source_tcp,
        AutoAudioEncoder::new(AudioEncoderConfig::default()),
        tcp_host,
        1,
    );
    tcp_pipeline
        .init(AudioEncoderConfig::default())
        .await
        .unwrap();

    let fid = tcp_pipeline.step_stream_audio_frame().await.unwrap();
    assert_eq!(fid, 1);

    let pkt = tcp_client.recv_packet().await.unwrap();
    assert_eq!(pkt.header.packet_type, PacketType::AudioFrame);
    let audio = AudioPacket::deserialize(&pkt.payload).unwrap();
    assert_eq!(audio.meta.frame_id, 1);

    // 2. Over Paced UDP endpoint
    let (udp_host, mut udp_client) = PacedUdpEndpoint::create_connected_pair(100).await.unwrap();
    let source_udp = SineAudioSource::new_default();
    let mut udp_pipeline = HostAudioPipeline::new(
        source_udp,
        AutoAudioEncoder::new(AudioEncoderConfig::default()),
        udp_host,
        1,
    );
    udp_pipeline
        .init(AudioEncoderConfig::default())
        .await
        .unwrap();

    let fid_udp = udp_pipeline.step_stream_audio_frame().await.unwrap();
    assert_eq!(fid_udp, 1);

    let pkt_udp = udp_client.recv_packet().await.unwrap();
    assert_eq!(pkt_udp.header.packet_type, PacketType::AudioFrame);
    let audio_udp = AudioPacket::deserialize(&pkt_udp.payload).unwrap();
    assert_eq!(audio_udp.meta.frame_id, 1);
}
