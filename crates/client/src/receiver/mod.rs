//! Streaming receiver integrating jitter buffer, FEC recovery, and frame reassembly.

pub mod loss_recovery;

use std::time::{Duration, Instant};

use linux_quest_protocol::packet::Packet;
use linux_quest_protocol::reassembler::{AssembledFrame, FrameReassembler};
use linux_quest_protocol::video::VideoChunk;
use linux_quest_transport::jitter_buffer::{
    AdaptiveJitterBuffer, JitterBufferConfig, JitterBufferStats,
};

use crate::config::ClientConfig;
use crate::error::ClientResult;

/// Client network receiver that reconstructs lost packets and reassembles video frames.
pub struct ClientReceiver {
    jitter_buffer: AdaptiveJitterBuffer,
    reassembler: FrameReassembler,
    packets_received: u64,
    stream_restarts_seen: u64,
}

impl ClientReceiver {
    /// Creates a new client receiver configured with jitter buffer parameters.
    pub fn new(config: &ClientConfig) -> Self {
        let initial_depth = config.jitter_buffer_initial_depth_ms;
        let jb_config = JitterBufferConfig {
            min_delay: Duration::from_millis(initial_depth),
            max_delay: Duration::from_millis((initial_depth * 4).max(30)),
            max_queue_depth: config.jitter_buffer_max_depth,
            ..Default::default()
        };

        Self {
            jitter_buffer: AdaptiveJitterBuffer::new(jb_config),
            reassembler: FrameReassembler::new(16),
            packets_received: 0,
            stream_restarts_seen: 0,
        }
    }

    /// Ingests a raw transport packet into the jitter buffer.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        self.packets_received += 1;
        self.jitter_buffer.ingest_packet(packet, now);
        let restarts = self.jitter_buffer.stats().stream_restarts;
        if restarts != self.stream_restarts_seen {
            // The host restarted its stream: half-built frames belong to the old one.
            self.stream_restarts_seen = restarts;
            self.reassembler = FrameReassembler::new(16);
        }
    }

    /// Pops all video frames whose jitter delay has elapsed at timestamp `now`.
    pub fn pop_ready_frames(&mut self, now: Instant) -> ClientResult<Vec<AssembledFrame>> {
        let mut completed_frames = Vec::new();

        while let Some(ready_pkt) = self.jitter_buffer.pop_ready_packet(now) {
            // Only process video frame chunks through the frame reassembler
            if ready_pkt.header.packet_type
                == linux_quest_protocol::packet::PacketType::VideoFrameChunk
            {
                match VideoChunk::deserialize(&ready_pkt.payload) {
                    Ok(chunk) => {
                        if let Some(frame) = self.reassembler.ingest_chunk(chunk)? {
                            completed_frames.push(frame);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            seq = ready_pkt.header.sequence,
                            "Failed to deserialize video chunk payload"
                        );
                    }
                }
            }
        }

        Ok(completed_frames)
    }

    /// Ingests a packet and immediately attempts to pop ready frames.
    pub fn ingest_and_pop(
        &mut self,
        packet: Packet,
        now: Instant,
    ) -> ClientResult<Vec<AssembledFrame>> {
        self.ingest_packet(packet, now);
        self.pop_ready_frames(now)
    }

    /// Returns current jitter buffer statistics.
    pub fn jitter_buffer_stats(&self) -> &JitterBufferStats {
        self.jitter_buffer.stats()
    }

    /// Total transport packets received.
    pub fn packets_received(&self) -> u64 {
        self.packets_received
    }

    /// Packets recovered via Forward Error Correction.
    pub fn fec_recovered_count(&self) -> u64 {
        self.jitter_buffer.stats().packets_reconstructed_fec
    }

    /// Packets permanently lost.
    pub fn packets_lost_count(&self) -> u64 {
        self.jitter_buffer.stats().packets_lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::video::{VideoChunkMeta, VideoCodec};

    #[test]
    fn test_client_receiver_ingest_and_reassemble() {
        let config = ClientConfig::default();
        let mut receiver = ClientReceiver::new(&config);

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
            pts_us: 100_000,
        };
        let payload = Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]);
        let chunk = VideoChunk::new(meta.clone(), payload.clone());
        let chunk_bytes = chunk.serialize().unwrap();

        let header = linux_quest_protocol::packet::PacketHeader::new(
            linux_quest_protocol::packet::PacketType::VideoFrameChunk,
            1,
            1,
            100_000,
            &chunk_bytes,
        );
        let packet = Packet::new(header, chunk_bytes);

        let arrival = Instant::now();
        receiver.ingest_packet(packet, arrival);

        // After jitter buffer target playout delay (e.g. +10ms)
        let playout_time = arrival + Duration::from_millis(10);
        let frames = receiver.pop_ready_frames(playout_time).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].meta.frame_id, 1);
        assert_eq!(frames[0].bitstream, payload);
    }
}
