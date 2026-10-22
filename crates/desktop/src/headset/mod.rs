//! Device identity and an original Blender-authored headset preview.
mod discovery;
mod mesh;
mod view;

pub use discovery::{discover_usb, UsbDetection};
pub use mesh::{draw_origin, Orbit, Rect};
pub use view::{show_details, Preview};

use crate::daemon::Snapshot;
use linux_quest_protocol::ClientConnectionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    Quest3,
    #[default]
    Generic,
}

impl Model {
    /// Use whole model names: a Quest 3S must never select Quest 3 geometry.
    pub fn from_name(name: &str) -> Self {
        let words = name.replace(['_', '-'], " ").to_ascii_lowercase();
        let words = words.trim();
        match words {
            "quest 3" | "meta quest 3" | "oculus quest 3" | "simulated quest 3" | "eureka" => {
                Self::Quest3
            }
            _ => Self::Generic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub model: Model,
    pub name: String,
    pub connection: String,
    pub app_connected: bool,
}

impl Identity {
    pub fn reported(snapshot: &Snapshot) -> Option<Self> {
        let report = snapshot.headset.as_ref()?;
        let name = report.status.device.trim();
        let model = Model::from_name(name);
        let name = if model == Model::Quest3 && !name.to_ascii_lowercase().starts_with("simulated")
        {
            "Meta Quest 3".to_string()
        } else if name.is_empty() {
            "Connected headset".to_string()
        } else {
            name.chars().take(80).collect()
        };
        let connection = match snapshot.status.as_ref().map(|status| status.link) {
            Some(ClientConnectionMode::UsbAdb) => "Headset app connected over USB",
            Some(ClientConnectionMode::Wifi) => "Headset app connected over Wi-Fi",
            _ => "Headset app connected",
        };
        Some(Self {
            model,
            name,
            connection: connection.into(),
            app_connected: true,
        })
    }

    pub fn detected(snapshot: Option<&Snapshot>, usb: &UsbDetection) -> Option<Self> {
        snapshot.and_then(Self::reported).or_else(|| match usb {
            UsbDetection::One(identity) => Some(identity.clone()),
            _ => None,
        })
    }

    pub fn preview_note(&self) -> &'static str {
        match self.model {
            Model::Quest3 => "Original Quest 3 model. Drag to rotate; scroll to zoom.",
            Model::Generic => {
                "Generic preview: no matching model asset. Drag to rotate; scroll to zoom."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_model_names_and_adb_aliases_are_recognized() {
        for name in [
            "Quest 3",
            "META QUEST 3",
            "Quest_3",
            "Oculus Quest 3",
            "eureka",
            "Simulated Quest 3",
        ] {
            assert_eq!(Model::from_name(name), Model::Quest3, "{name}");
        }
    }

    #[test]
    fn other_headsets_keep_the_generic_preview() {
        for name in [
            "Quest 3S",
            "Meta Quest 3S",
            "Quest_3S",
            "Quest 2",
            "Quest Pro",
            "Meta Quest",
            "",
            "Quest 30",
        ] {
            assert_eq!(Model::from_name(name), Model::Generic, "{name}");
        }
    }
    fn reported_device(name: &str) -> Snapshot {
        Snapshot {
            headset: Some(linux_quest_host::HeadsetReport {
                status: linux_quest_protocol::HeadsetStatus {
                    device: name.into(),
                    app_version: "0.1.0".into(),
                    settings: linux_quest_protocol::HeadsetSettings {
                        bitrate_mbps: 30,
                        adaptive_bitrate: true,
                        codec: linux_quest_protocol::HeadsetCodec::Av1,
                        refresh_rate_hz: 90.,
                        curve: linux_quest_protocol::ScreenCurve::Flat,
                        hud: false,
                        passthrough: false,
                    },
                    screens: vec![],
                    showing: vec![],
                    refresh_rates_hz: vec![],
                    stats: Default::default(),
                },
                peer: "127.0.0.1:1234".into(),
                age_ms: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_connected_app_wins_over_another_headset_on_usb() {
        let usb = UsbDetection::One(Identity {
            model: Model::Quest3,
            name: "Meta Quest 3".into(),
            connection: "USB detected".into(),
            app_connected: false,
        });
        let identity = Identity::detected(Some(&reported_device("Quest 3S")), &usb).unwrap();
        assert_eq!(identity.name, "Quest 3S");
        assert_eq!(identity.model, Model::Generic);
        assert!(identity.app_connected);
    }

    #[test]
    fn usb_detection_is_a_fallback_and_ambiguous_devices_stay_unselected() {
        let identity = Identity {
            model: Model::Quest3,
            name: "Meta Quest 3".into(),
            connection: "USB detected".into(),
            app_connected: false,
        };
        assert_eq!(
            Identity::detected(None, &UsbDetection::One(identity.clone())),
            Some(identity)
        );
        assert!(Identity::detected(None, &UsbDetection::Multiple).is_none());
        assert!(Identity::detected(None, &UsbDetection::None).is_none());
    }
}
