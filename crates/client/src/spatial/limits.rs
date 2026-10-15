//! Ergonomic limits and clamping constants for comfortable spatial computing.
//!
//! Prevents vergence-accommodation conflict, extreme neck strain, and unreadable display placements.

use std::f32::consts::{FRAC_PI_3, PI};

/// Minimum comfortable viewing distance from eyes (0.3 meters).
/// Closer than this causes severe eye strain / vergence-accommodation conflict.
pub const MIN_DISTANCE_M: f32 = 0.3;

/// Maximum comfortable viewing distance from eyes (5.0 meters).
/// Beyond this, standard desktop fonts become unreadable without excessive scaling.
pub const MAX_DISTANCE_M: f32 = 5.0;

/// Default viewing distance (1.2 meters).
pub const DEFAULT_DISTANCE_M: f32 = 1.2;

/// Minimum tilt / pitch angle (-60 degrees = -PI / 3 radians).
pub const MIN_TILT_RAD: f32 = -FRAC_PI_3;

/// Maximum tilt / pitch angle (+60 degrees = +PI / 3 radians).
pub const MAX_TILT_RAD: f32 = FRAC_PI_3;

/// Minimum yaw angle (-120 degrees = -2*PI / 3 radians).
pub const MIN_YAW_RAD: f32 = -PI * (2.0 / 3.0);

/// Maximum yaw angle (+120 degrees = +2*PI / 3 radians).
pub const MAX_YAW_RAD: f32 = PI * (2.0 / 3.0);

/// Minimum curved cylinder radius in meters (0.5m).
pub const MIN_CURVE_RADIUS_M: f32 = 0.5;

/// Maximum curved cylinder radius in meters (5.0m).
pub const MAX_CURVE_RADIUS_M: f32 = 5.0;

/// Default curved cylinder radius in meters (1.5m).
pub const DEFAULT_CURVE_RADIUS_M: f32 = 1.5;

/// Minimum central arc angle in radians (0.1 rad ≈ 5.7 degrees).
pub const MIN_CURVE_ANGLE_RAD: f32 = 0.1;

/// Maximum central arc angle in radians (PI rad = 180 degrees).
pub const MAX_CURVE_ANGLE_RAD: f32 = PI;

/// Default central arc angle in radians (1.2 rad ≈ 68.75 degrees).
pub const DEFAULT_CURVE_ANGLE_RAD: f32 = 1.2;

/// Minimum physical screen width in meters (0.2m).
pub const MIN_SCREEN_WIDTH_M: f32 = 0.2;

/// Maximum physical screen width in meters (5.0m).
pub const MAX_SCREEN_WIDTH_M: f32 = 5.0;

/// Minimum physical screen height in meters (0.1m).
pub const MIN_SCREEN_HEIGHT_M: f32 = 0.1;

/// Maximum physical screen height in meters (3.0m).
pub const MAX_SCREEN_HEIGHT_M: f32 = 3.0;

/// Minimum aspect ratio (0.25).
pub const MIN_ASPECT_RATIO: f32 = 0.25;

/// Maximum aspect ratio (4.0).
pub const MAX_ASPECT_RATIO: f32 = 4.0;

/// Minimum height (Y position in stage space) in meters (0.2m).
pub const MIN_HEIGHT_M: f32 = 0.2;

/// Maximum height (Y position in stage space) in meters (3.0m).
pub const MAX_HEIGHT_M: f32 = 3.0;

/// Default height (eye level in seated stage space) in meters (1.4m).
pub const DEFAULT_HEIGHT_M: f32 = 1.4;

/// Clamps viewing distance to ergonomic bounds [MIN_DISTANCE_M, MAX_DISTANCE_M].
pub fn clamp_distance(distance: f32) -> f32 {
    distance.clamp(MIN_DISTANCE_M, MAX_DISTANCE_M)
}

/// Clamps tilt/pitch angle to ergonomic bounds [MIN_TILT_RAD, MAX_TILT_RAD].
pub fn clamp_tilt(tilt: f32) -> f32 {
    tilt.clamp(MIN_TILT_RAD, MAX_TILT_RAD)
}

/// Clamps yaw angle to ergonomic bounds [MIN_YAW_RAD, MAX_YAW_RAD].
pub fn clamp_yaw(yaw: f32) -> f32 {
    yaw.clamp(MIN_YAW_RAD, MAX_YAW_RAD)
}

/// Clamps curvature radius to [MIN_CURVE_RADIUS_M, MAX_CURVE_RADIUS_M].
pub fn clamp_curve_radius(radius: f32) -> f32 {
    radius.clamp(MIN_CURVE_RADIUS_M, MAX_CURVE_RADIUS_M)
}

/// Clamps curvature angle to [MIN_CURVE_ANGLE_RAD, MAX_CURVE_ANGLE_RAD].
pub fn clamp_curve_angle(angle: f32) -> f32 {
    angle.clamp(MIN_CURVE_ANGLE_RAD, MAX_CURVE_ANGLE_RAD)
}

/// Clamps physical screen dimensions in meters to comfortable ranges.
pub fn clamp_screen_size(width: f32, height: f32) -> (f32, f32) {
    (
        width.clamp(MIN_SCREEN_WIDTH_M, MAX_SCREEN_WIDTH_M),
        height.clamp(MIN_SCREEN_HEIGHT_M, MAX_SCREEN_HEIGHT_M),
    )
}

/// Clamps aspect ratio to [MIN_ASPECT_RATIO, MAX_ASPECT_RATIO].
pub fn clamp_aspect_ratio(aspect: f32) -> f32 {
    aspect.clamp(MIN_ASPECT_RATIO, MAX_ASPECT_RATIO)
}

/// Clamps height to [MIN_HEIGHT_M, MAX_HEIGHT_M].
pub fn clamp_height(y: f32) -> f32 {
    y.clamp(MIN_HEIGHT_M, MAX_HEIGHT_M)
}
