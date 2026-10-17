use std::collections::HashSet;
use tracing::info;

use crate::backend::kernel_drm::KernelDrmDisplayBackend;
use crate::backend::mutter::MutterDisplayBackend;
use crate::backend::wlr::WlrDisplayBackend;
use crate::backend::x11::X11DisplayBackend;
use crate::backend::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::edid::EdidGenerator;
use crate::error::DisplayError;
use crate::layout::{DisplayLayout, DisplayLayoutMode};

/// High-level virtual multi-monitor manager managing monitor lifecycle, layouts, and display servers.
pub struct VirtualDisplayManager {
    backend: Box<dyn VirtualDisplayBackend>,
    configs: Vec<VirtualMonitorConfig>,
    layout_mode: DisplayLayoutMode,
}

impl VirtualDisplayManager {
    /// Creates a manager with an explicitly chosen backend.
    pub fn new(backend: Box<dyn VirtualDisplayBackend>) -> Self {
        Self {
            backend,
            configs: Vec::new(),
            layout_mode: DisplayLayoutMode::Horizontal,
        }
    }

    /// Automatically discovers and selects the best available display backend for the current environment.
    ///
    /// Selection order: wlroots (Sway/Hyprland), GNOME Mutter, X11, then the kernel DRM
    /// dummy-display fallback (VKMS/EVDI, see [`crate::backend::kernel_drm`]).
    ///
    /// Returns `DisplayError::BackendUnavailable` if none of these is usable. The message
    /// includes the kernel fallback's diagnostics (missing module, permissions) so the user
    /// knows what to fix.
    pub async fn create_auto() -> Result<Self, DisplayError> {
        // 1. Check wlroots / Sway / Hyprland (preferred when Wayland socket is active)
        let wlr = WlrDisplayBackend::new();
        if wlr.is_available() {
            info!("Selected wlroots/Wayland display backend: {}", wlr.name());
            let mut mgr = Self::new(Box::new(wlr));
            mgr.backend.init().await?;
            return Ok(mgr);
        }

        // 2. Check GNOME Mutter (D-Bus)
        if let Ok(conn) = zbus::Connection::session().await {
            if MutterDisplayBackend::probe_screencast_service(&conn).await {
                info!("Selected GNOME Mutter display backend");
                let mut mutter = MutterDisplayBackend::new();
                mutter.init().await?;
                return Ok(Self::new(Box::new(mutter)));
            }
        }

        // 3. Check X11 (xrandr)
        let x11 = X11DisplayBackend::new();
        if x11.is_available() {
            info!("Selected X11 display backend: {}", x11.name());
            let mut mgr = Self::new(Box::new(x11));
            mgr.backend.init().await?;
            return Ok(mgr);
        }

        // 4. Kernel DRM dummy display (VKMS / EVDI): last resort for compositors without a
        // virtual output API. Needs no session socket, only the kernel module and access.
        let kernel = KernelDrmDisplayBackend::new();
        if kernel.is_available() {
            info!(
                "Selected kernel DRM dummy-display backend: {}",
                kernel.name()
            );
            let mut mgr = Self::new(Box::new(kernel));
            mgr.backend.init().await?;
            return Ok(mgr);
        }

        Err(DisplayError::BackendUnavailable(format!(
            "No supported display server detected (GNOME Mutter, Sway, Hyprland, or X11), \
             and the kernel DRM fallback (VKMS/EVDI) is unavailable: {}",
            kernel.diagnostics().join("; ")
        )))
    }

    /// Returns the name of the active backend.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Dynamically sets the total number of active virtual monitors (1, 2, or 3).
    /// Creates, updates, or destroys monitors as needed and recalculates layout positions.
    pub async fn set_monitor_count(
        &mut self,
        target_count: u32,
        base_config: &VirtualMonitorConfig,
        layout_mode: DisplayLayoutMode,
    ) -> Result<Vec<VirtualMonitor>, DisplayError> {
        if target_count == 0 {
            return Err(DisplayError::InvalidConfiguration(
                "Target monitor count cannot be 0".into(),
            ));
        }
        if target_count > 3 {
            return Err(DisplayError::MaxMonitorsExceeded(3));
        }

        base_config.validate()?;
        self.layout_mode = layout_mode;
        let current_count = self.configs.len() as u32;

        // 1. Ensure existing monitors are aligned to requested mode
        for existing in &mut self.configs {
            if existing.width != base_config.width
                || existing.height != base_config.height
                || existing.refresh_rate != base_config.refresh_rate
                || existing.dpi != base_config.dpi
            {
                let name = format!("{}-{}", base_config.name, existing.id);
                let edid = EdidGenerator::generate(
                    base_config.width,
                    base_config.height,
                    base_config.refresh_rate,
                    &name,
                    existing.id,
                )?;

                let mut new_cfg = VirtualMonitorConfig::new(
                    existing.id,
                    name,
                    base_config.width,
                    base_config.height,
                    base_config.refresh_rate,
                );
                new_cfg.dpi = base_config.dpi;
                new_cfg.edid = Some(edid);

                let old_cfg = existing.clone();
                self.backend.destroy_monitor(existing.id).await?;
                if let Err(e) = self.backend.create_monitor(&new_cfg).await {
                    // Transactional rollback: attempt to restore previous monitor configuration
                    if self.backend.create_monitor(&old_cfg).await.is_ok() {
                        *existing = old_cfg;
                    } else {
                        let failed_id = existing.id;
                        self.configs.retain(|c| c.id != failed_id);
                    }
                    return Err(e);
                }
                *existing = new_cfg;
            } else {
                existing.dpi = base_config.dpi;
            }
        }

        // 2. Adjust count
        if target_count > current_count {
            let to_add = target_count - current_count;
            let existing_ids: HashSet<u32> = self.configs.iter().map(|c| c.id).collect();
            let mut available_ids: Vec<u32> =
                (1..=3).filter(|id| !existing_ids.contains(id)).collect();
            available_ids.sort();

            for &id in available_ids.iter().take(to_add as usize) {
                let name = format!("{}-{id}", base_config.name);
                let edid = EdidGenerator::generate(
                    base_config.width,
                    base_config.height,
                    base_config.refresh_rate,
                    &name,
                    id,
                )?;

                let mut cfg = VirtualMonitorConfig::new(
                    id,
                    name,
                    base_config.width,
                    base_config.height,
                    base_config.refresh_rate,
                );
                cfg.dpi = base_config.dpi;
                cfg.edid = Some(edid);

                self.backend.create_monitor(&cfg).await?;
                self.configs.push(cfg);
            }
        } else if target_count < current_count {
            let to_remove = current_count - target_count;
            let mut ids_to_remove: Vec<u32> = self.configs.iter().map(|c| c.id).collect();
            ids_to_remove.sort();

            for &id in ids_to_remove.iter().rev().take(to_remove as usize) {
                self.backend.destroy_monitor(id).await?;
                self.configs.retain(|c| c.id != id);
            }
        }

        // 3. Maintain strictly sorted monitor order by ID for stable spatial placement
        self.configs.sort_by_key(|c| c.id);

        // 4. Apply spatial layout across active monitors
        self.refresh_layout().await?;

        Ok(self.backend.list_monitors())
    }

    /// Adds a single virtual monitor with allocation from available IDs.
    pub async fn add_monitor(
        &mut self,
        mut config: VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;
        if self.configs.len() >= 3 {
            return Err(DisplayError::MaxMonitorsExceeded(3));
        }

        let existing_ids: HashSet<u32> = self.configs.iter().map(|c| c.id).collect();
        if existing_ids.contains(&config.id) {
            let next_id = (1..=3).find(|id| !existing_ids.contains(id)).unwrap_or(1);
            config.id = next_id;
        }

        if config.edid.is_none() {
            let edid = EdidGenerator::generate(
                config.width,
                config.height,
                config.refresh_rate,
                &config.name,
                config.id,
            )?;
            config.edid = Some(edid);
        }

        let monitor = self.backend.create_monitor(&config).await?;
        self.configs.push(config);
        self.configs.sort_by_key(|c| c.id);
        self.refresh_layout().await?;
        Ok(monitor)
    }

    /// Destroys a virtual monitor by identifier.
    pub async fn remove_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        self.backend.destroy_monitor(id).await?;
        self.configs.retain(|c| c.id != id);
        self.configs.sort_by_key(|c| c.id);
        self.refresh_layout().await?;
        Ok(())
    }

    /// Sets the desktop layout arrangement (Horizontal, Vertical, Grid).
    pub async fn set_layout_mode(&mut self, mode: DisplayLayoutMode) -> Result<(), DisplayError> {
        self.layout_mode = mode;
        self.refresh_layout().await?;
        Ok(())
    }

    /// Re-evaluates and applies monitor positions.
    async fn refresh_layout(&mut self) -> Result<(), DisplayError> {
        let layout = DisplayLayout::new(self.layout_mode, self.configs.clone());
        self.backend.apply_layout(&layout).await
    }

    /// Returns a list of all currently active virtual monitors.
    pub fn list_active_monitors(&self) -> Vec<VirtualMonitor> {
        self.backend.list_monitors()
    }
}
