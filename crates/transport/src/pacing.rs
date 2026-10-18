use std::time::{Duration, Instant};

/// Packet pacer to prevent packet bursts from saturating Wi-Fi router buffers.
/// Uses a leaky-bucket pacing algorithm with sub-millisecond precision.
pub struct PacketPacer {
    rate_bytes_per_sec: u64,
    last_send_time: Instant,
    burst_allowance_bytes: usize,
    accumulated_bytes: usize,
}

impl PacketPacer {
    pub fn new(target_mbps: u32) -> Self {
        let rate_bytes_per_sec = (target_mbps as u64 * 1_000_000) / 8;
        // Default burst allowance: up to ~2 packets (2800 bytes)
        let burst_allowance_bytes = 2800;
        Self {
            rate_bytes_per_sec,
            last_send_time: Instant::now(),
            burst_allowance_bytes,
            accumulated_bytes: 0,
        }
    }

    /// Dynamically update target transmission bitrate.
    pub fn set_target_mbps(&mut self, target_mbps: u32) {
        self.rate_bytes_per_sec = (target_mbps as u64 * 1_000_000) / 8;
    }

    pub fn target_mbps(&self) -> u32 {
        ((self.rate_bytes_per_sec * 8) / 1_000_000) as u32
    }

    /// Wait for transmission slot according to current pacing rate.
    pub async fn pace(&mut self, packet_size_bytes: usize) {
        let delay = self.reserve(packet_size_bytes, Instant::now());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// Reserves a transmission slot for a packet of `packet_size_bytes` at time `now` and returns
    /// how long after `now` the packet may be sent, without sleeping.
    ///
    /// `now` is the caller's clock. It may be a virtual schedule time that runs ahead of the real
    /// clock, which lets a sender coalesce sub-millisecond waits into micro-bursts (timer
    /// granularity is about 1 ms) while the long-run rate still follows the schedule.
    /// [`PacketPacer::pace`] is `reserve(.., Instant::now())` followed by a sleep.
    pub fn reserve(&mut self, packet_size_bytes: usize, now: Instant) -> Duration {
        if self.rate_bytes_per_sec == 0 {
            return Duration::ZERO;
        }

        let mut elapsed = now.saturating_duration_since(self.last_send_time);

        // Reset on large gaps (> 1 second) to prevent token arithmetic overflow and ensure freshness
        if elapsed > Duration::from_secs(1) {
            self.accumulated_bytes = 0;
            self.last_send_time = now;
            elapsed = Duration::ZERO;
        }

        let elapsed_nanos = elapsed.as_nanos();

        // Drain accumulated tokens according to elapsed time using u128 arithmetic
        let bytes_budget =
            ((elapsed_nanos * self.rate_bytes_per_sec as u128) / 1_000_000_000) as usize;
        self.accumulated_bytes = self.accumulated_bytes.saturating_sub(bytes_budget);
        self.accumulated_bytes += packet_size_bytes;

        if self.accumulated_bytes > self.burst_allowance_bytes {
            let excess = self.accumulated_bytes - self.burst_allowance_bytes;
            let delay_nanos =
                ((excess as u128 * 1_000_000_000) / self.rate_bytes_per_sec as u128) as u64;
            let delay = Duration::from_nanos(delay_nanos);
            self.last_send_time = now + delay;
            self.accumulated_bytes = self.burst_allowance_bytes;
            delay
        } else {
            self.last_send_time = now;
            Duration::ZERO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_packet_pacer_rate_adjustment() {
        let mut pacer = PacketPacer::new(100);
        assert_eq!(pacer.target_mbps(), 100);

        pacer.set_target_mbps(200);
        assert_eq!(pacer.target_mbps(), 200);
    }

    #[tokio::test]
    async fn test_packet_pacer_pacing_delay() {
        let mut pacer = PacketPacer::new(50); // 50 Mbps
        let start = Instant::now();

        // Send 10 packets of 1400 bytes each
        for _ in 0..10 {
            pacer.pace(1400).await;
        }

        let elapsed = start.elapsed();
        // 14000 bytes at 50Mbps = 112,000 bits / 50,000,000 bps = 2.24ms.
        // Assert tighter bounds: at least 1.5ms (>65% of expected) and under 50ms.
        assert!(
            elapsed.as_micros() >= 1500,
            "Pacing should take at least 1.5ms, got {:?}",
            elapsed
        );
        assert!(elapsed.as_millis() < 50);
    }

    #[test]
    fn test_reserve_spreads_a_burst_over_time_without_sleeping() {
        let mut pacer = PacketPacer::new(10); // 1.25 MB/s
        let t0 = Instant::now();
        let mut cursor = t0;
        let mut last_delay = Duration::ZERO;
        // 100 packets of 1250 bytes = 125 kB, which takes 100 ms at 10 Mbps (minus the burst).
        for _ in 0..100 {
            last_delay = pacer.reserve(1250, cursor);
            cursor += last_delay;
        }
        assert!(!last_delay.is_zero(), "steady state paces every packet");
        let total = cursor - t0;
        assert!(
            total >= Duration::from_millis(95) && total <= Duration::from_millis(100),
            "125 kB at 10 Mbps minus a 2800 B burst allowance, got {total:?}"
        );
    }

    #[test]
    fn test_reserve_with_zero_rate_never_delays() {
        let mut pacer = PacketPacer::new(0);
        assert_eq!(pacer.reserve(1400, Instant::now()), Duration::ZERO);
    }
}
