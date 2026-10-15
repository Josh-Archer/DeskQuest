pub mod clipboard;
pub mod error;
pub mod handshake;
pub mod input;
pub mod packet;
pub mod reassembler;
pub mod telemetry;
pub mod video;

pub use clipboard::{
    chunk_clipboard_text, AssembledClipboard, ClipboardMessage, ClipboardReassembler,
    ClipboardSyncEngine, SyncAction, DEFAULT_CLIPBOARD_CHUNK_SIZE, MAX_CLIPBOARD_TEXT_SIZE,
};
pub use error::ProtocolError;
pub use handshake::{ClientHandshake, DisplayConfigMessage, DisplayInfo, ServerHandshake};
pub use input::{
    ElementState, InputBatch, InputEvent, KeyModifiers, MouseButton, RedundantKeyEvent,
    INPUT_BATCH_HEADER_SIZE, INPUT_BATCH_MAGIC, INPUT_BATCH_VERSION,
};
pub use packet::{
    Packet, PacketHeader, PacketType, FLAG_COMPRESSED, FLAG_FEC_PROTECTED, FLAG_INTRA_REFRESH,
    FLAG_KEYFRAME, FLAG_LAST_CHUNK, FLAG_NONE, HEADER_SIZE, PROTOCOL_MAGIC, PROTOCOL_VERSION,
};
pub use reassembler::{AssembledFrame, FrameReassembler};
pub use telemetry::FrameLatencyBreakdown;
pub use video::{VideoChunk, VideoChunkMeta, VideoCodec};

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_packet_header_encode_decode() {
        let payload = b"Hello Linux Quest";
        let header = PacketHeader::new(PacketType::Ping, 1, 42, 1_000_000, payload);

        let mut buf = bytes::BytesMut::new();
        header.encode(&mut buf);
        assert_eq!(buf.len(), HEADER_SIZE);

        let mut slice = &buf[..];
        let decoded = PacketHeader::decode(&mut slice).expect("Failed to decode header");

        assert_eq!(decoded.magic, PROTOCOL_MAGIC);
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.packet_type, PacketType::Ping);
        assert_eq!(decoded.stream_id, 1);
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.timestamp_us, 1_000_000);
        assert_eq!(decoded.payload_len, payload.len() as u32);
        assert_eq!(decoded.checksum, header.checksum);
    }

    #[test]
    fn test_full_packet_roundtrip() {
        let payload = Bytes::from_static(b"video-frame-slice-data-here-12345");
        let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, 101, 500_000, &payload)
            .with_flags(packet::FLAG_KEYFRAME);

        let packet = Packet::new(header, payload.clone());
        let wire_bytes = packet.to_bytes();

        let decoded = Packet::from_bytes(&wire_bytes).expect("Failed to decode packet");
        assert_eq!(decoded.header.packet_type, PacketType::VideoFrameChunk);
        assert_eq!(decoded.header.flags, packet::FLAG_KEYFRAME);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_packet_checksum_validation() {
        let payload = Bytes::from_static(b"critical-telemetry-payload");
        let header = PacketHeader::new(PacketType::TelemetryReport, 0, 1, 100, &payload);
        let packet = Packet::new(header, payload);
        let mut wire_bytes = packet.to_bytes().to_vec();

        // Corrupt one payload byte
        let last_idx = wire_bytes.len() - 1;
        wire_bytes[last_idx] ^= 0xFF;

        let err = Packet::from_bytes(&wire_bytes).expect_err("Should have failed checksum");
        match err {
            ProtocolError::ChecksumMismatch { .. } => (),
            other => panic!("Expected ChecksumMismatch, got {:?}", other),
        }
    }

    #[test]
    fn test_video_chunk_serialization() {
        let meta = VideoChunkMeta {
            frame_id: 100,
            chunk_index: 0,
            total_chunks: 3,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 3840,
            height: 2160,
            fps: 90,
            pts_us: 1234567,
        };
        let payload = Bytes::from_static(&[0x11, 0x22, 0x33, 0x44]);
        let chunk = VideoChunk::new(meta.clone(), payload.clone());

        let serialized = chunk.serialize().expect("Serialization failed");
        let deserialized = VideoChunk::deserialize(&serialized).expect("Deserialization failed");

        assert_eq!(deserialized.meta, meta);
        assert_eq!(deserialized.payload, payload);
    }

    #[test]
    fn test_frame_reassembler_in_order() {
        let mut reassembler = FrameReassembler::new(4);
        let meta_base = VideoChunkMeta {
            frame_id: 1,
            chunk_index: 0,
            total_chunks: 2,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 1920,
            height: 1080,
            fps: 60,
            pts_us: 1000,
        };

        let chunk0 = VideoChunk::new(
            VideoChunkMeta {
                chunk_index: 0,
                ..meta_base.clone()
            },
            Bytes::from_static(b"part1-"),
        );
        let chunk1 = VideoChunk::new(
            VideoChunkMeta {
                chunk_index: 1,
                ..meta_base
            },
            Bytes::from_static(b"part2"),
        );

        assert!(reassembler.ingest_chunk(chunk0).unwrap().is_none());
        let frame = reassembler
            .ingest_chunk(chunk1)
            .unwrap()
            .expect("Frame must be complete");
        assert_eq!(frame.bitstream, Bytes::from_static(b"part1-part2"));
    }

    #[test]
    fn test_frame_reassembler_out_of_order() {
        let mut reassembler = FrameReassembler::new(4);
        let meta_base = VideoChunkMeta {
            frame_id: 2,
            chunk_index: 0,
            total_chunks: 3,
            codec: VideoCodec::Hevc,
            is_keyframe: false,
            is_intra_refresh: true,
            width: 1920,
            height: 1080,
            fps: 90,
            pts_us: 2000,
        };

        let chunk2 = VideoChunk::new(
            VideoChunkMeta {
                chunk_index: 2,
                ..meta_base.clone()
            },
            Bytes::from_static(b"[end]"),
        );
        let chunk0 = VideoChunk::new(
            VideoChunkMeta {
                chunk_index: 0,
                ..meta_base.clone()
            },
            Bytes::from_static(b"[start]"),
        );
        let chunk1 = VideoChunk::new(
            VideoChunkMeta {
                chunk_index: 1,
                ..meta_base
            },
            Bytes::from_static(b"[middle]"),
        );

        // Feed chunks out of order: 2, 0, 1
        assert!(reassembler.ingest_chunk(chunk2).unwrap().is_none());
        assert!(reassembler.ingest_chunk(chunk0).unwrap().is_none());
        let frame = reassembler
            .ingest_chunk(chunk1)
            .unwrap()
            .expect("Frame must be complete");
        assert_eq!(frame.bitstream, Bytes::from_static(b"[start][middle][end]"));
    }

    #[test]
    fn test_telemetry_breakdown() {
        let mut telemetry = FrameLatencyBreakdown::new(42, 1000);
        telemetry.capture_duration_us = 1200;
        telemetry.encode_duration_us = 3100;
        telemetry.host_queue_duration_us = 300;
        telemetry.transport_duration_us = 1500;
        telemetry.client_decode_duration_us = 2400;
        telemetry.client_render_duration_us = 800;

        let total = telemetry.compute_total();
        assert_eq!(total, 9300);
        assert!((telemetry.total_ms() - 9.3).abs() < 0.001);
    }

    #[test]
    fn test_fec_parity_packet_roundtrip() {
        let parity_payload = Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x99]);
        let header = PacketHeader::new(PacketType::FecParity, 0, 77, 999_999, &parity_payload)
            .with_flags(FLAG_FEC_PROTECTED);

        let packet = Packet::new(header, parity_payload.clone());
        let wire = packet.to_bytes();

        let decoded = Packet::from_bytes(&wire).expect("Failed to decode FEC parity packet");
        assert_eq!(decoded.header.packet_type, PacketType::FecParity);
        assert_eq!(decoded.header.flags, FLAG_FEC_PROTECTED);
        assert_eq!(decoded.payload, parity_payload);
    }
}
