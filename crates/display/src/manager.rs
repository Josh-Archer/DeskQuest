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

/// One entry in the automatic backend selection: a backend that is available, or the reasons
/// it is not.
pub(crate) struct Candidate {
    label: &'static str,
    backend: Option<Box<dyn VirtualDisplayBackend>>,
    hints: Vec<String>,
}

impl Candidate {
    pub(crate) fn available(
        label: &'static str,
        backend: impl VirtualDisplayBackend + 'static,
    ) -> Self {
        Self {
            label,
            backend: Some(Box::new(backend)),
            hints: Vec::new(),
        }
    }

    pub(crate) fn unavailable(label: &'static str, hints: Vec<String>) -> Self {
        Self {
            label,
            backend: None,
            hints,
        }
    }

    pub(crate) fn probed(
        label: &'static str,
        is_available: bool,
        hints: Vec<String>,
        backend: impl VirtualDisplayBackend + 'static,
    ) -> Self {
        if is_available {
            Self::available(label, backend)
        } else {
            Self::unavailable(label, hints)
        }
    }
}

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
    /// dummy-display fallback (EVDI, see [`crate::backend::kernel_drm`]), which is always last.
    ///
    /// A backend that is unavailable, or whose `init` fails, is skipped and the next one is
    /// tried. If none works the error is `DisplayError::BackendUnavailable` listing every
    /// backend's diagnostic, including the commands that fix the kernel fallback (install
    /// `evdi-dkms`, `modprobe evdi`, group membership/udev).
    pub async fn create_auto() -> Result<Self, DisplayError> {
        let mut candidates = Vec::new();

        let wlr = WlrDisplayBackend::new();
        candidates.push(Candidate::probed(
            "wlroots (Sway/Hyprland)",
            wlr.is_available(),
            vec!["neither SWAYSOCK nor HYPRLAND_INSTANCE_SIGNATURE is set".into()],
            wlr,
        ));

        // Mutter needs an async D-Bus probe, so it cannot use `is_available()`.
        let mutter_probe = match zbus::Connection::session().await {
            Ok(conn) if MutterDisplayBackend::probe_screencast_service(&conn).await => Ok(()),
            Ok(_) => Err("org.gnome.Mutter.ScreenCast is not owned on the session bus".to_string()),
            Err(e) => Err(format!("no D-Bus session bus: {e}")),
        };
        candidates.push(match mutter_probe {
            Ok(()) => Candidate::available("GNOME Mutter", MutterDisplayBackend::new()),
            Err(why) => Candidate::unavailable("GNOME Mutter", vec![why]),
        });

        let x11 = X11DisplayBackend::new();
        candidates.push(Candidate::probed(
            "X11 (xrandr)",
            x11.is_available(),
            vec!["$DISPLAY is not set".into()],
            x11,
        ));

        // Kernel EVDI fallback: last resort for sessions without a virtual output API.
        let kernel = KernelDrmDisplayBackend::new();
        let hints = kernel.diagnostics().to_vec();
        candidates.push(Candidate::probed(
            "kernel DRM (EVDI)",
            kernel.is_available(),
            hints,
            kernel,
        ));

        Self::select(candidates).await
    }

    /// Picks the first candidate that is available and initialises, in order. Crate-visible so
    /// selection and the aggregated error can be tested with fake backends.
    pub(crate) async fn select(candidates: Vec<Candidate>) -> Result<Self, DisplayError> {
        let mut failures: Vec<String> = Vec::new();
        for candidate in candidates {
            let Candidate {
                label,
                backend,
                hints,
            } = candidate;
            match backend {
                None => failures.push(format!("{label}: {}", hints.join("; "))),
                Some(mut backend) => match backend.init().await {
                    Ok(()) => {
                        info!("Selected display backend: {}", backend.name());
                        return Ok(Self::new(backend));
                    }
                    Err(e) => failures.push(format!("{label}: init failed: {e}")),
                },
            }
        }
        Err(DisplayError::BackendUnavailable(format!(
            "No usable display backend. {}",
            failures
                .iter()
                .map(|f| format!("[{f}]"))
                .collect::<Vec<_>>()
                .join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kernel_drm::MockKernel;
    use async_trait::async_trait;

    /// Backend whose `init` result is scripted.
    struct Fake {
        name: &'static str,
        init_error: Option<&'static str>,
    }

    #[async_trait]
    impl VirtualDisplayBackend for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn init(&mut self) -> Result<(), DisplayError> {
            match self.init_error {
                Some(why) => Err(DisplayError::BackendUnavailable(why.into())),
                None => Ok(()),
            }
        }
        async fn create_monitor(
            &mut self,
            _config: &VirtualMonitorConfig,
        ) -> Result<VirtualMonitor, DisplayError> {
            Err(DisplayError::InvalidConfiguration("fake".into()))
        }
        async fn destroy_monitor(&mut self, _id: u32) -> Result<(), DisplayError> {
            Ok(())
        }
        async fn apply_layout(&mut self, _layout: &DisplayLayout) -> Result<(), DisplayError> {
            Ok(())
        }
        fn list_monitors(&self) -> Vec<VirtualMonitor> {
            Vec::new()
        }
    }

    fn fake(name: &'static str, init_error: Option<&'static str>) -> Fake {
        Fake { name, init_error }
    }

    #[tokio::test]
    async fn select_prefers_earlier_backends_over_the_kernel_fallback() {
        let kernel = KernelDrmDisplayBackend::with_ops(Box::new(MockKernel::new()));
        assert!(kernel.is_available());
        let mgr = VirtualDisplayManager::select(vec![
            Candidate::unavailable("wlroots", vec!["no socket".into()]),
            Candidate::available("X11", fake("x11", None)),
            Candidate::available("kernel DRM (EVDI)", kernel),
        ])
        .await
        .unwrap();
        assert_eq!(mgr.backend_name(), "x11");
    }

    #[tokio::test]
    async fn select_falls_through_a_failing_init_to_the_next_backend() {
        let mgr = VirtualDisplayManager::select(vec![
            Candidate::available("Mutter", fake("mutter", Some("bus gone"))),
            Candidate::available("kernel", fake("kernel", None)),
        ])
        .await
        .unwrap();
        assert_eq!(mgr.backend_name(), "kernel");
    }

    #[tokio::test]
    async fn select_uses_the_kernel_fallback_when_nothing_else_works() {
        let kernel = KernelDrmDisplayBackend::with_ops(Box::new(MockKernel::new()));
        let mgr = VirtualDisplayManager::select(vec![
            Candidate::unavailable("wlroots", vec!["no socket".into()]),
            Candidate::unavailable("X11", vec!["no DISPLAY".into()]),
            Candidate::available("kernel DRM (EVDI)", kernel),
        ])
        .await
        .unwrap();
        assert_eq!(mgr.backend_name(), "Kernel DRM (EVDI)");
    }

    #[tokio::test]
    async fn select_reports_every_backend_when_nothing_works() {
        let kernel =
            KernelDrmDisplayBackend::with_ops(Box::new(MockKernel::new().without_module()));
        let hints = kernel.diagnostics().to_vec();
        let Err(err) = VirtualDisplayManager::select(vec![
            Candidate::unavailable("wlroots (Sway/Hyprland)", vec!["no SWAYSOCK".into()]),
            Candidate::available("GNOME Mutter", fake("mutter", Some("bus gone"))),
            Candidate::unavailable("X11 (xrandr)", vec!["$DISPLAY is not set".into()]),
            Candidate::probed("kernel DRM (EVDI)", kernel.is_available(), hints, kernel),
        ])
        .await
        else {
            panic!("nothing should be usable");
        };
        let DisplayError::BackendUnavailable(msg) = err else {
            panic!("wrong error kind: {err:?}");
        };
        for needle in [
            "wlroots (Sway/Hyprland): no SWAYSOCK",
            "GNOME Mutter: init failed",
            "bus gone",
            "X11 (xrandr): $DISPLAY is not set",
            "kernel DRM (EVDI):",
            "apt install evdi-dkms",
            "sudo modprobe evdi",
        ] {
            assert!(msg.contains(needle), "missing `{needle}` in: {msg}");
        }
    }

    /// Smoke test of the real discovery path. Only asserts when the machine has no usable
    /// display backend (true in the sandbox and most CI); otherwise it skips.
    #[tokio::test]
    async fn create_auto_error_lists_all_four_backends() {
        match VirtualDisplayManager::create_auto().await {
            Ok(_) => {}
            Err(DisplayError::BackendUnavailable(msg)) => {
                for label in ["wlroots", "GNOME Mutter", "X11", "kernel DRM (EVDI)"] {
                    assert!(msg.contains(label), "missing {label} in: {msg}");
                }
                let order: Vec<usize> = ["wlroots", "GNOME Mutter", "X11", "kernel DRM (EVDI)"]
                    .iter()
                    .map(|l| msg.find(l).unwrap())
                    .collect();
                assert!(order.windows(2).all(|w| w[0] < w[1]), "kernel must be last");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
