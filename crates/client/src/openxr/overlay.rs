//! Pointer rays, cursors and Horizon-style window chrome, drawn as extra compositor layers.
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
/// Rounded-rectangle mask that gives each screen Horizon's rounded corners: it sets the
/// destination alpha the desktop layer is then blended by. Stretched to the screen, whose
/// aspect it shares.
pub const MASK_TEXTURE: (u32, u32) = (512, 288);

/// Ray width in meters.
pub const RAY_WIDTH_M: f32 = 0.006;
/// Length of a ray that hits nothing.
pub const RAY_MISS_LENGTH_M: f32 = 2.0;
/// Cursor diameter in meters.
pub const CURSOR_SIZE_M: f32 = 0.025;

// Window chrome, after Horizon OS's own windows: rounded corners and nothing else until a
// pointer comes near an edge, then a thin white grab pill under the bottom edge, a soft
// glowing line just outside a side or the top, or a short arc around a corner. Sizes are
// fractions of the screen's width, so a resized screen keeps its proportions.

/// Corner radius of a screen, as a fraction of its width (about 3% on Horizon's windows).
pub const CORNER_RADIUS_FRAC: f32 = 0.03;

/// Grab pill texture; its aspect is the pill's.
pub const BAR_TEXTURE: (u32, u32) = (320, 16);
/// Grab pill width, and its gap below the bottom edge, as fractions of the screen's width.
pub const BAR_WIDTH_FRAC: f32 = 0.14;
pub const BAR_GAP_FRAC: f32 = 0.03;

/// Edge handle texture: a vertical glowing line, turned for the top edge.
pub const EDGE_TEXTURE: (u32, u32) = (32, 256);
/// Edge handle length as a fraction of the screen's height; its width (glow included) and how
/// far outside the edge it sits, as fractions of the screen's width.
pub const EDGE_LENGTH_FRAC: f32 = 0.35;
pub const EDGE_WIDTH_FRAC: f32 = 0.03;
pub const EDGE_GAP_FRAC: f32 = 0.04;

/// Resize handle texture: an arc around the top-left corner, turned for the others.
pub const CORNER_TEXTURE: (u32, u32) = (128, 128);
/// Gap between a rounded corner and its arc, and the arc's thickness, as fractions of the
/// screen's width.
pub const ARC_GAP_FRAC: f32 = 0.022;
pub const ARC_THICKNESS_FRAC: f32 = 0.006;
/// Half the angle the arc spans around the corner.
pub const ARC_HALF_SPAN_RAD: f32 = 0.55;

/// How far the soft frame reaches past the window's edge, as a fraction of its width.
pub const FRAME_MARGIN_FRAC: f32 = 0.04;
/// Frame texture width; its height follows the grown window's aspect.
pub const FRAME_TEXTURE_WIDTH: u32 = 640;

/// Frame texture size for windows of `aspect` (width / height).
pub fn frame_texture_size(aspect: f32) -> (u32, u32) {
    let grown_w = 1.0 + 2.0 * FRAME_MARGIN_FRAC;
    let grown_h = 1.0 / aspect.max(0.1) + 2.0 * FRAME_MARGIN_FRAC;
    let h = (FRAME_TEXTURE_WIDTH as f32 * grown_h / grown_w).round() as u32;
    (FRAME_TEXTURE_WIDTH, h.max(1))
}

/// The soft frame for windows of `aspect`: clear over the window itself, a thin bright rim
/// hugging its rounded edge, and a soft glow fading out over the margin. Lengths are
/// fractions of the window's width.
pub fn frame_rgba(aspect: f32) -> Vec<u8> {
    let (w, h) = frame_texture_size(aspect);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let grown_w = 1.0 + 2.0 * FRAME_MARGIN_FRAC;
    let per_px = grown_w / w as f32;
    let (half_w, half_h) = (0.5, 0.5 / aspect.max(0.1));
    let r = CORNER_RADIUS_FRAC;
    for y in 0..h {
        for x in 0..w {
            let dx = ((x as f32 + 0.5) - w as f32 / 2.0) * per_px;
            let dy = ((y as f32 + 0.5) - h as f32 / 2.0) * per_px;
            let qx = dx.abs() - (half_w - r);
            let qy = dy.abs() - (half_h - r);
            // Signed distance from the window's rounded edge, positive outside.
            let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
            if d < -per_px {
                continue;
            }
            let rim = 0.9 * (-(d / 0.0025).powi(2)).exp();
            let glow = 0.28 * (-d.max(0.0) / 0.010).exp();
            let fade = 1.0 - smoothstep(0.6 * FRAME_MARGIN_FRAC, FRAME_MARGIN_FRAC, d);
            let inside = smoothstep(-per_px, 0.0, d);
            put(
                &mut px,
                w,
                x,
                y,
                [240, 245, 255],
                rim.max(glow) * fade * inside,
            );
        }
    }
    px
}

/// Side of the resize handle's square texture, as a fraction of the screen's width.
pub fn corner_handle_frac() -> f32 {
    2.0 * (CORNER_RADIUS_FRAC + ARC_GAP_FRAC + ARC_THICKNESS_FRAC)
}

/// A window handle (grab pill, edge line or corner arc) to draw this frame.
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

/// The soft glowing outline around a window being moved or resized, as on Horizon OS. It is
/// drawn as a layer of the window's own shape (curved or flat), grown by
/// [`FRAME_MARGIN_FRAC`].
#[derive(Debug, Clone, PartialEq)]
pub struct FrameVisual {
    /// The window grown by the frame's margin; the frame texture covers exactly this.
    pub window: crate::spatial::ScreenTransform,
}

/// The window handles to draw for the screen in use: at most one at a time, like Horizon,
/// which shows the handle under the pointer, plus the frame while it is moved or resized.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowChrome {
    /// Soft outline around the window while it is moved or resized.
    pub frame: Option<FrameVisual>,
    /// Grab pill under the bottom edge: pointing near the bottom, or moving the window.
    pub bar: Option<HandleVisual>,
    /// Glowing line just outside the side or top edge being pointed at.
    pub edge: Option<HandleVisual>,
    /// Arc around the corner being pointed at or dragged.
    pub corner: Option<HandleVisual>,
}

/// What to draw over the scene this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayScene {
    pub pointers: Vec<PointerVisual>,
    pub chrome: WindowChrome,
}

/// Writes a pixel premultiplied by its alpha. The layers are blended with
/// `BLEND_TEXTURE_SOURCE_ALPHA` and without `UNPREMULTIPLIED_ALPHA`, so the compositor takes
/// colours as premultiplied: a white pixel with low or zero alpha would otherwise still add
/// full white, and every soft edge would show as a solid white block.
fn put(px: &mut [u8], w: u32, x: u32, y: u32, rgb: [u8; 3], a: f32) {
    let i = ((y * w + x) * 4) as usize;
    let a = a.clamp(0.0, 1.0);
    let scale = |c: u8| (f32::from(c) * a).round() as u8;
    px[i..i + 4].copy_from_slice(&[
        scale(rgb[0]),
        scale(rgb[1]),
        scale(rgb[2]),
        (a * 255.0).round() as u8,
    ]);
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

/// The screen mask: opaque inside a rounded rectangle filling the texture, clear in the
/// corners, with an antialiased edge.
pub fn mask_rgba() -> Vec<u8> {
    let (w, h) = MASK_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let r = CORNER_RADIUS_FRAC * w as f32;
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    // The shape reaches just past the texture so the straight edges stay fully opaque and
    // only the rounded corners are soft.
    let (hx, hy) = (cx + 0.75, cy + 0.75);
    for y in 0..h {
        for x in 0..w {
            let qx = ((x as f32 + 0.5) - cx).abs() - (hx - r);
            let qy = ((y as f32 + 0.5) - cy).abs() - (hy - r);
            let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
            // Only the alpha is used; premultiplied, so the colour is black.
            put(
                &mut px,
                w,
                x,
                y,
                [0, 0, 0],
                1.0 - smoothstep(-0.75, 0.75, sd),
            );
        }
    }
    px
}

/// The grab pill: a white capsule filling the texture. `alpha` is its opacity.
pub fn bar_rgba(alpha: f32) -> Vec<u8> {
    let (w, h) = BAR_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let r = h as f32 / 2.0;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let cx = fx.clamp(r, w as f32 - r);
            let d = ((fx - cx).powi(2) + (fy - r).powi(2)).sqrt() - (r - 1.0);
            put(
                &mut px,
                w,
                x,
                y,
                [255, 255, 255],
                alpha * (1.0 - smoothstep(-0.75, 0.75, d)),
            );
        }
    }
    px
}

/// An edge handle: a thin bright line in a soft glow, brightest in the middle and fading to
/// nothing at both ends. `alpha` is its peak opacity.
pub fn edge_rgba(alpha: f32) -> Vec<u8> {
    let (w, h) = EDGE_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let along = (std::f32::consts::PI * (y as f32 + 0.5) / h as f32)
            .sin()
            .powi(2);
        for x in 0..w {
            let across = ((x as f32 + 0.5) / w as f32 - 0.5).abs() * 2.0;
            let core = 1.0 - smoothstep(0.06, 0.16, across);
            let glow = 0.4 * (1.0 - across).max(0.0).powi(3);
            let a = alpha * along * (core + glow * (1.0 - core));
            put(&mut px, w, x, y, [235, 242, 255], a);
        }
    }
    px
}

/// A resize handle for the top-left corner: a short white arc with round ends, concentric
/// with the screen's rounded corner and just outside it. The texture's centre is the centre
/// of the corner's rounding. `alpha` is its opacity.
pub fn corner_rgba(alpha: f32) -> Vec<u8> {
    use std::f32::consts::{PI, TAU};
    let (w, h) = CORNER_TEXTURE;
    let mut px = vec![0u8; (w * h * 4) as usize];
    // Lengths are fractions of the screen's width.
    let side = corner_handle_frac();
    let radius = CORNER_RADIUS_FRAC + ARC_GAP_FRAC + ARC_THICKNESS_FRAC * 0.5;
    let half_thick = ARC_THICKNESS_FRAC * 0.5;
    let aa = 0.75 * side / w as f32;
    // Up and to the left (texture y grows downwards).
    let centre_angle = (-1.0f32).atan2(-1.0);
    for y in 0..h {
        for x in 0..w {
            let dx = ((x as f32 + 0.5) / w as f32 - 0.5) * side;
            let dy = ((y as f32 + 0.5) / h as f32 - 0.5) * side;
            let off = (dy.atan2(dx) - centre_angle + PI).rem_euclid(TAU) - PI;
            let dist = if off.abs() <= ARC_HALF_SPAN_RAD {
                ((dx * dx + dy * dy).sqrt() - radius).abs()
            } else {
                // Past an end of the arc: distance to its round cap.
                let a = centre_angle + off.signum() * ARC_HALF_SPAN_RAD;
                ((dx - radius * a.cos()).powi(2) + (dy - radius * a.sin()).powi(2)).sqrt()
            };
            let a = alpha * (1.0 - smoothstep(half_thick - aa, half_thick + aa, dist));
            put(&mut px, w, x, y, [255, 255, 255], a);
        }
    }
    px
}

/// Bottom band of the screen, as a fraction of its height, where pointing shows the grab
/// pill (as does pointing just below the screen).
pub const BAR_REVEAL_ZONE: f32 = 0.12;
/// Band along a side or the top, as a fraction of the screen, where pointing shows that
/// edge's handle (as does pointing just outside it).
pub const EDGE_REVEAL_ZONE: f32 = 0.06;
/// Fraction of width/height around a corner where pointing reveals its resize handle: the
/// same zone in which a grab resizes instead of moving.
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

/// A side or the top edge of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
}

/// The side or top edge a hover at `uv` is nearest to, if within its reveal band (the bottom
/// edge has the grab pill instead).
pub fn edge_near(uv: (f32, f32)) -> Option<Edge> {
    let (u, v) = uv;
    [
        (EDGE_REVEAL_ZONE - u, Edge::Left),
        (u - (1.0 - EDGE_REVEAL_ZONE), Edge::Right),
        (EDGE_REVEAL_ZONE - v, Edge::Top),
    ]
    .into_iter()
    .filter(|(depth, _)| *depth >= 0.0)
    .max_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, edge)| edge)
}

/// `pose` turned by `roll` about its own normal and lifted 2 mm towards the viewer, so a
/// handle on the screen never z-fights it.
fn on_surface(pose: ScreenPose, roll: f32) -> ScreenPose {
    let normal = pose.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
    let spin = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), roll);
    ScreenPose::new(pose.position + normal * 0.002, pose.orientation * spin)
}

/// Horizon-style chrome for `screen`. Nothing shows by default. Pointing near a corner shows
/// its arc; near the bottom (or just below it) the grab pill; near a side or the top that
/// edge's glowing line. Resizing shows the dragged corner's arc and moving shows the pill,
/// both bright; a handle also brightens while the pointer is on it, past the screen's edge.
/// `hover_uv` is where a pointer hits the window, outside `[0, 1]` in the margin around the
/// screen.
pub fn window_chrome(
    screen: &crate::spatial::ScreenTransform,
    hover_uv: Option<(f32, f32)>,
    moving: bool,
    resizing_corner: Option<(f32, f32)>,
) -> WindowChrome {
    use std::f32::consts::{FRAC_PI_2, PI};
    let (w, h) = screen.size_m();
    let hover = hover_uv.filter(|_| !moving && resizing_corner.is_none());
    let mut chrome = WindowChrome {
        frame: (moving || resizing_corner.is_some()).then(|| FrameVisual {
            window: screen.with_margin(FRAME_MARGIN_FRAC * w),
        }),
        ..WindowChrome::default()
    };

    let corner = resizing_corner
        .map(|c| (c, true))
        .or_else(|| hover.and_then(corner_near).map(|c| (c, false)));
    if let Some(((cu, cv), active)) = corner {
        // The centre of the corner's rounding: one radius in from both edges.
        let (iu, iv) = (CORNER_RADIUS_FRAC, CORNER_RADIUS_FRAC * w / h);
        let at = screen.surface_pose(
            if cu > 0.5 { 1.0 - iu } else { iu },
            if cv > 0.5 { 1.0 - iv } else { iv },
        );
        // The texture draws the top-left corner; turn it for the others.
        let roll = match (cu > 0.5, cv > 0.5) {
            (false, false) => 0.0,
            (true, false) => -FRAC_PI_2,
            (true, true) => PI,
            (false, true) => FRAC_PI_2,
        };
        let side = corner_handle_frac() * w;
        chrome.corner = Some(HandleVisual {
            pose: on_surface(at, roll),
            size: (side, side),
            active,
        });
        return chrome;
    }
    if resizing_corner.is_some() {
        return chrome;
    }

    if moving || hover.is_some_and(|(_, v)| v >= 1.0 - BAR_REVEAL_ZONE) {
        let edge = screen.surface_pose(0.5, 1.0);
        let up = edge.orientation.rotate_vec(Vec3::new(0.0, 1.0, 0.0));
        let size = (
            BAR_WIDTH_FRAC * w,
            BAR_WIDTH_FRAC * w * BAR_TEXTURE.1 as f32 / BAR_TEXTURE.0 as f32,
        );
        let drop = BAR_GAP_FRAC * w + size.1 * 0.5;
        let on_pill = hover.is_some_and(|(u, v)| v > 1.0 && (u - 0.5).abs() <= BAR_WIDTH_FRAC);
        chrome.bar = Some(HandleVisual {
            pose: on_surface(
                ScreenPose::new(edge.position - up * drop, edge.orientation),
                0.0,
            ),
            size,
            active: moving || on_pill,
        });
        return chrome;
    }

    if let Some((u, v)) = hover {
        if let Some(edge) = edge_near((u, v)) {
            let gap = EDGE_GAP_FRAC * w;
            let (pose, roll, outside) = match edge {
                Edge::Left => (screen.surface_pose(-gap / w, 0.5), 0.0, u < 0.0),
                Edge::Right => (screen.surface_pose(1.0 + gap / w, 0.5), 0.0, u > 1.0),
                Edge::Top => (screen.surface_pose(0.5, -gap / h), FRAC_PI_2, v < 0.0),
            };
            let length = EDGE_LENGTH_FRAC * h;
            chrome.edge = Some(HandleVisual {
                pose: on_surface(pose, roll),
                size: (EDGE_WIDTH_FRAC * w, length),
                active: outside,
            });
        }
    }
    chrome
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
    fn chrome_stays_hidden_until_pointing_near_an_edge_or_corner() {
        let screen = crate::spatial::ScreenTransform::default();
        let none = WindowChrome::default();
        assert_eq!(window_chrome(&screen, None, false, None), none);
        assert_eq!(
            window_chrome(&screen, Some((0.5, 0.5)), false, None),
            none,
            "the middle of the screen shows nothing"
        );
        let c = window_chrome(&screen, Some((0.5, 0.95)), false, None);
        assert!(c.bar.is_some_and(|b| !b.active) && c.edge.is_none() && c.corner.is_none());
        let c = window_chrome(&screen, Some((0.5, 1.04)), false, None);
        assert!(
            c.bar.unwrap().active,
            "pointing at the pill itself lights it"
        );
        let c = window_chrome(&screen, Some((0.98, 0.5)), false, None);
        assert!(c.edge.is_some_and(|e| !e.active) && c.bar.is_none() && c.corner.is_none());
        let c = window_chrome(&screen, Some((1.03, 0.5)), false, None);
        assert!(
            c.edge.unwrap().active,
            "pointing past the edge lights its handle"
        );
        let c = window_chrome(&screen, Some((0.03, 0.03)), false, None);
        assert!(c.corner.is_some_and(|k| !k.active) && c.edge.is_none() && c.bar.is_none());
        let c = window_chrome(&screen, Some((0.98, 0.5)), true, None);
        assert!(
            c.bar.unwrap().active && c.edge.is_none(),
            "moving shows only the pill"
        );
        let c = window_chrome(&screen, Some((0.5, 0.5)), false, Some((1.0, 1.0)));
        assert!(c.corner.unwrap().active && c.bar.is_none() && c.edge.is_none());
    }

    #[test]
    fn handles_sit_just_outside_their_edges_where_a_pointer_can_reach_them() {
        use crate::config::DisplayMode;
        use crate::spatial::{Ray, ScreenTransform};
        for mode in [DisplayMode::CurvedCylinder, DisplayMode::FlatQuad] {
            let screen = ScreenTransform {
                display_mode: mode,
                ..ScreenTransform::default()
            };
            let (w, h) = screen.size_m();
            let local = |p: Vec3| {
                screen
                    .orientation
                    .conjugate()
                    .rotate_vec(p - screen.position)
            };
            let viewer = screen.position + Vec3::new(0.0, 0.0, screen.distance);
            let reachable = |p: Vec3| {
                screen
                    .intersect_window(&Ray::new(viewer, p - viewer))
                    .map(|hit| hit.uv)
            };

            let bar = window_chrome(&screen, None, true, None).bar.unwrap();
            assert!(
                local(bar.pose.position).y < -h * 0.5,
                "{mode:?}: pill below"
            );
            assert!(reachable(bar.pose.position).is_some_and(|(_, v)| v > 1.0));

            let right = window_chrome(&screen, Some((0.99, 0.5)), false, None)
                .edge
                .unwrap();
            let right_edge = local(screen.surface_pose(1.0, 0.5).position);
            assert!(local(right.pose.position).x > right_edge.x, "{mode:?}");
            assert!(reachable(right.pose.position).is_some_and(|(u, _)| u > 1.0));

            let top = window_chrome(&screen, Some((0.5, 0.01)), false, None)
                .edge
                .unwrap();
            assert!(
                local(top.pose.position).y > h * 0.5,
                "{mode:?}: above the top"
            );
            // Turned to lie along the top edge.
            let along = top.pose.orientation.rotate_vec(Vec3::new(0.0, 1.0, 0.0));
            assert!(local(screen.position + along).y.abs() < 1e-3, "{mode:?}");

            let corner = window_chrome(&screen, Some((0.02, 0.02)), false, None)
                .corner
                .unwrap();
            let c = local(corner.pose.position);
            let left_edge = local(screen.surface_pose(0.0, 0.5).position);
            assert!(
                c.x > left_edge.x && c.y < h * 0.5,
                "{mode:?}: centred inside"
            );
            assert!((corner.size.0 - corner_handle_frac() * w).abs() < 1e-5);
        }
    }

    #[test]
    fn edges_and_corners_are_told_apart() {
        assert_eq!(edge_near((0.5, 0.5)), None);
        assert_eq!(edge_near((-0.02, 0.5)), Some(Edge::Left));
        assert_eq!(edge_near((0.97, 0.4)), Some(Edge::Right));
        assert_eq!(edge_near((0.5, 0.02)), Some(Edge::Top));
        assert_eq!(edge_near((0.5, 0.99)), None, "the bottom has the pill");
        assert_eq!(corner_near((0.5, 0.05)), None);
        assert_eq!(corner_near((0.03, 0.97)), Some((0.0, 1.0)));
        assert_eq!(corner_near((1.04, -0.03)), Some((1.0, 0.0)), "just outside");
    }

    #[test]
    fn textures_have_the_declared_sizes_and_shapes() {
        let alpha = |px: &[u8], w: u32, x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
        assert_eq!(
            ray_rgba().len(),
            (RAY_TEXTURE.0 * RAY_TEXTURE.1 * 4) as usize
        );
        let (cw, ch) = CURSOR_TEXTURE;
        let cursor = cursor_rgba();
        assert!(
            alpha(&cursor, cw, cw / 2, ch / 2) > 200,
            "cursor centre is opaque"
        );
        assert_eq!(cursor[3], 0, "cursor corner is transparent");

        let (mw, mh) = MASK_TEXTURE;
        let mask = mask_rgba();
        assert_eq!(
            alpha(&mask, mw, mw / 2, mh / 2),
            255,
            "mask is opaque inside"
        );
        assert_eq!(alpha(&mask, mw, mw / 2, 0), 255, "and along the edges");
        assert_eq!(alpha(&mask, mw, 0, 0), 0, "and clear in the corners");
        assert_eq!(alpha(&mask, mw, mw - 1, mh - 1), 0);

        let (bw, bh) = BAR_TEXTURE;
        let bar = bar_rgba(0.5);
        assert!(
            alpha(&bar, bw, bw / 2, bh / 2) > 100,
            "pill body is visible"
        );
        assert_eq!(alpha(&bar, bw, 0, 0), 0, "pill ends are round");

        let (ew, eh) = EDGE_TEXTURE;
        let edge = edge_rgba(0.9);
        assert!(
            alpha(&edge, ew, ew / 2, eh / 2) > 200,
            "edge line is bright mid-way"
        );
        assert!(alpha(&edge, ew, ew / 2, 0) < 5, "and fades out at its ends");
        assert!(alpha(&edge, ew, 0, eh / 2) < 10, "its glow fades sideways");

        let (kw, kh) = CORNER_TEXTURE;
        let corner = corner_rgba(1.0);
        assert_eq!(
            alpha(&corner, kw, kw / 2, kh / 2),
            0,
            "the arc's centre is empty"
        );
        // On the arc, up and to the left of its centre.
        let r = (CORNER_RADIUS_FRAC + ARC_GAP_FRAC + ARC_THICKNESS_FRAC * 0.5)
            / corner_handle_frac()
            * kw as f32;
        let d = (r / std::f32::consts::SQRT_2).round() as u32;
        assert!(
            alpha(&corner, kw, kw / 2 - d, kh / 2 - d) > 200,
            "the arc is drawn"
        );
        assert_eq!(
            alpha(&corner, kw, kw - 1, kh - 1),
            0,
            "nothing on the screen side"
        );
    }

    #[test]
    fn overlay_pixels_are_premultiplied() {
        let alpha = |px: &[u8], w: u32, x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
        let rgb = |px: &[u8], w: u32, x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };
        // Transparent pixels must be black, or the compositor adds them as white.
        let (cw, _) = CURSOR_TEXTURE;
        let cursor = cursor_rgba();
        assert_eq!(alpha(&cursor, cw, 0, 0), 0);
        assert_eq!(rgb(&cursor, cw, 0, 0), [0, 0, 0]);
        let (kw, kh) = CORNER_TEXTURE;
        let corner = corner_rgba(1.0);
        assert_eq!(rgb(&corner, kw, kw - 1, kh - 1), [0, 0, 0]);
        let (ew, eh) = EDGE_TEXTURE;
        let edge = edge_rgba(1.0);
        assert_eq!(
            rgb(&edge, ew, 0, 0),
            [0, 0, 0],
            "the edge strip is not a block"
        );
        // A colour never exceeds its alpha.
        for px in [cursor, corner, edge, bar_rgba(0.6), ray_rgba()].iter() {
            for p in px.chunks(4) {
                assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "{p:?}");
            }
        }
        let _ = eh;
    }

    #[test]
    fn the_frame_glows_around_the_window_and_leaves_it_clear() {
        let aspect = 16.0 / 9.0;
        let (w, h) = frame_texture_size(aspect);
        let frame = frame_rgba(aspect);
        let alpha = |x: u32, y: u32| frame[((y * w + x) * 4 + 3) as usize];
        assert_eq!(alpha(w / 2, h / 2), 0, "the window itself stays clear");
        // On the window's left edge, half way down: the bright rim.
        let margin_px = (FRAME_MARGIN_FRAC / (1.0 + 2.0 * FRAME_MARGIN_FRAC) * w as f32) as u32;
        assert!(
            alpha(margin_px, h / 2) > 150,
            "rim {}",
            alpha(margin_px, h / 2)
        );
        // Out in the margin it is fainter, and gone at the texture's edge.
        assert!(alpha(margin_px / 2, h / 2) < alpha(margin_px, h / 2));
        assert_eq!(alpha(0, h / 2), 0);
        assert_eq!(alpha(0, 0), 0);
    }

    #[test]
    fn the_frame_shows_while_moving_or_resizing_only() {
        let screen = crate::spatial::ScreenTransform::default();
        assert!(window_chrome(&screen, Some((0.5, 0.95)), false, None)
            .frame
            .is_none());
        let moving = window_chrome(&screen, None, true, None);
        let frame = moving.frame.expect("frame while moving");
        let (w, _) = screen.size_m();
        let (fw, _) = frame.window.size_m();
        assert!((fw - w * (1.0 + 2.0 * FRAME_MARGIN_FRAC)).abs() < 1e-3);
        assert!(window_chrome(&screen, None, false, Some((0.0, 0.0)))
            .frame
            .is_some());
    }
}
