//! 6DoF screen grab state machine coordinating hover, rigid drag, push/pull, tilt/curve, and layout persistence on release.

use crate::config::DisplayMode;
use crate::spatial::interaction::{InteractionInput, InteractionSource};
use crate::spatial::limits::{
    clamp_curve_angle, clamp_distance, clamp_tilt, MIN_TWO_HAND_SEPARATION_M,
};
use crate::spatial::pose::{ScreenPose, Vec3};
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

    /// Two-handed scale state, present while a second hand or controller also grabs the screen.
    pub two_hand: Option<Box<TwoHandScale>>,
}

/// State captured when a second hand or controller joins an active grab to scale the screen.
#[derive(Debug, Clone, PartialEq)]
pub struct TwoHandScale {
    /// Input source of the second hand or controller.
    pub secondary_source: InteractionSource,

    /// Distance in meters between the two grab poses when the second hand joined.
    pub join_separation: f32,

    /// Screen transform when the second hand joined. The scale is applied relative to this size.
    pub base_transform: ScreenTransform,

    /// Midpoint between the two grab poses on the previous frame.
    pub last_midpoint: Vec3,
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
        self.update_two(input, None, screens, dt_seconds)
    }

    /// Processes a frame with an optional second hand or controller.
    ///
    /// While grabbing, a second input from the other hand that is also grabbing turns the grab into a
    /// two-handed scale: the screen size follows the ratio of the current to the joining distance between
    /// the two grab poses, and the screen moves with the midpoint of the hands. When either hand releases,
    /// the remaining hand continues as a one-hand grab. `secondary` is ignored outside an active grab.
    pub fn update_two<'a>(
        &mut self,
        input: &InteractionInput,
        secondary: Option<&InteractionInput>,
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

                // Another hand or controller pointing at a screen and squeezing takes the grab, so
                // the user does not have to squeeze with the hand that happened to hover first.
                let takeover = (input.is_grabbing && input.source != current_source)
                    .then_some(input.aim_ray.as_ref())
                    .flatten()
                    .and_then(|ray| {
                        ScreenTransform::find_closest_hit(ray, screens_vec.iter().copied())
                    });
                let (current_display_id, current_source) = match takeover {
                    Some((id, _)) => (id, input.source),
                    None => (current_display_id, current_source),
                };

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
                        two_hand: None,
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

                // Other hand or controller that is grabbing and can take part in a two-handed scale
                let live_secondary = secondary
                    .filter(|s| s.is_grabbing && s.source != session.source)
                    .and_then(|s| s.grab_pose.map(|pose| (s.source, pose)));

                // Grab released: transition out and emit GrabEnded to trigger layout persistence
                if !input.is_grabbing {
                    // While scaling, the other hand takes over as a one-hand grab instead of ending it
                    if session.two_hand.is_some() {
                        if let Some((sec_source, sec_pose)) = live_secondary {
                            session.two_hand = None;
                            session.source = sec_source;
                            session.initial_controller_pose = sec_pose;
                            session.initial_screen_pose = session.current_screen_transform.pose();
                            session.initial_screen_transform =
                                session.current_screen_transform.clone();
                            session.current_screen_transform.clamp();
                            return Some(InteractionEvent::GrabUpdated {
                                display_id: session.display_id,
                                source: session.source,
                                transform: session.current_screen_transform.clone(),
                            });
                        }
                    }

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

                let primary_pose = input.grab_pose?;

                // Second hand released: rebase the one-hand drag on the current pose so the screen does not jump
                if let Some(th) = &session.two_hand {
                    if live_secondary.map(|(source, _)| source) != Some(th.secondary_source) {
                        session.two_hand = None;
                        session.initial_controller_pose = primary_pose;
                        session.initial_screen_pose = session.current_screen_transform.pose();
                        session.initial_screen_transform = session.current_screen_transform.clone();
                    }
                }

                // Second hand joined: start the two-handed scale from the current screen size
                if session.two_hand.is_none() {
                    if let Some((sec_source, sec_pose)) = live_secondary {
                        let separation = (primary_pose.position - sec_pose.position).length();
                        if separation >= MIN_TWO_HAND_SEPARATION_M {
                            session.two_hand = Some(Box::new(TwoHandScale {
                                secondary_source: sec_source,
                                join_separation: separation,
                                base_transform: session.current_screen_transform.clone(),
                                last_midpoint: (primary_pose.position + sec_pose.position) * 0.5,
                            }));
                        }
                    }
                }

                // Two-handed scale: size from hand separation, position from midpoint motion
                if let (Some(th), Some((_, sec_pose))) = (&mut session.two_hand, live_secondary) {
                    let midpoint = (primary_pose.position + sec_pose.position) * 0.5;
                    let separation = (primary_pose.position - sec_pose.position).length();

                    let mut transform = session.current_screen_transform.clone();
                    transform.translate(midpoint - th.last_midpoint);
                    th.last_midpoint = midpoint;
                    transform.set_uniform_scale_from(
                        &th.base_transform,
                        separation / th.join_separation,
                    );

                    apply_thumbstick(
                        &mut transform,
                        input.thumbstick,
                        dt_seconds,
                        (self.push_pull_speed, self.tilt_speed, self.curve_speed),
                    );

                    transform.clamp();
                    session.current_screen_transform = transform.clone();

                    return Some(InteractionEvent::GrabUpdated {
                        display_id: session.display_id,
                        source: session.source,
                        transform,
                    });
                }

                // One-hand drag: calculate 6DoF rigid-body transform following controller pose
                let current_ctrl_pose = primary_pose;
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

                // 6. and 7. Thumbstick push/pull, tilt and curve adjustments
                apply_thumbstick(
                    &mut transform,
                    input.thumbstick,
                    dt_seconds,
                    (self.push_pull_speed, self.tilt_speed, self.curve_speed),
                );

                transform.clamp();
                session.current_screen_transform = transform.clone();

                Some(InteractionEvent::GrabUpdated {
                    display_id: session.display_id,
                    source: session.source,
                    transform,
                })
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

/// Applies thumbstick push/pull (Y) and tilt or curve (X) to a grabbed screen transform.
///
/// `speeds` is `(push_pull_speed, tilt_speed, curve_speed)`.
fn apply_thumbstick(
    transform: &mut ScreenTransform,
    thumbstick: (f32, f32),
    dt_seconds: f32,
    speeds: (f32, f32, f32),
) {
    let (push_pull_speed, tilt_speed, curve_speed) = speeds;

    // Thumbstick Y: push/pull distance
    let ts_y = thumbstick.1;
    if ts_y.abs() > 0.05 {
        let delta_dist = ts_y * push_pull_speed * dt_seconds;
        transform.push_pull(delta_dist);
    }

    // Thumbstick X: curvature angle (curved) or tilt (flat)
    let ts_x = thumbstick.0;
    if ts_x.abs() > 0.05 {
        match transform.display_mode {
            DisplayMode::CurvedCylinder => {
                let delta_angle = ts_x * curve_speed * dt_seconds;
                transform.curve_angle = clamp_curve_angle(transform.curve_angle + delta_angle);
            }
            DisplayMode::FlatQuad => {
                let delta_tilt = ts_x * tilt_speed * dt_seconds;
                transform.set_tilt(transform.tilt + delta_tilt);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::interaction::Handedness;
    use crate::spatial::limits::{MAX_SCREEN_WIDTH_M, MIN_SCREEN_WIDTH_M};
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

    /// Flat quad screen at the default position (0, 1.4, -1.2), 1.6 m x 0.9 m.
    fn flat_screen() -> ScreenTransform {
        let mut screen = ScreenTransform::for_display(0);
        screen.display_mode = DisplayMode::FlatQuad;
        screen
    }

    /// Hover input whose aim ray points at the centre of `flat_screen()`.
    fn hover_input(hand: Handedness, pos: Vec3) -> InteractionInput {
        InteractionInput {
            source: InteractionSource::Hand(hand),
            aim_ray: Some(crate::spatial::ray::Ray::new(
                Vec3::new(0.0, 1.4, 0.0),
                Vec3::new(0.0, 0.0, -1.0),
            )),
            grab_pose: Some(ScreenPose::new(pos, Quat::IDENTITY)),
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        }
    }

    /// Grab input for a hand at `pos`, without an aim ray (as used for a second hand).
    fn grab_input(hand: Handedness, pos: Vec3, grabbing: bool) -> InteractionInput {
        InteractionInput {
            source: InteractionSource::Hand(hand),
            aim_ray: None,
            grab_pose: Some(ScreenPose::new(pos, Quat::IDENTITY)),
            is_grabbing: grabbing,
            is_selecting: grabbing,
            thumbstick: (0.0, 0.0),
        }
    }

    /// Starts a right-hand grab on the flat screen and returns the state machine ready to scale.
    fn start_right_grab(screen: &ScreenTransform) -> (GrabStateMachine, InteractionInput) {
        let mut sm = GrabStateMachine::new();
        let right_pos = Vec3::new(0.15, 1.4, -0.4);
        let hover = hover_input(Handedness::Right, right_pos);
        sm.update(&hover, vec![(0, screen)], 0.016);

        let mut grab = grab_input(Handedness::Right, right_pos, true);
        grab.aim_ray = hover.aim_ray;
        let ev = sm
            .update(&grab, vec![(0, screen)], 0.016)
            .expect("GrabStarted");
        assert!(matches!(ev, InteractionEvent::GrabStarted { .. }));
        (sm, grab)
    }

    fn assert_size(transform: &ScreenTransform, width: f32, height: f32) {
        assert!(
            (transform.quad_size.0 - width).abs() < 1e-4,
            "width {} != {}",
            transform.quad_size.0,
            width
        );
        assert!(
            (transform.quad_size.1 - height).abs() < 1e-4,
            "height {} != {}",
            transform.quad_size.1,
            height
        );
    }

    fn transform_of(event: Option<InteractionEvent>) -> ScreenTransform {
        match event.expect("expected an event") {
            InteractionEvent::GrabUpdated { transform, .. } => transform,
            other => panic!("expected GrabUpdated, got {other:?}"),
        }
    }

    /// Joins the left hand at x = -`half_sep` while the right hand is at x = +`half_sep`, returns the event transform.
    fn set_hands(
        sm: &mut GrabStateMachine,
        screen: &ScreenTransform,
        right_grab: &InteractionInput,
        centre_x: f32,
        half_sep: f32,
    ) -> ScreenTransform {
        let right = grab_input(
            Handedness::Right,
            Vec3::new(centre_x + half_sep, 1.4, -0.4),
            true,
        );
        let left = grab_input(
            Handedness::Left,
            Vec3::new(centre_x - half_sep, 1.4, -0.4),
            true,
        );
        let _ = right_grab;
        transform_of(sm.update_two(&right, Some(&left), vec![(0, screen)], 0.016))
    }

    #[test]
    fn test_two_hand_scale_up_doubles_size() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);

        // Second hand joins 0.3 m away: scale factor starts at 1
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );
        assert_size(&t, 1.6, 0.9);
        assert!(sm.grab_session().unwrap().two_hand.is_some());

        // Hands move apart to 0.6 m: size doubles, aspect ratio kept
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.3,
        );
        assert_size(&t, 3.2, 1.8);
        assert!((t.quad_size.0 / t.quad_size.1 - 16.0 / 9.0).abs() < 1e-4);
        assert!(sm.is_grabbing());
    }

    #[test]
    fn test_two_hand_scale_down_halves_size() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );

        // Hands move together to 0.15 m apart: size halves
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.075,
        );
        assert_size(&t, 0.8, 0.45);
    }

    #[test]
    fn test_two_hand_scale_clamps_to_min_and_max() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );

        // Far apart (3 m): width clamps to MAX_SCREEN_WIDTH_M, aspect ratio preserved
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            1.5,
        );
        assert!((t.quad_size.0 - MAX_SCREEN_WIDTH_M).abs() < 1e-4);
        assert!((t.quad_size.0 / t.quad_size.1 - 16.0 / 9.0).abs() < 1e-4);

        // Very close together (3 cm): width clamps to MIN_SCREEN_WIDTH_M
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.015,
        );
        assert!((t.quad_size.0 - MIN_SCREEN_WIDTH_M).abs() < 1e-4);
        assert!((t.quad_size.0 / t.quad_size.1 - 16.0 / 9.0).abs() < 1e-4);
    }

    #[test]
    fn test_two_hand_scale_curved_widens_the_arc() {
        // Default display mode is the curved cylinder: its arc (central angle) carries the size
        let screen = ScreenTransform::for_display(0);
        assert_eq!(screen.display_mode, DisplayMode::CurvedCylinder);
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );

        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.3,
        );
        assert!((t.curve_radius - screen.curve_radius).abs() < 1e-6);
        assert!(
            t.curve_angle > screen.curve_angle * 1.2,
            "arc widens: {} -> {}",
            screen.curve_angle,
            t.curve_angle
        );
        assert!((t.aspect_ratio - screen.aspect_ratio).abs() < 1e-6);
    }

    #[test]
    fn test_two_hand_scale_moves_screen_with_midpoint() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );

        // Both hands shift 0.2 m to the right without changing separation: screen follows the midpoint
        let t = set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.2,
            0.15,
        );
        assert!((t.position.x - 0.2).abs() < 1e-4);
        assert!((t.position.y - 1.4).abs() < 1e-4);
        assert_size(&t, 1.6, 0.9);
    }

    #[test]
    fn test_two_hand_second_hand_not_grabbing_does_not_scale() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);

        let right = grab_input(Handedness::Right, Vec3::new(0.45, 1.4, -0.4), true);
        let left_open = grab_input(Handedness::Left, Vec3::new(-0.45, 1.4, -0.4), false);
        let t = transform_of(sm.update_two(&right, Some(&left_open), vec![(0, &screen)], 0.016));
        assert_size(&t, 1.6, 0.9);
        assert!(sm.grab_session().unwrap().two_hand.is_none());
    }

    #[test]
    fn test_two_hand_release_one_hand_continues_one_hand_grab() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.3,
        );

        // Right hand releases mid-scale while left keeps grabbing: left takes over, no jump
        let right_release = grab_input(Handedness::Right, Vec3::new(0.3, 1.4, -0.4), false);
        let left = grab_input(Handedness::Left, Vec3::new(-0.3, 1.4, -0.4), true);
        let t = transform_of(sm.update_two(&right_release, Some(&left), vec![(0, &screen)], 0.016));
        assert_size(&t, 3.2, 1.8);
        assert!(
            t.position.x.abs() < 1e-4,
            "screen should not jump on handover"
        );
        assert_eq!(
            sm.grab_session().unwrap().source,
            InteractionSource::Hand(Handedness::Left)
        );
        assert!(sm.grab_session().unwrap().two_hand.is_none());
        assert!(sm.is_grabbing());

        // Left hand moves 0.1 m to the left: one-hand drag moves the screen with it
        let left_moved = grab_input(Handedness::Left, Vec3::new(-0.4, 1.4, -0.4), true);
        let t = transform_of(sm.update(&left_moved, vec![(0, &screen)], 0.016));
        assert!((t.position.x - (-0.1)).abs() < 1e-4);
        assert_size(&t, 3.2, 1.8);

        // Releasing the last hand ends the grab with the scaled size kept
        let left_release = grab_input(Handedness::Left, Vec3::new(-0.4, 1.4, -0.4), false);
        match sm.update(&left_release, vec![(0, &screen)], 0.016) {
            Some(InteractionEvent::GrabEnded {
                final_transform, ..
            }) => assert_size(&final_transform, 3.2, 1.8),
            other => panic!("expected GrabEnded, got {other:?}"),
        }
    }

    #[test]
    fn test_two_hand_second_release_continues_one_hand_grab() {
        let screen = flat_screen();
        let (mut sm, _) = start_right_grab(&screen);
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.15,
        );
        set_hands(
            &mut sm,
            &screen,
            &grab_input(Handedness::Right, Vec3::ZERO, true),
            0.0,
            0.3,
        );

        // Left hand releases mid-scale: right hand continues one-handed, scaled size is kept
        let right = grab_input(Handedness::Right, Vec3::new(0.3, 1.4, -0.4), true);
        let left_release = grab_input(Handedness::Left, Vec3::new(-0.3, 1.4, -0.4), false);
        let t = transform_of(sm.update_two(&right, Some(&left_release), vec![(0, &screen)], 0.016));
        assert_size(&t, 3.2, 1.8);
        assert!(sm.grab_session().unwrap().two_hand.is_none());
        assert_eq!(
            sm.grab_session().unwrap().source,
            InteractionSource::Hand(Handedness::Right)
        );

        // First frame after the release does not jump; then one-hand drag moves the screen by the hand delta
        let t = transform_of(sm.update_two(&right, None, vec![(0, &screen)], 0.016));
        assert!(t.position.x.abs() < 1e-4);
        let right_moved = grab_input(Handedness::Right, Vec3::new(0.5, 1.4, -0.4), true);
        let t = transform_of(sm.update_two(&right_moved, None, vec![(0, &screen)], 0.016));
        assert!((t.position.x - 0.2).abs() < 1e-4);
        assert_size(&t, 3.2, 1.8);
    }

    #[test]
    fn test_single_hand_behaviour_unchanged_by_update_two_without_secondary() {
        let screen = flat_screen();
        let hover = hover_input(Handedness::Right, Vec3::new(0.0, 1.4, -0.4));
        let mut grab = hover;
        grab.is_grabbing = true;
        let mut drag = grab;
        drag.grab_pose = Some(ScreenPose::new(Vec3::new(0.3, 1.3, -0.4), Quat::IDENTITY));
        let mut release = drag;
        release.is_grabbing = false;

        let mut plain = GrabStateMachine::new();
        let mut two = GrabStateMachine::new();
        for input in [&hover, &grab, &drag, &release] {
            let a = plain.update(input, vec![(0, &screen)], 0.016);
            let b = two.update_two(input, None, vec![(0, &screen)], 0.016);
            assert_eq!(a, b);
        }
        assert!(!two.is_grabbing());
    }
}
