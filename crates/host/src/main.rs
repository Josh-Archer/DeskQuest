use clap::{Args, Parser, Subcommand, ValueEnum};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

use linux_quest_display::DisplayLayoutMode;
use linux_quest_encoder::{probe_backends, BackendPreference};
use linux_quest_host::adb_link::{spawn_adb_supervisor, AdbSupervisorConfig};
use linux_quest_host::codec_select::{plan_stream, CLIENT_DECODE_CODECS, CODEC_PREFERENCE};
use linux_quest_host::congestion::RateCoordinator;
use linux_quest_host::link_status::{
    new_shared_link_tracker, SharedLinkTracker, DEFAULT_INPUT_LIVENESS,
};
use linux_quest_host::service;
use linux_quest_host::tui::{
    run_dashboard_with_source, IpcTelemetrySource, LiveDaemonTelemetrySource, MockTelemetrySource,
};
use linux_quest_host::video_sender::multi::{
    parse_stream_count, MultiStreamOptions, MultiVideoSenderHandle, StreamSize,
};
use linux_quest_host::video_sender::KeyframeRequests;
use linux_quest_host::video_sender::VideoControl;
use linux_quest_host::{
    daemon_socket_path, run_ipc_server, run_ipc_server_with, send_daemon_request, DaemonRequest,
    DaemonResponse, HeadsetHub, HostClipboardService, HostInputReceiver, IpcContext,
    MultiDisplayHost,
};
use linux_quest_input::InputInjector as _;

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

    /// Show a system tray icon with the headset's status, pause and resume, Choose Screen,
    /// and a shortcut to the desktop app
    Tray(TrayArgs),

    /// Install or remove the systemd user service that runs the daemon at login and restarts
    /// it if it fails
    Service(ServiceArgs),
}

#[derive(Args, Debug)]
struct ServiceArgs {
    #[command(subcommand)]
    action: ServiceCommands,
}

#[derive(Subcommand, Debug)]
enum ServiceCommands {
    /// Write ~/.config/systemd/user/linux-quest-host.service for this binary and reload
    /// systemd. Daemon options go after `--`: `service install --enable -- --streams 3`
    Install(ServiceInstallArgs),
    /// Print the unit files `install` would write, without writing them
    Print(ServiceInstallArgs),
    /// Disable and stop the services, and remove their unit files
    Uninstall,
}

#[derive(Args, Debug, Default)]
struct ServiceInstallArgs {
    /// Also install linux-quest-tray.service for the tray icon
    #[arg(long)]
    tray: bool,
    /// Enable the units, so they start with every graphical login
    #[arg(long)]
    enable: bool,
    /// Start (or restart) the units now
    #[arg(long)]
    start: bool,
    /// Replace unit files that already exist with different contents
    #[arg(long)]
    force: bool,
    /// Options for `linux-quest-host daemon`, after `--`
    #[arg(last = true, value_name = "DAEMON_OPTIONS")]
    daemon_args: Vec<String>,
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

    /// Send a synthetic test pattern when no real screen capture is available (for testing
    /// encode, transport and decode on the headset)
    #[arg(long, conflicts_with = "no_video")]
    test_pattern: bool,

    /// Video bitrate in Mbit/s (default 30)
    #[arg(long, value_name = "MBPS", default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=500))]
    bitrate: u32,

    /// Keep the video bitrate fixed at --bitrate instead of adapting it to client feedback
    /// (loss, delay, jitter). By default --bitrate is the ceiling and the host lowers it live
    /// when the link degrades.
    #[arg(long)]
    fixed_bitrate: bool,

    /// Number of video streams (headset screens) to send, 1 to 3. Stream 0 is the desktop
    /// capture; streams 1 and up are synthetic test patterns for now. All go to --video-port.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_stream_count)]
    streams: u16,

    /// Size and frame rate of the synthetic extra streams (1 and up) as WxH@FPS
    #[arg(long, value_name = "WxH@FPS", default_value_t = StreamSize::default())]
    extra_stream_size: StreamSize,
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

/// The absolute path of this binary, for `ExecStart`. After a package upgrade Linux reports
/// the replaced file as `<path> (deleted)`; the path itself is what the unit must run.
fn service_exe() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let shown = exe.display().to_string();
    let exe = match shown.strip_suffix(" (deleted)") {
        Some(path) => std::path::PathBuf::from(path),
        None => exe,
    };
    linux_quest_host::service::check_exe(&exe)?;
    Ok(exe)
}

fn service_install_options(args: ServiceInstallArgs) -> Result<service::InstallOptions, String> {
    service::check_daemon_args(&args.daemon_args)?;
    // Catch typos now rather than in a restart loop: the options must parse as `daemon`'s.
    let argv = ["linux-quest-host", "daemon"]
        .into_iter()
        .map(String::from)
        .chain(args.daemon_args.iter().cloned());
    if let Err(e) = Cli::try_parse_from(argv) {
        return Err(format!("the daemon options do not parse:\n{e}"));
    }
    Ok(service::InstallOptions {
        tray: args.tray,
        enable: args.enable,
        start: args.start,
        force: args.force,
        daemon_args: args.daemon_args,
    })
}

/// `true` when the daemon socket answers but the user service is not the one running: a
/// daemon started by hand holds the PID lock, so starting the service would fail.
async fn manual_daemon_running() -> bool {
    let socket_live = tokio::net::UnixStream::connect(daemon_socket_path())
        .await
        .is_ok();
    let service_active = std::process::Command::new("systemctl")
        .args(["--user", "--quiet", "is-active", service::DAEMON_UNIT])
        .status()
        .is_ok_and(|s| s.success());
    socket_live && !service_active
}

async fn run_service_command(args: ServiceArgs) -> Result<(), String> {
    let unit_dir = service::user_unit_dir()
        .ok_or("cannot find the systemd user unit directory (HOME is not set)")?;
    match args.action {
        ServiceCommands::Print(install) => {
            let exe = service_exe()?;
            let options = service_install_options(install)?;
            let plan = service::plan_install(&unit_dir, &exe.display().to_string(), &options);
            for unit in plan.units {
                println!("# {}", unit.path.display());
                print!("{}", unit.contents);
                println!();
            }
            Ok(())
        }
        ServiceCommands::Install(install) => {
            let exe = service_exe()?;
            let options = service_install_options(install)?;
            if options.start && manual_daemon_running().await {
                return Err(format!(
                    "a daemon started outside systemd is running (socket {}). Stop it first: the \
                     service would fail on its PID lock. Or install without --start",
                    daemon_socket_path().display()
                ));
            }
            if let Some(warning) = service::build_dir_warning(&exe) {
                eprintln!("Note: {warning}");
            }
            let plan = service::plan_install(&unit_dir, &exe.display().to_string(), &options);
            let outcomes =
                service::apply_install(&plan, options.force, &mut service::SystemctlRunner)?;
            for (path, outcome) in &outcomes {
                let what = match outcome {
                    service::WriteOutcome::Created => "Wrote",
                    service::WriteOutcome::Updated => "Updated",
                    service::WriteOutcome::Unchanged => "Unchanged",
                };
                println!("{what} {}", path.display());
            }
            if let Some(exec) = plan.units[0]
                .contents
                .lines()
                .find(|l| l.starts_with("ExecStart="))
            {
                println!("  {exec}");
            }
            println!("Reloaded the systemd user manager.");
            let names: Vec<&str> = plan
                .units
                .iter()
                .map(|u| u.name.trim_end_matches(".service"))
                .collect();
            match (options.enable, options.start) {
                (true, true) => println!("Enabled and started: {}", names.join(", ")),
                (true, false) => println!(
                    "Enabled; starts at the next login. Start it now with: systemctl --user start {}",
                    names.join(" ")
                ),
                (false, true) => println!(
                    "Started, but not enabled for the next login. Enable it with: \
                     systemctl --user enable {}",
                    names.join(" ")
                ),
                (false, false) => println!(
                    "Not enabled or started. To start it now and at every login:\n  \
                     systemctl --user enable --now {}",
                    names.join(" ")
                ),
            }
            println!("Logs: journalctl --user -u linux-quest-host -f");
            Ok(())
        }
        ServiceCommands::Uninstall => {
            let removed = service::apply_uninstall(&unit_dir, &mut service::SystemctlRunner)?;
            if removed.is_empty() {
                println!("No Linux Quest user units in {}", unit_dir.display());
            }
            for path in removed {
                println!("Removed {}", path.display());
            }
            Ok(())
        }
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
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Arc<RateCoordinator>,
    display_map: linux_quest_input::SharedDisplayMap,
    tunnel_video: tokio::sync::broadcast::Sender<Vec<u8>>,
    headset: Arc<HeadsetHub>,
) -> (Option<u16>, Option<u16>) {
    let mut input_port = None;
    let control_port;
    let mut usb_packet_handler: Option<Arc<dyn Fn(linux_quest_protocol::Packet) + Send + Sync>> =
        None;
    let input_bind_addr =
        std::net::SocketAddr::from(([0, 0, 0, 0], linux_quest_transport::DEFAULT_INPUT_PORT));
    match HostInputReceiver::bind_auto_mapped(input_bind_addr, display_map).await {
        Ok(input_service) => {
            let mut input_service = input_service
                .with_link_tracker(link.clone())
                .with_keyframe_requests(keyframe_requests.clone())
                .with_rate_coordinator(rate.clone());
            input_port = input_service.local_addr().ok().map(|a| a.port());
            let running_flag = input_service.running_flag();
            let auto_injector = input_service.injector();
            let usb_injector = auto_injector.clone();
            let usb_keyframes = keyframe_requests.clone();
            let usb_rate = rate.clone();
            usb_packet_handler = Some(Arc::new(move |packet| {
                if packet.header.packet_type != linux_quest_protocol::PacketType::InputEvent {
                    return;
                }
                let Ok(batch) = linux_quest_protocol::InputBatch::decode(&packet.payload) else {
                    return;
                };
                let now = std::time::Instant::now();
                for event in batch.events {
                    match event {
                        linux_quest_protocol::InputEvent::KeyframeRequest => {
                            let _ = usb_keyframes.request(0);
                        }
                        linux_quest_protocol::InputEvent::KeyframeRequestFor { stream_id } => {
                            let _ = usb_keyframes.request(stream_id);
                        }
                        linux_quest_protocol::InputEvent::NetworkFeedback(feedback) => {
                            usb_rate.on_feedback(&feedback, now);
                        }
                        event => {
                            if let Ok(mut injector) = usb_injector.lock() {
                                if let Err(error) = injector.inject_event(event) {
                                    tracing::warn!(%error, "Failed to inject USB tunnel input event");
                                }
                            }
                        }
                    }
                }
            }));
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
            let usb_keyframes = keyframe_requests.clone();
            let mut clipboard_service = clipboard_service
                .with_link_tracker(link)
                .with_headset_hub(headset)
                .with_tunnel_video(tunnel_video)
                .with_tunnel_keyframe_request(Arc::new(move || {
                    for stream_id in
                        0..linux_quest_host::video_sender::KeyframeRequests::MAX_STREAMS
                    {
                        let _ = usb_keyframes.request(stream_id);
                    }
                }));
            if let Some(handler) = usb_packet_handler {
                clipboard_service = clipboard_service.with_tunnel_packet_handler(handler);
            }
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
            // 48444 carries the USB tunnel (video, input, settings) as well as LAN control, so a
            // daemon without it cannot serve a USB headset. Exit so systemd can restart us.
            tracing::error!(
                error = %e,
                "Failed to bind control channel on {control_bind_addr} after retrying; the USB \
                 tunnel, settings and clipboard cannot work. Is another linux-quest-host running? Exiting."
            );
            eprintln!(
                "error: could not bind control port {control_bind_addr}: {e}. \
                 Another linux-quest-host may be running."
            );
            std::process::exit(1);
        }
    }

    (control_port, input_port)
}

/// Starts the UDP video sender unless `--no-video`. When it cannot run (no real capture backend,
/// no hardware encoder), logs exactly one line and returns `None`; the daemon keeps running.
/// Keeps `map` in step with the virtual monitors so pointer positions on a non-primary stream
/// land on the right monitor. Polled because monitors come and go through the IPC server.
fn spawn_display_map_refresh(
    host: Arc<MultiDisplayHost>,
    map: linux_quest_input::SharedDisplayMap,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let regions = host.list_monitors().await.into_iter().map(|m| {
                (
                    m.id as u16,
                    linux_quest_input::DisplayRegion {
                        x: m.x,
                        y: m.y,
                        width: m.width,
                        height: m.height,
                    },
                )
            });
            let next = linux_quest_input::DisplayMap::from_regions(regions);
            if let Ok(mut current) = map.write() {
                *current = next;
            }
        }
    });
}

async fn start_video(
    args: &DaemonArgs,
    link: SharedLinkTracker,
    keyframe_requests: Arc<KeyframeRequests>,
    rate: Arc<RateCoordinator>,
    tunnel_video: tokio::sync::broadcast::Sender<Vec<u8>>,
    control: Arc<VideoControl>,
) -> Option<MultiVideoSenderHandle> {
    if args.no_video {
        info!("Video sender disabled by --no-video");
        return None;
    }
    let options = MultiStreamOptions {
        video: linux_quest_host::video_sender::daemon::DaemonVideoOptions {
            video_port: args.video_port,
            encoder: args.encoder.unwrap_or_else(BackendPreference::from_env),
            test_pattern: args.test_pattern,
            bitrate_mbps: args.bitrate,
        },
        streams: args.streams,
        extra_stream: args.extra_stream_size,
    };
    match linux_quest_host::video_sender::multi::start_daemon_video_senders_with(
        link,
        options,
        keyframe_requests,
        Some(rate),
        Some(tunnel_video),
        control,
    )
    .await
    {
        Ok(handle) => {
            info!(
                port = args.video_port,
                "Video sender ready; it streams over the active headset transport"
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
        Commands::Service(args) => {
            if let Err(e) = run_service_command(args).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tray(args) => {
            let interval = Duration::from_millis(args.interval_ms.max(500));
            linux_quest_host::tray::run_tray(IpcTelemetrySource, interval).await?;
        }
        Commands::Daemon(daemon_args) => {
            let preference = daemon_args
                .encoder
                .unwrap_or_else(BackendPreference::from_env);
            log_stream_plan(preference).await;
            let host = Arc::new(MultiDisplayHost::create_auto().await?);
            let run_tui = daemon_args.tui || cli.tui;
            let link = new_shared_link_tracker(DEFAULT_INPUT_LIVENESS);
            let display_map = linux_quest_input::SharedDisplayMap::default();
            spawn_display_map_refresh(host.clone(), display_map.clone());
            // Layout and settings the headset reports on the control channel.
            let headset = Arc::new(HeadsetHub::new());

            if run_tui {
                let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
                let (usb_video_tx, _) = tokio::sync::broadcast::channel(256);
                let shutdown_tx_clone = shutdown_tx.clone();
                let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shutdown_flag_clone = shutdown_flag.clone();

                // The TUI run binds the listeners but does not advertise them.
                let _ = start_stream_services(
                    &shutdown_tx,
                    link.clone(),
                    KeyframeRequests::new(),
                    Arc::new(RateCoordinator::new(
                        daemon_args.bitrate.saturating_mul(1000),
                        false,
                    )),
                    display_map.clone(),
                    usb_video_tx,
                    headset.clone(),
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
                let (usb_video_tx, _) = tokio::sync::broadcast::channel(256);
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
                let keyframe_requests = KeyframeRequests::new();
                // Adaptive bitrate: --bitrate is the ceiling; client feedback lowers it live.
                let rate = Arc::new(
                    RateCoordinator::new(
                        daemon_args.bitrate.saturating_mul(1000),
                        !daemon_args.fixed_bitrate,
                    )
                    .with_link(link.clone()),
                );
                linux_quest_host::congestion::install_global(rate.clone());
                {
                    let rate = rate.clone();
                    let mut stop = shutdown_tx.subscribe();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(Duration::from_millis(250));
                        loop {
                            tokio::select! {
                                _ = tick.tick() => rate.tick(std::time::Instant::now()),
                                _ = stop.recv() => break,
                            }
                        }
                    });
                }
                let (control_port, input_port) = start_stream_services(
                    &shutdown_tx,
                    link.clone(),
                    keyframe_requests.clone(),
                    rate.clone(),
                    display_map.clone(),
                    usb_video_tx.clone(),
                    headset.clone(),
                )
                .await;
                let adb_handle = start_adb_bridge(&daemon_args, &shutdown_tx, link.clone());

                // Pause and resume, and the screen picker, for the IPC server.
                let video_control = VideoControl::new();
                let video = start_video(
                    &daemon_args,
                    link.clone(),
                    keyframe_requests,
                    rate.clone(),
                    usb_video_tx,
                    video_control.clone(),
                )
                .await;

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

                let ipc = IpcContext {
                    host,
                    link,
                    headset,
                    video: video_control,
                    rate: video.is_some().then_some(rate),
                };
                let served = run_ipc_server_with(ipc, shutdown_rx).await;
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
                DaemonResponse::DashboardSnapshot(_)
                | DaemonResponse::Status(_)
                | DaemonResponse::Headset(_) => {
                    println!("Received an unexpected response from the daemon.");
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
    use super::{wait_for_shutdown_signal, Cli, Commands};
    use clap::Parser;
    use tokio::signal::unix::{signal, SignalKind};

    fn parse_daemon(args: &[&str]) -> super::DaemonArgs {
        let argv = std::iter::once("linux-quest-host").chain(args.iter().copied());
        match Cli::try_parse_from(argv)
            .expect("daemon args should parse")
            .command
        {
            Some(Commands::Daemon(daemon)) => daemon,
            other => panic!("expected daemon subcommand, got {other:?}"),
        }
    }

    #[test]
    fn fixed_bitrate_flag_defaults_off() {
        assert!(!parse_daemon(&["daemon"]).fixed_bitrate);
        assert!(parse_daemon(&["daemon", "--fixed-bitrate"]).fixed_bitrate);
    }

    #[test]
    fn daemon_bitrate_option_parses_and_defaults_to_30() {
        assert_eq!(parse_daemon(&["daemon", "--bitrate", "12"]).bitrate, 12);
        assert_eq!(parse_daemon(&["daemon"]).bitrate, 30);
        assert!(Cli::try_parse_from(["linux-quest-host", "daemon", "--bitrate", "0"]).is_err());
        assert!(Cli::try_parse_from(["linux-quest-host", "daemon", "--bitrate", "501"]).is_err());
    }

    #[test]
    fn daemon_streams_option_is_one_to_three_and_defaults_to_one() {
        assert_eq!(parse_daemon(&["daemon"]).streams, 1);
        assert_eq!(parse_daemon(&["daemon", "--streams", "1"]).streams, 1);
        assert_eq!(parse_daemon(&["daemon", "--streams", "3"]).streams, 3);
        for bad in ["0", "4", "-1", "two", ""] {
            let err = Cli::try_parse_from(["linux-quest-host", "daemon", "--streams", bad])
                .expect_err(bad);
            if bad == "0" || bad == "4" {
                assert!(
                    err.to_string().contains("1 to 3"),
                    "{bad:?} gets a clear message: {err}"
                );
            }
        }
    }

    #[test]
    fn daemon_extra_stream_size_parses_and_defaults_to_1080p60() {
        let default = parse_daemon(&["daemon"]).extra_stream_size;
        assert_eq!(
            (default.width, default.height, default.fps),
            (1920, 1080, 60)
        );
        let size =
            parse_daemon(&["daemon", "--extra-stream-size", "1280x720@30"]).extra_stream_size;
        assert_eq!((size.width, size.height, size.fps), (1280, 720, 30));
        for bad in [
            "1280x720",
            "720p",
            "1281x720@30",
            "1280x720@0",
            "1280x720@x",
        ] {
            let err =
                Cli::try_parse_from(["linux-quest-host", "daemon", "--extra-stream-size", bad])
                    .expect_err(bad);
            assert!(err.to_string().contains("WxH@FPS"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn streams_and_extra_stream_size_work_together_with_the_other_daemon_flags() {
        let daemon = parse_daemon(&[
            "daemon",
            "--test-pattern",
            "--bitrate",
            "20",
            "--streams",
            "2",
            "--extra-stream-size",
            "1600x900@72",
        ]);
        assert_eq!((daemon.streams, daemon.bitrate), (2, 20));
        assert!(daemon.test_pattern);
        assert_eq!(daemon.extra_stream_size.to_string(), "1600x900@72");
    }

    #[test]
    fn service_install_takes_daemon_options_after_a_double_dash() {
        let cli = Cli::try_parse_from([
            "linux-quest-host",
            "service",
            "install",
            "--tray",
            "--enable",
            "--",
            "--streams",
            "3",
        ])
        .expect("service install parses");
        let Some(Commands::Service(super::ServiceArgs {
            action: super::ServiceCommands::Install(args),
        })) = cli.command
        else {
            panic!("expected service install");
        };
        assert!(args.tray && args.enable && !args.start && !args.force);
        assert_eq!(args.daemon_args, ["--streams", "3"]);
        let options = super::service_install_options(args).expect("valid daemon options");
        assert_eq!(options.daemon_args, ["--streams", "3"]);
    }

    #[test]
    fn service_install_rejects_daemon_options_that_do_not_parse() {
        let args = super::ServiceInstallArgs {
            daemon_args: vec!["--streams".into(), "9".into()],
            ..Default::default()
        };
        let err = super::service_install_options(args).unwrap_err();
        assert!(err.contains("do not parse"), "{err}");
        let args = super::ServiceInstallArgs {
            daemon_args: vec!["--tui".into()],
            ..Default::default()
        };
        assert!(super::service_install_options(args).is_err());
    }

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
