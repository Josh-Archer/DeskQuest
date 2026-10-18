use clap::{Args, Parser, Subcommand, ValueEnum};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

use linux_quest_display::DisplayLayoutMode;
use linux_quest_encoder::{probe_backends, BackendPreference};
use linux_quest_host::adb_link::{spawn_adb_supervisor, AdbSupervisorConfig};
use linux_quest_host::codec_select::{plan_stream, CLIENT_DECODE_CODECS, CODEC_PREFERENCE};
use linux_quest_host::link_status::{
    new_shared_link_tracker, SharedLinkTracker, DEFAULT_INPUT_LIVENESS,
};
use linux_quest_host::tui::{
    run_dashboard_with_source, IpcTelemetrySource, LiveDaemonTelemetrySource, MockTelemetrySource,
};
use linux_quest_host::{
    daemon_socket_path, run_ipc_server, send_daemon_request, DaemonRequest, DaemonResponse,
    HostClipboardService, HostInputReceiver, MultiDisplayHost,
};

#[derive(Parser, Debug)]
#[command(
    name = "linux-quest-host",
    author = "Josh Archer",
    version = "0.1.0",
    about = "Linux Quest 3 VR Streaming Host & Virtual Display Daemon"
)]
struct Cli {
    /// Launch interactive terminal status dashboard directly
    #[arg(long, global = true)]
    tui: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Virtual multi-monitor management commands (communicates with running daemon)
    Display(DisplayArgs),

    /// Run persistent background daemon holding virtual monitors and display sessions
    Daemon(DaemonArgs),

    /// Launch interactive terminal status dashboard
    Dashboard(DashboardArgs),

    /// Show a system tray icon with connection status and service controls
    /// (requires a build with `--features tray`)
    Tray(TrayArgs),
}

#[derive(Args, Debug, Default)]
struct TrayArgs {
    /// How often to poll the daemon, in milliseconds (minimum 500)
    #[arg(long, default_value_t = 2000)]
    interval_ms: u64,
}

#[derive(Args, Debug, Default)]
struct DaemonArgs {
    /// Run daemon with interactive terminal status dashboard in foreground
    #[arg(long)]
    tui: bool,

    /// Video encoder backend: auto (NVENC, then VA-API), nvenc, vaapi or mock.
    /// Overrides LINUX_QUEST_VIDEO_ENCODER when given.
    #[arg(long, value_name = "BACKEND", value_parser = parse_encoder_backend)]
    encoder: Option<BackendPreference>,

    /// Do not advertise this daemon on the LAN via mDNS (Quest clients then need manual IP entry)
    #[arg(long)]
    no_advertise: bool,

    /// Do not manage the USB `adb reverse` tunnel (USB connections then need a manual
    /// `adb reverse tcp:48444 tcp:48444`, and are not recognised as USB until one exists)
    #[arg(long)]
    no_adb: bool,

    /// Serial of the ADB device to use when several are attached (skips the Quest model check)
    #[arg(long, value_name = "SERIAL", conflicts_with = "no_adb")]
    adb_serial: Option<String>,

    /// Do not start the UDP video sender (it is on by default; ignored with --tui)
    #[arg(long)]
    no_video: bool,

    /// UDP port on the headset that video datagrams are sent to
    #[arg(long, default_value_t = linux_quest_host::video_sender::DEFAULT_VIDEO_PORT)]
    video_port: u16,
}

fn parse_encoder_backend(value: &str) -> Result<BackendPreference, String> {
    value.parse()
}

#[derive(Args, Debug, Default)]
struct DashboardArgs {
    /// Run with simulated telemetry without requiring active Quest headset or running daemon
    #[arg(short, long)]
    demo: bool,

    /// Dashboard refresh / tick rate in milliseconds
    #[arg(short = 'r', long, default_value_t = 100)]
    tick_rate_ms: u64,
}

#[derive(Args, Debug)]
struct DisplayArgs {
    #[command(subcommand)]
    action: DisplayCommands,
}

#[derive(Subcommand, Debug)]
enum DisplayCommands {
    /// Toggle total active virtual monitors (1, 2, or 3)
    Toggle {
        /// Target number of virtual monitors (1, 2, or 3)
        #[arg(value_parser = clap::value_parser!(u32))]
        count: u32,

        /// Horizontal resolution per display
        #[arg(short = 'W', long, default_value_t = 2560)]
        width: u32,

        /// Vertical resolution per display
        #[arg(short = 'H', long, default_value_t = 1440)]
        height: u32,

        /// Target refresh rate in Hz (e.g. 60, 72, 90, 120)
        #[arg(short, long, default_value_t = 90)]
        refresh: u32,

        /// Target display DPI for UI scaling
        #[arg(short = 'd', long, default_value_t = 96)]
        dpi: u32,

        /// Spatial desktop arrangement
        #[arg(short, long, value_enum, default_value_t = CliLayoutMode::Horizontal)]
        layout: CliLayoutMode,
    },

    /// List all currently active virtual displays recognized by the system
    List,

    /// Remove a virtual display by monotonic ID
    Remove {
        /// Monitor ID to remove
        id: u32,
    },

    /// Apply spatial layout across active virtual monitors
    Layout {
        /// Layout mode
        #[arg(value_enum)]
        mode: CliLayoutMode,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CliLayoutMode {
    Horizontal,
    Vertical,
    Grid,
}

impl From<CliLayoutMode> for DisplayLayoutMode {
    fn from(mode: CliLayoutMode) -> Self {
        match mode {
            CliLayoutMode::Horizontal => DisplayLayoutMode::Horizontal,
            CliLayoutMode::Vertical => DisplayLayoutMode::Vertical,
            CliLayoutMode::Grid => DisplayLayoutMode::Grid,
        }
    }
}

/// Logs which backend and codec a stream would use with `preference`, given what each
/// backend reports for each codec. Probing talks to libva and NVENC, so it runs on a
/// blocking thread. The plan is not yet applied to a stream (see `codec_select`).
async fn log_stream_plan(preference: BackendPreference) {
    let probes = match tokio::task::spawn_blocking(|| {
        CODEC_PREFERENCE
            .iter()
            .map(|&codec| (codec, probe_backends(codec)))
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(probes) => probes,
        Err(e) => {
            // A panic in a probe (libva and NVENC are loaded and called from here)
            // must not look like "no encoder found".
            tracing::error!(
                "video encoder capability probing failed ({}): {e}. No stream plan was made",
                if e.is_panic() { "panic" } else { "cancelled" }
            );
            return;
        }
    };
    for (codec, results) in &probes {
        for p in results {
            info!(
                "encoder probe {:?} for {:?}: {} ({})",
                p.kind.name(),
                codec,
                if p.available {
                    "available"
                } else {
                    "unavailable"
                },
                p.reason
            );
        }
    }
    let plan = plan_stream(preference, CLIENT_DECODE_CODECS, |backend, codec| {
        probes.iter().any(|(c, results)| {
            *c == codec && results.iter().any(|p| p.kind == backend && p.available)
        })
    });
    match plan {
        Some(plan) => info!(
            "video stream plan: encoder {} with {:?} (preference {:?})",
            plan.backend.name(),
            plan.codec,
            preference
        ),
        None => tracing::warn!(
            "no hardware video encoder can serve the Quest client (preference {:?}). This plan \
             is informational only: the daemon does not start a stream from it yet, and the \
             encoder used by the streaming pipeline (AutoVideoEncoder) falls back to the \
             software mock, which produces no real video",
            preference
        ),
    }
}

/// Resolves on SIGINT (Ctrl-C) or when the pre-registered SIGTERM stream fires.
/// SIGTERM is what systemd sends on `systemctl --user stop`.
async fn wait_for_shutdown_signal(mut term: tokio::signal::unix::Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Starts the Quest-facing services: the UDP input receiver and the TCP control/clipboard
/// listener. Both report connection events to `link`, and both stop on the shutdown broadcast.
///
/// Returns the ports actually bound, as `(control, input)`; `None` where a bind failed.
async fn start_stream_services(
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
    link: SharedLinkTracker,
    keyframe_requests: Arc<AtomicBool>,
) -> (Option<u16>, Option<u16>) {
    let mut input_port = None;
    let mut control_port = None;
    let input_bind_addr =
        std::net::SocketAddr::from(([0, 0, 0, 0], linux_quest_transport::DEFAULT_INPUT_PORT));
    match HostInputReceiver::bind_auto(input_bind_addr).await {
        Ok(input_service) => {
            let mut input_service = input_service
                .with_link_tracker(link.clone())
                .with_keyframe_requests(keyframe_requests);
            input_port = input_service.local_addr().ok().map(|a| a.port());
            let running_flag = input_service.running_flag();
            let auto_injector = input_service.injector();
            let mut shutdown_rx_input = shutdown_tx.subscribe();
            tokio::spawn(async move {
                info!(
                    addr = %input_bind_addr,
                    "Input receiver service listening on UDP input channel"
                );
                tokio::select! {
                    res = input_service.run() => {
                        if let Err(e) = res {
                            tracing::warn!("Input receiver service exited: {e}");
                        }
                    }
                    _ = shutdown_rx_input.recv() => {
                        running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                        if let Ok(mut inj) = auto_injector.lock() {
                            let _ = inj.release_all();
                        }
                    }
                }
            });
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to bind UDP input channel on {input_bind_addr}; input receiver disabled"
            );
        }
    }

    let control_bind_addr =
        std::net::SocketAddr::from(([0, 0, 0, 0], linux_quest_transport::DEFAULT_CONTROL_PORT));
    match HostClipboardService::bind_auto(control_bind_addr).await {
        Ok(clipboard_service) => {
            let mut clipboard_service = clipboard_service.with_link_tracker(link);
            control_port = clipboard_service.local_addr().map(|a| a.port());
            let running_flag = clipboard_service.running_flag();
            let mut shutdown_rx_clipboard = shutdown_tx.subscribe();
            tokio::spawn(async move {
                info!(
                    addr = %control_bind_addr,
                    "Clipboard synchronization service listening on control channel"
                );
                tokio::select! {
                    res = clipboard_service.run() => {
                        if let Err(e) = res {
                            tracing::warn!("Clipboard synchronization service exited: {e}");
                        }
                    }
                    _ = shutdown_rx_clipboard.recv() => {
                        running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            });
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to bind control channel on {control_bind_addr}; clipboard synchronization disabled"
            );
        }
    }

    (control_port, input_port)
}

/// Starts the UDP video sender unless `--no-video`. When it cannot run (no real capture backend,
/// no hardware encoder), logs exactly one line and returns `None`; the daemon keeps running.
async fn start_video(
    args: &DaemonArgs,
    link: SharedLinkTracker,
    keyframe_requests: Arc<AtomicBool>,
) -> Option<linux_quest_host::video_sender::VideoSenderHandle> {
    if args.no_video {
        info!("UDP video sender disabled by --no-video");
        return None;
    }
    let options = linux_quest_host::video_sender::daemon::DaemonVideoOptions {
        video_port: args.video_port,
        encoder: args.encoder.unwrap_or_else(BackendPreference::from_env),
    };
    match linux_quest_host::video_sender::daemon::start_daemon_video_sender(
        link,
        options,
        keyframe_requests,
    )
    .await
    {
        Ok(handle) => {
            info!(
                port = args.video_port,
                "UDP video sender ready; it sends to the headset once its input channel is live"
            );
            Some(handle)
        }
        Err(reason) => {
            tracing::warn!(
                "UDP video sender not started ({reason}); the daemon keeps running without video"
            );
            None
        }
    }
}

/// Starts the ADB bridge supervisor unless disabled. It is best effort: a missing `adb` or
/// device only produces log lines. Stops (and removes the reverse mapping) on `shutdown_tx`.
fn start_adb_bridge(
    args: &DaemonArgs,
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
    link: SharedLinkTracker,
) -> Option<tokio::task::JoinHandle<()>> {
    if args.no_adb {
        info!("ADB bridge disabled (--no-adb); not managing the USB reverse tunnel");
        return None;
    }
    let runner = Arc::new(linux_quest_transport::SystemAdbRunner::default());
    let config = AdbSupervisorConfig::new(args.adb_serial.clone());
    Some(spawn_adb_supervisor(
        runner,
        config,
        link,
        shutdown_tx.subscribe(),
    ))
}

/// Waits for the ADB supervisor to remove its reverse mapping, bounded so a hung `adb`
/// cannot block daemon exit.
async fn finish_adb_bridge(handle: Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle {
        if tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .is_err()
        {
            tracing::warn!("ADB bridge did not stop in time; leaving it");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let is_tui = cli.tui
        || matches!(&cli.command, Some(Commands::Dashboard(_)))
        || matches!(
            &cli.command,
            Some(Commands::Daemon(DaemonArgs { tui: true, .. }))
        );

    if !is_tui {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .init();
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            if cli.tui {
                Commands::Dashboard(DashboardArgs::default())
            } else {
                use clap::CommandFactory;
                let _ = Cli::command().print_help();
                println!();
                return Ok(());
            }
        }
    };

    match command {
        Commands::Dashboard(args) => {
            let tick_rate = Duration::from_millis(args.tick_rate_ms.max(20));
            if args.demo {
                let source = MockTelemetrySource::new();
                run_dashboard_with_source(source, tick_rate, None).await?;
            } else {
                let socket = daemon_socket_path();
                if !socket.exists() {
                    eprintln!(
                        "Error: Cannot connect to Linux Quest daemon at {:?}",
                        socket
                    );
                    eprintln!("Please start the host daemon first:");
                    eprintln!("  linux-quest-host daemon");
                    eprintln!("Or run the dashboard in simulation mode:");
                    eprintln!("  linux-quest-host dashboard --demo");
                    std::process::exit(1);
                }
                let source = IpcTelemetrySource;
                run_dashboard_with_source(source, tick_rate, None).await?;
            }
        }
        Commands::Tray(args) => {
            #[cfg(feature = "tray")]
            {
                let interval = Duration::from_millis(args.interval_ms.max(500));
                linux_quest_host::tray::run_tray(IpcTelemetrySource, interval).await?;
            }
            #[cfg(not(feature = "tray"))]
            {
                let _ = args;
                eprintln!("This build has no tray support.");
                eprintln!("Rebuild with the `tray` feature:");
                eprintln!(
                    "  cargo install --path crates/host --bin linux-quest-host --features tray"
                );
                std::process::exit(2);
            }
        }
        Commands::Daemon(daemon_args) => {
            let preference = daemon_args
                .encoder
                .unwrap_or_else(BackendPreference::from_env);
            log_stream_plan(preference).await;
            let host = Arc::new(MultiDisplayHost::create_auto().await?);
            let run_tui = daemon_args.tui || cli.tui;
            let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);

            if run_tui {
                let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
                let shutdown_tx_clone = shutdown_tx.clone();
                let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shutdown_flag_clone = shutdown_flag.clone();

                // The TUI run binds the listeners but does not advertise them.
                let _ = start_stream_services(
                    &shutdown_tx,
                    link.clone(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await;
                let adb_handle = start_adb_bridge(&daemon_args, &shutdown_tx, link.clone());

                let host_clone = host.clone();
                let link_clone = link.clone();
                let ipc_handle = tokio::spawn(async move {
                    if let Err(e) = run_ipc_server(host_clone, link_clone, shutdown_rx).await {
                        tracing::error!("Daemon IPC server error: {e}");
                    }
                });

                let source = LiveDaemonTelemetrySource::new(host.clone(), link.clone());
                let _ = run_dashboard_with_source(
                    source,
                    Duration::from_millis(100),
                    Some(shutdown_flag),
                )
                .await;

                // TUI exited (user hit q or Esc), shutdown daemon cleanly
                let _ = shutdown_tx_clone.send(());
                shutdown_flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = ipc_handle.await;
                finish_adb_bridge(adb_handle).await;
            } else {
                info!(
                    backend = host.backend_name().await,
                    "Linux Quest host daemon starting"
                );

                let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
                let shutdown_tx_clone = shutdown_tx.clone();
                // systemd stops units with SIGTERM; Ctrl-C delivers SIGINT. Handle both.
                let term_signal =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                tokio::spawn(async move {
                    wait_for_shutdown_signal(term_signal).await;
                    info!("Shutdown signal received; stopping daemon...");
                    let _ = shutdown_tx_clone.send(());
                });

                // Client keyframe requests: set by the input receiver, consumed by the video sender.
                let keyframe_requests = Arc::new(AtomicBool::new(false));
                let (control_port, input_port) =
                    start_stream_services(&shutdown_tx, link.clone(), keyframe_requests.clone())
                        .await;
                let adb_handle = start_adb_bridge(&daemon_args, &shutdown_tx, link.clone());

                let video = start_video(&daemon_args, link.clone(), keyframe_requests).await;

                // Advertise only the ports that were really bound above.
                let listening = match (control_port, input_port) {
                    (Some(control), Some(input)) => {
                        Some(linux_quest_host::advertise::ListeningPorts { control, input })
                    }
                    _ => None,
                };
                let advertiser = linux_quest_host::advertise::advertise_daemon_with_video(
                    !daemon_args.no_advertise,
                    linux_quest_host::advertise::system_instance_name(),
                    listening,
                    video.as_ref().map(|v| v.video_port()),
                )
                .await;

                let served = run_ipc_server(host, link, shutdown_rx).await;
                // The IPC server may have ended on its own; make sure the bridge stops too.
                let _ = shutdown_tx.send(());
                finish_adb_bridge(adb_handle).await;
                if let Some(advertiser) = advertiser {
                    advertiser.stop().await;
                }
                if let Some(video) = video {
                    video.stop().await;
                }
                served?;
                info!("Daemon stopped cleanly.");
            }
        }
        Commands::Display(display_args) => {
            let req = match display_args.action {
                DisplayCommands::Toggle {
                    count,
                    width,
                    height,
                    refresh,
                    dpi,
                    layout,
                } => DaemonRequest::Toggle {
                    count,
                    width,
                    height,
                    refresh,
                    dpi,
                    layout: layout.into(),
                },
                DisplayCommands::List => DaemonRequest::List,
                DisplayCommands::Remove { id } => DaemonRequest::Remove { id },
                DisplayCommands::Layout { mode } => DaemonRequest::Layout { mode: mode.into() },
            };

            let response = match send_daemon_request(req).await {
                Ok(resp) => resp,
                Err(_) => {
                    eprintln!(
                        "Error: Cannot connect to Linux Quest daemon at {:?}",
                        daemon_socket_path()
                    );
                    eprintln!("Please start the host daemon first:");
                    eprintln!("  linux-quest-host daemon");
                    std::process::exit(1);
                }
            };

            match response {
                DaemonResponse::Monitors(monitors) => {
                    if monitors.is_empty() {
                        println!("No active virtual monitors provisioned.");
                    } else {
                        println!("Active virtual monitors ({}):", monitors.len());
                        for m in monitors {
                            println!(
                                "  [ID {}] {}: {}x{} @ {}Hz (connector: {}, pos: +{}+{})",
                                m.id,
                                m.name,
                                m.width,
                                m.height,
                                m.refresh_rate,
                                m.connector,
                                m.x,
                                m.y
                            );
                        }
                    }
                }
                DaemonResponse::DashboardSnapshot(_) => {
                    println!("Received DashboardSnapshot response from daemon.");
                }
                DaemonResponse::Success(msg) => {
                    println!("{msg}");
                }
                DaemonResponse::Error(err) => {
                    eprintln!("Error from daemon: {err}");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::wait_for_shutdown_signal;
    use tokio::signal::unix::{signal, SignalKind};

    #[tokio::test]
    async fn sigterm_triggers_daemon_shutdown() {
        // Register the handler before raising the signal; otherwise SIGTERM's default
        // action would terminate the test process.
        let term = signal(SignalKind::terminate()).expect("register SIGTERM handler");
        let pid = std::process::id() as libc::pid_t;
        // SAFETY: kill(2) with our own pid and a standard signal number.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        assert_eq!(rc, 0, "kill(SIGTERM) failed");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_shutdown_signal(term),
        )
        .await
        .expect("SIGTERM should resolve the shutdown future");
    }
}
