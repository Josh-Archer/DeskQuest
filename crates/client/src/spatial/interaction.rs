//! Interaction input models, 26-joint hand tracking definitions, and gesture detection with hysteresis.

use serde::{Deserialize, Serialize};

use crate::spatial::pose::{Quat, ScreenPose, Vec3};
use crate::spatial::ray::Ray;

/// OpenXR controller or hand tracker handedness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Handedness {
    Left,
    Right,
}

/// Identifies whether an interaction input originates from a controller or tracked hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InteractionSource {
    Controller(Handedness),
    Hand(Handedness),
}

impl InteractionSource {
    /// Returns the handedness of this input source.
    pub fn handedness(&self) -> Handedness {
        match self {
            InteractionSource::Controller(h) | InteractionSource::Hand(h) => *h,
        }
    }

    /// Whether this input comes from a controller.
    pub fn is_controller(&self) -> bool {
        matches!(self, InteractionSource::Controller(_))
    }

    /// Whether this input comes from hand tracking.
    pub fn is_hand(&self) -> bool {
        matches!(self, InteractionSource::Hand(_))
    }
}

/// Controller input state captured from OpenXR action sets.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ControllerInputState {
    /// 6DoF pointing / aim pose in stage reference space.
    pub aim_pose: Option<ScreenPose>,

    /// 6DoF palm / grip pose in stage reference space.
    pub grip_pose: Option<ScreenPose>,

    /// Analog index trigger depression `[0.0, 1.0]`.
    pub trigger_value: f32,

    /// Digital trigger click / threshold crossed.
    pub trigger_clicked: bool,

    /// Analog grip squeeze `[0.0, 1.0]`.
    pub grip_value: f32,

    /// Digital grip click / grab threshold crossed.
    pub grip_clicked: bool,

    /// 2D thumbstick position `(x, y)` in `[-1.0, 1.0]`.
    /// - `x`: horizontal deflection (left negative, right positive).
    /// - `y`: vertical deflection (down negative, up positive).
    pub thumbstick: (f32, f32),

    /// Digital thumbstick click.
    pub thumbstick_clicked: bool,
}

impl ControllerInputState {
    /// Returns true if either digital grip clicked or analog squeeze is above grab threshold (0.5).
    pub fn is_grabbing(&self) -> bool {
        self.grip_clicked || self.grip_value >= 0.5
    }

    /// Returns true if either digital trigger clicked or analog trigger is above select threshold (0.5).
    pub fn is_selecting(&self) -> bool {
        self.trigger_clicked || self.trigger_value >= 0.5
    }
}

/// Number of standard joints defined by the OpenXR `XR_EXT_hand_tracking` extension (26 joints).
pub const HAND_JOINT_COUNT: usize = 26;

/// 26 standard OpenXR hand joint identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum HandJoint {
    Palm = 0,
    Wrist = 1,
    ThumbMetacarpal = 2,
    ThumbProximal = 3,
    ThumbDistal = 4,
    ThumbTip = 5,
    IndexMetacarpal = 6,
    IndexProximal = 7,
    IndexIntermediate = 8,
    IndexDistal = 9,
    IndexTip = 10,
    MiddleMetacarpal = 11,
    MiddleProximal = 12,
    MiddleIntermediate = 13,
    MiddleDistal = 14,
    MiddleTip = 15,
    RingMetacarpal = 16,
    RingProximal = 17,
    RingIntermediate = 18,
    RingDistal = 19,
    RingTip = 20,
    LittleMetacarpal = 21,
    LittleProximal = 22,
    LittleIntermediate = 23,
    LittleDistal = 24,
    LittleTip = 25,
}

impl HandJoint {
    /// Converts a raw OpenXR joint index to `HandJoint`.
    pub fn from_index(index: usize) -> Option<Self> {
        if index < HAND_JOINT_COUNT {
            // SAFETY: index is bounded within 0..HAND_JOINT_COUNT which exactly matches enum variants
            Some(unsafe { std::mem::transmute::<usize, HandJoint>(index) })
        } else {
            None
        }
    }

    /// Index of this joint in the 26-joint array.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// 3D pose and radius of a single hand joint.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct HandJointData {
    /// 3D position in meters in stage reference space.
    pub position: Vec3,
    /// Orientation quaternion.
    pub orientation: Quat,
    /// Estimated joint radius in meters.
    pub radius: f32,
    /// Whether this joint was actively tracked and valid this frame.
    pub is_valid: bool,
}

/// Full 26-joint skeletal snapshot for a tracked hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandTrackingData {
    /// Whether the hand is currently in tracking view and active.
    pub is_active: bool,
    /// 26 joint poses indexed by `HandJoint`.
    pub joints: [HandJointData; HAND_JOINT_COUNT],
}

impl Default for HandTrackingData {
    fn default() -> Self {
        Self {
            is_active: false,
            joints: [HandJointData::default(); HAND_JOINT_COUNT],
        }
    }
}

impl HandTrackingData {
    /// Retrieves the pose of a specific joint.
    pub fn joint(&self, joint: HandJoint) -> &HandJointData {
        &self.joints[joint.index()]
    }

    /// Retrieves a mutable reference to the pose of a specific joint.
    pub fn joint_mut(&mut self, joint: HandJoint) -> &mut HandJointData {
        &mut self.joints[joint.index()]
    }
}

/// Gesture recognition state derived from joint poses.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HandGestureState {
    /// Whether the hand is actively tracked.
    pub is_active: bool,

    /// Euclidean distance between thumb tip and index tip in meters.
    pub pinch_distance: f32,

    /// Whether an index pinch gesture is active (with hysteresis applied).
    pub is_pinching: bool,

    /// Average curl distance between fingertips and palm in meters.
    pub grab_distance: f32,

    /// Whether a fist/palm grab gesture is active (with hysteresis applied).
    pub is_grabbing: bool,

    /// 3D pointing ray computed from wrist and index knuckle geometry.
    pub aim_ray: Option<Ray>,

    /// 6DoF pose of the pinch point (midpoint between thumb and index tip).
    pub pinch_pose: Option<ScreenPose>,

    /// 6DoF pose of the palm center.
    pub palm_pose: Option<ScreenPose>,
}

/// Default distance threshold in meters to enter pinch state (25 mm).
pub const DEFAULT_PINCH_START_THRESHOLD_M: f32 = 0.025;

/// Default distance threshold in meters to exit pinch state (40 mm) for hysteresis.
pub const DEFAULT_PINCH_RELEASE_THRESHOLD_M: f32 = 0.040;

/// Default curl distance threshold in meters to enter fist grab state (55 mm).
pub const DEFAULT_GRAB_START_THRESHOLD_M: f32 = 0.055;

/// Default curl distance threshold in meters to exit fist grab state (75 mm) for hysteresis.
pub const DEFAULT_GRAB_RELEASE_THRESHOLD_M: f32 = 0.075;

/// Gesture detector with hysteresis preventing jitter near pinch/grab transition boundaries.
#[derive(Debug, Clone)]
pub struct HandGestureDetector {
    /// Distance threshold to start a pinch gesture in meters.
    pub pinch_start_threshold: f32,
    /// Distance threshold to release a pinch gesture in meters (hysteresis).
    pub pinch_release_threshold: f32,
    /// Distance threshold to start a fist grab gesture in meters.
    pub grab_start_threshold: f32,
    /// Distance threshold to release a fist grab gesture in meters (hysteresis).
    pub grab_release_threshold: f32,

    left_pinching: bool,
    left_grabbing: bool,
    right_pinching: bool,
    right_grabbing: bool,
}

impl Default for HandGestureDetector {
    fn default() -> Self {
        Self {
            pinch_start_threshold: DEFAULT_PINCH_START_THRESHOLD_M,
            pinch_release_threshold: DEFAULT_PINCH_RELEASE_THRESHOLD_M,
            grab_start_threshold: DEFAULT_GRAB_START_THRESHOLD_M,
            grab_release_threshold: DEFAULT_GRAB_RELEASE_THRESHOLD_M,
            left_pinching: false,
            left_grabbing: false,
            right_pinching: false,
            right_grabbing: false,
        }
    }
}

impl HandGestureDetector {
    /// Creates a gesture detector with default Quest ergonomic thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates gesture recognition for the specified hand with hysteresis.
    pub fn update(&mut self, hand: Handedness, data: &HandTrackingData) -> HandGestureState {
        if !data.is_active {
            match hand {
                Handedness::Left => {
                    self.left_pinching = false;
                    self.left_grabbing = false;
                }
                Handedness::Right => {
                    self.right_pinching = false;
                    self.right_grabbing = false;
                }
            }
            return HandGestureState::default();
        }

        let thumb_tip = data.joint(HandJoint::ThumbTip);
        let index_tip = data.joint(HandJoint::IndexTip);
        let middle_tip = data.joint(HandJoint::MiddleTip);
        let ring_tip = data.joint(HandJoint::RingTip);
        let little_tip = data.joint(HandJoint::LittleTip);
        let palm = data.joint(HandJoint::Palm);
        let wrist = data.joint(HandJoint::Wrist);
        let index_prox = data.joint(HandJoint::IndexProximal);

        // 1. Pinch Detection (Thumb Tip to Index Tip)
        let pinch_distance = (thumb_tip.position - index_tip.position).length();

        let is_pinching = match hand {
            Handedness::Left => {
                if self.left_pinching {
                    self.left_pinching = pinch_distance <= self.pinch_release_threshold;
                } else {
                    self.left_pinching = pinch_distance <= self.pinch_start_threshold;
                }
                self.left_pinching
            }
            Handedness::Right => {
                if self.right_pinching {
                    self.right_pinching = pinch_distance <= self.pinch_release_threshold;
                } else {
                    self.right_pinching = pinch_distance <= self.pinch_start_threshold;
                }
                self.right_pinching
            }
        };

        // 2. Fist / Palm Grab Detection (Average distance from 4 fingertips to palm)
        let d_index = (index_tip.position - palm.position).length();
        let d_middle = (middle_tip.position - palm.position).length();
        let d_ring = (ring_tip.position - palm.position).length();
        let d_little = (little_tip.position - palm.position).length();
        let grab_distance = (d_index + d_middle + d_ring + d_little) * 0.25;

        let is_grabbing = match hand {
            Handedness::Left => {
                if self.left_grabbing {
                    self.left_grabbing = grab_distance <= self.grab_release_threshold;
                } else {
                    self.left_grabbing = grab_distance <= self.grab_start_threshold;
                }
                self.left_grabbing
            }
            Handedness::Right => {
                if self.right_grabbing {
                    self.right_grabbing = grab_distance <= self.grab_release_threshold;
                } else {
                    self.right_grabbing = grab_distance <= self.grab_start_threshold;
                }
                self.right_grabbing
            }
        };

        // 3. Aim Ray calculation
        // Ray origin is near the index knuckle; pointing direction is along index finger axis from wrist
        let aim_origin = if is_pinching {
            (thumb_tip.position + index_tip.position) * 0.5
        } else {
            index_prox.position
        };

        let pointer_vec = index_prox.position - wrist.position;
        let aim_dir = if pointer_vec.length_squared() > 1e-4 {
            pointer_vec.normalize()
        } else {
            -Vec3::UNIT_Z
        };
        let aim_ray = Some(Ray::new(aim_origin, aim_dir));

        // 4. Pinch Pose (midpoint between thumb and index)
        let pinch_pos = (thumb_tip.position + index_tip.position) * 0.5;
        let pinch_pose = Some(ScreenPose::new(pinch_pos, index_prox.orientation));

        // 5. Palm Pose
        let palm_pose = Some(ScreenPose::new(palm.position, palm.orientation));

        HandGestureState {
            is_active: true,
            pinch_distance,
            is_pinching,
            grab_distance,
            is_grabbing,
            aim_ray,
            pinch_pose,
            palm_pose,
        }
    }
}

/// Unified interaction input abstraction feeding the screen manipulation state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InteractionInput {
    /// Originating source (controller or hand).
    pub source: InteractionSource,

    /// Pointing ray for screen hover and target detection.
    pub aim_ray: Option<Ray>,

    /// 6DoF reference pose used for screen dragging.
    pub grab_pose: Option<ScreenPose>,

    /// Whether grab manipulation is active (controller grip squeezed or hand pinching/grabbing).
    pub is_grabbing: bool,

    /// Whether selection action is active (controller trigger clicked or hand pinched).
    pub is_selecting: bool,

    /// Thumbstick input: `(x, y)` in `[-1.0, 1.0]`.
    pub thumbstick: (f32, f32),
}

impl InteractionInput {
    /// Constructs unified interaction input from an OpenXR controller input state.
    pub fn from_controller(hand: Handedness, state: &ControllerInputState) -> Self {
        let aim_ray = state.aim_pose.as_ref().map(Ray::from_aim_pose);
        let grab_pose = state.grip_pose.or(state.aim_pose);

        Self {
            source: InteractionSource::Controller(hand),
            aim_ray,
            grab_pose,
            is_grabbing: state.is_grabbing(),
            is_selecting: state.is_selecting(),
            thumbstick: state.thumbstick,
        }
    }

    /// Constructs unified interaction input from a recognized hand gesture state.
    pub fn from_hand(hand: Handedness, gesture: &HandGestureState) -> Self {
        let grab_pose = if gesture.is_pinching {
            gesture.pinch_pose.or(gesture.palm_pose)
        } else {
            gesture.palm_pose
        };

        Self {
            source: InteractionSource::Hand(hand),
            aim_ray: gesture.aim_ray,
            grab_pose,
            is_grabbing: gesture.is_pinching || gesture.is_grabbing,
            is_selecting: gesture.is_pinching,
            thumbstick: (0.0, 0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_controller_input_state_thresholds() {
        let mut state = ControllerInputState::default();
        assert!(!state.is_grabbing());
        assert!(!state.is_selecting());

        // Digital click
        state.grip_clicked = true;
        assert!(state.is_grabbing());
        state.grip_clicked = false;

        // Analog squeeze threshold
        state.grip_value = 0.49;
        assert!(!state.is_grabbing());
        state.grip_value = 0.50;
        assert!(state.is_grabbing());

        state.trigger_value = 0.6;
        assert!(state.is_selecting());
    }

    #[test]
    fn test_hand_gesture_hysteresis() {
        let mut detector = HandGestureDetector::new();
        let mut data = HandTrackingData {
            is_active: true,
            ..Default::default()
        };

        // Initially fingers wide apart: 100mm (0.1m)
        data.joint_mut(HandJoint::ThumbTip).position = Vec3::new(0.0, 0.0, 0.0);
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.1, 0.0, 0.0);

        let g1 = detector.update(Handedness::Right, &data);
        assert!(!g1.is_pinching);

        // Move closer to 30mm (between start 25mm and release 40mm) - should still not pinch!
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.030, 0.0, 0.0);
        let g2 = detector.update(Handedness::Right, &data);
        assert!(
            !g2.is_pinching,
            "Should not pinch before start threshold (25mm)"
        );

        // Pinch closes to 20mm (< 25mm start threshold) - becomes pinched!
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.020, 0.0, 0.0);
        let g3 = detector.update(Handedness::Right, &data);
        assert!(g3.is_pinching, "Should become pinched under 25mm");

        // Fingers open back to 35mm (between start 25mm and release 40mm) - should STAY pinched due to hysteresis!
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.035, 0.0, 0.0);
        let g4 = detector.update(Handedness::Right, &data);
        assert!(
            g4.is_pinching,
            "Should STAY pinched under release threshold (40mm)"
        );

        // Fingers open past 40mm release threshold (e.g. 45mm) - pinch releases!
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.045, 0.0, 0.0);
        let g5 = detector.update(Handedness::Right, &data);
        assert!(!g5.is_pinching, "Should release pinch once exceeding 40mm");
    }

    #[test]
    fn test_pinch_jitter_around_start_threshold_does_not_flicker() {
        let mut detector = HandGestureDetector::new();
        let mut data = HandTrackingData {
            is_active: true,
            ..Default::default()
        };
        data.joint_mut(HandJoint::ThumbTip).position = Vec3::new(0.0, 0.0, 0.0);

        // Fingers open at 26 mm: not pinched yet
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.026, 0.0, 0.0);
        assert!(!detector.update(Handedness::Right, &data).is_pinching);

        // Crossing below the 25 mm start threshold pinches once
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.024, 0.0, 0.0);
        assert!(detector.update(Handedness::Right, &data).is_pinching);

        // Distance jitters around the start threshold, between 22 mm and 31 mm:
        // the 40 mm release threshold keeps the pinch on for every sample
        for mm in [
            0.026, 0.024, 0.027, 0.023, 0.025, 0.031, 0.022, 0.029, 0.024, 0.026,
        ] {
            data.joint_mut(HandJoint::IndexTip).position = Vec3::new(mm, 0.0, 0.0);
            assert!(
                detector.update(Handedness::Right, &data).is_pinching,
                "pinch flickered off at {mm} m"
            );
        }

        // Only opening past the 40 mm release threshold ends the pinch
        data.joint_mut(HandJoint::IndexTip).position = Vec3::new(0.041, 0.0, 0.0);
        assert!(!detector.update(Handedness::Right, &data).is_pinching);
    }

    #[test]
    fn test_unified_interaction_input_conversion() {
        let ctrl_state = ControllerInputState {
            aim_pose: Some(ScreenPose::IDENTITY),
            grip_pose: Some(ScreenPose::IDENTITY),
            grip_value: 0.8,
            thumbstick: (0.2, -0.5),
            ..Default::default()
        };

        let input = InteractionInput::from_controller(Handedness::Right, &ctrl_state);
        assert_eq!(
            input.source,
            InteractionSource::Controller(Handedness::Right)
        );
        assert!(input.is_grabbing);
        assert_eq!(input.thumbstick, (0.2, -0.5));
        assert!(input.aim_ray.is_some());
    }
}
