//! Mouse motion and scroll coalescing per frame at up to 120Hz.

use linux_quest_protocol::InputEvent;

/// Coalesces high-frequency mouse movements and scrolling into per-frame protocol events.
#[derive(Debug)]
pub struct MouseCoalescer {
    accum_dx: f32,
    accum_dy: f32,
    remainder_dx: f32,
    remainder_dy: f32,
    latest_absolute: Option<(u32, u32, u16)>,
    scroll_dx: f32,
    scroll_dy: f32,
    is_high_res_scroll: bool,
    last_flush_us: u64,
    min_interval_us: u64,
}

impl Default for MouseCoalescer {
    fn default() -> Self {
        Self::new(120) // Default to 120Hz for Quest 3
    }
}

impl MouseCoalescer {
    pub fn new(max_frequency_hz: u32) -> Self {
        let max_hz = max_frequency_hz.clamp(1, 240);
        let min_interval_us = 1_000_000 / max_hz as u64;
        Self {
            accum_dx: 0.0,
            accum_dy: 0.0,
            remainder_dx: 0.0,
            remainder_dy: 0.0,
            latest_absolute: None,
            scroll_dx: 0.0,
            scroll_dy: 0.0,
            is_high_res_scroll: false,
            last_flush_us: 0,
            min_interval_us,
        }
    }

    pub fn min_interval_us(&self) -> u64 {
        self.min_interval_us
    }

    pub fn accumulate_relative(&mut self, dx: f32, dy: f32) {
        self.accum_dx += dx;
        self.accum_dy += dy;
    }

    pub fn accumulate_absolute(&mut self, x: u32, y: u32, display_id: u16) {
        self.latest_absolute = Some((x, y, display_id));
    }

    pub fn accumulate_scroll(&mut self, delta_x: f32, delta_y: f32, is_high_res: bool) {
        self.scroll_dx += delta_x;
        self.scroll_dy += delta_y;
        if is_high_res {
            self.is_high_res_scroll = true;
        }
    }

    /// Flush coalesced movements into discrete `InputEvent`s if interval elapsed or forced.
    pub fn flush(&mut self, current_time_us: u64, force: bool) -> Vec<InputEvent> {
        if !force && current_time_us.saturating_sub(self.last_flush_us) < self.min_interval_us {
            return Vec::new();
        }

        let mut events = Vec::new();

        // 1. Process relative mouse motion with sub-pixel remainder preservation
        let total_x = self.accum_dx + self.remainder_dx;
        let total_y = self.accum_dy + self.remainder_dy;
        let int_x = total_x.trunc() as i32;
        let int_y = total_y.trunc() as i32;

        if int_x != 0 || int_y != 0 {
            events.push(InputEvent::MouseMoveRelative {
                dx: int_x,
                dy: int_y,
            });
            self.remainder_dx = total_x.fract();
            self.remainder_dy = total_y.fract();
        } else {
            self.remainder_dx = total_x;
            self.remainder_dy = total_y;
        }
        self.accum_dx = 0.0;
        self.accum_dy = 0.0;

        // 2. Process absolute mouse position
        if let Some((x, y, display_id)) = self.latest_absolute.take() {
            events.push(InputEvent::MouseMoveAbsolute { x, y, display_id });
        }

        // 3. Process mouse scroll
        if self.scroll_dx.abs() > 0.001 || self.scroll_dy.abs() > 0.001 {
            if self.is_high_res_scroll {
                events.push(InputEvent::MouseScrollHiRes {
                    delta_x: self.scroll_dx,
                    delta_y: self.scroll_dy,
                });
            } else {
                events.push(InputEvent::MouseScroll {
                    delta_x: self.scroll_dx,
                    delta_y: self.scroll_dy,
                });
            }
            self.scroll_dx = 0.0;
            self.scroll_dy = 0.0;
            self.is_high_res_scroll = false;
        }

        self.last_flush_us = current_time_us;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relative_motion_coalescing() {
        let mut coalescer = MouseCoalescer::new(120);

        coalescer.accumulate_relative(10.0, 5.0);
        coalescer.accumulate_relative(5.0, -2.0);
        coalescer.accumulate_relative(-3.0, 1.0);

        let events = coalescer.flush(10_000, true);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], InputEvent::MouseMoveRelative { dx: 12, dy: 4 });

        // Subsequent flush without new movement yields no events
        let next_events = coalescer.flush(20_000, true);
        assert!(next_events.is_empty());
    }

    #[test]
    fn test_sub_pixel_remainder_accumulation() {
        let mut coalescer = MouseCoalescer::new(120);

        // Sub-pixel moves: 0.4 + 0.4 = 0.8 (< 1.0 -> 0)
        coalescer.accumulate_relative(0.4, 0.4);
        coalescer.accumulate_relative(0.4, 0.4);
        let ev1 = coalescer.flush(10_000, true);
        assert!(ev1.is_empty());

        // Next move: 0.3 -> 0.8 + 0.3 = 1.1 (>= 1.0 -> 1 with 0.1 remainder)
        coalescer.accumulate_relative(0.3, 0.3);
        let ev2 = coalescer.flush(20_000, true);
        assert_eq!(ev2.len(), 1);
        assert_eq!(ev2[0], InputEvent::MouseMoveRelative { dx: 1, dy: 1 });
    }

    #[test]
    fn test_absolute_motion_coalescing_keeps_latest() {
        let mut coalescer = MouseCoalescer::new(120);

        coalescer.accumulate_absolute(100, 200, 0);
        coalescer.accumulate_absolute(150, 250, 0);
        coalescer.accumulate_absolute(300, 400, 1);

        let events = coalescer.flush(10_000, true);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            InputEvent::MouseMoveAbsolute {
                x: 300,
                y: 400,
                display_id: 1,
            }
        );
    }

    #[test]
    fn test_scroll_coalescing() {
        let mut coalescer = MouseCoalescer::new(120);

        coalescer.accumulate_scroll(10.0, 20.0, true);
        coalescer.accumulate_scroll(5.0, -5.0, true);

        let events = coalescer.flush(10_000, true);
        assert_eq!(events.len(), 1);
        match events[0] {
            InputEvent::MouseScrollHiRes { delta_x, delta_y } => {
                assert!((delta_x - 15.0).abs() < 0.001);
                assert!((delta_y - 15.0).abs() < 0.001);
            }
            ref other => panic!("Expected MouseScrollHiRes, got {:?}", other),
        }
    }

    #[test]
    fn test_frequency_rate_limiting() {
        let mut coalescer = MouseCoalescer::new(120); // ~8333 us interval

        coalescer.accumulate_relative(5.0, 5.0);
        // Flush at t = 10,000 (forced initial)
        let ev1 = coalescer.flush(10_000, true);
        assert_eq!(ev1.len(), 1);

        // Attempt flush at t = 12,000 (only 2,000 us elapsed, < 8333) without force
        coalescer.accumulate_relative(5.0, 5.0);
        let ev2 = coalescer.flush(12_000, false);
        assert!(ev2.is_empty());

        // At t = 19,000 (9,000 us elapsed, > 8333) without force -> successfully flushes
        let ev3 = coalescer.flush(19_000, false);
        assert_eq!(ev3.len(), 1);
        assert_eq!(ev3[0], InputEvent::MouseMoveRelative { dx: 5, dy: 5 });
    }
}
