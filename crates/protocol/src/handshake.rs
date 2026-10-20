use crate::video::VideoCodec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub display_id: u16,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub dpi: u32,
    pub is_virtual: bool,
    /// Monitor origin in the host desktop layout, in pixels.
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHandshake {
    pub client_version: u32,
    pub device_model: String,
    pub supported_codecs: Vec<VideoCodec>,
    pub requested_displays: u16,
    pub max_bandwidth_mbps: u32,
    pub preferred_refresh_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHandshake {
    pub server_version: u32,
    pub selected_codec: VideoCodec,
    pub negotiated_bitrate_mbps: u32,
    pub displays: Vec<DisplayInfo>,
}

/// Dynamic multi-monitor control message exchanged over PacketType::DisplayConfig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayConfigMessage {
    /// Request to set virtual monitor count (1, 2, or 3) and layout
    SetMonitorCount {
        count: u32,
        width: u32,
        height: u32,
        refresh_rate: u32,
        dpi: u32,
        layout_mode: u8,
    },
    /// Notification of updated active monitors
    ActiveMonitors(Vec<DisplayInfo>),
    /// A monitor became active while the session was running.
    MonitorAdded(DisplayInfo),
    /// A monitor was removed while the session was running.
    MonitorRemoved { display_id: u16 },
    /// Notification of an error during display configuration
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_geometry_and_lifecycle_messages_round_trip() {
        let display = DisplayInfo {
            display_id: 3,
            name: "Quest-Virtual-3".into(),
            width: 2560,
            height: 1440,
            refresh_rate: 90,
            dpi: 96,
            is_virtual: true,
            x: 5120,
            y: 0,
        };
        for message in [
            DisplayConfigMessage::ActiveMonitors(vec![display.clone()]),
            DisplayConfigMessage::MonitorAdded(display.clone()),
            DisplayConfigMessage::MonitorRemoved { display_id: 3 },
        ] {
            let encoded = bincode::serialize(&message).unwrap();
            let decoded: DisplayConfigMessage = bincode::deserialize(&encoded).unwrap();
            assert_eq!(decoded, message);
        }
    }
}
