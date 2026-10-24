//! Middle-finger pinch scrolling, independent of index-pinch mouse selection.

use super::{
    HandJoint, HandTrackingData, InteractionInput, InteractionSource, Quat, Ray, ScreenTransform,
    Vec3,
};

const START_M: f32 = 0.025;
const RELEASE_M: f32 = 0.040;
const DEAD_ZONE_M: f32 = 0.005;
const NOTCHES_PER_METER: f32 = 40.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct HandScroll {
    pub suppress_buttons: bool,
    pub anchor_ray: Option<Ray>,
    pub delta: (f32, f32),
}

#[derive(Debug)]
struct Session {
    source: InteractionSource,
    display: u32,
    orientation: Quat,
    ray: Ray,
    previous: Vec3,
    moving: bool,
}

#[derive(Debug, Default)]
pub struct HandScrollDetector {
    session: Option<Session>,
}

impl HandScrollDetector {
    pub fn reset(&mut self) {
        self.session = None;
    }

    pub fn update(
        &mut self,
        input: &InteractionInput,
        hand: Option<&HandTrackingData>,
        screens: &[(u32, &ScreenTransform)],
    ) -> HandScroll {
        let was_active = self.session.is_some();
        let valid = hand.filter(|h| {
            input.source.is_hand()
                && h.is_active
                && [
                    HandJoint::ThumbTip,
                    HandJoint::MiddleTip,
                    HandJoint::IndexTip,
                ]
                .iter()
                .all(|j| {
                    let p = h.joint(*j);
                    p.is_valid
                        && p.position.x.is_finite()
                        && p.position.y.is_finite()
                        && p.position.z.is_finite()
                })
        });
        let Some(hand) = valid else {
            self.reset();
            return HandScroll {
                suppress_buttons: was_active && input.source.is_hand(),
                ..Default::default()
            };
        };
        let thumb = hand.joint(HandJoint::ThumbTip).position;
        let middle = hand.joint(HandJoint::MiddleTip).position;
        let index = hand.joint(HandJoint::IndexTip).position;
        let position = (thumb + middle) * 0.5;
        let threshold = if self.session.is_some() {
            RELEASE_M
        } else {
            START_M
        };
        // A released index finger distinguishes scrolling from clicking or a fist grab.
        if (thumb - middle).length() > threshold || (thumb - index).length() <= RELEASE_M {
            self.reset();
            return HandScroll {
                suppress_buttons: was_active,
                ..Default::default()
            };
        }
        if self.session.as_ref().is_some_and(|s| {
            s.source != input.source || !screens.iter().any(|(id, _)| *id == s.display)
        }) {
            self.reset();
            return HandScroll {
                suppress_buttons: was_active,
                ..Default::default()
            };
        }
        if self.session.is_none() {
            let Some(ray) = input.aim_ray else {
                return HandScroll::default();
            };
            let Some((display, _)) =
                ScreenTransform::find_closest_hit(&ray, screens.iter().copied())
            else {
                return HandScroll::default();
            };
            let orientation = screens
                .iter()
                .find(|(id, _)| *id == display)
                .unwrap()
                .1
                .orientation;
            self.session = Some(Session {
                source: input.source,
                display,
                orientation,
                ray,
                previous: position,
                moving: false,
            });
        }
        let session = self.session.as_mut().unwrap();
        let motion = session
            .orientation
            .conjugate()
            .rotate_vec(position - session.previous);
        let distance = (motion.x * motion.x + motion.y * motion.y).sqrt();
        let mut delta = (0.0, 0.0);
        // Keep a small dead zone until movement is intentional. Ignore tracking jumps.
        if distance > 0.15 {
            session.previous = position;
            session.moving = false;
        } else if session.moving || distance >= DEAD_ZONE_M {
            session.moving = true;
            session.previous = position;
            delta = (-motion.x * NOTCHES_PER_METER, -motion.y * NOTCHES_PER_METER);
        }
        HandScroll {
            suppress_buttons: true,
            anchor_ray: Some(session.ray),
            delta,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::Handedness;

    fn sample() -> (InteractionInput, HandTrackingData, ScreenTransform) {
        let screen = ScreenTransform::for_display(0);
        let input = InteractionInput {
            source: InteractionSource::Hand(Handedness::Right),
            aim_ray: Some(Ray::new(Vec3::new(0.0, 1.0, 0.0), -Vec3::UNIT_Z)),
            grab_pose: None,
            is_grabbing: false,
            is_selecting: false,
            thumbstick: (0.0, 0.0),
        };
        let mut hand = HandTrackingData {
            is_active: true,
            ..Default::default()
        };
        for (joint, x) in [
            (HandJoint::ThumbTip, 0.0),
            (HandJoint::MiddleTip, 0.01),
            (HandJoint::IndexTip, 0.1),
        ] {
            let j = hand.joint_mut(joint);
            j.is_valid = true;
            j.position = Vec3::new(x, 1.0, 0.0);
        }
        (input, hand, screen)
    }

    #[test]
    fn middle_pinch_moves_content_and_keeps_pointer_anchored() {
        let (mut input, mut hand, screen) = sample();
        let mut detector = HandScrollDetector::default();
        let start = detector.update(&input, Some(&hand), &[(0, &screen)]);
        assert!(start.suppress_buttons);
        assert_eq!(start.delta, (0.0, 0.0));
        for joint in [
            HandJoint::ThumbTip,
            HandJoint::MiddleTip,
            HandJoint::IndexTip,
        ] {
            hand.joint_mut(joint).position.y += 0.1;
        }
        input.aim_ray = None; // Once started, the target stays latched until release.
        let motion = detector.update(&input, Some(&hand), &[(0, &screen)]);
        assert!((motion.delta.1 + 4.0).abs() < 0.001);
        assert_eq!(motion.anchor_ray, start.anchor_ray);
    }

    #[test]
    fn index_pinch_and_invalid_or_lost_tracking_never_scroll() {
        let (input, mut hand, screen) = sample();
        let mut detector = HandScrollDetector::default();
        hand.joint_mut(HandJoint::IndexTip).position.x = 0.01;
        assert!(
            !detector
                .update(&input, Some(&hand), &[(0, &screen)])
                .suppress_buttons
        );
        hand.joint_mut(HandJoint::IndexTip).position.x = 0.1;
        assert!(
            detector
                .update(&input, Some(&hand), &[(0, &screen)])
                .suppress_buttons
        );
        hand.joint_mut(HandJoint::MiddleTip).is_valid = false;
        let lost = detector.update(&input, Some(&hand), &[(0, &screen)]);
        assert_eq!(lost.delta, (0.0, 0.0));
        assert!(lost.suppress_buttons); // Release cannot accidentally become a click.
        assert!(
            !detector
                .update(&input, None, &[(0, &screen)])
                .suppress_buttons
        );
    }

    #[test]
    fn small_jitter_is_ignored_and_removed_target_ends_scroll() {
        let (input, mut hand, screen) = sample();
        let mut detector = HandScrollDetector::default();
        detector.update(&input, Some(&hand), &[(0, &screen)]);
        hand.joint_mut(HandJoint::ThumbTip).position.y += 0.002;
        assert_eq!(
            detector.update(&input, Some(&hand), &[(0, &screen)]).delta,
            (0.0, 0.0)
        );
        assert!(detector
            .update(&input, Some(&hand), &[])
            .anchor_ray
            .is_none());
    }
}
