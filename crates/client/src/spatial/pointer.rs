//! Converts spatial ray hits and button state into host pointer events.

use std::time::{Duration, Instant};

use linux_quest_protocol::{ElementState, InputEvent, MouseButton};
use serde::{Deserialize, Serialize};

use super::transform::ScreenTransform;
use super::{InteractionInput, InteractionSource};

const DEFAULT_CLICK_DEAD_ZONE_MS: u64 = 90;

/// Default pointer bindings. Controller trigger selects, thumbstick click is secondary click,
/// and the thumbstick scrolls while a ray is over a panel. Hand pinch selects.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PointerBindings {
    pub primary_button: MouseButton,
    pub secondary_button: MouseButton,
    pub scroll_scale: f32,
    pub hand_filter_alpha: f32,
    /// Hold duration before a short pinch is interpreted as a click and a longer hold as a drag.
    pub click_dead_zone_ms: u64,
}

impl Default for PointerBindings {
    fn default() -> Self {
        Self {
            primary_button: MouseButton::Left,
            secondary_button: MouseButton::Right,
            scroll_scale: 12.0,
            hand_filter_alpha: 0.45,
            click_dead_zone_ms: DEFAULT_CLICK_DEAD_ZONE_MS,
        }
    }
}

/// Per-frame state for one pointer source.
#[derive(Debug)]
pub struct PointerInput {
    bindings: PointerBindings,
    filtered_uv: Option<(f32, f32)>,
    held_button: Option<(MouseButton, Instant, bool)>,
    last_secondary: bool,
}

impl Default for PointerInput {
    fn default() -> Self {
        Self::new(PointerBindings::default())
    }
}

impl PointerInput {
    pub fn new(bindings: PointerBindings) -> Self {
        Self {
            bindings,
            filtered_uv: None,
            held_button: None,
            last_secondary: false,
        }
    }

    /// Produces an absolute pointer position and button/scroll events for the selected source.
    /// Coordinates are normalized to the uinput ABS range, so the host can cover its desktop.
    pub fn update(
        &mut self,
        input: &InteractionInput,
        secondary_down: bool,
        scroll: (f32, f32),
        screens: &[(u32, &ScreenTransform)],
        now: Instant,
    ) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let hit = input
            .aim_ray
            .as_ref()
            .and_then(|ray| ScreenTransform::find_closest_hit(ray, screens.iter().copied()));
        if let Some((display_id, hit)) = hit {
            let uv = if input.source.is_hand() {
                self.filter(hit.uv)
            } else {
                self.filtered_uv = None;
                hit.uv
            };
            out.push(InputEvent::MouseMoveAbsolute {
                x: (uv.0.clamp(0.0, 1.0) * u16::MAX as f32).round() as u32,
                y: (uv.1.clamp(0.0, 1.0) * u16::MAX as f32).round() as u32,
                display_id: display_id.min(u16::MAX as u32) as u16,
            });
            if scroll.0.abs() > 0.001 || scroll.1.abs() > 0.001 {
                out.push(InputEvent::MouseScrollHiRes {
                    delta_x: scroll.0 * self.bindings.scroll_scale,
                    delta_y: -scroll.1 * self.bindings.scroll_scale,
                });
            }
        } else {
            self.filtered_uv = None;
        }

        let secondary_down = secondary_down && hit.is_some();
        if secondary_down != self.last_secondary {
            out.push(InputEvent::MouseButton {
                button: self.bindings.secondary_button,
                state: if secondary_down {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                },
            });
            self.last_secondary = secondary_down;
        }

        let primary_down = input.is_selecting && hit.is_some();
        match (self.held_button, primary_down) {
            (None, true) => self.held_button = Some((self.bindings.primary_button, now, false)),
            (Some((button, started, sent)), true)
                if !sent
                    && now.duration_since(started)
                        >= Duration::from_millis(self.bindings.click_dead_zone_ms) =>
            {
                out.push(InputEvent::MouseButton {
                    button,
                    state: ElementState::Pressed,
                });
                self.held_button = Some((button, started, true));
            }
            (Some((button, _, true)), false) => {
                out.push(InputEvent::MouseButton {
                    button,
                    state: ElementState::Released,
                });
                self.held_button = None;
            }
            (Some((button, _, false)), false) => {
                out.push(InputEvent::MouseButton {
                    button,
                    state: ElementState::Pressed,
                });
                out.push(InputEvent::MouseButton {
                    button,
                    state: ElementState::Released,
                });
                self.held_button = None;
            }
            _ => {}
        }
        out
    }

    fn filter(&mut self, uv: (f32, f32)) -> (f32, f32) {
        let alpha = self.bindings.hand_filter_alpha.clamp(0.01, 1.0);
        let filtered = self.filtered_uv.map_or(uv, |prev| {
            (
                prev.0 + alpha * (uv.0 - prev.0),
                prev.1 + alpha * (uv.1 - prev.1),
            )
        });
        self.filtered_uv = Some(filtered);
        filtered
    }
}

impl InteractionInput {
    pub fn pointer_scroll(&self) -> (f32, f32) {
        if matches!(self.source, InteractionSource::Controller(_)) {
            self.thumbstick
        } else {
            (0.0, 0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::ray::Ray;
    use crate::spatial::InteractionSource;
    use crate::spatial::{Handedness, Quat, ScreenPose, Vec3};

    fn input(source: InteractionSource, selecting: bool) -> InteractionInput {
        InteractionInput {
            source,
            aim_ray: Some(Ray::new(
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(0.0, 0.0, -1.0),
            )),
            grab_pose: Some(ScreenPose::new(Vec3::ZERO, Quat::IDENTITY)),
            is_grabbing: false,
            is_selecting: selecting,
            thumbstick: (0.0, 0.0),
        }
    }

    #[test]
    fn maps_ray_hit_to_absolute_coordinates_and_panel_id() {
        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::default();
        let events = pointer.update(
            &input(InteractionSource::Controller(Handedness::Right), false),
            false,
            (0.0, 0.0),
            &[(7, &screen)],
            Instant::now(),
        );
        let uv = screen
            .intersect_ray(
                &input(InteractionSource::Controller(Handedness::Right), false)
                    .aim_ray
                    .unwrap(),
            )
            .unwrap()
            .uv;
        assert_eq!(
            events,
            vec![InputEvent::MouseMoveAbsolute {
                x: (uv.0 * u16::MAX as f32).round() as u32,
                y: (uv.1 * u16::MAX as f32).round() as u32,
                display_id: 7
            }]
        );
    }

    #[test]
    fn short_hand_pinch_is_a_click_and_hold_becomes_drag() {
        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::default();
        let start = Instant::now();
        let hand = InteractionSource::Hand(Handedness::Right);
        let down = pointer.update(
            &input(hand, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start,
        );
        assert_eq!(down.len(), 1); // position only; click is deferred
        let up = pointer.update(
            &input(hand, false),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(40),
        );
        assert!(up.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Pressed,
        }));
        assert!(up.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Released,
        }));
        pointer.update(
            &input(hand, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(100),
        );
        let drag = pointer.update(
            &input(hand, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(200),
        );
        assert!(drag.iter().any(|event| matches!(
            event,
            InputEvent::MouseButton {
                state: ElementState::Pressed,
                ..
            }
        )));
    }

    #[test]
    fn controller_trigger_clicks_and_holds_drag() {
        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::default();
        let start = Instant::now();
        let controller = InteractionSource::Controller(Handedness::Right);
        pointer.update(
            &input(controller, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start,
        );
        let click = pointer.update(
            &input(controller, false),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(40),
        );
        assert!(click.iter().any(|event| matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed
            }
        )));
        assert!(click.iter().any(|event| matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Released
            }
        )));

        pointer.update(
            &input(controller, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(100),
        );
        let drag = pointer.update(
            &input(controller, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(200),
        );
        assert!(drag.iter().any(|event| matches!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed
            }
        )));
    }

    #[test]
    fn secondary_button_scroll_and_release_are_emitted() {
        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::default();
        let controller = InteractionSource::Controller(Handedness::Right);
        let start = Instant::now();
        let down = pointer.update(
            &input(controller, false),
            true,
            (0.5, -0.25),
            &[(4, &screen)],
            start,
        );
        assert!(down.contains(&InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ElementState::Pressed,
        }));
        assert!(down.contains(&InputEvent::MouseScrollHiRes {
            delta_x: 6.0,
            delta_y: 3.0,
        }));
        let up = pointer.update(
            &input(controller, false),
            false,
            (0.0, 0.0),
            &[(4, &screen)],
            start + Duration::from_millis(16),
        );
        assert!(up.contains(&InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ElementState::Released,
        }));
    }

    #[test]
    fn pointer_buttons_are_released_when_ray_leaves_panel() {
        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::default();
        let controller = InteractionSource::Controller(Handedness::Right);
        let start = Instant::now();
        pointer.update(
            &input(controller, true),
            true,
            (0.0, 0.0),
            &[(0, &screen)],
            start,
        );
        let mut off_panel = input(controller, true);
        off_panel.aim_ray = None;
        let events = pointer.update(
            &off_panel,
            true,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(1),
        );
        assert!(events.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Pressed,
        }));
        assert!(events.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Released,
        }));
        assert!(events.contains(&InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ElementState::Released,
        }));
    }

    #[test]
    fn bindings_are_serializable_and_control_button_and_filter_settings() {
        let bindings = PointerBindings {
            primary_button: MouseButton::Middle,
            secondary_button: MouseButton::Extra,
            scroll_scale: 4.0,
            hand_filter_alpha: 0.8,
            click_dead_zone_ms: 25,
        };
        let json = serde_json::to_string(&bindings).unwrap();
        let decoded: PointerBindings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.primary_button, MouseButton::Middle);
        assert_eq!(decoded.secondary_button, MouseButton::Extra);
        assert_eq!(decoded.scroll_scale, 4.0);
        assert_eq!(decoded.hand_filter_alpha, 0.8);
        assert_eq!(decoded.click_dead_zone_ms, 25);

        let screen = ScreenTransform::for_display(0);
        let mut pointer = PointerInput::new(decoded);
        let hand = InteractionSource::Hand(Handedness::Right);
        let start = Instant::now();
        pointer.update(
            &input(hand, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start,
        );
        let drag = pointer.update(
            &input(hand, true),
            false,
            (0.0, 0.0),
            &[(0, &screen)],
            start + Duration::from_millis(30),
        );
        assert!(drag.contains(&InputEvent::MouseButton {
            button: MouseButton::Middle,
            state: ElementState::Pressed,
        }));
    }
}
