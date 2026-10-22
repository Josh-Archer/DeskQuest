//! Read-only, bounded ADB discovery. An authorized USB device is not an app connection.
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::{Identity, Model};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UsbDetection {
    #[default]
    None,
    One(Identity),
    Multiple,
}

/// Never retain or display device serial numbers. Ignore phones and unauthorized devices.
fn parse_devices(output: &str) -> UsbDetection {
    let mut headsets = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let _serial = fields.next();
        if fields.next() != Some("device") {
            continue;
        }
        if !fields.clone().any(|field| field.starts_with("usb:")) {
            continue;
        }
        let Some(model) = fields.find_map(|field| field.strip_prefix("model:")) else {
            continue;
        };
        let name = model.replace('_', " ");
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("quest ")
            && !lower.starts_with("meta quest")
            && !lower.starts_with("oculus quest")
        {
            continue;
        }
        let model = Model::from_name(&name);
        headsets.push(Identity {
            model,
            name: if model == Model::Quest3 {
                "Meta Quest 3".into()
            } else {
                name
            },
            connection: "USB detected · headset app not connected".into(),
            app_connected: false,
        });
    }
    match headsets.len() {
        0 => UsbDetection::None,
        1 => UsbDetection::One(headsets.remove(0)),
        _ => UsbDetection::Multiple,
    }
}

struct Cache {
    checked: Instant,
    detection: UsbDetection,
}

/// At most one read-only query every five seconds, on the IPC runtime rather than GTK.
/// A missing or hung adb is treated as unavailable; it cannot freeze the desktop.
pub async fn discover_usb() -> UsbDetection {
    static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().await;
    if let Some(cached) = cache
        .as_ref()
        .filter(|cached| cached.checked.elapsed() < Duration::from_secs(5))
    {
        return cached.detection.clone();
    }
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        Command::new("adb")
            .args(["devices", "-l"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let detection = match result {
        Ok(Ok(output)) if output.status.success() => {
            parse_devices(&String::from_utf8_lossy(&output.stdout))
        }
        _ => UsbDetection::None,
    };
    *cache = Some(Cache {
        checked: Instant::now(),
        detection: detection.clone(),
    });
    detection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_an_authorized_quest_and_ignores_a_phone() {
        let output = "List of devices attached\nprivate-serial device usb:4-1 product:eureka model:Quest_3 device:eureka\nphone device usb:4-1 model:Pixel_9\n";
        let UsbDetection::One(identity) = parse_devices(output) else {
            panic!("one Quest expected")
        };
        assert_eq!(identity.model, Model::Quest3);
        assert!(!identity.app_connected);
        assert!(!format!("{identity:?}").contains("private-serial"));
    }

    #[test]
    fn unauthorized_offline_and_malformed_devices_are_not_connected() {
        assert_eq!(
            parse_devices(
                "serial unauthorized model:Quest_3\nserial offline model:Quest_3\nserial device\n"
            ),
            UsbDetection::None
        );
    }

    #[test]
    fn multiple_headsets_are_ambiguous_instead_of_picking_the_first() {
        assert_eq!(
            parse_devices("one device usb:4-1 model:Quest_3\ntwo device usb:4-2 model:Quest_3\n"),
            UsbDetection::Multiple
        );
    }

    #[test]
    fn wireless_adb_is_not_reported_as_a_usb_connection() {
        assert_eq!(
            parse_devices("192.0.2.1:5555 device model:Quest_3\n"),
            UsbDetection::None
        );
    }

    #[test]
    fn a_quest_3s_is_detected_with_a_generic_model() {
        let UsbDetection::One(identity) = parse_devices("serial device usb:4-1 model:Quest_3S\n")
        else {
            panic!("one Quest expected")
        };
        assert_eq!(identity.name, "Quest 3S");
        assert_eq!(identity.model, Model::Generic);
    }
}
