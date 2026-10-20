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

/// Grab bar texture size (a soft pill).
pub const BAR_TEXTURE: (u32, u32) = (256, 32);
/// Grab bar size in meters, and its gap below the screen's bottom edge.
pub const BAR_SIZE_M: (f32, f32) = (0.24, 0.02);
pub const BAR_GAP_M: f32 = 0.035;
/// Corner handle texture size (an L-shaped arc for the top-left corner, rotated for others).
pub const CORNER_TEXTURE: (u32, u32) = (64, 64);
/// Corner handle size in meters.
pub const CORNER_SIZE_M: f32 = 0.07;

/// A handle (grab bar or corner) to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandleVisual {
    pub pose: ScreenPose,
    pub size: (f32, f32),
    /// Brighter: hovered or in use.
    pub active: bool,
}

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
    /// Grab bar under the screen, shown only when pointing near the bottom edge or moving it,
    /// like Horizon's own windows.
    pub bar: Option<HandleVisual>,
    /// Resize handle at the hovered (or dragged) corner.
    pub corner: Option<HandleVisual>,
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
            // Mostly clear, like Horizon's own window frames: a faint fill plus a thin, soft
            // highlight along the outer edge.
            let rim = 1.0 - smoothstep(0.0, 0.035, -outside);
            put(
                &mut px,
                w,
                x,
                y,
                [240, 244, 255],
                (0.08 + 0.22 * rim) * outer * ring,
            );
        }
    }
    px
}

/// Horizon-style grab bar: a soft white pill. `alpha` is the opacity of its body.
pub fn bar_rgba(alpha: f32) -> Vec<u8> {
    let (w, h) = BAR_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let r = h as f32 / 2.0;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance to the pill's centre segment, minus its radius.
            let cx = fx.clamp(r, w as f32 - r);
            let d = ((fx - cx).powi(2) + (fy - r).powi(2)).sqrt() - (r - 1.5);
            put(
                &mut px,
                w,
                x,
                y,
                [255, 255, 255],
                alpha * (1.0 - smoothstep(-1.5, 1.0, d)),
            );
        }
    }
    px
}

/// Corner handle for the top-left corner: a soft quarter-round "L" hugging the corner.
pub fn corner_rgba(alpha: f32) -> Vec<u8> {
    let (w, h) = CORNER_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let s = w as f32;
    let (outer, thickness) = (0.62 * s, 0.13 * s);
    for y in 0..h {
        for x in 0..w {
            // Arc centred at (outer, outer) from the corner, covering the top-left quadrant.
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = (outer - fx, outer - fy);
            let in_quadrant = dx >= -1.0 && dy >= -1.0;
            let dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
            let band = (dist - (outer - thickness * 0.5)).abs() - thickness * 0.5;
            let a = if in_quadrant {
                1.0 - smoothstep(-1.0, 1.5, band)
            } else {
                0.0
            };
            put(&mut px, w, x, y, [255, 255, 255], alpha * a);
        }
    }
    px
}

/// Fraction of the screen, from the bottom edge, where pointing reveals the grab bar.
pub const BAR_REVEAL_ZONE: f32 = 0.3;
/// Fraction of width/height around a corner where pointing reveals its resize handle.
pub const CORNER_REVEAL_ZONE: f32 = crate::spatial::CORNER_ZONE;

/// The corner `(u, v)` (each 0 or 1) a hover at `uv` is near, if any.
pub fn corner_near(uv: (f32, f32)) -> Option<(f32, f32)> {
    let pick = |t: f32| {
        if t <= CORNER_REVEAL_ZONE {
            Some(0.0)
        } else if t >= 1.0 - CORNER_REVEAL_ZONE {
            Some(1.0)
        } else {
            None
        }
    };
    Some((pick(uv.0)?, pick(uv.1)?))
}

/// Horizon-style window handles for `screen`: nothing by default, the grab bar when pointing
/// at the lower part of the screen or while moving it, a corner handle when pointing near a
/// corner or while resizing from it. `hover_uv` is where a pointer hits the screen.
pub fn window_handles(
    screen: &crate::spatial::ScreenTransform,
    hover_uv: Option<(f32, f32)>,
    moving: bool,
    resizing_corner: Option<(f32, f32)>,
) -> (Option<HandleVisual>, Option<HandleVisual>) {
    let bar = (moving || hover_uv.is_some_and(|uv| uv.1 >= 1.0 - BAR_REVEAL_ZONE)).then(|| {
        let edge = screen.surface_pose(0.5, 1.0);
        let up = edge.orientation.rotate_vec(Vec3::new(0.0, 1.0, 0.0));
        let drop = BAR_GAP_M + BAR_SIZE_M.1 * 0.5;
        HandleVisual {
            pose: ScreenPose::new(edge.position - up * drop, edge.orientation),
            size: BAR_SIZE_M,
            active: moving,
        }
    });
    let corner = resizing_corner
        .map(|c| (c, true))
        .or_else(|| hover_uv.and_then(corner_near).map(|c| (c, false)))
        .map(|((cu, cv), active)| {
            let at = screen.surface_pose(cu, cv);
            // The texture draws the top-left corner; turn it about the screen normal.
            let roll = match (cu > 0.5, cv > 0.5) {
                (false, false) => 0.0,
                (true, false) => -std::f32::consts::FRAC_PI_2,
                (true, true) => std::f32::consts::PI,
                (false, true) => std::f32::consts::FRAC_PI_2,
            };
            let normal = at.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
            let spin = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), roll);
            HandleVisual {
                pose: ScreenPose::new(at.position + normal * 0.002, at.orientation * spin),
                size: (CORNER_SIZE_M, CORNER_SIZE_M),
                active,
            }
        });
    (bar, corner)
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
    fn handles_stay_hidden_until_pointing_near_an_edge_or_corner() {
        let screen = crate::spatial::ScreenTransform::default();
        assert_eq!(window_handles(&screen, None, false, None), (None, None));
        let (bar, corner) = window_handles(&screen, Some((0.5, 0.4)), false, None);
        assert!(
            bar.is_none() && corner.is_none(),
            "middle of the screen shows nothing"
        );
        let (bar, corner) = window_handles(&screen, Some((0.5, 0.9)), false, None);
        assert!(bar.is_some() && corner.is_none());
        let (bar, _) = window_handles(&screen, None, true, None);
        assert!(bar.unwrap().active, "the bar shows, brighter, while moving");
        let (_, corner) = window_handles(&screen, Some((0.95, 0.05)), false, None);
        assert!(corner.is_some_and(|c| !c.active));
        let (_, corner) = window_handles(&screen, None, false, Some((1.0, 1.0)));
        assert!(corner.unwrap().active);
    }

    #[test]
    fn the_bar_sits_below_the_bottom_edge_and_corners_at_the_corners() {
        let screen = crate::spatial::ScreenTransform::default();
        let (_, h) = screen.size_m();
        let (bar, _) = window_handles(&screen, None, true, None);
        let bar = bar.unwrap();
        assert!(bar.pose.position.y < screen.position.y - h * 0.5);
        let (_, corner) = window_handles(&screen, None, false, Some((0.0, 0.0)));
        let corner = corner.unwrap();
        assert!(corner.pose.position.y > screen.position.y + h * 0.49);
        assert!(corner.pose.position.x < screen.position.x);
        assert_eq!(corner_near((0.5, 0.05)), None);
        assert_eq!(corner_near((0.03, 0.97)), Some((0.0, 1.0)));
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
        let bar = bar_rgba(0.5);
        let (bw, bh) = BAR_TEXTURE;
        let mid = (((bh / 2) * bw + bw / 2) * 4 + 3) as usize;
        assert!(bar[mid] > 100, "bar body is visible");
        assert_eq!(bar[3], 0, "bar corner is transparent");
        let corner = corner_rgba(0.6);
        let (kw, _) = CORNER_TEXTURE;
        assert_eq!(
            corner[((63 * kw + 63) * 4 + 3) as usize],
            0,
            "far corner is empty"
        );
        let border = border_rgba((0.9, 0.85));
        let (bw, bh) = BORDER_TEXTURE;
        let ring = (((bh / 2) * bw + (bw as f32 * 0.02) as u32) * 4 + 3) as usize;
        assert!(border[ring] > 15, "border ring is faintly visible");
        assert!(border[ring] < 120, "border stays mostly transparent");
    }
}
