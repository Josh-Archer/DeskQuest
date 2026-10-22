//! Lets the host read and change the headset's screen layout and settings.
//!
//! - [`link`]: the control connection thread (TCP 48444, over the LAN or the USB tunnel).
//! - [`convert`]: the headset's types to and from the wire types of
//!   `linux_quest_protocol::headset`.
//!
//! The render loop owns the state: it drains [`HostCommand`]s, applies them with
//! `QuestClientRuntime::apply_host_command`, and publishes `QuestClientRuntime::headset_status`
//! whenever `take_host_status_due` says so.

pub mod convert;
pub mod link;

pub use link::{control_candidates, HostCommand, HostLink, LinkConfig};

/// How often the status (with its stats) is sent when nothing changed.
pub const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
pub(crate) mod tests {
    use linux_quest_protocol::{
        HeadsetCodec, HeadsetSettings, HeadsetStats, HeadsetStatus, ScreenCurve, ScreenPlacement,
    };

    pub(crate) fn sample_status() -> HeadsetStatus {
        HeadsetStatus {
            device: "Quest 3".into(),
            app_version: "test".into(),
            settings: HeadsetSettings {
                bitrate_mbps: 80,
                adaptive_bitrate: true,
                codec: HeadsetCodec::Av1,
                refresh_rate_hz: 90.0,
                curve: ScreenCurve::Curved { radius_m: 3.0 },
                hud: true,
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
                ScreenCurve::Curved { radius_m: 3.0 },
            )],
            showing: vec![0],
            refresh_rates_hz: vec![72.0, 90.0, 120.0],
            stats: HeadsetStats::default(),
        }
    }
}
