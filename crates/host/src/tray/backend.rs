//! StatusNotifierItem adapter for the tray, built on `ksni`.
//!
//! This is the only code that talks D-Bus. It holds no state of its own: the
//! controller hands it a [`TrayView`] and it renders that view as a tray item.

use async_trait::async_trait;
use ksni::menu::StandardItem;
use ksni::{Category, Handle, Icon, MenuItem, OfflineReason, ToolTip, TrayMethods};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::commands::ProcessRunner;
use super::controller::{Flow, IpcDaemonControl, TrayConfig, TrayController, TrayHost};
use super::icon::{render_argb, ICON_SIZES};
use super::model::{menu_entries, MenuEntry, TrayAction, TrayView, TRAY_TITLE};
use crate::tui::TelemetrySource;

const TRAY_ID: &str = "linux-quest-host";

/// Runs the tray until the user picks Quit or the process receives SIGINT or SIGTERM.
///
/// The daemon is not touched. The tray keeps polling when no StatusNotifier host is
/// running, and shows the icon once one starts.
pub async fn run_tray<S>(source: S, interval: std::time::Duration) -> std::io::Result<()>
where
    S: TelemetrySource + 'static,
{
    let (actions_tx, mut actions_rx) = mpsc::unbounded_channel();
    let host = KsniHost {
        actions: actions_tx,
        handle: None,
    };
    let mut controller = TrayController::new(source, host, ProcessRunner, TrayConfig::from_env())
        .with_daemon_control(std::sync::Arc::new(IpcDaemonControl));

    let mut term = signal(SignalKind::terminate())?;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!(?interval, "linux-quest tray started");

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                controller.refresh().await;
            }
            action = actions_rx.recv() => {
                let Some(action) = action else { break };
                if controller.handle(action).await == Flow::Quit {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }

    controller.shutdown().await;
    info!("linux-quest tray stopped");
    Ok(())
}

/// The ksni tray item. Menu clicks are forwarded to the controller over a channel.
struct QuestTray {
    view: TrayView,
    actions: mpsc::UnboundedSender<TrayAction>,
}

impl QuestTray {
    fn request(&self, action: TrayAction) {
        if self.actions.send(action).is_err() {
            warn!("tray action dropped: controller has stopped");
        }
    }
}

impl ksni::Tray for QuestTray {
    fn id(&self) -> String {
        TRAY_ID.to_string()
    }

    fn title(&self) -> String {
        TRAY_TITLE.to_string()
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON_SIZES
            .iter()
            .map(|&size| Icon {
                width: size as i32,
                height: size as i32,
                data: render_argb(self.view.state, size),
            })
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: TRAY_TITLE.to_string(),
            description: self.view.status_line.clone(),
        }
    }

    // Left click opens the desktop app.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.request(TrayAction::OpenApp);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu_entries(&self.view)
            .into_iter()
            .map(|entry| match entry {
                MenuEntry::Status(label) => StandardItem {
                    label,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                MenuEntry::Separator => MenuItem::Separator,
                MenuEntry::Action {
                    label,
                    action,
                    enabled,
                } => StandardItem {
                    label: label.to_string(),
                    enabled,
                    activate: Box::new(move |tray: &mut Self| tray.request(action)),
                    ..Default::default()
                }
                .into(),
            })
            .collect()
    }

    fn watcher_online(&self) {
        info!("StatusNotifier host found; tray icon visible");
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        // Returning true keeps the tray registered, so it appears once a host starts.
        warn!(
            ?reason,
            "no StatusNotifier host (system tray) running; tray icon will appear when one starts"
        );
        true
    }
}

/// [`TrayHost`] backed by a ksni service. The service is spawned on first publish.
struct KsniHost {
    actions: mpsc::UnboundedSender<TrayAction>,
    handle: Option<Handle<QuestTray>>,
}

#[async_trait]
impl TrayHost for KsniHost {
    async fn publish(&mut self, view: &TrayView) -> Result<(), String> {
        if let Some(handle) = &self.handle {
            if !handle.is_closed() {
                let view = view.clone();
                return handle
                    .update(move |tray| tray.view = view)
                    .await
                    .ok_or_else(|| "tray service stopped".to_string());
            }
        }

        self.handle = None;
        let tray = QuestTray {
            view: view.clone(),
            actions: self.actions.clone(),
        };
        // `assume_sni_available` lets spawn succeed without a StatusNotifier host, so
        // a missing host is reported through `watcher_offline` instead of as an error.
        let handle = tray
            .assume_sni_available(true)
            .spawn()
            .await
            .map_err(|e| e.to_string())?;
        self.handle = Some(handle);
        Ok(())
    }

    async fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray::model::ConnectionState;
    use ksni::Tray;

    fn tray_for(state: ConnectionState) -> (QuestTray, mpsc::UnboundedReceiver<TrayAction>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let tray = QuestTray {
            view: TrayView::for_state(state),
            actions: tx,
        };
        (tray, rx)
    }

    #[test]
    fn icon_pixmaps_cover_all_sizes_with_matching_buffers() {
        let (tray, _rx) = tray_for(ConnectionState::Usb);
        let icons = tray.icon_pixmap();
        assert_eq!(icons.len(), ICON_SIZES.len());
        for icon in icons {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }

    #[test]
    fn menu_mirrors_model_and_status_row_is_disabled() {
        let (tray, _rx) = tray_for(ConnectionState::Wifi);
        let items = tray.menu();
        assert_eq!(items.len(), menu_entries(&tray.view).len());
        match &items[0] {
            MenuItem::Standard(item) => {
                assert_eq!(item.label, "Connected over Wi-Fi");
                assert!(!item.enabled);
            }
            _ => panic!("expected a standard status row first"),
        }
    }
}
