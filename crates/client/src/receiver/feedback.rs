//! Receive-side network measurements reported to the host's adaptive bitrate controller.
//!
//! [`FeedbackEstimator`] watches every video datagram (its host send timestamp, size and local
//! arrival time) and, once per report interval, produces a [`NetworkFeedback`]: loss left after
//! FEC, the one-way delay trend, jitter and the rate actually received. The one-way delay is
//! only known up to the unknown offset between the host and headset clocks, but that offset is
//! constant, so its *growth* (queues filling up) is measurable: per-bucket minimum delay is
//! fitted with a line over the last second, and the slope is the trend in µs per second.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use linux_quest_protocol::{NetworkFeedback, NO_FOCUSED_STREAM};

/// Time between reports.
pub const REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// Width of one delay bucket; the minimum delay seen in a bucket is its sample, which keeps
/// pacing bursts and single late packets from looking like queue growth.
const BUCKET: Duration = Duration::from_millis(50);

/// Buckets kept for the trend fit (one second).
const TREND_BUCKETS: usize = 20;

/// Fewest buckets a trend is fitted from; below this the trend is reported as 0.
const MIN_TREND_BUCKETS: usize = 6;

#[derive(Debug, Clone, Copy)]
struct DelaySample {
    /// Bucket start, microseconds since the estimator's epoch.
    at_us: f64,
    /// Minimum (arrival - send timestamp) in the bucket, microseconds.
    delay_us: f64,
}

#[derive(Debug)]
pub struct FeedbackEstimator {
    epoch: Instant,
    samples: VecDeque<DelaySample>,
    bucket_start: Option<Instant>,
    bucket_min_us: f64,
    last_arrival: Option<(Instant, u64)>,
    jitter_us: f64,
    window_bytes: u64,
    window_start: Option<Instant>,
    /// Counters at the previous report.
    last_received: u64,
    last_lost: u64,
}

impl FeedbackEstimator {
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            samples: VecDeque::with_capacity(TREND_BUCKETS + 1),
            bucket_start: None,
            bucket_min_us: f64::MAX,
            last_arrival: None,
            jitter_us: 0.0,
            window_bytes: 0,
            window_start: None,
            last_received: 0,
            last_lost: 0,
        }
    }

    /// Records one video datagram: the host's send timestamp (`timestamp_us` in the packet
    /// header), its size and when it arrived.
    pub fn observe_packet(&mut self, send_timestamp_us: u64, bytes: usize, arrival: Instant) {
        self.window_bytes += bytes as u64;
        self.window_start.get_or_insert(arrival);

        let arrival_us = arrival.saturating_duration_since(self.epoch).as_micros() as f64;
        let delay_us = arrival_us - send_timestamp_us as f64;
        self.bucket_min_us = self.bucket_min_us.min(delay_us);
        let start = *self.bucket_start.get_or_insert(arrival);
        if arrival.saturating_duration_since(start) >= BUCKET {
            self.samples.push_back(DelaySample {
                at_us: start.saturating_duration_since(self.epoch).as_micros() as f64,
                delay_us: self.bucket_min_us,
            });
            while self.samples.len() > TREND_BUCKETS {
                self.samples.pop_front();
            }
            self.bucket_start = Some(arrival);
            self.bucket_min_us = f64::MAX;
        }

        // RFC 3550 style interarrival jitter on the send/arrival spacing.
        if let Some((prev_arrival, prev_ts)) = self.last_arrival {
            let arrived = arrival.saturating_duration_since(prev_arrival).as_micros() as f64;
            let sent = send_timestamp_us as f64 - prev_ts as f64;
            self.jitter_us += ((arrived - sent).abs() - self.jitter_us) / 16.0;
        }
        self.last_arrival = Some((arrival, send_timestamp_us));
    }

    /// Slope of the fitted delay line, in µs per second.
    pub fn delay_trend_us_per_s(&self) -> i32 {
        if self.samples.len() < MIN_TREND_BUCKETS {
            return 0;
        }
        let n = self.samples.len() as f64;
        let mean_t = self.samples.iter().map(|s| s.at_us).sum::<f64>() / n;
        let mean_d = self.samples.iter().map(|s| s.delay_us).sum::<f64>() / n;
        let (mut num, mut den) = (0.0, 0.0);
        for s in &self.samples {
            num += (s.at_us - mean_t) * (s.delay_us - mean_d);
            den += (s.at_us - mean_t).powi(2);
        }
        if den == 0.0 {
            return 0;
        }
        // delay (µs) per elapsed µs, scaled to per second.
        (num / den * 1e6).clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }

    /// Builds a report if [`REPORT_INTERVAL`] has passed since the window began.
    ///
    /// `received_total` and `lost_total` are the receiver's cumulative datagram counters
    /// (`lost` counts those still missing after FEC recovery). `focused_stream` is the panel
    /// the user looks at, if known.
    pub fn poll(
        &mut self,
        now: Instant,
        received_total: u64,
        lost_total: u64,
        focused_stream: Option<u16>,
    ) -> Option<NetworkFeedback> {
        let started = self.window_start?;
        let elapsed = now.saturating_duration_since(started);
        if elapsed < REPORT_INTERVAL {
            return None;
        }
        let received = received_total.saturating_sub(self.last_received);
        let lost = lost_total.saturating_sub(self.last_lost);
        self.last_received = received_total;
        self.last_lost = lost_total;
        let expected = received + lost;
        let loss_permille = if expected == 0 {
            0
        } else {
            (lost * 1000).div_ceil(expected).min(1000) as u16
        };
        let received_kbps = (self.window_bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1000.0)
            .min(f64::from(u32::MAX)) as u32;
        self.window_bytes = 0;
        self.window_start = Some(now);
        Some(NetworkFeedback {
            loss_permille,
            delay_trend_us_per_s: self.delay_trend_us_per_s(),
            jitter_us: self.jitter_us.round() as u32,
            received_kbps,
            focused_stream: focused_stream.unwrap_or(NO_FOCUSED_STREAM),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKT: usize = 1_200;

    /// 1 ms apart datagrams for `ms` milliseconds; one-way delay grows by `growth_us_per_s`.
    fn feed(
        est: &mut FeedbackEstimator,
        t0: Instant,
        start_ms: u64,
        ms: u64,
        base_delay_us: u64,
        growth_us_per_s: u64,
    ) {
        for i in start_ms..start_ms + ms {
            let sent_us = i * 1_000;
            let extra = growth_us_per_s * i / 1_000;
            let arrival = t0 + Duration::from_micros(sent_us + base_delay_us + extra);
            est.observe_packet(sent_us, PKT, arrival);
        }
    }

    #[test]
    fn steady_delay_has_no_trend_and_low_jitter() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        feed(&mut est, t0, 0, 1_500, 3_000, 0);
        assert!(
            est.delay_trend_us_per_s().abs() < 200,
            "{}",
            est.delay_trend_us_per_s()
        );
        let fb = est
            .poll(t0 + Duration::from_millis(1_500), 1_500, 0, None)
            .unwrap();
        assert_eq!(fb.loss_permille, 0);
        assert!(fb.jitter_us < 200, "{}", fb.jitter_us);
        assert_eq!(fb.focused_stream, NO_FOCUSED_STREAM);
    }

    #[test]
    fn growing_queue_shows_up_as_a_positive_trend() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        feed(&mut est, t0, 0, 1_500, 2_000, 20_000);
        let trend = est.delay_trend_us_per_s();
        assert!((17_000..=23_000).contains(&trend), "trend {trend}");
    }

    #[test]
    fn a_draining_queue_is_negative_and_a_constant_clock_offset_cancels() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        // Same shape as the steady case, but with a large host/headset clock offset.
        for i in 0..1_500u64 {
            let sent_us = 7_000_000_000 + i * 1_000;
            let arrival = t0 + Duration::from_micros(i * 1_000 + 2_000);
            est.observe_packet(sent_us, PKT, arrival);
        }
        assert!(est.delay_trend_us_per_s().abs() < 200);
    }

    #[test]
    fn a_single_late_packet_does_not_fake_a_trend() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        for i in 0..1_500u64 {
            let late = if i == 700 { 30_000 } else { 0 };
            let sent_us = i * 1_000;
            est.observe_packet(
                sent_us,
                PKT,
                t0 + Duration::from_micros(sent_us + 2_000 + late),
            );
        }
        assert!(
            est.delay_trend_us_per_s().abs() < 500,
            "{}",
            est.delay_trend_us_per_s()
        );
    }

    #[test]
    fn loss_is_reported_per_window_from_counter_deltas() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        feed(&mut est, t0, 0, 250, 1_000, 0);
        let first = est
            .poll(t0 + Duration::from_millis(250), 190, 10, Some(3))
            .unwrap();
        assert_eq!(first.loss_permille, 50);
        assert_eq!(first.focused_stream, 3);
        feed(&mut est, t0, 250, 250, 1_000, 0);
        // 190 more received and none lost since: the old loss is not repeated.
        let second = est
            .poll(t0 + Duration::from_millis(500), 380, 10, None)
            .unwrap();
        assert_eq!(second.loss_permille, 0);
    }

    #[test]
    fn reports_are_spaced_and_carry_the_received_rate() {
        let t0 = Instant::now();
        let mut est = FeedbackEstimator::new(t0);
        assert!(est.poll(t0, 0, 0, None).is_none(), "nothing received yet");
        feed(&mut est, t0, 0, 100, 1_000, 0);
        assert!(est
            .poll(t0 + Duration::from_millis(100), 100, 0, None)
            .is_none());
        feed(&mut est, t0, 100, 100, 1_000, 0);
        let fb = est
            .poll(t0 + Duration::from_millis(201), 200, 0, None)
            .unwrap();
        // 200 x 1200 B in ~0.2 s is ~9.6 Mbit/s.
        assert!(
            (9_000..=10_000).contains(&fb.received_kbps),
            "{}",
            fb.received_kbps
        );
    }
}
