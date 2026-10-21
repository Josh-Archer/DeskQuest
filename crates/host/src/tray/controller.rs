//! Tray controller: polls the daemon, publishes view changes, and runs menu actions.
//!
//! The controller is generic over the telemetry source, the tray host (the D-Bus
//! layer) and the command runner, so the whole flow is tested with mocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::commands::{app_command, dashboard_command, restart_command, CommandRunner};
use super::model::{ConnectionState, TrayAction, TrayView};
use crate::ipc::{DaemonRequest, DaemonResponse, DaemonStatus};
use crate::tui::TelemetrySource;

/// The daemon requests the tray makes besides polling its snapshot. A trait so the controller
/// is tested without a daemon.
#[async_trait]
pub trait DaemonControl: Send + Sync {
    /// The daemon's own status. An older daemon does not have it and fails.
    async fn status(&self) -> Result<DaemonStatus, String>;
    async fn set_streaming(&self, enabled: bool) -> Result<(), String>;
    /// Shows GNOME's screen-share dialog and waits for the user's answer (minutes at most).
    async fn choose_screen(&self) -> Result<(), String>;
}

/// [`DaemonControl`] over the daemon's IPC socket.
pub struct IpcDaemonControl;

async fn ipc(req: DaemonRequest) -> Result<DaemonResponse, String> {
    match crate::ipc::send_daemon_request(req).await {
        Ok(DaemonResponse::Error(e)) => Err(e),
        Ok(resp) => Ok(resp),
        Err(e) => Err(e.to_string()),
    }
}

#[async_trait]
impl DaemonControl for IpcDaemonControl {
    async fn status(&self) -> Result<DaemonStatus, String> {
        match ipc(DaemonRequest::Status).await? {
            DaemonResponse::Status(status) => Ok(*status),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    async fn set_streaming(&self, enabled: bool) -> Result<(), String> {
        ipc(DaemonRequest::SetStreaming { enabled })
            .await
            .map(|_| ())
    }

    async fn choose_screen(&self) -> Result<(), String> {
        ipc(DaemonRequest::ChooseScreen).await.map(|_| ())
    }
}

/// After a status request fails (an older daemon), it is not asked again for this long.
const STATUS_RETRY: Duration = Duration::from_secs(60);

/// The side that actually shows the icon (the D-Bus layer in production).
#[async_trait]
pub trait TrayHost: Send {
    /// Shows or updates the tray with `view`.
    ///
    /// An error means no tray could be shown this time, for example because no
    /// StatusNotifier host is running. The controller keeps going and calls again on
    /// the next poll.
    async fn publish(&mut self, view: &TrayView) -> Result<(), String>;

    /// Removes the icon. Called once on shutdown.
    async fn close(&mut self) {}
}

/// Longest a single daemon poll may take. A wedged daemon must not freeze the menu or
/// SIGTERM handling, so a slow answer counts as the daemon being offline.
pub const POLL_TIMEOUT: Duration = Duration::from_millis(1500);

/// Longest the service restart may take before it is abandoned (and killed).
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Name of the host binary looked up on `$PATH` when its own path cannot be trusted.
const HOST_BINARY: &str = "linux-quest-host";

/// Turns the path reported by `current_exe()` into something safe to launch.
///
/// After a package upgrade Linux reports the replaced binary as `<path> (deleted)`. The
/// suffix is stripped; if nothing exists at the stripped path the bare binary name is
/// used so the terminal finds it on `$PATH`. `exists` is injected for testing.
pub fn resolve_host_exe(raw: Option<&str>, exists: impl Fn(&str) -> bool) -> String {
    const DELETED: &str = " (deleted)";
    match raw {
        None => HOST_BINARY.to_string(),
        Some(path) => match path.strip_suffix(DELETED) {
            None => path.to_string(),
            Some(stripped) if !stripped.is_empty() && exists(stripped) => stripped.to_string(),
            Some(_) => HOST_BINARY.to_string(),
        },
    }
}

/// Environment-dependent inputs, passed in so tests do not read the process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayConfig {
    /// Value of `$TERMINAL`, if set.
    pub terminal: Option<String>,
    /// Path to the running `linux-quest-host` binary.
    pub host_exe: String,
}

impl TrayConfig {
    pub fn from_env() -> Self {
        Self {
            terminal: std::env::var("TERMINAL").ok(),
            host_exe: resolve_host_exe(
                std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
                    .as_deref(),
                |p| std::path::Path::new(p).exists(),
            ),
        }
    }
}

/// Whether the event loop should keep running after an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

pub struct TrayController<S, H, R> {
    source: S,
    host: H,
    runner: R,
    config: TrayConfig,
    last_published: Option<TrayView>,
    host_failing: bool,
    poll_timeout: Duration,
    action_timeout: Duration,
    daemon: Option<Arc<dyn DaemonControl>>,
    /// The screen picker is open (set by the task that waits for it).
    choosing: Arc<AtomicBool>,
    /// The daemon failed the status request; not asked again until then.
    status_unsupported_until: Option<Instant>,
}

impl<S, H, R> TrayController<S, H, R>
where
    S: TelemetrySource,
    H: TrayHost,
    R: CommandRunner,
{
    pub fn new(source: S, host: H, runner: R, config: TrayConfig) -> Self {
        Self {
            source,
            host,
            runner,
            config,
            last_published: None,
            host_failing: false,
            poll_timeout: POLL_TIMEOUT,
            action_timeout: ACTION_TIMEOUT,
            daemon: None,
            choosing: Arc::new(AtomicBool::new(false)),
            status_unsupported_until: None,
        }
    }

    /// Enables the daemon actions (pause, choose screen) and the status details.
    pub fn with_daemon_control(mut self, daemon: Arc<dyn DaemonControl>) -> Self {
        self.daemon = Some(daemon);
        self
    }

    /// The view last shown.
    pub fn current_view(&self) -> Option<&TrayView> {
        self.last_published.as_ref()
    }

    /// The daemon's status, unless it is an older one that does not answer it.
    async fn daemon_status(&mut self) -> Option<DaemonStatus> {
        let daemon = self.daemon.clone()?;
        if self
            .status_unsupported_until
            .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        match tokio::time::timeout(self.poll_timeout, daemon.status()).await {
            Ok(Ok(status)) => {
                self.status_unsupported_until = None;
                Some(status)
            }
            Ok(Err(err)) => {
                debug!(error = %err, "daemon has no status request (older version?)");
                self.status_unsupported_until = Some(Instant::now() + STATUS_RETRY);
                None
            }
            Err(_) => None,
        }
    }

    /// Overrides the poll and action timeouts (tests use short ones).
    #[cfg(test)]
    fn with_timeouts(mut self, poll: Duration, action: Duration) -> Self {
        self.poll_timeout = poll;
        self.action_timeout = action;
        self
    }

    /// Polls the daemon once and publishes the view if it changed (or was never shown).
    pub async fn refresh(&mut self) {
        let view = match tokio::time::timeout(self.poll_timeout, self.source.fetch_snapshot()).await
        {
            Ok(Ok(snapshot)) => {
                let status = self.daemon_status().await;
                TrayView::from_daemon(
                    &snapshot,
                    status.as_ref(),
                    self.choosing.load(Ordering::SeqCst),
                )
            }
            Err(_elapsed) => {
                debug!(timeout = ?self.poll_timeout, "daemon status poll timed out");
                TrayView::for_state(ConnectionState::DaemonOffline)
            }
            Ok(Err(err)) => {
                debug!(error = %err, "daemon status unavailable");
                TrayView::for_state(ConnectionState::DaemonOffline)
            }
        };
        if self.last_published.as_ref() == Some(&view) {
            return;
        }

        match self.host.publish(&view).await {
            Ok(()) => {
                if self.host_failing {
                    info!("system tray host is available; tray icon shown");
                    self.host_failing = false;
                }
                self.last_published = Some(view);
            }
            Err(err) => {
                // Warn once per outage, then retry quietly on each poll.
                if !self.host_failing {
                    warn!(
                        error = %err,
                        "could not show tray icon (no StatusNotifier host?); \
                         continuing without it and retrying"
                    );
                    self.host_failing = true;
                }
                self.last_published = None;
            }
        }
    }

    /// Runs a menu action. Failures are logged and do not stop the tray.
    pub async fn handle(&mut self, action: TrayAction) -> Flow {
        match action {
            TrayAction::OpenApp => {
                let cmd = app_command(&self.config.host_exe, |p| p.exists());
                let spawned =
                    tokio::time::timeout(self.action_timeout, self.runner.spawn_detached(&cmd))
                        .await
                        .unwrap_or_else(|_| Err("timed out starting the app".to_string()));
                if let Err(err) = spawned {
                    warn!(
                        error = %err,
                        "could not open the Linux Quest app; install it with \
                         `cargo install --path crates/desktop`"
                    );
                }
                Flow::Continue
            }
            TrayAction::ToggleStreaming => {
                let current = self
                    .last_published
                    .as_ref()
                    .and_then(|v| v.streaming_enabled);
                if let (Some(daemon), Some(enabled)) = (self.daemon.clone(), current) {
                    match tokio::time::timeout(self.action_timeout, daemon.set_streaming(!enabled))
                        .await
                    {
                        Ok(Ok(())) => {
                            info!(streaming = !enabled, "streaming toggled from the tray")
                        }
                        Ok(Err(err)) => warn!(error = %err, "could not change streaming"),
                        Err(_) => warn!("changing streaming timed out"),
                    }
                    self.refresh().await;
                }
                Flow::Continue
            }
            TrayAction::ChooseScreen => {
                let Some(daemon) = self.daemon.clone() else {
                    return Flow::Continue;
                };
                if self.choosing.swap(true, Ordering::SeqCst) {
                    return Flow::Continue;
                }
                // The user may take minutes in the dialog; the tray keeps running meanwhile.
                let choosing = self.choosing.clone();
                tokio::spawn(async move {
                    match daemon.choose_screen().await {
                        Ok(()) => info!("screen chosen from the tray"),
                        Err(err) => warn!(error = %err, "no screen chosen"),
                    }
                    choosing.store(false, Ordering::SeqCst);
                });
                self.refresh().await;
                Flow::Continue
            }
            TrayAction::OpenDashboard => {
                let cmd = dashboard_command(self.config.terminal.as_deref(), &self.config.host_exe);
                let spawned =
                    tokio::time::timeout(self.action_timeout, self.runner.spawn_detached(&cmd))
                        .await
                        .unwrap_or_else(|_| Err("timed out starting terminal".to_string()));
                if let Err(err) = spawned {
                    warn!(
                        error = %err,
                        "could not open dashboard; set $TERMINAL to a terminal emulator that accepts -e"
                    );
                }
                Flow::Continue
            }
            TrayAction::RestartService => {
                let cmd = restart_command();
                match tokio::time::timeout(self.action_timeout, self.runner.run_to_completion(&cmd))
                    .await
                {
                    Ok(Ok(())) => info!("requested restart of linux-quest-host"),
                    Ok(Err(err)) => {
                        tracing::error!(error = %err, "could not restart linux-quest-host")
                    }
                    Err(_elapsed) => tracing::error!(
                        timeout = ?self.action_timeout,
                        "restarting linux-quest-host timed out; gave up waiting"
                    ),
                }
                Flow::Continue
            }
            TrayAction::Quit => Flow::Quit,
        }
    }

    /// Removes the icon from the tray host.
    pub async fn shutdown(&mut self) {
        self.host.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray::commands::LaunchCommand;
    use crate::tui::{DashboardSnapshot, MockTelemetrySource};
    use crate::HostError;
    use linux_quest_protocol::ClientConnectionMode;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;

    // Scripted poll results: `None` is a daemon error, `Some(mode)` a snapshot.
    struct ScriptedSource(VecDeque<Option<ClientConnectionMode>>);

    #[async_trait]
    impl TelemetrySource for ScriptedSource {
        async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError> {
            match self.0.pop_front().flatten() {
                Some(mode) => {
                    MockTelemetrySource::new()
                        .with_connection_mode(mode)
                        .fetch_snapshot()
                        .await
                }
                None => Err(HostError::Ipc("daemon not running".into())),
            }
        }
    }

    #[derive(Clone, Default)]
    struct MockHost {
        published: Arc<Mutex<Vec<TrayView>>>,
        // Number of upcoming publish calls that should fail.
        failures: Arc<Mutex<usize>>,
        closed: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl TrayHost for MockHost {
        async fn publish(&mut self, view: &TrayView) -> Result<(), String> {
            {
                let mut failures = self.failures.lock();
                if *failures > 0 {
                    *failures -= 1;
                    return Err("no StatusNotifier host".into());
                }
            }
            self.published.lock().push(view.clone());
            Ok(())
        }

        async fn close(&mut self) {
            *self.closed.lock() = true;
        }
    }

    #[derive(Clone, Default)]
    struct MockRunner {
        calls: Arc<Mutex<Vec<(&'static str, LaunchCommand)>>>,
        fail: bool,
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn spawn_detached(&mut self, cmd: &LaunchCommand) -> Result<(), String> {
            self.calls.lock().push(("detached", cmd.clone()));
            if self.fail {
                Err("no terminal".into())
            } else {
                Ok(())
            }
        }

        async fn run_to_completion(&mut self, cmd: &LaunchCommand) -> Result<(), String> {
            self.calls.lock().push(("wait", cmd.clone()));
            if self.fail {
                Err("systemctl failed".into())
            } else {
                Ok(())
            }
        }
    }

    fn config(terminal: Option<&str>) -> TrayConfig {
        TrayConfig {
            terminal: terminal.map(str::to_string),
            host_exe: "/usr/bin/linux-quest-host".into(),
        }
    }

    fn states(published: &Arc<Mutex<Vec<TrayView>>>) -> Vec<ConnectionState> {
        published.lock().iter().map(|v| v.state).collect()
    }

    #[tokio::test]
    async fn publishes_on_first_poll_and_on_change_only() {
        let host = MockHost::default();
        let published = host.published.clone();
        let source = ScriptedSource(
            [
                Some(ClientConnectionMode::UsbAdb),
                Some(ClientConnectionMode::UsbAdb),
                Some(ClientConnectionMode::Wifi),
                Some(ClientConnectionMode::Wifi),
                None,
                Some(ClientConnectionMode::Disconnected),
            ]
            .into_iter()
            .collect(),
        );
        let mut c = TrayController::new(source, host, MockRunner::default(), config(None));
        for _ in 0..6 {
            c.refresh().await;
        }
        assert_eq!(
            states(&published),
            vec![
                ConnectionState::Usb,
                ConnectionState::Wifi,
                ConnectionState::DaemonOffline,
                ConnectionState::Disconnected,
            ]
        );
    }

    #[tokio::test]
    async fn keeps_running_without_tray_host_and_recovers() {
        let host = MockHost::default();
        *host.failures.lock() = 2;
        let published = host.published.clone();
        let source = ScriptedSource(vec![Some(ClientConnectionMode::Wifi); 4].into());
        let mut c = TrayController::new(source, host, MockRunner::default(), config(None));

        c.refresh().await; // fails
        c.refresh().await; // fails again, retried
        assert!(published.lock().is_empty());
        c.refresh().await; // host appears
        assert_eq!(states(&published), vec![ConnectionState::Wifi]);

        // The controller still handles actions while no host was present.
        assert_eq!(c.handle(TrayAction::Quit).await, Flow::Quit);
    }

    #[tokio::test]
    async fn open_dashboard_spawns_configured_terminal_detached() {
        let runner = MockRunner::default();
        let calls = runner.calls.clone();
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            runner,
            config(Some("kitty")),
        );
        assert_eq!(c.handle(TrayAction::OpenDashboard).await, Flow::Continue);
        assert_eq!(
            *calls.lock(),
            vec![(
                "detached",
                LaunchCommand {
                    program: "kitty".into(),
                    args: vec![
                        "-e".into(),
                        "/usr/bin/linux-quest-host".into(),
                        "dashboard".into()
                    ],
                }
            )]
        );
    }

    #[tokio::test]
    async fn restart_runs_systemctl_to_completion() {
        let runner = MockRunner::default();
        let calls = runner.calls.clone();
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            runner,
            config(None),
        );
        assert_eq!(c.handle(TrayAction::RestartService).await, Flow::Continue);
        assert_eq!(*calls.lock(), vec![("wait", restart_command())]);
    }

    #[tokio::test]
    async fn failed_actions_do_not_stop_the_tray() {
        let runner = MockRunner {
            fail: true,
            ..Default::default()
        };
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            runner,
            config(None),
        );
        assert_eq!(c.handle(TrayAction::OpenDashboard).await, Flow::Continue);
        assert_eq!(c.handle(TrayAction::RestartService).await, Flow::Continue);
    }

    // A source whose daemon never answers.
    struct HangingSource;

    #[async_trait]
    impl TelemetrySource for HangingSource {
        async fn fetch_snapshot(&mut self) -> Result<DashboardSnapshot, HostError> {
            std::future::pending().await
        }
    }

    // A runner whose commands never finish.
    struct HangingRunner;

    #[async_trait]
    impl CommandRunner for HangingRunner {
        async fn spawn_detached(&mut self, _cmd: &LaunchCommand) -> Result<(), String> {
            std::future::pending().await
        }
        async fn run_to_completion(&mut self, _cmd: &LaunchCommand) -> Result<(), String> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn hanging_daemon_poll_times_out_as_offline() {
        let host = MockHost::default();
        let published = host.published.clone();
        let mut c = TrayController::new(HangingSource, host, MockRunner::default(), config(None))
            .with_timeouts(Duration::from_millis(20), Duration::from_secs(5));
        tokio::time::timeout(Duration::from_secs(2), c.refresh())
            .await
            .expect("refresh must not hang on a wedged daemon");
        assert_eq!(states(&published), vec![ConnectionState::DaemonOffline]);
    }

    #[tokio::test]
    async fn hanging_actions_time_out_and_continue() {
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            HangingRunner,
            config(None),
        )
        .with_timeouts(Duration::from_secs(5), Duration::from_millis(20));
        let flow = tokio::time::timeout(Duration::from_secs(2), async {
            let a = c.handle(TrayAction::RestartService).await;
            let b = c.handle(TrayAction::OpenDashboard).await;
            (a, b)
        })
        .await
        .expect("actions must time out");
        assert_eq!(flow, (Flow::Continue, Flow::Continue));
    }

    /// A daemon whose status the test sets, recording what it is asked.
    #[derive(Default)]
    struct MockDaemon {
        status: Mutex<Option<DaemonStatus>>,
        status_calls: std::sync::atomic::AtomicU32,
        streaming: Mutex<Vec<bool>>,
        choose_calls: std::sync::atomic::AtomicU32,
        /// Released by the test to "answer" the screen picker.
        picked: tokio::sync::Notify,
    }

    #[async_trait]
    impl DaemonControl for MockDaemon {
        async fn status(&self) -> Result<DaemonStatus, String> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            self.status
                .lock()
                .clone()
                .ok_or_else(|| "unknown request".to_string())
        }
        async fn set_streaming(&self, enabled: bool) -> Result<(), String> {
            self.streaming.lock().push(enabled);
            if let Some(status) = self.status.lock().as_mut() {
                status.streaming_enabled = enabled;
            }
            Ok(())
        }
        async fn choose_screen(&self) -> Result<(), String> {
            self.choose_calls.fetch_add(1, Ordering::SeqCst);
            self.picked.notified().await;
            Ok(())
        }
    }

    fn new_daemon_status() -> DaemonStatus {
        DaemonStatus {
            version: "test".into(),
            uptime_secs: 1,
            streaming_enabled: true,
            video_streams: 1,
            capture: crate::ipc::DaemonCapture::Screen,
            can_choose_screen: true,
            bitrate_mbps: 30,
            adaptive_bitrate: true,
            link: ClientConnectionMode::Wifi,
            headset_connected: false,
            display_backend: "mock".into(),
        }
    }

    fn wifi_source(polls: usize) -> ScriptedSource {
        ScriptedSource(vec![Some(ClientConnectionMode::Wifi); polls].into())
    }

    #[tokio::test]
    async fn pause_and_resume_go_to_the_daemon_and_show_at_once() {
        let daemon = Arc::new(MockDaemon::default());
        *daemon.status.lock() = Some(new_daemon_status());
        let host = MockHost::default();
        let published = host.published.clone();
        let mut c = TrayController::new(wifi_source(4), host, MockRunner::default(), config(None))
            .with_daemon_control(daemon.clone());
        c.refresh().await;
        assert_eq!(c.current_view().unwrap().streaming_enabled, Some(true));

        assert_eq!(c.handle(TrayAction::ToggleStreaming).await, Flow::Continue);
        assert_eq!(*daemon.streaming.lock(), vec![false]);
        assert_eq!(c.current_view().unwrap().streaming_enabled, Some(false));
        assert_eq!(
            published.lock().last().unwrap().detail.as_deref(),
            Some("Streaming paused")
        );
        c.handle(TrayAction::ToggleStreaming).await;
        assert_eq!(*daemon.streaming.lock(), vec![false, true]);
    }

    #[tokio::test]
    async fn choose_screen_runs_in_the_background_and_only_once_at_a_time() {
        let daemon = Arc::new(MockDaemon::default());
        *daemon.status.lock() = Some(new_daemon_status());
        let mut c = TrayController::new(
            wifi_source(6),
            MockHost::default(),
            MockRunner::default(),
            config(None),
        )
        .with_daemon_control(daemon.clone());
        c.refresh().await;
        // The handler returns while the picker is still open.
        assert_eq!(c.handle(TrayAction::ChooseScreen).await, Flow::Continue);
        assert!(c.current_view().unwrap().choosing_screen);
        c.handle(TrayAction::ChooseScreen).await;
        tokio::task::yield_now().await;
        assert_eq!(daemon.choose_calls.load(Ordering::SeqCst), 1);

        daemon.picked.notify_one();
        for _ in 0..100 {
            if !c.choosing.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        c.refresh().await;
        assert!(!c.current_view().unwrap().choosing_screen);
    }

    #[tokio::test]
    async fn an_older_daemon_is_not_asked_for_its_status_every_poll() {
        let daemon = Arc::new(MockDaemon::default());
        let mut c = TrayController::new(
            wifi_source(3),
            MockHost::default(),
            MockRunner::default(),
            config(None),
        )
        .with_daemon_control(daemon.clone());
        c.refresh().await;
        c.refresh().await;
        assert_eq!(daemon.status_calls.load(Ordering::SeqCst), 1);
        let view = c.current_view().unwrap();
        assert_eq!(view.state, ConnectionState::Wifi);
        assert_eq!(view.streaming_enabled, None, "no pause item for it");
    }

    #[tokio::test]
    async fn open_app_starts_the_desktop_app() {
        let runner = MockRunner::default();
        let calls = runner.calls.clone();
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            runner,
            config(None),
        );
        assert_eq!(c.handle(TrayAction::OpenApp).await, Flow::Continue);
        let calls = calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "detached");
        assert!(calls[0].1.program.ends_with("linux-quest"));
    }

    #[test]
    fn deleted_exe_suffix_is_stripped_when_target_exists() {
        let exe = resolve_host_exe(Some("/usr/bin/linux-quest-host (deleted)"), |_| true);
        assert_eq!(exe, "/usr/bin/linux-quest-host");
    }

    #[test]
    fn deleted_exe_falls_back_to_path_lookup() {
        assert_eq!(
            resolve_host_exe(Some("/usr/bin/linux-quest-host (deleted)"), |_| false),
            "linux-quest-host"
        );
        assert_eq!(resolve_host_exe(None, |_| true), "linux-quest-host");
    }

    #[test]
    fn normal_exe_path_is_unchanged() {
        assert_eq!(
            resolve_host_exe(Some("/opt/x/linux-quest-host"), |_| false),
            "/opt/x/linux-quest-host"
        );
    }

    #[tokio::test]
    async fn quit_does_not_run_commands() {
        let runner = MockRunner::default();
        let calls = runner.calls.clone();
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            MockHost::default(),
            runner,
            config(None),
        );
        assert_eq!(c.handle(TrayAction::Quit).await, Flow::Quit);
        assert!(calls.lock().is_empty());
    }

    #[tokio::test]
    async fn shutdown_closes_host() {
        let host = MockHost::default();
        let closed = host.closed.clone();
        let mut c = TrayController::new(
            ScriptedSource(VecDeque::new()),
            host,
            MockRunner::default(),
            config(None),
        );
        c.shutdown().await;
        assert!(*closed.lock());
    }
}
