use clap::{Args, Parser, Subcommand, ValueEnum};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

use linux_quest_display::DisplayLayoutMode;
use linux_quest_host::tui::{run_dashboard_with_source, IpcTelemetrySource, MockTelemetrySource};
use linux_quest_host::{
    daemon_socket_path, run_ipc_server, send_daemon_request, DaemonRequest, DaemonResponse,
    MultiDisplayHost,
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
}

#[derive(Args, Debug, Default)]
struct DaemonArgs {
    /// Run daemon with interactive terminal status dashboard in foreground
    #[arg(long)]
    tui: bool,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let is_tui = cli.tui
        || matches!(&cli.command, Some(Commands::Dashboard(_)))
        || matches!(
            &cli.command,
            Some(Commands::Daemon(DaemonArgs { tui: true }))
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
        Commands::Daemon(daemon_args) => {
            let host = Arc::new(MultiDisplayHost::create_auto().await?);
            let run_tui = daemon_args.tui || cli.tui;

            if run_tui {
                let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
                let shutdown_tx_clone = shutdown_tx.clone();
                let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shutdown_flag_clone = shutdown_flag.clone();

                let host_clone = host.clone();
                let ipc_handle = tokio::spawn(async move {
                    if let Err(e) = run_ipc_server(host_clone, shutdown_rx).await {
                        tracing::error!("Daemon IPC server error: {e}");
                    }
                });

                let source = MockTelemetrySource::new();
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
            } else {
                info!(
                    backend = host.backend_name().await,
                    "Linux Quest host daemon starting"
                );

                let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
                let shutdown_tx_clone = shutdown_tx.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    info!("Ctrl-C signal received; stopping daemon...");
                    let _ = shutdown_tx_clone.send(());
                });

                run_ipc_server(host, shutdown_rx).await?;
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
