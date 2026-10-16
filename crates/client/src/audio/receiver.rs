//! Audio receiver featuring an adaptive jitter buffer keyed by packet sequence numbers.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use linux_quest_protocol::audio::AudioPacket;
use linux_quest_protocol::packet::{Packet, PacketType};

use crate::error::ClientResult;

/// Base offset for 64-bit continuous sequence number unwrapping.
/// Initialized to 2^32 so that differences backwards from the initial sequence
/// can never underflow `u64` even under severe packet reordering.
const SEQUENCE_EPOCH_OFFSET: u64 = 1u64 << 32;

/// Configuration options for the audio jitter buffer and receiver.
#[derive(Debug, Clone, Copy)]
pub struct AudioReceiverConfig {
    /// Minimum playout delay buffer (default: 2.5ms for ultra-low latency).
    pub min_delay: Duration,
    /// Maximum playout delay buffer to prevent audio lag accumulation (default: 30ms).
    pub max_delay: Duration,
    /// Multiplier applied to RFC 3550 inter-arrival jitter estimate (default: 2.0).
    pub jitter_multiplier: f64,
    /// Maximum number of audio frames held in memory (default: 256).
    pub max_queue_depth: usize,
    /// Time to wait for a missing sequence before declaring packet lost and triggering PLC (default: 5ms).
    pub loss_hold_timeout: Duration,
}

impl Default for AudioReceiverConfig {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_micros(2_500), // 2.5ms
            max_delay: Duration::from_millis(30),    // 30ms cap
            jitter_multiplier: 2.0,
            max_queue_depth: 256,
            loss_hold_timeout: Duration::from_millis(5),
        }
    }
}

/// Statistics collected by the audio jitter buffer and receiver.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AudioReceiverStats {
    /// Total audio packets ingested.
    pub packets_received: u64,
    /// Audio packets successfully delivered for playout.
    pub packets_delivered: u64,
    /// Missing packets declared permanently lost (triggered PLC).
    pub packets_lost: u64,
    /// Late packets dropped because their playout deadline had already passed.
    pub packets_late: u64,
    /// Duplicate packets rejected.
    pub packets_duplicate: u64,
    /// Smoothed inter-arrival jitter in microseconds (RFC 3550).
    pub current_jitter_us: f64,
    /// Current calculated target playout delay.
    pub current_target_delay: Duration,
}

impl AudioReceiverStats {
    /// Packet loss rate as a ratio in `[0.0, 1.0]`.
    pub fn loss_rate(&self) -> f64 {
        let total = self.packets_received + self.packets_lost;
        if total == 0 {
            0.0
        } else {
            self.packets_lost as f64 / total as f64
        }
    }
}

/// Item popped from the audio jitter buffer ready for decode / playout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPlayoutItem {
    /// An intact audio packet ready to be decoded.
    Packet(AudioPacket),
    /// A missing audio packet that was declared lost, indicating Packet Loss Concealment (PLC) should run.
    Lost {
        /// Sequence number of the lost frame.
        sequence: u32,
        /// Estimated capture timestamp in microseconds.
        pts_us: u64,
        /// Expected samples per channel.
        samples_per_channel: u16,
    },
}

struct QueuedAudioPacket {
    packet: AudioPacket,
    arrival_instant: Instant,
}

/// Adaptive jitter buffer keyed by 32-bit sequence numbers with 64-bit unwrapping,
/// RFC 3550 jitter smoothing, reordering recovery, and PLC trigger deadlines.
pub struct AudioJitterBuffer {
    config: AudioReceiverConfig,
    queue: BTreeMap<u64, QueuedAudioPacket>,
    initial_seq: Option<u32>,
    highest_seen_unwrapped: Option<u64>,
    expected_unwrapped: Option<u64>,
    gap_hold_anchor: Option<(u64, Instant)>,

    last_arrival_instant: Option<Instant>,
    last_pts_us: Option<u64>,
    last_emitted_pts_us: u64,
    last_samples_per_channel: u16,
    last_sample_rate: u32,

    jitter_us: f64,
    stats: AudioReceiverStats,
}

impl AudioJitterBuffer {
    /// Creates a new audio jitter buffer with the given configuration.
    pub fn new(config: AudioReceiverConfig) -> Self {
        Self {
            config,
            queue: BTreeMap::new(),
            initial_seq: None,
            highest_seen_unwrapped: None,
            expected_unwrapped: None,
            gap_hold_anchor: None,
            last_arrival_instant: None,
            last_pts_us: None,
            last_emitted_pts_us: 0,
            last_samples_per_channel: 120, // default 2.5ms @ 48kHz
            last_sample_rate: 48000,
            jitter_us: 0.0,
            stats: AudioReceiverStats::default(),
        }
    }

    /// Unwraps a 32-bit sequence number into a monotonic 64-bit sequence number.
    pub fn unwrap_sequence(&mut self, seq: u32) -> u64 {
        match self.highest_seen_unwrapped {
            None => {
                self.initial_seq = Some(seq);
                let unwrapped = SEQUENCE_EPOCH_OFFSET;
                self.highest_seen_unwrapped = Some(unwrapped);
                unwrapped
            }
            Some(highest_unwrapped) => {
                let initial = self.initial_seq.unwrap_or(seq);
                let highest_32 =
                    initial.wrapping_add((highest_unwrapped - SEQUENCE_EPOCH_OFFSET) as u32);
                let delta = (seq as i32).wrapping_sub(highest_32 as i32) as i64;
                let candidate = (highest_unwrapped as i64 + delta) as u64;

                if candidate > highest_unwrapped {
                    self.highest_seen_unwrapped = Some(candidate);
                }
                candidate
            }
        }
    }

    /// Converts an unwrapped 64-bit sequence number back to its wire 32-bit sequence number.
    pub fn wrap_sequence(&self, unwrapped: u64) -> u32 {
        let initial = self.initial_seq.unwrap_or(0);
        initial.wrapping_add(unwrapped.wrapping_sub(SEQUENCE_EPOCH_OFFSET) as u32)
    }

    /// Dynamically computes the target playout delay.
    pub fn current_target_delay(&self) -> Duration {
        let calculated = self.jitter_us * self.config.jitter_multiplier;
        let clamped_us = (calculated as u64)
            .max(self.config.min_delay.as_micros() as u64)
            .min(self.config.max_delay.as_micros() as u64);
        Duration::from_micros(clamped_us)
    }

    /// Ingests an audio packet into the jitter buffer at arrival timestamp `now`.
    pub fn ingest_packet(&mut self, packet: AudioPacket, now: Instant) {
        self.stats.packets_received += 1;

        // Remember audio parameters for lost frame estimation
        if packet.meta.sample_rate > 0 {
            self.last_sample_rate = packet.meta.sample_rate;
        }
        if packet.meta.samples_per_channel > 0 {
            self.last_samples_per_channel = packet.meta.samples_per_channel;
        }

        // Update RFC 3550 inter-arrival jitter
        if let (Some(last_arrival), Some(last_pts)) = (self.last_arrival_instant, self.last_pts_us)
        {
            let arrival_delta_us = now.duration_since(last_arrival).as_micros() as f64;
            let pts_delta_us = packet.meta.pts_us.saturating_sub(last_pts) as f64;
            let diff = (arrival_delta_us - pts_delta_us).abs();
            // J = J + (|D| - J) / 16.0
            self.jitter_us += (diff - self.jitter_us) / 16.0;
        }
        self.last_arrival_instant = Some(now);
        self.last_pts_us = Some(packet.meta.pts_us);

        let unwrapped_seq = self.unwrap_sequence(packet.meta.sequence);

        // Check if packet arrived late (playout cursor has already advanced past it)
        if let Some(expected) = self.expected_unwrapped {
            if unwrapped_seq < expected {
                self.stats.packets_late += 1;
                return;
            }
        }

        // Check for duplicates
        if self.queue.contains_key(&unwrapped_seq) {
            self.stats.packets_duplicate += 1;
            return;
        }

        // Insert into queue
        self.queue.insert(
            unwrapped_seq,
            QueuedAudioPacket {
                packet,
                arrival_instant: now,
            },
        );

        // Protect against unbounded queue growth under severe stall
        if self.queue.len() > self.config.max_queue_depth {
            if let Some(&oldest_key) = self.queue.keys().next() {
                self.queue.remove(&oldest_key);
                self.stats.packets_lost += 1;
            }
        }
    }

    /// Pops the next ready audio frame or packet-loss event whose playout delay has elapsed.
    pub fn pop_ready_item(&mut self, now: Instant) -> Option<AudioPlayoutItem> {
        let target_delay = self.current_target_delay();
        self.stats.current_jitter_us = self.jitter_us;
        self.stats.current_target_delay = target_delay;

        // Clean out any stale packets behind cursor
        if let Some(expected) = self.expected_unwrapped {
            while let Some(&first_key) = self.queue.keys().next() {
                if first_key < expected {
                    self.queue.remove(&first_key);
                } else {
                    break;
                }
            }
        }

        let &first_unwrapped_seq = self.queue.keys().next()?;

        // Playout cursor initialization
        let expected = match self.expected_unwrapped {
            Some(exp) => exp,
            None => {
                // Initialize cursor to the first received packet
                self.expected_unwrapped = Some(first_unwrapped_seq);
                first_unwrapped_seq
            }
        };

        if first_unwrapped_seq == expected {
            // Expected packet is present in queue!
            let arrival = self.queue.get(&first_unwrapped_seq)?.arrival_instant;
            let age = now.duration_since(arrival);

            if age >= target_delay {
                let queued = self.queue.remove(&first_unwrapped_seq)?;
                self.expected_unwrapped = Some(expected + 1);
                self.gap_hold_anchor = None;
                self.stats.packets_delivered += 1;

                self.last_emitted_pts_us = queued.packet.meta.pts_us;
                self.last_samples_per_channel = queued.packet.meta.samples_per_channel;

                return Some(AudioPlayoutItem::Packet(queued.packet));
            } else {
                // Target jitter delay not yet elapsed
                return None;
            }
        }

        // first_unwrapped_seq > expected: Sequence hole / packet loss gap!
        let gap_start = match self.gap_hold_anchor {
            Some((seq, start)) if seq == expected => start,
            _ => {
                self.gap_hold_anchor = Some((expected, now));
                now
            }
        };

        // Determine if hold timeout for missing packet has expired
        let future_packet_arrival = self.queue.get(&first_unwrapped_seq)?.arrival_instant;
        let future_packet_age = now.duration_since(future_packet_arrival);

        let timeout_expired = now.duration_since(gap_start) >= self.config.loss_hold_timeout
            || future_packet_age >= target_delay + self.config.loss_hold_timeout;

        if timeout_expired {
            // Declare sequence `expected` lost and advance cursor
            self.expected_unwrapped = Some(expected + 1);
            self.gap_hold_anchor = None;
            self.stats.packets_lost += 1;

            let frame_duration_us = (self.last_samples_per_channel as u64 * 1_000_000)
                / self.last_sample_rate.max(1) as u64;
            self.last_emitted_pts_us += frame_duration_us;

            Some(AudioPlayoutItem::Lost {
                sequence: self.wrap_sequence(expected),
                pts_us: self.last_emitted_pts_us,
                samples_per_channel: self.last_samples_per_channel,
            })
        } else {
            // Wait for missing packet or reordered packet
            None
        }
    }

    /// Current expected sequence number (cursor).
    pub fn expected_sequence(&self) -> Option<u32> {
        self.expected_unwrapped.map(|s| self.wrap_sequence(s))
    }

    /// Current number of packets buffered in the jitter queue.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Current receiver stats.
    pub fn stats(&self) -> &AudioReceiverStats {
        &self.stats
    }

    /// Resets all jitter buffer state and statistics.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.initial_seq = None;
        self.highest_seen_unwrapped = None;
        self.expected_unwrapped = None;
        self.gap_hold_anchor = None;
        self.last_arrival_instant = None;
        self.last_pts_us = None;
        self.jitter_us = 0.0;
        self.stats = AudioReceiverStats::default();
    }
}

/// Client audio network receiver managing packet demuxing and jitter buffering.
pub struct AudioReceiver {
    jitter_buffer: AudioJitterBuffer,
}

impl AudioReceiver {
    /// Creates a new audio receiver with the given configuration.
    pub fn new(config: AudioReceiverConfig) -> Self {
        Self {
            jitter_buffer: AudioJitterBuffer::new(config),
        }
    }

    /// Ingests a raw transport packet.
    ///
    /// If the packet is an `AudioFrame`, deserializes `AudioPacket` and ingests it.
    /// Returns `Ok(true)` if handled, `Ok(false)` if not an audio packet, or `Err` on deserialization failure.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) -> ClientResult<bool> {
        if packet.header.packet_type == PacketType::AudioFrame {
            let audio_packet = AudioPacket::deserialize(&packet.payload)?;
            self.jitter_buffer.ingest_packet(audio_packet, now);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Ingests an already deserialized audio packet directly.
    pub fn ingest_audio_packet(&mut self, packet: AudioPacket, now: Instant) {
        self.jitter_buffer.ingest_packet(packet, now);
    }

    /// Pops the next ready audio frame or packet loss event.
    pub fn pop_ready_item(&mut self, now: Instant) -> Option<AudioPlayoutItem> {
        self.jitter_buffer.pop_ready_item(now)
    }

    /// Receiver and jitter buffer statistics.
    pub fn stats(&self) -> &AudioReceiverStats {
        self.jitter_buffer.stats()
    }

    /// Calculated target playout delay.
    pub fn target_delay(&self) -> Duration {
        self.jitter_buffer.current_target_delay()
    }

    /// Number of audio packets currently queued.
    pub fn queue_len(&self) -> usize {
        self.jitter_buffer.queue_len()
    }

    /// Resets the audio receiver.
    pub fn reset(&mut self) {
        self.jitter_buffer.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::audio::{AudioCodec, AudioFrameMeta};

    fn make_test_audio_packet(seq: u32, pts_us: u64) -> AudioPacket {
        let meta = AudioFrameMeta {
            frame_id: seq as u64,
            sequence: seq,
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 120, // 2.5ms
            codec: AudioCodec::Opus,
            pts_us,
        };
        AudioPacket::new(meta, Bytes::from_static(&[0xFC, 0x01, 0x02, 0x03]))
    }

    #[test]
    fn test_audio_jitter_buffer_in_order_delivery() {
        let config = AudioReceiverConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            loss_hold_timeout: Duration::from_millis(10),
            ..Default::default()
        };
        let mut jb = AudioJitterBuffer::new(config);

        let t0 = Instant::now();
        jb.ingest_packet(make_test_audio_packet(1, 1000), t0);
        jb.ingest_packet(
            make_test_audio_packet(2, 3500),
            t0 + Duration::from_micros(2500),
        );

        // Before min delay: nothing ready
        assert!(jb.pop_ready_item(t0 + Duration::from_millis(2)).is_none());

        // After min delay: packet 1 ready
        let item1 = jb
            .pop_ready_item(t0 + Duration::from_millis(6))
            .expect("Packet 1 not ready");
        match item1 {
            AudioPlayoutItem::Packet(p) => assert_eq!(p.meta.sequence, 1),
            _ => panic!("Expected packet 1"),
        }

        // Playout packet 2
        let item2 = jb
            .pop_ready_item(t0 + Duration::from_millis(9))
            .expect("Packet 2 not ready");
        match item2 {
            AudioPlayoutItem::Packet(p) => assert_eq!(p.meta.sequence, 2),
            _ => panic!("Expected packet 2"),
        }

        assert_eq!(jb.stats().packets_delivered, 2);
        assert_eq!(jb.stats().packets_lost, 0);
    }

    #[test]
    fn test_audio_jitter_buffer_reordering() {
        let config = AudioReceiverConfig {
            min_delay: Duration::from_millis(5),
            ..Default::default()
        };
        let mut jb = AudioJitterBuffer::new(config);

        let t0 = Instant::now();
        // Ingest sequence 2 before sequence 1 (out of order)
        jb.ingest_packet(make_test_audio_packet(2, 3500), t0);
        jb.ingest_packet(
            make_test_audio_packet(1, 1000),
            t0 + Duration::from_millis(1),
        );

        let t_play = t0 + Duration::from_millis(8);

        // Sequence 1 should be popped first
        let item1 = jb.pop_ready_item(t_play).expect("Item 1 not ready");
        match item1 {
            AudioPlayoutItem::Packet(p) => assert_eq!(p.meta.sequence, 1),
            _ => panic!("Expected packet 1"),
        }

        // Sequence 2 popped next
        let item2 = jb.pop_ready_item(t_play).expect("Item 2 not ready");
        match item2 {
            AudioPlayoutItem::Packet(p) => assert_eq!(p.meta.sequence, 2),
            _ => panic!("Expected packet 2"),
        }

        assert_eq!(jb.stats().packets_delivered, 2);
    }

    #[test]
    fn test_audio_jitter_buffer_loss_detection_and_plc_trigger() {
        let config = AudioReceiverConfig {
            min_delay: Duration::from_millis(2),
            loss_hold_timeout: Duration::from_millis(5),
            ..Default::default()
        };
        let mut jb = AudioJitterBuffer::new(config);

        let t0 = Instant::now();
        // Packet 1 arrives
        jb.ingest_packet(make_test_audio_packet(1, 1000), t0);
        // Packet 2 is lost!
        // Packet 3 arrives
        jb.ingest_packet(
            make_test_audio_packet(3, 6000),
            t0 + Duration::from_millis(2),
        );

        // Pop packet 1
        let item1 = jb.pop_ready_item(t0 + Duration::from_millis(3)).unwrap();
        assert!(matches!(item1, AudioPlayoutItem::Packet(p) if p.meta.sequence == 1));

        // Attempt pop before loss_hold_timeout: waiting for missing packet 2
        assert!(jb.pop_ready_item(t0 + Duration::from_millis(5)).is_none());

        // After loss_hold_timeout: packet 2 should be declared Lost (triggers PLC)
        let item2 = jb
            .pop_ready_item(t0 + Duration::from_millis(9))
            .expect("Loss event not triggered");
        match item2 {
            AudioPlayoutItem::Lost { sequence, .. } => assert_eq!(sequence, 2),
            _ => panic!("Expected Lost event for sequence 2"),
        }

        // Now packet 3 is popped immediately
        let item3 = jb.pop_ready_item(t0 + Duration::from_millis(9)).unwrap();
        match item3 {
            AudioPlayoutItem::Packet(p) => assert_eq!(p.meta.sequence, 3),
            _ => panic!("Expected packet 3"),
        }

        assert_eq!(jb.stats().packets_delivered, 2);
        assert_eq!(jb.stats().packets_lost, 1);
        assert!(jb.stats().loss_rate() > 0.0);
    }

    #[test]
    fn test_audio_jitter_buffer_duplicate_and_late_packet_handling() {
        let config = AudioReceiverConfig {
            min_delay: Duration::from_millis(2),
            ..Default::default()
        };
        let mut jb = AudioJitterBuffer::new(config);

        let t0 = Instant::now();
        jb.ingest_packet(make_test_audio_packet(1, 1000), t0);
        // Ingest duplicate 1
        jb.ingest_packet(make_test_audio_packet(1, 1000), t0);
        assert_eq!(jb.stats().packets_duplicate, 1);

        // Pop packet 1
        let _ = jb.pop_ready_item(t0 + Duration::from_millis(3)).unwrap();

        // Ingest late packet 1 again after cursor has advanced past it
        jb.ingest_packet(
            make_test_audio_packet(1, 1000),
            t0 + Duration::from_millis(4),
        );
        assert_eq!(jb.stats().packets_late, 1);
    }

    #[test]
    fn test_audio_jitter_buffer_sequence_wrap_around() {
        let mut jb = AudioJitterBuffer::new(AudioReceiverConfig::default());

        let seq_max = u32::MAX;
        let unwrapped_max = jb.unwrap_sequence(seq_max);

        let seq_next = 0u32;
        let unwrapped_next = jb.unwrap_sequence(seq_next);

        // Monotonic unwrapped sequence must advance smoothly across wrap boundary
        assert_eq!(unwrapped_next, unwrapped_max + 1);
    }

    #[test]
    fn test_audio_receiver_packet_demuxing() {
        let mut rx = AudioReceiver::new(AudioReceiverConfig::default());
        let audio_pkt = make_test_audio_packet(10, 50_000);
        let serialized = audio_pkt.serialize().unwrap();

        let header = linux_quest_protocol::packet::PacketHeader::new(
            PacketType::AudioFrame,
            1,
            10,
            50_000,
            &serialized,
        );
        let net_pkt = Packet::new(header, serialized);

        let arrival = Instant::now();
        assert!(rx.ingest_packet(net_pkt, arrival).unwrap());
        assert_eq!(rx.queue_len(), 1);

        // Feed non-audio packet
        let ping_hdr =
            linux_quest_protocol::packet::PacketHeader::new(PacketType::Ping, 1, 11, 51_000, &[]);
        let ping_pkt = Packet::new(ping_hdr, Bytes::new());
        assert!(!rx.ingest_packet(ping_pkt, arrival).unwrap());
        assert_eq!(rx.queue_len(), 1); // Not queued
    }
}
