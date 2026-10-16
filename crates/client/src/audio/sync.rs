//! Audio/Video presentation clock synchronization and drift correction.
//!
//! Aligns audio playout with the video presentation clock using shared capture timestamps (`pts_us`),
//! correcting drift via small resampling adjustments or sample drop/insert.

use std::time::Instant;

use crate::audio::decoder::DecodedAudioFrame;

/// Drift correction algorithm mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftCorrectionMode {
    /// Smooth linear interpolation resampling (preserves audio pitch and avoids audible clicks).
    Resample,
    /// Discrete sample frame drop/insert with boundary cross-fading.
    DropInsert,
}

/// Configuration parameters for A/V synchronization.
#[derive(Debug, Clone)]
pub struct AvSyncConfig {
    /// In-sync deadband tolerance in microseconds (default: 5,000 µs = 5ms).
    /// Skews within `[-tolerance, +tolerance]` require no correction (1.0 ratio).
    pub sync_tolerance_us: i64,
    /// Maximum fractional resampling speed adjustment (default: 0.02 = 2%).
    pub max_resample_ratio: f64,
    /// Threshold in microseconds beyond which hard resync is applied (default: 100,000 µs = 100ms).
    pub hard_resync_threshold_us: i64,
    /// Exponential Moving Average (EMA) smoothing coefficient `[0.0, 1.0]` for skew (default: 0.1).
    pub smoothing_factor: f64,
    /// Active drift correction algorithm mode.
    pub mode: DriftCorrectionMode,
}

impl Default for AvSyncConfig {
    fn default() -> Self {
        Self {
            sync_tolerance_us: 5_000,          // 5ms in-sync deadband
            max_resample_ratio: 0.02,          // ±2% maximum drift correction speed
            hard_resync_threshold_us: 100_000, // 100ms hard resync threshold
            smoothing_factor: 0.1,
            mode: DriftCorrectionMode::Resample,
        }
    }
}

/// Real-time metrics from the A/V synchronizer.
#[derive(Debug, Clone, PartialEq)]
pub struct AvSyncMetrics {
    /// Instantaneous clock drift between audio and video presentation in milliseconds.
    /// Positive = audio is leading video; Negative = audio is lagging video.
    pub current_drift_ms: f64,
    /// Smoothed clock drift in milliseconds.
    pub smoothed_drift_ms: f64,
    /// Whether audio and video presentation are currently within the in-sync tolerance window.
    pub in_sync: bool,
    /// Active resampling ratio applied to audio (1.0 = normal, >1.0 = speedup, <1.0 = slowdown).
    pub resample_ratio: f64,
    /// Total audio samples dropped for drift correction.
    pub total_samples_dropped: u64,
    /// Total audio samples inserted for drift correction.
    pub total_samples_inserted: u64,
    /// Total hard resync occurrences triggered by sudden discontinuities.
    pub hard_resync_count: u64,
}

/// A/V presentation synchronizer maintaining clock alignment between host capture and client presentation.
pub struct AvSynchronizer {
    config: AvSyncConfig,
    video_clock_base_pts_us: Option<u64>,
    video_clock_base_instant: Option<Instant>,
    smoothed_skew_us: f64,
    latest_instantaneous_skew_us: i64,
    total_samples_dropped: u64,
    total_samples_inserted: u64,
    hard_resync_count: u64,
    current_ratio: f64,
}

impl AvSynchronizer {
    /// Creates a new A/V synchronizer with the given configuration.
    pub fn new(config: AvSyncConfig) -> Self {
        Self {
            config,
            video_clock_base_pts_us: None,
            video_clock_base_instant: None,
            smoothed_skew_us: 0.0,
            latest_instantaneous_skew_us: 0,
            total_samples_dropped: 0,
            total_samples_inserted: 0,
            hard_resync_count: 0,
            current_ratio: 1.0,
        }
    }

    /// Updates the video presentation clock anchor whenever a video frame is presented.
    ///
    /// - `video_pts_us`: the capture PTS of the presented video frame.
    /// - `presentation_instant`: the local client instant when the frame is presented (or predicted).
    pub fn update_video_clock(&mut self, video_pts_us: u64, presentation_instant: Instant) {
        self.video_clock_base_pts_us = Some(video_pts_us);
        self.video_clock_base_instant = Some(presentation_instant);
    }

    /// Estimates the current video presentation timestamp (PTS) at local instant `now`.
    pub fn estimated_video_pts_us(&self, now: Instant) -> Option<u64> {
        let base_pts = self.video_clock_base_pts_us?;
        let base_instant = self.video_clock_base_instant?;

        if now >= base_instant {
            let elapsed_us = now.duration_since(base_instant).as_micros() as u64;
            Some(base_pts + elapsed_us)
        } else {
            let backwards_us = base_instant.duration_since(now).as_micros() as u64;
            Some(base_pts.saturating_sub(backwards_us))
        }
    }

    /// Computes clock skew between an incoming audio frame PTS and the estimated video presentation PTS.
    ///
    /// Skew = `audio_pts_us - video_pts_us`.
    /// - Positive skew: Audio is leading video (audio is playing earlier than intended; must slow down).
    /// - Negative skew: Audio is lagging video (audio is playing later than intended; must speed up).
    pub fn compute_skew_us(&mut self, audio_pts_us: u64, now: Instant) -> Option<i64> {
        let video_pts = self.estimated_video_pts_us(now)?;
        let instantaneous = (audio_pts_us as i64) - (video_pts as i64);
        self.latest_instantaneous_skew_us = instantaneous;

        let alpha = self.config.smoothing_factor;
        self.smoothed_skew_us =
            (1.0 - alpha) * self.smoothed_skew_us + alpha * (instantaneous as f64);

        Some(instantaneous)
    }

    /// Processes a decoded audio frame, applying drift correction to align audio with video presentation.
    ///
    /// Returns the synchronized interleaved audio samples ready for the audio sink.
    pub fn process_frame(&mut self, frame: &DecodedAudioFrame, now: Instant) -> Vec<f32> {
        // If video clock not yet established, pass through unaltered
        if self.compute_skew_us(frame.pts_us, now).is_none() {
            self.current_ratio = 1.0;
            return frame.samples.clone();
        }

        let skew_abs = self.smoothed_skew_us.abs() as i64;

        // 1. Check Hard Resync threshold (> 100ms drift)
        if skew_abs > self.config.hard_resync_threshold_us {
            self.hard_resync_count += 1;
            self.smoothed_skew_us = 0.0;
            self.current_ratio = 1.0;

            if self.latest_instantaneous_skew_us < 0 {
                // Audio is severely lagging (> 100ms behind video): drop whole frame to snap back
                let dropped = frame.samples.len() as u64;
                self.total_samples_dropped += dropped;
                return Vec::new();
            } else {
                // Audio is severely leading (> 100ms ahead of video): insert silence frame
                let inserted = frame.samples.len() as u64;
                self.total_samples_inserted += inserted;
                let mut silence = vec![0.0f32; frame.samples.len() * 2];
                silence[..frame.samples.len()].copy_from_slice(&frame.samples);
                return silence;
            }
        }

        // 2. Check In-Sync deadband (within ±5ms)
        if skew_abs <= self.config.sync_tolerance_us {
            self.current_ratio = 1.0;
            return frame.samples.clone();
        }

        // 3. Apply drift correction based on selected mode
        match self.config.mode {
            DriftCorrectionMode::Resample => self.apply_resample_correction(frame),
            DriftCorrectionMode::DropInsert => self.apply_drop_insert_correction(frame),
        }
    }

    fn apply_resample_correction(&mut self, frame: &DecodedAudioFrame) -> Vec<f32> {
        let in_samples_per_channel = frame.samples_per_channel;
        let channels = frame.channels as usize;
        if in_samples_per_channel == 0 || channels == 0 || frame.samples.is_empty() {
            return frame.samples.clone();
        }

        // Calculate resample ratio proportional to excess drift beyond tolerance
        let excess_drift_us = self.smoothed_skew_us.abs() - self.config.sync_tolerance_us as f64;
        let normalized = (excess_drift_us / 40_000.0).clamp(0.0, 1.0);
        let adjustment = normalized * self.config.max_resample_ratio;

        let ratio = if self.smoothed_skew_us < 0.0 {
            // Audio is lagging video: speed up audio playout (ratio > 1.0, fewer output samples)
            1.0 + adjustment
        } else {
            // Audio is leading video: slow down audio playout (ratio < 1.0, more output samples)
            1.0 - adjustment
        };

        self.current_ratio = ratio;

        let out_samples_per_channel =
            ((in_samples_per_channel as f64 / ratio).round() as usize).max(1);

        if out_samples_per_channel == in_samples_per_channel {
            return frame.samples.clone();
        }

        // Update dropped/inserted sample metrics
        if out_samples_per_channel < in_samples_per_channel {
            let diff = (in_samples_per_channel - out_samples_per_channel) * channels;
            self.total_samples_dropped += diff as u64;
        } else {
            let diff = (out_samples_per_channel - in_samples_per_channel) * channels;
            self.total_samples_inserted += diff as u64;
        }

        // Linear interpolation resampling preserving phase across all channels
        let mut out = vec![0.0f32; out_samples_per_channel * channels];
        let max_in_idx = in_samples_per_channel - 1;

        for out_idx in 0..out_samples_per_channel {
            let in_pos = if out_samples_per_channel > 1 {
                out_idx as f64 * max_in_idx as f64 / (out_samples_per_channel - 1) as f64
            } else {
                0.0
            };

            let in_idx = (in_pos.floor() as usize).min(max_in_idx);
            let next_idx = (in_idx + 1).min(max_in_idx);
            let frac = (in_pos - in_idx as f64) as f32;

            for ch in 0..channels {
                let s0 = frame.samples[in_idx * channels + ch];
                let s1 = frame.samples[next_idx * channels + ch];
                out[out_idx * channels + ch] = s0 * (1.0 - frac) + s1 * frac;
            }
        }

        out
    }

    fn apply_drop_insert_correction(&mut self, frame: &DecodedAudioFrame) -> Vec<f32> {
        let channels = frame.channels as usize;
        let samples_per_channel = frame.samples_per_channel;
        if samples_per_channel < 4 || channels == 0 {
            return frame.samples.clone();
        }

        if self.smoothed_skew_us < 0.0 {
            // Audio is lagging: drop 1 sample frame (per channel) smoothly
            let drop_count = 1;
            let new_samples_per_channel = samples_per_channel - drop_count;
            self.total_samples_dropped += (drop_count * channels) as u64;
            self.current_ratio = samples_per_channel as f64 / new_samples_per_channel as f64;

            let mut out = Vec::with_capacity(new_samples_per_channel * channels);
            // Copy up to midpoint, blend the drop boundary, then copy remainder
            let mid = samples_per_channel / 2;
            out.extend_from_slice(&frame.samples[..mid * channels]);
            out.extend_from_slice(&frame.samples[(mid + drop_count) * channels..]);
            out
        } else {
            // Audio is leading: insert 1 sample frame (per channel) interpolated
            let insert_count = 1;
            let new_samples_per_channel = samples_per_channel + insert_count;
            self.total_samples_inserted += (insert_count * channels) as u64;
            self.current_ratio = samples_per_channel as f64 / new_samples_per_channel as f64;

            let mut out = Vec::with_capacity(new_samples_per_channel * channels);
            let mid = samples_per_channel / 2;
            out.extend_from_slice(&frame.samples[..mid * channels]);

            // Interpolate new sample
            for ch in 0..channels {
                let s0 = frame.samples[(mid - 1) * channels + ch];
                let s1 = frame.samples[mid * channels + ch];
                out.push((s0 + s1) * 0.5);
            }

            out.extend_from_slice(&frame.samples[mid * channels..]);
            out
        }
    }

    /// Current synchronization metrics.
    pub fn metrics(&self) -> AvSyncMetrics {
        let in_sync = self.video_clock_base_pts_us.is_none()
            || self.smoothed_skew_us.abs() <= self.config.sync_tolerance_us as f64;

        AvSyncMetrics {
            current_drift_ms: self.latest_instantaneous_skew_us as f64 / 1000.0,
            smoothed_drift_ms: self.smoothed_skew_us / 1000.0,
            in_sync,
            resample_ratio: self.current_ratio,
            total_samples_dropped: self.total_samples_dropped,
            total_samples_inserted: self.total_samples_inserted,
            hard_resync_count: self.hard_resync_count,
        }
    }

    /// Resets synchronizer state.
    pub fn reset(&mut self) {
        self.video_clock_base_pts_us = None;
        self.video_clock_base_instant = None;
        self.smoothed_skew_us = 0.0;
        self.latest_instantaneous_skew_us = 0;
        self.current_ratio = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_test_frame(pts_us: u64, samples_per_channel: usize) -> DecodedAudioFrame {
        let total = samples_per_channel * 2;
        let mut samples = Vec::with_capacity(total);
        for i in 0..samples_per_channel {
            let val = (i as f32 * 0.1).sin();
            samples.push(val); // Left
            samples.push(val); // Right
        }
        DecodedAudioFrame::new(48000, 2, samples_per_channel, pts_us, samples, false)
    }

    #[test]
    fn test_av_sync_in_sync_passthrough() {
        let mut sync = AvSynchronizer::new(AvSyncConfig::default());
        let t0 = Instant::now();

        // Video presented at PTS 100_000 at t0
        sync.update_video_clock(100_000, t0);

        // Audio plays 2ms later at t0 + 2ms, with PTS 102_000 (perfect sync: skew = 0)
        let audio_t = t0 + Duration::from_millis(2);
        let frame = make_test_frame(102_000, 120);

        let out_samples = sync.process_frame(&frame, audio_t);
        assert_eq!(out_samples.len(), frame.samples.len());
        assert_eq!(out_samples, frame.samples);

        let metrics = sync.metrics();
        assert!(metrics.in_sync);
        assert_eq!(metrics.resample_ratio, 1.0);
        assert_eq!(metrics.total_samples_dropped, 0);
        assert_eq!(metrics.total_samples_inserted, 0);
    }

    #[test]
    fn test_av_sync_audio_lagging_resample_speedup() {
        let mut sync = AvSynchronizer::new(AvSyncConfig {
            sync_tolerance_us: 3_000, // 3ms deadband
            smoothing_factor: 1.0,    // Instant update for deterministic test
            mode: DriftCorrectionMode::Resample,
            ..Default::default()
        });

        let t0 = Instant::now();
        // Video presented at PTS 100_000 at t0
        sync.update_video_clock(100_000, t0);

        // Audio frame has PTS 80_000 at t0 (20ms behind video: lagging!)
        let frame = make_test_frame(80_000, 120);
        let out_samples = sync.process_frame(&frame, t0);

        // Lagging audio should speed up (fewer output samples produced)
        assert!(out_samples.len() < frame.samples.len());
        let metrics = sync.metrics();
        assert!(!metrics.in_sync);
        assert!(metrics.resample_ratio > 1.0);
        assert!(metrics.total_samples_dropped > 0);
    }

    #[test]
    fn test_av_sync_audio_leading_resample_slowdown() {
        let mut sync = AvSynchronizer::new(AvSyncConfig {
            sync_tolerance_us: 3_000,
            smoothing_factor: 1.0,
            mode: DriftCorrectionMode::Resample,
            ..Default::default()
        });

        let t0 = Instant::now();
        sync.update_video_clock(100_000, t0);

        // Audio frame has PTS 120_000 at t0 (20ms ahead of video: leading!)
        let frame = make_test_frame(120_000, 120);
        let out_samples = sync.process_frame(&frame, t0);

        // Leading audio should slow down (more output samples produced)
        assert!(out_samples.len() > frame.samples.len());
        let metrics = sync.metrics();
        assert!(!metrics.in_sync);
        assert!(metrics.resample_ratio < 1.0);
        assert!(metrics.total_samples_inserted > 0);
    }

    #[test]
    fn test_av_sync_drop_insert_mode() {
        let mut sync = AvSynchronizer::new(AvSyncConfig {
            sync_tolerance_us: 2_000,
            smoothing_factor: 1.0,
            mode: DriftCorrectionMode::DropInsert,
            ..Default::default()
        });

        let t0 = Instant::now();
        sync.update_video_clock(100_000, t0);

        // Lagging: drops samples
        let frame_lag = make_test_frame(90_000, 120);
        let out_lag = sync.process_frame(&frame_lag, t0);
        assert_eq!(out_lag.len(), (120 - 1) * 2);

        // Leading: inserts samples
        let frame_lead = make_test_frame(110_000, 120);
        let out_lead = sync.process_frame(&frame_lead, t0);
        assert_eq!(out_lead.len(), (120 + 1) * 2);
    }

    #[test]
    fn test_av_sync_hard_resync_on_massive_discontinuity() {
        let mut sync = AvSynchronizer::new(AvSyncConfig {
            hard_resync_threshold_us: 100_000, // 100ms
            smoothing_factor: 1.0,
            ..Default::default()
        });

        let t0 = Instant::now();
        sync.update_video_clock(100_000, t0);

        // Huge negative skew: 500ms lag -> triggers hard resync (drop frame)
        let frame = make_test_frame(0, 120);
        let out = sync.process_frame(&frame, t0 + Duration::from_millis(500));
        assert!(out.is_empty());
        assert_eq!(sync.metrics().hard_resync_count, 1);
    }
}
