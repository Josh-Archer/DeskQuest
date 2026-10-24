//! Turns encoded frames into UDP datagrams in exactly the layout the Quest client consumes.
//!
//! # Wire layout (fixed by the client's receiver; this module only produces it)
//!
//! Every datagram is one [`Packet`]: a 32-byte [`PacketHeader`] followed by `payload_len` bytes.
//!
//! * Source datagram: type `VideoFrameChunk`, payload = [`VideoChunk::serialize`] (a bincode
//!   [`VideoChunkMeta`] plus up to `max_payload` bytes of bitstream). `meta.chunk_index` and
//!   `meta.total_chunks` index the chunk within the whole frame.
//! * FEC groups ("blocks"): the client's [`AdaptiveJitterBuffer`] groups packets by
//!   `header.timestamp_us`, so for FEC-protected packets that field is the block id and must be
//!   unique per block. It is the send time in microseconds, forced to be strictly increasing.
//!   Sources carry `FLAG_FEC_PROTECTED` and their position in the block in `header.reserved`
//!   (the FEC index). A block's `k` source packets use `k` consecutive sequence numbers, and its
//!   `m` parity packets use the `m` sequence numbers right after them.
//! * Parity datagram: type `FecParity`, `FLAG_FEC_PROTECTED`, FEC index `k + p`, payload =
//!   [`ReedSolomonFec::encode`] output (an FEC header plus the parity symbol).
//! * `FLAG_KEYFRAME` / `FLAG_INTRA_REFRESH` mirror the frame; `FLAG_LAST_CHUNK` marks the last
//!   chunk of the frame. The frame's capture timestamp travels in `VideoChunkMeta::pts_us`.
//!
//! [`AdaptiveJitterBuffer`]: linux_quest_transport::AdaptiveJitterBuffer

use std::collections::VecDeque;

use bytes::{BufMut, Bytes, BytesMut};
use linux_quest_encoder::{EncodedFrame, EncoderError};
use linux_quest_protocol::{
    PacketHeader, PacketType, ProtocolError, VideoChunk, VideoChunkMeta, FLAG_FEC_PROTECTED,
    FLAG_INTRA_REFRESH, FLAG_KEYFRAME, FLAG_LAST_CHUNK, HEADER_SIZE,
};
use linux_quest_transport::fec::{FecError, ReedSolomonFec, FEC_HEADER_SIZE};
use thiserror::Error;

/// UDP port on the headset that the client binds for video (`0.0.0.0:48440`).
pub const DEFAULT_VIDEO_PORT: u16 = 48440;

/// Default bitstream bytes per datagram. This is the encoder's own default `max_chunk_size`.
pub const DEFAULT_MAX_PAYLOAD: usize = 1200;

/// Largest bitstream payload accepted per datagram. With the 32-byte header, the 4-byte meta
/// length prefix, the 38-byte bincode meta and the FEC header and length prefix of a parity
/// symbol, the largest datagram stays below [`MAX_DATAGRAM_SIZE`].
pub const MAX_PAYLOAD_LIMIT: usize = 1300;

/// Hard cap on one datagram, equal to the transport crate's `DEFAULT_UDP_MTU`.
pub const MAX_DATAGRAM_SIZE: usize = 1400;

/// The Reed-Solomon code works over GF(2^8): `k + m` must not exceed 256.
const MAX_FEC_TOTAL: usize = 256;

#[derive(Error, Debug)]
pub enum VideoSendError {
    #[error("encoder: {0}")]
    Encode(#[from] EncoderError),

    #[error("forward error correction: {0}")]
    Fec(#[from] FecError),

    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("frame needs {chunks} chunks, more than the 65535 the wire format allows")]
    FrameTooLarge { chunks: usize },

    #[error("datagram of {size} bytes exceeds the {max} byte limit")]
    DatagramTooLarge { size: usize, max: usize },
}

/// Forward error correction settings. Loss recovery is FEC only; there is no retransmission.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FecConfig {
    /// Most source packets in one FEC block. A frame with more chunks is split into several
    /// balanced blocks. Smaller blocks recover bursts sooner but cost more parity per packet.
    pub group_size: usize,
    /// Parity packets per source packet, rounded up and at least one. `0.0` turns FEC off.
    /// With the defaults (20 sources, 0.25) every block of 17..=20 sources carries 5 parity
    /// packets and tolerates any 5 lost datagrams out of its 22 to 25.
    pub parity_ratio: f64,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            group_size: 20,
            parity_ratio: 0.25,
        }
    }
}

impl FecConfig {
    /// Source packets per block after clamping to what GF(2^8) allows.
    fn group(&self) -> usize {
        self.group_size.clamp(1, MAX_FEC_TOTAL / 2)
    }

    /// Parity packets for a block of `k` source packets.
    pub fn parity_for(&self, k: usize) -> usize {
        if self.parity_ratio <= 0.0 || k == 0 {
            return 0;
        }
        // The epsilon keeps 30 * 0.1 = 3.0000000000000004 from rounding up to 4.
        let m = ((k as f64 * self.parity_ratio) - 1e-9).ceil().max(1.0) as usize;
        m.min(MAX_FEC_TOTAL.saturating_sub(k))
    }
}

/// One UDP datagram ready to send.
#[derive(Debug, Clone)]
pub struct Datagram {
    pub bytes: Bytes,
    pub is_parity: bool,
}

struct PlannedBlock {
    /// Serialized `VideoChunk`s and whether each is the last chunk of the frame.
    chunks: Vec<(Bytes, bool)>,
    parity: usize,
}

/// An encoded frame prepared for sending: chunks serialized and split into FEC blocks.
/// Blocks are turned into datagrams one at a time with [`Packetizer::next_block`], so the
/// parity of a later block is computed while earlier blocks are still being paced out.
pub struct FramePlan {
    pub frame_id: u64,
    pub fps: u32,
    pub is_keyframe: bool,
    pub is_intra_refresh: bool,
    pub source_count: usize,
    pub parity_count: usize,
    /// Exact total of all datagram sizes, used to choose the pacing rate.
    pub wire_bytes: usize,
    /// Wire size of the largest FEC block (its sources plus its parity).
    pub max_block_wire_bytes: usize,
    blocks: VecDeque<PlannedBlock>,
}

impl FramePlan {
    pub fn datagram_count(&self) -> usize {
        self.source_count + self.parity_count
    }

    pub fn is_exhausted(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Stateful packetizer: owns the sequence counter and the FEC block id clock.
pub struct Packetizer {
    stream_id: u16,
    sequence: u32,
    last_block_id: u64,
    max_payload: usize,
    fec: FecConfig,
}

impl Packetizer {
    pub fn new(stream_id: u16, max_payload: usize, fec: FecConfig) -> Self {
        Self {
            stream_id,
            sequence: 0,
            last_block_id: 0,
            max_payload: max_payload.clamp(1, MAX_PAYLOAD_LIMIT),
            fec,
        }
    }

    /// Changes the parity ratio for blocks planned from now on. Plans already made keep the
    /// parity they were built with.
    pub fn set_parity_ratio(&mut self, ratio: f64) {
        self.fec.parity_ratio = ratio.clamp(0.0, 1.0);
    }

    /// Sequence number the next datagram will carry.
    pub fn next_sequence(&self) -> u32 {
        self.sequence
    }

    /// Serializes `frame` and splits it into FEC blocks. Returns `None` for a frame without
    /// any bitstream. Chunks larger than `max_payload` are split (zero-copy), so the result is
    /// MTU-safe whatever the encoder's chunk size is.
    pub fn plan(&self, frame: &EncodedFrame) -> Result<Option<FramePlan>, VideoSendError> {
        let Some(first) = frame.chunks.first() else {
            return Ok(None);
        };
        let base_meta = &first.meta;

        let mut pieces: Vec<Bytes> = Vec::with_capacity(frame.chunks.len());
        for chunk in &frame.chunks {
            let payload = &chunk.payload;
            if payload.len() <= self.max_payload {
                if !payload.is_empty() {
                    pieces.push(payload.clone());
                }
            } else {
                let mut offset = 0;
                while offset < payload.len() {
                    let end = (offset + self.max_payload).min(payload.len());
                    pieces.push(payload.slice(offset..end));
                    offset = end;
                }
            }
        }
        if pieces.is_empty() {
            return Ok(None);
        }
        if pieces.len() > u16::MAX as usize {
            return Err(VideoSendError::FrameTooLarge {
                chunks: pieces.len(),
            });
        }
        let total_chunks = pieces.len() as u16;

        let mut serialized: Vec<(Bytes, bool)> = Vec::with_capacity(pieces.len());
        for (index, payload) in pieces.into_iter().enumerate() {
            let meta = VideoChunkMeta {
                frame_id: frame.frame_id,
                chunk_index: index as u16,
                total_chunks,
                codec: base_meta.codec,
                is_keyframe: frame.is_keyframe,
                is_intra_refresh: frame.is_intra_refresh,
                width: base_meta.width,
                height: base_meta.height,
                fps: base_meta.fps,
                pts_us: base_meta.pts_us,
            };
            let bytes = VideoChunk::new(meta, payload).serialize()?;
            serialized.push((bytes, index + 1 == total_chunks as usize));
        }

        let source_count = serialized.len();
        let block_count = source_count.div_ceil(self.fec.group());
        let base = source_count / block_count;
        let extra = source_count % block_count;

        let mut blocks = VecDeque::with_capacity(block_count);
        let mut wire_bytes = 0;
        let mut max_block_wire_bytes = 0;
        let mut parity_count = 0;
        let mut iter = serialized.into_iter();
        for block_index in 0..block_count {
            let k = base + usize::from(block_index < extra);
            let chunks: Vec<(Bytes, bool)> = iter.by_ref().take(k).collect();
            let m = self.fec.parity_for(k);
            let max_len = chunks.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
            let mut block_bytes = chunks
                .iter()
                .map(|(c, _)| HEADER_SIZE + c.len())
                .sum::<usize>();
            // Parity datagram: header + FEC header + symbol (2-byte length prefix + data).
            block_bytes += m * (HEADER_SIZE + FEC_HEADER_SIZE + max_len + 2);
            wire_bytes += block_bytes;
            max_block_wire_bytes = max_block_wire_bytes.max(block_bytes);
            parity_count += m;
            blocks.push_back(PlannedBlock { chunks, parity: m });
        }

        Ok(Some(FramePlan {
            frame_id: frame.frame_id,
            fps: base_meta.fps,
            is_keyframe: frame.is_keyframe,
            is_intra_refresh: frame.is_intra_refresh,
            source_count,
            parity_count,
            wire_bytes,
            max_block_wire_bytes,
            blocks,
        }))
    }

    /// Builds the datagrams of the next FEC block of `plan`: its source packets in order,
    /// then its parity packets. Sequence numbers and the block id are assigned here, so a
    /// frame abandoned part-way leaves no gap in the sequence space. `now_us` is the sender's
    /// monotonic clock in microseconds.
    pub fn next_block(
        &mut self,
        plan: &mut FramePlan,
        now_us: u64,
    ) -> Result<Option<Vec<Datagram>>, VideoSendError> {
        let Some(block) = plan.blocks.pop_front() else {
            return Ok(None);
        };
        let k = block.chunks.len();
        let m = block.parity;
        let block_id = self.alloc_block_id(now_us);

        let mut frame_flags = 0;
        if plan.is_keyframe {
            frame_flags |= FLAG_KEYFRAME;
        }
        if plan.is_intra_refresh {
            frame_flags |= FLAG_INTRA_REFRESH;
        }
        if m > 0 {
            frame_flags |= FLAG_FEC_PROTECTED;
        }

        let mut out = Vec::with_capacity(k + m);
        for (index, (payload, is_last)) in block.chunks.iter().enumerate() {
            let mut flags = frame_flags;
            if *is_last {
                flags |= FLAG_LAST_CHUNK;
            }
            let mut header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                self.stream_id,
                self.take_sequence(),
                block_id,
                payload,
            )
            .with_flags(flags);
            if m > 0 {
                header = header.with_fec_index(index as u16);
            }
            out.push(Datagram {
                bytes: encode_datagram(&header, payload)?,
                is_parity: false,
            });
        }

        if m > 0 {
            let sources: Vec<Bytes> = block.chunks.iter().map(|(c, _)| c.clone()).collect();
            let parities = ReedSolomonFec::encode(block_id, &sources, m)?;
            for (p, parity) in parities.into_iter().enumerate() {
                let header = PacketHeader::new(
                    PacketType::FecParity,
                    self.stream_id,
                    self.take_sequence(),
                    block_id,
                    &parity,
                )
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index((k + p) as u16);
                out.push(Datagram {
                    bytes: encode_datagram(&header, &parity)?,
                    is_parity: true,
                });
            }
        }
        Ok(Some(out))
    }

    /// Plans and builds every datagram of `frame` at once. The sender itself goes block by
    /// block; this is for tests and tools.
    pub fn packetize(
        &mut self,
        frame: &EncodedFrame,
        now_us: u64,
    ) -> Result<Vec<Datagram>, VideoSendError> {
        let Some(mut plan) = self.plan(frame)? else {
            return Ok(Vec::new());
        };
        let mut all = Vec::with_capacity(plan.datagram_count());
        while let Some(block) = self.next_block(&mut plan, now_us)? {
            all.extend(block);
        }
        Ok(all)
    }

    fn take_sequence(&mut self) -> u32 {
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        seq
    }

    /// FEC block ids are the header timestamp; they must be unique and are kept increasing.
    fn alloc_block_id(&mut self, now_us: u64) -> u64 {
        let id = now_us.max(self.last_block_id + 1);
        self.last_block_id = id;
        id
    }
}

pub(super) fn encode_datagram(
    header: &PacketHeader,
    payload: &[u8],
) -> Result<Bytes, VideoSendError> {
    let size = HEADER_SIZE + payload.len();
    if size > MAX_DATAGRAM_SIZE {
        return Err(VideoSendError::DatagramTooLarge {
            size,
            max: MAX_DATAGRAM_SIZE,
        });
    }
    let mut buf = BytesMut::with_capacity(size);
    header.encode(&mut buf);
    buf.put_slice(payload);
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{Packet, VideoCodec};

    pub(crate) fn frame(frame_id: u64, bytes: usize, keyframe: bool, chunk: usize) -> EncodedFrame {
        let data: Vec<u8> = (0..bytes)
            .map(|i| (i as u64).wrapping_mul(31).wrapping_add(frame_id) as u8)
            .collect();
        let data = Bytes::from(data);
        let total = bytes.div_ceil(chunk).max(1);
        let chunks = (0..total)
            .map(|i| {
                let start = i * chunk;
                let end = (start + chunk).min(bytes);
                VideoChunk::new(
                    VideoChunkMeta {
                        frame_id,
                        chunk_index: i as u16,
                        total_chunks: total as u16,
                        codec: VideoCodec::Hevc,
                        is_keyframe: keyframe,
                        is_intra_refresh: false,
                        width: 1920,
                        height: 1080,
                        fps: 60,
                        pts_us: frame_id * 16_666,
                    },
                    data.slice(start..end),
                )
            })
            .collect();
        EncodedFrame {
            frame_id,
            is_keyframe: keyframe,
            is_intra_refresh: false,
            picture_type: if keyframe { 3 } else { 0 },
            chunks,
            encode_duration_us: 0,
        }
    }

    #[test]
    fn parity_ratio_rounding() {
        let fec = FecConfig::default();
        assert_eq!(fec.parity_for(1), 1, "a tiny block still gets one parity");
        assert_eq!(fec.parity_for(4), 1);
        assert_eq!(fec.parity_for(5), 2, "5 * 0.25 = 1.25 rounds up");
        assert_eq!(fec.parity_for(16), 4, "16 * 0.25 is exactly 4");
        assert_eq!(fec.parity_for(17), 5);
        assert_eq!(fec.parity_for(20), 5);
        let thirty = FecConfig {
            group_size: 30,
            parity_ratio: 0.1,
        };
        assert_eq!(thirty.parity_for(30), 3, "30 * 0.1 must not round up to 4");
        let off = FecConfig {
            parity_ratio: 0.0,
            ..FecConfig::default()
        };
        assert_eq!(off.parity_for(20), 0);
        let huge = FecConfig {
            group_size: 200,
            parity_ratio: 5.0,
        };
        assert!(200 + huge.parity_for(200) <= 256);
    }

    #[test]
    fn datagrams_follow_the_client_wire_layout() {
        let mut p = Packetizer::new(7, 1200, FecConfig::default());
        // 25 chunks: two balanced blocks of 13 and 12 sources, 4 and 3 parity (25% each).
        let f = frame(1, 25 * 1200 - 17, true, 1200);
        let datagrams = p.packetize(&f, 5_000).unwrap();
        assert_eq!(datagrams.len(), 25 + 7);

        let mut seqs = Vec::new();
        let mut block_ids = Vec::new();
        let mut sources = 0;
        let mut parities = 0;
        for (i, d) in datagrams.iter().enumerate() {
            assert!(d.bytes.len() <= MAX_DATAGRAM_SIZE);
            let pkt = Packet::from_bytes(&d.bytes).expect("checksum and header valid");
            assert_eq!(pkt.header.stream_id, 7);
            assert_eq!(
                pkt.header.sequence, i as u32,
                "consecutive sequence numbers"
            );
            assert!(pkt.header.flags & FLAG_FEC_PROTECTED != 0);
            if !d.is_parity {
                assert!(pkt.header.flags & FLAG_KEYFRAME != 0);
            }
            seqs.push(pkt.header.sequence);
            block_ids.push(pkt.header.timestamp_us);
            match pkt.header.packet_type {
                PacketType::VideoFrameChunk => {
                    sources += 1;
                    let chunk = VideoChunk::deserialize(&pkt.payload).unwrap();
                    assert_eq!(chunk.meta.frame_id, 1);
                    assert_eq!(chunk.meta.total_chunks, 25);
                    assert_eq!(chunk.meta.pts_us, 16_666);
                    assert!(chunk.payload.len() <= 1200);
                    let last = chunk.meta.chunk_index == 24;
                    assert_eq!(pkt.header.flags & FLAG_LAST_CHUNK != 0, last);
                }
                PacketType::FecParity => parities += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!((sources, parities), (25, 7));
        // Block ids: 13+4 datagrams share one, 12+3 share the next, strictly increasing.
        assert!(block_ids[..17].iter().all(|b| *b == block_ids[0]));
        assert!(block_ids[17..].iter().all(|b| *b == block_ids[17]));
        assert!(block_ids[17] > block_ids[0]);

        // FEC index is the position in the block; parity indices follow the sources.
        let idx = |n: usize| {
            Packet::from_bytes(&datagrams[n].bytes)
                .unwrap()
                .header
                .fec_index()
        };
        assert_eq!(idx(0), 0);
        assert_eq!(idx(12), 12);
        assert_eq!(idx(13), 13, "first parity of a 13-source block");
        assert_eq!(idx(14), 14);
        assert_eq!(idx(16), 16, "last parity of the 13-source block");
        assert_eq!(idx(17), 0, "next block restarts at 0");
        assert_eq!(idx(28), 11, "last source of the 12-source block");
        assert_eq!(idx(29), 12, "first parity of the 12-source block");
    }

    #[test]
    fn fec_off_sends_plain_chunks() {
        let mut p = Packetizer::new(
            0,
            1200,
            FecConfig {
                parity_ratio: 0.0,
                ..FecConfig::default()
            },
        );
        let datagrams = p.packetize(&frame(3, 5000, false, 1200), 1).unwrap();
        assert_eq!(datagrams.len(), 5);
        for d in &datagrams {
            let pkt = Packet::from_bytes(&d.bytes).unwrap();
            assert_eq!(pkt.header.flags & FLAG_FEC_PROTECTED, 0);
            assert!(!d.is_parity);
        }
    }

    #[test]
    fn oversized_encoder_chunks_are_split_to_the_mtu_safe_size() {
        let mut p = Packetizer::new(0, 1200, FecConfig::default());
        // Encoder produced 3 chunks of 4000 bytes each.
        let datagrams = p.packetize(&frame(1, 12_000, false, 4000), 1).unwrap();
        let sources: Vec<_> = datagrams.iter().filter(|d| !d.is_parity).collect();
        // Each 4000-byte chunk splits into 1200 + 1200 + 1200 + 400.
        assert_eq!(sources.len(), 12);
        for d in &datagrams {
            assert!(d.bytes.len() <= MAX_DATAGRAM_SIZE);
        }
        for d in sources {
            let pkt = Packet::from_bytes(&d.bytes).unwrap();
            let chunk = VideoChunk::deserialize(&pkt.payload).unwrap();
            assert!(chunk.payload.len() <= 1200);
            assert_eq!(chunk.meta.total_chunks as usize, 12);
        }
    }

    #[test]
    fn largest_datagrams_fit_the_mtu_limit() {
        let mut p = Packetizer::new(0, usize::MAX, FecConfig::default());
        let datagrams = p.packetize(&frame(1, 40_000, false, 40_000), 1).unwrap();
        let max = datagrams.iter().map(|d| d.bytes.len()).max().unwrap();
        assert!(max <= MAX_DATAGRAM_SIZE, "largest datagram {max}");
    }

    #[test]
    fn block_ids_are_unique_even_with_a_stalled_clock() {
        let mut p = Packetizer::new(0, 1200, FecConfig::default());
        let mut ids = Vec::new();
        for n in 0..4 {
            for d in p
                .packetize(&frame(n + 1, 30_000, false, 1200), 100)
                .unwrap()
            {
                let h = Packet::from_bytes(&d.bytes).unwrap().header;
                if ids.last() != Some(&h.timestamp_us) {
                    ids.push(h.timestamp_us);
                }
            }
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, ids, "each block has its own increasing id");
    }

    #[test]
    fn empty_frame_produces_no_datagrams() {
        let mut p = Packetizer::new(0, 1200, FecConfig::default());
        let mut f = frame(1, 10, false, 1200);
        f.chunks.clear();
        assert!(p.packetize(&f, 1).unwrap().is_empty());
        assert_eq!(p.next_sequence(), 0);
    }

    #[test]
    fn aborting_a_plan_leaves_no_sequence_gap() {
        let mut p = Packetizer::new(0, 1200, FecConfig::default());
        let mut plan = p.plan(&frame(1, 50 * 1200, false, 1200)).unwrap().unwrap();
        let first = p.next_block(&mut plan, 1).unwrap().unwrap();
        assert!(!plan.is_exhausted());
        drop(plan); // abandoned
        let next = p.packetize(&frame(2, 1000, false, 1200), 2).unwrap();
        let seq = |d: &Datagram| Packet::from_bytes(&d.bytes).unwrap().header.sequence;
        assert_eq!(seq(&next[0]), seq(first.last().unwrap()) + 1);
    }

    #[test]
    fn wire_bytes_estimate_is_exact() {
        let mut p = Packetizer::new(0, 1200, FecConfig::default());
        let f = frame(1, 61_000, false, 1200);
        let plan = p.plan(&f).unwrap().unwrap();
        let estimate = plan.wire_bytes;
        let datagrams = p.packetize(&f, 1).unwrap();
        let actual: usize = datagrams.iter().map(|d| d.bytes.len()).sum();
        assert_eq!(estimate, actual);
    }
}
