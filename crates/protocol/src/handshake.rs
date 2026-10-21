use crate::session::{ResumeStatus, SessionToken};
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
    /// Token from a previous `ServerHandshake` to resume that session; `None` for a new one.
    pub resume_token: Option<SessionToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHandshake {
    pub server_version: u32,
    pub selected_codec: VideoCodec,
    pub negotiated_bitrate_mbps: u32,
    pub displays: Vec<DisplayInfo>,
    /// Token the client presents to resume this session after a link drop.
    pub session_token: SessionToken,
    pub resume_status: ResumeStatus,
    /// How long the host keeps the session after the link drops, in milliseconds.
    pub resume_grace_ms: u32,
}

/// First stream/display id used for per-window panels. Monitors use small ids, so a window
/// panel's `DisplayInfo::display_id` never collides with a monitor's.
pub const WINDOW_STREAM_ID_BASE: u16 = 0x1000;

/// A top-level application window on the host that can be streamed as its own panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Host window identifier (X11 window id, or portal-assigned id).
    pub window_id: u64,
    pub title: String,
    pub app_name: String,
    pub width: u32,
    pub height: u32,
    /// Window origin in the host desktop layout, in pixels.
    pub x: i32,
    pub y: i32,
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
    /// Ask the host for the windows that can be popped out as panels.
    ListWindows,
    /// Reply to `ListWindows`.
    WindowList(Vec<WindowInfo>),
    /// Replace the set of windows streamed as panels (empty removes all). The host replies
    /// with `ActiveWindowPanels`.
    SetWindowPanels { window_ids: Vec<u64> },
    /// Active window panels; `display_id` is the stream id, `x`/`y` the window's host position.
    ActiveWindowPanels(Vec<DisplayInfo>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_messages_round_trip() {
        let window = WindowInfo {
            window_id: 0x4a00007,
            title: "Editor".into(),
            app_name: "code".into(),
            width: 1280,
            height: 720,
            x: 100,
            y: 50,
        };
        for message in [
            DisplayConfigMessage::ListWindows,
            DisplayConfigMessage::WindowList(vec![window]),
            DisplayConfigMessage::SetWindowPanels {
                window_ids: vec![1, 2],
            },
            DisplayConfigMessage::ActiveWindowPanels(vec![]),
        ] {
            let encoded = bincode::serialize(&message).unwrap();
            let decoded: DisplayConfigMessage = bincode::deserialize(&encoded).unwrap();
            assert_eq!(decoded, message);
        }
    }

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
