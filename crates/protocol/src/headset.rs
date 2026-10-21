//! What the headset reports to the host about itself, and the screen layout and settings the
//! host can set on it. Carried on the control channel (TCP 48444).
//!
//! The flow, after a control connection opens:
//!
//! 1. The host sends `HeadsetStatusRequest`. A headset that never receives it is talking to an
//!    older host and sends none of these messages.
//! 2. The headset answers with a [`HeadsetStatus`], and sends a fresh one whenever its layout or
//!    settings change, and every few seconds (for the stats).
//! 3. The host may send a new layout ([`ScreenPlacement`]s) or [`HeadsetSettings`]. The headset
//!    clamps them to what it supports, applies them, and reports the result in its next
//!    status, so the host always sees what the headset really shows.
//!
//! The encoding is bincode, which is not self-describing: these structs must not change shape.
//! A later version adds new message variants instead.
//!
//! Coordinates are the headset's stage space, in meters: +X right, +Y up, -Z forward, with the
//! seated user at the origin. A screen faces +Z in its own frame, so a screen in front of the
//! user (at -Z) with the identity orientation faces them.

use serde::{Deserialize, Serialize};

/// How a screen bends.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ScreenCurve {
    Flat,
    /// Curved around a vertical axis `radius_m` meters behind the screen's center (towards the
    /// user), like Horizon OS's curved windows.
    Curved {
        radius_m: f32,
    },
}

impl ScreenCurve {
    pub fn is_curved(&self) -> bool {
        matches!(self, Self::Curved { .. })
    }
}

/// Where one screen is and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenPlacement {
    /// The screen's display id, which is also its video stream id. 0 is the desktop capture.
    pub display_id: u32,
    /// Center of the screen, in meters.
    pub position: [f32; 3],
    /// Orientation as a unit quaternion `[x, y, z, w]`.
    pub orientation: [f32; 4],
    /// Visible width and height in meters (for a curved screen, the width along the arc).
    pub width_m: f32,
    pub height_m: f32,
    pub curve: ScreenCurve,
}

impl ScreenPlacement {
    /// A screen on the sphere around a user whose eyes are at `eye_height_m`: turned `yaw_rad`
    /// to the left (negative turns right), tilted `tilt_rad` up, `distance_m` away, facing them.
    #[allow(clippy::too_many_arguments)]
    pub fn facing_user(
        display_id: u32,
        yaw_rad: f32,
        tilt_rad: f32,
        distance_m: f32,
        eye_height_m: f32,
        width_m: f32,
        height_m: f32,
        curve: ScreenCurve,
    ) -> Self {
        let (sy, cy) = yaw_rad.sin_cos();
        let (st, ct) = tilt_rad.sin_cos();
        let position = [
            -sy * ct * distance_m,
            eye_height_m + st * distance_m,
            -cy * ct * distance_m,
        ];
        Self {
            display_id,
            position,
            orientation: yaw_then_pitch(yaw_rad, tilt_rad),
            width_m,
            height_m,
            curve,
        }
    }

    /// How far the screen is turned to the left of straight ahead, seen from above (radians,
    /// negative to the right). Taken from where the screen is, not how it is rotated.
    pub fn yaw_rad(&self) -> f32 {
        (-self.position[0]).atan2(-self.position[2])
    }

    /// Distance from the user's vertical axis, seen from above, in meters.
    pub fn horizontal_distance_m(&self) -> f32 {
        self.position[0].hypot(self.position[2])
    }

    /// Which way the screen faces, seen from above: the turn (radians, left positive) of its
    /// own forward axis. Equals [`Self::yaw_rad`] for a screen that faces the user.
    pub fn facing_yaw_rad(&self) -> f32 {
        let [x, y, z, w] = self.orientation;
        // The rotated +Z axis; its X and Z give the heading.
        let fx = 2.0 * (x * z + w * y);
        let fz = 1.0 - 2.0 * (x * x + y * y);
        fx.atan2(fz)
    }

    /// True when every number is finite and the size is positive.
    pub fn is_valid(&self) -> bool {
        let curve_ok = match self.curve {
            ScreenCurve::Flat => true,
            ScreenCurve::Curved { radius_m } => radius_m.is_finite() && radius_m > 0.0,
        };
        self.position.iter().all(|v| v.is_finite())
            && self.orientation.iter().all(|v| v.is_finite())
            && self.orientation.iter().map(|v| v * v).sum::<f32>() > 1e-6
            && self.width_m.is_finite()
            && self.height_m.is_finite()
            && self.width_m > 0.0
            && self.height_m > 0.0
            && curve_ok
    }
}

/// Rotation by `yaw` about +Y, then `pitch` about the turned X axis, as `[x, y, z, w]`.
fn yaw_then_pitch(yaw: f32, pitch: f32) -> [f32; 4] {
    let (sy, cy) = (yaw * 0.5).sin_cos();
    let (sp, cp) = (pitch * 0.5).sin_cos();
    // q_yaw * q_pitch with q_yaw = (0, sy, 0, cy) and q_pitch = (sp, 0, 0, cp).
    [cy * sp, sy * cp, -sy * sp, cy * cp]
}

/// Video codec the headset prefers to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadsetCodec {
    Av1,
    Hevc,
}

/// The headset's own settings that the host can read and change.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HeadsetSettings {
    /// Video bitrate in Mbit/s: the ceiling when `adaptive_bitrate`, else the fixed rate.
    pub bitrate_mbps: u32,
    pub adaptive_bitrate: bool,
    pub codec: HeadsetCodec,
    /// Display refresh rate in Hz (one of [`HeadsetStatus::refresh_rates_hz`]).
    pub refresh_rate_hz: f32,
    /// Curve of the screens. Setting it bends (or flattens) every screen; the headset reports
    /// the desktop screen's curve.
    pub curve: ScreenCurve,
    /// Head-locked performance overlay.
    pub hud: bool,
    /// Show the room (passthrough) behind the screens.
    pub passthrough: bool,
}

/// Counters from the headset's own view of the stream, for the host's status pages.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct HeadsetStats {
    /// Frames the headset rendered per second.
    pub render_fps: f32,
    /// Estimated motion-to-photon latency in milliseconds (0 when unknown).
    pub motion_to_photon_ms: f32,
    /// Last decode time of the desktop stream in microseconds.
    pub decode_latency_us: u64,
    pub frames_decoded: u64,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub fec_recovered: u64,
    /// Video the headset received, in kbit/s.
    pub received_kbps: u32,
}

/// Everything the headset reports about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadsetStatus {
    /// Headset model, such as "Quest 3".
    pub device: String,
    /// Version of the headset app.
    pub app_version: String,
    pub settings: HeadsetSettings,
    /// Every screen the headset has a place for, the desktop (display 0) first.
    pub screens: Vec<ScreenPlacement>,
    /// Display ids of the screens that show video right now.
    pub showing: Vec<u32>,
    /// Refresh rates the display supports, in Hz.
    pub refresh_rates_hz: Vec<f32>,
    pub stats: HeadsetStats,
}

impl HeadsetStatus {
    /// The placement of `display_id`, if the headset has one.
    pub fn screen(&self, display_id: u32) -> Option<&ScreenPlacement> {
        self.screens.iter().find(|s| s.display_id == display_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn a_screen_straight_ahead_faces_the_user() {
        let s = ScreenPlacement::facing_user(0, 0.0, 0.0, 1.2, 1.4, 1.6, 0.9, ScreenCurve::Flat);
        assert!(close(s.position[0], 0.0) && close(s.position[1], 1.4));
        assert!(close(s.position[2], -1.2));
        assert_eq!(s.orientation, [0.0, 0.0, 0.0, 1.0]);
        assert!(close(s.yaw_rad(), 0.0) && close(s.horizontal_distance_m(), 1.2));
        assert!(s.is_valid());
    }

    #[test]
    fn positive_yaw_is_to_the_left_and_still_faces_the_user() {
        let yaw = 0.45;
        let s = ScreenPlacement::facing_user(2, yaw, 0.0, 1.25, 1.4, 1.6, 0.9, ScreenCurve::Flat);
        assert!(s.position[0] < 0.0, "left of the user");
        assert!(close(s.yaw_rad(), yaw));
        assert!(close(s.facing_yaw_rad(), yaw));
        assert!(close(s.horizontal_distance_m(), 1.25));
        // Unit quaternion.
        let norm: f32 = s.orientation.iter().map(|v| v * v).sum();
        assert!(close(norm, 1.0));
    }

    #[test]
    fn tilt_raises_the_screen_and_keeps_its_heading() {
        let s = ScreenPlacement::facing_user(1, -0.3, 0.2, 1.0, 1.4, 1.0, 0.5, ScreenCurve::Flat);
        assert!(s.position[1] > 1.4);
        assert!(close(s.facing_yaw_rad(), -0.3));
        assert!(close(s.horizontal_distance_m(), 0.2f32.cos()));
    }

    #[test]
    fn invalid_placements_are_detected() {
        let good = ScreenPlacement::facing_user(
            0,
            0.0,
            0.0,
            1.0,
            1.4,
            1.0,
            0.5,
            ScreenCurve::Curved { radius_m: 3.0 },
        );
        assert!(good.is_valid());
        for bad in [
            ScreenPlacement {
                width_m: 0.0,
                ..good
            },
            ScreenPlacement {
                position: [f32::NAN, 0.0, 0.0],
                ..good
            },
            ScreenPlacement {
                orientation: [0.0; 4],
                ..good
            },
            ScreenPlacement {
                curve: ScreenCurve::Curved { radius_m: -1.0 },
                ..good
            },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }

    #[test]
    fn status_round_trips_through_bincode() {
        let status = HeadsetStatus {
            device: "Quest 3".into(),
            app_version: "0.1.0".into(),
            settings: HeadsetSettings {
                bitrate_mbps: 80,
                adaptive_bitrate: true,
                codec: HeadsetCodec::Av1,
                refresh_rate_hz: 90.0,
                curve: ScreenCurve::Curved { radius_m: 3.0 },
                hud: false,
                passthrough: true,
            },
            screens: vec![ScreenPlacement::facing_user(
                0,
                0.0,
                0.0,
                1.2,
                1.4,
                1.8,
                1.0,
                ScreenCurve::Flat,
            )],
            showing: vec![0],
            refresh_rates_hz: vec![72.0, 90.0, 120.0],
            stats: HeadsetStats {
                render_fps: 89.5,
                packets_lost: 3,
                ..Default::default()
            },
        };
        let bytes = bincode::serialize(&status).unwrap();
        let back: HeadsetStatus = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, status);
        assert_eq!(back.screen(0).unwrap().display_id, 0);
        assert!(back.screen(1).is_none());
    }
}
