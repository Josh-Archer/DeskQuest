//! OpenXR frame pacing and rendering synchronization loop.

use std::time::{Duration, Instant};

use crate::error::ClientResult;
use crate::openxr::context::OpenXrContext;

/// Frame timing metrics calculated per presentation cycle.
#[derive(Debug, Clone, Default)]
pub struct FramePacingMetrics {
    /// Time spent waiting for display VSync in microseconds.
    pub wait_frame_duration_us: u64,
    /// Time spent rendering and composing layers in microseconds.
    pub render_duration_us: u64,
    /// Software estimate in microseconds: the refresh-rate frame budget plus render time.
    /// Not a measurement and not glass-to-glass latency (no capture, encode, transport or
    /// panel scanout).
    pub motion_to_photon_latency_us: u64,
    /// Predicted display presentation time.
    pub predicted_display_time: i64,
    /// Whether OpenXR runtime requests that layers should be rendered this frame.
    pub should_render: bool,
}

/// Selects a surface presentation slot with enough time for the native surface queue to accept
/// the decoded buffer. A predicted OpenXR display time may be only a fraction of a refresh
/// interval away by the time decoding finishes; Android may then classify the buffer as late.
pub(crate) fn surface_presentation_time_ns(
    predicted_display_ns: i64,
    now_ns: i64,
    frame_period_ns: i64,
    minimum_lead_ns: i64,
) -> i64 {
    let period = frame_period_ns.max(1);
    let deadline = now_ns.saturating_add(minimum_lead_ns.max(0));
    if predicted_display_ns >= deadline {
        return predicted_display_ns;
    }

    let late_by = deadline.saturating_sub(predicted_display_ns);
    let periods = (late_by / period).saturating_add(i64::from(late_by % period != 0));
    predicted_display_ns.saturating_add(periods.saturating_mul(period))
}

/// Orchestrates the OpenXR frame pacing loop synchronized to headset refresh rates.
pub struct FrameLoopEngine {
    last_frame_instant: Instant,
    last_pacing_metrics: FramePacingMetrics,
    frame_counter: u64,
}

impl Default for FrameLoopEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameLoopEngine {
    /// Creates a new frame loop engine.
    pub fn new() -> Self {
        Self {
            last_frame_instant: Instant::now(),
            last_pacing_metrics: FramePacingMetrics::default(),
            frame_counter: 0,
        }
    }

    /// Paces the loop to the target refresh rate.
    pub fn wait_frame_pacing(
        &mut self,
        context: &OpenXrContext,
    ) -> ClientResult<FramePacingMetrics> {
        let wait_start = Instant::now();

        let budget_us = if let Some(mgr) = context.refresh_rate_manager() {
            mgr.frame_budget_us()
        } else {
            11_111 // 90 Hz default
        };

        let (predicted_time, should_render) = if context.is_simulated() {
            // In simulation mode, pace according to target refresh rate
            let elapsed = self.last_frame_instant.elapsed().as_micros() as u64;
            if elapsed < budget_us {
                let sleep_duration = Duration::from_micros(budget_us - elapsed);
                std::thread::sleep(sleep_duration);
            }
            self.frame_counter += 1;
            (self.frame_counter as i64 * (budget_us as i64 * 1000), true)
        } else {
            // In live mode, OpenXR FrameWaiter blocks until the display pacing slot
            let frame_state = context.wait_frame()?;
            context.begin_frame()?;
            self.frame_counter += 1;
            (
                frame_state.predicted_display_time.as_nanos(),
                frame_state.should_render,
            )
        };

        let wait_duration = wait_start.elapsed().as_micros() as u64;
        self.last_frame_instant = Instant::now();

        let metrics = FramePacingMetrics {
            wait_frame_duration_us: wait_duration,
            render_duration_us: 0,
            motion_to_photon_latency_us: budget_us,
            predicted_display_time: predicted_time,
            should_render,
        };

        self.last_pacing_metrics = metrics.clone();
        Ok(metrics)
    }

    /// Records completed frame render duration and updates motion-to-photon latency.
    pub fn record_render_duration(&mut self, duration_us: u64) {
        self.last_pacing_metrics.render_duration_us = duration_us;
        self.last_pacing_metrics.motion_to_photon_latency_us += duration_us;
    }

    /// Returns the most recent frame pacing metrics.
    pub fn last_metrics(&self) -> FramePacingMetrics {
        self.last_pacing_metrics.clone()
    }

    /// Total frames presented since loop start.
    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClientConfig;

    #[test]
    fn surface_timestamp_keeps_predicted_slot_when_it_has_enough_lead() {
        assert_eq!(
            surface_presentation_time_ns(20_000_000, 10_000_000, 11_111_111, 2_000_000),
            20_000_000
        );
    }

    #[test]
    fn surface_timestamp_advances_to_first_slot_with_safe_lead() {
        assert_eq!(
            surface_presentation_time_ns(20_000_000, 19_000_000, 11_111_111, 2_000_000),
            31_111_111
        );
        assert_eq!(
            surface_presentation_time_ns(20_000_000, 42_000_000, 11_111_111, 2_000_000),
            53_333_333
        );
    }

    #[test]
    fn surface_timestamp_accepts_slot_exactly_at_minimum_lead() {
        assert_eq!(
            surface_presentation_time_ns(20_000_000, 9_000_000, 11_000_000, 11_000_000),
            20_000_000
        );
    }

    #[test]
    fn test_frame_loop_engine_pacing() {
        let config = ClientConfig::default();
        let ctx = OpenXrContext::new_simulated(&config);
        let mut engine = FrameLoopEngine::new();

        let metrics = engine.wait_frame_pacing(&ctx).unwrap();
        assert_eq!(engine.frame_counter(), 1);
        assert!(metrics.predicted_display_time > 0);
        assert!(metrics.should_render);
    }
}
