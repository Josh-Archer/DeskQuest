//! 6DoF screen grab state machine coordinating hover, rigid drag, corner resize, push/pull, tilt/curve, and layout persistence on release.

use crate::config::DisplayMode;
use crate::spatial::interaction::{InteractionInput, InteractionSource};
use crate::spatial::limits::{
    clamp_curve_angle, clamp_distance, clamp_tilt, MIN_TWO_HAND_SEPARATION_M,
};
use crate::spatial::pose::{ScreenPose, Vec3};
use crate::spatial::ray::{Ray, RayHit};
use crate::spatial::transform::ScreenTransform;

/// Size of the corner zones, as a fraction of the screen's width and height, that start a resize.
pub const CORNER_ZONE: f32 = 0.12;

/// Screen corner that a corner resize grabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Returns the corner whose zone contains the screen point `uv`, or `None` when it is on an edge
/// midpoint or in the interior.
///
/// `uv` uses the [`RayHit::uv`] convention: `(0, 0)` is the top-left corner and `(1, 1)` the
/// bottom-right. A point is in a corner zone when its `u` and its `v` are both within
/// [`CORNER_ZONE`] of the same pair of edges, boundaries included.
pub fn corner_at(uv: (f32, f32)) -> Option<Corner> {
    let (u, v) = uv;
    let on_left = u <= CORNER_ZONE;
    let on_right = u >= 1.0 - CORNER_ZONE;
    let on_top = v <= CORNER_ZONE;
    let on_bottom = v >= 1.0 - CORNER_ZONE;
    match (on_left, on_right, on_top, on_bottom) {
        (true, _, true, _) => Some(Corner::TopLeft),
        (_, true, true, _) => Some(Corner::TopRight),
        (true, _, _, true) => Some(Corner::BottomLeft),
        (_, true, _, true) => Some(Corner::BottomRight),
        _ => None,
    }
}

/// What a grab does to the screen while it is held.
#[derive(Debug, Clone, PartialEq)]
pub enum GrabMode {
    /// Rigid drag: the screen follows the grabbing hand or controller.
    Move,

    /// Corner resize: the screen size follows how far the grabbing ray reaches from the screen
    /// centre. Position and orientation stay fixed.
    Resize {
        /// Corner the grab started from.
        corner: Corner,

        /// Distance in meters from the screen centre to the grabbing ray's hit point at grab start.
        initial_reach: f32,

        /// Screen transform when the grab started. The size is always scaled from this. Boxed to keep
        /// `InteractionState` small, as `GrabSession::two_hand` is.
        base: Box<ScreenTransform>,
    },
}

/// Active grab manipulation session tracking initial poses and cumulative offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct GrabSession {
    /// Target display ID being manipulated.
    pub display_id: u32,

    /// Source that initiated this grab.
    pub source: InteractionSource,

    /// Whether the grab moves the screen or resizes it from a corner.
    pub mode: GrabMode,

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

    /// Corner under the hover ray, if hovering a screen at one of its corner zones.
    pub fn hovered_corner(&self) -> Option<Corner> {
        match &self.state {
            InteractionState::Hovering { hit, .. } => corner_at(hit.uv),
            _ => None,
        }
    }

    /// Corner being dragged while a corner resize is active.
    pub fn resizing(&self) -> Option<Corner> {
        match &self.state {
            InteractionState::Grabbing(GrabSession {
                mode: GrabMode::Resize { corner, .. },
                ..
            }) => Some(*corner),
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
                hit,
                source,
            } => {
                let hover_hit = *hit;
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
                // The hit that decides between move and corner resize belongs to the grabbing source.
                let (current_display_id, current_source, grab_hit) = match takeover {
                    Some((id, takeover_hit)) => (id, input.source, takeover_hit),
                    None => (current_display_id, current_source, hover_hit),
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

                    let mode = match corner_at(grab_hit.uv) {
                        Some(corner) => GrabMode::Resize {
                            corner,
                            initial_reach: initial_reach(
                                input.aim_ray.as_ref(),
                                &grab_hit,
                                &target_screen,
                            ),
                            base: Box::new(target_screen.clone()),
                        },
                        None => GrabMode::Move,
                    };

                    let session = GrabSession {
                        display_id: current_display_id,
                        source: input.source,
                        mode,
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

                // Corner resize: the grabbing ray's reach on the screen plane sets the size. Position,
                // orientation, thumbstick and any second hand are ignored. Without a usable ray the
                // size is kept.
                if let GrabMode::Resize {
                    initial_reach,
                    base,
                    ..
                } = &session.mode
                {
                    let mut transform = session.current_screen_transform.clone();
                    let reach = input
                        .aim_ray
                        .as_ref()
                        .and_then(|ray| plane_reach(ray, base));
                    if let Some(reach) = reach {
                        if *initial_reach > MIN_CORNER_REACH_M {
                            transform.set_uniform_scale_from(base, reach / *initial_reach);
                        }
                    }

                    session.current_screen_transform = transform.clone();
                    return Some(InteractionEvent::GrabUpdated {
                        display_id: session.display_id,
                        source: session.source,
                        transform,
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

/// Smallest grab-start reach, in meters, that can scale a corner resize. Below it the size is kept.
const MIN_CORNER_REACH_M: f32 = 1e-3;

/// Unit normal of the screen's facing plane, pointing towards the viewer.
fn facing_normal(screen: &ScreenTransform) -> Vec3 {
    screen.orientation.rotate_vec(Vec3::UNIT_Z).normalize()
}

/// Distance in meters from the screen centre to `point`, measured within the screen's facing plane.
fn in_plane_reach(point: Vec3, screen: &ScreenTransform) -> f32 {
    let offset = point - screen.position;
    let normal = facing_normal(screen);
    (offset - normal * offset.dot(normal)).length()
}

/// Distance in meters from the screen centre to where `ray` meets the screen's facing plane.
///
/// `None` when the ray is parallel to the plane or the plane lies behind its origin.
fn plane_reach(ray: &Ray, screen: &ScreenTransform) -> Option<f32> {
    let normal = facing_normal(screen);
    let approach = ray.direction.dot(normal);
    if approach.abs() < 1e-6 {
        return None;
    }

    let t = (screen.position - ray.origin).dot(normal) / approach;
    if t <= 1e-4 {
        return None;
    }
    Some(in_plane_reach(ray.at(t), screen))
}

/// Reach of a corner grab at the moment it starts.
///
/// Measured the same way as the reach during the drag, from the grabbing frame's ray, so an
/// unchanged ray gives a scale of exactly 1. A curved screen's hit point lies on the arc, not on
/// the facing plane, so the hover hit point alone would make the size jump on the first frame.
/// The hit point is used when the grabbing frame has no usable ray.
fn initial_reach(aim_ray: Option<&Ray>, hit: &RayHit, screen: &ScreenTransform) -> f32 {
    aim_ray
        .and_then(|ray| plane_reach(ray, screen))
        .unwrap_or_else(|| in_plane_reach(hit.point, screen))
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
    use crate::spatial::limits::{MAX_CURVE_ANGLE_RAD, MAX_SCREEN_WIDTH_M, MIN_SCREEN_WIDTH_M};
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

    /// Where the user's eyes sit in the corner resize tests. Aim rays start here.
    const USER_EYE: Vec3 = Vec3::new(0.0, 1.4, 0.0);

    /// Aim ray from `origin` through `target`.
    fn aim_at(origin: Vec3, target: Vec3) -> Ray {
        Ray::new(origin, target - origin)
    }

    /// Hover or grab input for `hand` whose aim ray is `ray`.
    fn ray_input(hand: Handedness, ray: Ray, grabbing: bool) -> InteractionInput {
        InteractionInput {
            source: InteractionSource::Hand(hand),
            aim_ray: Some(ray),
            grab_pose: Some(ScreenPose::new(ray.origin, Quat::IDENTITY)),
            is_grabbing: grabbing,
            is_selecting: grabbing,
            thumbstick: (0.0, 0.0),
        }
    }

    /// Point at screen coordinates `(u, v)` (see `RayHit::uv`) on an unrotated screen.
    fn screen_point(screen: &ScreenTransform, u: f32, v: f32) -> Vec3 {
        match screen.display_mode {
            DisplayMode::FlatQuad => {
                let (width, height) = screen.quad_size;
                Vec3::new(
                    screen.position.x + (u - 0.5) * width,
                    screen.position.y + (0.5 - v) * height,
                    screen.position.z,
                )
            }
            DisplayMode::CurvedCylinder => {
                let radius = screen.curve_radius;
                let angle = (u - 0.5) * screen.curve_angle;
                let arc_height = radius * screen.curve_angle / screen.aspect_ratio;
                let axis = screen.cylinder_axis_pose().position;
                Vec3::new(
                    axis.x + radius * angle.sin(),
                    screen.position.y + (0.5 - v) * arc_height,
                    axis.z - radius * angle.cos(),
                )
            }
        }
    }

    /// Point where the ray from `origin` through `through` meets the facing plane of an unrotated screen.
    fn on_facing_plane(screen: &ScreenTransform, origin: Vec3, through: Vec3) -> Vec3 {
        let dir = through - origin;
        let t = (screen.position.z - origin.z) / dir.z;
        origin + dir * t
    }

    /// Aim ray from the user to a point on the screen plane that is `factor` times as far from the
    /// centre as the plane point of `through`.
    fn scaled_reach_ray(screen: &ScreenTransform, through: Vec3, factor: f32) -> Ray {
        let grab_point = on_facing_plane(screen, USER_EYE, through);
        aim_at(
            USER_EYE,
            screen.position + (grab_point - screen.position) * factor,
        )
    }

    /// Hovers `through` with the right hand, then squeezes. Returns the machine grabbing from there and the grab ray.
    fn start_corner_grab(screen: &ScreenTransform, through: Vec3) -> (GrabStateMachine, Ray) {
        let mut sm = GrabStateMachine::new();
        let ray = aim_at(USER_EYE, through);
        sm.update(
            &ray_input(Handedness::Right, ray, false),
            vec![(0, screen)],
            0.016,
        );
        assert!(sm.hovered_corner().is_some(), "hover should be at a corner");

        let ev = sm
            .update(
                &ray_input(Handedness::Right, ray, true),
                vec![(0, screen)],
                0.016,
            )
            .expect("GrabStarted");
        assert!(matches!(ev, InteractionEvent::GrabStarted { .. }));
        (sm, ray)
    }

    #[test]
    fn test_corner_at_zones_and_boundaries() {
        assert_eq!(corner_at((0.0, 0.0)), Some(Corner::TopLeft));
        assert_eq!(corner_at((1.0, 0.0)), Some(Corner::TopRight));
        assert_eq!(corner_at((0.0, 1.0)), Some(Corner::BottomLeft));
        assert_eq!(corner_at((1.0, 1.0)), Some(Corner::BottomRight));

        // Zone boundaries count as inside the zone
        let far = 1.0 - CORNER_ZONE;
        assert_eq!(corner_at((CORNER_ZONE, CORNER_ZONE)), Some(Corner::TopLeft));
        assert_eq!(corner_at((far, far)), Some(Corner::BottomRight));
        assert_eq!(corner_at((far, CORNER_ZONE)), Some(Corner::TopRight));
        assert_eq!(corner_at((CORNER_ZONE, far)), Some(Corner::BottomLeft));

        // Just outside a zone in one direction is not a corner
        assert_eq!(corner_at((CORNER_ZONE + 0.01, CORNER_ZONE)), None);
        assert_eq!(corner_at((far - 0.01, 0.0)), None);
        assert_eq!(corner_at((0.0, CORNER_ZONE + 0.01)), None);

        // Edge midpoints and the interior are not corners
        assert_eq!(corner_at((0.5, 0.0)), None);
        assert_eq!(corner_at((0.0, 0.5)), None);
        assert_eq!(corner_at((0.05, 0.5)), None);
        assert_eq!(corner_at((0.5, 0.5)), None);
    }

    #[test]
    fn test_grab_started_in_middle_is_move() {
        let screen = flat_screen();
        let (sm, _) = start_right_grab(&screen);
        assert!(matches!(sm.grab_session().unwrap().mode, GrabMode::Move));
        assert_eq!(sm.resizing(), None);
    }

    #[test]
    fn test_corner_grab_resizes_flat_screen_by_reach() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);
        assert!(matches!(
            sm.grab_session().unwrap().mode,
            GrabMode::Resize {
                corner: Corner::TopLeft,
                ..
            }
        ));
        assert_eq!(sm.resizing(), Some(Corner::TopLeft));

        // Ray dragged out to twice the grab reach: the flat screen doubles about its centre
        let out = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 2.0),
            true,
        );
        let t = transform_of(sm.update(&out, vec![(0, &screen)], 0.016));
        assert_size(&t, 3.2, 1.8);
        assert_eq!(t.position, screen.position);
        assert!(sm.grab_session().unwrap().two_hand.is_none());

        // Ray pulled in to half the grab reach, same grab: the screen shrinks from the base size
        let back_in = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 0.5),
            true,
        );
        let t = transform_of(sm.update(&back_in, vec![(0, &screen)], 0.016));
        assert_size(&t, 0.8, 0.45);
        assert_eq!(t.position, screen.position);
    }

    #[test]
    fn test_corner_grab_resizes_curved_screen_arc() {
        let screen = ScreenTransform::for_display(0);
        assert_eq!(screen.display_mode, DisplayMode::CurvedCylinder);
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);

        let out = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 2.0),
            true,
        );
        let t = transform_of(sm.update(&out, vec![(0, &screen)], 0.016));
        assert!((t.curve_angle - 2.0 * screen.curve_angle).abs() < 1e-5);
        assert!((t.curve_radius - screen.curve_radius).abs() < 1e-6);
        assert!((t.aspect_ratio - screen.aspect_ratio).abs() < 1e-6);
        assert_eq!(t.position, screen.position);

        // Far out: the arc clamps at a half turn
        let far = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 6.0),
            true,
        );
        let t = transform_of(sm.update(&far, vec![(0, &screen)], 0.016));
        assert!((t.curve_angle - MAX_CURVE_ANGLE_RAD).abs() < 1e-6);
    }

    #[test]
    fn test_corner_grab_resize_clamps_to_size_limits() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);

        // Ten times the reach: the width clamps to the maximum, aspect ratio kept
        let out = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 10.0),
            true,
        );
        let t = transform_of(sm.update(&out, vec![(0, &screen)], 0.016));
        assert!((t.quad_size.0 - MAX_SCREEN_WIDTH_M).abs() < 1e-4);
        assert!((t.quad_size.0 / t.quad_size.1 - 16.0 / 9.0).abs() < 1e-4);
    }

    #[test]
    fn test_corner_grab_does_not_jump_when_ray_is_unchanged() {
        // The grab frame's own ray must give a scale of 1, on flat and curved screens alike
        for screen in [flat_screen(), ScreenTransform::for_display(0)] {
            let corner = screen_point(&screen, 0.05, 0.05);
            let (mut sm, grab_ray) = start_corner_grab(&screen, corner);
            let same = ray_input(Handedness::Right, grab_ray, true);
            let t = transform_of(sm.update(&same, vec![(0, &screen)], 0.016));
            assert!((t.quad_size.0 - screen.quad_size.0).abs() < 1e-5);
            assert!((t.curve_angle - screen.curve_angle).abs() < 1e-5);
        }
    }

    #[test]
    fn test_corner_resize_keeps_size_when_ray_misses_the_plane() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);

        // Parallel to the screen, then pointing away from it: neither meets the plane in front
        for ray in [
            aim_at(USER_EYE, USER_EYE + Vec3::UNIT_X),
            aim_at(USER_EYE, USER_EYE + Vec3::UNIT_Z),
        ] {
            let t = transform_of(sm.update(
                &ray_input(Handedness::Right, ray, true),
                vec![(0, &screen)],
                0.016,
            ));
            assert_size(&t, 1.6, 0.9);
        }

        // A grab frame without an aim ray keeps the size too
        let no_ray = grab_input(Handedness::Right, USER_EYE, true);
        let t = transform_of(sm.update(&no_ray, vec![(0, &screen)], 0.016));
        assert_size(&t, 1.6, 0.9);
        assert!(sm.is_grabbing());
    }

    #[test]
    fn test_corner_resize_release_emits_grab_ended_with_resized_transform() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);

        let out = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 2.0),
            true,
        );
        transform_of(sm.update(&out, vec![(0, &screen)], 0.016));

        // Release on the same ray: still over the resized screen, so the grab returns to hover
        let release = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 2.0),
            false,
        );
        match sm.update(&release, vec![(0, &screen)], 0.016) {
            Some(InteractionEvent::GrabEnded {
                display_id: 0,
                final_transform,
                ..
            }) => assert_size(&final_transform, 3.2, 1.8),
            other => panic!("expected GrabEnded, got {other:?}"),
        }
        assert!(sm.is_hovering());
        assert_eq!(sm.resizing(), None);
    }

    #[test]
    fn test_corner_resize_ignores_second_hand() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let (mut sm, _) = start_corner_grab(&screen, corner);

        // Left hand grabs 0.4 m away, which would start a two-handed scale in a move grab
        let left = grab_input(Handedness::Left, Vec3::new(-0.4, 1.4, 0.0), true);
        let still = ray_input(Handedness::Right, aim_at(USER_EYE, corner), true);
        let t = transform_of(sm.update_two(&still, Some(&left), vec![(0, &screen)], 0.016));
        assert_size(&t, 1.6, 0.9);
        assert!(sm.grab_session().unwrap().two_hand.is_none());

        let out = ray_input(
            Handedness::Right,
            scaled_reach_ray(&screen, corner, 2.0),
            true,
        );
        let t = transform_of(sm.update_two(&out, Some(&left), vec![(0, &screen)], 0.016));
        assert_size(&t, 3.2, 1.8);
        assert!(sm.grab_session().unwrap().two_hand.is_none());
        assert_eq!(sm.resizing(), Some(Corner::TopLeft));
    }

    #[test]
    fn test_takeover_grab_uses_the_grabbing_sources_own_hit() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let centre = screen.position;

        // Right hand hovers the middle, left hand squeezes while pointing at a corner: resize
        let mut sm = GrabStateMachine::new();
        sm.update(
            &ray_input(Handedness::Right, aim_at(USER_EYE, centre), false),
            vec![(0, &screen)],
            0.016,
        );
        let ev = sm
            .update(
                &ray_input(Handedness::Left, aim_at(USER_EYE, corner), true),
                vec![(0, &screen)],
                0.016,
            )
            .expect("GrabStarted");
        assert!(matches!(ev, InteractionEvent::GrabStarted { .. }));
        assert_eq!(
            sm.grab_session().unwrap().source,
            InteractionSource::Hand(Handedness::Left)
        );
        assert!(matches!(
            sm.grab_session().unwrap().mode,
            GrabMode::Resize {
                corner: Corner::TopLeft,
                ..
            }
        ));

        // Right hand hovers a corner, left hand squeezes in the middle: a plain move
        let mut sm = GrabStateMachine::new();
        sm.update(
            &ray_input(Handedness::Right, aim_at(USER_EYE, corner), false),
            vec![(0, &screen)],
            0.016,
        );
        sm.update(
            &ray_input(Handedness::Left, aim_at(USER_EYE, centre), true),
            vec![(0, &screen)],
            0.016,
        );
        assert!(matches!(sm.grab_session().unwrap().mode, GrabMode::Move));
    }

    #[test]
    fn test_hovered_corner_and_resizing_report_state() {
        let screen = flat_screen();
        let corner = screen_point(&screen, 0.05, 0.05);
        let centre = screen.position;
        let mut sm = GrabStateMachine::new();
        assert_eq!(sm.hovered_corner(), None);
        assert_eq!(sm.resizing(), None);

        let hover_corner = ray_input(Handedness::Right, aim_at(USER_EYE, corner), false);
        sm.update(&hover_corner, vec![(0, &screen)], 0.016);
        assert_eq!(sm.hovered_corner(), Some(Corner::TopLeft));
        assert_eq!(sm.resizing(), None);

        // Hovering the middle clears the corner
        sm.update(
            &ray_input(Handedness::Right, aim_at(USER_EYE, centre), false),
            vec![(0, &screen)],
            0.016,
        );
        assert_eq!(sm.hovered_corner(), None);

        // Grabbing from the corner: resizing reports it, and no hover corner while grabbing
        sm.update(&hover_corner, vec![(0, &screen)], 0.016);
        let grab_corner = ray_input(Handedness::Right, aim_at(USER_EYE, corner), true);
        sm.update(&grab_corner, vec![(0, &screen)], 0.016);
        assert_eq!(sm.hovered_corner(), None);
        assert_eq!(sm.resizing(), Some(Corner::TopLeft));

        // Released over the same corner: resizing ends and the corner is hovered again
        let release = ray_input(Handedness::Right, aim_at(USER_EYE, corner), false);
        sm.update(&release, vec![(0, &screen)], 0.016);
        assert_eq!(sm.resizing(), None);
        assert_eq!(sm.hovered_corner(), Some(Corner::TopLeft));
    }
}
