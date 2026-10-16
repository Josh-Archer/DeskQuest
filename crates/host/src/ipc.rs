use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

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
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
                        tokio::spawn(async move {
                            if let Err(e) = handle_ipc_client(stream, host_clone).await {
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

async fn handle_ipc_client(
    mut stream: UnixStream,
    host: Arc<MultiDisplayHost>,
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
            let displays: Vec<crate::tui::model::DisplaySnapshot> = monitors
                .into_iter()
                .map(|m| crate::tui::model::DisplaySnapshot {
                    id: m.id as u16,
                    name: m.name,
                    width: m.width,
                    height: m.height,
                    refresh_rate: m.refresh_rate,
                    fps: m.refresh_rate as f32,
                    bitrate_mbps: 75.0,
                    codec: "AV1".into(),
                    is_virtual: m.is_virtual,
                })
                .collect();

            let streaming = !displays.is_empty();
            let conn_mode = if streaming {
                linux_quest_protocol::ClientConnectionMode::UsbAdb
            } else {
                linux_quest_protocol::ClientConnectionMode::Disconnected
            };

            let mut video_latency = linux_quest_protocol::FrameLatencyBreakdown {
                frame_id: 1,
                capture_start_us: 1000,
                capture_duration_us: 1150,
                encode_duration_us: 2850,
                host_queue_duration_us: 300,
                transport_duration_us: 800,
                client_decode_duration_us: 3100,
                client_render_duration_us: 600,
                total_motion_to_photon_us: 0,
            };
            video_latency.compute_total();

            let mut audio_latency = linux_quest_protocol::AudioLatencyBreakdown {
                frame_id: 1,
                capture_start_us: 1000,
                capture_duration_us: 300,
                encode_duration_us: 400,
                transport_duration_us: 800,
                jitter_buffer_delay_us: 2500,
                decode_duration_us: 200,
                playout_buffer_duration_us: 500,
                total_latency_us: 0,
            };
            audio_latency.compute_total();

            let snap = crate::tui::model::DashboardSnapshot {
                uptime_secs: 0,
                streaming,
                displays,
                video_latency,
                audio_latency,
                client: linux_quest_protocol::ClientTransportTelemetry {
                    mode: conn_mode,
                    rtt_us: 1100,
                    smoothed_rtt_us: 1050,
                    packet_loss_rate: 0.0001,
                    packets_sent: 1000,
                    packets_recv: 999,
                    packets_lost: 1,
                    fec_recoveries: 1,
                    bitrate_mbps: 85.0,
                },
                audio: linux_quest_protocol::AudioStreamTelemetry {
                    active: streaming,
                    codec: "Opus".into(),
                    sample_rate: 48000,
                    channels: 2,
                    frame_size_ms: 2.5,
                    bitrate_kbps: 128,
                    jitter_us: 350,
                    buffer_delay_us: 2500,
                    buffer_health_pct: 100.0,
                    plc_count: 0,
                },
            };
            DaemonResponse::DashboardSnapshot(Box::new(snap))
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
