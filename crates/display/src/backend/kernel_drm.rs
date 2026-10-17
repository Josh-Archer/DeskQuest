//! Kernel DRM dummy-display fallback backend (VKMS / EVDI).
//!
//! This is the last-resort backend in [`crate::VirtualDisplayManager::create_auto`]. It is
//! selected only when no wlroots, Mutter or X11 session is usable, and it provisions virtual
//! outputs by talking to the kernel directly instead of a display server API.
//!
//! # Driver selection
//!
//! 1. **VKMS** (in-tree `vkms` module) when `/sys/module/vkms` exists and the VKMS configfs
//!    root `/sys/kernel/config/vkms` exists and is writable.
//! 2. **EVDI** (out-of-tree module, usually via DKMS) when `/sys/devices/evdi` (or
//!    `/sys/module/evdi`) exists and `/sys/devices/evdi/add` is writable.
//!
//! When neither is ready, [`KernelDrmDisplayBackend::diagnostics`] explains what is missing
//! (module not loaded, configfs not mounted, not running as a user with write access) with
//! the command to fix it.
//!
//! # What is real
//!
//! - Module, sysfs and `/dev/dri` detection, permission checks and the diagnostics above.
//!   These are plain `std::fs` and `access(2)` calls, and the decision logic is unit-tested
//!   against [`MockKernel`] without hardware.
//! - EDID handling: a generated or caller-supplied EDID is checked for length, the VESA
//!   header and the base-block checksum before anything is written to the kernel.
//! - Lifecycle bookkeeping: outputs are tracked per monitor id, destroyed in the reverse
//!   path that created them, and a failed VKMS create removes the directories it made.
//!
//! # Best-effort, needs hardware validation
//!
//! Nothing in this module has been run against a real kernel. Specifically:
//!
//! - **VKMS configfs layout** (`<root>/linux-quest-<id>/connectors/VIRTUAL-<id>/edid` and
//!   `<root>/linux-quest-<id>/enabled`) follows the upstream configfs interface as remembered.
//!   The attribute names and whether `connectors/` is auto-created by configfs are unverified.
//!   VKMS configfs support depends on the kernel build.
//! - **EVDI** uses the `/sys/devices/evdi/add` attribute and the `EVDI_CONNECT` DRM ioctl.
//!   The request number and the [`EvdiConnectArgs`] layout are reproduced from memory of
//!   `evdi_drm.h` and are not checked against any EVDI release. A mismatch shows up as an
//!   ioctl error or as the module ignoring the EDID.
//! - **Mode setting**: that the kernel adopts the EDID's preferred timing for the virtual CRTC
//!   is assumed, not verified.
//! - **Visibility**: creating a DRM output does not show anything to the user. A compositor or
//!   another KMS client has to drive the new card. [`KernelDrmDisplayBackend::apply_layout`]
//!   only updates the geometry this backend reports; it does not reconfigure the kernel or a
//!   compositor.
//! - **EVDI teardown**: destroying a monitor disconnects the EDID but leaves the EVDI device
//!   node in place, because removing it could affect other EVDI clients.
//!
//! No new dependencies are added: `libc` was already a dependency of this crate, so this module
//! is not behind a cargo feature.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::{info, warn};

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::edid::EdidGenerator;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;

/// Maximum number of virtual outputs provisioned by this backend (matches the manager limit).
const MAX_MONITORS: u32 = 3;

const VKMS_MODULE_PATH: &str = "/sys/module/vkms";
const VKMS_CONFIGFS_ROOT: &str = "/sys/kernel/config/vkms";
const EVDI_MODULE_PATH: &str = "/sys/module/evdi";
const EVDI_DEVICES_PATH: &str = "/sys/devices/evdi";
const DRI_DIR: &str = "/dev/dri";
const DRM_CLASS_DIR: &str = "/sys/class/drm";

/// `DRM_IOCTL_BASE` is 'd'; `DRM_COMMAND_BASE` is 0x40; `DRM_EVDI_CONNECT` is 0x02.
const DRM_IOCTL_BASE: u32 = b'd' as u32;
const DRM_COMMAND_BASE: u32 = 0x40;
const DRM_EVDI_CONNECT: u32 = 0x02;

/// Linux `_IOW` encoding: direction (write = 1), argument size, type and number.
const fn iow(ty: u32, nr: u32, size: u32) -> u32 {
    (1 << 30) | (size << 16) | (ty << 8) | nr
}

/// Argument block for the EVDI_CONNECT ioctl.
///
/// UNVERIFIED: reproduced from memory of `evdi_drm.h`. Check against the installed EVDI
/// release before relying on it.
#[repr(C)]
#[derive(Debug)]
struct EvdiConnectArgs {
    connected: i32,
    dev_index: u32,
    edid_length: u32,
    edid: *const u8,
}

const DRM_IOCTL_EVDI_CONNECT: u32 = iow(
    DRM_IOCTL_BASE,
    DRM_COMMAND_BASE + DRM_EVDI_CONNECT,
    std::mem::size_of::<EvdiConnectArgs>() as u32,
);

/// Kernel driver that provisions the virtual outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelDriver {
    Vkms,
    Evdi,
}

/// Raw kernel access used by this backend.
///
/// [`SysKernel`] is the real implementation. [`MockKernel`] is an in-memory model for tests.
/// Reads take `&self`; anything that changes kernel state takes `&mut self`.
pub trait KernelOps: Send + Sync {
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Names of the entries directly inside `path`.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>>;
    /// File name of the symlink target at `path`, e.g. the driver name for `.../device/driver`.
    fn link_name(&self, path: &Path) -> Option<String>;
    /// Whether the current process may write `path` (`access(2)` with `W_OK`).
    fn is_writable(&self, path: &Path) -> bool;
    /// Effective uid of the current process, used in diagnostics.
    fn euid(&self) -> u32;
    /// Writes `data` to an existing attribute file.
    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Creates a single directory (`mkdir`, which is also how configfs items are created).
    fn create_dir(&mut self, path: &Path) -> io::Result<()>;
    /// Removes an empty directory (`rmdir`).
    fn remove_dir(&mut self, path: &Path) -> io::Result<()>;
    /// Issues EVDI_CONNECT on a DRM card node. With `connected == false` the EDID is ignored.
    fn evdi_connect(&mut self, card: &Path, connected: bool, edid: &[u8]) -> io::Result<()>;
}

/// Real kernel access using `std::fs`, `access(2)`, and the EVDI ioctl.
#[derive(Debug, Default, Clone, Copy)]
pub struct SysKernel;

fn access_ok(path: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string for the duration of the call.
    unsafe { libc::access(c_path.as_ptr(), mode) == 0 }
}

fn card_index(card: &Path) -> u32 {
    card.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("card"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

impl KernelOps for SysKernel {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect()
    }

    fn link_name(&self, path: &Path) -> Option<String> {
        std::fs::read_link(path)
            .ok()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    fn is_writable(&self, path: &Path) -> bool {
        access_ok(path, libc::W_OK)
    }

    fn euid(&self) -> u32 {
        // SAFETY: geteuid(2) has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        // No O_CREAT or O_TRUNC: sysfs and configfs attributes already exist, and configfs
        // does not support truncation.
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.write_all(data)
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn remove_dir(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn evdi_connect(&mut self, card: &Path, connected: bool, edid: &[u8]) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(card)?;
        let edid_length = if connected {
            u32::try_from(edid.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?
        } else {
            0
        };
        let mut args = EvdiConnectArgs {
            connected: i32::from(connected),
            dev_index: card_index(card),
            edid_length,
            edid: if connected {
                edid.as_ptr()
            } else {
                std::ptr::null()
            },
        };
        // SAFETY: `file` owns a valid open DRM node for the duration of the call. `args` is a
        // #[repr(C)] block for the ioctl. When connected, `edid` points at a slice that outlives
        // this synchronous call.
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                DRM_IOCTL_EVDI_CONNECT as _,
                &mut args as *mut EvdiConnectArgs,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// A DRM card node under `/dev/dri` and the driver bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardInfo {
    pub name: String,
    pub driver: Option<String>,
}

/// Snapshot of what the kernel exposes for virtual displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelProbe {
    pub euid: u32,
    pub dri_dir_present: bool,
    pub cards: Vec<CardInfo>,
    pub vkms_module: bool,
    pub vkms_configfs_present: bool,
    pub vkms_configfs_writable: bool,
    pub evdi_module: bool,
    pub evdi_add_writable: bool,
}

impl KernelProbe {
    /// Reads the probe fields through `ops`.
    pub fn scan(ops: &dyn KernelOps) -> Self {
        Self {
            euid: ops.euid(),
            dri_dir_present: ops.exists(Path::new(DRI_DIR)),
            cards: scan_cards(ops),
            vkms_module: ops.exists(Path::new(VKMS_MODULE_PATH)),
            vkms_configfs_present: ops.exists(Path::new(VKMS_CONFIGFS_ROOT)),
            vkms_configfs_writable: ops.is_writable(Path::new(VKMS_CONFIGFS_ROOT)),
            evdi_module: ops.exists(Path::new(EVDI_MODULE_PATH))
                || ops.exists(Path::new(EVDI_DEVICES_PATH)),
            evdi_add_writable: ops.is_writable(&Path::new(EVDI_DEVICES_PATH).join("add")),
        }
    }
}

fn is_card_node(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn scan_cards(ops: &dyn KernelOps) -> Vec<CardInfo> {
    let mut cards: Vec<CardInfo> = ops
        .list_dir(Path::new(DRI_DIR))
        .unwrap_or_default()
        .into_iter()
        .filter(|name| is_card_node(name))
        .map(|name| {
            let driver = ops.link_name(&Path::new(DRM_CLASS_DIR).join(&name).join("device/driver"));
            CardInfo { name, driver }
        })
        .collect();
    cards.sort_by(|a, b| a.name.cmp(&b.name));
    cards
}

/// Chooses a driver from a probe, or returns the diagnostics explaining why none is usable.
pub fn plan(probe: &KernelProbe) -> Result<KernelDriver, Vec<String>> {
    if probe.vkms_module && probe.vkms_configfs_present && probe.vkms_configfs_writable {
        return Ok(KernelDriver::Vkms);
    }
    if probe.evdi_module && probe.evdi_add_writable {
        return Ok(KernelDriver::Evdi);
    }

    let uid = probe.euid;
    let mut hints = Vec::new();
    if !probe.dri_dir_present {
        hints.push("no /dev/dri directory: no DRM driver is loaded".to_string());
    }
    if !probe.vkms_module {
        hints.push("VKMS module not loaded: run `sudo modprobe vkms`".to_string());
    } else if !probe.vkms_configfs_present {
        hints.push(
            "VKMS is loaded but /sys/kernel/config/vkms is missing: mount configfs with \
             `sudo modprobe configfs && sudo mount -t configfs none /sys/kernel/config`, \
             or this kernel may lack VKMS configfs support"
                .to_string(),
        );
    } else if !probe.vkms_configfs_writable {
        hints.push(format!(
            "/sys/kernel/config/vkms is not writable by uid {uid}: run as root or grant write access"
        ));
    }
    if !probe.evdi_module {
        hints.push(
            "EVDI module not loaded: run `sudo modprobe evdi` (requires the EVDI DKMS package)"
                .to_string(),
        );
    } else if !probe.evdi_add_writable {
        hints.push(format!(
            "EVDI is loaded but /sys/devices/evdi/add is not writable by uid {uid}: \
             run as root or grant write access"
        ));
    }
    if !probe.cards.is_empty() {
        let list = probe
            .cards
            .iter()
            .map(|c| {
                format!(
                    "{} ({})",
                    c.name,
                    c.driver.as_deref().unwrap_or("unknown driver")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        hints.push(format!("DRM cards present: {list}"));
    }
    Err(hints)
}

/// Validates an EDID block before it is handed to the kernel.
///
/// Checks that the length is a non-zero multiple of 128 bytes, the VESA header
/// `00 FF FF FF FF FF FF 00` is present, and the base block checksum is zero.
pub fn validate_edid(edid: &[u8]) -> Result<(), DisplayError> {
    const VESA_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.is_empty() || !edid.len().is_multiple_of(128) {
        return Err(DisplayError::Edid(format!(
            "EDID length {} is not a non-zero multiple of 128 bytes",
            edid.len()
        )));
    }
    if edid[..8] != VESA_HEADER {
        return Err(DisplayError::Edid(
            "EDID header is not the VESA 00 FF FF FF FF FF FF 00 pattern".into(),
        ));
    }
    if edid[..128].iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
        return Err(DisplayError::Edid(
            "EDID base block checksum is invalid".into(),
        ));
    }
    Ok(())
}

fn kernel_io_error(context: &str, path: &Path, err: io::Error) -> DisplayError {
    let hint = if err.kind() == io::ErrorKind::PermissionDenied {
        " (permission denied: run as root or grant write access)"
    } else {
        ""
    };
    DisplayError::CommandFailed(format!("{context} at {}: {err}{hint}", path.display()))
}

/// Kernel resources owned by one virtual monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KernelOutput {
    Vkms {
        device_dir: PathBuf,
        connector_dir: PathBuf,
    },
    Evdi {
        card: PathBuf,
    },
}

/// Virtual display backend that provisions outputs through VKMS or EVDI.
pub struct KernelDrmDisplayBackend {
    ops: Box<dyn KernelOps>,
    driver: Option<KernelDriver>,
    diagnostics: Vec<String>,
    monitors: HashMap<u32, VirtualMonitor>,
    outputs: HashMap<u32, KernelOutput>,
}

impl KernelDrmDisplayBackend {
    /// Probes the real kernel (`/sys`, `/dev/dri`) and selects a driver if one is usable.
    pub fn new() -> Self {
        Self::with_ops(Box::new(SysKernel))
    }

    /// Builds the backend over a custom [`KernelOps`], probing it immediately.
    pub fn with_ops(ops: Box<dyn KernelOps>) -> Self {
        let probe = KernelProbe::scan(ops.as_ref());
        let (driver, diagnostics) = match plan(&probe) {
            Ok(driver) => (Some(driver), Vec::new()),
            Err(hints) => (None, hints),
        };
        Self {
            ops,
            driver,
            diagnostics,
            monitors: HashMap::new(),
            outputs: HashMap::new(),
        }
    }

    /// The driver selected at construction, if any.
    pub fn driver(&self) -> Option<KernelDriver> {
        self.driver
    }

    /// Why the backend is unavailable, with suggested fixes. Empty when a driver was selected.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    fn unavailable_error(&self) -> DisplayError {
        DisplayError::BackendUnavailable(format!(
            "Kernel DRM dummy display (VKMS/EVDI) unavailable: {}",
            self.diagnostics.join("; ")
        ))
    }

    fn evdi_cards(&self) -> Vec<(PathBuf, String)> {
        scan_cards(self.ops.as_ref())
            .into_iter()
            .filter(|c| c.driver.as_deref() == Some("evdi"))
            .map(|c| (Path::new(DRI_DIR).join(&c.name), c.name))
            .collect()
    }

    /// Creates a VKMS device with one connector carrying `edid`, then enables it.
    ///
    /// The device directory is created first and must be new. On any later failure, only
    /// the entries under it (created by this call) are removed.
    fn provision_vkms(
        &mut self,
        id: u32,
        connector: &str,
        edid: &[u8],
    ) -> Result<KernelOutput, DisplayError> {
        let device_dir = Path::new(VKMS_CONFIGFS_ROOT).join(format!("linux-quest-{id}"));
        let connector_dir = device_dir.join("connectors").join(connector);

        self.ops
            .create_dir(&device_dir)
            .map_err(|e| kernel_io_error("create VKMS device", &device_dir, e))?;

        match self.populate_vkms(&device_dir, &connector_dir, edid) {
            Ok(()) => Ok(KernelOutput::Vkms {
                device_dir,
                connector_dir,
            }),
            Err(e) => {
                // Best effort: the connector is missing if its mkdir never ran, and that is fine.
                let _ = self.ops.remove_dir(&connector_dir);
                let _ = self.ops.remove_dir(&device_dir);
                Err(e)
            }
        }
    }

    fn populate_vkms(
        &mut self,
        device_dir: &Path,
        connector_dir: &Path,
        edid: &[u8],
    ) -> Result<(), DisplayError> {
        self.ops
            .create_dir(connector_dir)
            .map_err(|e| kernel_io_error("create VKMS connector", connector_dir, e))?;
        self.ops
            .write(&connector_dir.join("edid"), edid)
            .map_err(|e| kernel_io_error("write EDID to VKMS connector", connector_dir, e))?;
        self.ops
            .write(&device_dir.join("enabled"), b"1")
            .map_err(|e| kernel_io_error("enable VKMS device", device_dir, e))?;
        Ok(())
    }

    /// Adds an EVDI device and connects `edid` to the DRM card it creates.
    ///
    /// Returns the connector name (the card node name) and the output handle.
    fn provision_evdi(&mut self, edid: &[u8]) -> Result<(String, KernelOutput), DisplayError> {
        let add = Path::new(EVDI_DEVICES_PATH).join("add");
        let before: Vec<PathBuf> = self.evdi_cards().into_iter().map(|(p, _)| p).collect();

        self.ops
            .write(&add, b"1")
            .map_err(|e| kernel_io_error("add EVDI device", &add, e))?;

        let (card, name) = self
            .evdi_cards()
            .into_iter()
            .find(|(path, _)| !before.contains(path))
            .ok_or_else(|| {
                DisplayError::CommandFailed(
                    "EVDI device was added but no new evdi DRM card appeared under /dev/dri \
                     (check dmesg)"
                        .into(),
                )
            })?;

        if let Err(e) = self.ops.evdi_connect(&card, true, edid) {
            // The added device is left in place: removing it could affect other EVDI clients.
            warn!(
                "EVDI connect failed on {}; the device remains added",
                card.display()
            );
            return Err(kernel_io_error("connect EDID to EVDI device", &card, e));
        }
        Ok((name, KernelOutput::Evdi { card }))
    }

    fn teardown(&mut self, output: &KernelOutput) -> Result<(), DisplayError> {
        match output {
            KernelOutput::Vkms {
                device_dir,
                connector_dir,
            } => {
                self.ops
                    .write(&device_dir.join("enabled"), b"0")
                    .map_err(|e| kernel_io_error("disable VKMS device", device_dir, e))?;
                self.ops
                    .remove_dir(connector_dir)
                    .map_err(|e| kernel_io_error("remove VKMS connector", connector_dir, e))?;
                self.ops
                    .remove_dir(device_dir)
                    .map_err(|e| kernel_io_error("remove VKMS device", device_dir, e))?;
                Ok(())
            }
            KernelOutput::Evdi { card } => self
                .ops
                .evdi_connect(card, false, &[])
                .map_err(|e| kernel_io_error("disconnect EVDI device", card, e)),
        }
    }
}

impl Default for KernelDrmDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualDisplayBackend for KernelDrmDisplayBackend {
    fn name(&self) -> &'static str {
        "Kernel DRM (VKMS/EVDI)"
    }

    fn is_available(&self) -> bool {
        self.driver.is_some()
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        if self.driver.is_none() {
            return Err(self.unavailable_error());
        }
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        let Some(driver) = self.driver else {
            return Err(self.unavailable_error());
        };
        config.validate()?;
        if self.monitors.contains_key(&config.id) {
            return Err(DisplayError::InvalidConfiguration(format!(
                "Monitor id {} is already active",
                config.id
            )));
        }
        if self.monitors.len() as u32 >= MAX_MONITORS {
            return Err(DisplayError::MaxMonitorsExceeded(MAX_MONITORS));
        }

        let edid = match &config.edid {
            Some(bytes) => bytes.clone(),
            None => EdidGenerator::generate(
                config.width,
                config.height,
                config.refresh_rate,
                &config.name,
                config.id,
            )?,
        };
        validate_edid(&edid)?;

        let (connector, output) = match driver {
            KernelDriver::Vkms => {
                let connector = format!("VIRTUAL-{}", config.id);
                let output = self.provision_vkms(config.id, &connector, &edid)?;
                (connector, output)
            }
            KernelDriver::Evdi => self.provision_evdi(&edid)?,
        };

        let monitor = VirtualMonitor {
            id: config.id,
            name: config.name.clone(),
            connector: connector.clone(),
            width: config.width,
            height: config.height,
            refresh_rate: config.refresh_rate,
            x: 0,
            y: 0,
            dpi: config.dpi,
            is_virtual: true,
            edid_path: None,
        };
        info!(
            "Provisioned kernel DRM output {} ({:?}, {}x{}@{})",
            connector, driver, config.width, config.height, config.refresh_rate
        );
        self.monitors.insert(config.id, monitor.clone());
        self.outputs.insert(config.id, output);
        Ok(monitor)
    }

    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let output =
            self.outputs.get(&id).cloned().ok_or_else(|| {
                DisplayError::OutputNotFound(format!("Monitor id {id} not found"))
            })?;
        self.teardown(&output)?;
        self.outputs.remove(&id);
        self.monitors.remove(&id);
        Ok(())
    }

    /// Updates the geometry this backend reports. It does not reconfigure the kernel or
    /// any compositor.
    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
        for p in layout.compute_placements() {
            if let Some(m) = self.monitors.get_mut(&p.id) {
                m.x = p.x;
                m.y = p.y;
                m.width = p.width;
                m.height = p.height;
                m.refresh_rate = p.refresh_rate;
            }
        }
        Ok(())
    }

    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self.monitors.values().cloned().collect();
        list.sort_by_key(|m| m.id);
        list
    }
}

#[derive(Debug, Default)]
struct MockState {
    euid: u32,
    paths: BTreeSet<PathBuf>,
    read_only: BTreeSet<PathBuf>,
    links: HashMap<PathBuf, String>,
    writes: Vec<(PathBuf, Vec<u8>)>,
    connects: Vec<(PathBuf, bool, Vec<u8>)>,
    fail_writes_suffix: Option<String>,
    evdi_add_creates_card: bool,
    next_card: u32,
}

/// In-memory [`KernelOps`] for headless tests.
///
/// Clones share state, so a test can keep a handle after giving one clone to the backend.
/// `mkdir` does not check the parent (configfs creates default groups itself). Attribute
/// writes require the parent directory to exist and do not create a path.
#[derive(Debug, Clone, Default)]
pub struct MockKernel {
    state: Arc<Mutex<MockState>>,
}

impl MockKernel {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        let mut guard = self.state.lock().expect("mock kernel lock poisoned");
        f(&mut guard)
    }

    /// Marks a file or directory as present.
    pub fn with_path(self, path: &str) -> Self {
        self.with_state(|s| {
            s.paths.insert(PathBuf::from(path));
        });
        self
    }

    /// Marks a path as present but not writable by the current process.
    pub fn with_read_only(self, path: &str) -> Self {
        self.with_state(|s| {
            s.paths.insert(PathBuf::from(path));
            s.read_only.insert(PathBuf::from(path));
        });
        self
    }

    /// Makes `path` a symlink whose target file name is `target`.
    pub fn with_link(self, path: &str, target: &str) -> Self {
        self.with_state(|s| {
            s.links.insert(PathBuf::from(path), target.to_string());
        });
        self
    }

    pub fn with_euid(self, uid: u32) -> Self {
        self.with_state(|s| s.euid = uid);
        self
    }

    /// Writing to `<...>/evdi/add` creates `/dev/dri/cardN` bound to the `evdi` driver.
    pub fn with_evdi_add_creating_cards(self) -> Self {
        self.with_state(|s| s.evdi_add_creates_card = true);
        self
    }

    /// Makes attribute writes whose path ends with `suffix` fail.
    pub fn failing_writes_to(self, suffix: &str) -> Self {
        self.with_state(|s| s.fail_writes_suffix = Some(suffix.to_string()));
        self
    }

    /// Attribute writes made so far, in order.
    pub fn writes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.with_state(|s| s.writes.clone())
    }

    /// EVDI_CONNECT calls made so far, in order.
    pub fn connects(&self) -> Vec<(PathBuf, bool, Vec<u8>)> {
        self.with_state(|s| s.connects.clone())
    }

    /// Whether a path currently exists in the model.
    pub fn has_path(&self, path: &str) -> bool {
        self.with_state(|s| s.paths.contains(Path::new(path)))
    }
}

impl KernelOps for MockKernel {
    fn exists(&self, path: &Path) -> bool {
        self.with_state(|s| s.paths.contains(path))
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        self.with_state(|s| {
            if !s.paths.contains(path) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            Ok(s.paths
                .iter()
                .filter(|child| child.parent() == Some(path))
                .filter_map(|child| child.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect())
        })
    }

    fn link_name(&self, path: &Path) -> Option<String> {
        self.with_state(|s| s.links.get(path).cloned())
    }

    fn is_writable(&self, path: &Path) -> bool {
        self.with_state(|s| s.paths.contains(path) && !s.read_only.contains(path))
    }

    fn euid(&self) -> u32 {
        self.with_state(|s| s.euid)
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.with_state(|s| {
            if let Some(suffix) = &s.fail_writes_suffix {
                if path.ends_with(suffix) {
                    return Err(io::Error::other("injected write failure"));
                }
            }
            if s.read_only.contains(path) {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            if !path.parent().is_some_and(|p| s.paths.contains(p)) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            s.writes.push((path.to_path_buf(), data.to_vec()));
            if s.evdi_add_creates_card && path.ends_with("evdi/add") {
                let name = format!("card{}", s.next_card);
                s.next_card += 1;
                s.paths.insert(Path::new(DRI_DIR).join(&name));
                s.links.insert(
                    Path::new(DRM_CLASS_DIR).join(&name).join("device/driver"),
                    "evdi".to_string(),
                );
            }
            Ok(())
        })
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        self.with_state(|s| {
            if s.paths.contains(path) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            s.paths.insert(path.to_path_buf());
            Ok(())
        })
    }

    fn remove_dir(&mut self, path: &Path) -> io::Result<()> {
        self.with_state(|s| {
            if s.paths.remove(path) {
                Ok(())
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        })
    }

    fn evdi_connect(&mut self, card: &Path, connected: bool, edid: &[u8]) -> io::Result<()> {
        self.with_state(|s| {
            if !s.paths.contains(card) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            s.connects
                .push((card.to_path_buf(), connected, edid.to_vec()));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::DisplayLayoutMode;

    const DEVICE_DIR: &str = "/sys/kernel/config/vkms/linux-quest-1";
    const CONNECTOR_DIR: &str = "/sys/kernel/config/vkms/linux-quest-1/connectors/VIRTUAL-1";

    fn vkms_host() -> MockKernel {
        MockKernel::new()
            .with_path("/dev/dri")
            .with_path("/sys/module/vkms")
            .with_path("/sys/kernel/config/vkms")
            .with_euid(0)
    }

    fn evdi_host() -> MockKernel {
        MockKernel::new()
            .with_path("/dev/dri")
            .with_path("/sys/module/evdi")
            .with_path("/sys/devices/evdi")
            .with_path("/sys/devices/evdi/add")
            .with_evdi_add_creating_cards()
            .with_euid(0)
    }

    #[test]
    fn test_edid_validation_accepts_generated_block() {
        let edid = EdidGenerator::generate(1920, 1080, 60, "Quest-1080p-1", 1).unwrap();
        validate_edid(&edid).unwrap();
    }

    #[test]
    fn test_edid_validation_rejects_bad_length_header_and_checksum() {
        let err = validate_edid(&[0u8; 100]).unwrap_err();
        assert!(err.to_string().contains("multiple of 128"));

        let err = validate_edid(&[0u8; 128]).unwrap_err();
        assert!(err.to_string().contains("header"));

        let mut edid = EdidGenerator::generate(1920, 1080, 60, "Quest-1080p-1", 1).unwrap();
        edid[127] = edid[127].wrapping_add(1);
        let err = validate_edid(&edid).unwrap_err();
        assert!(err.to_string().contains("checksum"));
    }

    #[test]
    fn test_plan_prefers_vkms_when_configfs_writable() {
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(vkms_host()));
        assert!(backend.is_available());
        assert_eq!(backend.driver(), Some(KernelDriver::Vkms));
        assert!(backend.diagnostics().is_empty());
    }

    #[test]
    fn test_plan_selects_evdi_when_vkms_not_loaded() {
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(evdi_host()));
        assert_eq!(backend.driver(), Some(KernelDriver::Evdi));
    }

    #[test]
    fn test_unavailable_without_modules_suggests_modprobe() {
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(
            MockKernel::new().with_path("/dev/dri").with_euid(1000),
        ));
        assert!(!backend.is_available());
        let joined = backend.diagnostics().join("; ");
        assert!(joined.contains("sudo modprobe vkms"), "{joined}");
        assert!(joined.contains("sudo modprobe evdi"), "{joined}");
    }

    #[test]
    fn test_unavailable_without_dri_reports_no_driver() {
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(MockKernel::new()));
        assert!(!backend.is_available());
        assert!(backend.diagnostics().iter().any(|d| d.contains("/dev/dri")));
    }

    #[test]
    fn test_vkms_loaded_without_configfs_suggests_mount() {
        let kernel = MockKernel::new()
            .with_path("/dev/dri")
            .with_path("/sys/module/vkms")
            .with_euid(0);
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel));
        assert!(!backend.is_available());
        let joined = backend.diagnostics().join("; ");
        assert!(joined.contains("mount -t configfs"), "{joined}");
    }

    #[test]
    fn test_unwritable_configfs_reports_uid() {
        let kernel = vkms_host()
            .with_read_only("/sys/kernel/config/vkms")
            .with_euid(1000);
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel));
        assert!(!backend.is_available());
        let joined = backend.diagnostics().join("; ");
        assert!(joined.contains("uid 1000"), "{joined}");
    }

    #[test]
    fn test_evdi_add_not_writable_reports_permission() {
        let kernel = MockKernel::new()
            .with_path("/dev/dri")
            .with_path("/sys/module/evdi")
            .with_path("/sys/devices/evdi")
            .with_read_only("/sys/devices/evdi/add")
            .with_euid(1000);
        let backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel));
        assert!(!backend.is_available());
        assert!(backend
            .diagnostics()
            .iter()
            .any(|d| d.contains("/sys/devices/evdi/add is not writable by uid 1000")));
    }

    #[tokio::test]
    async fn test_init_fails_with_diagnostics_when_unavailable() {
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(MockKernel::new()));
        let err = backend.init().await.unwrap_err();
        match err {
            DisplayError::BackendUnavailable(msg) => assert!(msg.contains("modprobe vkms")),
            other => panic!("Expected BackendUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_vkms_monitor_lifecycle_writes_edid_and_enables() {
        let kernel = vkms_host();
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()));
        backend.init().await.unwrap();

        let cfg = VirtualMonitorConfig::preset_1080p(1, 60);
        let expected_edid = EdidGenerator::generate(1920, 1080, 60, &cfg.name, 1).unwrap();
        let monitor = backend.create_monitor(&cfg).await.unwrap();

        assert_eq!(monitor.connector, "VIRTUAL-1");
        assert!(monitor.is_virtual);
        assert!(kernel.has_path(CONNECTOR_DIR));
        let writes = kernel.writes();
        assert!(writes.contains(&(
            PathBuf::from(format!("{CONNECTOR_DIR}/edid")),
            expected_edid
        )));
        assert!(writes.contains(&(
            PathBuf::from(format!("{DEVICE_DIR}/enabled")),
            b"1".to_vec()
        )));

        backend.destroy_monitor(1).await.unwrap();
        assert!(kernel.writes().contains(&(
            PathBuf::from(format!("{DEVICE_DIR}/enabled")),
            b"0".to_vec()
        )));
        assert!(!kernel.has_path(CONNECTOR_DIR));
        assert!(!kernel.has_path(DEVICE_DIR));
        assert!(backend.list_monitors().is_empty());
    }

    #[tokio::test]
    async fn test_vkms_failed_edid_write_rolls_back_directories() {
        let kernel = vkms_host().failing_writes_to("edid");
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()));

        let cfg = VirtualMonitorConfig::preset_1080p(1, 60);
        let err = backend.create_monitor(&cfg).await.unwrap_err();
        assert!(err.to_string().contains("write EDID"), "{err}");
        assert!(!kernel.has_path(CONNECTOR_DIR));
        assert!(!kernel.has_path(DEVICE_DIR));
        assert!(backend.list_monitors().is_empty());
    }

    #[tokio::test]
    async fn test_malformed_edid_rejected_before_touching_kernel() {
        let kernel = vkms_host();
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()));

        let cfg = VirtualMonitorConfig::preset_1080p(1, 60).with_edid(vec![0u8; 100]);
        let err = backend.create_monitor(&cfg).await.unwrap_err();
        assert!(matches!(err, DisplayError::Edid(_)), "{err:?}");
        assert!(kernel.writes().is_empty());
        assert!(!kernel.has_path(DEVICE_DIR));
    }

    #[tokio::test]
    async fn test_destroy_unknown_monitor_is_output_not_found() {
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(vkms_host()));
        let err = backend.destroy_monitor(9).await.unwrap_err();
        assert!(matches!(err, DisplayError::OutputNotFound(_)));
    }

    #[tokio::test]
    async fn test_evdi_monitor_connects_edid_to_new_card() {
        let kernel = evdi_host();
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel.clone()));
        assert_eq!(backend.driver(), Some(KernelDriver::Evdi));

        let cfg = VirtualMonitorConfig::preset_1440p(2, 60);
        let expected_edid = EdidGenerator::generate(2560, 1440, 60, &cfg.name, 2).unwrap();
        let monitor = backend.create_monitor(&cfg).await.unwrap();

        assert_eq!(monitor.connector, "card0");
        assert!(kernel
            .writes()
            .contains(&(PathBuf::from("/sys/devices/evdi/add"), b"1".to_vec())));
        assert_eq!(
            kernel.connects(),
            vec![(PathBuf::from("/dev/dri/card0"), true, expected_edid)]
        );

        backend.destroy_monitor(2).await.unwrap();
        let connects = kernel.connects();
        assert_eq!(
            connects.last(),
            Some(&(PathBuf::from("/dev/dri/card0"), false, Vec::new()))
        );
    }

    #[tokio::test]
    async fn test_evdi_add_without_new_card_is_reported() {
        let kernel = MockKernel::new()
            .with_path("/dev/dri")
            .with_path("/sys/module/evdi")
            .with_path("/sys/devices/evdi")
            .with_path("/sys/devices/evdi/add")
            .with_euid(0);
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(kernel));
        let cfg = VirtualMonitorConfig::preset_1080p(1, 60);
        let err = backend.create_monitor(&cfg).await.unwrap_err();
        assert!(err.to_string().contains("no new evdi DRM card"), "{err}");
        assert!(backend.list_monitors().is_empty());
    }

    #[tokio::test]
    async fn test_apply_layout_updates_reported_geometry() {
        let mut backend = KernelDrmDisplayBackend::with_ops(Box::new(vkms_host()));
        let cfg1 = VirtualMonitorConfig::preset_1080p(1, 60);
        backend.create_monitor(&cfg1).await.unwrap();

        let layout = DisplayLayout::new(DisplayLayoutMode::Horizontal, vec![cfg1]);
        backend.apply_layout(&layout).await.unwrap();
        assert_eq!(backend.list_monitors()[0].x, 0);
    }
}
