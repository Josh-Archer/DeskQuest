//! Kernel DRM dummy-display fallback backend, EVDI only.
//!
//! Last-resort backend in [`crate::VirtualDisplayManager::create_auto`]: it is used only when
//! no wlroots, Mutter or X11 session is usable. It provisions virtual outputs through the
//! out-of-tree EVDI ("Extensible Virtual Display Interface") kernel module.
//!
//! # Why not VKMS
//!
//! An earlier version also drove VKMS through configfs. That cannot work for this feature:
//! the VKMS configfs connector exposes only a `status` attribute (no `edid`), so the EDID
//! write that carries the requested resolution always fails, and a usable pipeline needs
//! plane + CRTC + encoder + connector items with `possible_*` symlinks and ordered teardown,
//! none of which was implemented. The VKMS path was removed instead of kept half-working.
//!
//! # How EVDI works (what this module relies on)
//!
//! Sources: `module/evdi_drm.h`, `evdi_sysfs.c`, `evdi_platform_drv.c`, `evdi_painter.c`,
//! `evdi_connector.c`, `evdi_drm_drv.c` and `library/evdi_lib.c` of DisplayLink/evdi at the
//! commit pinned in `tests/fixtures/evdi-ee3a4b2-abi.txt`.
//!
//! - Writing a decimal count to `/sys/devices/evdi/add` (mode 0200, root only) creates that
//!   many platform devices `evdi.N` (at most 16 in total) and a DRM card for each. The module
//!   parameter `initial_device_count` pre-creates cards at load. Devices are only removed by
//!   `remove_all`, which destroys *every* card including other applications'. **This backend
//!   never writes `remove_all`**; cards it created are left in place and reused.
//! - Each card is `/sys/bus/platform/devices/evdi.N/drm/cardM` (libevdi finds cards this way)
//!   with node `/dev/dri/cardM`. A card with a `device` symlink next to it is attached to a USB
//!   DisplayLink dock and is never touched here.
//! - `DRM_IOCTL_EVDI_CONNECT` on an open card fd plugs a monitor in with the supplied EDID.
//!   **The module disconnects when that fd is closed** (`evdi_painter_close`, called from
//!   `postclose`/`preclose`, when `file == painter->drm_filp`). So the fd is stored in the
//!   output handle and closing it *is* the teardown; no `connected = 0` call is needed.
//! - A second CONNECT from another fd on a card that is already connected does not fail: it
//!   silently takes the card over ("Double connect - replacing"). Idle cards are therefore
//!   chosen by reading the connector `status` in sysfs first, never by trying to connect.
//! - The client must service the card fd for the compositor to treat the output as live; see
//!   [`painter`].
//!
//! # Card selection and creation
//!
//! Scan, select and CONNECT run under an exclusive `flock` ([`KernelOps::lock_selection`]) so
//! two processes (or two backends in one process) using this crate cannot both pick the same
//! idle card. The lock is held until CONNECT has succeeded.
//!
//! 1. Reuse an evdi card that is not USB-attached and whose connector status is
//!    `disconnected` (an unreadable status counts as busy).
//! 2. Otherwise write `1` to `add`, then poll (bounded, [`ADD_WAIT`]) for a card that was not
//!    in the before-set, retrying `open` on `ENOENT`/`EACCES` while udev creates the node and
//!    applies its permissions. A new card is only chosen if, after a short settle delay
//!    ([`USB_SETTLE`]), it still has no USB marker and its status reads `disconnected`:
//!    the module creates the `device` symlink of a USB-attached card slightly after the card
//!    itself appears.
//! 3. After opening, the fd is made non-master like libevdi does (see `drop_master`); if it
//!    is still master afterwards the open fails.
//!
//! Remaining limits (not solvable from here): the lock only serialises clients that take it.
//! The DisplayLink service, other `libevdi` users and anything writing `add` directly do not,
//! so a card they create or connect between our scan and our CONNECT can still be taken
//! ("double connect" silently replaces the other client). A USB marker that appears later than
//! [`USB_SETTLE`] after the card shows up is not seen either. If no lock file can be created
//! safely the monitor creation fails instead of proceeding unlocked.
//!
//! Before any EVDI ioctl, the opened node is checked with `DRM_IOCTL_VERSION` to be driver
//! `evdi`: request `0x40` is `DRM_COMMAND_BASE + 0` for every DRM driver, so sending it to an
//! i915/amdgpu node would invoke some unrelated driver ioctl.
//!
//! # Verified vs unverified
//!
//! Verified by reading upstream source and a C compiler (see [`abi`]): ioctl request numbers,
//! struct layouts, event layouts, sysfs/`add` semantics, disconnect-on-close, double-connect,
//! vblank/`update_ready` handling.
//!
//! Not verified (no EVDI module, GPU or display exists where this was written; nothing has
//! been run against real EVDI): that the kernel really exposes `status` under
//! `/sys/class/drm/cardN-*` for the evdi connector (derived from generic DRM sysfs and the
//! connector type `DVI-I` in `evdi_connector.c`), where `/sys/class/drm/cardN/device/driver`
//! points (not used; selection goes through the libevdi layout above plus the
//! `DRM_IOCTL_VERSION` check), that a compositor accepts and lights the output, and the
//! udev permission recipe in the diagnostics.

pub mod abi;
#[doc(hidden)]
pub mod mock;
pub mod painter;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::edid::EdidGenerator;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;

pub use mock::MockKernel;
pub use painter::{PainterService, PainterSnapshot};

use abi::{
    as_bytes_mut, DrmEvdiConnect, DrmVersion, DRM_IOCTL_AUTH_MAGIC, DRM_IOCTL_DROP_MASTER,
    DRM_IOCTL_EVDI_CONNECT, DRM_IOCTL_VERSION, EVDI_DRIVER_NAME, MIN_MODULE_MINOR, MODULE_MAJOR,
};

/// Root of the EVDI sysfs interface (`root_device_register("evdi")`).
pub const EVDI_ROOT: &str = "/sys/devices/evdi";
const PLATFORM_DEVICES: &str = "/sys/bus/platform/devices";
const DRM_CLASS: &str = "/sys/class/drm";
const DRI_DIR: &str = "/dev/dri";
/// Upper bound on virtual monitors, matching the manager.
const MAX_MONITORS: u32 = 3;
/// The module caps the number of devices at `EVDI_DEVICE_COUNT_MAX`.
const EVDI_DEVICE_COUNT_MAX: usize = 16;
/// How long to wait for a freshly added card's node to appear and become openable.
pub const ADD_WAIT: Duration = Duration::from_millis(2000);
/// Poll interval while waiting for `add`.
pub const ADD_POLL: Duration = Duration::from_millis(20);
/// Settle delay before trusting the (absence of a) USB marker on a freshly added card.
pub const USB_SETTLE: Duration = Duration::from_millis(100);
/// How long to wait for the cross-process selection lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(20);
/// Maximum EDID size the module accepts: `255 * 128 + sizeof(struct edid)`.
const MAX_EDID_SIZE: usize = 256 * 128;

// ---------------------------------------------------------------------------------------
// Kernel access abstraction
// ---------------------------------------------------------------------------------------

/// An open DRM card fd. Dropping the last reference closes the fd, which makes the EVDI
/// module disconnect the monitor that fd connected.
pub trait CardHandle: Send + Sync {
    /// `ioctl(2)`. `arg.len()` must equal the size encoded in `request` (checked by
    /// [`SysKernel`]; the model in [`mock`] enforces it too). Returns the non-negative
    /// return value.
    fn ioctl(&self, request: u32, arg: &mut [u8]) -> io::Result<i32>;
    /// Non-blocking `read(2)` of DRM events; `WouldBlock` when none are queued.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// Waits until the fd is readable or `timeout` elapses. `Ok(false)` on timeout.
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool>;
    /// Declares the allocation behind the destination buffer of the next `GRABPIX`. The real
    /// kernel cannot know a user buffer's length (an undersized one is a user-space memory
    /// bug), so [`SysCard`] ignores this; the model in [`mock`] uses it to bounds-check what
    /// the kernel would write.
    fn register_buffer(&self, _ptr: *mut u8, _len: usize) {}
}

/// Guard for the cross-process card-selection lock; dropping it releases the lock.
pub trait SelectionLock: Send {}

/// Raw kernel/filesystem access used by this backend.
///
/// [`SysKernel`] is the real implementation; [`MockKernel`] is a model of the EVDI rules for
/// tests. All methods take `&self` so a background thread and the backend can share one.
pub trait KernelOps: Send + Sync {
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Names of the entries directly inside `path`.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>>;
    /// Reads a small text file (sysfs attribute).
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// `access(path, W_OK)`.
    fn is_writable(&self, path: &Path) -> bool;
    /// `access(path, R_OK | W_OK)`.
    fn can_read_write(&self, path: &Path) -> bool;
    /// Writes `data` to an existing sysfs attribute (no create, no truncate).
    fn write_attr(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Opens a DRM card node read/write, non-blocking, close-on-exec.
    fn open_card(&self, path: &Path) -> io::Result<Arc<dyn CardHandle>>;
    /// Takes the exclusive card-selection lock (waiting a bounded time), see the module docs.
    fn lock_selection(&self) -> io::Result<Box<dyn SelectionLock>>;
    /// Effective uid, for diagnostics.
    fn euid(&self) -> u32;
    /// Sleeps; the model uses a virtual clock.
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Real kernel access.
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

struct SysCard {
    file: std::fs::File,
}

impl CardHandle for SysCard {
    fn ioctl(&self, request: u32, arg: &mut [u8]) -> io::Result<i32> {
        use std::os::unix::io::AsRawFd;
        if abi::ioc_size(request) != arg.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ioctl {request:#010x} encodes {} argument bytes but {} were supplied",
                    abi::ioc_size(request),
                    arg.len()
                ),
            ));
        }
        let ptr = if arg.is_empty() {
            std::ptr::null_mut()
        } else {
            arg.as_mut_ptr()
        };
        loop {
            // SAFETY: the fd is open for the lifetime of `self`; `ptr` is null (no-argument
            // request) or points to `arg.len()` bytes, exactly the size encoded in `request`.
            let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), request as _, ptr) };
            if rc >= 0 {
                return Ok(rc);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        use std::io::Read;
        (&self.file).read(buf)
    }

    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        use std::os::unix::io::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one valid pollfd for the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(err)
            };
        }
        if pfd.revents & libc::POLLIN != 0 {
            return Ok(true);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "DRM fd reported an error or hang-up",
            ));
        }
        Ok(false)
    }
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

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn is_writable(&self, path: &Path) -> bool {
        access_ok(path, libc::W_OK)
    }

    fn can_read_write(&self, path: &Path) -> bool {
        access_ok(path, libc::R_OK | libc::W_OK)
    }

    fn write_attr(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        // No O_CREAT/O_TRUNC: sysfs attributes already exist.
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.write_all(data)
    }

    fn open_card(&self, path: &Path) -> io::Result<Arc<dyn CardHandle>> {
        use std::os::unix::fs::OpenOptionsExt;
        // std adds O_CLOEXEC: the fd must not leak into child processes, or closing ours
        // would not disconnect the monitor.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        Ok(Arc::new(SysCard { file }))
    }

    fn lock_selection(&self) -> io::Result<Box<dyn SelectionLock>> {
        let euid = self.euid();
        let path = lock_file_path(euid);
        let file = open_lock_file(&path, euid)?;
        flock_exclusive(&file, LOCK_WAIT, LOCK_POLL)
            .map_err(|e| io::Error::new(e.kind(), format!("lock file {}: {e}", path.display())))?;
        Ok(Box::new(FlockGuard { _file: file }))
    }

    fn euid(&self) -> u32 {
        // SAFETY: geteuid(2) has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }
}

/// Holds the locked file; closing it releases the `flock`.
struct FlockGuard {
    _file: std::fs::File,
}

impl SelectionLock for FlockGuard {}

/// Whether `dir` is a directory owned by `euid` that nobody else can write to.
fn private_dir(dir: &Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).is_ok_and(|m| m.is_dir() && m.uid() == euid && m.mode() & 0o022 == 0)
}

/// Where the selection lock lives: `$XDG_RUNTIME_DIR` or `/run/user/$UID` if that is a private
/// directory of ours, else `/tmp/linux-quest-evdi-$UID.lock` (sticky, world-writable: the file
/// itself is verified by [`open_lock_file`]).
fn lock_file_path(euid: u32) -> PathBuf {
    const NAME: &str = "linux-quest-evdi.lock";
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && private_dir(p, euid));
    if let Some(dir) = xdg {
        return dir.join(NAME);
    }
    let run = PathBuf::from(format!("/run/user/{euid}"));
    if private_dir(&run, euid) {
        return run.join(NAME);
    }
    PathBuf::from(format!("/tmp/linux-quest-evdi-{euid}.lock"))
}

/// Opens (creating with mode 0600) the lock file without following symlinks and verifies it
/// is a regular, single-link file owned by `euid` that only the owner can access. Anything
/// else (a file pre-created by another user, a symlink, a hard link) is refused.
fn open_lock_file(path: &Path, euid: u32) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot open lock file {} safely: {e}", path.display()),
            )
        })?;
    let md = file.metadata()?;
    if !md.file_type().is_file() || md.uid() != euid || md.mode() & 0o077 != 0 || md.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing lock file {}: it must be a regular file with a single link, owned by \
                 uid {euid} and mode 0600 (found uid {}, mode {:o}, {} link(s))",
                path.display(),
                md.uid(),
                md.mode() & 0o7777,
                md.nlink()
            ),
        ));
    }
    Ok(file)
}

/// `flock(LOCK_EX)` polled with `LOCK_NB` so a stuck holder cannot hang us forever.
fn flock_exclusive(file: &std::fs::File, wait: Duration, poll: Duration) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let deadline = std::time::Instant::now() + wait;
    loop {
        // SAFETY: the fd is open for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock => {}
            io::ErrorKind::Interrupted => continue,
            _ => return Err(err),
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "another process held the EVDI card-selection lock for more than {}s",
                    wait.as_secs_f32()
                ),
            ));
        }
        std::thread::sleep(poll);
    }
}

// ---------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------

/// An EVDI DRM card as seen through sysfs and `/dev/dri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvdiCard {
    /// DRM minor, the `N` of `/dev/dri/cardN`.
    pub index: u32,
    /// Platform device name, e.g. `evdi.0`.
    pub platform: String,
    /// Attached to a USB parent (a DisplayLink dock): never used by this backend.
    pub usb_attached: bool,
    /// Connector status: `Some(true)` connected, `Some(false)` disconnected, `None` if it
    /// could not be read.
    pub connected: Option<bool>,
    /// `/dev/dri/cardN` exists.
    pub node_present: bool,
    /// The current process can open the node read/write (`access(2)`).
    pub accessible: bool,
}

impl EvdiCard {
    /// Path of the device node.
    pub fn node(&self) -> PathBuf {
        Path::new(DRI_DIR).join(format!("card{}", self.index))
    }

    /// Whether this card is a candidate for reuse: ours to take and known to be idle.
    pub fn is_reusable(&self) -> bool {
        !self.usb_attached && self.connected == Some(false)
    }
}

fn parse_card_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("card")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn connector_connected(ops: &dyn KernelOps, index: u32) -> Option<bool> {
    let prefix = format!("card{index}-");
    let entries = ops.list_dir(Path::new(DRM_CLASS)).ok()?;
    let mut seen = false;
    for entry in entries.iter().filter(|e| e.starts_with(&prefix)) {
        let status = ops
            .read_to_string(&Path::new(DRM_CLASS).join(entry).join("status"))
            .ok()?;
        seen = true;
        if status.trim() == "connected" {
            return Some(true);
        }
    }
    seen.then_some(false)
}

/// Lists EVDI cards, sorted by DRM minor.
///
/// Layout from libevdi (`find_unused_card_for`): `/sys/bus/platform/devices/evdi*` each with
/// `drm/cardN`; a `device` entry next to `drm` marks a USB-attached card.
pub fn scan_evdi_cards(ops: &dyn KernelOps) -> Vec<EvdiCard> {
    let Ok(platform) = ops.list_dir(Path::new(PLATFORM_DEVICES)) else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for pdev in platform.iter().filter(|n| n.starts_with("evdi")) {
        let dir = Path::new(PLATFORM_DEVICES).join(pdev);
        let Ok(drm) = ops.list_dir(&dir.join("drm")) else {
            continue;
        };
        for index in drm.iter().filter_map(|n| parse_card_index(n)) {
            let node = Path::new(DRI_DIR).join(format!("card{index}"));
            cards.push(EvdiCard {
                index,
                platform: pdev.clone(),
                usb_attached: ops.exists(&dir.join("device")),
                connected: connector_connected(ops, index),
                node_present: ops.exists(&node),
                accessible: ops.can_read_write(&node),
            });
        }
    }
    cards.sort_by_key(|c| c.index);
    cards
}

/// Everything the availability decision is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelProbe {
    pub euid: u32,
    /// `/sys/devices/evdi` exists, i.e. the module is loaded.
    pub module_loaded: bool,
    /// `/sys/devices/evdi/add` is writable by this process.
    pub add_writable: bool,
    pub cards: Vec<EvdiCard>,
}

impl KernelProbe {
    pub fn scan(ops: &dyn KernelOps) -> Self {
        let root = Path::new(EVDI_ROOT);
        let module_loaded = ops.exists(root);
        Self {
            euid: ops.euid(),
            module_loaded,
            add_writable: module_loaded && ops.is_writable(&root.join("add")),
            cards: if module_loaded {
                scan_evdi_cards(ops)
            } else {
                Vec::new()
            },
        }
    }

    /// Idle, non-USB cards this process can open.
    pub fn usable_idle_cards(&self) -> impl Iterator<Item = &EvdiCard> {
        self.cards
            .iter()
            .filter(|c| c.is_reusable() && c.node_present && c.accessible)
    }

    /// Whether a monitor could be created right now: the module is loaded and either `add`
    /// is writable or an idle card is openable.
    pub fn is_ready(&self) -> bool {
        self.module_loaded && (self.add_writable || self.usable_idle_cards().next().is_some())
    }

    /// Why the backend is unavailable, each entry with the exact fix. Empty when ready.
    pub fn diagnostics(&self) -> Vec<String> {
        if self.is_ready() {
            return Vec::new();
        }
        if !self.module_loaded {
            return vec![format!(
                "EVDI kernel module is not loaded ({EVDI_ROOT} is missing). Install it \
                 (Debian/Ubuntu: `sudo apt install evdi-dkms`; other distros: build \
                 DisplayLink/evdi) and load it with `sudo modprobe evdi`"
            )];
        }
        let mut out = Vec::new();
        let idle: Vec<_> = self
            .cards
            .iter()
            .filter(|c| c.is_reusable() && c.node_present)
            .collect();
        if !idle.is_empty() {
            let names: Vec<String> = idle.iter().map(|c| format!("card{}", c.index)).collect();
            out.push(format!(
                "EVDI card(s) {} are idle but /dev/dri/cardN is not readable and writable by \
                 uid {} (nodes are normally root:video 0660, applied by udev). Add yourself \
                 to the owning group with `sudo usermod -aG video $USER` and log in again, or \
                 install a udev rule that grants your seat/group access to evdi cards",
                names.join(", "),
                self.euid
            ));
        } else if !self.cards.is_empty() {
            let busy = self.cards.iter().filter(|c| !c.is_reusable()).count();
            out.push(format!(
                "all {busy} EVDI card(s) are USB-attached, connected to another client, or \
                 have an unreadable connector status; free one or add another"
            ));
        }
        if !self.add_writable {
            out.push(format!(
                "{EVDI_ROOT}/add is not writable by uid {} (it is mode 0200, root only), so a \
                 new card cannot be created. Run as root, or pre-create cards at module load: \
                 `echo 'options evdi initial_device_count=2' | sudo tee /etc/modprobe.d/evdi.conf \
                 && sudo modprobe -r evdi && sudo modprobe evdi`",
                self.euid
            ));
        }
        out
    }
}

/// Checks that `edid` is something the EVDI module will accept and a compositor can parse:
/// a non-zero multiple of 128 bytes, at most 256 blocks (the module's `MAX_EDID_SIZE`),
/// starting with the VESA header, every 128-byte block summing to zero.
pub fn validate_edid(edid: &[u8]) -> Result<(), DisplayError> {
    const VESA_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.is_empty() || !edid.len().is_multiple_of(128) {
        return Err(DisplayError::Edid(format!(
            "EDID length {} is not a non-zero multiple of 128 bytes",
            edid.len()
        )));
    }
    if edid.len() > MAX_EDID_SIZE {
        return Err(DisplayError::Edid(format!(
            "EDID length {} exceeds the EVDI module limit of {MAX_EDID_SIZE} bytes",
            edid.len()
        )));
    }
    if edid[..8] != VESA_HEADER {
        return Err(DisplayError::Edid(
            "EDID header is not the VESA 00 FF FF FF FF FF FF 00 pattern".into(),
        ));
    }
    for (i, block) in edid.chunks(128).enumerate() {
        if block.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
            return Err(DisplayError::Edid(format!(
                "EDID block {i} checksum is invalid"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------------------

/// A connected monitor. Owns the card fd that issued CONNECT and the thread servicing it.
///
/// Dropping stops and joins the thread first, then closes the fd; closing the fd is what makes
/// the module disconnect the monitor. Field order matters only for documentation: `Drop` does
/// the ordering explicitly.
pub struct EvdiOutput {
    index: u32,
    service: Option<PainterService>,
    card: Arc<dyn CardHandle>,
    failure_logged: AtomicBool,
}

impl EvdiOutput {
    /// DRM minor of the card this monitor lives on.
    pub fn card_index(&self) -> u32 {
        self.index
    }

    /// Servicing statistics.
    pub fn stats(&self) -> Option<PainterSnapshot> {
        self.service.as_ref().map(PainterService::stats)
    }

    /// Whether the servicing thread is alive.
    pub fn is_serviced(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(PainterService::is_running)
    }

    /// `Err(reason)` if nobody services the card any more (painter thread died on a fatal
    /// error or panicked): the compositor sees a connected output that never presents.
    pub fn health(&self) -> Result<(), String> {
        if self.is_serviced() {
            return Ok(());
        }
        Err(self
            .service
            .as_ref()
            .and_then(PainterService::failure)
            .unwrap_or_else(|| "painter thread is not running".into()))
    }

    /// Logs the failure at error level, the first time only.
    fn log_failure_once(&self, id: u32, reason: &str) {
        if !self.failure_logged.swap(true, Ordering::Relaxed) {
            error!(
                "EVDI monitor {id} on card{} failed and is no longer serviced: {reason}",
                self.index
            );
        }
    }
}

impl Drop for EvdiOutput {
    fn drop(&mut self) {
        // Stop and join the thread (it holds a clone of the fd), then `self.card` is dropped
        // after this body returns: the last reference closes the fd and the module
        // disconnects. Nothing here can panic. The join is bounded by `painter::STOP_TIMEOUT`
        // (normally one `POLL_INTERVAL`); if the thread is stuck in the kernel it is detached
        // and keeps the fd open until it returns. Async callers go through `destroy_monitor`,
        // which runs this on a blocking thread.
        if let Some(mut service) = self.service.take() {
            service.stop();
        }
        debug!(
            "Closing EVDI card{} (strong refs {})",
            self.index,
            Arc::strong_count(&self.card)
        );
    }
}

#[derive(Debug)]
enum ConnectError {
    Open(io::Error),
    NotEvdi(String),
    ModuleVersion(String),
    Master(String),
    Ioctl(&'static str, io::Error),
    Thread(io::Error),
}

impl ConnectError {
    /// Worth retrying while udev creates the node and applies permissions after `add`.
    fn is_transient(&self) -> bool {
        matches!(
            self,
            ConnectError::Open(e)
                if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied)
        )
    }

    /// Failures that say the card or module is unusable for us, not that udev is still
    /// working: creating yet another card would fail the same way.
    fn is_environment_fault(&self) -> bool {
        matches!(
            self,
            ConnectError::NotEvdi(_) | ConnectError::ModuleVersion(_) | ConnectError::Master(_)
        )
    }

    fn describe(&self, path: &Path, euid: u32) -> String {
        match self {
            ConnectError::Open(e) if e.kind() == io::ErrorKind::PermissionDenied => format!(
                "cannot open {}: permission denied for uid {euid}. EVDI nodes are normally \
                 root:video 0660 (udev applies this, sometimes a moment after the card \
                 appears). Add yourself to the group with `sudo usermod -aG video $USER` and \
                 log in again, or run as root",
                path.display()
            ),
            ConnectError::Open(e) => format!("cannot open {}: {e}", path.display()),
            ConnectError::NotEvdi(name) => format!(
                "{} is driver `{name}`, not `{EVDI_DRIVER_NAME}`; refusing to send EVDI ioctls",
                path.display()
            ),
            ConnectError::ModuleVersion(v) => format!(
                "{}: EVDI module version {v} is not compatible (need {MODULE_MAJOR}.x with \
                 x >= {MIN_MODULE_MINOR}); update evdi-dkms",
                path.display()
            ),
            ConnectError::Master(why) => format!(
                "{}: this process is still DRM master of the card ({why}); a compositor could \
                 not take it. Refusing to connect",
                path.display()
            ),
            ConnectError::Ioctl(what, e) => format!("{what} on {} failed: {e}", path.display()),
            ConnectError::Thread(e) => format!("cannot start the EVDI painter thread: {e}"),
        }
    }
}

/// `DRM_IOCTL_VERSION` check: the node must be the `evdi` driver, module `1.x`, `x >= 9`.
fn verify_evdi(card: &dyn CardHandle) -> Result<(), ConnectError> {
    let mut name = [0u8; 64];
    let mut date = [0u8; 64];
    let mut desc = [0u8; 64];
    let mut v = DrmVersion {
        version_major: 0,
        version_minor: 0,
        version_patchlevel: 0,
        _pad: 0,
        name_len: name.len(),
        name: name.as_mut_ptr().cast(),
        date_len: date.len(),
        date: date.as_mut_ptr().cast(),
        desc_len: desc.len(),
        desc: desc.as_mut_ptr().cast(),
    };
    // SAFETY: DrmVersion has explicit padding only; the buffers outlive the call.
    let bytes = unsafe { as_bytes_mut(&mut v) };
    card.ioctl(DRM_IOCTL_VERSION, bytes)
        .map_err(|e| ConnectError::Ioctl("DRM_IOCTL_VERSION", e))?;
    let len = v.name_len.min(name.len());
    let found = String::from_utf8_lossy(&name[..len]).into_owned();
    if found != EVDI_DRIVER_NAME {
        return Err(ConnectError::NotEvdi(found));
    }
    if v.version_major != MODULE_MAJOR || v.version_minor < MIN_MODULE_MINOR {
        return Err(ConnectError::ModuleVersion(format!(
            "{}.{}.{}",
            v.version_major, v.version_minor, v.version_patchlevel
        )));
    }
    Ok(())
}

/// libevdi's `drm_is_master`: `DRM_IOCTL_AUTH_MAGIC` with magic 0 needs master, so a non-master
/// fd fails with `EACCES`; anything else (success or `EINVAL`) means master.
fn is_master(card: &dyn CardHandle) -> bool {
    let mut magic = [0u8; 4];
    match card.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic) {
        Err(e) => e.raw_os_error() != Some(libc::EACCES),
        Ok(_) => true,
    }
}

/// Like libevdi's `open_as_slave`: a fresh open of an idle primary node can be DRM master,
/// which would stop the compositor from taking master on the card. Drop it, then verify; fail
/// if it cannot be dropped or the fd is still master afterwards.
fn drop_master(card: &dyn CardHandle, index: u32) -> Result<(), ConnectError> {
    if !is_master(card) {
        return Ok(());
    }
    debug!("card{index}: fd is DRM master after open, dropping it");
    card.ioctl(DRM_IOCTL_DROP_MASTER, &mut [])
        .map_err(|e| ConnectError::Master(format!("DRM_IOCTL_DROP_MASTER failed: {e}")))?;
    if is_master(card) {
        return Err(ConnectError::Master(
            "still master after DRM_IOCTL_DROP_MASTER".into(),
        ));
    }
    Ok(())
}

/// Opens `/dev/dri/card{index}`, verifies it, connects `edid`, and starts servicing.
fn open_and_connect(
    ops: &dyn KernelOps,
    index: u32,
    edid: &[u8],
) -> Result<EvdiOutput, ConnectError> {
    let path = Path::new(DRI_DIR).join(format!("card{index}"));
    let card = ops.open_card(&path).map_err(ConnectError::Open)?;
    verify_evdi(card.as_ref())?;

    drop_master(card.as_ref(), index)?;

    let mut args = DrmEvdiConnect {
        connected: 1,
        dev_index: index as i32,
        edid: edid.as_ptr(),
        edid_length: u32::try_from(edid.len())
            .map_err(|_| ConnectError::Ioctl("connect", io::ErrorKind::InvalidInput.into()))?,
        // 0 = no limit (`evdi_mode_valid` returns MODE_OK when pixel_per_second_limit == 0).
        pixel_area_limit: 0,
        pixel_per_second_limit: 0,
        _pad: 0,
    };
    // SAFETY: DrmEvdiConnect has explicit padding only; `edid` outlives the synchronous call.
    let bytes = unsafe { as_bytes_mut(&mut args) };
    card.ioctl(DRM_IOCTL_EVDI_CONNECT, bytes)
        .map_err(|e| ConnectError::Ioctl("DRM_IOCTL_EVDI_CONNECT", e))?;

    // If starting the thread fails, `card` is dropped here: the fd closes and the module
    // disconnects again.
    let service = PainterService::spawn(card.clone(), format!("card{index}"))
        .map_err(ConnectError::Thread)?;
    Ok(EvdiOutput {
        index,
        service: Some(service),
        card,
        failure_logged: AtomicBool::new(false),
    })
}

fn add_error(err: &io::Error, euid: u32) -> DisplayError {
    let add = format!("{EVDI_ROOT}/add");
    let msg = match err.kind() {
        io::ErrorKind::PermissionDenied => format!(
            "cannot write {add}: permission denied for uid {euid} (mode 0200, root only). Run \
             as root, or pre-create cards: `echo 'options evdi initial_device_count=2' | sudo \
             tee /etc/modprobe.d/evdi.conf && sudo modprobe -r evdi && sudo modprobe evdi`"
        ),
        io::ErrorKind::InvalidInput => format!(
            "{add} rejected the request: the module allows at most {EVDI_DEVICE_COUNT_MAX} \
             devices. Stop other EVDI clients so cards become idle and reusable; this backend \
             never calls remove_all because it would destroy other applications' monitors"
        ),
        _ => format!("cannot write {add}: {err}"),
    };
    DisplayError::CommandFailed(msg)
}

/// Finds an idle card or creates one, and connects `edid` to it. Blocking.
///
/// Holds the cross-process selection lock for the whole call, i.e. until CONNECT succeeded
/// (the returned output keeps the connection; the lock is released on return).
fn acquire_output(
    ops: &dyn KernelOps,
    edid: &[u8],
    held: &HashSet<u32>,
) -> Result<EvdiOutput, DisplayError> {
    let euid = ops.euid();
    let _lock = ops.lock_selection().map_err(|e| {
        DisplayError::CommandFailed(format!(
            "cannot take the EVDI card-selection lock ({e}); not choosing a card without it, \
             because another process could be taking the same one"
        ))
    })?;
    let mut notes: Vec<String> = Vec::new();
    let mut environment_fault: Option<String> = None;

    // 1. Reuse an idle card.
    let cards = scan_evdi_cards(ops);
    for card in cards
        .iter()
        .filter(|c| c.is_reusable() && !held.contains(&c.index))
    {
        match open_and_connect(ops, card.index, edid) {
            Ok(out) => {
                info!("Reusing idle EVDI card{}", card.index);
                return Ok(out);
            }
            Err(e) => {
                let note = e.describe(&card.node(), euid);
                warn!("Skipping EVDI card{}: {note}", card.index);
                if e.is_environment_fault() {
                    environment_fault.get_or_insert_with(|| note.clone());
                }
                notes.push(note);
            }
        }
    }
    // A wrong driver, incompatible module or undroppable master is not fixed by adding
    // another card (it would fail the same way): report it instead.
    if let Some(fault) = environment_fault {
        return Err(DisplayError::CommandFailed(format!(
            "{fault}. Not creating another card, it would fail the same way"
        )));
    }

    // 2. Create one.
    let add = Path::new(EVDI_ROOT).join("add");
    if !ops.exists(&add) {
        return Err(DisplayError::BackendUnavailable(format!(
            "EVDI module is not loaded ({EVDI_ROOT} missing); run `sudo modprobe evdi`"
        )));
    }
    let before: HashSet<u32> = cards.iter().map(|c| c.index).collect();
    ops.write_attr(&add, b"1")
        .map_err(|e| add_error(&e, euid))?;

    let attempts = (ADD_WAIT.as_millis() / ADD_POLL.as_millis()).max(1);
    let mut last_transient: Option<String> = None;
    let mut settled: HashSet<u32> = HashSet::new();
    let mut rejected: HashMap<u32, String> = HashMap::new();
    for _ in 0..attempts {
        for fresh in scan_evdi_cards(ops)
            .into_iter()
            .filter(|c| !before.contains(&c.index) && !held.contains(&c.index))
        {
            // The module creates the `device` symlink of a USB-attached card a moment after
            // the card appears: look again once, after a bounded settle delay, before
            // trusting "no USB marker".
            let card = if settled.insert(fresh.index) {
                ops.sleep(USB_SETTLE);
                match scan_evdi_cards(ops)
                    .into_iter()
                    .find(|c| c.index == fresh.index)
                {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                fresh
            };
            if card.usb_attached {
                rejected.insert(card.index, format!("card{} is USB-attached", card.index));
                continue;
            }
            match card.connected {
                Some(false) => {}
                Some(true) => {
                    rejected.insert(
                        card.index,
                        format!("card{} is already connected to another client", card.index),
                    );
                    continue;
                }
                None => {
                    // Unreadable status is treated as busy, exactly as on the reuse path.
                    rejected.insert(
                        card.index,
                        format!("card{} has an unreadable connector status", card.index),
                    );
                    continue;
                }
            }
            match open_and_connect(ops, card.index, edid) {
                Ok(out) => {
                    info!("Created and connected EVDI card{}", card.index);
                    return Ok(out);
                }
                Err(e) if e.is_transient() => {
                    last_transient = Some(e.describe(&card.node(), euid));
                }
                Err(e) => {
                    return Err(DisplayError::CommandFailed(e.describe(&card.node(), euid)));
                }
            }
        }
        ops.sleep(ADD_POLL);
    }

    let mut msg = match last_transient {
        Some(t) => format!(
            "EVDI card was added but could not be opened within {}ms: {t}",
            ADD_WAIT.as_millis()
        ),
        None => format!(
            "wrote 1 to {EVDI_ROOT}/add but no usable new evdi DRM card appeared within {}ms \
             (check `dmesg | grep -i evdi`)",
            ADD_WAIT.as_millis()
        ),
    };
    if !rejected.is_empty() {
        let mut why: Vec<_> = rejected.into_values().collect();
        why.sort();
        msg.push_str(&format!(". New card(s) not used: {}", why.join("; ")));
    }
    msg.push_str(
        ". The added device stays in place and will be reused by the next attempt; it is \
         never removed individually",
    );
    if !notes.is_empty() {
        msg.push_str(&format!(". Earlier attempts: {}", notes.join("; ")));
    }
    Err(DisplayError::CommandFailed(msg))
}

// ---------------------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------------------

/// Virtual display backend that provisions outputs through EVDI.
pub struct KernelDrmDisplayBackend {
    ops: Arc<dyn KernelOps>,
    available: bool,
    diagnostics: Vec<String>,
    monitors: HashMap<u32, VirtualMonitor>,
    outputs: HashMap<u32, EvdiOutput>,
}

impl KernelDrmDisplayBackend {
    /// Probes the real kernel (`/sys`, `/dev/dri`).
    pub fn new() -> Self {
        Self::with_ops(Box::new(SysKernel))
    }

    /// Builds the backend over custom [`KernelOps`], probing immediately.
    pub fn with_ops(ops: Box<dyn KernelOps>) -> Self {
        let mut backend = Self {
            ops: Arc::from(ops),
            available: false,
            diagnostics: Vec::new(),
            monitors: HashMap::new(),
            outputs: HashMap::new(),
        };
        backend.refresh_probe();
        backend
    }

    fn refresh_probe(&mut self) {
        let probe = KernelProbe::scan(self.ops.as_ref());
        self.available = probe.is_ready();
        self.diagnostics = probe.diagnostics();
    }

    /// Why the backend is unavailable, with the fix commands. Empty when it is available.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// DRM minor of the card behind monitor `id`.
    pub fn card_index(&self, id: u32) -> Option<u32> {
        self.outputs.get(&id).map(EvdiOutput::card_index)
    }

    /// Event-servicing counters for monitor `id`.
    pub fn painter_stats(&self, id: u32) -> Option<PainterSnapshot> {
        self.outputs.get(&id).and_then(EvdiOutput::stats)
    }

    /// Monitors whose painter thread died (fatal error or panic), with the reason. Their
    /// outputs are still registered but nobody services them: the compositor sees a
    /// connected output that never presents.
    pub fn failed_monitors(&self) -> Vec<(u32, String)> {
        let mut failed: Vec<_> = self
            .outputs
            .iter()
            .filter_map(|(id, out)| out.health().err().map(|why| (*id, why)))
            .collect();
        failed.sort_by_key(|(id, _)| *id);
        failed
    }

    /// Health check: tears down every monitor whose painter died (closing its fd frees the
    /// card for reuse) and returns one error per torn-down monitor. Logs at error level the
    /// first time a failure is seen. Also run at the start of `init`/`create_monitor`.
    pub fn check_health(&mut self) -> Vec<(u32, DisplayError)> {
        let mut errors = Vec::new();
        for (id, why) in self.failed_monitors() {
            if let Some(out) = self.outputs.remove(&id) {
                out.log_failure_once(id, &why);
                // The thread already exited, so this drop does not block.
                let card = out.card_index();
                drop(out);
                errors.push((
                    id,
                    DisplayError::CommandFailed(format!(
                        "monitor {id} on card{card} failed and was torn down: {why}"
                    )),
                ));
            }
            self.monitors.remove(&id);
        }
        errors
    }

    fn unavailable_error(&self) -> DisplayError {
        DisplayError::BackendUnavailable(format!(
            "Kernel DRM dummy display (EVDI) unavailable: {}",
            self.diagnostics.join("; ")
        ))
    }
}

impl Default for KernelDrmDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KernelDrmDisplayBackend {
    fn drop(&mut self) {
        // Dropping each output joins its painter thread and closes its fd, which disconnects
        // the monitor in the kernel. Best effort; nothing here panics. This blocks the
        // calling thread for about one painter poll interval per output (bounded by
        // `painter::STOP_TIMEOUT` each if a thread is stuck in the kernel); call
        // `destroy_monitor` first from async code if that matters.
        if !self.outputs.is_empty() {
            debug!("Dropping {} EVDI output(s)", self.outputs.len());
            self.outputs.clear();
        }
    }
}

#[async_trait]
impl VirtualDisplayBackend for KernelDrmDisplayBackend {
    fn name(&self) -> &'static str {
        "Kernel DRM (EVDI)"
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        self.check_health();
        self.refresh_probe();
        if !self.available {
            return Err(self.unavailable_error());
        }
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        // Reap monitors whose painter died so their ids and cards are free again.
        self.check_health();
        self.refresh_probe();
        if !self.available {
            return Err(self.unavailable_error());
        }
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

        let ops = self.ops.clone();
        let held: HashSet<u32> = self.outputs.values().map(EvdiOutput::card_index).collect();
        // Polling and sleeping happen on a blocking thread, not the async executor.
        let output =
            tokio::task::spawn_blocking(move || acquire_output(ops.as_ref(), &edid, &held))
                .await
                .map_err(|e| DisplayError::CommandFailed(format!("EVDI worker failed: {e}")))??;

        // Sysfs name of the connector: `card{N}-DVI-I-1` (type DVI-I from evdi_connector.c,
        // one connector per card). Compositors will usually show it as `DVI-I-1`; unverified.
        let connector = format!("card{}-DVI-I-1", output.card_index());
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
            "Provisioned EVDI output {connector} ({}x{}@{})",
            config.width, config.height, config.refresh_rate
        );
        self.monitors.insert(config.id, monitor.clone());
        self.outputs.insert(config.id, output);
        Ok(monitor)
    }

    /// Closing the card fd is the teardown: the module disconnects the monitor and the card
    /// becomes idle and reusable. The card itself is never removed.
    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let output = self
            .outputs
            .remove(&id)
            .ok_or_else(|| DisplayError::OutputNotFound(format!("Monitor id {id} not found")))?;
        self.monitors.remove(&id);
        // Joining the painter thread can take up to one poll interval.
        if tokio::task::spawn_blocking(move || drop(output))
            .await
            .is_err()
        {
            warn!("EVDI teardown task failed for monitor {id}");
        }
        Ok(())
    }

    /// Updates the geometry this backend reports. It does not reconfigure the kernel or any
    /// compositor: placement of an EVDI monitor is the compositor's decision.
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

    /// Lists the monitors that are alive. A monitor whose painter died is *not* listed (it is
    /// failed, see [`KernelDrmDisplayBackend::failed_monitors`]); it is logged at error level
    /// the first time and torn down by the next [`KernelDrmDisplayBackend::check_health`],
    /// `init` or `create_monitor`.
    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self
            .monitors
            .values()
            .filter(|m| match self.outputs.get(&m.id) {
                Some(out) => match out.health() {
                    Ok(()) => true,
                    Err(why) => {
                        out.log_failure_once(m.id, &why);
                        false
                    }
                },
                None => false,
            })
            .cloned()
            .collect();
        list.sort_by_key(|m| m.id);
        list
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A fresh scratch directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lq-evdi-lock-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn euid() -> u32 {
        SysKernel.euid()
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn lock_file_is_created_private_and_excludes_a_second_holder() {
        let dir = scratch("excl");
        let path = dir.join("evdi.lock");
        let first = open_lock_file(&path, euid()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        flock_exclusive(&first, Duration::from_millis(50), Duration::from_millis(5)).unwrap();

        // A second open file description conflicts, even inside one process.
        let second = open_lock_file(&path, euid()).unwrap();
        let err = flock_exclusive(&second, Duration::from_millis(60), Duration::from_millis(5))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        drop(first); // releases the lock
        flock_exclusive(
            &second,
            Duration::from_millis(200),
            Duration::from_millis(5),
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_waits_for_a_holder_that_releases_in_time() {
        let dir = scratch("wait");
        let path = dir.join("evdi.lock");
        let holder = open_lock_file(&path, euid()).unwrap();
        flock_exclusive(&holder, Duration::from_millis(50), Duration::from_millis(5)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(holder);
        });
        let waiter = open_lock_file(&path, euid()).unwrap();
        flock_exclusive(&waiter, Duration::from_secs(2), Duration::from_millis(5)).unwrap();
        releaser.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinked_lock_file_is_refused() {
        let dir = scratch("link");
        let target = dir.join("target");
        std::fs::write(&target, b"").unwrap();
        set_mode(&target, 0o600);
        let link = dir.join("evdi.lock");
        symlink(&target, &link).unwrap();
        let err = open_lock_file(&link, euid()).unwrap_err();
        assert!(err.to_string().contains("safely"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_file_with_loose_permissions_is_refused() {
        let dir = scratch("mode");
        let path = dir.join("evdi.lock");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o644);
        let err = open_lock_file(&path, euid()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("mode 0600"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_file_owned_by_someone_else_is_refused() {
        let dir = scratch("owner");
        let path = dir.join("evdi.lock");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o600);
        // Ask for a different effective uid than the file's owner: the same check that
        // rejects a file pre-created by another user in /tmp.
        let err = open_lock_file(&path, euid() + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("owned by"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hard_linked_lock_file_is_refused() {
        let dir = scratch("hard");
        let path = dir.join("evdi.lock");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o600);
        std::fs::hard_link(&path, dir.join("other")).unwrap();
        assert!(open_lock_file(&path, euid()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_path_is_private_runtime_dir_or_tmp_and_world_writable_dirs_are_not_trusted() {
        let text = lock_file_path(euid()).to_string_lossy().into_owned();
        assert!(
            text.ends_with("linux-quest-evdi.lock")
                || text == format!("/tmp/linux-quest-evdi-{}.lock", euid()),
            "{text}"
        );
        let dir = scratch("perm");
        set_mode(&dir, 0o777);
        assert!(!private_dir(&dir, euid()));
        set_mode(&dir, 0o700);
        assert!(private_dir(&dir, euid()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
