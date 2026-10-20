//! Per-monitor stream admission and lifecycle state.
//!
//! This is deliberately independent of OpenXR so network/control code can validate an entire
//! monitor update before allocating decoder and composition resources.

use std::collections::BTreeMap;

use linux_quest_protocol::DisplayInfo;
use thiserror::Error;

/// Hardware decoder limits advertised by the client implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderBudget {
    pub max_sessions: usize,
    /// Maximum aggregate decoded pixels per second.
    pub max_pixels_per_second: u64,
}

impl DecoderBudget {
    /// Conservative Quest profile: up to three sessions and roughly 1 Gpixel/s aggregate.
    pub const fn quest_default() -> Self {
        Self {
            max_sessions: 3,
            max_pixels_per_second: 1_000_000_000,
        }
    }

    pub fn pixel_rate(self, displays: &[DisplayInfo]) -> u64 {
        displays.iter().fold(0u64, |sum, display| {
            sum.saturating_add(
                u64::from(display.width)
                    .saturating_mul(u64::from(display.height))
                    .saturating_mul(u64::from(display.refresh_rate)),
            )
        })
    }

    pub fn check(self, displays: &[DisplayInfo]) -> Result<(), DecoderBudgetError> {
        let pixel_rate = self.pixel_rate(displays);
        if displays.len() > self.max_sessions {
            return Err(DecoderBudgetError::SessionCount {
                requested: displays.len(),
                maximum: self.max_sessions,
            });
        }
        if pixel_rate > self.max_pixels_per_second {
            return Err(DecoderBudgetError::PixelRate {
                requested: pixel_rate,
                maximum: self.max_pixels_per_second,
            });
        }
        Ok(())
    }
}

impl Default for DecoderBudget {
    fn default() -> Self {
        Self::quest_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecoderBudgetError {
    #[error("monitor request needs {requested} decoder sessions; Quest limit is {maximum}")]
    SessionCount { requested: usize, maximum: usize },
    #[error(
        "monitor request needs {requested} decoded pixels/s; Quest budget is {maximum} pixels/s"
    )]
    PixelRate { requested: u64, maximum: u64 },
    #[error("monitor list contains duplicate display id {0}")]
    DuplicateDisplay(u16),
}

/// Result of atomically applying a monitor list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamChanges {
    pub added: Vec<DisplayInfo>,
    pub removed: Vec<u16>,
    pub updated: Vec<DisplayInfo>,
}

/// Active display descriptors indexed by packet `stream_id` / display id.
#[derive(Debug, Clone)]
pub struct ClientStreamRegistry {
    budget: DecoderBudget,
    active: BTreeMap<u16, DisplayInfo>,
}

impl ClientStreamRegistry {
    pub fn new(budget: DecoderBudget) -> Self {
        Self {
            budget,
            active: BTreeMap::new(),
        }
    }

    /// Validate first, then replace the full active set. A rejected update leaves state intact.
    pub fn apply(
        &mut self,
        displays: Vec<DisplayInfo>,
    ) -> Result<StreamChanges, DecoderBudgetError> {
        let mut candidate = BTreeMap::new();
        for display in displays {
            if candidate
                .insert(display.display_id, display.clone())
                .is_some()
            {
                return Err(DecoderBudgetError::DuplicateDisplay(display.display_id));
            }
        }
        let candidate_values: Vec<_> = candidate.values().cloned().collect();
        self.budget.check(&candidate_values)?;

        let mut changes = StreamChanges::default();
        for (id, old) in &self.active {
            match candidate.get(id) {
                None => changes.removed.push(*id),
                Some(new) if new != old => changes.updated.push(new.clone()),
                _ => {}
            }
        }
        for (id, new) in &candidate {
            if !self.active.contains_key(id) {
                changes.added.push(new.clone());
            }
        }
        self.active = candidate;
        Ok(changes)
    }

    pub fn get(&self, stream_id: u16) -> Option<&DisplayInfo> {
        self.active.get(&stream_id)
    }

    pub fn active(&self) -> impl Iterator<Item = (&u16, &DisplayInfo)> {
        self.active.iter()
    }

    pub fn budget(&self) -> DecoderBudget {
        self.budget
    }
}

impl Default for ClientStreamRegistry {
    fn default() -> Self {
        Self::new(DecoderBudget::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u16, width: u32, height: u32, refresh_rate: u32) -> DisplayInfo {
        DisplayInfo {
            display_id: id,
            name: format!("Monitor {id}"),
            width,
            height,
            refresh_rate,
            dpi: 96,
            is_virtual: true,
            x: i32::from(id) * width as i32,
            y: 0,
        }
    }

    #[test]
    fn monitor_updates_report_add_remove_and_update() {
        let mut registry = ClientStreamRegistry::default();
        let first = display(1, 1920, 1080, 90);
        let second = display(2, 1920, 1080, 90);
        assert_eq!(
            registry.apply(vec![first.clone()]).unwrap().added,
            vec![first.clone()]
        );
        let mut resized = second.clone();
        resized.x = 2000;
        let changes = registry
            .apply(vec![first.clone(), resized.clone()])
            .unwrap();
        assert_eq!(changes.added, vec![resized.clone()]);
        let changes = registry.apply(vec![resized.clone()]).unwrap();
        assert_eq!(changes.removed, vec![1]);
        let mut moved = resized.clone();
        moved.y = 100;
        assert_eq!(
            registry.apply(vec![moved.clone()]).unwrap().updated,
            vec![moved]
        );
    }

    #[test]
    fn over_budget_updates_are_clear_and_atomic() {
        let mut registry = ClientStreamRegistry::new(DecoderBudget {
            max_sessions: 2,
            max_pixels_per_second: 500_000_000,
        });
        registry.apply(vec![display(1, 1920, 1080, 60)]).unwrap();
        let err = registry
            .apply(vec![display(1, 3840, 2160, 120)])
            .unwrap_err();
        assert!(err.to_string().contains("Quest budget"));
        assert_eq!(registry.active().count(), 1);
        let err = registry
            .apply(vec![
                display(1, 1920, 1080, 60),
                display(2, 1920, 1080, 60),
                display(3, 1920, 1080, 60),
            ])
            .unwrap_err();
        assert!(err.to_string().contains("3 decoder sessions"));
    }
}
