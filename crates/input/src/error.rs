//! Error types for Linux input injection.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors arising during input injection and virtual device management.
#[derive(Error, Debug)]
pub enum InputError {
    #[error("Failed to open uinput device: {0}")]
    DeviceOpenFailed(String),

    #[error(
        "/dev/uinput does not exist at '{0}'. Ensure the uinput kernel module is loaded with 'sudo modprobe uinput'."
    )]
    UinputNotFound(PathBuf),

    #[error(
        "Permission denied accessing '{0}'. Current user lacks write permissions to /dev/uinput. Ensure your user is in the 'input' group ('sudo usermod -aG input $USER') or configure udev rule in /etc/udev/rules.d/99-uinput.rules."
    )]
    UinputPermissionDenied(PathBuf),

    #[error("Failed to configure uinput device: {0}")]
    DeviceSetupFailed(String),

    #[error("Failed to create uinput device: {0}")]
    DeviceCreateFailed(String),

    #[error("Failed to emit event: {0}")]
    EmitFailed(String),

    #[error("Clipboard sync error: {0}")]
    ClipboardError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl InputError {
    /// Classifies an I/O error encountered while opening `/dev/uinput` into a user-actionable error.
    pub fn from_open_io(err: std::io::Error, path: &Path) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => Self::UinputNotFound(path.to_path_buf()),
            std::io::ErrorKind::PermissionDenied => {
                Self::UinputPermissionDenied(path.to_path_buf())
            }
            _ => Self::DeviceOpenFailed(format!("Failed to open {}: {}", path.display(), err)),
        }
    }
}
