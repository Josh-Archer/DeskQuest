//! Pure-Rust spatial screen model, 6DoF transforms, and layout persistence.

pub mod layout;
pub mod limits;
pub mod pose;
pub mod transform;

pub use layout::{SpatialLayoutStore, SpatialScreenLayout, CURRENT_LAYOUT_VERSION};
pub use limits::*;
pub use pose::{Quat, ScreenPose, Vec3};
pub use transform::{CylinderLayerParams, QuadLayerParams, ScreenTransform};
