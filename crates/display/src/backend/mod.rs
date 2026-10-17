pub mod kernel_drm;
pub mod mock;
pub mod mutter;
pub mod wlr;
pub mod x11;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;

/// Represents an active virtual monitor on the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMonitor {
    pub id: u32,
    pub name: String,
    pub connector: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub x: i32,
    pub y: i32,
    pub dpi: u32,
    pub is_virtual: bool,
    pub edid_path: Option<String>,
}

/// Common trait implemented by all display server integration backends.
#[async_trait]
pub trait VirtualDisplayBackend: Send + Sync {
    /// Human-readable backend name (e.g. "Mutter (D-Bus)", "wlroots (Wayland)", "X11 (xrandr)").
    fn name(&self) -> &'static str;

    /// Checks if this backend is currently accessible in the running desktop session.
    fn is_available(&self) -> bool;

    /// Initializes backend connection and resources.
    async fn init(&mut self) -> Result<(), DisplayError>;

    /// Spawns a new virtual monitor output.
    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError>;

    /// Destroys a previously created virtual monitor.
    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError>;

    /// Applies layout positioning across active monitors.
    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError>;

    /// Lists all currently tracked virtual monitors.
    fn list_monitors(&self) -> Vec<VirtualMonitor>;
}
