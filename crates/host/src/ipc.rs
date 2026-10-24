use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use crate::congestion::RateCoordinator;
use crate::headset::{HeadsetHub, APPLY_TIMEOUT};
use crate::link_status::{LinkStatus, SharedLinkTracker};
use crate::tui::model::DashboardSnapshot;
use crate::video_sender::control::{CaptureKind, VideoControl, CHOOSE_SCREEN_TIMEOUT};
use crate::MultiDisplayHost;
use linux_quest_display::{DisplayLayoutMode, VirtualMonitor, VirtualMonitorConfig};
use linux_quest_protocol::{
    ClientConnectionMode, HeadsetSettings, HeadsetSettingsPatch, HeadsetStats, HeadsetStatus,
    ScreenPlacement,
};

/// Request sent from a client (CLI, tray, desktop app) to the background daemon.
///
/// New variants go at the end: bincode encodes the variant by its index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonRequest {
    Toggle {
        count: u32,
        width: u32,
        height: u32,
        refresh: u32,
        dpi: u32,
        layout: DisplayLayoutMode,
    },
    List,
    Remove {
        id: u32,
    },
    Layout {
        mode: DisplayLayoutMode,
    },
    DashboardSnapshot,
    /// The daemon's own state ([`DaemonStatus`]).
    Status,
    /// The headset's status, layout and settings as it last reported them
    /// ([`DaemonResponse::Headset`]; `None` when no headset is on the settings channel).
    GetHeadset,
    /// Places the headset's screens. Answered with the headset's new status, which shows what
    /// it applied (it clamps to its comfort limits).
    SetHeadsetLayout {
        screens: Vec<ScreenPlacement>,
    },
    /// Changes the headset's settings; the bitrate also becomes the host's video bitrate.
    /// Answered with the headset's new status.
    SetHeadsetSettings {
        settings: HeadsetSettings,
    },
    /// Sets the host's video bitrate: the ceiling, adapting below it, or fixed.
    SetBitrate {
        mbps: u32,
        adaptive: bool,
    },
    /// Pauses (`false`) or resumes (`true`) video to the headset.
    SetStreaming {
        enabled: bool,
    },
    /// Shows GNOME's screen-share dialog so the user can pick what the desktop screen shows.
    /// The answer comes once they picked or cancelled, up to about five minutes later.
    ChooseScreen,
    /// Changes only the headset settings `patch` carries; the rest keep the values the headset
    /// last reported. The host's video bitrate follows only when the patch changes the
    /// bitrate. Answered with the headset's new status.
    PatchHeadsetSettings {
        patch: HeadsetSettingsPatch,
    },
}

/// Response returned from the daemon. New variants go at the end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    Monitors(Vec<VirtualMonitor>),
    DashboardSnapshot(Box<crate::tui::model::DashboardSnapshot>),
    Success(String),
    Error(String),
    Status(Box<DaemonStatus>),
    Headset(Option<Box<HeadsetReport>>),
}

/// What stream 0 shows, on the IPC wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonCapture {
    /// No video is sent (`--no-video`, or no capture or encoder could start).
    None,
    /// The desktop, through the screen-cast portal.
    Screen,
    /// The synthetic test pattern.
    TestPattern,
}

impl From<CaptureKind> for DaemonCapture {
    fn from(kind: CaptureKind) -> Self {
        match kind {
            CaptureKind::None => Self::None,
            CaptureKind::Screen => Self::Screen,
            CaptureKind::TestPattern => Self::TestPattern,
        }
    }
}

/// The daemon's state, for the desktop app and the tray.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub uptime_secs: u64,
    /// Video goes to the headset when one is connected (not paused).
    pub streaming_enabled: bool,
    /// Video streams running; 0 when the video sender did not start.
    pub video_streams: u16,
    pub capture: DaemonCapture,
    /// A screen capture runs that can show the screen picker.
    pub can_choose_screen: bool,
    /// Video bitrate ceiling in Mbit/s (0 when the daemon has no video).
    pub bitrate_mbps: u32,
    pub adaptive_bitrate: bool,
    /// Transport of the connected headset.
    pub link: ClientConnectionMode,
    /// A headset answers on the settings channel.
    pub headset_connected: bool,
    /// Backend that creates virtual monitors.
    pub display_backend: String,
}

/// The headset's last report and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadsetReport {
    pub status: HeadsetStatus,
    /// Address of its control connection (`127.0.0.1` through the USB tunnel).
    pub peer: String,
    /// How long ago it reported, in milliseconds.
    pub age_ms: u64,
}

/// What the IPC server works with.
#[derive(Clone)]
pub struct IpcContext {
    pub host: Arc<MultiDisplayHost>,
    pub link: SharedLinkTracker,
    pub headset: Arc<HeadsetHub>,
    pub video: Arc<VideoControl>,
    /// The video bitrate controller, when video runs.
    pub rate: Option<Arc<RateCoordinator>>,
}

impl IpcContext {
    /// A context with no headset channel, video or rate controller of its own yet.
    pub fn new(host: Arc<MultiDisplayHost>, link: SharedLinkTracker) -> Self {
        Self {
            host,
            link,
            headset: Arc::new(HeadsetHub::new()),
            video: VideoControl::new(),
            rate: None,
        }
    }
}

/// Maximum allowed IPC payload frame size (1 Megabyte).
pub const MAX_IPC_FRAME_SIZE: usize = 1024 * 1024;

/// Overrides [`daemon_runtime_dir`] (for a test or demo daemon next to the real one).
pub const RUNTIME_DIR_ENV: &str = "LINUX_QUEST_RUNTIME_DIR";

/// Returns the private runtime directory for the DeskQuest daemon:
/// `$LINUX_QUEST_RUNTIME_DIR` if set, else the active DeskQuest socket directory when present,
/// falling back to `$XDG_RUNTIME_DIR/linux-quest` for older installs.
pub fn daemon_runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let runtime_dir = Path::new(&runtime_dir);
        let deskquest_dir = runtime_dir.join("deskquest");
        if deskquest_dir.join("daemon.sock").exists() {
            deskquest_dir
        } else {
            runtime_dir.join("linux-quest")
        }
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/linux-quest-{uid}"))
    }
}

/// Returns the standard Unix domain socket path for the DeskQuest daemon.
pub fn daemon_socket_path() -> PathBuf {
    daemon_runtime_dir().join("daemon.sock")
}

/// Returns the standard PID file path for the DeskQuest daemon.
pub fn daemon_pid_path() -> PathBuf {
    daemon_runtime_dir().join("daemon.pid")
}

/// Runs the IPC listener loop inside the daemon process.
pub async fn run_ipc_server(
    host: Arc<MultiDisplayHost>,
    link: SharedLinkTracker,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_ipc_server_with(IpcContext::new(host, link), shutdown_rx).await
}

/// [`run_ipc_server`] with the daemon's headset hub, video control and rate controller.
pub async fn run_ipc_server_with(
    ctx: IpcContext,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    mark_daemon_start();
    let runtime_dir = daemon_runtime_dir();
    let socket_path = daemon_socket_path();
    let pid_path = daemon_pid_path();

    // 1. Ensure private runtime directory exists with restrictive 0700 permissions
    std::fs::create_dir_all(&runtime_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    }

    // 2. Acquire exclusive non-blocking PID lock via libc::flock BEFORE any socket operations
    let pid_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&pid_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pid_path, std::fs::Permissions::from_mode(0o600))?;
    }

    use std::os::unix::io::AsRawFd;
    let lock_res = unsafe { libc::flock(pid_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_res != 0 {
        return Err("Another instance of linux-quest-host daemon holds the PID file lock".into());
    }

    // Write current PID into lockfile
    use std::io::Write;
    let mut pid_file_writer = pid_file;
    pid_file_writer.set_len(0)?;
    writeln!(pid_file_writer, "{}", std::process::id())?;
    pid_file_writer.flush()?;

    // 3. Now that PID lock is held, probe socket and clean up stale socket if present
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).await.is_ok() {
            return Err(
                "Another instance of linux-quest-host daemon is already running (socket is active)"
                    .into(),
            );
        }
        // Stale socket from previous ungraceful termination
        std::fs::remove_file(&socket_path)?;
    }

    // 4. Bind socket and enforce 0600 permissions
    let listener = UnixListener::bind(&socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    info!(path = ?socket_path, "DeskQuest daemon IPC socket bound (mode 0600)");

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_ipc_client(stream, ctx).await {
                                warn!("IPC client error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("IPC listener accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                info!("IPC server shutting down");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

static DAEMON_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Records the daemon start time (first call wins). Called when the IPC server or the live
/// telemetry source is created, so uptime counts from daemon startup.
pub fn mark_daemon_start() {
    DAEMON_START.get_or_init(std::time::Instant::now);
}

/// Seconds since [`mark_daemon_start`] was first called (0 if it never was).
pub fn daemon_uptime_secs() -> u64 {
    DAEMON_START.get().map_or(0, |t| t.elapsed().as_secs())
}

/// Fills the per-display bitrate columns and the client's total from the video senders'
/// rates. Streams are matched to displays by id; displays without a stream keep zeros.
pub fn apply_stream_rates(
    snapshot: &mut DashboardSnapshot,
    rates: &[crate::congestion::StreamRateSnapshot],
) {
    for display in &mut snapshot.displays {
        if let Some(rate) = rates.iter().find(|r| r.stream_id == display.id) {
            display.bitrate_mbps = rate.actual_kbps as f32 / 1000.0;
            display.target_bitrate_mbps = rate.target_kbps as f32 / 1000.0;
        }
    }
    snapshot.client.bitrate_mbps = rates.iter().map(|r| r.actual_kbps).sum::<u32>() as f32 / 1000.0;
}

/// Builds the dashboard snapshot from the live virtual monitors and the derived link state.
///
/// `client.mode` comes only from `link`. `streaming` is true only when a client is connected
/// AND at least one virtual display exists. This means "link up with something to stream", not
/// that frames are flowing: UDP video starts only when the capture and encoder backends could
/// be opened, and never over the USB (ADB reverse) path.
///
/// The wire layout of [`DashboardSnapshot`] is fixed (tray and TUI depend on it). Metrics the
/// host does not measure yet (bitrate, fps, latency breakdowns, RTT, packet counters, audio
/// stats) are reported as zero / empty instead of made-up numbers.
pub fn build_dashboard_snapshot(
    monitors: Vec<VirtualMonitor>,
    link: LinkStatus,
    uptime_secs: u64,
) -> DashboardSnapshot {
    let displays: Vec<crate::tui::model::DisplaySnapshot> = monitors
        .into_iter()
        .map(|m| crate::tui::model::DisplaySnapshot {
            id: m.id as u16,
            name: m.name,
            width: m.width,
            height: m.height,
            refresh_rate: m.refresh_rate,
            fps: 0.0,
            bitrate_mbps: 0.0,
            target_bitrate_mbps: 0.0,
            codec: String::new(),
            is_virtual: m.is_virtual,
        })
        .collect();

    let streaming = link.connected && displays.iter().any(|d| d.is_virtual);
    let conn_mode = link.connection_mode();

    // Not measured yet: all-zero breakdowns (totals stay zero).
    let video_latency = linux_quest_protocol::FrameLatencyBreakdown::default();
    let audio_latency = linux_quest_protocol::AudioLatencyBreakdown::default();

    DashboardSnapshot {
        uptime_secs,
        streaming,
        displays,
        video_latency,
        audio_latency,
        client: linux_quest_protocol::ClientTransportTelemetry {
            mode: conn_mode,
            rtt_us: 0,
            smoothed_rtt_us: 0,
            packet_loss_rate: 0.0,
            packets_sent: 0,
            packets_recv: 0,
            packets_lost: 0,
            fec_recoveries: 0,
            bitrate_mbps: 0.0,
        },
        audio: linux_quest_protocol::AudioStreamTelemetry {
            active: false,
            codec: String::new(),
            sample_rate: 0,
            channels: 0,
            frame_size_ms: 0.0,
            bitrate_kbps: 0,
            jitter_us: 0,
            buffer_delay_us: 0,
            buffer_health_pct: 0.0,
            plc_count: 0,
        },
    }
}

/// Fills what the headset measures into the snapshot: packet counters and its decode and
/// motion-to-photon times.
pub fn apply_headset_stats(snapshot: &mut DashboardSnapshot, stats: &HeadsetStats) {
    let client = &mut snapshot.client;
    client.packets_recv = stats.packets_received;
    client.packets_lost = stats.packets_lost;
    client.fec_recoveries = stats.fec_recovered;
    let total = stats.packets_received + stats.packets_lost;
    client.packet_loss_rate = if total == 0 {
        0.0
    } else {
        stats.packets_lost as f32 / total as f32
    };
    snapshot.video_latency.client_decode_duration_us =
        u32::try_from(stats.decode_latency_us).unwrap_or(u32::MAX);
    snapshot.video_latency.total_motion_to_photon_us =
        (stats.motion_to_photon_ms.max(0.0) * 1000.0).round() as u32;
}

/// The daemon's state for [`DaemonRequest::Status`].
pub async fn daemon_status(ctx: &IpcContext) -> DaemonStatus {
    let link = ctx.link.lock().status(std::time::Instant::now());
    DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: daemon_uptime_secs(),
        streaming_enabled: !ctx.video.is_paused(),
        video_streams: ctx.video.running_streams(),
        capture: ctx.video.capture_kind().into(),
        can_choose_screen: ctx.video.can_choose_screen(),
        bitrate_mbps: ctx.rate.as_ref().map_or(0, |r| r.max_kbps() / 1000),
        adaptive_bitrate: ctx.rate.as_ref().is_some_and(|r| r.is_adaptive()),
        link: link.connection_mode(),
        headset_connected: ctx.headset.is_connected(),
        display_backend: ctx.host.backend_name().await.to_string(),
    }
}

/// The headset's last report, for [`DaemonRequest::GetHeadset`].
pub fn headset_report(hub: &HeadsetHub) -> Option<Box<HeadsetReport>> {
    hub.status().map(|(status, peer, at)| {
        Box::new(HeadsetReport {
            status,
            peer: peer.to_string(),
            age_ms: at.elapsed().as_millis() as u64,
        })
    })
}

/// Answers one request.
pub async fn handle_request(ctx: &IpcContext, req: DaemonRequest) -> DaemonResponse {
    let headset_answer = |result: Result<HeadsetStatus, crate::HeadsetError>| match result {
        Ok(_) => DaemonResponse::Headset(headset_report(&ctx.headset)),
        Err(e) => DaemonResponse::Error(e.to_string()),
    };
    match req {
        DaemonRequest::Toggle {
            count,
            width,
            height,
            refresh,
            dpi,
            layout,
        } => {
            let base_cfg =
                VirtualMonitorConfig::new(1, "Quest-Virtual", width, height, refresh).with_dpi(dpi);
            match ctx.host.toggle_monitors(count, &base_cfg, layout).await {
                Ok(monitors) => DaemonResponse::Monitors(monitors),
                Err(e) => DaemonResponse::Error(e.to_string()),
            }
        }
        DaemonRequest::List => DaemonResponse::Monitors(ctx.host.list_monitors().await),
        DaemonRequest::Remove { id } => match ctx.host.remove_monitor(id).await {
            Ok(()) => DaemonResponse::Success(format!("Removed virtual monitor ID {id}")),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::Layout { mode } => match ctx.host.set_layout(mode).await {
            Ok(()) => DaemonResponse::Success(format!("Applied layout: {mode:?}")),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::DashboardSnapshot => {
            let monitors = ctx.host.list_monitors().await;
            let status = ctx.link.lock().status(std::time::Instant::now());
            let connected = status.connected;
            let mut snapshot = build_dashboard_snapshot(monitors, status, daemon_uptime_secs());
            apply_stream_rates(&mut snapshot, &crate::congestion::global_snapshot());
            // Video to a connected headset is streaming, virtual monitors or not (stream 0
            // is usually the desktop itself).
            if connected && ctx.video.running_streams() > 0 {
                snapshot.streaming = true;
            }
            if ctx.video.is_paused() {
                snapshot.streaming = false;
            }
            if let Some((headset, _, _)) = ctx.headset.status() {
                apply_headset_stats(&mut snapshot, &headset.stats);
            }
            DaemonResponse::DashboardSnapshot(Box::new(snapshot))
        }
        DaemonRequest::Status => DaemonResponse::Status(Box::new(daemon_status(ctx).await)),
        DaemonRequest::GetHeadset => DaemonResponse::Headset(headset_report(&ctx.headset)),
        DaemonRequest::SetHeadsetLayout { screens } => {
            headset_answer(ctx.headset.apply_layout(screens, APPLY_TIMEOUT).await)
        }
        DaemonRequest::SetHeadsetSettings { settings } => {
            if let Some(rate) = &ctx.rate {
                if rate.max_kbps() != settings.bitrate_mbps.saturating_mul(1000)
                    || rate.is_adaptive() != settings.adaptive_bitrate
                {
                    rate.set_limit(
                        settings.bitrate_mbps.saturating_mul(1000),
                        settings.adaptive_bitrate,
                    );
                }
            }
            headset_answer(ctx.headset.apply_settings(settings, APPLY_TIMEOUT).await)
        }
        DaemonRequest::PatchHeadsetSettings { patch } => {
            let applied = ctx.headset.apply_settings_patch(patch, APPLY_TIMEOUT).await;
            if let (Ok(status), Some(rate)) = (&applied, &ctx.rate) {
                if patch.touches_bitrate() {
                    rate.set_limit(
                        status.settings.bitrate_mbps.saturating_mul(1000),
                        status.settings.adaptive_bitrate,
                    );
                }
            }
            headset_answer(applied)
        }
        DaemonRequest::SetBitrate { mbps, adaptive } => match &ctx.rate {
            _ if !(1..=500).contains(&mbps) => {
                DaemonResponse::Error(format!("bitrate {mbps} Mbit/s is outside 1 to 500"))
            }
            Some(rate) => {
                rate.set_limit(mbps.saturating_mul(1000), adaptive);
                DaemonResponse::Success(format!(
                    "Video bitrate {} {mbps} Mbit/s",
                    if adaptive { "up to" } else { "fixed at" }
                ))
            }
            None => DaemonResponse::Error("the daemon is not sending video".into()),
        },
        DaemonRequest::SetStreaming { enabled } => {
            ctx.video.set_paused(!enabled);
            DaemonResponse::Success(
                if enabled {
                    "Streaming to the headset"
                } else {
                    "Streaming paused"
                }
                .into(),
            )
        }
        DaemonRequest::ChooseScreen => match ctx.video.choose_screen(CHOOSE_SCREEN_TIMEOUT).await {
            Ok(()) => DaemonResponse::Success("Now sharing the chosen screen".into()),
            Err(e) => DaemonResponse::Error(e),
        },
    }
}

async fn handle_ipc_client(
    mut stream: UnixStream,
    ctx: IpcContext,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read request length (4 bytes big-endian) and enforce frame cap
    let len = stream.read_u32().await? as usize;
    if len == 0 || len > MAX_IPC_FRAME_SIZE {
        return Err(format!("IPC payload size {len} is invalid or exceeds 1MB limit").into());
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    // 2. Process request. One a newer client sends that this daemon does not know gets an
    //    error answer instead of a dropped connection.
    let resp = match bincode::deserialize::<DaemonRequest>(&buf) {
        Ok(req) => handle_request(&ctx, req).await,
        Err(e) => DaemonResponse::Error(format!(
            "this daemon does not understand the request ({e}); is it older than the client?"
        )),
    };

    // 3. Send response
    let resp_bytes = bincode::serialize(&resp)?;
    stream.write_u32(resp_bytes.len() as u32).await?;
    stream.write_all(&resp_bytes).await?;
    stream.flush().await?;

    Ok(())
}

/// Sends a request from the CLI to the running daemon.
pub async fn send_daemon_request(
    req: DaemonRequest,
) -> Result<DaemonResponse, Box<dyn std::error::Error + Send + Sync>> {
    send_daemon_request_to(&daemon_socket_path(), req).await
}

/// [`send_daemon_request`] to the daemon listening on `socket_path`.
pub async fn send_daemon_request_to(
    socket_path: &Path,
    req: DaemonRequest,
) -> Result<DaemonResponse, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(socket_path).await?;

    let req_bytes = bincode::serialize(&req)?;
    stream.write_u32(req_bytes.len() as u32).await?;
    stream.write_all(&req_bytes).await?;
    stream.flush().await?;

    let resp_len = stream.read_u32().await? as usize;
    if resp_len == 0 || resp_len > MAX_IPC_FRAME_SIZE {
        return Err(
            format!("Daemon response size {resp_len} is invalid or exceeds 1MB limit").into(),
        );
    }
    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    let resp: DaemonResponse = bincode::deserialize(&resp_buf)?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link_status::{LinkTracker, DEFAULT_INPUT_LIVENESS};
    use linux_quest_protocol::ClientConnectionMode;
    use std::time::Instant;

    fn virtual_monitor(id: u32) -> VirtualMonitor {
        VirtualMonitor {
            id,
            name: format!("Quest-Virtual-{id}"),
            connector: format!("VIRTUAL{id}"),
            width: 2560,
            height: 1440,
            refresh_rate: 90,
            x: 0,
            y: 0,
            dpi: 96,
            is_virtual: true,
            edid_path: None,
        }
    }

    #[test]
    fn stream_rates_fill_the_target_and_actual_bitrate_columns() {
        let mut snap = build_dashboard_snapshot(
            vec![virtual_monitor(1), virtual_monitor(2), virtual_monitor(3)],
            LinkTracker::new(DEFAULT_INPUT_LIVENESS).status(Instant::now()),
            0,
        );
        apply_stream_rates(
            &mut snap,
            &[
                crate::congestion::StreamRateSnapshot {
                    stream_id: 1,
                    target_kbps: 20_000,
                    actual_kbps: 18_500,
                    fec_ratio: 0.25,
                },
                crate::congestion::StreamRateSnapshot {
                    stream_id: 2,
                    target_kbps: 10_000,
                    actual_kbps: 9_500,
                    fec_ratio: 0.25,
                },
            ],
        );
        assert_eq!(snap.displays[0].target_bitrate_mbps, 20.0);
        assert_eq!(snap.displays[0].bitrate_mbps, 18.5);
        assert_eq!(snap.displays[1].target_bitrate_mbps, 10.0);
        assert_eq!(
            snap.displays[2].target_bitrate_mbps, 0.0,
            "no stream for display 3"
        );
        assert_eq!(snap.client.bitrate_mbps, 28.0);
    }

    #[test]
    fn virtual_displays_without_a_client_report_disconnected() {
        // The original bug: displays existed, so the snapshot claimed USB streaming.
        let snap = build_dashboard_snapshot(
            vec![virtual_monitor(1), virtual_monitor(2)],
            LinkTracker::new(DEFAULT_INPUT_LIVENESS).status(Instant::now()),
            0,
        );

        assert_eq!(snap.displays.len(), 2);
        assert!(!snap.streaming);
        assert!(!snap.audio.active);
        assert_eq!(snap.client.mode, ClientConnectionMode::Disconnected);
    }

    #[test]
    fn snapshot_reports_usb_only_when_loopback_peer_and_reverse_tunnel() {
        let now = Instant::now();
        let mut tracker = LinkTracker::new(DEFAULT_INPUT_LIVENESS);
        tracker.adb_event(
            &linux_quest_transport::AdbBridgeEvent::ReversePortEstablished {
                serial: "QUEST".into(),
                port: 48444,
            },
        );
        tracker.control_accepted("127.0.0.1:50000".parse().unwrap(), now);

        let snap = build_dashboard_snapshot(vec![virtual_monitor(1)], tracker.status(now), 0);
        assert!(snap.streaming);
        assert_eq!(snap.client.mode, ClientConnectionMode::UsbAdb);
    }

    #[test]
    fn snapshot_reports_wifi_for_lan_peer() {
        let now = Instant::now();
        let mut tracker = LinkTracker::new(DEFAULT_INPUT_LIVENESS);
        tracker.control_accepted("192.168.1.20:50000".parse().unwrap(), now);

        let snap = build_dashboard_snapshot(vec![], tracker.status(now), 0);
        // Connected, but no virtual display: nothing to stream.
        assert!(!snap.streaming);
        assert_eq!(snap.client.mode, ClientConnectionMode::Wifi);

        let snap = build_dashboard_snapshot(vec![virtual_monitor(1)], tracker.status(now), 0);
        assert!(snap.streaming);
    }

    #[test]
    fn snapshot_has_no_fabricated_metrics() {
        let now = Instant::now();
        let mut tracker = LinkTracker::new(DEFAULT_INPUT_LIVENESS);
        tracker.control_accepted("192.168.1.20:50000".parse().unwrap(), now);

        let snap = build_dashboard_snapshot(vec![virtual_monitor(1)], tracker.status(now), 7);
        assert_eq!(snap.uptime_secs, 7);
        assert_eq!(snap.client.rtt_us, 0);
        assert_eq!(snap.client.bitrate_mbps, 0.0);
        assert_eq!(snap.client.packets_sent, 0);
        assert_eq!(snap.video_latency.total_motion_to_photon_us, 0);
        assert_eq!(snap.displays[0].bitrate_mbps, 0.0);
        assert!(!snap.audio.active);
    }

    fn context() -> IpcContext {
        let host = Arc::new(MultiDisplayHost::new(Box::new(
            linux_quest_display::backend::mock::MockDisplayBackend::new(),
        )));
        let link = crate::link_status::new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
        IpcContext {
            rate: Some(Arc::new(RateCoordinator::new(30_000, true))),
            ..IpcContext::new(host, link)
        }
    }

    fn expect_error(resp: DaemonResponse) -> String {
        match resp {
            DaemonResponse::Error(e) => e,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_reports_video_capture_bitrate_and_headset() {
        let ctx = context();
        ctx.video.set_running(3, CaptureKind::Screen);
        let _picker = ctx.video.register_reselect();
        let DaemonResponse::Status(status) = handle_request(&ctx, DaemonRequest::Status).await
        else {
            panic!("expected a status")
        };
        assert!(status.streaming_enabled);
        assert_eq!(status.video_streams, 3);
        assert_eq!(status.capture, DaemonCapture::Screen);
        assert!(status.can_choose_screen);
        assert_eq!((status.bitrate_mbps, status.adaptive_bitrate), (30, true));
        assert!(!status.headset_connected);
        assert_eq!(status.link, ClientConnectionMode::Disconnected);
    }

    #[tokio::test]
    async fn pausing_stops_streaming_in_the_snapshot_and_resuming_restores_it() {
        let ctx = context();
        ctx.video.set_running(1, CaptureKind::Screen);
        ctx.link
            .lock()
            .input_packet("192.168.1.20:48442".parse().unwrap(), Instant::now());
        let streaming = |resp| match resp {
            DaemonResponse::DashboardSnapshot(snap) => snap.streaming,
            other => panic!("{other:?}"),
        };
        // No virtual monitor, but the desktop stream goes to a connected headset.
        assert!(streaming(
            handle_request(&ctx, DaemonRequest::DashboardSnapshot).await
        ));
        handle_request(&ctx, DaemonRequest::SetStreaming { enabled: false }).await;
        assert!(ctx.video.is_paused());
        assert!(!streaming(
            handle_request(&ctx, DaemonRequest::DashboardSnapshot).await
        ));
        handle_request(&ctx, DaemonRequest::SetStreaming { enabled: true }).await;
        assert!(streaming(
            handle_request(&ctx, DaemonRequest::DashboardSnapshot).await
        ));
    }

    #[tokio::test]
    async fn bitrate_changes_reach_the_rate_controller() {
        let ctx = context();
        let resp = handle_request(
            &ctx,
            DaemonRequest::SetBitrate {
                mbps: 50,
                adaptive: false,
            },
        )
        .await;
        assert!(matches!(resp, DaemonResponse::Success(_)), "{resp:?}");
        let rate = ctx.rate.as_ref().unwrap();
        assert_eq!((rate.max_kbps(), rate.is_adaptive()), (50_000, false));
        let err = expect_error(
            handle_request(
                &ctx,
                DaemonRequest::SetBitrate {
                    mbps: 0,
                    adaptive: true,
                },
            )
            .await,
        );
        assert!(err.contains("outside"), "{err}");
        let no_video = IpcContext {
            rate: None,
            ..context()
        };
        let err = expect_error(
            handle_request(
                &no_video,
                DaemonRequest::SetBitrate {
                    mbps: 20,
                    adaptive: true,
                },
            )
            .await,
        );
        assert!(err.contains("not sending video"), "{err}");
    }

    #[tokio::test]
    async fn headset_requests_without_a_headset_fail_with_a_reason() {
        let ctx = context();
        assert!(matches!(
            handle_request(&ctx, DaemonRequest::GetHeadset).await,
            DaemonResponse::Headset(None)
        ));
        let status = crate::headset::tests::status("q");
        let err = expect_error(
            handle_request(
                &ctx,
                DaemonRequest::SetHeadsetLayout {
                    screens: status.screens,
                },
            )
            .await,
        );
        assert!(err.contains("no headset"), "{err}");
    }

    #[tokio::test]
    async fn headset_settings_go_to_the_headset_and_the_bitrate_to_the_host() {
        let ctx = context();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let id = ctx.headset.next_peer_id();
        ctx.headset
            .status_received(id, peer, &tx, crate::headset::tests::status("Quest 3"));
        let hub = ctx.headset.clone();
        let headset = tokio::spawn(async move {
            let Some(linux_quest_transport::ControlMessage::SetHeadsetSettings(settings)) =
                rx.recv().await
            else {
                panic!("expected settings")
            };
            let mut next = crate::headset::tests::status("Quest 3");
            next.settings = settings;
            hub.status_received(id, peer, &tx, next);
        });
        let mut settings = crate::headset::tests::status("Quest 3").settings;
        settings.bitrate_mbps = 45;
        settings.adaptive_bitrate = false;
        settings.passthrough = false;
        let resp = handle_request(&ctx, DaemonRequest::SetHeadsetSettings { settings }).await;
        let DaemonResponse::Headset(Some(report)) = resp else {
            panic!("expected the headset's report, got {resp:?}")
        };
        assert!(!report.status.settings.passthrough);
        assert_eq!(report.peer, "127.0.0.1:50000");
        let rate = ctx.rate.as_ref().unwrap();
        assert_eq!((rate.max_kbps(), rate.is_adaptive()), (45_000, false));
        headset.await.unwrap();
    }

    /// Issue #114: changing one setting must not overwrite the headset's bitrate (80) with
    /// the host's (30), on the headset or in the host's encoder.
    #[tokio::test]
    async fn a_settings_patch_keeps_the_headsets_bitrate_and_the_hosts_rate() {
        let ctx = context();
        let rate = ctx.rate.as_ref().unwrap().clone();
        let host_rate = (rate.max_kbps(), rate.is_adaptive());
        let reported = crate::headset::tests::status("Quest 3");
        assert_eq!(reported.settings.bitrate_mbps, 80);
        assert_ne!(host_rate.0, 80_000);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let id = ctx.headset.next_peer_id();
        ctx.headset.status_received(id, peer, &tx, reported.clone());
        let hub = ctx.headset.clone();
        let headset = tokio::spawn(async move {
            let Some(linux_quest_transport::ControlMessage::SetHeadsetSettings(settings)) =
                rx.recv().await
            else {
                panic!("expected settings")
            };
            let mut next = crate::headset::tests::status("Quest 3");
            next.settings = settings;
            hub.status_received(id, peer, &tx, next);
            settings
        });
        let patch = HeadsetSettingsPatch {
            hud: Some(!reported.settings.hud),
            ..Default::default()
        };
        let resp = handle_request(&ctx, DaemonRequest::PatchHeadsetSettings { patch }).await;
        let DaemonResponse::Headset(Some(report)) = resp else {
            panic!("expected the headset's report, got {resp:?}")
        };
        let sent = headset.await.unwrap();
        assert_eq!(
            sent,
            HeadsetSettings {
                hud: !reported.settings.hud,
                ..reported.settings
            },
            "only the HUD may change"
        );
        assert_eq!(report.status.settings.bitrate_mbps, 80);
        assert_eq!((rate.max_kbps(), rate.is_adaptive()), host_rate);
    }

    #[tokio::test]
    async fn choose_screen_needs_a_screen_capture_and_relays_its_answer() {
        let ctx = context();
        let err = expect_error(handle_request(&ctx, DaemonRequest::ChooseScreen).await);
        assert!(err.contains("no screen capture"), "{err}");

        let mut picker = ctx.video.register_reselect();
        let capture = tokio::spawn(async move {
            let request = picker.recv().await.unwrap();
            request.reply.send(Ok(())).unwrap();
        });
        assert!(matches!(
            handle_request(&ctx, DaemonRequest::ChooseScreen).await,
            DaemonResponse::Success(_)
        ));
        capture.await.unwrap();
    }

    #[tokio::test]
    async fn an_unknown_request_gets_an_error_answer() {
        let ctx = context();
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = tokio::spawn(async move { handle_ipc_client(server, ctx).await.is_ok() });
        // A variant index no daemon knows, as a newer client might send.
        let body = 4_000u32.to_le_bytes();
        client.write_u32(body.len() as u32).await.unwrap();
        client.write_all(&body).await.unwrap();
        let len = client.read_u32().await.unwrap() as usize;
        let mut buf = vec![0u8; len];
        client.read_exact(&mut buf).await.unwrap();
        let resp: DaemonResponse = bincode::deserialize(&buf).unwrap();
        assert!(expect_error(resp).contains("does not understand"));
        assert!(served.await.unwrap());
    }

    #[test]
    fn headset_stats_fill_the_snapshot() {
        let mut snap = build_dashboard_snapshot(
            vec![],
            LinkTracker::new(DEFAULT_INPUT_LIVENESS).status(Instant::now()),
            0,
        );
        apply_headset_stats(
            &mut snap,
            &HeadsetStats {
                packets_received: 990,
                packets_lost: 10,
                fec_recovered: 4,
                decode_latency_us: 2_500,
                motion_to_photon_ms: 31.5,
                ..Default::default()
            },
        );
        assert_eq!(snap.client.packets_lost, 10);
        assert!((snap.client.packet_loss_rate - 0.01).abs() < 1e-6);
        assert_eq!(snap.video_latency.client_decode_duration_us, 2_500);
        assert_eq!(snap.video_latency.total_motion_to_photon_us, 31_500);
    }

    #[test]
    fn existing_request_variants_keep_their_wire_index() {
        let index = |r: &DaemonRequest| {
            u32::from_le_bytes(bincode::serialize(r).unwrap()[..4].try_into().unwrap())
        };
        assert_eq!(index(&DaemonRequest::DashboardSnapshot), 4);
        assert_eq!(index(&DaemonRequest::Status), 5);
        assert_eq!(index(&DaemonRequest::ChooseScreen), 11);
        assert_eq!(
            index(&DaemonRequest::PatchHeadsetSettings {
                patch: HeadsetSettingsPatch::default()
            }),
            12
        );
    }

    #[test]
    fn daemon_uptime_counts_from_mark() {
        mark_daemon_start();
        assert!(daemon_uptime_secs() < 5);
    }
}
