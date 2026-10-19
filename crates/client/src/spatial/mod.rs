//! Pure-Rust spatial screen model, 6DoF transforms, pointing rays, gestures, and layout persistence.

pub mod grab;
pub mod interaction;
pub mod layout;
pub mod limits;
pub mod pose;
pub mod ray;
pub mod transform;

pub use grab::{GrabSession, GrabStateMachine, InteractionEvent, InteractionState, TwoHandScale};
pub use interaction::{
    ControllerInputState, HandGestureDetector, HandGestureState, HandJoint, HandJointData,
    HandTrackingData, Handedness, InteractionInput, InteractionSource,
    DEFAULT_GRAB_RELEASE_THRESHOLD_M, DEFAULT_GRAB_START_THRESHOLD_M,
    DEFAULT_PINCH_RELEASE_THRESHOLD_M, DEFAULT_PINCH_START_THRESHOLD_M, HAND_JOINT_COUNT,
};
pub use layout::{SpatialLayoutStore, SpatialScreenLayout, CURRENT_LAYOUT_VERSION};
pub use limits::*;
pub use pose::{Quat, ScreenPose, Vec3};
pub use ray::{ray_intersects_cylinder, ray_intersects_quad, Ray, RayHit};
pub use transform::{CylinderLayerParams, QuadLayerParams, ScreenTransform};
