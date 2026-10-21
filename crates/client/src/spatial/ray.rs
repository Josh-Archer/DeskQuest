//! 3D pointing rays and ray intersection algorithms for flat quad and curved cylinder screens.

use crate::config::DisplayMode;
use crate::spatial::limits::WINDOW_MARGIN_FRAC;
use crate::spatial::pose::{ScreenPose, Vec3};
use crate::spatial::transform::ScreenTransform;

/// 3D pointing ray with an origin point and normalized direction vector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    /// Origin point in reference space (meters).
    pub origin: Vec3,
    /// Normalized direction vector.
    pub direction: Vec3,
}

impl Ray {
    /// Creates a new ray, normalizing the direction vector.
    pub fn new(origin: Vec3, direction: Vec3) -> Self {
        Self {
            origin,
            direction: direction.normalize(),
        }
    }

    /// Constructs a pointing ray from an OpenXR 6DoF aim pose.
    ///
    /// In OpenXR coordinate conventions:
    /// - `+X` is right
    /// - `+Y` is up
    /// - `-Z` is forward (the pointing direction of controller and hand aim poses)
    pub fn from_aim_pose(pose: &ScreenPose) -> Self {
        let forward = pose.orientation.rotate_vec(-Vec3::UNIT_Z).normalize();
        Self {
            origin: pose.position,
            direction: forward,
        }
    }

    /// Computes the 3D point along the ray at distance `t` in meters.
    #[inline]
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.direction * t
    }
}

/// Intersection record of a ray hitting a virtual screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    /// Distance in meters from ray origin to intersection point along ray direction.
    pub distance: f32,

    /// 3D intersection point in world/stage reference space.
    pub point: Vec3,

    /// Surface normal vector at the intersection point, oriented towards the ray origin.
    pub normal: Vec3,

    /// Normalized 2D coordinates on the screen surface:
    /// - `u`: horizontal coordinate in `[0.0, 1.0]` (0.0 = left edge, 1.0 = right edge)
    /// - `v`: vertical coordinate in `[0.0, 1.0]` (0.0 = top edge, 1.0 = bottom edge)
    pub uv: (f32, f32),
}

/// Computes the intersection of a ray with a flat rectangular quad screen.
///
/// # Arguments
/// * `ray` - Pointing ray in stage space.
/// * `quad_pose` - 6DoF center pose of the quad.
/// * `size` - Physical width and height of the quad in meters `(width, height)`.
pub fn ray_intersects_quad(ray: &Ray, quad_pose: &ScreenPose, size: (f32, f32)) -> Option<RayHit> {
    let (width, height) = size;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    // Transform ray to quad local coordinate space where quad center is at origin,
    // quad normal is local +Z (facing user), local X is right, local Y is up.
    let inv_orient = quad_pose.orientation.conjugate();
    let local_origin = inv_orient.rotate_vec(ray.origin - quad_pose.position);
    let local_dir = inv_orient.rotate_vec(ray.direction);

    // Quad plane is local Z = 0
    if local_dir.z.abs() < 1e-6 {
        return None; // Ray is parallel to the quad plane
    }

    // local_origin.z + t * local_dir.z = 0
    let t = -local_origin.z / local_dir.z;
    if t <= 1e-4 {
        return None; // Intersection is behind ray origin
    }

    let local_hit_x = local_origin.x + t * local_dir.x;
    let local_hit_y = local_origin.y + t * local_dir.y;

    let half_w = width * 0.5;
    let half_h = height * 0.5;

    if local_hit_x.abs() > half_w || local_hit_y.abs() > half_h {
        return None; // Outside quad bounds
    }

    let u = ((local_hit_x + half_w) / width).clamp(0.0, 1.0);
    let v = (1.0 - (local_hit_y + half_h) / height).clamp(0.0, 1.0);

    // Normal points towards ray origin
    let local_normal = if local_origin.z >= 0.0 {
        Vec3::UNIT_Z
    } else {
        -Vec3::UNIT_Z
    };
    let world_normal = quad_pose.orientation.rotate_vec(local_normal).normalize();

    Some(RayHit {
        distance: t,
        point: ray.at(t),
        normal: world_normal,
        uv: (u, v),
    })
}

/// Computes the intersection of a ray with a curved cylinder screen layer.
///
/// # Arguments
/// * `ray` - Pointing ray in stage space.
/// * `axis_pose` - 6DoF pose of the cylinder axis in stage space (Y is cylinder axis).
/// * `radius` - Radius of curvature in meters.
/// * `central_angle` - Central arc angle in radians.
/// * `aspect_ratio` - Screen aspect ratio (`arc_width / height`).
pub fn ray_intersects_cylinder(
    ray: &Ray,
    axis_pose: &ScreenPose,
    radius: f32,
    central_angle: f32,
    aspect_ratio: f32,
) -> Option<RayHit> {
    if radius <= 0.0 || central_angle <= 0.0 || aspect_ratio <= 0.0 {
        return None;
    }

    let arc_width = radius * central_angle;
    let height = arc_width / aspect_ratio;
    let half_height = height * 0.5;
    let half_angle = central_angle * 0.5;

    // Transform ray to cylinder local space where cylinder axis is along local Y.
    // The screen section is centered at local (x=0, z=-radius) spanning [-half_angle, +half_angle].
    let inv_orient = axis_pose.orientation.conjugate();
    let local_origin = inv_orient.rotate_vec(ray.origin - axis_pose.position);
    let local_dir = inv_orient.rotate_vec(ray.direction);

    // Solve intersection with infinite cylinder: x^2 + z^2 = R^2
    let a = local_dir.x * local_dir.x + local_dir.z * local_dir.z;
    if a < 1e-8 {
        return None; // Ray is parallel to cylinder axis
    }

    let b = 2.0 * (local_origin.x * local_dir.x + local_origin.z * local_dir.z);
    let c = local_origin.x * local_origin.x + local_origin.z * local_origin.z - radius * radius;

    let discriminant = b * b - 4.0 * a * c;
    if discriminant < 0.0 {
        return None; // No intersection with cylinder
    }

    let sqrt_disc = discriminant.sqrt();
    let t1 = (-b - sqrt_disc) / (2.0 * a);
    let t2 = (-b + sqrt_disc) / (2.0 * a);

    // Check candidate roots in ascending order
    let check_hit = |t: f32| -> Option<RayHit> {
        if t <= 1e-4 {
            return None;
        }

        let local_hit_y = local_origin.y + t * local_dir.y;
        if local_hit_y.abs() > half_height {
            return None; // Exceeds vertical height
        }

        let local_hit_x = local_origin.x + t * local_dir.x;
        let local_hit_z = local_origin.z + t * local_dir.z;

        // Front arc check: the screen is placed in the -Z half of the cylinder
        // angle phi = atan2(x, -z), with phi in [-half_angle, +half_angle]
        if -local_hit_z <= 0.0 {
            return None; // Hit is on back half of cylinder
        }

        let phi = local_hit_x.atan2(-local_hit_z);
        if phi.abs() > half_angle {
            return None; // Outside angular arc span
        }

        let u = ((phi + half_angle) / central_angle).clamp(0.0, 1.0);
        let v = (1.0 - (local_hit_y + half_height) / height).clamp(0.0, 1.0);

        // Surface normal: on inside surface facing user at origin, normal points inwards (-x, 0, -z)
        let is_inside = local_origin.x * local_origin.x + local_origin.z * local_origin.z
            < radius * radius + 1e-4;
        let local_normal = if is_inside {
            Vec3::new(-local_hit_x, 0.0, -local_hit_z).normalize()
        } else {
            Vec3::new(local_hit_x, 0.0, local_hit_z).normalize()
        };
        let world_normal = axis_pose.orientation.rotate_vec(local_normal).normalize();

        Some(RayHit {
            distance: t,
            point: ray.at(t),
            normal: world_normal,
            uv: (u, v),
        })
    };

    if let Some(hit) = check_hit(t1) {
        Some(hit)
    } else {
        check_hit(t2)
    }
}

impl ScreenTransform {
    /// Tests whether a pointing ray intersects this spatial screen, taking into account
    /// flat quad vs curved cylinder projection mode and 6DoF orientation.
    pub fn intersect_ray(&self, ray: &Ray) -> Option<RayHit> {
        match self.display_mode {
            DisplayMode::FlatQuad => {
                let quad_pose = self.pose();
                ray_intersects_quad(ray, &quad_pose, self.quad_size)
            }
            DisplayMode::CurvedCylinder => {
                let axis_pose = self.cylinder_axis_pose();
                ray_intersects_cylinder(
                    ray,
                    &axis_pose,
                    self.curve_radius,
                    self.curve_angle,
                    self.aspect_ratio,
                )
            }
        }
    }

    /// Like [`Self::intersect_ray`], for the screen's whole window: it reaches
    /// [`WINDOW_MARGIN_FRAC`] of the width past every edge, where the grab bar, edge handles and
    /// resize arcs are. A hit in that margin has a `uv` outside `[0, 1]`.
    pub fn intersect_window(&self, ray: &Ray) -> Option<RayHit> {
        let (w, h) = self.size_m();
        let margin = WINDOW_MARGIN_FRAC * w;
        let mut hit = self.with_margin(margin).intersect_ray(ray)?;
        let (u, v) = hit.uv;
        hit.uv = (
            (u * (w + 2.0 * margin) - margin) / w,
            (v * (h + 2.0 * margin) - margin) / h,
        );
        Some(hit)
    }

    /// Finds the closest window (screen plus its handle margin) the ray points at.
    pub fn find_closest_window_hit<'a>(
        ray: &Ray,
        screens: impl IntoIterator<Item = (u32, &'a ScreenTransform)>,
    ) -> Option<(u32, RayHit)> {
        closest(screens, |screen| screen.intersect_window(ray))
    }

    /// Finds the closest screen intersected by the pointing ray among a set of screens.
    pub fn find_closest_hit<'a>(
        ray: &Ray,
        screens: impl IntoIterator<Item = (u32, &'a ScreenTransform)>,
    ) -> Option<(u32, RayHit)> {
        closest(screens, |screen| screen.intersect_ray(ray))
    }
}

/// The nearest of `screens` that `hit` reports a hit on.
fn closest<'a>(
    screens: impl IntoIterator<Item = (u32, &'a ScreenTransform)>,
    hit: impl Fn(&ScreenTransform) -> Option<RayHit>,
) -> Option<(u32, RayHit)> {
    screens
        .into_iter()
        .filter_map(|(id, screen)| hit(screen).map(|h| (id, h)))
        .min_by(|a, b| a.1.distance.total_cmp(&b.1.distance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::pose::Quat;
    use std::f32::consts::FRAC_PI_2;

    #[test]
    fn test_ray_from_aim_pose() {
        let pose = ScreenPose::new(Vec3::new(0.1, 1.2, -0.3), Quat::from_euler(0.0, 0.0, 0.0));
        let ray = Ray::from_aim_pose(&pose);

        assert_eq!(ray.origin, Vec3::new(0.1, 1.2, -0.3));
        // Aim pose points in -Z direction
        assert!((ray.direction.z - (-1.0)).abs() < 1e-5);
        assert!(ray.direction.x.abs() < 1e-5);
        assert!(ray.direction.y.abs() < 1e-5);

        // Yaw 90 degrees CCW (pointing towards -X)
        let rotated_pose = ScreenPose::new(Vec3::ZERO, Quat::from_euler(0.0, FRAC_PI_2, 0.0));
        let rotated_ray = Ray::from_aim_pose(&rotated_pose);
        assert!((rotated_ray.direction.x - (-1.0)).abs() < 1e-5);
        assert!(rotated_ray.direction.z.abs() < 1e-5);
    }

    #[test]
    fn test_ray_intersects_quad_center_and_uv() {
        let quad_pose = ScreenPose::new(Vec3::new(0.0, 1.4, -1.5), Quat::IDENTITY);
        let size = (1.6, 0.9);

        // Ray from user origin aiming directly at quad center
        let ray = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.0, 0.0, -1.0));

        let hit = ray_intersects_quad(&ray, &quad_pose, size).expect("Should hit quad");
        assert!((hit.distance - 1.5).abs() < 1e-5);
        assert_eq!(hit.point, Vec3::new(0.0, 1.4, -1.5));
        assert!((hit.uv.0 - 0.5).abs() < 1e-5);
        assert!((hit.uv.1 - 0.5).abs() < 1e-5);

        // Ray aiming at top-left corner
        let corner_ray = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(-0.8, 0.45, -1.5));
        let corner_hit =
            ray_intersects_quad(&corner_ray, &quad_pose, size).expect("Should hit top-left corner");
        assert!((corner_hit.uv.0 - 0.0).abs() < 1e-4);
        assert!((corner_hit.uv.1 - 0.0).abs() < 1e-4);
    }

    #[test]
    fn test_ray_intersects_quad_misses() {
        let quad_pose = ScreenPose::new(Vec3::new(0.0, 1.4, -1.5), Quat::IDENTITY);
        let size = (1.6, 0.9);

        // Ray aiming to the right of the quad
        let ray_miss = Ray::new(
            Vec3::new(0.0, 1.4, 0.0),
            Vec3::new(1.0, 0.0, -1.5), // hits x = 1.0 > 0.8
        );
        assert!(ray_intersects_quad(&ray_miss, &quad_pose, size).is_none());

        // Ray pointing away from the quad (+Z)
        let ray_away = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.0, 0.0, 1.0));
        assert!(ray_intersects_quad(&ray_away, &quad_pose, size).is_none());
    }

    #[test]
    fn test_ray_intersects_cylinder() {
        let axis_pose = ScreenPose::new(Vec3::new(0.0, 1.4, 0.0), Quat::IDENTITY);
        let radius = 1.5;
        let central_angle = 1.2;
        let aspect_ratio = 16.0 / 9.0;

        // User at origin aiming directly forward at center of cylinder arc
        let ray = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.0, 0.0, -1.0));

        let hit = ray_intersects_cylinder(&ray, &axis_pose, radius, central_angle, aspect_ratio)
            .expect("Should hit cylinder center");
        assert!((hit.distance - 1.5).abs() < 1e-4);
        assert!((hit.point.z - (-1.5)).abs() < 1e-4);
        assert!((hit.uv.0 - 0.5).abs() < 1e-4);
        assert!((hit.uv.1 - 0.5).abs() < 1e-4);

        // Ray aiming outside angular arc
        let ray_outside_arc = Ray::new(
            Vec3::new(0.0, 1.4, 0.0),
            Vec3::new(1.5, 0.0, 0.0), // 90 degrees sideways
        );
        assert!(ray_intersects_cylinder(
            &ray_outside_arc,
            &axis_pose,
            radius,
            central_angle,
            aspect_ratio
        )
        .is_none());
    }

    #[test]
    fn test_screen_transform_closest_hit() {
        let mut screen0 = ScreenTransform::for_display(0);
        screen0.display_mode = DisplayMode::FlatQuad;
        screen0.set_distance(1.2);

        let mut screen1 = ScreenTransform::for_display(1);
        screen1.display_mode = DisplayMode::FlatQuad;

        let screens = vec![(0, &screen0), (1, &screen1)];

        // Ray aimed at center screen
        let ray = Ray::new(Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.0, 0.0, -1.0));
        let (id, hit) =
            ScreenTransform::find_closest_hit(&ray, screens.clone()).expect("Should hit screen 0");
        assert_eq!(id, 0);
        assert!((hit.distance - 1.2).abs() < 1e-4);
    }
}
