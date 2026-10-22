//! The Screens page's top-down view of the headset's screens: where each one is seen from
//! above, what the pointer is over, and how a drag or scroll changes a screen. Pure math, no
//! GTK, so it is tested here.
//!
//! World coordinates are the headset's stage space in meters seen from above: `x` to the
//! right, `z` towards the user (forward is `-z`). On the canvas forward is up.

use linux_quest_protocol::{yaw_pitch_orientation, ScreenCurve, ScreenPlacement};

/// The headset's own comfort limits (it clamps to them too); mirrored here so a drag stops
/// where the headset would.
pub const MIN_DISTANCE_M: f32 = 0.3;
pub const MAX_DISTANCE_M: f32 = 5.0;
pub const MIN_WIDTH_M: f32 = 0.2;
pub const MAX_WIDTH_M: f32 = 5.0;
pub const MAX_YAW_DEG: f32 = 120.0;

/// A point seen from above, in meters.
pub type Point = (f32, f32);

/// Maps world meters to canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Where the user is on the canvas.
    pub origin: (f64, f64),
    pub px_per_m: f64,
}

impl View {
    /// A view of `width` x `height` pixels that shows the user and every screen, with
    /// `pad` pixels to spare on each side.
    pub fn fit(width: f64, height: f64, pad: f64, screens: &[ScreenPlacement]) -> Self {
        // Always show the user and a meter in front and to each side, so one screen does not
        // fill the canvas.
        let mut min = (-1.0f32, -1.5f32);
        let mut max = (1.0f32, 0.5f32);
        for p in screens.iter().flat_map(footprint) {
            min = (min.0.min(p.0), min.1.min(p.1));
            max = (max.0.max(p.0), max.1.max(p.1));
        }
        let span = (f64::from(max.0 - min.0), f64::from(max.1 - min.1));
        let room = ((width - 2.0 * pad).max(1.0), (height - 2.0 * pad).max(1.0));
        let px_per_m = (room.0 / span.0).min(room.1 / span.1).clamp(20.0, 600.0);
        // Centre the box on the canvas.
        let centre = (
            f64::from(min.0 + max.0) / 2.0,
            f64::from(min.1 + max.1) / 2.0,
        );
        Self {
            origin: (
                width / 2.0 - centre.0 * px_per_m,
                height / 2.0 - centre.1 * px_per_m,
            ),
            px_per_m,
        }
    }

    pub fn to_canvas(self, (x, z): Point) -> (f64, f64) {
        (
            self.origin.0 + f64::from(x) * self.px_per_m,
            self.origin.1 + f64::from(z) * self.px_per_m,
        )
    }

    pub fn to_world(self, (cx, cy): (f64, f64)) -> Point {
        (
            ((cx - self.origin.0) / self.px_per_m) as f32,
            ((cy - self.origin.1) / self.px_per_m) as f32,
        )
    }
}

/// The screen's centre seen from above.
pub fn centre(s: &ScreenPlacement) -> Point {
    (s.position[0], s.position[2])
}

/// Unit vector along the screen's face, towards its right edge.
fn right_axis(s: &ScreenPlacement) -> Point {
    let phi = s.facing_yaw_rad();
    (phi.cos(), -phi.sin())
}

/// The screen's face seen from above, from its left edge to its right edge: two points for a
/// flat screen, an arc for a curved one.
pub fn footprint(s: &ScreenPlacement) -> Vec<Point> {
    let c = centre(s);
    match s.curve {
        ScreenCurve::Flat => {
            let r = right_axis(s);
            let h = s.width_m / 2.0;
            vec![
                (c.0 - r.0 * h, c.1 - r.1 * h),
                (c.0 + r.0 * h, c.1 + r.1 * h),
            ]
        }
        ScreenCurve::Curved { radius_m } => {
            let radius = radius_m.max(0.05);
            let phi = s.facing_yaw_rad();
            // The curve's axis is one radius behind the centre, towards the user.
            let axis = (c.0 + radius * phi.sin(), c.1 + radius * phi.cos());
            let theta = s.width_m / radius;
            const STEPS: usize = 24;
            (0..=STEPS)
                .map(|i| {
                    let psi = -theta / 2.0 + theta * i as f32 / STEPS as f32;
                    (
                        axis.0 + radius * (psi - phi).sin(),
                        axis.1 - radius * (psi - phi).cos(),
                    )
                })
                .collect()
        }
    }
}

/// Which part of a screen the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The screen itself: dragging moves it around the user.
    Body,
    /// An end: dragging resizes it.
    Edge,
}

fn distance_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let len2 = ab.0 * ab.0 + ab.1 * ab.1;
    let t = if len2 > 0.0 {
        (((p.0 - a.0) * ab.0 + (p.1 - a.1) * ab.1) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let q = (a.0 + ab.0 * t, a.1 + ab.1 * t);
    (p.0 - q.0).hypot(p.1 - q.1)
}

/// The screen (index into `screens`) under canvas point `p`, and which part of it. Ends win
/// over bodies, and nearer screens over farther ones.
pub fn hit(
    view: &View,
    screens: &[ScreenPlacement],
    p: (f64, f64),
    tolerance_px: f64,
) -> Option<(usize, Part)> {
    let mut best: Option<(usize, Part, f64)> = None;
    for (i, s) in screens.iter().enumerate() {
        let pts: Vec<(f64, f64)> = footprint(s)
            .into_iter()
            .map(|q| view.to_canvas(q))
            .collect();
        let (first, last) = (pts[0], pts[pts.len() - 1]);
        let edge = (p.0 - first.0)
            .hypot(p.1 - first.1)
            .min((p.0 - last.0).hypot(p.1 - last.1));
        let body = pts
            .windows(2)
            .map(|w| distance_to_segment(p, w[0], w[1]))
            .fold(f64::INFINITY, f64::min);
        let candidate = if edge <= tolerance_px {
            Some((Part::Edge, edge))
        } else if body <= tolerance_px {
            // Bodies rank after any end within reach.
            Some((Part::Body, body + tolerance_px))
        } else {
            None
        };
        if let Some((part, score)) = candidate {
            if best.is_none_or(|(_, _, s)| score < s) {
                best = Some((i, part, score));
            }
        }
    }
    best.map(|(i, part, _)| (i, part))
}

/// `s` moved under the pointer at world point `p`, still facing the user, at the same height,
/// tilt and size. The distance stays within the headset's limits.
pub fn drag_to(s: &ScreenPlacement, p: Point) -> ScreenPlacement {
    let yaw = (-p.0)
        .atan2(-p.1)
        .clamp(-MAX_YAW_DEG.to_radians(), MAX_YAW_DEG.to_radians());
    let distance = p.0.hypot(p.1).clamp(MIN_DISTANCE_M, MAX_DISTANCE_M);
    s.moved_around_user(yaw, distance)
}

/// `s` with width `width` (clamped) and its aspect ratio kept.
pub fn with_width(s: &ScreenPlacement, width: f32) -> ScreenPlacement {
    let width = width.clamp(MIN_WIDTH_M, MAX_WIDTH_M);
    let aspect = s.height_m / s.width_m.max(f32::EPSILON);
    ScreenPlacement {
        width_m: width,
        height_m: width * aspect,
        ..*s
    }
}

/// `s` resized so the end being dragged follows world point `p`; it grows or shrinks
/// symmetrically about its centre.
pub fn resize_to(s: &ScreenPlacement, p: Point) -> ScreenPlacement {
    let c = centre(s);
    let r = right_axis(s);
    let half_chord = ((p.0 - c.0) * r.0 + (p.1 - c.1) * r.1).abs();
    let width = match s.curve {
        ScreenCurve::Flat => 2.0 * half_chord,
        ScreenCurve::Curved { radius_m } => {
            let radius = radius_m.max(0.05);
            2.0 * radius * (half_chord / radius).min(1.0).asin()
        }
    };
    with_width(s, width)
}

/// `s` scaled by `factor` (scroll wheel), aspect ratio kept.
pub fn scaled(s: &ScreenPlacement, factor: f32) -> ScreenPlacement {
    with_width(s, s.width_m * factor)
}

/// `s` turned to `angle_right_deg` (the UI's convention: positive to the right).
pub fn with_angle(s: &ScreenPlacement, angle_right_deg: f32) -> ScreenPlacement {
    let yaw = (-angle_right_deg)
        .clamp(-MAX_YAW_DEG, MAX_YAW_DEG)
        .to_radians();
    s.moved_around_user(yaw, s.horizontal_distance_m())
}

/// `s` at `distance_m` from the user, same direction.
pub fn with_distance(s: &ScreenPlacement, distance_m: f32) -> ScreenPlacement {
    s.moved_around_user(
        s.yaw_rad(),
        distance_m.clamp(MIN_DISTANCE_M, MAX_DISTANCE_M),
    )
}

/// `s` tilted `tilt_deg` up (negative down), same place.
pub fn with_tilt(s: &ScreenPlacement, tilt_deg: f32) -> ScreenPlacement {
    ScreenPlacement {
        orientation: yaw_pitch_orientation(
            s.facing_yaw_rad(),
            tilt_deg.clamp(-60.0, 60.0).to_radians(),
        ),
        ..*s
    }
}

/// `s` with its centre `height_m` above the floor.
pub fn with_height(s: &ScreenPlacement, height_m: f32) -> ScreenPlacement {
    let mut out = *s;
    out.position[1] = height_m.clamp(0.2, 3.0);
    out
}

/// The angle the UI shows: degrees to the right of straight ahead.
pub fn angle_right_deg(s: &ScreenPlacement) -> f32 {
    -s.yaw_rad().to_degrees()
}

/// What the UI calls display `id`.
pub fn display_name(id: u32) -> String {
    match id {
        0 => "Desktop".to_string(),
        n => format!("Screen {}", n + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn flat(id: u32, yaw: f32, distance: f32) -> ScreenPlacement {
        ScreenPlacement::facing_user(id, yaw, 0.0, distance, 1.4, 1.6, 0.9, ScreenCurve::Flat)
    }

    #[test]
    fn a_flat_screen_ahead_spans_left_to_right() {
        let s = flat(0, 0.0, 1.2);
        let pts = footprint(&s);
        assert_eq!(pts.len(), 2);
        assert!(close(pts[0].0, -0.8) && close(pts[1].0, 0.8));
        assert!(close(pts[0].1, -1.2) && close(pts[1].1, -1.2));
    }

    #[test]
    fn a_curved_screen_bows_towards_the_user_with_its_middle_at_the_centre() {
        let s = ScreenPlacement {
            curve: ScreenCurve::Curved { radius_m: 1.2 },
            ..flat(0, 0.0, 1.2)
        };
        let pts = footprint(&s);
        let mid = pts[pts.len() / 2];
        assert!(close(mid.0, 0.0) && close(mid.1, -1.2));
        // The ends come closer to the user than the middle.
        assert!(pts[0].1 > mid.1 && pts[pts.len() - 1].1 > mid.1);
        // A cylinder around the user: every point is 1.2 m from them.
        for p in &pts {
            assert!(close(p.0.hypot(p.1), 1.2), "{p:?}");
        }
        assert!(
            pts[0].0 < 0.0 && pts[pts.len() - 1].0 > 0.0,
            "left to right"
        );
    }

    #[test]
    fn the_view_round_trips_and_fits_every_screen() {
        let screens = [flat(0, 0.0, 1.2), flat(1, -1.2, 2.5), flat(2, 1.0, 1.0)];
        let view = View::fit(800.0, 500.0, 20.0, &screens);
        for p in screens.iter().flat_map(footprint) {
            let c = view.to_canvas(p);
            assert!(
                (0.0..=800.0).contains(&c.0) && (0.0..=500.0).contains(&c.1),
                "{c:?}"
            );
            let back = view.to_world(c);
            assert!(close(back.0, p.0) && close(back.1, p.1));
        }
        // Forward is up.
        let user = view.to_canvas((0.0, 0.0));
        assert!(view.to_canvas((0.0, -1.0)).1 < user.1);
    }

    #[test]
    fn hits_find_ends_before_bodies_and_miss_empty_space() {
        let screens = [flat(0, 0.0, 1.2)];
        let view = View {
            origin: (400.0, 400.0),
            px_per_m: 100.0,
        };
        assert_eq!(
            hit(&view, &screens, (400.0, 282.0), 8.0),
            Some((0, Part::Body))
        );
        assert_eq!(
            hit(&view, &screens, (481.0, 280.0), 8.0),
            Some((0, Part::Edge))
        );
        assert_eq!(hit(&view, &screens, (400.0, 380.0), 8.0), None);
    }

    #[test]
    fn dragging_moves_the_screen_around_the_user_within_limits() {
        let s = flat(1, 0.0, 1.2);
        let moved = drag_to(&s, (1.0, -1.0));
        assert!(close(angle_right_deg(&moved), 45.0));
        assert!(close(moved.horizontal_distance_m(), 2f32.sqrt()));
        assert!(
            close(moved.facing_yaw_rad(), moved.yaw_rad()),
            "faces the user"
        );
        let far = drag_to(&s, (0.0, -9.0));
        assert!(close(far.horizontal_distance_m(), MAX_DISTANCE_M));
        let behind = drag_to(&s, (0.0, 1.0));
        assert!(angle_right_deg(&behind).abs() <= MAX_YAW_DEG + 1e-3);
    }

    #[test]
    fn resizing_follows_the_dragged_end_and_keeps_the_aspect_ratio() {
        let s = flat(0, 0.0, 1.2);
        let wider = resize_to(&s, (1.2, -1.2));
        assert!(close(wider.width_m, 2.4));
        assert!(close(wider.height_m / wider.width_m, 0.9 / 1.6));
        // An arc: the width along the curve for that chord.
        let curved = ScreenPlacement {
            curve: ScreenCurve::Curved { radius_m: 1.2 },
            ..s
        };
        let end = footprint(&curved)[24];
        let same = resize_to(&curved, end);
        assert!(close(same.width_m, 1.6), "{}", same.width_m);
        assert!(close(scaled(&s, 100.0).width_m, MAX_WIDTH_M));
    }

    #[test]
    fn sidebar_edits_change_one_thing_each() {
        let s = flat(2, 0.4, 1.3);
        let turned = with_angle(&s, 30.0);
        assert!(close(angle_right_deg(&turned), 30.0));
        assert!(close(turned.horizontal_distance_m(), 1.3));
        let pushed = with_distance(&s, 2.0);
        assert!(close(pushed.yaw_rad(), 0.4) && close(pushed.horizontal_distance_m(), 2.0));
        let tilted = with_tilt(&s, 15.0);
        assert!(close(tilted.pitch_rad().to_degrees(), 15.0));
        assert_eq!(tilted.position, s.position);
        let raised = with_height(&s, 1.8);
        assert!(close(raised.position[1], 1.8));
        assert_eq!(display_name(0), "Desktop");
        assert_eq!(display_name(2), "Screen 3");
    }
}
