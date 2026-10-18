use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType, FLAG_FEC_PROTECTED};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::fec::{reed_solomon::ReedSolomonFec, xor::XorFec, FecHeader, FecScheme};

#[derive(Debug, Clone, Copy)]
pub struct JitterBufferConfig {
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub jitter_multiplier: f64,
    pub rtt_multiplier: f64,
    pub max_queue_depth: usize,
    pub fec_hold_timeout: Duration,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_micros(1_500), // 1.5ms for low-latency streaming
            max_delay: Duration::from_micros(20_000), // 20ms cap to prevent lag accumulation
            jitter_multiplier: 2.0,                  // 2-sigma jitter guard
            rtt_multiplier: 0.1,                     // 10% RTT contribution
            max_queue_depth: 1024, // Cap queue depth to prevent unbounded memory growth
            fec_hold_timeout: Duration::from_millis(10), // Wait up to 10ms for FEC parity recovery
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct JitterBufferStats {
    pub current_jitter_us: f64,
    pub smoothed_rtt_us: f64,
    pub current_target_delay_us: u64,
    pub packets_received: u64,
    pub packets_delivered: u64,
    pub packets_lost: u64,
    pub packets_reconstructed_fec: u64,
    /// Times the sender's sequence jumped far backwards (the host restarted its stream) and
    /// the buffer started over.
    pub stream_restarts: u64,
}

impl JitterBufferStats {
    pub fn loss_rate(&self) -> f64 {
        let total = self.packets_received + self.packets_lost;
        if total == 0 {
            0.0
        } else {
            self.packets_lost as f64 / total as f64
        }
    }
}

struct QueuedPacket {
    packet: Packet,
    arrival_instant: Instant,
}

struct FecBlockTracker {
    scheme: FecScheme,
    k: usize,
    m: usize,
    base_sequence: Option<u32>,
    unwrapped_base_seq: Option<u64>,
    first_arrival: Instant,
    sources: HashMap<usize, (u32, Packet)>, // index_in_block -> (seq, Packet)
    parities: HashMap<usize, Bytes>,        // parity_index -> raw_bytes
}

impl FecBlockTracker {
    fn is_complete(&self) -> bool {
        if self.k == 0 {
            return false;
        }
        (0..self.k).all(|i| self.sources.contains_key(&i))
    }

    fn prune_invalid_indices(&mut self) {
        if self.k > 0 {
            self.sources.retain(|&idx, _| idx < self.k);
        }
        if self.m > 0 {
            self.parities.retain(|&idx, _| idx < self.m);
        }
    }
}

/// Base offset used for continuous 64-bit sequence number unwrapping.
/// Starts at 2^32 so that differences backwards from the initial sequence
/// can never underflow u64 even under severe packet reordering.
/// A packet this many sequence numbers behind the playout cursor means the sender restarted.
/// Real reordering is a few packets; at 10,000 datagrams/s this is over 6 s of traffic.
const STREAM_RESTART_BEHIND: u64 = 1 << 16;

const SEQUENCE_EPOCH_OFFSET: u64 = 1u64 << 32;

/// Maximum number of active FEC blocks tracked simultaneously in memory.
///
/// Parity packets are identified across sequence gaps either via active tracked FEC blocks
/// in `fec_blocks` or via explicit retention in `received_parity_ranges` (bounded up to 2048 ranges).
/// Even if an ancient FEC block is evicted or pruned once playout advances past its sequence range,
/// parity sequence numbers that were actually received remain tracked to prevent miscounting
/// parity packet sequences as lost media packets.
pub const MAX_FEC_BLOCKS: usize = 64;

/// Maximum number of disjoint parity sequence ranges tracked simultaneously in memory.
pub const MAX_PARITY_RANGES: usize = 2048;

/// Maximum number of source packets (K) allowed per FEC block in GF(2^8).
pub const MAX_FEC_K: usize = 256;

/// Dynamic, adaptive jitter buffer with RFC 3550 jitter calculation and integrated FEC recovery.
pub struct AdaptiveJitterBuffer {
    config: JitterBufferConfig,
    jitter_us: f64,
    smoothed_rtt_us: f64,
    last_arrival_instant: Option<Instant>,
    last_sender_timestamp_us: Option<u64>,
    highest_seen_unwrapped: Option<u64>,
    expected_unwrapped: Option<u64>,
    fec_hold_anchor: Option<(u64, Instant)>,
    received_parity_ranges: BTreeMap<u64, u64>,

    queue: BTreeMap<u64, QueuedPacket>,
    fec_blocks: HashMap<u64, FecBlockTracker>,

    stats: JitterBufferStats,
}

impl AdaptiveJitterBuffer {
    pub fn new(config: JitterBufferConfig) -> Self {
        Self {
            config,
            jitter_us: 0.0,
            smoothed_rtt_us: 0.0,
            last_arrival_instant: None,
            last_sender_timestamp_us: None,
            highest_seen_unwrapped: None,
            expected_unwrapped: None,
            fec_hold_anchor: None,
            received_parity_ranges: BTreeMap::new(),
            queue: BTreeMap::new(),
            fec_blocks: HashMap::new(),
            stats: JitterBufferStats::default(),
        }
    }

    /// Returns the currently expected 32-bit sequence number (cursor), if initialized.
    pub fn expected_sequence(&self) -> Option<u32> {
        self.expected_unwrapped.map(|s| s as u32)
    }

    #[cfg(test)]
    pub(crate) fn set_expected_sequence(&mut self, seq: u32) {
        let unwrapped = self.unwrap_sequence(seq);
        self.expected_unwrapped = Some(unwrapped);
    }

    #[cfg(test)]
    pub(crate) fn received_parity_range_count(&self) -> usize {
        self.received_parity_ranges.len()
    }

    /// Records a received parity sequence number into contiguous merged ranges, bounded at MAX_PARITY_RANGES.
    fn record_received_parity(&mut self, seq: u64) {
        if let Some(expected) = self.expected_unwrapped {
            if seq < expected {
                return;
            }
        }

        let mut merged_start = seq;
        let mut merged_end = seq + 1;

        if let Some((&start, &end)) = self.received_parity_ranges.range(..=seq).next_back() {
            if seq < end {
                return;
            } else if end == seq {
                merged_start = start;
                self.received_parity_ranges.remove(&start);
            }
        }

        if let Some((&next_start, &next_end)) =
            self.received_parity_ranges.range(merged_end..).next()
        {
            if next_start == merged_end {
                merged_end = next_end;
                self.received_parity_ranges.remove(&next_start);
            }
        }

        self.received_parity_ranges.insert(merged_start, merged_end);

        if self.received_parity_ranges.len() > MAX_PARITY_RANGES {
            if let Some(expected) = self.expected_unwrapped {
                self.prune_parity_ranges(expected);
            }
            while self.received_parity_ranges.len() > MAX_PARITY_RANGES {
                if let Some(&oldest_start) = self.received_parity_ranges.keys().next() {
                    self.received_parity_ranges.remove(&oldest_start);
                } else {
                    break;
                }
            }
        }
    }

    /// Prunes parity ranges where `end <= expected`, and trims any range straddling `expected`.
    fn prune_parity_ranges(&mut self, expected: u64) {
        while let Some((&start, &end)) = self.received_parity_ranges.iter().next() {
            if end <= expected {
                self.received_parity_ranges.remove(&start);
            } else if start < expected {
                self.received_parity_ranges.remove(&start);
                self.received_parity_ranges.insert(expected, end);
                break;
            } else {
                break;
            }
        }
    }

    /// Unwraps 32-bit wire sequence numbers into a monotonically increasing 64-bit sequence space.
    fn unwrap_sequence(&mut self, seq: u32) -> u64 {
        match self.highest_seen_unwrapped {
            None => {
                let unwrapped = SEQUENCE_EPOCH_OFFSET + (seq as u64);
                self.highest_seen_unwrapped = Some(unwrapped);
                unwrapped
            }
            Some(highest) => {
                let highest_wire = highest as u32;
                let diff = seq.wrapping_sub(highest_wire) as i32 as i64;
                let unwrapped = (highest as i64 + diff) as u64;
                if unwrapped > highest {
                    self.highest_seen_unwrapped = Some(unwrapped);
                }
                unwrapped
            }
        }
    }

    /// Forgets all sequence, FEC and queue state, keeping the statistics.
    fn restart_stream(&mut self) {
        tracing::info!(
            expected = ?self.expected_unwrapped,
            "video sequence jumped far backwards; treating it as a new stream"
        );
        self.last_arrival_instant = None;
        self.last_sender_timestamp_us = None;
        self.highest_seen_unwrapped = None;
        self.expected_unwrapped = None;
        self.fec_hold_anchor = None;
        self.received_parity_ranges.clear();
        self.queue.clear();
        self.fec_blocks.clear();
        self.stats.stream_restarts += 1;
    }

    pub fn stats(&self) -> &JitterBufferStats {
        &self.stats
    }

    /// Update smoothed RTT estimate via Exponentially Weighted Moving Average (EWMA, alpha = 0.125).
    pub fn update_rtt(&mut self, rtt_sample: Duration) {
        let sample_us = rtt_sample.as_micros() as f64;
        if self.smoothed_rtt_us == 0.0 {
            self.smoothed_rtt_us = sample_us;
        } else {
            // EWMA with alpha = 0.125 / Jacobson's algorithm (RFC 6298: (1 - alpha) * SRTT + alpha * sample)
            self.smoothed_rtt_us = 0.875 * self.smoothed_rtt_us + 0.125 * sample_us;
        }
        self.stats.smoothed_rtt_us = self.smoothed_rtt_us;
    }

    /// Computes the dynamically adjusted playout delay target based on current network jitter and RTT.
    pub fn current_target_delay(&self) -> Duration {
        let jitter_contrib = self.jitter_us * self.config.jitter_multiplier;
        let rtt_contrib = self.smoothed_rtt_us * self.config.rtt_multiplier;
        let dynamic_us = (jitter_contrib + rtt_contrib).round() as u64;

        let dynamic_dur = Duration::from_micros(dynamic_us);
        dynamic_dur.clamp(self.config.min_delay, self.config.max_delay)
    }

    /// Ingests an arriving packet into the jitter buffer, updating jitter statistics and FEC blocks.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        self.stats.packets_received += 1;

        // Handle FEC parity packet first without polluting media RFC 3550 jitter calculation
        if packet.header.packet_type == PacketType::FecParity {
            let seq = packet.header.sequence;
            let unwrapped_seq = self.unwrap_sequence(seq);
            self.record_received_parity(unwrapped_seq);

            let mut slice = &packet.payload[..];
            if let Ok(fec_header) = FecHeader::decode(&mut slice) {
                if !self.fec_blocks.contains_key(&fec_header.block_id)
                    && self.fec_blocks.len() >= MAX_FEC_BLOCKS
                {
                    if let Some(&oldest) = self.fec_blocks.keys().min() {
                        self.fec_blocks.remove(&oldest);
                    }
                }

                let src_cnt = fec_header.source_count as usize;
                let par_cnt = fec_header.parity_count as usize;
                let p_idx = fec_header.parity_index as usize;

                // Validate FEC dimensions: k >= 1, m >= 1, k + m <= 256, parity_index < m
                if src_cnt == 0 || par_cnt == 0 || src_cnt + par_cnt > MAX_FEC_K || p_idx >= par_cnt
                {
                    return;
                }

                let tracker = self
                    .fec_blocks
                    .entry(fec_header.block_id)
                    .or_insert_with(|| FecBlockTracker {
                        scheme: fec_header.scheme,
                        k: src_cnt,
                        m: par_cnt,
                        base_sequence: None,
                        unwrapped_base_seq: None,
                        first_arrival: now,
                        sources: HashMap::new(),
                        parities: HashMap::new(),
                    });

                tracker.scheme = fec_header.scheme;
                tracker.k = src_cnt;
                tracker.m = par_cnt;
                tracker.prune_invalid_indices();
                if tracker.parities.len() < tracker.m {
                    tracker
                        .parities
                        .entry(p_idx)
                        .or_insert(packet.payload.clone());
                    self.try_fec_recovery(fec_header.block_id, now);
                }
            }
            return;
        }

        let seq = packet.header.sequence;
        let mut unwrapped_seq = self.unwrap_sequence(seq);

        // A packet far behind the playout cursor is not reordering: the sender restarted and
        // its sequence started over. Without this every new packet would be stale until the
        // new sequence caught up with the old one, hours later.
        if self
            .expected_unwrapped
            .is_some_and(|expected| unwrapped_seq.saturating_add(STREAM_RESTART_BEHIND) < expected)
        {
            self.restart_stream();
            unwrapped_seq = self.unwrap_sequence(seq);
        }

        let is_stale = match self.expected_unwrapped {
            Some(expected) => unwrapped_seq < expected,
            None => false,
        };

        if is_stale {
            // Discard packets older than expected playout cursor immediately to prevent
            // stale out-of-order packets from polluting RFC 3550 jitter estimates or entering queue.
            // But if packet belongs to an FEC protected block, still ingest it into the FEC tracker
            // so it can help recover missing packets that are still ahead of expected cursor.
            if (packet.header.flags & FLAG_FEC_PROTECTED) != 0 {
                let block_id = packet.header.timestamp_us;
                self.record_fec_source_and_recover(block_id, seq, unwrapped_seq, &packet, now);
            }
            return;
        }

        // Discard duplicate packets immediately if already present in queue.
        // This prevents retransmissions from overwriting arrival_instant (which would restart
        // playout delay and FEC hold timers) and prevents duplicates from polluting RFC 3550 jitter estimates.
        if self.queue.contains_key(&unwrapped_seq) {
            return;
        }

        // Media packets: RFC 3550 Interarrival Jitter Calculation
        if let (Some(last_arr), Some(last_ts)) =
            (self.last_arrival_instant, self.last_sender_timestamp_us)
        {
            let elapsed_arr_us = now.duration_since(last_arr).as_micros() as f64;
            let elapsed_sender_us = (packet.header.timestamp_us as f64) - (last_ts as f64);
            let d = (elapsed_arr_us - elapsed_sender_us).abs();

            self.jitter_us += (d - self.jitter_us) / 16.0;
            self.stats.current_jitter_us = self.jitter_us;
        }

        self.last_arrival_instant = Some(now);
        self.last_sender_timestamp_us = Some(packet.header.timestamp_us);

        let target_delay = self.current_target_delay();
        self.stats.current_target_delay_us = target_delay.as_micros() as u64;

        let mut rec_range: Option<(u64, u64)> = None;

        // Check if packet belongs to an FEC protected block
        if (packet.header.flags & FLAG_FEC_PROTECTED) != 0 {
            let block_id = packet.header.timestamp_us; // Group by frame/block timestamp
            rec_range =
                self.record_fec_source_and_recover(block_id, seq, unwrapped_seq, &packet, now);
        }

        self.queue.insert(
            unwrapped_seq,
            QueuedPacket {
                packet,
                arrival_instant: now,
            },
        );

        let prot_min = unwrapped_seq.min(rec_range.map(|r| r.0).unwrap_or(unwrapped_seq));
        let prot_max = unwrapped_seq.max(rec_range.map(|r| r.1).unwrap_or(unwrapped_seq));
        self.enforce_queue_capacity(prot_min, prot_max);
    }

    /// Ingests a source packet into the FEC tracker for its block, and attempts recovery if dimensions match.
    /// Returns the reconstructed unwrapped sequence number range (min, max) if packets were recovered.
    fn record_fec_source_and_recover(
        &mut self,
        block_id: u64,
        seq: u32,
        unwrapped_seq: u64,
        packet: &Packet,
        now: Instant,
    ) -> Option<(u64, u64)> {
        if !self.fec_blocks.contains_key(&block_id) && self.fec_blocks.len() >= MAX_FEC_BLOCKS {
            if let Some(&oldest) = self.fec_blocks.keys().min() {
                self.fec_blocks.remove(&oldest);
            }
        }

        let tracker = self
            .fec_blocks
            .entry(block_id)
            .or_insert_with(|| FecBlockTracker {
                scheme: FecScheme::ReedSolomon,
                k: 0,
                m: 0,
                base_sequence: None,
                unwrapped_base_seq: None,
                first_arrival: now,
                sources: HashMap::new(),
                parities: HashMap::new(),
            });

        let idx = packet.header.fec_index() as usize;
        let valid_idx = if tracker.k > 0 {
            idx < tracker.k && tracker.sources.len() < tracker.k
        } else {
            idx < MAX_FEC_K && tracker.sources.len() < MAX_FEC_K
        };
        if valid_idx {
            let implied_base = seq.wrapping_sub(idx as u32);
            let unwrapped_implied_base = unwrapped_seq.saturating_sub(idx as u64);
            let consistent = match tracker.base_sequence {
                None => {
                    tracker.base_sequence = Some(implied_base);
                    tracker.unwrapped_base_seq = Some(unwrapped_implied_base);
                    true
                }
                Some(b) => b == implied_base,
            };
            if consistent {
                tracker.sources.insert(idx, (seq, packet.clone()));
                return self.try_fec_recovery(block_id, now);
            } else {
                tracing::warn!(
                    "Inconsistent FEC (seq, index) in block {}: seq={}, idx={}, expected_base={:?}",
                    block_id,
                    seq,
                    idx,
                    tracker.base_sequence
                );
            }
        }
        None
    }

    /// Registers FEC block parameters for a block of source packets with default ReedSolomon scheme.
    pub fn register_fec_block(&mut self, block_id: u64, k: usize, m: usize) {
        self.register_fec_block_with_scheme(block_id, k, m, FecScheme::ReedSolomon);
    }

    /// Registers FEC block parameters with explicit FEC scheme.
    pub fn register_fec_block_with_scheme(
        &mut self,
        block_id: u64,
        k: usize,
        m: usize,
        scheme: FecScheme,
    ) {
        if k == 0 || k + m > MAX_FEC_K {
            return;
        }
        if !self.fec_blocks.contains_key(&block_id) && self.fec_blocks.len() >= MAX_FEC_BLOCKS {
            if let Some(&oldest) = self.fec_blocks.keys().min() {
                self.fec_blocks.remove(&oldest);
            }
        }
        let tracker = self
            .fec_blocks
            .entry(block_id)
            .or_insert_with(|| FecBlockTracker {
                scheme,
                k,
                m,
                base_sequence: None,
                unwrapped_base_seq: None,
                first_arrival: Instant::now(),
                sources: HashMap::new(),
                parities: HashMap::new(),
            });
        tracker.scheme = scheme;
        tracker.k = k;
        tracker.m = m;
        tracker.prune_invalid_indices();
    }

    /// Attempts FEC recovery on a block if enough data + parity packets are available.
    /// Returns the reconstructed unwrapped sequence number range (min, max) if packets were recovered.
    pub fn try_fec_recovery(&mut self, block_id: u64, _now: Instant) -> Option<(u64, u64)> {
        let tracker = match self.fec_blocks.get_mut(&block_id) {
            Some(t) if t.k > 0 => t,
            _ => return None,
        };

        let k = tracker.k;
        let m = tracker.m;

        if tracker.is_complete() {
            return None;
        }

        let available = tracker.sources.len() + tracker.parities.len();
        if available < k {
            return None;
        }

        let mut source_payloads: HashMap<usize, Bytes> =
            HashMap::with_capacity(tracker.sources.len());
        for (&idx, (_seq, pkt)) in &tracker.sources {
            source_payloads.insert(idx, pkt.payload.clone());
        }

        let recovered = match tracker.scheme {
            FecScheme::Xor => {
                let parity = tracker.parities.get(&0);
                XorFec::decode(block_id, k, source_payloads, parity)
            }
            FecScheme::ReedSolomon => {
                let parities_vec: Vec<(usize, Bytes)> = tracker
                    .parities
                    .iter()
                    .map(|(&idx, bytes)| (idx, bytes.clone()))
                    .collect();
                ReedSolomonFec::decode(block_id, k, m, source_payloads, &parities_vec)
            }
        };

        if let Ok(all_sources) = recovered {
            let ref_arrival = tracker.first_arrival;
            let base_seq = tracker.base_sequence?;
            let unwrapped_base = tracker.unwrapped_base_seq?;
            let ref_entry = tracker.sources.values().next();
            if let Some((_ref_seq, ref_pkt)) = ref_entry {
                let packet_type = ref_pkt.header.packet_type;
                let stream_id = ref_pkt.header.stream_id;
                let timestamp_us = ref_pkt.header.timestamp_us;

                let mut min_rec_seq: Option<u64> = None;
                let mut max_rec_seq: Option<u64> = None;
                for (idx, payload) in all_sources.into_iter().enumerate() {
                    if let std::collections::hash_map::Entry::Vacant(e) = tracker.sources.entry(idx)
                    {
                        let seq = base_seq.wrapping_add(idx as u32);
                        let unwrapped_seq = unwrapped_base + (idx as u64);

                        let rec_header =
                            PacketHeader::new(packet_type, stream_id, seq, timestamp_us, &payload)
                                .with_flags(FLAG_FEC_PROTECTED)
                                .with_fec_index(idx as u16);

                        let recovered_packet = Packet::new(rec_header, payload);
                        e.insert((seq, recovered_packet.clone()));

                        // Discard reconstructed packets if playout has already advanced past them.
                        let is_stale = match self.expected_unwrapped {
                            Some(expected) => unwrapped_seq < expected,
                            None => false,
                        };

                        if !is_stale {
                            if let std::collections::btree_map::Entry::Vacant(q_entry) =
                                self.queue.entry(unwrapped_seq)
                            {
                                min_rec_seq = Some(
                                    min_rec_seq.map_or(unwrapped_seq, |m| m.min(unwrapped_seq)),
                                );
                                max_rec_seq = Some(
                                    max_rec_seq.map_or(unwrapped_seq, |m| m.max(unwrapped_seq)),
                                );
                                q_entry.insert(QueuedPacket {
                                    packet: recovered_packet,
                                    arrival_instant: ref_arrival,
                                });
                                if let Some(highest) = self.highest_seen_unwrapped {
                                    if unwrapped_seq > highest {
                                        self.highest_seen_unwrapped = Some(unwrapped_seq);
                                    }
                                }
                                self.stats.packets_reconstructed_fec += 1;
                            }
                        }
                    }
                }

                if let (Some(rec_min), Some(rec_max)) = (min_rec_seq, max_rec_seq) {
                    self.enforce_queue_capacity(rec_min, rec_max);
                    return Some((rec_min, rec_max));
                }
            }
        }

        None
    }

    /// Enforces maximum queue depth while protecting in-order sequences within `[prot_min, prot_max]`.
    /// 1. Drops premature future packets from the tail (seq > prot_max).
    /// 2. Evicts stale packets from the front (seq < prot_min) via standard loss accounting.
    /// 3. If the protected set itself exceeds max_queue_depth, drops from the high end (`pop_last()`)
    ///    so the earliest playout sequences stay.
    fn enforce_queue_capacity(&mut self, prot_min: u64, prot_max: u64) {
        // Step 1: Drop tail packets strictly greater than prot_max
        while self.queue.len() > self.config.max_queue_depth {
            if let Some(&tail_seq) = self.queue.keys().next_back() {
                if tail_seq > prot_max {
                    self.queue.pop_last();
                    continue;
                }
            }
            break;
        }

        // Step 2: Evict stale packets from the front strictly less than prot_min
        self.evict_stale_before(self.config.max_queue_depth, prot_min);

        // Step 3: If still over max_queue_depth, all remaining packets are within the protected span.
        // Drop from the high end (tail) so the earliest playout sequence stays at the head.
        while self.queue.len() > self.config.max_queue_depth {
            self.queue.pop_last();
        }
    }

    /// Checks whether a missing sequence number belongs to an active, incomplete FEC block
    /// that can still be reconstructed (i.e. K > 0, M > 0, remaining missing packets <= M).
    pub fn is_sequence_fec_recoverable(&self, missing_seq: u32) -> bool {
        for tracker in self.fec_blocks.values() {
            let k = tracker.k;
            let m = tracker.m;
            // Playout should only hold if block dimensions are known and parity is configured
            if k == 0 || m == 0 {
                continue;
            }

            let base_seq = match tracker.base_sequence {
                Some(b) => b,
                None => continue,
            };

            let offset = missing_seq.wrapping_sub(base_seq);
            if offset < (k as u32) {
                let missing_idx = offset as usize;
                if !tracker.sources.contains_key(&missing_idx) {
                    let missing_count = k.saturating_sub(tracker.sources.len());
                    if missing_count > 0 && missing_count <= m {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Checks whether any missing sequence number in the half-open interval `[start, end)`
    /// belongs to an active, incomplete FEC block that can still be reconstructed.
    /// Iterates over active FEC blocks (at most MAX_FEC_BLOCKS = 64) rather than the gap width.
    fn has_fec_recoverable_in_range(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return false;
        }

        for tracker in self.fec_blocks.values() {
            let k = tracker.k;
            let m = tracker.m;
            if k == 0 || m == 0 {
                continue;
            }

            let missing_count = k.saturating_sub(tracker.sources.len());
            if missing_count == 0 || missing_count > m {
                continue;
            }

            if let Some(unwrapped_base) = tracker.unwrapped_base_seq {
                let block_start = unwrapped_base;
                let block_end = unwrapped_base + (k as u64);
                if block_start < end && block_end > start {
                    for idx in 0..k {
                        if !tracker.sources.contains_key(&idx) {
                            let seq = block_start + (idx as u64);
                            if seq >= start && seq < end {
                                return true;
                            }
                        }
                    }
                }
            } else if let Some(base_seq) = tracker.base_sequence {
                let start_wire = start as u32;
                let gap_width = end.saturating_sub(start);
                if gap_width < (1u64 << 31) {
                    let gap_wire = gap_width as u32;
                    for idx in 0..k {
                        if !tracker.sources.contains_key(&idx) {
                            let seq = base_seq.wrapping_add(idx as u32);
                            if seq.wrapping_sub(start_wire) < gap_wire {
                                return true;
                            }
                        }
                    }
                }
            }
        }

        false
    }

    /// Checks whether an unwrapped sequence number corresponds to a parity packet
    /// (either received as FecParity, or within the parity range of a known FEC block).
    fn is_unwrapped_sequence_parity(&self, seq: u64) -> bool {
        if let Some((&_start, &end)) = self.received_parity_ranges.range(..=seq).next_back() {
            if seq < end {
                return true;
            }
        }

        for tracker in self.fec_blocks.values() {
            if let Some(base) = tracker.unwrapped_base_seq {
                if tracker.k > 0 && tracker.m > 0 {
                    let parity_start = base + tracker.k as u64;
                    let parity_end = parity_start + tracker.m as u64;
                    if seq >= parity_start && seq < parity_end {
                        return true;
                    }
                }
            } else if let Some(base_wire) = tracker.base_sequence {
                if tracker.k > 0 && tracker.m > 0 {
                    let seq_wire = seq as u32;
                    let offset = seq_wire.wrapping_sub(base_wire);
                    let k = tracker.k as u32;
                    let m = tracker.m as u32;
                    if offset >= k && offset < k.saturating_add(m) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Counts how many sequence numbers in the half-open interval `[start, end)` correspond to parity packets.
    /// Uses interval overlaps for active FEC blocks and received parity ranges,
    /// avoiding loop iteration over large gaps.
    fn count_parity_in_range(&self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }

        let mut intervals: Vec<(u64, u64)> = Vec::new();
        for tracker in self.fec_blocks.values() {
            if let Some(base) = tracker.unwrapped_base_seq {
                if tracker.k > 0 && tracker.m > 0 {
                    let p_start = base + tracker.k as u64;
                    let p_end = p_start + tracker.m as u64;
                    let o_start = start.max(p_start);
                    let o_end = end.min(p_end);
                    if o_end > o_start {
                        intervals.push((o_start, o_end));
                    }
                }
            } else if let Some(base_wire) = tracker.base_sequence {
                if tracker.k > 0 && tracker.m > 0 {
                    let gap_width = end.saturating_sub(start);
                    if gap_width < (1u64 << 31) {
                        let k = tracker.k as u32;
                        let m = tracker.m as u32;
                        let p_wire_start = base_wire.wrapping_add(k);
                        let start_wire = start as u32;
                        let offset = p_wire_start.wrapping_sub(start_wire);
                        if offset < gap_width as u32 {
                            let p_start = start + offset as u64;
                            let p_end = (p_start + m as u64).min(end);
                            if p_end > p_start {
                                intervals.push((p_start, p_end));
                            }
                        }
                    }
                }
            }
        }

        for (&r_start, &r_end) in &self.received_parity_ranges {
            let o_start = start.max(r_start);
            let o_end = end.min(r_end);
            if o_end > o_start {
                intervals.push((o_start, o_end));
            }
        }

        if intervals.is_empty() {
            return 0;
        }

        intervals.sort_unstable_by_key(|&(s, _)| s);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, e) in intervals {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            merged.push((s, e));
        }

        merged.iter().map(|&(s, e)| e - s).sum()
    }

    /// Prunes completed or expired FEC blocks where playout cursor has advanced past their entire sequence range.
    fn prune_completed_fec_blocks(&mut self) {
        if let Some(expected) = self.expected_unwrapped {
            self.fec_blocks.retain(|_, tracker| {
                if tracker.k > 0 {
                    if let Some(base) = tracker.unwrapped_base_seq {
                        let block_end = base + (tracker.k + tracker.m) as u64;
                        return expected < block_end;
                    }
                }
                true
            });
            self.prune_parity_ranges(expected);
        }
    }

    /// Pops the next packet ready for playout if its target delay has elapsed.
    pub fn pop_ready_packet(&mut self, now: Instant) -> Option<Packet> {
        let target_delay = self.current_target_delay();

        while let Some(&first_unwrapped_seq) = self.queue.keys().next() {
            let arrival_instant = match self.queue.get(&first_unwrapped_seq) {
                Some(p) => p.arrival_instant,
                None => break,
            };

            if let Some(mut expected) = self.expected_unwrapped {
                if first_unwrapped_seq < expected {
                    // Packet arrived after playout cursor has already advanced past it; discard
                    self.queue.remove(&first_unwrapped_seq);
                    continue;
                }

                // Advance expected cursor past any leading parity sequences between expected and first_unwrapped_seq
                while expected < first_unwrapped_seq {
                    if let Some((&_start, &end)) =
                        self.received_parity_ranges.range(..=expected).next_back()
                    {
                        if expected < end {
                            expected = end.min(first_unwrapped_seq);
                            self.expected_unwrapped = Some(expected);
                            continue;
                        }
                    }
                    if self.is_unwrapped_sequence_parity(expected) {
                        expected += 1;
                        self.expected_unwrapped = Some(expected);
                    } else {
                        break;
                    }
                }
                self.prune_completed_fec_blocks();

                if first_unwrapped_seq > expected {
                    // Hole detected between expected and first_unwrapped_seq.
                    // If ANY missing sequence in the gap belongs to an incomplete recoverable FEC block
                    // (accounting for parity packets consuming sequence numbers without being queued),
                    // hold playout up to fec_hold_timeout anchored to when the hole was first detected.
                    let gap_has_recoverable =
                        self.has_fec_recoverable_in_range(expected, first_unwrapped_seq);

                    if gap_has_recoverable {
                        let hold_start = match self.fec_hold_anchor {
                            Some((held_seq, start)) if held_seq == expected => start,
                            _ => {
                                self.fec_hold_anchor = Some((expected, now));
                                now
                            }
                        };

                        if now.duration_since(hold_start) < self.config.fec_hold_timeout {
                            return None;
                        }
                    }
                    // Hole is not recoverable or FEC hold deadline has expired: clear anchor
                    self.fec_hold_anchor = None;
                }
            }

            let age = now.duration_since(arrival_instant);
            if age >= target_delay {
                let item = self.queue.remove(&first_unwrapped_seq)?;
                if let Some(expected) = self.expected_unwrapped {
                    if first_unwrapped_seq > expected {
                        let total_gap = first_unwrapped_seq - expected;
                        let parity_count =
                            self.count_parity_in_range(expected, first_unwrapped_seq);
                        let lost_media = total_gap.saturating_sub(parity_count);
                        self.stats.packets_lost += lost_media;
                    }
                }
                self.fec_hold_anchor = None;
                self.expected_unwrapped = Some(first_unwrapped_seq + 1);
                self.prune_completed_fec_blocks();
                self.stats.packets_delivered += 1;
                return Some(item.packet);
            } else {
                return None;
            }
        }

        None
    }

    /// Evicts ancient packets if queue exceeds maximum threshold, but stops before reaching `prot_min`.
    fn evict_stale_before(&mut self, max_depth: usize, prot_min: u64) {
        while self.queue.len() > max_depth {
            if let Some((&first_unwrapped_seq, _)) = self.queue.iter().next() {
                if first_unwrapped_seq >= prot_min {
                    break;
                }
                self.queue.remove(&first_unwrapped_seq);
                if let Some(mut expected) = self.expected_unwrapped {
                    // Skip any leading parity sequences
                    while expected < first_unwrapped_seq {
                        if let Some((&_start, &end)) =
                            self.received_parity_ranges.range(..=expected).next_back()
                        {
                            if expected < end {
                                expected = end.min(first_unwrapped_seq);
                                self.expected_unwrapped = Some(expected);
                                continue;
                            }
                        }
                        if self.is_unwrapped_sequence_parity(expected) {
                            expected += 1;
                            self.expected_unwrapped = Some(expected);
                        } else {
                            break;
                        }
                    }
                    self.prune_completed_fec_blocks();

                    if first_unwrapped_seq >= expected {
                        let total_gap = first_unwrapped_seq - expected;
                        let parity_count =
                            self.count_parity_in_range(expected, first_unwrapped_seq);
                        let lost_media = total_gap.saturating_sub(parity_count);
                        // Plus 1 for first_unwrapped_seq itself being evicted
                        self.stats.packets_lost += lost_media + 1;
                        self.expected_unwrapped = Some(first_unwrapped_seq + 1);
                        self.prune_completed_fec_blocks();
                    }
                } else {
                    self.stats.packets_lost += 1;
                    self.expected_unwrapped = Some(first_unwrapped_seq + 1);
                }
            } else {
                break;
            }
        }
    }

    /// Evicts ancient packets if queue exceeds maximum threshold to avoid bufferbloat.
    pub fn evict_stale(&mut self, max_depth: usize) {
        self.evict_stale_before(max_depth, u64::MAX);
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[test]
    fn test_jitter_buffer_in_order_delivery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let payload1 = Bytes::from_static(b"frame-chunk-1");
        let header1 = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, &payload1);
        jb.ingest_packet(Packet::new(header1, payload1), now);

        // Before target delay, packet is not ready
        assert!(jb.pop_ready_packet(now).is_none());

        // After target delay, packet pops
        let playout_time = now + Duration::from_millis(5);
        let popped = jb.pop_ready_packet(playout_time).expect("Should be ready");
        assert_eq!(popped.header.sequence, 1);
    }

    #[test]
    fn sender_restart_with_a_lower_sequence_starts_a_new_stream() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let mut now = Instant::now();
        let packet = |seq: u32| {
            let payload = Bytes::from_static(b"chunk");
            let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, &payload);
            Packet::new(header, payload)
        };
        // An old stream, well past the restart threshold.
        for seq in 200_000..200_010 {
            jb.ingest_packet(packet(seq), now);
            now += Duration::from_millis(1);
            while jb
                .pop_ready_packet(now + Duration::from_millis(30))
                .is_some()
            {}
        }
        assert_eq!(jb.stats().stream_restarts, 0);

        // The sender restarted: its sequence starts over at 0.
        jb.ingest_packet(packet(0), now);
        assert_eq!(jb.stats().stream_restarts, 1);
        jb.ingest_packet(packet(1), now);
        let first = jb
            .pop_ready_packet(now + Duration::from_millis(30))
            .expect("new stream is delivered");
        assert_eq!(first.header.sequence, 0);
        assert_eq!(
            jb.pop_ready_packet(now + Duration::from_millis(30))
                .map(|p| p.header.sequence),
            Some(1)
        );
        assert_eq!(jb.stats().stream_restarts, 1);
    }

    #[test]
    fn modest_reordering_is_not_a_restart() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let payload = Bytes::from_static(b"chunk");
        for seq in [5000u32, 5001, 4000] {
            let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, &payload);
            jb.ingest_packet(Packet::new(header, payload.clone()), now);
            while jb
                .pop_ready_packet(now + Duration::from_millis(30))
                .is_some()
            {}
        }
        assert_eq!(jb.stats().stream_restarts, 0);
    }

    #[test]
    fn test_jitter_buffer_reordering() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"p1"),
            Bytes::from_static(b"p1"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, 2000, b"p2"),
            Bytes::from_static(b"p2"),
        );

        // Arrive out of order: 2 then 1
        jb.ingest_packet(p2, now);
        jb.ingest_packet(p1, now);

        let playout = now + Duration::from_millis(5);
        let out1 = jb.pop_ready_packet(playout).unwrap();
        let out2 = jb.pop_ready_packet(playout).unwrap();

        assert_eq!(out1.header.sequence, 1);
        assert_eq!(out2.header.sequence, 2);
    }

    #[test]
    fn test_jitter_buffer_fec_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 999;
        let s0 = Bytes::from_static(b"slice-0");
        let s1 = Bytes::from_static(b"slice-1");
        let s2 = Bytes::from_static(b"slice-2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();

        jb.register_fec_block(block_id, 3, 1);

        // Ingest s0 and s2 (s1 dropped), plus parity 0
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );

        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );

        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(parity_pkt, now);

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);
        assert_eq!(jb.queue_len(), 3); // s0, reconstructed s1, s2
    }

    #[test]
    fn test_jitter_buffer_duplicate_parity_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 1000;
        let sources = vec![
            Bytes::from_static(b"data-0"),
            Bytes::from_static(b"data-1"),
            Bytes::from_static(b"data-2"),
            Bytes::from_static(b"data-3"),
        ];

        let parities = ReedSolomonFec::encode(block_id, &sources, 2).unwrap();
        jb.register_fec_block(block_id, 4, 2);

        // Sources 0 and 1 arrive (sources 2 and 3 dropped)
        for (i, source) in sources.iter().enumerate().take(2) {
            let p = Packet::new(
                PacketHeader::new(
                    PacketType::VideoFrameChunk,
                    0,
                    100 + i as u32,
                    block_id,
                    source,
                )
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(i as u16),
                source.clone(),
            );
            jb.ingest_packet(p, now);
        }

        // Parity 0 arrives twice (network duplication)
        let parity0_dup1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 200, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let parity0_dup2 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 201, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        jb.ingest_packet(parity0_dup1, now);
        jb.ingest_packet(parity0_dup2, now);

        // Parity 1 arrives
        let parity1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 202, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );
        jb.ingest_packet(parity1, now);

        // All 4 source packets must be recovered despite duplicate parity
        assert_eq!(jb.stats().packets_reconstructed_fec, 2);
        assert_eq!(jb.queue_len(), 4);
    }

    #[test]
    fn test_jitter_buffer_clean_reconstructed_flags() {
        use linux_quest_protocol::{FLAG_KEYFRAME, FLAG_LAST_CHUNK};

        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 2000;
        let s0 = Bytes::from_static(b"chunk0");
        let s1 = Bytes::from_static(b"chunk1");
        let s2 = Bytes::from_static(b"chunk2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Chunk 0 has KEYFRAME
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED | FLAG_KEYFRAME)
                .with_fec_index(0),
            s0,
        );
        // Chunk 2 has LAST_CHUNK
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED | FLAG_LAST_CHUNK)
                .with_fec_index(2),
            s2,
        );
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );

        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(parity_pkt, now);

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        let playout = now + Duration::from_millis(50);
        let pkt0 = jb.pop_ready_packet(playout).unwrap();
        let pkt1 = jb.pop_ready_packet(playout).unwrap();
        let pkt2 = jb.pop_ready_packet(playout).unwrap();

        assert_eq!(pkt0.header.sequence, 10);
        assert_eq!(pkt1.header.sequence, 11);
        assert_eq!(pkt2.header.sequence, 12);

        // Recovered chunk 1 MUST NOT copy KEYFRAME or LAST_CHUNK flags
        assert_eq!(pkt1.header.flags, FLAG_FEC_PROTECTED);
    }

    #[test]
    fn test_jitter_buffer_late_packet_discard() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Ingest sequence 5
        let p5 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, 5000, b"p5"),
            Bytes::from_static(b"p5"),
        );
        jb.ingest_packet(p5, now);

        let playout = now + Duration::from_millis(50);
        let popped = jb.pop_ready_packet(playout).unwrap();
        assert_eq!(popped.header.sequence, 5);

        // Now late packet sequence 4 arrives
        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, 4000, b"p4"),
            Bytes::from_static(b"p4"),
        );
        jb.ingest_packet(p4, now);

        // Popping must discard late sequence 4 and return None
        assert!(jb.pop_ready_packet(playout).is_none());
        assert_eq!(jb.stats().packets_delivered, 1);
    }

    #[test]
    fn test_jitter_buffer_holds_for_fec_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        let block_id = 5000;
        let s0 = Bytes::from_static(b"packet-10");
        let s1 = Bytes::from_static(b"packet-11");
        let s2 = Bytes::from_static(b"packet-12");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Sequence 10 and 12 arrive (sequence 11 lost)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );

        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);

        // Playout time after min_delay (e.g. 6ms)
        let playout_1 = now + Duration::from_millis(6);
        let popped_0 = jb
            .pop_ready_packet(playout_1)
            .expect("Sequence 10 should pop");
        assert_eq!(popped_0.header.sequence, 10);

        // Playout holds for sequence 11 instead of prematurely popping 12 and discarding 11!
        let hold_check = jb.pop_ready_packet(playout_1);
        assert!(
            hold_check.is_none(),
            "Playout should hold for missing sequence 11 in recoverable block"
        );

        // Now parity arrives within hold window
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 20, block_id, &parities[0]),
            parities[0].clone(),
        );
        jb.ingest_packet(parity_pkt, now + Duration::from_millis(8));

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        let playout_2 = now + Duration::from_millis(8);
        let popped_1 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 11 should pop after recovery");
        let popped_2 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 12 should pop next");

        assert_eq!(popped_1.header.sequence, 11);
        assert_eq!(popped_2.header.sequence, 12);
        assert_eq!(jb.stats().packets_lost, 0);
        assert_eq!(jb.stats().packets_delivered, 3);
    }

    #[test]
    fn test_jitter_buffer_max_queue_depth_eviction() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 5,
            ..Default::default()
        });
        let now = Instant::now();

        for seq in 1..=10 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }

        assert!(jb.queue_len() <= 5);
        assert_eq!(jb.stats().packets_lost, 5);
    }

    #[test]
    fn test_jitter_buffer_fec_index_cap_when_k_unknown() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Packet with out-of-range fec_index (>= MAX_FEC_K = 256)
        let p_invalid = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 9999, b"data")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(300),
            Bytes::from_static(b"data"),
        );
        jb.ingest_packet(p_invalid, now);

        // Should not have inserted into block sources
        if let Some(tracker) = jb.fec_blocks.get(&9999) {
            assert!(!tracker.sources.contains_key(&300));
        }
    }

    #[test]
    fn test_jitter_buffer_m_zero_does_not_hold_playout() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 12345;

        // Block with zero parity (m = 0) can never be reconstructed
        jb.register_fec_block(block_id, 3, 0);

        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, b"chunk0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"chunk0"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, b"chunk2")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            Bytes::from_static(b"chunk2"),
        );
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);

        // Sequence 11 is missing, but with m == 0 it should not hold playout
        assert!(
            !jb.is_sequence_fec_recoverable(11),
            "Sequence should not be recoverable when m == 0"
        );
    }

    #[test]
    fn test_jitter_buffer_unknown_k_does_not_hold_playout() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 54321;

        // Media packet arrives with FEC_PROTECTED before any parity or register_fec_block (k == 0)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, b"chunk0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"chunk0"),
        );
        jb.ingest_packet(p0, now);

        // With k == 0 (dimensions unknown), playout should not hold
        assert!(
            !jb.is_sequence_fec_recoverable(11),
            "Sequence should not be recoverable when k is unknown"
        );
    }

    #[test]
    fn test_jitter_buffer_fec_recovery_respects_max_queue_depth() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 4,
            ..Default::default()
        });
        let now = Instant::now();
        let block_id = 8888;

        let s0 = Bytes::from_static(b"source-0");
        let s1 = Bytes::from_static(b"source-1");
        let s2 = Bytes::from_static(b"source-2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Ingest source 0 and 2 (source 1 missing)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);

        // Ingest 2 more independent packets to fill queue to max depth 4
        let p_extra1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 13, 9001, b"extra1"),
            Bytes::from_static(b"extra1"),
        );
        let p_extra2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 14, 9002, b"extra2"),
            Bytes::from_static(b"extra2"),
        );
        jb.ingest_packet(p_extra1, now);
        jb.ingest_packet(p_extra2, now);
        assert_eq!(jb.queue_len(), 4);

        // Now parity arrives and triggers recovery of source 1 (seq 11)
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 20, block_id, &parities[0]),
            parities[0].clone(),
        );
        jb.ingest_packet(parity_pkt, now);

        // FEC recovery should have occurred
        assert_eq!(jb.stats().packets_reconstructed_fec, 1);
        // Queue length MUST be clamped to max_queue_depth
        assert!(
            jb.queue_len() <= 4,
            "Queue length {} should not exceed max_queue_depth 4",
            jb.queue_len()
        );
    }

    #[test]
    fn test_jitter_buffer_eviction_advances_expected_sequence_without_double_counting() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 3,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Ingest packets 1 and 2
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"data1"),
            Bytes::from_static(b"data1"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, 1000, b"data2"),
            Bytes::from_static(b"data2"),
        );
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p2, now);

        // Pop packet 1 -> expected_sequence is initialized to Some(2)
        let popped_1 = jb.pop_ready_packet(now).expect("Packet 1 ready");
        assert_eq!(popped_1.header.sequence, 1);
        assert_eq!(jb.expected_sequence(), Some(2));
        assert_eq!(jb.stats().packets_lost, 0);

        // Now burst of packets arrives: 5, 6, 7, 8 (packets 2, 3, 4 are lost in transit)
        for seq in 5..=8 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }

        // With max_queue_depth = 3, packets should be clamped
        assert!(jb.queue_len() <= 3);

        // Now pop the next ready packet from queue (which will be seq 6 or later after eviction)
        let popped_next = jb.pop_ready_packet(now).expect("Next packet ready");
        let popped_seq = popped_next.header.sequence;

        // Total delivered: packet 1 + popped_next
        assert_eq!(jb.stats().packets_delivered, 2);
        // Total packets up to popped_seq was popped_seq.
        // Packets lost + packets delivered should equal popped_seq! (No double counting)
        assert_eq!(
            jb.stats().packets_lost + jb.stats().packets_delivered,
            popped_seq as u64,
            "Gaps must be counted exactly once without double counting between eviction and playout"
        );
    }

    #[test]
    fn test_jitter_buffer_inconsistent_fec_index_rejected() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 424242;

        jb.register_fec_block(block_id, 4, 1);

        // First packet establishes base_sequence: seq 100 with fec_index 0 -> base = 100
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 100, block_id, b"s0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"s0"),
        );
        jb.ingest_packet(p0, now);

        // Inconsistent packet: seq 200 with fec_index 1 -> implied base = 199 != 100
        let p_inconsistent = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 200, block_id, b"s1_bad")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            Bytes::from_static(b"s1_bad"),
        );
        jb.ingest_packet(p_inconsistent, now);

        let tracker = jb.fec_blocks.get(&block_id).expect("Tracker exists");
        assert_eq!(tracker.base_sequence, Some(100));
        assert!(tracker.sources.contains_key(&0));
        // Index 1 with inconsistent base must be rejected from tracker.sources
        assert!(!tracker.sources.contains_key(&1));
    }

    #[test]
    fn test_jitter_buffer_fec_parity_dimensions_clamped_and_wrap_safe() {
        use crate::fec::FecHeader;

        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 999999;

        // Malformed parity packet with massive k=65535, m=65535
        let raw_hdr = FecHeader::new(FecScheme::ReedSolomon, block_id, 65535, 65535, 0, 100);
        let mut buf = bytes::BytesMut::new();
        raw_hdr.encode(&mut buf);
        buf.extend_from_slice(b"fake-parity-payload");
        let payload = buf.freeze();

        let p_malformed = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 10, block_id, &payload),
            payload,
        );
        jb.ingest_packet(p_malformed, now);

        // Tracker should not have been created with illegal dimensions
        assert!(!jb.fec_blocks.contains_key(&block_id));

        // Wrap-safe test near u32::MAX
        let wrap_block_id = 888888;
        jb.register_fec_block(wrap_block_id, 2, 1);
        let p_wrap0 = Packet::new(
            PacketHeader::new(
                PacketType::VideoFrameChunk,
                0,
                u32::MAX - 2,
                wrap_block_id,
                b"w0",
            )
            .with_flags(FLAG_FEC_PROTECTED)
            .with_fec_index(0),
            Bytes::from_static(b"w0"),
        );
        jb.ingest_packet(p_wrap0, now);

        // Sequence u32::MAX - 1 is missing, base_seq is u32::MAX - 2.
        // Must not panic with debug overflow arithmetic!
        assert!(jb.is_sequence_fec_recoverable(u32::MAX - 1));
        assert!(!jb.is_sequence_fec_recoverable(10));
    }

    #[test]
    fn test_jitter_buffer_sequence_wrap_across_u32_max() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Ingest packet u32::MAX
        let p_max = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        jb.ingest_packet(p_max, now);

        // Pop packet u32::MAX -> cursor must wrap to 0 without debug panic
        let popped = jb.pop_ready_packet(now).expect("u32::MAX should pop");
        assert_eq!(popped.header.sequence, u32::MAX);
        assert_eq!(jb.expected_sequence(), Some(0));

        // Ingest packet 0 (post-wrap)
        let p_zero = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, 101, b"zero"),
            Bytes::from_static(b"zero"),
        );
        jb.ingest_packet(p_zero, now);

        // Packet 0 must not be discarded as old
        let popped_zero = jb.pop_ready_packet(now).expect("Packet 0 should pop");
        assert_eq!(popped_zero.header.sequence, 0);
        assert_eq!(jb.expected_sequence(), Some(1));
    }

    #[test]
    fn test_jitter_buffer_evict_stale_wraps_cursor_at_u32_max() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 1,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        jb.ingest_packet(p1, now);
        // Pop to set expected_sequence = Some(u32::MAX)
        let _ = jb.pop_ready_packet(now);
        assert_eq!(jb.expected_sequence(), Some(0));

        // Now test eviction when first_seq is u32::MAX and expected is u32::MAX
        let mut jb2 = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 1,
            ..Default::default()
        });
        jb2.set_expected_sequence(u32::MAX);
        let p_max = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        let p_zero = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, 101, b"zero"),
            Bytes::from_static(b"zero"),
        );
        jb2.ingest_packet(p_max, now);
        jb2.ingest_packet(p_zero, now);

        // Eviction should have run, advancing expected_sequence to Some(0) instead of saturating
        assert_eq!(jb2.expected_sequence(), Some(0));
    }

    #[test]
    fn test_jitter_buffer_queue_order_and_fec_across_wrap_boundary() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();
        let block_id = 12345678;

        let s0 = Bytes::from_static(b"data-wrap-0");
        let s1 = Bytes::from_static(b"data-wrap-1");
        let s2 = Bytes::from_static(b"data-wrap-2");
        let s3 = Bytes::from_static(b"data-wrap-3");

        let sources = vec![s0.clone(), s1.clone(), s2.clone(), s3.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 4, 1);

        // Source packets straddling u32::MAX:
        // Index 0 -> seq u32::MAX - 1 (4294967294)
        // Index 1 -> seq u32::MAX     (4294967295) -- WE DROP THIS PACKET TO TRIGGER FEC RECOVERY
        // Index 2 -> seq 0            (post-wrap)
        // Index 3 -> seq 1            (post-wrap)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX - 1, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );
        let p3 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block_id, &s3)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(3),
            s3,
        );
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 2, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );

        // Ingest ALL packets (both sides of the wrap: pre-wrap p0, post-wrap p2, p3, and parity)
        // simultaneously BEFORE popping anything!
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(p3, now);
        jb.ingest_packet(parity_pkt, now);

        // Verification 1: FEC recovery reconstructed the dropped packet (seq u32::MAX)
        assert_eq!(
            jb.stats().packets_reconstructed_fec,
            1,
            "FEC must recover packet across the wrap boundary"
        );
        assert_eq!(jb.queue_len(), 4, "All 4 packets must be in queue");

        // Verification 2: Playout delivers strictly in sequential order across wrap
        let playout = now + Duration::from_millis(30);
        let out0 = jb.pop_ready_packet(playout).expect("Packet 0 should pop");
        let out1 = jb.pop_ready_packet(playout).expect("Packet 1 should pop");
        let out2 = jb.pop_ready_packet(playout).expect("Packet 2 should pop");
        let out3 = jb.pop_ready_packet(playout).expect("Packet 3 should pop");

        assert_eq!(out0.header.sequence, u32::MAX - 1);
        assert_eq!(out1.header.sequence, u32::MAX);
        assert_eq!(out2.header.sequence, 0);
        assert_eq!(out3.header.sequence, 1);
        assert!(jb.pop_ready_packet(playout).is_none());

        // Zero packets lost, exactly 4 delivered!
        assert_eq!(jb.stats().packets_lost, 0);
        assert_eq!(jb.stats().packets_delivered, 4);
    }

    #[test]
    fn test_jitter_buffer_late_packets_do_not_corrupt_jitter_estimate() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Ingest sequence 5 with timestamp 5000us
        let p5 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, 5000, b"p5"),
            Bytes::from_static(b"p5"),
        );
        jb.ingest_packet(p5, now);

        // Playout pops sequence 5, advancing expected cursor to 6
        let playout = now + Duration::from_millis(50);
        let popped = jb.pop_ready_packet(playout).unwrap();
        assert_eq!(popped.header.sequence, 5);
        assert_eq!(jb.stats().current_jitter_us, 0.0);
        assert_eq!(jb.last_sender_timestamp_us, Some(5000));

        // Now a stale packet (sequence 4) arrives 100ms later with timestamp 4000us
        let late_arrival = now + Duration::from_millis(150);
        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, 4000, b"p4"),
            Bytes::from_static(b"p4"),
        );
        jb.ingest_packet(p4, late_arrival);

        // Assert: Jitter estimate MUST NOT spike, and last_sender_timestamp MUST NOT be overwritten
        assert_eq!(
            jb.stats().current_jitter_us,
            0.0,
            "Late discarded packet must not corrupt RFC 3550 jitter calculation"
        );
        assert_eq!(
            jb.last_sender_timestamp_us,
            Some(5000),
            "Late discarded packet must not overwrite last_sender_timestamp"
        );
        assert_eq!(jb.queue_len(), 0);
    }

    #[test]
    fn test_jitter_buffer_register_fec_block_caps_max_fec_blocks() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());

        for i in 1..=100 {
            jb.register_fec_block(i, 4, 1);
        }

        assert!(
            jb.fec_blocks.len() <= MAX_FEC_BLOCKS,
            "fec_blocks count {} should not exceed MAX_FEC_BLOCKS {}",
            jb.fec_blocks.len(),
            MAX_FEC_BLOCKS
        );
    }

    #[test]
    fn test_jitter_buffer_duplicate_packet_does_not_restart_playout_or_fec_hold() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"data1"),
            Bytes::from_static(b"data1"),
        );
        jb.ingest_packet(p1.clone(), now);

        // Arrive again 4ms later as a network duplicate
        let t_dup = now + Duration::from_millis(4);
        jb.ingest_packet(p1, t_dup);

        // At now + 5.1ms, original target_delay (5ms) has elapsed since first arrival (now)
        let playout = now + Duration::from_millis(5) + Duration::from_micros(100);
        let popped = jb
            .pop_ready_packet(playout)
            .expect("Packet 1 must pop based on first arrival, not delayed by duplicate");
        assert_eq!(popped.header.sequence, 1);
        assert_eq!(
            jb.stats().current_jitter_us,
            0.0,
            "Duplicate packet must not corrupt jitter estimate"
        );
    }

    #[test]
    fn test_jitter_buffer_eviction_after_insert_does_not_delete_both_packets() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 3,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Packet 10 is ingested and played -> expected cursor is 11
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, 1000, b"data10"),
            Bytes::from_static(b"data10"),
        );
        jb.ingest_packet(p10, now);
        let popped_10 = jb.pop_ready_packet(now).unwrap();
        assert_eq!(popped_10.header.sequence, 10);
        assert_eq!(jb.expected_sequence(), Some(11));

        // Packets 12, 13, 14 arrive while packet 11 is in transit (queue full to max depth 3)
        for seq in 12..=14 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }
        assert_eq!(jb.queue_len(), 3);

        // Now packet 11 arrives. Eviction must run AFTER insert, so packet 12 is preserved!
        let p11 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, 1000, b"data11"),
            Bytes::from_static(b"data11"),
        );
        jb.ingest_packet(p11, now);

        // When packet 11 arrives, it is the in-order fill at expected cursor 11.
        // With tail eviction of premature future packets (14 dropped), packets 11, 12, 13 are all preserved and delivered!
        let out11 = jb
            .pop_ready_packet(now)
            .expect("Packet 11 (in-order fill) must be ready to play");
        assert_eq!(out11.header.sequence, 11);

        let out12 = jb
            .pop_ready_packet(now)
            .expect("Packet 12 must be ready to play");
        assert_eq!(out12.header.sequence, 12);

        let out13 = jb
            .pop_ready_packet(now)
            .expect("Packet 13 must be ready to play");
        assert_eq!(out13.header.sequence, 13);
        assert_eq!(
            jb.stats().packets_lost,
            0,
            "In-order packet 11 must not increment packets_lost"
        );
    }

    #[test]
    fn test_jitter_buffer_lossless_fec_parity_sequence_does_not_inflate_loss() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Block 1 (Frame 1):
        // Source 0 (seq 1), Source 1 (seq 2), Parity 0 (seq 3)
        let block1 = 1001;
        let s0 = Bytes::from_static(b"f1-s0");
        let s1 = Bytes::from_static(b"f1-s1");
        let parities1 = ReedSolomonFec::encode(block1, &[s0.clone(), s1.clone()], 1).unwrap();
        jb.register_fec_block(block1, 2, 1);

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block1, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, block1, &s1)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s1,
        );
        let p3_parity = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 3, block1, &parities1[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities1[0].clone(),
        );

        // Block 2 (Frame 2):
        // Source 0 (seq 4), Source 1 (seq 5), Parity 0 (seq 6)
        let block2 = 1002;
        let s2 = Bytes::from_static(b"f2-s0");
        let s3 = Bytes::from_static(b"f2-s1");
        let parities2 = ReedSolomonFec::encode(block2, &[s2.clone(), s3.clone()], 1).unwrap();
        jb.register_fec_block(block2, 2, 1);

        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, block2, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s2,
        );
        let p5 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, block2, &s3)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s3,
        );
        let p6_parity = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 6, block2, &parities2[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities2[0].clone(),
        );

        // Ingest Block 1 and Block 2 losslessly
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(p3_parity, now);
        jb.ingest_packet(p4, now);
        jb.ingest_packet(p5, now);
        jb.ingest_packet(p6_parity, now);

        // Pop all 4 media packets
        let out1 = jb.pop_ready_packet(now).expect("Packet 1 must pop");
        let out2 = jb.pop_ready_packet(now).expect("Packet 2 must pop");
        let out4 = jb.pop_ready_packet(now).expect("Packet 4 must pop");
        let out5 = jb.pop_ready_packet(now).expect("Packet 5 must pop");

        assert_eq!(out1.header.sequence, 1);
        assert_eq!(out2.header.sequence, 2);
        assert_eq!(out4.header.sequence, 4);
        assert_eq!(out5.header.sequence, 5);

        // Verification: Lossless transmission must report 0 packets lost and 0.0 loss rate!
        assert_eq!(
            jb.stats().packets_lost,
            0,
            "Parity sequences must not be counted as lost media"
        );
        assert_eq!(jb.stats().packets_delivered, 4);
        assert_eq!(jb.stats().loss_rate(), 0.0);
    }

    #[test]
    fn test_jitter_buffer_fec_hold_inspects_entire_gap_past_parity_sequence() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        // Block 1 (Frame 1):
        // Source 0 (seq 1), Source 1 (seq 2), Parity 0 (seq 3)
        let block1 = 1111;
        let s0 = Bytes::from_static(b"f1-s0");
        let s1 = Bytes::from_static(b"f1-s1");
        let sources1 = vec![s0.clone(), s1.clone()];
        let parities1 = ReedSolomonFec::encode(block1, &sources1, 1).unwrap();
        jb.register_fec_block(block1, 2, 1);

        // Ingest and play Block 1
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block1, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, block1, &s1)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s1,
        );
        let p_parity1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 3, block1, &parities1[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities1[0].clone(),
        );
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p_parity1, now);

        let playout_1 = now + Duration::from_millis(10);
        let popped_0 = jb.pop_ready_packet(playout_1).unwrap();
        let popped_1 = jb.pop_ready_packet(playout_1).unwrap();
        assert_eq!(popped_0.header.sequence, 1);
        assert_eq!(popped_1.header.sequence, 2);
        // Expected cursor is now 3 (the sequence consumed by parity packet 3!)
        assert_eq!(jb.expected_sequence(), Some(3));

        // Block 2 (Frame 2):
        // Source 0 (seq 4 - LOST IN TRANSIT), Source 1 (seq 5), Parity 0 (seq 6)
        let block2 = 2222;
        let s2 = Bytes::from_static(b"f2-s0");
        let s3 = Bytes::from_static(b"f2-s1");
        let sources2 = vec![s2.clone(), s3.clone()];
        let parities2 = ReedSolomonFec::encode(block2, &sources2, 1).unwrap();
        jb.register_fec_block(block2, 2, 1);

        // Only packet 5 arrives from Block 2 (packet 4 is missing)
        let p3 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, block2, &s3)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s3,
        );
        jb.ingest_packet(p3, now);

        // Playout check:
        // Expected cursor is 3 (parity). Missing gap is [3, 5).
        // Sequence 4 in this gap is FEC-recoverable!
        // Playout MUST HOLD instead of immediately skipping 3..5 and popping 5!
        let hold_check = jb.pop_ready_packet(playout_1);
        assert!(
            hold_check.is_none(),
            "Playout must hold because sequence 4 in gap [3, 5) is FEC-recoverable!"
        );

        // Now parity for Block 2 arrives within hold window
        let p_parity2 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 6, block2, &parities2[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities2[0].clone(),
        );
        jb.ingest_packet(p_parity2, now + Duration::from_millis(2));

        // Sequence 4 must have been reconstructed!
        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        // Now pop ready packets:
        let playout_2 = now + Duration::from_millis(15);
        let popped_s2 = jb
            .pop_ready_packet(playout_2)
            .expect("Reconstructed sequence 4 must pop!");
        let popped_s3 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 5 must pop next!");

        assert_eq!(popped_s2.header.sequence, 4);
        assert_eq!(popped_s3.header.sequence, 5);
        assert_eq!(jb.stats().packets_delivered, 4);
    }

    #[test]
    fn test_jitter_buffer_lost_parity_packet_does_not_inflate_media_loss() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // k=2, m=1, sources at sequences 1 and 2, parity sequence 3 NEVER ingested (lost on network)
        let block1 = 5001;
        let s0 = Bytes::from_static(b"data-1");
        let s1 = Bytes::from_static(b"data-2");
        jb.register_fec_block(block1, 2, 1);

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block1, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, block1, &s1)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s1,
        );
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p2, now);

        // Next media packet arrives at sequence 4 (Block 2)
        let block2 = 5002;
        let s2 = Bytes::from_static(b"data-4");
        jb.register_fec_block(block2, 1, 0);
        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, block2, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s2,
        );
        jb.ingest_packet(p4, now);

        // Playout pops packets 1, 2, 4
        let out1 = jb.pop_ready_packet(now).expect("Packet 1 must pop");
        let out2 = jb.pop_ready_packet(now).expect("Packet 2 must pop");
        let out4 = jb.pop_ready_packet(now).expect("Packet 4 must pop");

        assert_eq!(out1.header.sequence, 1);
        assert_eq!(out2.header.sequence, 2);
        assert_eq!(out4.header.sequence, 4);

        // Lost parity datagram at sequence 3 must NOT be counted as lost media!
        assert_eq!(jb.stats().packets_lost, 0);
        assert_eq!(jb.stats().packets_delivered, 3);
        assert_eq!(jb.stats().loss_rate(), 0.0);
    }

    #[test]
    fn test_jitter_buffer_large_gap_past_1024_does_not_count_parity_as_loss() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Packet 1000 arrives and plays -> expected is 1001
        let p1000 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1000, 1000, b"p1000"),
            Bytes::from_static(b"p1000"),
        );
        jb.ingest_packet(p1000, now);
        let _ = jb.pop_ready_packet(now);
        assert_eq!(jb.expected_sequence(), Some(1001));

        // Parity packet received at sequence 1500 (inside gap 1001..2000, >1024 size)
        let block_mid = 1500;
        let payload = Bytes::from_static(b"parity");
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 1500, block_mid, &payload),
            payload,
        );
        jb.ingest_packet(parity_pkt, now);

        // Next media packet arrives at sequence 2000 (total gap 999 sequences, with 1 parity)
        let p2000 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2000, 2000, b"p2000"),
            Bytes::from_static(b"p2000"),
        );
        jb.ingest_packet(p2000, now);

        let out2000 = jb.pop_ready_packet(now).expect("Packet 2000 must pop");
        assert_eq!(out2000.header.sequence, 2000);

        // Gap is [1001, 2000) = 999 sequence numbers. Sequence 1500 is parity.
        // Exactly 998 media packets must be counted as lost, NOT 999!
        assert_eq!(jb.stats().packets_lost, 998);
        assert_eq!(jb.stats().packets_delivered, 2);
    }

    #[test]
    fn test_jitter_buffer_fec_recovery_at_queue_max_is_not_dropped() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 2,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Depth 2, sources 10 and 11, recover sequence 12 (queue maximum)
        let block = 9999;
        let s0 = Bytes::from_static(b"s10");
        let s1 = Bytes::from_static(b"s11");
        let s2 = Bytes::from_static(b"s12");
        let parities =
            ReedSolomonFec::encode(block, &[s0.clone(), s1.clone(), s2.clone()], 1).unwrap();
        jb.register_fec_block(block, 3, 1);

        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p11 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, block, &s1)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s1,
        );
        jb.ingest_packet(p10, now);
        jb.ingest_packet(p11, now);
        assert_eq!(jb.queue_len(), 2);

        // Parity arrives and reconstructs sequence 12 (which is the queue maximum!)
        let p_parity = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        jb.ingest_packet(p_parity, now);

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);
        assert_eq!(jb.queue_len(), 2, "Queue clamped to max_queue_depth 2");

        // The reconstructed sequence 12 must NOT be dropped!
        // Playout delivers packet 11 and packet 12!
        let out11 = jb.pop_ready_packet(now).expect("Packet 11 must pop");
        let out12 = jb
            .pop_ready_packet(now)
            .expect("Reconstructed packet 12 must pop!");
        assert_eq!(out11.header.sequence, 11);
        assert_eq!(out12.header.sequence, 12);
        assert_eq!(jb.stats().packets_delivered, 2);
    }

    #[test]
    fn test_jitter_buffer_received_parity_sequences_strictly_capped_at_2048() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Ingest packet 1 and pop it -> expected cursor is Some(2)
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 100, b"p1"),
            Bytes::from_static(b"p1"),
        );
        jb.ingest_packet(p1, now);
        let _ = jb.pop_ready_packet(now);
        assert_eq!(jb.expected_sequence(), Some(2));

        // Now flood 3000 parity packets ahead of the cursor (all seq >= expected)
        for seq in 100..3100 {
            let p = Packet::new(
                PacketHeader::new(PacketType::FecParity, 0, seq, 1000, b"par"),
                Bytes::from_static(b"par"),
            );
            jb.ingest_packet(p, now);
        }

        // received_parity_ranges MUST be strictly capped at 2048!
        assert!(
            jb.received_parity_ranges.len() <= 2048,
            "received_parity_ranges len {} exceeded 2048 cap!",
            jb.received_parity_ranges.len()
        );
        // Contiguous sequences merge into 1 range!
        assert_eq!(jb.received_parity_range_count(), 1);

        // Also verify fragmented non-contiguous bursts cap at 2048
        let mut jb2 = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        for i in 0..3000 {
            let seq = 10000 + i * 2; // step of 2 prevents contiguous merging
            let p = Packet::new(
                PacketHeader::new(PacketType::FecParity, 0, seq, 1000, b"par"),
                Bytes::from_static(b"par"),
            );
            jb2.ingest_packet(p, now);
        }
        assert_eq!(jb2.received_parity_range_count(), 2048);
    }

    #[test]
    fn test_jitter_buffer_reconstructed_packet_at_head_not_dropped_by_front_eviction() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 2,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Reed-Solomon k=3, m=2, sources 10, 11, 12
        let block_id = 7777;
        let s10 = Bytes::from_static(b"src-10");
        let s11 = Bytes::from_static(b"src-11");
        let s12 = Bytes::from_static(b"src-12");
        let parities =
            ReedSolomonFec::encode(block_id, &[s10.clone(), s11.clone(), s12.clone()], 2).unwrap();
        jb.register_fec_block(block_id, 3, 2);

        // Queue starts as {11, 20}
        let p11 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, block_id, &s11)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s11,
        );
        let p20 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 20, 9999, b"future-20"),
            Bytes::from_static(b"future-20"),
        );
        jb.ingest_packet(p11, now);
        jb.ingest_packet(p20, now);
        assert_eq!(jb.queue_len(), 2);

        // Two parities arrive and reconstruct 10 and 12
        let p_par0 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let p_par1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 14, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );

        jb.ingest_packet(p_par0.clone(), now);
        jb.ingest_packet(p_par1.clone(), now);

        assert_eq!(jb.stats().packets_reconstructed_fec, 2);
        assert_eq!(jb.queue_len(), 2);

        // Playout MUST deliver 10, then 11! Sequence 10 must NOT be dropped by front eviction!
        let out10 = jb
            .pop_ready_packet(now)
            .expect("Reconstructed packet 10 at queue head must NOT be front-evicted!");
        assert_eq!(out10.header.sequence, 10);
        let out11 = jb.pop_ready_packet(now).expect("Packet 11 must pop!");
        assert_eq!(out11.header.sequence, 11);

        // Same shape on source-triggered path: queue {30, 31}, parities already stored, source 11 arrives
        let mut jb_src = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 2,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        jb_src.register_fec_block(block_id, 3, 2);

        // Queue starts as {30, 31}
        let p30 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 30, 8888, b"future-30"),
            Bytes::from_static(b"future-30"),
        );
        let p31 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 31, 8888, b"future-31"),
            Bytes::from_static(b"future-31"),
        );
        jb_src.ingest_packet(p30, now);
        jb_src.ingest_packet(p31, now);

        // Ingest parities first
        jb_src.ingest_packet(p_par0, now);
        jb_src.ingest_packet(p_par1, now);

        // Source 11 arrives, triggering recovery of 10 and 12
        let p11_clone = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, block_id, b"src-11")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            Bytes::from_static(b"src-11"),
        );
        jb_src.ingest_packet(p11_clone, now);

        assert_eq!(jb_src.stats().packets_reconstructed_fec, 2);
        assert_eq!(jb_src.queue_len(), 2);

        // Playout MUST deliver 10, then 11!
        let out10_src = jb_src
            .pop_ready_packet(now)
            .expect("Reconstructed packet 10 on source-triggered recovery must not be lost!");
        assert_eq!(out10_src.header.sequence, 10);
        let out11_src = jb_src.pop_ready_packet(now).expect("Packet 11 must pop!");
        assert_eq!(out11_src.header.sequence, 11);
    }

    #[test]
    fn test_jitter_buffer_late_source_behind_cursor_recovers_future_hole() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Reed-Solomon k=5, m=2, sequences 10 through 14
        let block_id = 8888;
        let s10 = Bytes::from_static(b"s10");
        let s11 = Bytes::from_static(b"s11");
        let s12 = Bytes::from_static(b"s12");
        let s13 = Bytes::from_static(b"s13");
        let s14 = Bytes::from_static(b"s14");
        let sources = vec![
            s10.clone(),
            s11.clone(),
            s12.clone(),
            s13.clone(),
            s14.clone(),
        ];
        let parities = ReedSolomonFec::encode(block_id, &sources, 2).unwrap();
        jb.register_fec_block(block_id, 5, 2);

        // Ingest sources 12 and 14
        let p12 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s12)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s12.clone(),
        );
        let p14 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 14, block_id, &s14)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(4),
            s14.clone(),
        );
        jb.ingest_packet(p12, now);
        jb.ingest_packet(p14, now);

        // Pop 12 so playout cursor advances to 13
        let out12 = jb.pop_ready_packet(now).expect("Packet 12 must pop");
        assert_eq!(out12.header.sequence, 12);
        assert_eq!(jb.expected_sequence(), Some(13));

        // Now ingest late source 10 (10 < 13 cursor)
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s10)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s10,
        );
        jb.ingest_packet(p10, now);

        // Ingest both parities (parities at wire sequences 15 and 16)
        let p_par0 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 15, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let p_par1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 16, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );

        jb.ingest_packet(p_par0, now);
        jb.ingest_packet(p_par1, now);

        // Sequence 13 must be reconstructed! (Sequence 11 was stale < 13, so discarded)
        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        // Playout delivers 13, then delivers 14 with 0 packet loss!
        let out13 = jb
            .pop_ready_packet(now)
            .expect("Reconstructed sequence 13 must pop!");
        assert_eq!(out13.header.sequence, 13);

        let out14 = jb.pop_ready_packet(now).expect("Packet 14 must pop!");
        assert_eq!(out14.header.sequence, 14);

        assert_eq!(
            jb.stats().packets_lost,
            0,
            "No media packets should be lost!"
        );
    }

    #[test]
    fn test_jitter_buffer_large_parity_burst_ahead_of_cursor_not_counted_as_media_loss() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Pop sequence 1 so cursor is 2
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 100, b"p1"),
            Bytes::from_static(b"p1"),
        );
        jb.ingest_packet(p1, now);
        let _ = jb.pop_ready_packet(now);
        assert_eq!(jb.expected_sequence(), Some(2));

        // Ingest 3000 FecParity packets at sequences 100..3100
        for seq in 100..3100 {
            let p = Packet::new(
                PacketHeader::new(PacketType::FecParity, 0, seq, 1000, b"par"),
                Bytes::from_static(b"par"),
            );
            jb.ingest_packet(p, now);
        }

        // Then media at 3100
        let p3100 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 3100, 2000, b"p3100"),
            Bytes::from_static(b"p3100"),
        );
        jb.ingest_packet(p3100, now);

        let out3100 = jb.pop_ready_packet(now).expect("Packet 3100 must pop!");
        assert_eq!(out3100.header.sequence, 3100);

        // Gap [2, 3100) is 3098 sequence numbers, 3000 of them parity.
        // Correct media loss is exactly 98, NOT 1050!
        assert_eq!(
            jb.stats().packets_lost,
            98,
            "packets_lost must be exactly 98 (gap 3098 - 3000 parities)!"
        );
    }

    #[test]
    fn test_jitter_buffer_late_fec_recovery_behind_expected_does_not_evict_future_packets() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(5),
            fec_hold_timeout: Duration::from_millis(10),
            max_queue_depth: 2,
            ..Default::default()
        });

        let t0 = Instant::now();
        let block_id = 1000;
        let p10_payload = Bytes::from_static(b"source-10");
        let p11_payload = Bytes::from_static(b"source-11");
        let p12_payload = Bytes::from_static(b"source-12");

        // Encode Reed-Solomon FEC block for k=3, m=1
        let parities = ReedSolomonFec::encode(
            block_id,
            &[
                p10_payload.clone(),
                p11_payload.clone(),
                p12_payload.clone(),
            ],
            1,
        )
        .expect("FEC encode should succeed");

        // Step 1: Ingest source 10 and 12 (source 11 is missing)
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &p10_payload)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            p10_payload,
        );
        let p12 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &p12_payload)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            p12_payload,
        );
        jb.register_fec_block(block_id, 3, 1);
        jb.ingest_packet(p10, t0);
        jb.ingest_packet(p12, t0);

        // At t0 + 10ms (> initial_target_delay), pop packet 10
        let t1 = t0 + Duration::from_millis(10);
        let out10 = jb.pop_ready_packet(t1).expect("Packet 10 should pop");
        assert_eq!(out10.header.sequence, 10);
        assert_eq!(jb.expected_sequence(), Some(11));

        // Playout holds waiting for missing recoverable packet 11
        assert!(jb.pop_ready_packet(t1).is_none());

        // At t1 + 15ms (> fec_hold_timeout), playout hold expires:
        // packet 11 is declared lost, expected cursor moves to 12, and packet 12 is popped!
        let t2 = t1 + Duration::from_millis(15);
        let out12 = jb
            .pop_ready_packet(t2)
            .expect("Packet 12 should pop after hold timeout");
        assert_eq!(out12.header.sequence, 12);
        assert_eq!(jb.stats().packets_lost, 1);
        assert_eq!(jb.expected_sequence(), Some(13));
        assert_eq!(jb.queue_len(), 0);

        // Step 2: Ingest future packets 20 and 21, filling queue to max_queue_depth 2
        let p20 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 20, 2000, b"p20"),
            Bytes::from_static(b"p20"),
        );
        let p21 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 21, 2000, b"p21"),
            Bytes::from_static(b"p21"),
        );
        jb.ingest_packet(p20, t2);
        jb.ingest_packet(p21, t2);
        assert_eq!(jb.queue_len(), 2);

        // Step 3: Late parity packet arrives for the old block (block_id 1000)
        let p_parity = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        jb.ingest_packet(p_parity, t2);

        // Packet 11 was recovered but is behind expected (13); it must NOT be inserted into queue,
        // must NOT increment packets_reconstructed_fec, and must NOT evict future packet 21!
        assert_eq!(jb.stats().packets_reconstructed_fec, 0);
        assert_eq!(jb.queue_len(), 2, "Queue must remain 2 with [20, 21]");

        // Playout delivers packet 20 and 21 cleanly!
        let t3 = t2 + Duration::from_millis(10);
        let out20 = jb.pop_ready_packet(t3).expect("Packet 20 must pop");
        let out21 = jb.pop_ready_packet(t3).expect("Packet 21 must pop");
        assert_eq!(out20.header.sequence, 20);
        assert_eq!(out21.header.sequence, 21);
        assert_eq!(jb.stats().packets_delivered, 4); // 10, 12, 20, 21
    }

    #[test]
    fn test_jitter_buffer_prunes_incomplete_expired_fec_blocks() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            fec_hold_timeout: Duration::from_millis(5),
            ..Default::default()
        });

        let t0 = Instant::now();
        let block_id = 5000;
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, b"p10")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"p10"),
        );
        jb.register_fec_block(block_id, 3, 1);
        jb.ingest_packet(p10, t0);

        // Pop packet 10 -> expected is 11
        let _ = jb.pop_ready_packet(t0);
        assert_eq!(jb.expected_sequence(), Some(11));
        assert!(jb.fec_blocks.contains_key(&block_id));

        // Advance playout past the entire block range (base 10 + k 3 + m 1 = 14)
        // by ingesting and popping packet 15
        let p15 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 15, 6000, b"p15"),
            Bytes::from_static(b"p15"),
        );
        let t1 = t0 + Duration::from_millis(20);
        jb.ingest_packet(p15, t1);
        let _ = jb.pop_ready_packet(t1);

        assert_eq!(jb.expected_sequence(), Some(16));
        // Incomplete block must be pruned once expected >= 14!
        assert!(
            !jb.fec_blocks.contains_key(&block_id),
            "Incomplete expired FEC block should have been pruned"
        );
    }

    #[test]
    fn test_jitter_buffer_multi_packet_fec_recovery_preserves_all_recovered_packets() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            max_queue_depth: 2,
            ..Default::default()
        });

        let now = Instant::now();
        let block_id = 1000;
        let p10_payload = Bytes::from_static(b"source-10");
        let p11_payload = Bytes::from_static(b"source-11");
        let p12_payload = Bytes::from_static(b"source-12");

        // Reed-Solomon block with k=3, m=2
        let parities = ReedSolomonFec::encode(
            block_id,
            &[
                p10_payload.clone(),
                p11_payload.clone(),
                p12_payload.clone(),
            ],
            2,
        )
        .expect("FEC encode should succeed");

        // Queue initially has source seq 10 and future seq 20 (max_queue_depth = 2)
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &p10_payload)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            p10_payload,
        );
        let p20 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 20, 2000, b"p20"),
            Bytes::from_static(b"p20"),
        );
        jb.register_fec_block(block_id, 3, 2);
        jb.ingest_packet(p10, now);
        jb.ingest_packet(p20, now);
        assert_eq!(jb.queue_len(), 2);

        // Ingest two parity packets for block 1000 (sequences 13 and 14), triggering recovery of 11 and 12
        let p_par0 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let p_par1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 14, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );
        jb.ingest_packet(p_par0, now);
        jb.ingest_packet(p_par1, now);

        // Both source 11 and source 12 are recovered!
        assert_eq!(jb.stats().packets_reconstructed_fec, 2);
        // Queue capping must have dropped future packet 20 and evicted 10 from the front,
        // leaving the queue with {11, 12} clamped to max_queue_depth 2.
        assert_eq!(jb.queue_len(), 2);

        // Playout pops packet 11, then packet 12!
        let out11 = jb.pop_ready_packet(now).expect("Packet 11 must pop");
        let out12 = jb.pop_ready_packet(now).expect("Packet 12 must pop");
        assert_eq!(out11.header.sequence, 11);
        assert_eq!(out12.header.sequence, 12);
        assert_eq!(jb.stats().packets_delivered, 2);
        // Recovered packet 12 must NOT have been lost!
        assert_eq!(
            jb.stats().packets_lost,
            1,
            "Only evicted packet 10 should be counted lost"
        );
    }

    #[test]
    fn test_jitter_buffer_source_triggered_recovery_does_not_tail_drop_reconstructed_packet() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            max_queue_depth: 3,
            ..Default::default()
        });

        let now = Instant::now();
        let block_id = 1000;
        let p10_payload = Bytes::from_static(b"source-10");
        let p11_payload = Bytes::from_static(b"source-11");
        let p12_payload = Bytes::from_static(b"source-12");

        let parities = ReedSolomonFec::encode(
            block_id,
            &[
                p10_payload.clone(),
                p11_payload.clone(),
                p12_payload.clone(),
            ],
            2,
        )
        .expect("FEC encode should succeed");

        // Queue initially has sequences 5, 6, and 7
        for seq in 5..=7 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 100, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }
        assert_eq!(jb.queue_len(), 3);

        // Register FEC block for 10, 11, 12
        jb.register_fec_block(block_id, 3, 2);

        // Ingest both parities (sequences 13 and 14)
        let p_par0 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let p_par1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 14, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );
        jb.ingest_packet(p_par0, now);
        jb.ingest_packet(p_par1, now);

        // Now source packet 11 arrives (index 1), which completes the required K packets
        // and triggers recovery of source packets 10 and 12!
        let p11 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, block_id, &p11_payload)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            p11_payload,
        );
        jb.ingest_packet(p11, now);

        // Both 10 and 12 were reconstructed
        assert_eq!(jb.stats().packets_reconstructed_fec, 2);
        // Queue capping must have evicted older packets 5, 6, 7 from the front,
        // keeping {10, 11, 12} intact!
        assert_eq!(jb.queue_len(), 3);

        let out10 = jb.pop_ready_packet(now).expect("Packet 10 must pop");
        let out11 = jb.pop_ready_packet(now).expect("Packet 11 must pop");
        let out12 = jb.pop_ready_packet(now).expect("Packet 12 must pop");
        assert_eq!(out10.header.sequence, 10);
        assert_eq!(out11.header.sequence, 11);
        assert_eq!(out12.header.sequence, 12);
        assert_eq!(jb.stats().packets_delivered, 3);
    }

    #[test]
    fn test_jitter_buffer_fec_hold_inspects_gap_beyond_256_packets() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            fec_hold_timeout: Duration::from_millis(50),
            ..Default::default()
        });

        let t0 = Instant::now();
        // Ingest packet 1 and pop it -> expected cursor is 2
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 100, b"p1"),
            Bytes::from_static(b"p1"),
        );
        jb.ingest_packet(p1, t0);
        let out1 = jb.pop_ready_packet(t0).expect("Packet 1 must pop");
        assert_eq!(out1.header.sequence, 1);
        assert_eq!(jb.expected_sequence(), Some(2));

        // Block with base 300, k=2, m=1
        // Source 300 is missing (index 0)
        // Source 301 is present (index 1)
        let block_id = 9000;
        jb.register_fec_block(block_id, 2, 1);
        let p301 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 301, block_id, b"p301")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            Bytes::from_static(b"p301"),
        );
        jb.ingest_packet(p301, t0);

        // Gap between expected (2) and head (301) is 299 packets (> 256).
        // Since sequence 300 in that gap belongs to recoverable block 9000,
        // playout MUST hold rather than immediately popping 301 and declaring 300 lost!
        let t1 = t0 + Duration::from_millis(10);
        let out = jb.pop_ready_packet(t1);
        assert!(
            out.is_none(),
            "Playout must hold for recoverable packet 300 despite gap > 256"
        );
        assert_eq!(jb.expected_sequence(), Some(2));
    }
}
