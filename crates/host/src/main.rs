use clap::{Args, Parser, Subcommand, ValueEnum};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use linux_quest_display::DisplayLayoutMode;
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
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Virtual multi-monitor management commands (communicates with running daemon)
    Display(DisplayArgs),

    /// Run persistent background daemon holding virtual monitors and display sessions
    Daemon,
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
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon => {
            let host = Arc::new(MultiDisplayHost::create_auto().await?);
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

            // Wire input receiver service feeding remote Quest events into the injector
            let input_bind_addr = std::net::SocketAddr::from((
                [0, 0, 0, 0],
                linux_quest_transport::DEFAULT_INPUT_PORT,
            ));
            match linux_quest_host::HostInputReceiver::bind_auto(input_bind_addr).await {
                Ok(mut input_service) => {
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

            // Wire clipboard synchronization service over reliable control channel
            let control_bind_addr = std::net::SocketAddr::from((
                [0, 0, 0, 0],
                linux_quest_transport::DEFAULT_CONTROL_PORT,
            ));
            match linux_quest_host::HostClipboardService::bind_auto(control_bind_addr).await {
                Ok(mut clipboard_service) => {
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

            run_ipc_server(host, shutdown_rx).await?;
            info!("Daemon stopped cleanly.");
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
