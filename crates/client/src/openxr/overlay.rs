//! Pointer rays, cursors and the screen's soft border, drawn as extra compositor layers.
//!
//! The client renders nothing itself: the desktop is a MediaCodec surface composited by the
//! runtime. These visuals are small RGBA textures (built once here) placed every frame as quad
//! or cylinder layers, so a user can see where a controller or hand points and which screen it
//! is about to grab.

use crate::spatial::{Quat, ScreenPose, Vec3};

/// Ray texture size (width x height pixels). The ray runs along the texture's height.
pub const RAY_TEXTURE: (u32, u32) = (16, 256);
/// Cursor texture size.
pub const CURSOR_TEXTURE: (u32, u32) = (64, 64);
/// Border texture size; its aspect is stretched to the screen plus margin.
pub const BORDER_TEXTURE: (u32, u32) = (512, 288);

/// Ray width in meters.
pub const RAY_WIDTH_M: f32 = 0.006;
/// Length of a ray that hits nothing.
pub const RAY_MISS_LENGTH_M: f32 = 2.0;
/// Cursor diameter in meters.
pub const CURSOR_SIZE_M: f32 = 0.025;
/// Border margin around the screen on each side, in meters.
pub const BORDER_MARGIN_M: f32 = 0.06;

/// One pointer to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerVisual {
    pub origin: Vec3,
    /// Unit direction.
    pub direction: Vec3,
    /// Where the ray ends: the screen hit distance, or [`RAY_MISS_LENGTH_M`].
    pub length: f32,
    /// True when the ray hits a screen (a cursor is drawn at its end).
    pub hit: bool,
}

/// What to draw over the scene this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayScene {
    pub pointers: Vec<PointerVisual>,
    /// Show the soft border: a pointer hovers or grabs the screen.
    pub border: bool,
    /// Brighter border while grabbing.
    pub border_active: bool,
}

fn put(px: &mut [u8], w: u32, x: u32, y: u32, rgb: [u8; 3], a: f32) {
    let i = ((y * w + x) * 4) as usize;
    let a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    px[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A white ray with soft side edges, fading out towards its far end (texture row 0 is the
/// origin end).
pub fn ray_rgba() -> Vec<u8> {
    let (w, h) = RAY_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let along = 1.0 - 0.6 * (y as f32 / (h - 1) as f32);
        for x in 0..w {
            let across = ((x as f32 + 0.5) / w as f32 - 0.5).abs() * 2.0;
            let edge = 1.0 - smoothstep(0.35, 1.0, across);
            put(&mut px, w, x, y, [255, 255, 255], 0.85 * along * edge);
        }
    }
    px
}

/// A white dot with a soft rim.
pub fn cursor_rgba() -> Vec<u8> {
    let (w, h) = CURSOR_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let dx = (x as f32 + 0.5) / w as f32 - 0.5;
            let dy = (y as f32 + 0.5) / h as f32 - 0.5;
            let r = (dx * dx + dy * dy).sqrt() * 2.0;
            put(
                &mut px,
                w,
                x,
                y,
                [255, 255, 255],
                1.0 - smoothstep(0.55, 1.0, r),
            );
        }
    }
    px
}

/// A soft rounded-rectangle frame: translucent white, transparent inside the screen area.
/// `inner` is the screen's share of the texture on each axis (the rest is margin).
pub fn border_rgba(inner: (f32, f32)) -> Vec<u8> {
    let (w, h) = BORDER_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    // Half extents in normalised [-1, 1] texture coordinates.
    let (ix, iy) = (inner.0.clamp(0.1, 0.99), inner.1.clamp(0.1, 0.99));
    let radius = 0.08;
    for y in 0..h {
        for x in 0..w {
            let u = ((x as f32 + 0.5) / w as f32) * 2.0 - 1.0;
            let v = ((y as f32 + 0.5) / h as f32) * 2.0 - 1.0;
            // Signed distance to a rounded rectangle filling the texture.
            let qx = u.abs() - (1.0 - radius);
            let qy = v.abs() - (1.0 - radius);
            let outside =
                (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - radius;
            let outer = 1.0 - smoothstep(-0.03, 0.0, outside);
            // Fully transparent over the screen itself, soft at its edge.
            let in_screen = (u.abs() / ix).max(v.abs() / iy);
            let ring = smoothstep(0.97, 1.0, in_screen);
            put(&mut px, w, x, y, [235, 240, 255], 0.55 * outer * ring);
        }
    }
    px
}

/// Rotation whose local X, Y, Z axes map to the given orthonormal world axes.
pub fn quat_from_basis(x: Vec3, y: Vec3, z: Vec3) -> Quat {
    // Standard rotation-matrix to quaternion; columns are x, y, z.
    let (m00, m01, m02) = (x.x, y.x, z.x);
    let (m10, m11, m12) = (x.y, y.y, z.y);
    let (m20, m21, m22) = (x.z, y.z, z.z);
    let trace = m00 + m11 + m22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        Quat {
            w: 0.25 * s,
            x: (m21 - m12) / s,
            y: (m02 - m20) / s,
            z: (m10 - m01) / s,
        }
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        Quat {
            w: (m21 - m12) / s,
            x: 0.25 * s,
            y: (m01 + m10) / s,
            z: (m02 + m20) / s,
        }
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        Quat {
            w: (m02 - m20) / s,
            x: (m01 + m10) / s,
            y: 0.25 * s,
            z: (m12 + m21) / s,
        }
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        Quat {
            w: (m10 - m01) / s,
            x: (m02 + m20) / s,
            y: (m12 + m21) / s,
            z: 0.25 * s,
        }
    };
    q.normalize()
}

/// Pose of a quad drawing `p`'s ray: centred halfway along it, its local +Y along the ray, and
/// its face turned towards `eye` as far as the ray direction allows.
pub fn ray_quad_pose(p: &PointerVisual, eye: Vec3) -> ScreenPose {
    let y = p.direction.normalize();
    let centre = p.origin + y * (p.length * 0.5);
    let to_eye = eye - centre;
    let mut z = to_eye - y * to_eye.dot(y);
    if z.length() < 1e-4 {
        // Looking straight down the ray: any perpendicular works.
        let up = if y.y.abs() < 0.9 {
            Vec3::new(0.0, 1.0, 0.0)
        } else {
            Vec3::new(1.0, 0.0, 0.0)
        };
        z = up - y * up.dot(y);
    }
    let z = z.normalize();
    let x = y.cross(z).normalize();
    ScreenPose::new(centre, quat_from_basis(x, y, z))
}

/// Pose of the cursor at `p`'s end, facing back along the ray.
pub fn cursor_pose(p: &PointerVisual) -> ScreenPose {
    let z = (p.direction * -1.0).normalize();
    let up = if z.y.abs() < 0.9 {
        Vec3::new(0.0, 1.0, 0.0)
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    };
    let x = up.cross(z).normalize();
    let y = z.cross(x).normalize();
    // Lift it 2 mm off the screen so it never z-fights the desktop.
    let point = p.origin + p.direction.normalize() * (p.length - 0.002);
    ScreenPose::new(point, quat_from_basis(x, y, z))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn basis_round_trips_through_the_quaternion() {
        let x = Vec3::new(0.0, 0.0, -1.0);
        let y = Vec3::new(0.0, 1.0, 0.0);
        let z = Vec3::new(1.0, 0.0, 0.0);
        let q = quat_from_basis(x, y, z);
        assert!(approx(q.rotate_vec(Vec3::new(1.0, 0.0, 0.0)), x));
        assert!(approx(q.rotate_vec(Vec3::new(0.0, 1.0, 0.0)), y));
        assert!(approx(q.rotate_vec(Vec3::new(0.0, 0.0, 1.0)), z));
    }

    #[test]
    fn ray_quad_lies_along_the_ray_and_faces_the_eye() {
        let p = PointerVisual {
            origin: Vec3::new(0.2, 1.2, -0.3),
            direction: Vec3::new(0.0, 0.0, -1.0),
            length: 1.0,
            hit: true,
        };
        let eye = Vec3::new(0.0, 1.6, 0.0);
        let pose = ray_quad_pose(&p, eye);
        assert!(approx(pose.position, Vec3::new(0.2, 1.2, -0.8)));
        let y = pose.orientation.rotate_vec(Vec3::new(0.0, 1.0, 0.0));
        assert!(approx(y, p.direction));
        let z = pose.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        assert!(z.dot(eye - pose.position) > 0.0, "quad must face the eye");
        assert!(z.dot(y).abs() < 1e-4);
    }

    #[test]
    fn cursor_sits_at_the_ray_end_and_faces_the_origin() {
        let p = PointerVisual {
            origin: Vec3::new(0.0, 1.0, 0.0),
            direction: Vec3::new(0.0, 0.0, -1.0),
            length: 1.5,
            hit: true,
        };
        let pose = cursor_pose(&p);
        assert!((pose.position.z - (-1.498)).abs() < 1e-4);
        let z = pose.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        assert!(approx(z, Vec3::new(0.0, 0.0, 1.0)));
    }

    #[test]
    fn textures_have_the_declared_sizes_and_transparent_centres() {
        assert_eq!(
            ray_rgba().len(),
            (RAY_TEXTURE.0 * RAY_TEXTURE.1 * 4) as usize
        );
        let cursor = cursor_rgba();
        let (cw, ch) = CURSOR_TEXTURE;
        let centre = (((ch / 2) * cw + cw / 2) * 4 + 3) as usize;
        assert!(cursor[centre] > 200, "cursor centre is opaque");
        assert_eq!(cursor[3], 0, "cursor corner is transparent");
        let border = border_rgba((0.9, 0.85));
        let (bw, bh) = BORDER_TEXTURE;
        let mid = (((bh / 2) * bw + bw / 2) * 4 + 3) as usize;
        assert_eq!(border[mid], 0, "border is transparent over the screen");
        let ring = (((bh / 2) * bw + (bw as f32 * 0.02) as u32) * 4 + 3) as usize;
        assert!(border[ring] > 60, "border ring is visible");
    }
}
