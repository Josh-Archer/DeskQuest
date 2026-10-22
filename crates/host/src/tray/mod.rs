//! System tray icon for the host, run with `linux-quest-host tray`.
//!
//! The tray is a separate process that polls the daemon over its IPC socket, so it
//! can outlive a daemon restart and can run while the daemon is down.
//!
//! Layout:
//! - [`model`], [`commands`], [`icon`] and [`controller`] are pure or have no D-Bus
//!   dependency, and are tested with mocks.
//! - [`backend`] is the StatusNotifierItem adapter built on `ksni` (pure Rust D-Bus, so it
//!   is part of every build).

pub mod backend;
pub mod commands;
pub mod controller;
pub mod icon;
pub mod model;

pub use backend::run_tray;
pub use commands::{
    app_command, dashboard_command, restart_command, CommandRunner, LaunchCommand, ProcessRunner,
};
pub use controller::{DaemonControl, Flow, IpcDaemonControl, TrayConfig, TrayController, TrayHost};
pub use model::{menu_entries, ConnectionState, MenuEntry, TrayAction, TrayView};
