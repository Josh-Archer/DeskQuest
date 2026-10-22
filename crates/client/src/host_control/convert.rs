//! Conversions between the headset's own screen and settings types and the wire types the
//! host sees (`linux_quest_protocol::headset`).

use linux_quest_protocol::{HeadsetCodec, HeadsetSettings, ScreenCurve, ScreenPlacement};

use crate::config::{ClientVideoCodec, DisplayMode};
use crate::settings::{BitrateMode, ClientSettings, REFRESH_RATES};
use crate::spatial::limits::{clamp_distance, MIN_CURVE_RADIUS_M};
use crate::spatial::{Quat, ScreenTransform, Vec3};

/// Where `transform` is, as the host sees it.
pub fn placement_of(display_id: u32, transform: &ScreenTransform) -> ScreenPlacement {
    let (width_m, height_m) = transform.size_m();
    let p = transform.position;
    let q = transform.orientation;
    ScreenPlacement {
        display_id,
        position: [p.x, p.y, p.z],
        orientation: [q.x, q.y, q.z, q.w],
        width_m,
        height_m,
        curve: curve_of(transform),
    }
}

/// The curve of `transform`.
pub fn curve_of(transform: &ScreenTransform) -> ScreenCurve {
    match transform.display_mode {
        DisplayMode::FlatQuad => ScreenCurve::Flat,
        DisplayMode::CurvedCylinder => ScreenCurve::Curved {
            radius_m: transform.curve_radius,
        },
    }
}

/// `base` moved, turned, sized and bent as `placement` says, within the headset's comfort
/// limits (distance, height, size and curve are clamped).
pub fn transform_from_placement(
    placement: &ScreenPlacement,
    base: &ScreenTransform,
) -> ScreenTransform {
    let mut out = base.clone();
    let [x, y, z] = placement.position;
    let [qx, qy, qz, qw] = placement.orientation;
    out.position = Vec3::new(x, y, z);
    out.orientation = Quat::new(qx, qy, qz, qw).normalize();
    let (tilt, yaw, roll) = out.orientation.to_euler();
    (out.tilt, out.yaw, out.roll) = (tilt, yaw, roll);

    // Keep the screen within reach: scale its horizontal offset onto the allowed distance.
    let horizontal = x.hypot(z);
    let distance = clamp_distance(horizontal);
    if horizontal > 1e-4 && (distance - horizontal).abs() > 1e-6 {
        let scale = distance / horizontal;
        out.position.x *= scale;
        out.position.z *= scale;
    } else if horizontal <= 1e-4 {
        out.position.z = -distance;
    }
    out.distance = distance;

    set_size_and_curve(
        &mut out,
        placement.width_m,
        placement.height_m,
        placement.curve,
    );
    out
}

/// Gives `transform` this visible size and curve, clamped.
fn set_size_and_curve(
    transform: &mut ScreenTransform,
    width: f32,
    height: f32,
    curve: ScreenCurve,
) {
    match curve {
        ScreenCurve::Flat => {
            transform.display_mode = DisplayMode::FlatQuad;
            transform.quad_size = (width, height);
        }
        ScreenCurve::Curved { radius_m } => {
            transform.display_mode = DisplayMode::CurvedCylinder;
            let radius = radius_m.max(MIN_CURVE_RADIUS_M);
            transform.curve_radius = radius;
            transform.curve_angle = width / radius;
            transform.aspect_ratio = width / height.max(f32::EPSILON);
        }
    }
    transform.clamp();
}

/// Bends or flattens `transform` to `curve`, keeping its visible size.
pub fn apply_curve(transform: &mut ScreenTransform, curve: ScreenCurve) {
    let (width, height) = transform.size_m();
    set_size_and_curve(transform, width, height, curve);
}

/// The settings the host sees. `curve` comes from the desktop screen.
pub fn headset_settings_of(settings: &ClientSettings, curve: ScreenCurve) -> HeadsetSettings {
    HeadsetSettings {
        bitrate_mbps: settings.bitrate_mbps,
        adaptive_bitrate: settings.bitrate_mode == BitrateMode::Adaptive,
        codec: match settings.codec {
            ClientVideoCodec::Av1 => HeadsetCodec::Av1,
            ClientVideoCodec::Hevc => HeadsetCodec::Hevc,
        },
        refresh_rate_hz: settings.refresh_rate,
        curve,
        hud: settings.hud_enabled,
        passthrough: settings.passthrough,
    }
}

/// The supported refresh rate closest to `hz` (from `supported`, or the panel's rates when
/// that is empty).
pub fn closest_refresh_rate(hz: f32, supported: &[f32]) -> f32 {
    let rates = if supported.is_empty() {
        &REFRESH_RATES[..]
    } else {
        supported
    };
    rates
        .iter()
        .copied()
        .filter(|r| r.is_finite())
        .min_by(|a, b| (a - hz).abs().total_cmp(&(b - hz).abs()))
        .unwrap_or(90.0)
}

/// `settings` with the host's values applied, clamped to what the headset supports.
pub fn apply_headset_settings(
    settings: &ClientSettings,
    wanted: &HeadsetSettings,
    supported_rates: &[f32],
) -> ClientSettings {
    let mut next = settings.clone();
    next.bitrate_mbps = wanted.bitrate_mbps;
    next.bitrate_mode = if wanted.adaptive_bitrate {
        BitrateMode::Adaptive
    } else {
        BitrateMode::Fixed
    };
    next.codec = match wanted.codec {
        HeadsetCodec::Av1 => ClientVideoCodec::Av1,
        HeadsetCodec::Hevc => ClientVideoCodec::Hevc,
    };
    // The panel only offers its own rates; keep to those the display supports too.
    let panel_rates: Vec<f32> = REFRESH_RATES
        .iter()
        .copied()
        .filter(|r| supported_rates.is_empty() || supported_rates.contains(r))
        .collect();
    next.refresh_rate = closest_refresh_rate(wanted.refresh_rate_hz, &panel_rates);
    next.hud_enabled = wanted.hud;
    next.passthrough = wanted.passthrough;
    next.sanitized()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::limits::{MAX_DISTANCE_M, MAX_SCREEN_WIDTH_M};

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn a_screen_survives_the_round_trip() {
        for mode in [DisplayMode::CurvedCylinder, DisplayMode::FlatQuad] {
            let mut tf = ScreenTransform::for_display(1);
            tf.display_mode = mode;
            let placement = placement_of(1, &tf);
            let back = transform_from_placement(&placement, &ScreenTransform::default());
            assert!((back.position - tf.position).length() < 1e-4, "{mode:?}");
            assert!(close(back.yaw, tf.yaw), "{mode:?}");
            assert!(close(back.distance, tf.distance), "{mode:?}");
            assert_eq!(back.display_mode, mode);
            let (w, h) = back.size_m();
            let (w0, h0) = tf.size_m();
            assert!(
                close(w, w0) && close(h, h0),
                "{mode:?}: {w}x{h} vs {w0}x{h0}"
            );
        }
    }

    #[test]
    fn the_host_convention_matches_the_headsets() {
        // A screen the protocol helper puts 0.45 rad to the left is where the headset puts its
        // display 2 (to the left at +0.45).
        let ours = ScreenTransform::for_display(2);
        let theirs = ScreenPlacement::facing_user(
            2,
            0.45,
            0.0,
            ours.distance,
            ours.position.y,
            1.0,
            0.5,
            ScreenCurve::Flat,
        );
        let placed = transform_from_placement(&theirs, &ours);
        assert!((placed.position - ours.position).length() < 1e-4);
        assert!(close(placed.yaw, ours.yaw));
        assert!(close(placement_of(2, &ours).yaw_rad(), 0.45));
    }

    #[test]
    fn out_of_range_placements_are_clamped() {
        let far =
            ScreenPlacement::facing_user(0, 0.0, 0.0, 40.0, 1.4, 50.0, 0.5, ScreenCurve::Flat);
        let tf = transform_from_placement(&far, &ScreenTransform::default());
        assert!(close(tf.distance, MAX_DISTANCE_M));
        assert!(close(tf.position.z, -MAX_DISTANCE_M));
        assert!(tf.quad_size.0 <= MAX_SCREEN_WIDTH_M);
        // A screen on the user's own axis is put straight ahead at the nearest distance.
        let mut on_axis = far;
        on_axis.position = [0.0, 1.4, 0.0];
        let tf = transform_from_placement(&on_axis, &ScreenTransform::default());
        assert!(tf.position.z < 0.0 && tf.distance > 0.0);
    }

    #[test]
    fn bending_keeps_the_visible_size() {
        let mut tf = ScreenTransform {
            display_mode: DisplayMode::FlatQuad,
            quad_size: (1.6, 0.9),
            ..ScreenTransform::default()
        };
        apply_curve(&mut tf, ScreenCurve::Curved { radius_m: 2.0 });
        assert_eq!(tf.display_mode, DisplayMode::CurvedCylinder);
        let (w, h) = tf.size_m();
        assert!(close(w, 1.6) && close(h, 0.9), "{w}x{h}");
        apply_curve(&mut tf, ScreenCurve::Flat);
        assert_eq!(tf.display_mode, DisplayMode::FlatQuad);
        assert!(close(tf.quad_size.0, 1.6) && close(tf.quad_size.1, 0.9));
    }

    #[test]
    fn settings_map_both_ways_and_unsupported_rates_snap_to_the_closest() {
        let settings = ClientSettings::default();
        let wire = headset_settings_of(&settings, ScreenCurve::Flat);
        assert_eq!(wire.bitrate_mbps, settings.bitrate_mbps);
        assert!(wire.adaptive_bitrate);
        let wanted = HeadsetSettings {
            bitrate_mbps: 500,
            adaptive_bitrate: false,
            codec: HeadsetCodec::Hevc,
            refresh_rate_hz: 100.0,
            hud: false,
            passthrough: false,
            ..wire
        };
        let next = apply_headset_settings(&settings, &wanted, &[72.0, 90.0]);
        assert_eq!(next.bitrate_mode, BitrateMode::Fixed);
        assert_eq!(next.bitrate_mbps, crate::settings::MAX_BITRATE_MBPS);
        assert_eq!(next.codec, ClientVideoCodec::Hevc);
        assert_eq!(
            next.refresh_rate, 90.0,
            "120 is not supported by this display"
        );
        assert!(!next.hud_enabled && !next.passthrough);
        // Untouched fields stay.
        assert_eq!(next.monitor_count, settings.monitor_count);
        assert_eq!(closest_refresh_rate(130.0, &[]), 120.0);
    }
}
