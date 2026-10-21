//! Persisted client settings and the in-headset settings panel.
//!
//! [`ClientSettings`] is the user-editable subset of the client configuration. It is stored as
//! JSON on the headset ([`SettingsStore`]) and applied onto a [`ClientConfig`] at launch.
//! [`panel::SettingsPanel`] edits it in VR and reports the [`SettingsAction`]s that the runtime
//! must carry out, such as sending a monitor change to the host over the control channel.

pub mod panel;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use linux_quest_protocol::DisplayConfigMessage;
use linux_quest_transport::ControlMessage;
use serde::{Deserialize, Serialize};

use crate::config::{ClientConfig, ClientVideoCodec, TransportMode};
use crate::error::ClientResult;
use crate::spatial::PointerBindings;

pub use panel::{PanelRow, SettingsPanel};

/// How the stream bitrate is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitrateMode {
    /// The host adapts the bitrate to link conditions within its own limits.
    Adaptive,
    /// The host holds the configured bitrate.
    Fixed,
}

/// Arrangement of the virtual monitors on the host desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutPreset {
    Horizontal,
    Vertical,
    Grid,
}

impl LayoutPreset {
    /// Wire value of `DisplayConfigMessage::SetMonitorCount::layout_mode`.
    pub fn wire_mode(self) -> u8 {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
            Self::Grid => 2,
        }
    }
}

/// Smallest and largest monitor count the host accepts.
pub const MIN_MONITORS: u32 = 1;
pub const MAX_MONITORS: u32 = 3;
/// Bounds and step for the fixed bitrate, in Mbps.
pub const MIN_BITRATE_MBPS: u32 = 10;
pub const MAX_BITRATE_MBPS: u32 = 200;
pub const BITRATE_STEP_MBPS: u32 = 10;
/// Selectable refresh rates (Hz).
pub const REFRESH_RATES: [f32; 3] = [72.0, 90.0, 120.0];
/// Selectable virtual monitor resolutions.
pub const RESOLUTIONS: [(u32, u32); 3] = [(1920, 1080), (2560, 1440), (3840, 2160)];

/// User-editable client settings that persist across launches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    /// Selected host control address; `None` keeps the configured or discovered host.
    pub host: Option<SocketAddr>,
    pub transport: TransportMode,
    pub codec: ClientVideoCodec,
    pub bitrate_mode: BitrateMode,
    pub bitrate_mbps: u32,
    pub refresh_rate: f32,
    pub monitor_count: u32,
    pub monitor_width: u32,
    pub monitor_height: u32,
    pub layout: LayoutPreset,
    pub passthrough: bool,
    pub audio_enabled: bool,
    pub hud_enabled: bool,
    pub pointer_bindings: PointerBindings,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            host: None,
            transport: TransportMode::AdbReverseTcp,
            codec: ClientVideoCodec::Av1,
            bitrate_mode: BitrateMode::Adaptive,
            bitrate_mbps: 80,
            refresh_rate: 90.0,
            monitor_count: 1,
            monitor_width: 1920,
            monitor_height: 1080,
            layout: LayoutPreset::Horizontal,
            passthrough: true,
            audio_enabled: true,
            hud_enabled: true,
            pointer_bindings: PointerBindings::default(),
        }
    }
}

impl ClientSettings {
    /// Seeds settings from an existing configuration (used when no settings file exists yet).
    pub fn from_config(config: &ClientConfig) -> Self {
        Self {
            host: Some(config.host_addr),
            transport: config.transport_mode,
            codec: config.codec,
            refresh_rate: config.target_refresh_rate,
            monitor_width: config.stream_width,
            monitor_height: config.stream_height,
            passthrough: config.enable_passthrough,
            hud_enabled: config.enable_hud,
            pointer_bindings: config.pointer_bindings,
            ..Self::default()
        }
    }

    /// Clamps every field into the range the host and the headset support.
    pub fn sanitized(mut self) -> Self {
        self.monitor_count = self.monitor_count.clamp(MIN_MONITORS, MAX_MONITORS);
        self.bitrate_mbps = self.bitrate_mbps.clamp(MIN_BITRATE_MBPS, MAX_BITRATE_MBPS);
        if !REFRESH_RATES.contains(&self.refresh_rate) {
            self.refresh_rate = 90.0;
        }
        if !RESOLUTIONS.contains(&(self.monitor_width, self.monitor_height)) {
            (self.monitor_width, self.monitor_height) = RESOLUTIONS[0];
        }
        self
    }

    /// Writes these settings onto a client configuration. A saved host overrides the config's.
    pub fn apply_to(&self, config: &mut ClientConfig) {
        if let Some(host) = self.host {
            config.host_addr = host;
        }
        config.transport_mode = self.transport;
        config.codec = self.codec;
        config.target_refresh_rate = self.refresh_rate;
        config.stream_width = self.monitor_width;
        config.stream_height = self.monitor_height;
        config.enable_passthrough = self.passthrough;
        config.enable_hud = self.hud_enabled;
        config.pointer_bindings = self.pointer_bindings;
    }

    /// Host request that makes the monitor count, resolution and layout match these settings.
    pub fn monitor_request(&self) -> DisplayConfigMessage {
        DisplayConfigMessage::SetMonitorCount {
            count: self.monitor_count,
            width: self.monitor_width,
            height: self.monitor_height,
            refresh_rate: self.refresh_rate as u32,
            dpi: 96,
            layout_mode: self.layout.wire_mode(),
        }
    }

    /// Control-channel bitrate request for the current mode.
    pub fn bitrate_request(&self) -> ControlMessage {
        match self.bitrate_mode {
            BitrateMode::Fixed => ControlMessage::BitrateAdjustment {
                target_mbps: self.bitrate_mbps,
                min_mbps: self.bitrate_mbps,
                max_mbps: self.bitrate_mbps,
            },
            BitrateMode::Adaptive => ControlMessage::BitrateAdjustment {
                target_mbps: self.bitrate_mbps,
                min_mbps: MIN_BITRATE_MBPS,
                max_mbps: self.bitrate_mbps.max(MIN_BITRATE_MBPS),
            },
        }
    }
}

/// Something the runtime must do after a settings change.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    /// Host, transport or codec changed: the session must reconnect to take effect.
    Reconnect,
    /// Request for the host's display configuration (monitor count, resolution, layout).
    Display(DisplayConfigMessage),
    /// Request on the control channel (bitrate).
    Control(ControlMessage),
    SetRefreshRate(f32),
    SetPassthrough(bool),
    SetAudio(bool),
    SetHud(bool),
}

/// JSON file store for [`ClientSettings`].
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Saves atomically (write to a temp file in the same directory, then rename).
    pub fn save(&self, settings: &ClientSettings) -> ClientResult<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(settings)?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Loads the settings, or returns `None` when the file is missing or unreadable.
    /// A corrupt file is logged and ignored so the headset still starts.
    pub fn load(&self) -> Option<ClientSettings> {
        let content = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str::<ClientSettings>(&content) {
            Ok(settings) => Some(settings.sanitized()),
            Err(e) => {
                tracing::warn!(path = ?self.path, error = ?e, "Ignoring corrupt settings file");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lq-settings-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn store_round_trips_and_missing_or_corrupt_files_yield_none() {
        let store = SettingsStore::new(temp_path("rt"));
        assert!(store.load().is_none());
        let settings = ClientSettings {
            monitor_count: 2,
            layout: LayoutPreset::Grid,
            audio_enabled: false,
            ..Default::default()
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), settings);

        std::fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_none());
        let _ = std::fs::remove_file(store.path());
    }

    #[test]
    fn partial_file_falls_back_to_defaults_and_out_of_range_values_are_clamped() {
        let settings: ClientSettings =
            serde_json::from_str(r#"{"monitor_count": 9, "refresh_rate": 100.0}"#).unwrap();
        let settings = settings.sanitized();
        assert_eq!(settings.monitor_count, MAX_MONITORS);
        assert_eq!(settings.refresh_rate, 90.0);
        assert!(settings.hud_enabled);
    }

    #[test]
    fn apply_to_overrides_config_and_keeps_host_when_unset() {
        let mut config = ClientConfig::default();
        let original_host = config.host_addr;
        let mut settings = ClientSettings {
            transport: TransportMode::Udp,
            codec: ClientVideoCodec::Hevc,
            refresh_rate: 120.0,
            passthrough: false,
            hud_enabled: false,
            ..Default::default()
        };
        settings.apply_to(&mut config);
        assert_eq!(config.host_addr, original_host);
        assert_eq!(config.transport_mode, TransportMode::Udp);
        assert_eq!(config.codec, ClientVideoCodec::Hevc);
        assert_eq!(config.target_refresh_rate, 120.0);
        assert!(!config.enable_passthrough && !config.enable_hud);

        settings.host = Some("192.168.1.20:48444".parse().unwrap());
        settings.apply_to(&mut config);
        assert_eq!(config.host_addr, settings.host.unwrap());
        config.validate().unwrap();
    }

    #[test]
    fn monitor_request_carries_count_resolution_and_layout() {
        let settings = ClientSettings {
            monitor_count: 2,
            monitor_width: 2560,
            monitor_height: 1440,
            layout: LayoutPreset::Vertical,
            ..Default::default()
        };
        assert_eq!(
            settings.monitor_request(),
            DisplayConfigMessage::SetMonitorCount {
                count: 2,
                width: 2560,
                height: 1440,
                refresh_rate: 90,
                dpi: 96,
                layout_mode: 1,
            }
        );
    }

    #[test]
    fn bitrate_request_pins_fixed_mode() {
        let settings = ClientSettings {
            bitrate_mode: BitrateMode::Fixed,
            bitrate_mbps: 50,
            ..Default::default()
        };
        assert_eq!(
            settings.bitrate_request(),
            ControlMessage::BitrateAdjustment {
                target_mbps: 50,
                min_mbps: 50,
                max_mbps: 50
            }
        );
    }
}
