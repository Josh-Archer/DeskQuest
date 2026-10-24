//! Pure-Rust spatial screen model, 6DoF transforms, pointing rays, gestures, and layout persistence.

pub mod arrangement;
pub mod grab;
pub mod interaction;
pub mod layout;
pub mod limits;
pub mod pointer;
pub mod pose;
pub mod ray;
pub mod scroll;
pub mod transform;

pub use arrangement::{
    host_arrangement_key, ArrangementBook, ArrangementBookStore, HostPlacement, LayoutPreset,
    PanelArrangement, FOCUS_RAISE_M, PANEL_GAP_RAD,
};
pub use grab::{
    corner_at, Corner, GrabMode, GrabSession, GrabStateMachine, InteractionEvent, InteractionState,
    TwoHandScale, CORNER_ZONE,
};
pub use interaction::{
    ControllerInputState, HandGestureDetector, HandGestureState, HandJoint, HandJointData,
    HandTrackingData, Handedness, InteractionInput, InteractionSource,
    DEFAULT_GRAB_RELEASE_THRESHOLD_M, DEFAULT_GRAB_START_THRESHOLD_M,
    DEFAULT_PINCH_RELEASE_THRESHOLD_M, DEFAULT_PINCH_START_THRESHOLD_M, HAND_JOINT_COUNT,
};
pub use layout::{SpatialLayoutStore, SpatialScreenLayout, CURRENT_LAYOUT_VERSION};
pub use limits::*;
pub use pointer::{PointerBindings, PointerInput};
pub use pose::{Quat, ScreenPose, Vec3};
pub use ray::{ray_intersects_cylinder, ray_intersects_quad, Ray, RayHit};
pub use transform::{CylinderLayerParams, QuadLayerParams, ScreenTransform};
