use std::time::{Duration, Instant};

use bytes::Bytes;
use linux_quest_client::config::{ClientConfig, ClientVideoCodec, DisplayMode};
use linux_quest_client::runtime::QuestClientRuntime;
use linux_quest_protocol::packet::{Packet, PacketHeader, PacketType};
use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};

#[test]
fn test_client_runtime_lifecycle_with_av1() {
    let config = ClientConfig {
        codec: ClientVideoCodec::Av1,
        display_mode: DisplayMode::CurvedCylinder,
        target_refresh_rate: 90.0,
        ..Default::default()
    };

    let mut runtime = QuestClientRuntime::new(config).expect("Failed to create client runtime");
    assert!(runtime.is_running());
    assert_eq!(runtime.decoder_stats().frames_rendered, 0);

    // Synthesize an AV1 frame packet
    let av1_frame_data = vec![
        0x0A, 0x0B, 0x00, 0x00, 0x00, 0x1E, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, // Seq header
        0x32, 0x04, 0x00, 0x00, 0x00, 0x01, // Frame header
    ];

    let meta = VideoChunkMeta {
        frame_id: 1,
        chunk_index: 0,
        total_chunks: 1,
        codec: VideoCodec::Av1,
        is_keyframe: true,
        is_intra_refresh: false,
        width: 1920,
        height: 1080,
        fps: 90,
        pts_us: 10_000,
    };
    let chunk = VideoChunk::new(meta, Bytes::from(av1_frame_data));
    let chunk_bytes = chunk.serialize().unwrap();
    let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 10_000, &chunk_bytes);
    let packet = Packet::new(header, chunk_bytes);

    let arrival = Instant::now();
    runtime.ingest_packet(packet, arrival);

    // Step frame loop after jitter playout delay (+10ms)
    let playout_time = arrival + Duration::from_millis(10);
    runtime
        .step_frame_at(playout_time)
        .expect("step_frame_at failed");

    // Frame should have been dequeued and released to OpenXR surface
    let stats = runtime.decoder_stats();
    assert_eq!(stats.frames_rendered, 1);
    assert_eq!(stats.frames_dropped, 0);

    let hud_text = runtime.hud_text();
    assert!(hud_text.contains("FPS:"));

    // Stop runtime
    runtime.stop();
    assert!(!runtime.is_running());
}

#[test]
fn test_client_runtime_lifecycle_with_hevc() {
    let config = ClientConfig {
        codec: ClientVideoCodec::Hevc,
        display_mode: DisplayMode::FlatQuad,
        target_refresh_rate: 120.0,
        ..Default::default()
    };

    let mut runtime = QuestClientRuntime::new(config).expect("Failed to create client runtime");
    assert!(runtime.is_running());

    // Synthesize an HEVC frame packet
    let hevc_frame_data = vec![
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF, // VPS
        0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, // SPS
        0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xC0, 0xF3, 0xC0, // PPS
        0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x08, // IDR_W_RADL
    ];

    let meta = VideoChunkMeta {
        frame_id: 1,
        chunk_index: 0,
        total_chunks: 1,
        codec: VideoCodec::Hevc,
        is_keyframe: true,
        is_intra_refresh: false,
        width: 1920,
        height: 1080,
        fps: 120,
        pts_us: 20_000,
    };
    let chunk = VideoChunk::new(meta, Bytes::from(hevc_frame_data));
    let chunk_bytes = chunk.serialize().unwrap();
    let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 20_000, &chunk_bytes);
    let packet = Packet::new(header, chunk_bytes);

    let arrival = Instant::now();
    runtime.ingest_packet(packet, arrival);

    let playout_time = arrival + Duration::from_millis(10);
    runtime
        .step_frame_at(playout_time)
        .expect("step_frame_at failed");

    let stats = runtime.decoder_stats();
    assert_eq!(stats.frames_rendered, 1);

    // Verify HUD text
    let hud_text = runtime.hud_text();
    assert!(hud_text.contains("120Hz"));
    assert!(hud_text.contains("mock.c2.qti.hevc.decoder.low_latency"));

    runtime.stop();
}

#[test]
fn test_client_refresh_rate_transitions() {
    let config = ClientConfig {
        target_refresh_rate: 72.0,
        ..Default::default()
    };

    let mut runtime = QuestClientRuntime::new(config).expect("Failed to create client runtime");
    assert!(runtime.is_running());

    // Switch to 90 Hz
    runtime.set_target_refresh_rate(90.0);
    runtime.step_frame().expect("step_frame failed");
    let hud_90 = runtime.hud_text();
    assert!(hud_90.contains("90Hz"));

    // Switch to 120 Hz
    runtime.set_target_refresh_rate(120.0);
    runtime.step_frame().expect("step_frame failed");
    let hud_120 = runtime.hud_text();
    assert!(hud_120.contains("120Hz"));

    runtime.stop();
}
