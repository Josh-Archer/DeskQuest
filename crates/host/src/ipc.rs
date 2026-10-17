use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use crate::link_status::{LinkStatus, SharedLinkTracker};
use crate::tui::model::DashboardSnapshot;
use crate::MultiDisplayHost;
use linux_quest_display::{DisplayLayoutMode, VirtualMonitor, VirtualMonitorConfig};

/// Request sent from CLI to persistent background daemon.
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
}

/// Response returned from daemon to CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    Monitors(Vec<VirtualMonitor>),
    DashboardSnapshot(Box<crate::tui::model::DashboardSnapshot>),
    Success(String),
    Error(String),
}

/// Maximum allowed IPC payload frame size (1 Megabyte).
pub const MAX_IPC_FRAME_SIZE: usize = 1024 * 1024;

/// Returns the private runtime directory for the Linux Quest daemon.
pub fn daemon_runtime_dir() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        Path::new(&runtime_dir).join("linux-quest")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/linux-quest-{uid}"))
    }
}

/// Returns the standard Unix domain socket path for the Linux Quest daemon.
pub fn daemon_socket_path() -> PathBuf {
    daemon_runtime_dir().join("daemon.sock")
}

/// Returns the standard PID file path for the Linux Quest daemon.
pub fn daemon_pid_path() -> PathBuf {
    daemon_runtime_dir().join("daemon.pid")
}

/// Runs the IPC listener loop inside the daemon process.
pub async fn run_ipc_server(
    host: Arc<MultiDisplayHost>,
    link: SharedLinkTracker,
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
    info!(path = ?socket_path, "Linux Quest daemon IPC socket bound (mode 0600)");

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let host_clone = host.clone();
                        let link_clone = link.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_ipc_client(stream, host_clone, link_clone).await {
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

/// Builds the dashboard snapshot from the live virtual monitors and the derived link state.
///
/// `client.mode` comes only from `link`. `streaming` is true only when a client is connected
/// AND at least one virtual display exists. The host has no video endpoint yet, so this means
/// "link up with something to stream", not that frames are flowing.
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

async fn handle_ipc_client(
    mut stream: UnixStream,
    host: Arc<MultiDisplayHost>,
    link: SharedLinkTracker,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read request length (4 bytes big-endian) and enforce frame cap
    let len = stream.read_u32().await? as usize;
    if len == 0 || len > MAX_IPC_FRAME_SIZE {
        return Err(format!("IPC payload size {len} is invalid or exceeds 1MB limit").into());
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let req: DaemonRequest = bincode::deserialize(&buf)?;

    // 2. Process request
    let resp = match req {
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
            match host.toggle_monitors(count, &base_cfg, layout).await {
                Ok(monitors) => DaemonResponse::Monitors(monitors),
                Err(e) => DaemonResponse::Error(e.to_string()),
            }
        }
        DaemonRequest::List => {
            let monitors = host.list_monitors().await;
            DaemonResponse::Monitors(monitors)
        }
        DaemonRequest::Remove { id } => match host.remove_monitor(id).await {
            Ok(()) => DaemonResponse::Success(format!("Removed virtual monitor ID {id}")),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::Layout { mode } => match host.set_layout(mode).await {
            Ok(()) => DaemonResponse::Success(format!("Applied layout: {mode:?}")),
            Err(e) => DaemonResponse::Error(e.to_string()),
        },
        DaemonRequest::DashboardSnapshot => {
            let monitors = host.list_monitors().await;
            let status = link.lock().status(std::time::Instant::now());
            DaemonResponse::DashboardSnapshot(Box::new(build_dashboard_snapshot(
                monitors,
                status,
                daemon_uptime_secs(),
            )))
        }
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
    let socket_path = daemon_socket_path();
    let mut stream = UnixStream::connect(&socket_path).await?;

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

    #[test]
    fn daemon_uptime_counts_from_mark() {
        mark_daemon_start();
        assert!(daemon_uptime_secs() < 5);
    }
}
