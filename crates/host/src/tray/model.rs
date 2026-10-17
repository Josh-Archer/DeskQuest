//! Pure state model for the tray icon.
//!
//! Nothing here touches D-Bus, so it compiles and is tested in every build.

use linux_quest_protocol::ClientConnectionMode;

use crate::tui::model::DashboardSnapshot;

/// Title shown as the tray item's name and in the tooltip header.
pub const TRAY_TITLE: &str = "Linux Quest host";

/// What the tray reports about the host's connection to a Quest headset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// The daemon's IPC socket did not answer. This is not the same as "no headset".
    DaemonOffline,
    /// The daemon is running but no headset is streaming.
    Disconnected,
    /// Streaming to a headset over USB (ADB reverse).
    Usb,
    /// Streaming to a headset over Wi-Fi.
    Wifi,
}

impl ConnectionState {
    /// Maps the daemon's dashboard snapshot onto a tray state.
    ///
    /// The daemon reports real link status: `client.mode` is the transport of the
    /// connected client (`Disconnected` when none is connected), and `streaming` is
    /// true only when a client is connected and a virtual display exists. A snapshot
    /// that is not streaming is therefore `Disconnected` regardless of the transport
    /// field, and a `Disconnected` transport is never shown as connected.
    pub fn from_snapshot(snapshot: &DashboardSnapshot) -> Self {
        if !snapshot.streaming {
            return Self::Disconnected;
        }
        match snapshot.client.mode {
            ClientConnectionMode::UsbAdb => Self::Usb,
            ClientConnectionMode::Wifi => Self::Wifi,
            ClientConnectionMode::Disconnected => Self::Disconnected,
        }
    }
}

/// Everything the tray needs to draw itself for one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub state: ConnectionState,
    pub status_line: String,
}

impl TrayView {
    pub fn for_state(state: ConnectionState) -> Self {
        let status_line = match state {
            ConnectionState::DaemonOffline => "Daemon not running",
            ConnectionState::Disconnected => "Disconnected: no Quest streaming",
            ConnectionState::Usb => "Connected over USB",
            ConnectionState::Wifi => "Connected over Wi-Fi",
        };
        Self {
            state,
            status_line: status_line.to_string(),
        }
    }
}

/// Actions a menu item or click can request. The controller decides what they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Opens `linux-quest-host dashboard` in a terminal.
    OpenDashboard,
    /// Runs `systemctl --user restart linux-quest-host`.
    RestartService,
    /// Exits the tray process. The daemon is not affected.
    Quit,
}

/// One row of the tray menu, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// Disabled row showing the current status.
    Status(String),
    Separator,
    Action {
        label: &'static str,
        action: TrayAction,
    },
}

/// The tray menu for a view. Order here is the order shown to the user.
pub fn menu_entries(view: &TrayView) -> Vec<MenuEntry> {
    vec![
        MenuEntry::Status(view.status_line.clone()),
        MenuEntry::Separator,
        MenuEntry::Action {
            label: "Open dashboard",
            action: TrayAction::OpenDashboard,
        },
        MenuEntry::Action {
            label: "Restart service",
            action: TrayAction::RestartService,
        },
        MenuEntry::Separator,
        MenuEntry::Action {
            label: "Quit",
            action: TrayAction::Quit,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{MockTelemetrySource, TelemetrySource};

    async fn snapshot_for(mode: ClientConnectionMode) -> DashboardSnapshot {
        MockTelemetrySource::new()
            .with_connection_mode(mode)
            .fetch_snapshot()
            .await
            .expect("mock snapshot")
    }

    #[tokio::test]
    async fn from_snapshot_maps_transport_modes() {
        let usb = snapshot_for(ClientConnectionMode::UsbAdb).await;
        let wifi = snapshot_for(ClientConnectionMode::Wifi).await;
        let idle = snapshot_for(ClientConnectionMode::Disconnected).await;
        assert_eq!(ConnectionState::from_snapshot(&usb), ConnectionState::Usb);
        assert_eq!(ConnectionState::from_snapshot(&wifi), ConnectionState::Wifi);
        assert_eq!(
            ConnectionState::from_snapshot(&idle),
            ConnectionState::Disconnected
        );
    }

    #[tokio::test]
    async fn from_snapshot_without_streaming_is_disconnected_even_if_mode_is_set() {
        let mut usb = snapshot_for(ClientConnectionMode::UsbAdb).await;
        usb.streaming = false;
        assert_eq!(
            ConnectionState::from_snapshot(&usb),
            ConnectionState::Disconnected
        );
    }

    #[tokio::test]
    async fn from_snapshot_wifi_without_streaming_is_disconnected() {
        let mut wifi = snapshot_for(ClientConnectionMode::Wifi).await;
        wifi.streaming = false;
        assert_eq!(
            ConnectionState::from_snapshot(&wifi),
            ConnectionState::Disconnected
        );
    }

    #[tokio::test]
    async fn from_snapshot_streaming_with_disconnected_mode_is_disconnected() {
        let mut idle = snapshot_for(ClientConnectionMode::Disconnected).await;
        idle.streaming = true;
        assert_eq!(
            ConnectionState::from_snapshot(&idle),
            ConnectionState::Disconnected
        );
    }

    #[test]
    fn every_state_has_distinct_status_text() {
        let states = [
            ConnectionState::DaemonOffline,
            ConnectionState::Disconnected,
            ConnectionState::Usb,
            ConnectionState::Wifi,
        ];
        let texts: Vec<String> = states
            .iter()
            .map(|s| TrayView::for_state(*s).status_line)
            .collect();
        for (i, a) in texts.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &texts[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn menu_has_status_first_then_actions_in_order() {
        let view = TrayView::for_state(ConnectionState::Usb);
        let entries = menu_entries(&view);
        assert_eq!(
            entries.first(),
            Some(&MenuEntry::Status("Connected over USB".to_string()))
        );
        let actions: Vec<TrayAction> = entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Action { action, .. } => Some(*action),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                TrayAction::OpenDashboard,
                TrayAction::RestartService,
                TrayAction::Quit
            ]
        );
        assert!(matches!(entries.last(), Some(MenuEntry::Action { .. })));
    }
}
