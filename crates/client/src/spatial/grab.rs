//! 6DoF screen grab state machine coordinating hover, rigid drag, push/pull, tilt/curve, and layout persistence on release.

use crate::config::DisplayMode;
use crate::spatial::interaction::{InteractionInput, InteractionSource};
use crate::spatial::limits::{clamp_curve_angle, clamp_distance, clamp_tilt};
use crate::spatial::pose::ScreenPose;
use crate::spatial::ray::RayHit;
use crate::spatial::transform::ScreenTransform;

/// Active grab manipulation session tracking initial poses and cumulative offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct GrabSession {
    /// Target display ID being manipulated.
    pub display_id: u32,

    /// Source that initiated this grab.
    pub source: InteractionSource,

    /// 6DoF controller/hand pose when grab started.
    pub initial_controller_pose: ScreenPose,

    /// 6DoF screen pose when grab started.
    pub initial_screen_pose: ScreenPose,

    /// Full screen transform model when grab started.
    pub initial_screen_transform: ScreenTransform,

    /// Current manipulated screen transform.
    pub current_screen_transform: ScreenTransform,
}

/// Lifecycle state of screen interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionState {
    /// Neither hand or controller is hovering over or manipulating a screen.
    Idle,

    /// Pointing ray intersects a screen surface, ready to grab.
    Hovering {
        display_id: u32,
        hit: RayHit,
        source: InteractionSource,
    },

    /// Actively grabbing and dragging a screen in 6DoF space.
    Grabbing(GrabSession),
}

/// Events emitted by the interaction state machine during frame updates.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionEvent {
    /// Pointing ray just started hovering over a screen.
    HoverStarted {
        display_id: u32,
        hit: RayHit,
        source: InteractionSource,
    },

    /// Pointing ray moved across the hovered screen surface.
    HoverUpdated {
        display_id: u32,
        hit: RayHit,
        source: InteractionSource,
    },

    /// Pointing ray left the hovered screen surface.
    HoverEnded {
        display_id: u32,
        source: InteractionSource,
    },

    /// Screen grab manipulation started (grip pressed or pinch gesture).
    GrabStarted {
        display_id: u32,
        source: InteractionSource,
    },

    /// Screen was dragged or adjusted, providing the new 6DoF transform.
    GrabUpdated {
        display_id: u32,
        source: InteractionSource,
        transform: ScreenTransform,
    },

    /// Grab released. Triggers persistence of the final spatial layout to disk.
    GrabEnded {
        display_id: u32,
        source: InteractionSource,
        final_transform: ScreenTransform,
    },
}

/// 6DoF screen manipulation state machine.
#[derive(Debug, Clone)]
pub struct GrabStateMachine {
    state: InteractionState,
    push_pull_speed: f32,
    tilt_speed: f32,
    curve_speed: f32,
}

impl Default for GrabStateMachine {
    fn default() -> Self {
        Self {
            state: InteractionState::Idle,
            push_pull_speed: 1.2, // 1.2 meters per second at full deflection
            tilt_speed: 1.0,      // ~57 degrees per second
            curve_speed: 0.8,     // central angle radians per second
        }
    }
}

impl GrabStateMachine {
    /// Creates a new grab state machine in the Idle state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current lifecycle state of the interaction state machine.
    pub fn state(&self) -> &InteractionState {
        &self.state
    }

    /// Returns true if currently in an active screen grab session.
    pub fn is_grabbing(&self) -> bool {
        matches!(self.state, InteractionState::Grabbing(_))
    }

    /// Returns true if currently hovering over a screen.
    pub fn is_hovering(&self) -> bool {
        matches!(self.state, InteractionState::Hovering { .. })
    }

    /// Returns the active grab session if currently grabbing.
    pub fn grab_session(&self) -> Option<&GrabSession> {
        match &self.state {
            InteractionState::Grabbing(session) => Some(session),
            _ => None,
        }
    }

    /// Processes a single frame of input against the active screens, updating state and emitting events.
    pub fn update<'a>(
        &mut self,
        input: &InteractionInput,
        screens: impl IntoIterator<Item = (u32, &'a ScreenTransform)> + Clone,
        dt_seconds: f32,
    ) -> Option<InteractionEvent> {
        let screens_vec: Vec<(u32, &'a ScreenTransform)> = screens.into_iter().collect();

        match &mut self.state {
            InteractionState::Idle => {
                if input.is_grabbing {
                    // Cannot grab out of thin air: must point and hover first
                    return None;
                }

                if let Some(ray) = &input.aim_ray {
                    if let Some((hit_id, hit)) =
                        ScreenTransform::find_closest_hit(ray, screens_vec.iter().copied())
                    {
                        self.state = InteractionState::Hovering {
                            display_id: hit_id,
                            hit,
                            source: input.source,
                        };
                        return Some(InteractionEvent::HoverStarted {
                            display_id: hit_id,
                            hit,
                            source: input.source,
                        });
                    }
                }
                None
            }

            InteractionState::Hovering {
                display_id,
                hit: _,
                source,
            } => {
                let current_display_id = *display_id;
                let current_source = *source;

                // Grab initiation: user squeezed grip or pinched while hovering
                if input.is_grabbing && input.source == current_source {
                    let target_screen = screens_vec
                        .iter()
                        .find(|(id, _)| *id == current_display_id)
                        .map(|(_, tf)| (*tf).clone())
                        .unwrap_or_else(|| ScreenTransform::for_display(current_display_id));

                    let ctrl_pose = input.grab_pose.unwrap_or(ScreenPose::IDENTITY);
                    let screen_pose = target_screen.pose();

                    let session = GrabSession {
                        display_id: current_display_id,
                        source: input.source,
                        initial_controller_pose: ctrl_pose,
                        initial_screen_pose: screen_pose,
                        initial_screen_transform: target_screen.clone(),
                        current_screen_transform: target_screen,
                    };

                    self.state = InteractionState::Grabbing(session);
                    return Some(InteractionEvent::GrabStarted {
                        display_id: current_display_id,
                        source: input.source,
                    });
                }

                // If input source matches, update hover ray hit
                if input.source == current_source {
                    if let Some(ray) = &input.aim_ray {
                        if let Some((new_id, new_hit)) =
                            ScreenTransform::find_closest_hit(ray, screens_vec.iter().copied())
                        {
                            if new_id == current_display_id {
                                self.state = InteractionState::Hovering {
                                    display_id: new_id,
                                    hit: new_hit,
                                    source: current_source,
                                };
                                return Some(InteractionEvent::HoverUpdated {
                                    display_id: new_id,
                                    hit: new_hit,
                                    source: current_source,
                                });
                            } else {
                                // Moved to a different screen
                                self.state = InteractionState::Hovering {
                                    display_id: new_id,
                                    hit: new_hit,
                                    source: current_source,
                                };
                                return Some(InteractionEvent::HoverStarted {
                                    display_id: new_id,
                                    hit: new_hit,
                                    source: current_source,
                                });
                            }
                        }
                    }

                    // Ray no longer hits any screen
                    self.state = InteractionState::Idle;
                    return Some(InteractionEvent::HoverEnded {
                        display_id: current_display_id,
                        source: current_source,
                    });
                }

                None
            }

            InteractionState::Grabbing(session) => {
                if input.source != session.source {
                    return None;
                }

                // Grab released: transition out and emit GrabEnded to trigger layout persistence
                if !input.is_grabbing {
                    session.current_screen_transform.clamp();
                    let final_transform = session.current_screen_transform.clone();
                    let display_id = session.display_id;
                    let source = session.source;

                    // Return to hover if ray still hits the screen, else Idle
                    if let Some(ray) = &input.aim_ray {
                        if let Some(ray_hit) = final_transform.intersect_ray(ray) {
                            self.state = InteractionState::Hovering {
                                display_id,
                                hit: ray_hit,
                                source,
                            };
                        } else {
                            self.state = InteractionState::Idle;
                        }
                    } else {
                        self.state = InteractionState::Idle;
                    }

                    return Some(InteractionEvent::GrabEnded {
                        display_id,
                        source,
                        final_transform,
                    });
                }

                // Drag update: calculate 6DoF rigid-body transform following controller pose
                if let Some(current_ctrl_pose) = input.grab_pose {
                    let initial_ctrl = session.initial_controller_pose;
                    let initial_screen = session.initial_screen_pose;

                    // 1. Controller rotation delta: dQ = Q_c * (Q_c0)^-1
                    let dq = (current_ctrl_pose.orientation * initial_ctrl.orientation.conjugate())
                        .normalize();

                    // 2. Initial relative offset from controller to screen: V0 = P_s0 - P_c0
                    let v0 = initial_screen.position - initial_ctrl.position;

                    // 3. Rotated offset: V = dQ * V0
                    let v_rotated = dq.rotate_vec(v0);

                    // 4. New screen position: P_s = P_c + V
                    let new_screen_pos = current_ctrl_pose.position + v_rotated;

                    // 5. New screen orientation: Q_s = dQ * Q_s0
                    let new_screen_orient = (dq * initial_screen.orientation).normalize();

                    let mut transform = session.current_screen_transform.clone();
                    transform.position = new_screen_pos;
                    transform.orientation = new_screen_orient;

                    // Synchronize spherical distance, yaw, tilt from new 6DoF pose
                    let horiz_dist = (transform.position.x * transform.position.x
                        + transform.position.z * transform.position.z)
                        .sqrt();
                    transform.distance = clamp_distance(horiz_dist);

                    let (pitch, yaw, roll) = transform.orientation.to_euler();
                    transform.tilt = clamp_tilt(pitch);
                    transform.yaw = yaw;
                    transform.roll = roll;

                    // 6. Thumbstick Push/Pull distance adjustment (thumbstick Y)
                    let ts_y = input.thumbstick.1;
                    if ts_y.abs() > 0.05 {
                        let delta_dist = ts_y * self.push_pull_speed * dt_seconds;
                        transform.push_pull(delta_dist);
                    }

                    // 7. Thumbstick Tilt and Curve adjustment (thumbstick X)
                    let ts_x = input.thumbstick.0;
                    if ts_x.abs() > 0.05 {
                        match transform.display_mode {
                            DisplayMode::CurvedCylinder => {
                                // Adjust curvature angle
                                let delta_angle = ts_x * self.curve_speed * dt_seconds;
                                transform.curve_angle =
                                    clamp_curve_angle(transform.curve_angle + delta_angle);
                            }
                            DisplayMode::FlatQuad => {
                                // Adjust tilt
                                let delta_tilt = ts_x * self.tilt_speed * dt_seconds;
                                transform.set_tilt(transform.tilt + delta_tilt);
                            }
                        }
                    }

                    transform.clamp();
                    session.current_screen_transform = transform.clone();

                    return Some(InteractionEvent::GrabUpdated {
                        display_id: session.display_id,
                        source: session.source,
                        transform,
                    });
                }

                None
            }
        }
    }

    /// Directly applies push/pull distance adjustment to the active grab session.
    pub fn push_pull(&mut self, delta_distance: f32) -> Option<&ScreenTransform> {
        if let InteractionState::Grabbing(session) = &mut self.state {
            session.current_screen_transform.push_pull(delta_distance);
            Some(&session.current_screen_transform)
        } else {
            None
        }
    }

    /// Directly applies tilt adjustment to the active grab session.
    pub fn adjust_tilt(&mut self, delta_tilt_rad: f32) -> Option<&ScreenTransform> {
        if let InteractionState::Grabbing(session) = &mut self.state {
            let new_tilt = session.current_screen_transform.tilt + delta_tilt_rad;
            session.current_screen_transform.set_tilt(new_tilt);
            Some(&session.current_screen_transform)
        } else {
            None
        }
    }

    /// Directly applies curvature radius and arc angle adjustment to the active grab session.
    pub fn adjust_curve(
        &mut self,
        delta_radius: f32,
        delta_angle_rad: f32,
    ) -> Option<&ScreenTransform> {
        if let InteractionState::Grabbing(session) = &mut self.state {
            let new_r = session.current_screen_transform.curve_radius + delta_radius;
            let new_theta = session.current_screen_transform.curve_angle + delta_angle_rad;
            session.current_screen_transform.set_curve(new_r, new_theta);
            Some(&session.current_screen_transform)
        } else {
            None
        }
    }

    /// Forcefully ends the current grab manipulation, returning `GrabEnded` event if active.
    pub fn release(&mut self) -> Option<InteractionEvent> {
        if let InteractionState::Grabbing(mut session) =
            std::mem::replace(&mut self.state, InteractionState::Idle)
        {
            session.current_screen_transform.clamp();
            let final_transform = session.current_screen_transform.clone();
            Some(InteractionEvent::GrabEnded {
                display_id: session.display_id,
                source: session.source,
                final_transform,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::interaction::Handedness;
    use crate::spatial::pose::{Quat, Vec3};
    use std::f32::consts::FRAC_PI_2;

    #[test]
    fn test_grab_state_machine_hover_and_grab_lifecycle() {
        let mut sm = GrabStateMachine::new();
        assert_eq!(*sm.state(), InteractionState::Idle);

        let mut screen = ScreenTransform::for_display(0);
        screen.display_mode = DisplayMode::FlatQuad;
        screen.set_distance(1.5);
        let screens = vec![(0, &screen)];

        // 1. Ray aiming at screen (not grabbing) -> HoverStarted
        let input_hover = InteractionInput {
            source: InteractionSource::Controller(Handedness::Right),
            aim_ray: Some(crate::spatial::ray::Ray::new(
                Vec3::new(0.0, 1.4, 0.0),
                Vec3::new(0.0, 0.0, -1.0),
            )),
            grab_pose: Some(ScreenPose::new(Vec3::new(0.2, 1.2, -0.4), Quat::IDENTITY)),
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };

        let ev1 = sm
            .update(&input_hover, screens.clone(), 0.016)
            .expect("HoverStarted");
        assert!(matches!(
            ev1,
            InteractionEvent::HoverStarted { display_id: 0, .. }
        ));
        assert!(sm.is_hovering());

        // 2. Start grab (grip pressed while hovering) -> GrabStarted
        let mut input_grab = input_hover;
        input_grab.is_grabbing = true;

        let ev2 = sm
            .update(&input_grab, screens.clone(), 0.016)
            .expect("GrabStarted");
        assert!(matches!(
            ev2,
            InteractionEvent::GrabStarted { display_id: 0, .. }
        ));
        assert!(sm.is_grabbing());

        // 3. Drag controller (translate right +0.3m, up +0.1m)
        let mut input_drag = input_grab;
        input_drag.grab_pose = Some(ScreenPose::new(Vec3::new(0.5, 1.3, -0.4), Quat::IDENTITY));

        let ev3 = sm
            .update(&input_drag, screens.clone(), 0.016)
            .expect("GrabUpdated");
        if let InteractionEvent::GrabUpdated { transform, .. } = ev3 {
            // Screen was originally at (0.0, 1.4, -1.5).
            // Controller moved by (+0.3, +0.1, 0.0).
            // Screen should follow offset and move to (+0.3, 1.5, -1.5)!
            assert!((transform.position.x - 0.3).abs() < 1e-4);
            assert!((transform.position.y - 1.5).abs() < 1e-4);
            assert!((transform.position.z - (-1.5)).abs() < 1e-4);
        } else {
            panic!("Expected GrabUpdated event");
        }

        // 4. Release grab (is_grabbing = false) -> GrabEnded with final transform
        let mut input_release = input_drag;
        input_release.is_grabbing = false;

        let ev4 = sm
            .update(&input_release, screens.clone(), 0.016)
            .expect("GrabEnded");
        if let InteractionEvent::GrabEnded {
            display_id,
            final_transform,
            ..
        } = ev4
        {
            assert_eq!(display_id, 0);
            assert!((final_transform.position.x - 0.3).abs() < 1e-4);
        } else {
            panic!("Expected GrabEnded event");
        }

        assert!(!sm.is_grabbing());
    }

    #[test]
    fn test_grab_state_machine_rigid_rotation_offset() {
        let mut sm = GrabStateMachine::new();
        let mut screen = ScreenTransform::for_display(0);
        screen.display_mode = DisplayMode::FlatQuad;
        screen.position = Vec3::new(0.0, 1.4, -1.0);
        screen.orientation = Quat::IDENTITY;
        let screens = vec![(0, &screen)];

        // Start grab at origin
        let input_start = InteractionInput {
            source: InteractionSource::Controller(Handedness::Right),
            aim_ray: Some(crate::spatial::ray::Ray::new(
                Vec3::new(0.0, 1.4, 0.0),
                Vec3::new(0.0, 0.0, -1.0),
            )),
            grab_pose: Some(ScreenPose::new(Vec3::new(0.0, 1.4, 0.0), Quat::IDENTITY)),
            is_grabbing: true,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };

        // Transition through hover first
        let mut input_hover = input_start;
        input_hover.is_grabbing = false;
        sm.update(&input_hover, screens.clone(), 0.016);
        sm.update(&input_start, screens.clone(), 0.016);
        assert!(sm.is_grabbing());

        // Rotate controller 90 degrees CCW (around Y):
        // Relative offset (0, 0, -1) rotates into (+1, 0, 0)!
        let rotated_pose = ScreenPose::new(
            Vec3::new(0.0, 1.4, 0.0),
            Quat::from_euler(0.0, FRAC_PI_2, 0.0),
        );
        let mut input_rotated = input_start;
        input_rotated.grab_pose = Some(rotated_pose);

        let ev = sm
            .update(&input_rotated, screens.clone(), 0.016)
            .expect("GrabUpdated");
        if let InteractionEvent::GrabUpdated { transform, .. } = ev {
            // Rotated offset from (0, 0, -1) rotated by 90 deg around Y:
            // Since OpenXR rotation maps -Z to -X or +X depending on yaw direction
            assert!(transform.position.x.abs() > 0.9);
            assert!((transform.position.z).abs() < 0.1);
        } else {
            panic!("Expected GrabUpdated");
        }
    }

    #[test]
    fn test_grab_state_machine_thumbstick_push_pull_and_curve() {
        let mut sm = GrabStateMachine::new();
        let mut screen = ScreenTransform::for_display(0);
        screen.display_mode = DisplayMode::CurvedCylinder;
        screen.set_distance(1.5);
        screen.curve_angle = 1.0;
        let screens = vec![(0, &screen)];

        // Start grab
        let input_hover = InteractionInput {
            source: InteractionSource::Controller(Handedness::Right),
            aim_ray: Some(crate::spatial::ray::Ray::new(
                Vec3::new(0.0, 1.4, 0.0),
                Vec3::new(0.0, 0.0, -1.0),
            )),
            grab_pose: Some(ScreenPose::new(Vec3::new(0.0, 1.4, 0.0), Quat::IDENTITY)),
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };
        sm.update(&input_hover, screens.clone(), 0.016);

        let mut input_grab = input_hover;
        input_grab.is_grabbing = true;
        sm.update(&input_grab, screens.clone(), 0.016);

        // Thumbstick push away: Y = +1.0
        let mut input_push = input_grab;
        input_push.thumbstick = (0.0, 1.0);
        let ev = sm
            .update(&input_push, screens.clone(), 0.1)
            .expect("Push update");
        if let InteractionEvent::GrabUpdated { transform, .. } = ev {
            // Distance should have increased by ~ 1.0 * 1.2 * 0.1 = 0.12m
            assert!(transform.distance > 1.55);
        }

        // Thumbstick curve adjust: X = +1.0
        let mut input_curve = input_grab;
        input_curve.thumbstick = (1.0, 0.0);
        let ev = sm
            .update(&input_curve, screens.clone(), 0.1)
            .expect("Curve update");
        if let InteractionEvent::GrabUpdated { transform, .. } = ev {
            assert!(transform.curve_angle > 1.05);
        }
    }
}
