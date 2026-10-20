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
    /// Notification of an error during display configuration
    Error(String),
}
