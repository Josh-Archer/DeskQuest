//! Pure state model for the tray icon.
//!
//! Nothing here touches D-Bus, so it compiles and is tested in every build.

use linux_quest_protocol::ClientConnectionMode;

use crate::tui::model::DashboardSnapshot;

/// Title shown as the tray item's name and in the tooltip header.
pub const TRAY_TITLE: &str = "DeskQuest";

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
    /// A second line: what is streaming and how fast, or why nothing is.
    pub detail: Option<String>,
    /// Whether video goes to the headset; `None` when the daemon does not say (it is offline
    /// or an older version), and the pause item is then not offered.
    pub streaming_enabled: Option<bool>,
    /// A screen capture runs that can show the screen picker.
    pub can_choose_screen: bool,
    /// The screen picker is open.
    pub choosing_screen: bool,
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
            detail: None,
            streaming_enabled: None,
            can_choose_screen: false,
            choosing_screen: false,
        }
    }

    /// The view for a daemon that answered with `snapshot` and, if it is new enough, `status`.
    pub fn from_daemon(
        snapshot: &DashboardSnapshot,
        status: Option<&crate::ipc::DaemonStatus>,
        choosing_screen: bool,
    ) -> Self {
        let Some(status) = status else {
            return Self::for_state(ConnectionState::from_snapshot(snapshot));
        };
        // The icon follows the link, so a paused stream still shows how the headset is
        // connected.
        let state = match status.link {
            ClientConnectionMode::UsbAdb => ConnectionState::Usb,
            ClientConnectionMode::Wifi => ConnectionState::Wifi,
            ClientConnectionMode::Disconnected => ConnectionState::Disconnected,
        };
        let mut view = Self::for_state(state);
        if state == ConnectionState::Disconnected {
            view.status_line = "Disconnected: no headset".to_string();
        }
        view.detail = Some(if status.video_streams == 0 {
            "No video: capture or encoder did not start".to_string()
        } else if !status.streaming_enabled {
            "Streaming paused".to_string()
        } else if state == ConnectionState::Disconnected {
            "Waiting for the headset".to_string()
        } else {
            let screens = match status.video_streams {
                1 => "1 screen".to_string(),
                n => format!("{n} screens"),
            };
            format!(
                "Streaming {screens} at {:.0} Mbit/s",
                snapshot.client.bitrate_mbps
            )
        });
        view.streaming_enabled = (status.video_streams > 0).then_some(status.streaming_enabled);
        view.can_choose_screen = status.can_choose_screen;
        view.choosing_screen = choosing_screen;
        view
    }
}

/// Actions a menu item or click can request. The controller decides what they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Opens the desktop app (`deskquest`).
    OpenApp,
    /// Opens `linux-quest-host dashboard` in a terminal.
    OpenDashboard,
    /// Pauses video to the headset, or resumes it.
    ToggleStreaming,
    /// Shows GNOME's screen-share dialog to pick what the headset's main screen shows.
    ChooseScreen,
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
        enabled: bool,
    },
}

/// The tray menu for a view. Order here is the order shown to the user.
pub fn menu_entries(view: &TrayView) -> Vec<MenuEntry> {
    let online = view.state != ConnectionState::DaemonOffline;
    let mut entries = vec![MenuEntry::Status(view.status_line.clone())];
    if let Some(detail) = &view.detail {
        entries.push(MenuEntry::Status(detail.clone()));
    }
    entries.push(MenuEntry::Separator);
    if let Some(enabled) = view.streaming_enabled {
        entries.push(MenuEntry::Action {
            label: if enabled {
                "Pause Streaming"
            } else {
                "Resume Streaming"
            },
            action: TrayAction::ToggleStreaming,
            enabled: true,
        });
    }
    entries.push(MenuEntry::Action {
        label: if view.choosing_screen {
            "Choosing Screen…"
        } else {
            "Choose Screen…"
        },
        action: TrayAction::ChooseScreen,
        enabled: online && view.can_choose_screen && !view.choosing_screen,
    });
    entries.push(MenuEntry::Action {
        label: "Open DeskQuest",
        action: TrayAction::OpenApp,
        enabled: true,
    });
    entries.push(MenuEntry::Action {
        label: "Open Dashboard",
        action: TrayAction::OpenDashboard,
        enabled: true,
    });
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action {
        label: "Restart Service",
        action: TrayAction::RestartService,
        enabled: true,
    });
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action {
        label: "Quit",
        action: TrayAction::Quit,
        enabled: true,
    });
    entries
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
                TrayAction::ChooseScreen,
                TrayAction::OpenApp,
                TrayAction::OpenDashboard,
                TrayAction::RestartService,
                TrayAction::Quit
            ],
            "an older daemon does not say whether it streams: no pause item"
        );
        assert!(matches!(entries.last(), Some(MenuEntry::Action { .. })));
    }

    fn daemon_status(link: ClientConnectionMode) -> crate::ipc::DaemonStatus {
        crate::ipc::DaemonStatus {
            version: "test".into(),
            uptime_secs: 1,
            streaming_enabled: true,
            video_streams: 3,
            capture: crate::ipc::DaemonCapture::Screen,
            can_choose_screen: true,
            bitrate_mbps: 30,
            adaptive_bitrate: true,
            link,
            headset_connected: true,
            display_backend: "mock".into(),
        }
    }

    fn action(entries: &[MenuEntry], wanted: TrayAction) -> Option<(&'static str, bool)> {
        entries.iter().find_map(|e| match e {
            MenuEntry::Action {
                label,
                action,
                enabled,
            } if *action == wanted => Some((*label, *enabled)),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_new_daemon_gets_pause_and_choose_screen_items() {
        let mut snap = snapshot_for(ClientConnectionMode::Wifi).await;
        snap.client.bitrate_mbps = 28.4;
        let status = daemon_status(ClientConnectionMode::Wifi);
        let view = TrayView::from_daemon(&snap, Some(&status), false);
        assert_eq!(view.state, ConnectionState::Wifi);
        assert_eq!(
            view.detail.as_deref(),
            Some("Streaming 3 screens at 28 Mbit/s")
        );
        let entries = menu_entries(&view);
        assert_eq!(entries[1], MenuEntry::Status(view.detail.clone().unwrap()));
        assert_eq!(
            action(&entries, TrayAction::ToggleStreaming),
            Some(("Pause Streaming", true))
        );
        assert_eq!(
            action(&entries, TrayAction::ChooseScreen),
            Some(("Choose Screen…", true))
        );

        // Paused: still connected (the icon keeps the link), and it offers to resume.
        let paused = crate::ipc::DaemonStatus {
            streaming_enabled: false,
            ..status.clone()
        };
        let view = TrayView::from_daemon(&snap, Some(&paused), false);
        assert_eq!(view.state, ConnectionState::Wifi);
        assert_eq!(view.detail.as_deref(), Some("Streaming paused"));
        assert_eq!(
            action(&menu_entries(&view), TrayAction::ToggleStreaming),
            Some(("Resume Streaming", true))
        );

        // While the picker is open, or with only a test pattern, it cannot be opened again.
        let view = TrayView::from_daemon(&snap, Some(&status), true);
        assert_eq!(
            action(&menu_entries(&view), TrayAction::ChooseScreen),
            Some(("Choosing Screen…", false))
        );
        let pattern = crate::ipc::DaemonStatus {
            can_choose_screen: false,
            ..status
        };
        let view = TrayView::from_daemon(&snap, Some(&pattern), false);
        assert_eq!(
            action(&menu_entries(&view), TrayAction::ChooseScreen),
            Some(("Choose Screen…", false))
        );
    }

    #[test]
    fn an_offline_daemon_offers_nothing_that_needs_it() {
        let entries = menu_entries(&TrayView::for_state(ConnectionState::DaemonOffline));
        assert_eq!(action(&entries, TrayAction::ToggleStreaming), None);
        assert_eq!(
            action(&entries, TrayAction::ChooseScreen),
            Some(("Choose Screen…", false))
        );
        assert_eq!(
            action(&entries, TrayAction::OpenApp),
            Some(("Open DeskQuest", true))
        );
    }
}
