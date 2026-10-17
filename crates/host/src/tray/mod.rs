//! System tray icon for the host, run with `linux-quest-host tray`.
//!
//! The tray is a separate process that polls the daemon over its IPC socket, so it
//! can outlive a daemon restart and can run while the daemon is down.
//!
//! Layout:
//! - [`model`], [`commands`], [`icon`] and [`controller`] are pure or have no D-Bus
//!   dependency. They compile and are tested in every build.
//! - `backend` (behind the `tray` cargo feature) is the StatusNotifierItem adapter
//!   built on `ksni`.

pub mod commands;
pub mod controller;
pub mod icon;
pub mod model;

#[cfg(feature = "tray")]
pub mod backend;

pub use commands::{
    dashboard_command, restart_command, CommandRunner, LaunchCommand, ProcessRunner,
};
pub use controller::{Flow, TrayConfig, TrayController, TrayHost};
pub use model::{menu_entries, ConnectionState, MenuEntry, TrayAction, TrayView};

#[cfg(feature = "tray")]
pub use backend::run_tray;
