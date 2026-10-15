//! OpenXR XR_EXT_hand_tracking support, skeletal joint querying, and mock hand backend.

use crate::error::{ClientError, ClientResult};
use crate::spatial::interaction::{
    HandJoint, HandJointData, HandTrackingData, Handedness, HAND_JOINT_COUNT,
};
use crate::spatial::pose::{Quat, Vec3};

/// Backend interface for querying skeletal hand tracking data from OpenXR `XR_EXT_hand_tracking`.
pub trait HandTrackingBackend: Send + Sync {
    /// Returns true if `XR_EXT_hand_tracking` extension is supported by the OpenXR runtime.
    fn is_supported(&self) -> bool;

    /// Returns true if hand tracking is actively tracking and delivering data.
    fn is_active(&self) -> bool;

    /// Locates all 26 skeletal joints in reference space at the given display time.
    fn locate_joints(
        &mut self,
        hand: Handedness,
        time: openxr::Time,
        stage_space: Option<&openxr::Space>,
    ) -> ClientResult<Option<HandTrackingData>>;
}

/// Simulated hand tracking backend for testing gestures, hysteresis, and manipulation.
#[derive(Debug, Clone)]
pub struct MockHandTrackingBackend {
    supported: bool,
    left_hand: Option<HandTrackingData>,
    right_hand: Option<HandTrackingData>,
    locate_count: usize,
}

impl Default for MockHandTrackingBackend {
    fn default() -> Self {
        Self::new(true)
    }
}

impl MockHandTrackingBackend {
    /// Creates a mock hand tracking backend with specified support flag.
    pub fn new(supported: bool) -> Self {
        Self {
            supported,
            left_hand: None,
            right_hand: None,
            locate_count: 0,
        }
    }

    /// Creates an unsupported hand tracking backend (e.g. running on 3DoF headset or desktop).
    pub fn unsupported() -> Self {
        Self::new(false)
    }

    /// Sets the full joint skeletal data for a hand.
    pub fn set_hand_data(&mut self, hand: Handedness, data: Option<HandTrackingData>) {
        match hand {
            Handedness::Left => self.left_hand = data,
            Handedness::Right => self.right_hand = data,
        }
    }

    /// Helper to simulate an open hand with fingertips spread out.
    pub fn simulate_open_hand(&mut self, hand: Handedness, palm_pos: Vec3, dir: Vec3) {
        let mut data = HandTrackingData {
            is_active: true,
            ..Default::default()
        };

        let forward = dir.normalize();
        let wrist_pos = palm_pos - forward * 0.08;
        data.joint_mut(HandJoint::Wrist).position = wrist_pos;
        data.joint_mut(HandJoint::Wrist).is_valid = true;

        data.joint_mut(HandJoint::Palm).position = palm_pos;
        data.joint_mut(HandJoint::Palm).is_valid = true;

        data.joint_mut(HandJoint::IndexProximal).position = palm_pos + forward * 0.05;
        data.joint_mut(HandJoint::IndexProximal).is_valid = true;

        // Thumb and index tips far apart (80mm)
        data.joint_mut(HandJoint::ThumbTip).position = palm_pos + Vec3::new(-0.04, 0.0, 0.04);
        data.joint_mut(HandJoint::ThumbTip).is_valid = true;

        data.joint_mut(HandJoint::IndexTip).position =
            palm_pos + forward * 0.09 + Vec3::new(0.02, 0.0, 0.0);
        data.joint_mut(HandJoint::IndexTip).is_valid = true;

        data.joint_mut(HandJoint::MiddleTip).position = palm_pos + forward * 0.10;
        data.joint_mut(HandJoint::MiddleTip).is_valid = true;

        data.joint_mut(HandJoint::RingTip).position = palm_pos + forward * 0.09;
        data.joint_mut(HandJoint::RingTip).is_valid = true;

        data.joint_mut(HandJoint::LittleTip).position = palm_pos + forward * 0.08;
        data.joint_mut(HandJoint::LittleTip).is_valid = true;

        self.set_hand_data(hand, Some(data));
    }

    /// Helper to simulate a pinch gesture with thumb and index tips at a specific distance.
    pub fn simulate_pinch(&mut self, hand: Handedness, pinch_pos: Vec3, dir: Vec3, distance: f32) {
        let mut data = HandTrackingData {
            is_active: true,
            ..Default::default()
        };

        let forward = dir.normalize();
        let palm_pos = pinch_pos - forward * 0.06;
        let wrist_pos = palm_pos - forward * 0.08;

        data.joint_mut(HandJoint::Wrist).position = wrist_pos;
        data.joint_mut(HandJoint::Wrist).is_valid = true;

        data.joint_mut(HandJoint::Palm).position = palm_pos;
        data.joint_mut(HandJoint::Palm).is_valid = true;

        data.joint_mut(HandJoint::IndexProximal).position = palm_pos + forward * 0.04;
        data.joint_mut(HandJoint::IndexProximal).is_valid = true;

        let half_d = distance * 0.5;
        data.joint_mut(HandJoint::ThumbTip).position = pinch_pos + Vec3::new(-half_d, 0.0, 0.0);
        data.joint_mut(HandJoint::ThumbTip).is_valid = true;

        data.joint_mut(HandJoint::IndexTip).position = pinch_pos + Vec3::new(half_d, 0.0, 0.0);
        data.joint_mut(HandJoint::IndexTip).is_valid = true;

        // Middle/ring/little fingertips loosely open
        data.joint_mut(HandJoint::MiddleTip).position = palm_pos + forward * 0.08;
        data.joint_mut(HandJoint::MiddleTip).is_valid = true;
        data.joint_mut(HandJoint::RingTip).position = palm_pos + forward * 0.07;
        data.joint_mut(HandJoint::RingTip).is_valid = true;
        data.joint_mut(HandJoint::LittleTip).position = palm_pos + forward * 0.06;
        data.joint_mut(HandJoint::LittleTip).is_valid = true;

        self.set_hand_data(hand, Some(data));
    }

    /// Helper to simulate a fist grab with all fingertips curled into the palm.
    pub fn simulate_fist_grab(&mut self, hand: Handedness, palm_pos: Vec3, curl_dist: f32) {
        let mut data = HandTrackingData {
            is_active: true,
            ..Default::default()
        };

        let wrist_pos = palm_pos - Vec3::new(0.0, 0.0, -0.08);
        data.joint_mut(HandJoint::Wrist).position = wrist_pos;
        data.joint_mut(HandJoint::Wrist).is_valid = true;

        data.joint_mut(HandJoint::Palm).position = palm_pos;
        data.joint_mut(HandJoint::Palm).is_valid = true;

        data.joint_mut(HandJoint::IndexProximal).position = palm_pos + Vec3::new(0.0, 0.0, -0.03);
        data.joint_mut(HandJoint::IndexProximal).is_valid = true;

        // All tips curled close to palm at curl_dist
        data.joint_mut(HandJoint::ThumbTip).position = palm_pos + Vec3::new(-curl_dist, 0.0, 0.0);
        data.joint_mut(HandJoint::ThumbTip).is_valid = true;

        data.joint_mut(HandJoint::IndexTip).position = palm_pos + Vec3::new(0.0, curl_dist, 0.0);
        data.joint_mut(HandJoint::IndexTip).is_valid = true;

        data.joint_mut(HandJoint::MiddleTip).position = palm_pos + Vec3::new(0.0, 0.0, -curl_dist);
        data.joint_mut(HandJoint::MiddleTip).is_valid = true;

        data.joint_mut(HandJoint::RingTip).position = palm_pos + Vec3::new(curl_dist, 0.0, 0.0);
        data.joint_mut(HandJoint::RingTip).is_valid = true;

        data.joint_mut(HandJoint::LittleTip).position = palm_pos + Vec3::new(0.0, -curl_dist, 0.0);
        data.joint_mut(HandJoint::LittleTip).is_valid = true;

        self.set_hand_data(hand, Some(data));
    }

    /// Returns the number of times `locate_joints` has been called.
    pub fn locate_count(&self) -> usize {
        self.locate_count
    }
}

impl HandTrackingBackend for MockHandTrackingBackend {
    fn is_supported(&self) -> bool {
        self.supported
    }

    fn is_active(&self) -> bool {
        self.supported && (self.left_hand.is_some() || self.right_hand.is_some())
    }

    fn locate_joints(
        &mut self,
        hand: Handedness,
        _time: openxr::Time,
        _stage_space: Option<&openxr::Space>,
    ) -> ClientResult<Option<HandTrackingData>> {
        self.locate_count += 1;
        if !self.supported {
            return Ok(None);
        }

        match hand {
            Handedness::Left => Ok(self.left_hand.clone()),
            Handedness::Right => Ok(self.right_hand.clone()),
        }
    }
}

/// Live OpenXR `XR_EXT_hand_tracking` backend managing native hand trackers and joint queries.
pub struct OpenXrHandTrackingBackend {
    left_tracker: Option<openxr::HandTracker>,
    right_tracker: Option<openxr::HandTracker>,
    supported: bool,
}

impl OpenXrHandTrackingBackend {
    /// Initializes OpenXR hand trackers for left and right hands.
    pub fn try_init<G: openxr::Graphics>(session: &openxr::Session<G>) -> ClientResult<Self> {
        let left_tracker = session
            .create_hand_tracker(openxr::Hand::LEFT)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to create left hand tracker: {e:?}"))
            })?;

        let right_tracker = session
            .create_hand_tracker(openxr::Hand::RIGHT)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to create right hand tracker: {e:?}"))
            })?;

        Ok(Self {
            left_tracker: Some(left_tracker),
            right_tracker: Some(right_tracker),
            supported: true,
        })
    }
}

impl HandTrackingBackend for OpenXrHandTrackingBackend {
    fn is_supported(&self) -> bool {
        self.supported
    }

    fn is_active(&self) -> bool {
        self.supported && (self.left_tracker.is_some() || self.right_tracker.is_some())
    }

    fn locate_joints(
        &mut self,
        hand: Handedness,
        time: openxr::Time,
        stage_space: Option<&openxr::Space>,
    ) -> ClientResult<Option<HandTrackingData>> {
        if !self.supported {
            return Ok(None);
        }

        let stage = match stage_space {
            Some(s) => s,
            None => return Ok(None),
        };

        let tracker = match hand {
            Handedness::Left => match &self.left_tracker {
                Some(t) => t,
                None => return Ok(None),
            },
            Handedness::Right => match &self.right_tracker {
                Some(t) => t,
                None => return Ok(None),
            },
        };

        let locations_opt = stage
            .locate_hand_joints(tracker, time)
            .map_err(|e| ClientError::OpenXr(format!("Failed to locate hand joints: {e:?}")))?;

        if let Some(locations) = locations_opt {
            let mut tracking_data = HandTrackingData {
                is_active: true,
                joints: [HandJointData::default(); HAND_JOINT_COUNT],
            };

            for (idx, loc) in locations.iter().enumerate().take(HAND_JOINT_COUNT) {
                let is_valid = loc.location_flags.contains(
                    openxr::SpaceLocationFlags::POSITION_VALID
                        | openxr::SpaceLocationFlags::ORIENTATION_VALID,
                );

                tracking_data.joints[idx] = HandJointData {
                    position: Vec3::new(
                        loc.pose.position.x,
                        loc.pose.position.y,
                        loc.pose.position.z,
                    ),
                    orientation: Quat::new(
                        loc.pose.orientation.x,
                        loc.pose.orientation.y,
                        loc.pose.orientation.z,
                        loc.pose.orientation.w,
                    ),
                    radius: loc.radius,
                    is_valid,
                };
            }

            Ok(Some(tracking_data))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_hand_tracking_lifecycle_and_helpers() {
        let mut backend = MockHandTrackingBackend::new(true);
        assert!(backend.is_supported());
        assert!(!backend.is_active());

        // Simulate open hand
        backend.simulate_open_hand(
            Handedness::Right,
            Vec3::new(0.0, 1.2, -0.3),
            Vec3::new(0.0, 0.0, -1.0),
        );
        assert!(backend.is_active());

        let right_data = backend
            .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
            .unwrap()
            .expect("Right hand data should be present");

        assert!(right_data.is_active);
        assert_eq!(backend.locate_count(), 1);
        assert!(right_data.joint(HandJoint::ThumbTip).is_valid);
        assert!(right_data.joint(HandJoint::IndexTip).is_valid);

        // Unsupported backend returns None
        let mut unsupported = MockHandTrackingBackend::unsupported();
        assert!(!unsupported.is_supported());
        assert!(!unsupported.is_active());
        assert!(unsupported
            .locate_joints(Handedness::Right, openxr::Time::from_nanos(0), None)
            .unwrap()
            .is_none());
    }
}
