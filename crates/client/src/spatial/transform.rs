//! 6DoF spatial screen transform model and mapping to OpenXR layer parameters.

use serde::{Deserialize, Serialize};

use crate::config::DisplayMode;
use crate::openxr::layers::DesktopLayerConfig;
use crate::spatial::limits::*;
use crate::spatial::pose::{Quat, ScreenPose, Vec3};

/// Parameters for OpenXR Quad composition layer submission.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadLayerParams {
    /// 6DoF pose of the quad center in reference space.
    pub pose: openxr::Posef,
    /// Physical width and height in meters.
    pub size: openxr::Extent2Df,
}

/// Parameters for OpenXR Cylinder composition layer submission.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CylinderLayerParams {
    /// 6DoF pose of the cylinder axis in reference space.
    pub pose: openxr::Posef,
    /// Cylinder curvature radius in meters.
    pub radius: f32,
    /// Central arc angle in radians.
    pub central_angle: f32,
    /// Aspect ratio (width / height) of the cylinder layer.
    pub aspect_ratio: f32,
}

/// 6DoF spatial transform and presentation geometry for a virtual desktop screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenTransform {
    /// 6DoF spatial position in stage reference space (x, y, z in meters).
    pub position: Vec3,

    /// 6DoF orientation quaternion.
    pub orientation: Quat,

    /// Viewing distance from user origin in meters.
    pub distance: f32,

    /// Pitch / tilt angle in radians (positive = looking/tilted upward).
    pub tilt: f32,

    /// Yaw angle in radians (positive = positioned to the left / rotated CCW).
    pub yaw: f32,

    /// Roll angle in radians.
    pub roll: f32,

    /// Physical monitor width and height in meters for flat quad mode.
    pub quad_size: (f32, f32),

    /// Curvature radius in meters for curved cylinder mode.
    pub curve_radius: f32,

    /// Central arc angle in radians for curved cylinder mode.
    pub curve_angle: f32,

    /// Aspect ratio (width / height) for curved cylinder mode.
    pub aspect_ratio: f32,

    /// Display projection mode (FlatQuad or CurvedCylinder).
    pub display_mode: DisplayMode,
}

impl Default for ScreenTransform {
    fn default() -> Self {
        let distance = DEFAULT_DISTANCE_M;
        let tilt = 0.0;
        let yaw = 0.0;
        let roll = 0.0;
        let orientation = Quat::from_euler(tilt, yaw, roll);
        let position = Vec3::new(0.0, DEFAULT_HEIGHT_M, -distance);

        Self {
            position,
            orientation,
            distance,
            tilt,
            yaw,
            roll,
            quad_size: (1.6, 0.9),
            curve_radius: DEFAULT_CURVE_RADIUS_M,
            curve_angle: DEFAULT_CURVE_ANGLE_RAD,
            aspect_ratio: 16.0 / 9.0,
            display_mode: DisplayMode::CurvedCylinder,
        }
    }
}

impl ScreenTransform {
    /// Creates an ergonomically positioned spatial screen layout tailored for a specific display ID.
    ///
    /// - Display 0: Primary center monitor in front of user.
    /// - Display 1: Secondary monitor angled to the right.
    /// - Display 2: Tertiary monitor angled to the left.
    /// - Display N: Additional monitors positioned in an ergonomic cockpit arc.
    pub fn for_display(display_id: u32) -> Self {
        let mut transform = Self::default();

        match display_id {
            0 => {
                // Primary center monitor
            }
            1 => {
                // Right-side monitor angled inward towards user
                let yaw = -0.45; // ~ -25.8 degrees
                transform.yaw = yaw;
                transform.distance = 1.25;
                transform.orientation = Quat::from_euler(0.0, yaw, 0.0);
                transform.update_position_from_distance_and_angles();
            }
            2 => {
                // Left-side monitor angled inward towards user
                let yaw = 0.45; // ~ +25.8 degrees
                transform.yaw = yaw;
                transform.distance = 1.25;
                transform.orientation = Quat::from_euler(0.0, yaw, 0.0);
                transform.update_position_from_distance_and_angles();
            }
            n => {
                // Further side monitors wrapping around the user
                let is_odd = (n % 2) == 1;
                let step = n.div_ceil(2) as f32;
                let sign = if is_odd { -1.0 } else { 1.0 };
                let yaw = clamp_yaw(sign * (0.45 * step));
                transform.yaw = yaw;
                transform.distance = clamp_distance(1.2 + 0.05 * step);
                transform.orientation = Quat::from_euler(0.0, yaw, 0.0);
                transform.update_position_from_distance_and_angles();
            }
        }

        transform.clamp();
        transform
    }

    /// Internal helper to synchronize 3D position from spherical distance, yaw, and tilt.
    fn update_position_from_distance_and_angles(&mut self) {
        let cos_tilt = self.tilt.cos();
        let sin_tilt = self.tilt.sin();
        let cos_yaw = self.yaw.cos();
        let sin_yaw = self.yaw.sin();

        // In OpenXR: -Z is forward, +X is right, +Y is up.
        // Positive yaw rotates counter-clockwise (to the left, -X).
        self.position.x = -sin_yaw * cos_tilt * self.distance;
        self.position.y = clamp_height(DEFAULT_HEIGHT_M + sin_tilt * self.distance);
        self.position.z = -cos_yaw * cos_tilt * self.distance;
    }

    /// Translates the 3D position by `delta` in meters, clamping to comfortable ergonomic bounds.
    pub fn translate(&mut self, delta: Vec3) {
        self.position += delta;
        self.clamp();

        // Update distance metric to match current position in horizontal plane
        let horiz_dist =
            (self.position.x * self.position.x + self.position.z * self.position.z).sqrt();
        self.distance = clamp_distance(horiz_dist);
    }

    /// Rotates the virtual screen by incremental Euler angles in radians: pitch (tilt), yaw, roll.
    pub fn rotate(&mut self, delta_pitch: f32, delta_yaw: f32, delta_roll: f32) {
        self.tilt = clamp_tilt(self.tilt + delta_pitch);
        self.yaw = clamp_yaw(self.yaw + delta_yaw);
        self.roll = (self.roll + delta_roll)
            .clamp(-std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_4);

        self.orientation = Quat::from_euler(self.tilt, self.yaw, self.roll);
        self.clamp();
    }

    /// Sets the viewing distance in meters, clamped to [MIN_DISTANCE_M, MAX_DISTANCE_M].
    pub fn set_distance(&mut self, distance: f32) {
        let clamped = clamp_distance(distance);
        let cur_horiz =
            (self.position.x * self.position.x + self.position.z * self.position.z).sqrt();

        if cur_horiz > 1e-4 {
            let scale = clamped / cur_horiz;
            self.position.x *= scale;
            self.position.z *= scale;
        } else {
            self.position.z = -clamped;
        }

        self.distance = clamped;
        self.clamp();
    }

    /// Pushes or pulls the virtual screen relative to the user by `delta_distance` meters.
    pub fn push_pull(&mut self, delta_distance: f32) {
        self.set_distance(self.distance + delta_distance);
    }

    /// Sets the vertical tilt (pitch) angle in radians, clamped to ergonomic bounds.
    pub fn set_tilt(&mut self, tilt_rad: f32) {
        self.tilt = clamp_tilt(tilt_rad);
        self.orientation = Quat::from_euler(self.tilt, self.yaw, self.roll);
        self.clamp();
    }

    /// Sets the horizontal yaw angle in radians, clamped to ergonomic bounds.
    pub fn set_yaw(&mut self, yaw_rad: f32) {
        self.yaw = clamp_yaw(yaw_rad);
        self.orientation = Quat::from_euler(self.tilt, self.yaw, self.roll);
        self.clamp();
    }

    /// Sets the curved cylinder curvature radius and central arc angle, clamped to comfortable ranges.
    pub fn set_curve(&mut self, radius: f32, central_angle_rad: f32) {
        self.curve_radius = clamp_curve_radius(radius);
        self.curve_angle = clamp_curve_angle(central_angle_rad);
    }

    /// Sets the flat quad physical dimensions in meters, clamped to comfortable ranges.
    pub fn set_quad_size(&mut self, width: f32, height: f32) {
        self.quad_size = clamp_screen_size(width, height);
    }

    /// Uniformly scales the screen relative to `base` by `factor`, preserving aspect ratio.
    ///
    /// Scales the size used by the active display mode: the flat quad width and height, or the
    /// curved cylinder radius (which scales the arc width and height at a fixed central angle).
    /// The factor is clamped to the size limits. Returns the factor actually applied.
    pub fn set_uniform_scale_from(&mut self, base: &ScreenTransform, factor: f32) -> f32 {
        let applied = match base.display_mode {
            DisplayMode::FlatQuad => {
                let applied = clamp_uniform_scale(factor, base.quad_size.0, base.quad_size.1);
                self.quad_size = (base.quad_size.0 * applied, base.quad_size.1 * applied);
                applied
            }
            // The cylinder is centred on the user, so a larger radius only moves the screen
            // away at the same apparent size. Widen the arc instead (the height follows from
            // the aspect ratio).
            DisplayMode::CurvedCylinder => {
                let angle = clamp_curve_angle(base.curve_angle * factor);
                self.curve_angle = angle;
                angle / base.curve_angle.max(f32::EPSILON)
            }
        };
        self.clamp();
        applied
    }

    /// Sets the cylinder aspect ratio, clamped to comfortable ranges.
    pub fn set_aspect_ratio(&mut self, aspect: f32) {
        self.aspect_ratio = clamp_aspect_ratio(aspect);
    }

    /// Returns the 6DoF `ScreenPose`.
    pub fn pose(&self) -> ScreenPose {
        ScreenPose::new(self.position, self.orientation)
    }

    /// Axis of the curved screen: one radius behind the screen centre (`position`), towards
    /// the viewer. The arc's middle is then exactly at `position`, whatever the radius, so the
    /// curvature can change without moving the screen. Used for both the compositor layer and
    /// the hit test, so what is drawn and what the pointer hits always agree.
    /// Width and height of the visible screen in meters.
    pub fn size_m(&self) -> (f32, f32) {
        match self.display_mode {
            DisplayMode::FlatQuad => self.quad_size,
            DisplayMode::CurvedCylinder => {
                let w = self.curve_radius * self.curve_angle;
                (w, w / self.aspect_ratio.max(f32::EPSILON))
            }
        }
    }

    /// Pose on the screen surface at texture coordinates `(u, v)` (u = 0 left, v = 0 top),
    /// facing the viewer like the screen does there. Values outside [0, 1] extend the
    /// surface, for handles just past an edge.
    pub fn surface_pose(&self, u: f32, v: f32) -> ScreenPose {
        let (w, h) = self.size_m();
        let up = self.orientation.rotate_vec(Vec3::new(0.0, 1.0, 0.0));
        let lift = up * ((0.5 - v) * h);
        match self.display_mode {
            DisplayMode::FlatQuad => {
                let right = self.orientation.rotate_vec(Vec3::new(1.0, 0.0, 0.0));
                ScreenPose::new(
                    self.position + right * ((u - 0.5) * w) + lift,
                    self.orientation,
                )
            }
            DisplayMode::CurvedCylinder => {
                let phi = (u - 0.5) * self.curve_angle;
                let local = Vec3::new(
                    self.curve_radius * phi.sin(),
                    0.0,
                    -self.curve_radius * phi.cos(),
                );
                let axis = self.cylinder_axis_pose();
                let turn = Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), -phi);
                ScreenPose::new(
                    axis.position + axis.orientation.rotate_vec(local) + lift,
                    self.orientation * turn,
                )
            }
        }
    }

    pub fn cylinder_axis_pose(&self) -> ScreenPose {
        let back = self
            .orientation
            .rotate_vec(Vec3::new(0.0, 0.0, self.curve_radius));
        ScreenPose::new(self.position + back, self.orientation)
    }

    /// Clamps all fields to safe, ergonomic ranges.
    pub fn clamp(&mut self) {
        self.distance = clamp_distance(self.distance);
        self.tilt = clamp_tilt(self.tilt);
        self.yaw = clamp_yaw(self.yaw);
        self.position.y = clamp_height(self.position.y);
        self.quad_size = clamp_screen_size(self.quad_size.0, self.quad_size.1);
        self.curve_radius = clamp_curve_radius(self.curve_radius);
        self.curve_angle = clamp_curve_angle(self.curve_angle);
        self.aspect_ratio = clamp_aspect_ratio(self.aspect_ratio);
    }

    /// Maps this transform into OpenXR `QuadLayerParams` for flat display rendering.
    pub fn to_quad_layer_params(&self) -> QuadLayerParams {
        let pose: openxr::Posef = self.pose().into();
        let size = openxr::Extent2Df {
            width: self.quad_size.0,
            height: self.quad_size.1,
        };
        QuadLayerParams { pose, size }
    }

    /// Maps this transform into OpenXR `CylinderLayerParams` for curved display rendering.
    pub fn to_cylinder_layer_params(&self) -> CylinderLayerParams {
        // The cylinder axis is positioned at the user origin with yaw and tilt applied
        let pose = openxr::Posef {
            orientation: self.orientation.into(),
            position: openxr::Vector3f {
                x: 0.0,
                y: self.position.y,
                z: 0.0,
            },
        };
        CylinderLayerParams {
            pose,
            radius: self.curve_radius,
            central_angle: self.curve_angle,
            aspect_ratio: self.aspect_ratio,
        }
    }

    /// Converts this transform into a `DesktopLayerConfig` compatible with OpenXR layer builders.
    pub fn to_desktop_layer_config(&self) -> DesktopLayerConfig {
        DesktopLayerConfig {
            mode: self.display_mode,
            quad_size: self.quad_size,
            distance: self.distance,
            cylinder_radius: self.curve_radius,
            cylinder_central_angle: self.curve_angle,
            cylinder_aspect_ratio: self.aspect_ratio,
            pose: Some(match self.display_mode {
                DisplayMode::CurvedCylinder => self.cylinder_axis_pose().into(),
                DisplayMode::FlatQuad => self.pose().into(),
            }),
        }
    }

    /// Creates a `ScreenTransform` initialized from an existing `DesktopLayerConfig`.
    pub fn from_desktop_layer_config(config: &DesktopLayerConfig) -> Self {
        let mut transform = Self {
            distance: config.distance,
            quad_size: config.quad_size,
            curve_radius: config.cylinder_radius,
            curve_angle: config.cylinder_central_angle,
            aspect_ratio: config.cylinder_aspect_ratio,
            display_mode: config.mode,
            ..Default::default()
        };

        if let Some(pose) = config.pose {
            let screen_pose: ScreenPose = pose.into();
            transform.position = screen_pose.position;
            transform.orientation = screen_pose.orientation;
            let (pitch, yaw, roll) = screen_pose.orientation.to_euler();
            transform.tilt = pitch;
            transform.yaw = yaw;
            transform.roll = roll;
        } else {
            transform.update_position_from_distance_and_angles();
        }

        transform.clamp();
        transform
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screen_transform_defaults() {
        let transform = ScreenTransform::default();
        assert_eq!(transform.distance, DEFAULT_DISTANCE_M);
        assert_eq!(transform.tilt, 0.0);
        assert_eq!(transform.yaw, 0.0);
        assert_eq!(transform.position.z, -DEFAULT_DISTANCE_M);
        assert_eq!(transform.position.y, DEFAULT_HEIGHT_M);
        assert_eq!(transform.display_mode, DisplayMode::CurvedCylinder);
    }

    #[test]
    fn test_multi_display_positioning() {
        let primary = ScreenTransform::for_display(0);
        assert_eq!(primary.yaw, 0.0);
        assert_eq!(primary.position.x, 0.0);

        let secondary = ScreenTransform::for_display(1);
        assert!(secondary.yaw < 0.0);
        assert!(secondary.position.x > 0.0); // Placed to the right

        let tertiary = ScreenTransform::for_display(2);
        assert!(tertiary.yaw > 0.0);
        assert!(tertiary.position.x < 0.0); // Placed to the left
    }

    #[test]
    fn test_push_pull_and_clamping() {
        let mut transform = ScreenTransform::default();
        transform.push_pull(0.5);
        assert!((transform.distance - 1.7).abs() < 1e-4);
        assert!((transform.position.z - (-1.7)).abs() < 1e-4);

        // Push too far: clamped to MAX_DISTANCE_M
        transform.push_pull(10.0);
        assert_eq!(transform.distance, MAX_DISTANCE_M);

        // Pull too close: clamped to MIN_DISTANCE_M
        transform.push_pull(-10.0);
        assert_eq!(transform.distance, MIN_DISTANCE_M);
    }

    #[test]
    fn test_set_tilt_and_yaw_clamping() {
        let mut transform = ScreenTransform::default();

        transform.set_tilt(2.0); // Greater than MAX_TILT_RAD
        assert_eq!(transform.tilt, MAX_TILT_RAD);

        transform.set_tilt(-2.0);
        assert_eq!(transform.tilt, MIN_TILT_RAD);

        transform.set_yaw(5.0); // Greater than MAX_YAW_RAD
        assert_eq!(transform.yaw, MAX_YAW_RAD);
    }

    #[test]
    fn test_to_layer_params() {
        let transform = ScreenTransform::default();
        let quad_params = transform.to_quad_layer_params();
        assert_eq!(quad_params.size.width, 1.6);
        assert_eq!(quad_params.size.height, 0.9);
        assert_eq!(quad_params.pose.position.z, -DEFAULT_DISTANCE_M);

        let cyl_params = transform.to_cylinder_layer_params();
        assert_eq!(cyl_params.radius, DEFAULT_CURVE_RADIUS_M);
        assert_eq!(cyl_params.central_angle, DEFAULT_CURVE_ANGLE_RAD);
        assert!((cyl_params.aspect_ratio - (16.0 / 9.0)).abs() < 1e-4);
    }

    #[test]
    fn test_desktop_layer_config_conversion() {
        let mut transform = ScreenTransform::default();
        transform.set_distance(1.8);
        transform.set_curve(2.0, 1.5);

        let config = transform.to_desktop_layer_config();
        assert_eq!(config.distance, 1.8);
        assert_eq!(config.cylinder_radius, 2.0);
        assert_eq!(config.cylinder_central_angle, 1.5);
        assert!(config.pose.is_some());

        let restored = ScreenTransform::from_desktop_layer_config(&config);
        assert_eq!(restored.distance, 1.8);
        assert_eq!(restored.curve_radius, 2.0);
        assert_eq!(restored.curve_angle, 1.5);
    }
}
