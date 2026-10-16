//! Integration tests for client audio receiver, jitter buffer, Opus decoder,
//! packet-loss concealment (PLC), A/V synchronization, and audio sink playout.

use std::time::{Duration, Instant};

use bytes::Bytes;
use linux_quest_capture::{AudioSampleFormat, RawAudioFrame};
use linux_quest_client::audio::{
    AudioDecoder, AudioJitterBuffer, AudioPlayoutItem, AudioReceiverConfig, AudioSinkConfig,
    AvSyncConfig, AvSynchronizer, ClientAudioPipeline, DecodedAudioFrame, DriftCorrectionMode,
    MockAudioDecoder, MockAudioSink, OpusAudioDecoder,
};
use linux_quest_client::config::ClientConfig;
use linux_quest_client::runtime::QuestClientRuntime;
use linux_quest_encoder::audio::{AudioApplication, AudioEncoder, AudioEncoderConfig};
use linux_quest_encoder::opus::OpusAudioEncoder;
use linux_quest_protocol::audio::{AudioCodec, AudioFrameMeta, AudioPacket};
use linux_quest_protocol::packet::{Packet, PacketHeader, PacketType};

fn make_test_audio_packet(seq: u32, pts_us: u64, payload: Bytes) -> AudioPacket {
    let meta = AudioFrameMeta {
        frame_id: seq as u64,
        sequence: seq,
        sample_rate: 48000,
        channels: 2,
        samples_per_channel: 120, // 2.5ms @ 48kHz
        codec: AudioCodec::Opus,
        pts_us,
    };
    AudioPacket::new(meta, payload)
}

fn make_network_packet(audio_pkt: &AudioPacket) -> Packet {
    let serialized = audio_pkt.serialize().unwrap();
    let header = PacketHeader::new(
        PacketType::AudioFrame,
        1,
        audio_pkt.meta.sequence,
        audio_pkt.meta.pts_us,
        &serialized,
    );
    Packet::new(header, serialized)
}

#[test]
fn test_audio_jitter_buffer_in_order_streaming() {
    let config = AudioReceiverConfig {
        min_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(30),
        loss_hold_timeout: Duration::from_millis(10),
        ..Default::default()
    };
    let mut jb = AudioJitterBuffer::new(config);

    let t0 = Instant::now();
    let frame_dur_us = 2500u64; // 2.5ms

    // Stream 20 consecutive packets
    for i in 1..=20 {
        let pts = 100_000 + (i as u64 - 1) * frame_dur_us;
        let arrival = t0 + Duration::from_micros((i as u64 - 1) * 2500);
        let pkt = make_test_audio_packet(i, pts, Bytes::from_static(&[0xFC, 1, 2, 3]));
        jb.ingest_packet(pkt, arrival);
    }

    assert_eq!(jb.queue_len(), 20);

    // Playout packets as time advances
    let mut popped_sequences = Vec::new();
    let playout_start = t0 + Duration::from_millis(6);

    for step in 0..20 {
        let now = playout_start + Duration::from_micros(step as u64 * 2500);
        if let Some(item) = jb.pop_ready_item(now) {
            match item {
                AudioPlayoutItem::Packet(p) => popped_sequences.push(p.meta.sequence),
                AudioPlayoutItem::Lost { sequence, .. } => {
                    panic!("Unexpected packet loss for seq {sequence}")
                }
            }
        }
    }

    assert_eq!(popped_sequences.len(), 20);
    for (idx, &seq) in popped_sequences.iter().enumerate() {
        assert_eq!(seq, (idx + 1) as u32);
    }

    assert_eq!(jb.stats().packets_delivered, 20);
    assert_eq!(jb.stats().packets_lost, 0);
    assert_eq!(jb.stats().loss_rate(), 0.0);
}

#[test]
fn test_audio_jitter_buffer_packet_loss_recovery_with_plc() {
    let config = AudioReceiverConfig {
        min_delay: Duration::from_millis(2),
        loss_hold_timeout: Duration::from_millis(5),
        ..Default::default()
    };
    let mut jb = AudioJitterBuffer::new(config);

    let t0 = Instant::now();
    let frame_dur_us = 2500u64;

    // Send packets 1, 2, 3. Drop packet 4 and 5. Send packet 6, 7.
    let transmitted = vec![1, 2, 3, 6, 7];
    for &seq in &transmitted {
        let pts = 100_000 + (seq as u64 - 1) * frame_dur_us;
        let arrival = t0 + Duration::from_micros((seq as u64 - 1) * 2500);
        let pkt = make_test_audio_packet(seq, pts, Bytes::from_static(&[0xFC, 0xAA]));
        jb.ingest_packet(pkt, arrival);
    }

    let mut delivered_packets = Vec::new();
    let mut lost_sequences = Vec::new();

    // Playout over time
    for ms in 2..=30 {
        let now = t0 + Duration::from_millis(ms);
        while let Some(item) = jb.pop_ready_item(now) {
            match item {
                AudioPlayoutItem::Packet(p) => delivered_packets.push(p.meta.sequence),
                AudioPlayoutItem::Lost { sequence, .. } => lost_sequences.push(sequence),
            }
        }
    }

    // Sequences 1, 2, 3 delivered intact
    assert_eq!(delivered_packets, vec![1, 2, 3, 6, 7]);
    // Sequences 4, 5 detected as lost and emitted for PLC
    assert_eq!(lost_sequences, vec![4, 5]);

    assert_eq!(jb.stats().packets_delivered, 5);
    assert_eq!(jb.stats().packets_lost, 2);
    assert!(jb.stats().loss_rate() > 0.0);
}

#[test]
fn test_audio_jitter_buffer_severe_reordering() {
    let config = AudioReceiverConfig {
        min_delay: Duration::from_millis(5),
        ..Default::default()
    };
    let mut jb = AudioJitterBuffer::new(config);

    let t0 = Instant::now();
    // Severe out-of-order transmission order
    let scramble = [4, 1, 3, 2, 6, 5, 8, 7, 10, 9];
    for (arrival_idx, &seq) in scramble.iter().enumerate() {
        let pts = 100_000 + (seq as u64 - 1) * 2500;
        let arrival = t0 + Duration::from_millis(arrival_idx as u64);
        jb.ingest_packet(
            make_test_audio_packet(seq, pts, Bytes::from_static(&[1, 2])),
            arrival,
        );
    }

    let playout_time = t0 + Duration::from_millis(25);
    let mut delivered = Vec::new();

    while let Some(item) = jb.pop_ready_item(playout_time) {
        match item {
            AudioPlayoutItem::Packet(p) => delivered.push(p.meta.sequence),
            AudioPlayoutItem::Lost { sequence, .. } => {
                panic!("No packets should be lost, but seq {sequence} was declared lost")
            }
        }
    }

    assert_eq!(delivered, (1..=10).collect::<Vec<u32>>());
    assert_eq!(jb.stats().packets_delivered, 10);
    assert_eq!(jb.stats().packets_lost, 0);
}

#[test]
fn test_audio_jitter_buffer_u32_sequence_wrapping() {
    let mut jb = AudioJitterBuffer::new(AudioReceiverConfig {
        min_delay: Duration::from_millis(1),
        ..Default::default()
    });

    let t0 = Instant::now();
    // Start at u32::MAX - 2 and wrap through 0, 1, 2
    let sequences: Vec<u32> = vec![u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2];

    for (idx, &seq) in sequences.iter().enumerate() {
        let pts = 10_000 + (idx as u64) * 2500;
        let arrival = t0 + Duration::from_millis(idx as u64);
        jb.ingest_packet(
            make_test_audio_packet(seq, pts, Bytes::from_static(&[0xAA])),
            arrival,
        );
    }

    let mut delivered = Vec::new();
    for ms in 1..=20 {
        let now = t0 + Duration::from_millis(ms);
        while let Some(item) = jb.pop_ready_item(now) {
            match item {
                AudioPlayoutItem::Packet(p) => delivered.push(p.meta.sequence),
                AudioPlayoutItem::Lost { sequence, .. } => {
                    panic!("Unexpected loss of wrapped seq {sequence}")
                }
            }
        }
    }

    assert_eq!(delivered, sequences);
    assert_eq!(jb.stats().packets_delivered, 6);
    assert_eq!(jb.stats().packets_lost, 0);
}

#[test]
fn test_av_sync_drift_correction_lagging_audio_resampling() {
    let sync_config = AvSyncConfig {
        sync_tolerance_us: 4_000, // 4ms
        max_resample_ratio: 0.02, // 2% max
        smoothing_factor: 0.5,
        mode: DriftCorrectionMode::Resample,
        ..Default::default()
    };
    let mut sync = AvSynchronizer::new(sync_config);

    let t0 = Instant::now();
    // Video presented at PTS 100_000 at t0
    sync.update_video_clock(100_000, t0);

    // Audio is lagging behind video clock by 20ms (PTS 80_000 at t0)
    let samples_per_channel = 120; // 2.5ms
    let total_samples = samples_per_channel * 2;
    let samples = vec![0.5f32; total_samples];

    // Process a series of frames
    let mut total_output_samples = 0;
    for i in 0..10 {
        let frame_pts = 80_000 + (i as u64) * 2500;
        let frame_time = t0 + Duration::from_micros(i as u64 * 2500);
        let frame = DecodedAudioFrame::new(
            48000,
            2,
            samples_per_channel,
            frame_pts,
            samples.clone(),
            false,
        );

        let out = sync.process_frame(&frame, frame_time);
        total_output_samples += out.len();
    }

    // Audio is lagging, so synchronizer should resample faster (ratio > 1.0),
    // producing fewer output samples so the sink consumes them faster and catches up!
    let expected_unadjusted = total_samples * 10;
    assert!(
        total_output_samples < expected_unadjusted,
        "Lagging audio should have dropped samples to catch up"
    );

    let metrics = sync.metrics();
    assert!(metrics.total_samples_dropped > 0);
    assert!(metrics.resample_ratio > 1.0);
}

#[test]
fn test_av_sync_drift_correction_leading_audio_resampling() {
    let sync_config = AvSyncConfig {
        sync_tolerance_us: 4_000,
        max_resample_ratio: 0.02,
        smoothing_factor: 0.5,
        mode: DriftCorrectionMode::Resample,
        ..Default::default()
    };
    let mut sync = AvSynchronizer::new(sync_config);

    let t0 = Instant::now();
    sync.update_video_clock(100_000, t0);

    // Audio is leading video clock by 20ms (PTS 120_000 at t0)
    let samples_per_channel = 120;
    let total_samples = samples_per_channel * 2;
    let samples = vec![0.5f32; total_samples];

    let mut total_output_samples = 0;
    for i in 0..10 {
        let frame_pts = 120_000 + (i as u64) * 2500;
        let frame_time = t0 + Duration::from_micros(i as u64 * 2500);
        let frame = DecodedAudioFrame::new(
            48000,
            2,
            samples_per_channel,
            frame_pts,
            samples.clone(),
            false,
        );

        let out = sync.process_frame(&frame, frame_time);
        total_output_samples += out.len();
    }

    // Audio is leading, so synchronizer slows down playback (ratio < 1.0),
    // producing more output samples so playback delays and video catches up!
    let expected_unadjusted = total_samples * 10;
    assert!(
        total_output_samples > expected_unadjusted,
        "Leading audio should have inserted samples to slow down"
    );

    let metrics = sync.metrics();
    assert!(metrics.total_samples_inserted > 0);
    assert!(metrics.resample_ratio < 1.0);
}

#[test]
fn test_end_to_end_audio_pipeline_with_loss_concealment_and_sync() {
    let rx_cfg = AudioReceiverConfig {
        min_delay: Duration::from_millis(2),
        loss_hold_timeout: Duration::from_millis(5),
        ..Default::default()
    };
    let sync_cfg = AvSyncConfig {
        sync_tolerance_us: 5_000,
        mode: DriftCorrectionMode::Resample,
        ..Default::default()
    };
    let decoder = MockAudioDecoder::new(48000, 2);
    let sink = MockAudioSink::new(AudioSinkConfig::default());

    let mut pipeline = ClientAudioPipeline::new(rx_cfg, sync_cfg, decoder, sink);
    pipeline.start(AudioSinkConfig::default()).unwrap();

    let t0 = Instant::now();
    pipeline.update_video_presentation(100_000, t0);

    // Feed 10 packets, dropping packet 4 and 8
    for seq in 1..=10 {
        if seq == 4 || seq == 8 {
            continue; // Dropped packet
        }
        let pts = 100_000 + (seq as u64 - 1) * 2500;
        let arrival = t0 + Duration::from_micros((seq as u64 - 1) * 2500);
        let audio_pkt = make_test_audio_packet(seq, pts, Bytes::from_static(&[1, 2, 3, 4]));
        let net_pkt = make_network_packet(&audio_pkt);
        assert!(pipeline.ingest_packet(net_pkt, arrival).unwrap());
    }

    let mut step_summaries = Vec::new();
    for ms in 2..=35 {
        let now = t0 + Duration::from_millis(ms);
        while let Some(summary) = pipeline.step(now).unwrap() {
            step_summaries.push(summary);
        }
    }

    // Total 10 frames played (8 normal + 2 PLC concealed)
    assert_eq!(step_summaries.len(), 10);
    let concealed_count = step_summaries.iter().filter(|s| s.is_concealed).count();
    assert_eq!(concealed_count, 2);

    assert_eq!(pipeline.receiver_stats().packets_delivered, 8);
    assert_eq!(pipeline.receiver_stats().packets_lost, 2);
    assert!(pipeline.sink().total_samples_written() >= 240 * 10);

    pipeline.stop().unwrap();
}

#[tokio::test]
async fn test_opus_codec_roundtrip_encode_decode_and_plc() {
    let enc_config = AudioEncoderConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48000,
        channels: 2,
        frame_duration_us: 2500, // 120 samples
        bitrate_bps: 96_000,
        fec_enabled: true,
        dtx_enabled: false,
        application: AudioApplication::RestrictedLowDelay,
    };

    let mut encoder = OpusAudioEncoder::new(enc_config).expect("Opus encoder init failed");
    let mut decoder = OpusAudioDecoder::new(48000, 2).expect("Opus decoder init failed");

    // Synthesize sine wave input frame
    let samples_per_channel = 120;
    let mut pcm_f32 = Vec::with_capacity(samples_per_channel * 2);
    for i in 0..samples_per_channel {
        let val = (i as f32 * 0.1).sin() * 0.8;
        pcm_f32.push(val);
        pcm_f32.push(val);
    }
    let mut pcm_bytes = Vec::with_capacity(pcm_f32.len() * 4);
    for &sample in &pcm_f32 {
        pcm_bytes.extend_from_slice(&sample.to_le_bytes());
    }

    let raw_frame = RawAudioFrame::new(
        48000,
        2,
        AudioSampleFormat::F32Le,
        samples_per_channel as u32,
        50_000,
        Bytes::from(pcm_bytes),
    );

    // 1. Encode with Opus
    let encoded = encoder
        .encode(&raw_frame)
        .await
        .expect("Opus encode failed");
    assert!(!encoded.data.is_empty());

    // 2. Decode with Opus
    let decoded = decoder
        .decode(Some(&encoded.data), false, samples_per_channel)
        .expect("Opus decode failed");
    assert_eq!(decoded.sample_rate, 48000);
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.samples_per_channel, samples_per_channel);
    assert_eq!(decoded.total_samples(), samples_per_channel * 2);
    assert!(!decoded.is_concealed);

    // Verify decoded audio is active and non-silent
    let energy: f32 = decoded.samples.iter().map(|&s| s * s).sum();
    assert!(energy > 0.01, "Decoded Opus audio energy is too low");

    // 3. Test Opus Packet Loss Concealment (PLC)
    let plc_frame = decoder
        .decode(None, false, samples_per_channel)
        .expect("Opus PLC decode failed");
    assert_eq!(plc_frame.samples_per_channel, samples_per_channel);
    assert!(plc_frame.is_concealed);

    // Verify no NaNs or Infinities in PLC frame
    for &s in &plc_frame.samples {
        assert!(!s.is_nan() && !s.is_infinite());
    }

    // 4. Test state reset
    decoder.reset().expect("Opus decoder reset failed");
}

#[test]
fn test_quest_client_runtime_audio_packet_routing() {
    let config = ClientConfig::default();
    let mut runtime = QuestClientRuntime::new(config).unwrap();

    let audio_pkt = make_test_audio_packet(1, 1000, Bytes::from_static(&[0xFC, 1, 2, 3]));
    let net_pkt = make_network_packet(&audio_pkt);

    let arrival = Instant::now();
    runtime.ingest_packet(net_pkt, arrival);

    // Audio receiver should have buffered the packet
    assert_eq!(runtime.audio_receiver().queue_len(), 1);
    assert_eq!(runtime.audio_receiver().stats().packets_received, 1);
}
