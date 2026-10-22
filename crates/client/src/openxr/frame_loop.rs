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
