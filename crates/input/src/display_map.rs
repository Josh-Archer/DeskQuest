//! Maps per-display pointer positions onto the host desktop.
//!
//! The client sends `MouseMoveAbsolute` as a position inside one display (0..=65535 on each
//! axis) tagged with that display's id. The injector's absolute device spans the whole desktop,
//! so a position on a second monitor has to be rebased onto that monitor's rectangle first.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use linux_quest_protocol::InputEvent;

use crate::{InputError, InputInjector};

/// Full scale of the absolute pointer axes (matches `UinputConfig::abs_max_*`).
const ABS_MAX: f64 = 65535.0;

/// A display's rectangle in host desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Display rectangles keyed by display id, plus the desktop bounds they live in.
#[derive(Debug, Clone, Default)]
pub struct DisplayMap {
    regions: HashMap<u16, DisplayRegion>,
    bounds: Option<DisplayRegion>,
}

pub type SharedDisplayMap = Arc<RwLock<DisplayMap>>;

impl DisplayMap {
    /// Builds a map whose desktop bounds are the union of `regions`.
    pub fn from_regions(regions: impl IntoIterator<Item = (u16, DisplayRegion)>) -> Self {
        let regions: HashMap<u16, DisplayRegion> = regions
            .into_iter()
            .filter(|(_, r)| r.width > 0 && r.height > 0)
            .collect();
        let bounds = regions.values().copied().reduce(|a, b| {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x as i64 + a.width as i64).max(b.x as i64 + b.width as i64);
            let y1 = (a.y as i64 + a.height as i64).max(b.y as i64 + b.height as i64);
            DisplayRegion {
                x: x0,
                y: y0,
                width: (x1 - x0 as i64) as u32,
                height: (y1 - y0 as i64) as u32,
            }
        });
        Self { regions, bounds }
    }

    /// Rebases a within-display position onto the desktop's absolute range.
    ///
    /// Returns `None` when `display_id` is unknown. Display 0 is the primary desktop stream and
    /// is already desktop-relative, so callers pass it through unchanged.
    pub fn to_desktop(&self, display_id: u16, x: u32, y: u32) -> Option<(u32, u32)> {
        let region = self.regions.get(&display_id)?;
        let bounds = self.bounds?;
        let fx = f64::from(x.min(ABS_MAX as u32)) / ABS_MAX;
        let fy = f64::from(y.min(ABS_MAX as u32)) / ABS_MAX;
        let px = f64::from(region.x) - f64::from(bounds.x) + fx * f64::from(region.width);
        let py = f64::from(region.y) - f64::from(bounds.y) + fy * f64::from(region.height);
        let ax = (px / f64::from(bounds.width) * ABS_MAX)
            .round()
            .clamp(0.0, ABS_MAX);
        let ay = (py / f64::from(bounds.height) * ABS_MAX)
            .round()
            .clamp(0.0, ABS_MAX);
        Some((ax as u32, ay as u32))
    }
}

/// Wraps an injector and rewrites absolute moves for non-primary displays.
pub struct DisplayMappedInjector<I> {
    inner: I,
    map: SharedDisplayMap,
}

impl<I> DisplayMappedInjector<I> {
    pub fn new(inner: I, map: SharedDisplayMap) -> Self {
        Self { inner, map }
    }
}

impl<I: InputInjector> InputInjector for DisplayMappedInjector<I> {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMoveAbsolute { x, y, display_id } if display_id != 0 => {
                let mapped = self
                    .map
                    .read()
                    .ok()
                    .and_then(|m| m.to_desktop(display_id, x, y));
                match mapped {
                    Some((x, y)) => {
                        self.inner
                            .inject_event(InputEvent::MouseMoveAbsolute { x, y, display_id })
                    }
                    // A guess would put the cursor on the wrong monitor; leave it where it is.
                    None => {
                        tracing::debug!(display_id, "Dropping pointer move for unknown display");
                        Ok(())
                    }
                }
            }
            other => self.inner.inject_event(other),
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        self.inner.release_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockInputInjector;

    fn region(x: i32, y: i32, width: u32, height: u32) -> DisplayRegion {
        DisplayRegion {
            x,
            y,
            width,
            height,
        }
    }

    fn two_side_by_side() -> DisplayMap {
        DisplayMap::from_regions([
            (1, region(0, 0, 1920, 1080)),
            (2, region(1920, 0, 1920, 1080)),
        ])
    }

    #[test]
    fn second_monitor_occupies_right_half_of_the_range() {
        let map = two_side_by_side();
        assert_eq!(map.to_desktop(2, 0, 0), Some((32768, 0)));
        assert_eq!(map.to_desktop(2, 65535, 65535), Some((65535, 65535)));
        assert_eq!(map.to_desktop(1, 65535, 0), Some((32768, 0)));
        // Centre of the second monitor is three quarters across the desktop.
        let (x, y) = map.to_desktop(2, 32768, 32768).unwrap();
        assert!((i64::from(x) - 49152).abs() <= 1, "x = {x}");
        assert!((i64::from(y) - 32768).abs() <= 1, "y = {y}");
    }

    #[test]
    fn stacked_monitors_and_negative_origins() {
        let map = DisplayMap::from_regions([
            (1, region(-1920, 0, 1920, 1080)),
            (2, region(0, 0, 1920, 1080)),
        ]);
        assert_eq!(map.to_desktop(1, 0, 0), Some((0, 0)));
        assert_eq!(map.to_desktop(2, 0, 0), Some((32768, 0)));
    }

    #[test]
    fn unknown_display_has_no_mapping() {
        assert_eq!(two_side_by_side().to_desktop(9, 1, 1), None);
        assert_eq!(DisplayMap::default().to_desktop(1, 1, 1), None);
    }

    #[test]
    fn injector_rewrites_known_displays_and_passes_everything_else() {
        let map: SharedDisplayMap = Arc::new(RwLock::new(two_side_by_side()));
        let mut inj = DisplayMappedInjector::new(MockInputInjector::new(), map);

        inj.inject_event(InputEvent::MouseMoveAbsolute {
            x: 0,
            y: 100,
            display_id: 2,
        })
        .unwrap();
        // Primary stream is desktop-relative already.
        inj.inject_event(InputEvent::MouseMoveAbsolute {
            x: 5,
            y: 6,
            display_id: 0,
        })
        .unwrap();
        // Unknown display: dropped.
        inj.inject_event(InputEvent::MouseMoveAbsolute {
            x: 7,
            y: 8,
            display_id: 7,
        })
        .unwrap();

        let events = &inj.inner.received_events;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            InputEvent::MouseMoveAbsolute {
                x: 32768,
                y: 100,
                display_id: 2
            }
        );
        assert_eq!(
            events[1],
            InputEvent::MouseMoveAbsolute {
                x: 5,
                y: 6,
                display_id: 0
            }
        );
    }
}
