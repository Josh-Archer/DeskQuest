//! In-memory model of the EVDI kernel rules, for tests without hardware.
//!
//! This is a *model*, written from the upstream sources listed in [`super`]. It is only as
//! right as that reading, and nothing here has been compared with a running module. What it
//! enforces, so that tests fail when the backend's assumptions are wrong:
//!
//! - **ioctl request numbers are exact.** Requests are matched against the constants computed
//!   in [`super::abi`]; anything else on an evdi node is `ENOTTY`. (The real kernel
//!   dispatches on the command number only and tolerates direction/size drift, so this is
//!   stricter than the kernel on purpose: it catches us encoding the wrong thing.) The
//!   argument length must equal the size encoded in the request.
//! - **CONNECT is only meaningful on an evdi fd.** A node bound to another driver counts a
//!   `foreign_private_ioctls` hit and fails `EINVAL`; tests assert the count stays zero.
//! - **EDID length rules** of `evdi_painter_connect`: `< 128` and `> 255*128+128` are
//!   `EINVAL`, a null pointer is `EFAULT`. The kernel does not check the checksum at connect
//!   time; like the real connector (`drm_add_edid_modes`) the model yields no modes for an
//!   invalid EDID, so the compositor model refuses to light it.
//! - **Disconnect-on-close.** The connection belongs to the fd that issued CONNECT; closing
//!   that fd (dropping the last `Arc` of the handle) disconnects. A second CONNECT from another
//!   fd silently replaces the first (counted in `double_connects`); `connected = 0` from a
//!   different fd is `EFAULT`.
//! - **Events only reach the connecting fd**, the vblank of a commit is held until
//!   `GRABPIX`/next commit/disconnect, and `UPDATE_READY` is only sent after
//!   `REQUEST_UPDATE` armed it. A compositor model ([`MockKernel::compositor_commit`]) uses
//!   this so a backend that does not service events is observed to stall.
//! - **`add` creates cards asynchronously** (sysfs entry, `/dev/dri` node and permissions
//!   appear after configurable virtual delays), fails `EACCES` for non-root, `EINVAL` beyond
//!   16 devices; `remove_all` kills every card (counted; the backend must never call it).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::abi::{
    self, read_struct, DrmEvdiConnect, DrmEvdiGrabpix, DrmVersion, DRM_EVDI_EVENT_MODE_CHANGED,
    DRM_EVDI_EVENT_UPDATE_READY, DRM_IOCTL_DROP_MASTER, DRM_IOCTL_EVDI_CONNECT,
    DRM_IOCTL_EVDI_GRABPIX, DRM_IOCTL_EVDI_REQUEST_UPDATE, DRM_IOCTL_VERSION, GRABPIX_MODE_DIRTY,
};
use super::{CardHandle, KernelOps, EVDI_ROOT};

const MAX_DEVICES: usize = 16;
const MAX_EDID: usize = 256 * 128;
const MAX_DIRTS: usize = 16;

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

#[derive(Debug, Default)]
struct Painter {
    /// Fd id that issued CONNECT (`painter->drm_filp`).
    filp: Option<u64>,
    is_connected: bool,
    edid: Vec<u8>,
    area_limit: u32,
    per_second_limit: u32,
    was_update_requested: bool,
    num_dirts: usize,
    needs_full_modeset: bool,
    scanout: Option<(u32, u32)>,
    held_vblank: bool,
    events: VecDeque<Vec<u8>>,
    commits: u64,
    flips_completed: u64,
}

impl Painter {
    fn release_vblank(&mut self) {
        if self.held_vblank {
            self.held_vblank = false;
            self.flips_completed += 1;
        }
    }

    fn disconnect(&mut self) {
        self.scanout = None;
        self.is_connected = false;
        self.events.clear();
        self.release_vblank();
        self.filp = None;
        self.was_update_requested = false;
        self.num_dirts = 0;
        self.edid.clear();
    }
}

#[derive(Debug)]
struct Card {
    minor: u32,
    platform: String,
    driver: String,
    usb: bool,
    sysfs_at: u64,
    node_at: u64,
    perm_at: u64,
    denied: bool,
    open_fds: usize,
    painter: Painter,
}

impl Card {
    fn sysfs_visible(&self, now: u64) -> bool {
        now >= self.sysfs_at
    }
    fn node_visible(&self, now: u64) -> bool {
        now >= self.node_at
    }
    fn accessible(&self, now: u64) -> bool {
        self.node_visible(now) && now >= self.perm_at && !self.denied
    }
}

#[derive(Debug)]
struct State {
    euid: u32,
    module_loaded: bool,
    add_root_only: bool,
    now_ms: u64,
    sysfs_delay: u64,
    node_delay: u64,
    perm_delay: u64,
    new_cards_denied: bool,
    status_unreadable: bool,
    /// Non-EVDI GPU cards by minor (e.g. `card0` = i915).
    foreign: BTreeMap<u32, String>,
    cards: BTreeMap<u32, Card>,
    next_fd: u64,
    // Observations.
    add_calls: u32,
    remove_all_calls: u32,
    foreign_private_ioctls: u32,
    rejected_requests: Vec<u32>,
    double_connects: u32,
    opened_nodes: Vec<u32>,
    grabs: Vec<(i32, i32, i32, i32)>,
    open_calls: u32,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

/// Cloneable handle to one modelled machine. Clones share state.
#[derive(Clone)]
pub struct MockKernel {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MockKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockKernel")
    }
}

/// What the modelled compositor saw when it tried to present a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Frame committed; the value is the commit number to pass to
    /// [`MockKernel::wait_flip`].
    Committed(u64),
    /// The connector is disconnected.
    NotConnected,
    /// The connected EDID yields no usable mode.
    NoModes,
    /// The requested mode is not the EDID's preferred mode.
    ModeNotInEdid,
}

impl Default for MockKernel {
    fn default() -> Self {
        Self::new()
    }
}

impl MockKernel {
    /// A machine with the evdi module loaded, no EVDI cards, one i915 GPU as `card0`, and a
    /// root user (so `add` is writable).
    pub fn new() -> Self {
        let mut foreign = BTreeMap::new();
        foreign.insert(0, "i915".to_string());
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    euid: 0,
                    module_loaded: true,
                    add_root_only: true,
                    now_ms: 0,
                    sysfs_delay: 0,
                    node_delay: 0,
                    perm_delay: 0,
                    new_cards_denied: false,
                    status_unreadable: false,
                    foreign,
                    cards: BTreeMap::new(),
                    next_fd: 1,
                    add_calls: 0,
                    remove_all_calls: 0,
                    foreign_private_ioctls: 0,
                    rejected_requests: Vec::new(),
                    double_connects: 0,
                    opened_nodes: Vec::new(),
                    grabs: Vec::new(),
                    open_calls: 0,
                }),
                cv: Condvar::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().expect("mock state poisoned")
    }

    // ---- builders ----------------------------------------------------------------------

    pub fn without_module(self) -> Self {
        self.lock().module_loaded = false;
        self
    }

    pub fn with_euid(self, uid: u32) -> Self {
        self.lock().euid = uid;
        self
    }

    /// Cards created by `add` appear after these virtual delays (ms), counted from the write:
    /// sysfs entry, `/dev/dri` node, and (after the node) permissions applied by udev.
    pub fn with_add_delays(self, sysfs: u64, node: u64, perm: u64) -> Self {
        {
            let mut s = self.lock();
            s.sysfs_delay = sysfs;
            s.node_delay = node;
            s.perm_delay = perm;
        }
        self
    }

    /// Cards created by `add` are never openable by this user (missing group).
    pub fn with_new_cards_denied(self) -> Self {
        self.lock().new_cards_denied = true;
        self
    }

    /// Connector `status` files cannot be read.
    pub fn with_unreadable_status(self) -> Self {
        self.lock().status_unreadable = true;
        self
    }

    /// An idle evdi card that already exists and is openable (e.g. `initial_device_count`).
    pub fn with_idle_card(self) -> Self {
        self.new_card(false, false);
        self
    }

    /// An existing idle card whose node this user cannot open.
    pub fn with_idle_card_denied(self) -> Self {
        let minor = self.new_card(false, false);
        self.lock().cards.get_mut(&minor).unwrap().denied = true;
        self
    }

    /// An existing card attached to a USB dock.
    pub fn with_usb_card(self) -> Self {
        self.new_card(true, false);
        self
    }

    /// An existing card already connected by another application (an fd this model keeps
    /// open for the lifetime of the machine).
    pub fn with_card_in_use_by_other_app(self) -> Self {
        self.new_card(false, true);
        self
    }

    /// Makes the node of an evdi-looking card belong to another driver (a sysfs/devnode race).
    pub fn with_card_driver(self, minor: u32, driver: &str) -> Self {
        self.lock().cards.get_mut(&minor).unwrap().driver = driver.to_string();
        self
    }

    /// Fills the module to its 16-device limit with busy cards.
    pub fn with_all_slots_busy(self) -> Self {
        for _ in 0..MAX_DEVICES {
            self.new_card(false, true);
        }
        self
    }

    fn new_card(&self, usb: bool, connected_by_other: bool) -> u32 {
        let (minor, id) = {
            let mut s = self.lock();
            let minor = Self::alloc_card(&mut s, usb, (0, 0, 0), false);
            let id = s.next_fd;
            s.next_fd += 1;
            (minor, id)
        };
        if connected_by_other {
            let fd = MockCardFd {
                shared: self.shared.clone(),
                minor,
                id,
            };
            {
                let mut s = self.lock();
                s.cards.get_mut(&minor).unwrap().open_fds += 1;
            }
            let mut connect = Vec::new();
            connect.extend_from_slice(&good_edid_bytes());
            let mut args = DrmEvdiConnect {
                connected: 1,
                dev_index: minor as i32,
                edid: connect.as_ptr(),
                edid_length: connect.len() as u32,
                pixel_area_limit: 0,
                pixel_per_second_limit: 0,
                _pad: 0,
            };
            // SAFETY: DrmEvdiConnect has explicit padding only.
            let bytes = unsafe { abi::as_bytes_mut(&mut args) };
            fd.ioctl(DRM_IOCTL_EVDI_CONNECT, bytes)
                .expect("modelled foreign client connects");
            // Leak the fd for the lifetime of the machine: the "other app" never exits.
            std::mem::forget(fd);
        }
        minor
    }

    fn alloc_card(s: &mut State, usb: bool, delays: (u64, u64, u64), from_add: bool) -> u32 {
        let (sysfs, node, perm) = delays;
        let mut minor = 0;
        while s.cards.contains_key(&minor) || s.foreign.contains_key(&minor) {
            minor += 1;
        }
        let mut k = 0;
        while s.cards.values().any(|c| c.platform == format!("evdi.{k}")) {
            k += 1;
        }
        let now = s.now_ms;
        let denied = from_add && s.new_cards_denied;
        s.cards.insert(
            minor,
            Card {
                minor,
                platform: format!("evdi.{k}"),
                driver: "evdi".into(),
                usb,
                sysfs_at: now + sysfs,
                node_at: now + sysfs + node,
                perm_at: now + sysfs + node + perm,
                denied,
                open_fds: 0,
                painter: Painter {
                    needs_full_modeset: true,
                    ..Painter::default()
                },
            },
        );
        minor
    }

    // ---- observations ------------------------------------------------------------------

    /// Minors of all evdi cards currently existing.
    pub fn card_minors(&self) -> Vec<u32> {
        self.lock().cards.keys().copied().collect()
    }

    pub fn add_calls(&self) -> u32 {
        self.lock().add_calls
    }

    /// Times `remove_all` was written. The backend must never do that.
    pub fn remove_all_calls(&self) -> u32 {
        self.lock().remove_all_calls
    }

    /// Driver-private ioctls received by non-evdi nodes. Must stay zero.
    pub fn foreign_private_ioctls(&self) -> u32 {
        self.lock().foreign_private_ioctls
    }

    /// Requests on evdi nodes that matched no real ABI constant (answered `ENOTTY`).
    pub fn rejected_requests(&self) -> Vec<u32> {
        self.lock().rejected_requests.clone()
    }

    pub fn double_connects(&self) -> u32 {
        self.lock().double_connects
    }

    /// Minors passed to `open_card`, in order.
    pub fn opened_nodes(&self) -> Vec<u32> {
        self.lock().opened_nodes.clone()
    }

    /// Whether the card's painter is connected (what `/sys/class/drm/.../status` reports).
    pub fn is_connected(&self, minor: u32) -> bool {
        self.lock()
            .cards
            .get(&minor)
            .is_some_and(|c| c.painter.is_connected)
    }

    /// The EDID the card is currently connected with.
    pub fn connected_edid(&self, minor: u32) -> Option<Vec<u8>> {
        let s = self.lock();
        let c = s.cards.get(&minor)?;
        c.painter.is_connected.then(|| c.painter.edid.clone())
    }

    /// `(pixel_area_limit, pixel_per_second_limit)` of the last CONNECT.
    pub fn pixel_limits(&self, minor: u32) -> Option<(u32, u32)> {
        let s = self.lock();
        let p = &s.cards.get(&minor)?.painter;
        Some((p.area_limit, p.per_second_limit))
    }

    /// Open fds on the card node.
    pub fn open_fds(&self, minor: u32) -> usize {
        self.lock().cards.get(&minor).map_or(0, |c| c.open_fds)
    }

    /// `(buf_width, buf_height, buf_byte_stride, num_rects)` of every GRABPIX received.
    pub fn grab_requests(&self) -> Vec<(i32, i32, i32, i32)> {
        self.lock().grabs.clone()
    }

    pub fn open_calls(&self) -> u32 {
        self.lock().open_calls
    }

    /// Preferred mode `(width, height)` parsed from the connected EDID, as the connector's
    /// `get_modes` would offer it; `None` if the EDID is invalid.
    pub fn edid_mode(&self, minor: u32) -> Option<(u32, u32)> {
        let s = self.lock();
        edid_preferred_mode(&s.cards.get(&minor)?.painter.edid)
    }

    // ---- environment -------------------------------------------------------------------

    /// Simulates somebody running `remove_all`: every card disappears and every painter is
    /// disconnected.
    pub fn remove_all(&self) {
        let mut s = self.lock();
        remove_all(&mut s);
        self.shared.cv.notify_all();
    }

    // ---- compositor model --------------------------------------------------------------

    /// Models the compositor presenting one frame of `width`x`height` on `minor`, following
    /// the module's commit path: scanout set, `MODE_CHANGED` after (re)connect, dirty rect,
    /// previous vblank released, new vblank held while pixels are pending, `UPDATE_READY` when
    /// armed.
    pub fn compositor_commit(&self, minor: u32, width: u32, height: u32) -> CommitOutcome {
        let mut s = self.lock();
        let Some(card) = s.cards.get_mut(&minor) else {
            return CommitOutcome::NotConnected;
        };
        let p = &mut card.painter;
        if !p.is_connected {
            return CommitOutcome::NotConnected;
        }
        match edid_preferred_mode(&p.edid) {
            None => return CommitOutcome::NoModes,
            Some(m) if m != (width, height) => return CommitOutcome::ModeNotInEdid,
            Some(_) => {}
        }
        p.scanout = Some((width, height));
        if p.needs_full_modeset {
            p.needs_full_modeset = false;
            let mut ev = Vec::new();
            ev.extend_from_slice(&DRM_EVDI_EVENT_MODE_CHANGED.to_ne_bytes());
            ev.extend_from_slice(&28u32.to_ne_bytes());
            for v in [width as i32, height as i32, 60, 32] {
                ev.extend_from_slice(&v.to_ne_bytes());
            }
            ev.extend_from_slice(&0x3432_5258u32.to_ne_bytes());
            p.events.push_back(ev);
        }
        p.num_dirts = (p.num_dirts + 1).min(MAX_DIRTS);
        p.release_vblank();
        p.commits += 1;
        if p.num_dirts > 0 && p.is_connected {
            p.held_vblank = true;
        } else {
            p.flips_completed += 1;
        }
        if p.was_update_requested && p.num_dirts > 0 {
            let mut ev = Vec::new();
            ev.extend_from_slice(&DRM_EVDI_EVENT_UPDATE_READY.to_ne_bytes());
            ev.extend_from_slice(&8u32.to_ne_bytes());
            p.events.push_back(ev);
            p.was_update_requested = false;
        }
        let n = p.commits;
        self.shared.cv.notify_all();
        CommitOutcome::Committed(n)
    }

    /// Waits (real time) until commit number `commit` has completed its page flip.
    pub fn wait_flip(&self, minor: u32, commit: u64, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            if s.cards
                .get(&minor)
                .is_some_and(|c| c.painter.flips_completed >= commit)
            {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            s = self
                .shared
                .cv
                .wait_timeout(s, deadline - now)
                .expect("mock state poisoned")
                .0;
        }
    }

    /// Presents a frame and waits for its flip, like a compositor that waits for flip
    /// completion before the next commit. `false` means it stalled.
    pub fn commit_and_wait(&self, minor: u32, width: u32, height: u32, timeout: Duration) -> bool {
        match self.compositor_commit(minor, width, height) {
            CommitOutcome::Committed(n) => self.wait_flip(minor, n, timeout),
            _ => false,
        }
    }
}

fn remove_all(s: &mut State) {
    for card in s.cards.values_mut() {
        card.painter.disconnect();
    }
    s.cards.clear();
}

fn good_edid_bytes() -> Vec<u8> {
    crate::edid::EdidGenerator::generate(1920, 1080, 60, "peer", 9).expect("edid")
}

/// Parses the first detailed timing descriptor (bytes 54..72) of a valid base block.
fn edid_preferred_mode(edid: &[u8]) -> Option<(u32, u32)> {
    if edid.len() < 128 || edid[..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] {
        return None;
    }
    if edid
        .chunks(128)
        .any(|b| b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) != 0)
    {
        return None;
    }
    let d = &edid[54..72];
    let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
    if pixel_clock == 0 {
        return None;
    }
    let h = d[2] as u32 | (((d[4] >> 4) as u32) << 8);
    let v = d[5] as u32 | (((d[7] >> 4) as u32) << 8);
    Some((h, v))
}

// ---------------------------------------------------------------------------------------
// KernelOps
// ---------------------------------------------------------------------------------------

fn evdi_minor_from_node(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    if path.parent()? != Path::new("/dev/dri") {
        return None;
    }
    super::parse_card_index(name)
}

impl KernelOps for MockKernel {
    fn exists(&self, path: &Path) -> bool {
        let s = self.lock();
        let p = path.to_string_lossy();
        if p == EVDI_ROOT || p == format!("{EVDI_ROOT}/add") {
            return s.module_loaded;
        }
        if p == "/dev/dri" {
            return true;
        }
        if let Some(minor) = evdi_minor_from_node(path) {
            return s.foreign.contains_key(&minor)
                || s.cards
                    .get(&minor)
                    .is_some_and(|c| c.node_visible(s.now_ms));
        }
        if let Some(rest) = p.strip_prefix("/sys/bus/platform/devices/") {
            if let Some(name) = rest.strip_suffix("/device") {
                return s
                    .cards
                    .values()
                    .any(|c| c.platform == name && c.usb && c.sysfs_visible(s.now_ms));
            }
        }
        false
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let s = self.lock();
        let p = path.to_string_lossy();
        let visible = |c: &&Card| c.sysfs_visible(s.now_ms);
        if p == "/sys/bus/platform/devices" {
            let mut v = vec!["serial8250".to_string(), "alarmtimer".to_string()];
            if s.module_loaded {
                v.extend(s.cards.values().filter(visible).map(|c| c.platform.clone()));
            }
            return Ok(v);
        }
        if let Some(rest) = p.strip_prefix("/sys/bus/platform/devices/") {
            if let Some(name) = rest.strip_suffix("/drm") {
                return s
                    .cards
                    .values()
                    .filter(visible)
                    .find(|c| c.platform == name)
                    .map(|c| vec![format!("card{}", c.minor)])
                    .ok_or_else(|| errno(libc::ENOENT));
            }
        }
        if p == "/sys/class/drm" {
            let mut v = Vec::new();
            for m in s.foreign.keys() {
                v.push(format!("card{m}"));
                v.push(format!("card{m}-HDMI-A-1"));
            }
            for c in s.cards.values().filter(visible) {
                v.push(format!("card{}", c.minor));
                v.push(format!("card{}-DVI-I-1", c.minor));
            }
            return Ok(v);
        }
        if p == "/dev/dri" {
            let mut v: Vec<String> = s.foreign.keys().map(|m| format!("card{m}")).collect();
            v.extend(
                s.cards
                    .values()
                    .filter(|c| c.node_visible(s.now_ms))
                    .map(|c| format!("card{}", c.minor)),
            );
            return Ok(v);
        }
        Err(errno(libc::ENOENT))
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let s = self.lock();
        let p = path.to_string_lossy();
        if let Some(rest) = p.strip_prefix("/sys/class/drm/card") {
            if let Some((minor, tail)) = rest.split_once('-') {
                if let (Ok(minor), Some(_)) = (minor.parse::<u32>(), tail.strip_suffix("/status")) {
                    if s.status_unreadable {
                        return Err(errno(libc::EACCES));
                    }
                    if s.foreign.contains_key(&minor) {
                        return Ok("disconnected\n".into());
                    }
                    if let Some(c) = s.cards.get(&minor).filter(|c| c.sysfs_visible(s.now_ms)) {
                        return Ok(if c.painter.is_connected {
                            "connected\n"
                        } else {
                            "disconnected\n"
                        }
                        .into());
                    }
                }
            }
        }
        Err(errno(libc::ENOENT))
    }

    fn is_writable(&self, path: &Path) -> bool {
        let s = self.lock();
        path.to_string_lossy() == format!("{EVDI_ROOT}/add")
            && s.module_loaded
            && (s.euid == 0 || !s.add_root_only)
    }

    fn can_read_write(&self, path: &Path) -> bool {
        let s = self.lock();
        match evdi_minor_from_node(path) {
            Some(m) if s.foreign.contains_key(&m) => true,
            Some(m) => s.cards.get(&m).is_some_and(|c| c.accessible(s.now_ms)),
            None => false,
        }
    }

    fn write_attr(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut s = self.lock();
        let p = path.to_string_lossy();
        if !s.module_loaded {
            return Err(errno(libc::ENOENT));
        }
        if p == format!("{EVDI_ROOT}/remove_all") {
            if s.euid != 0 {
                return Err(errno(libc::EACCES));
            }
            s.remove_all_calls += 1;
            remove_all(&mut s);
            self.shared.cv.notify_all();
            return Ok(());
        }
        if p != format!("{EVDI_ROOT}/add") {
            return Err(errno(libc::ENOENT));
        }
        if s.euid != 0 && s.add_root_only {
            return Err(errno(libc::EACCES));
        }
        s.add_calls += 1;
        let text = std::str::from_utf8(data).map_err(|_| errno(libc::EINVAL))?;
        let count: usize = text.trim().parse().map_err(|_| errno(libc::EINVAL))?;
        if count > MAX_DEVICES.saturating_sub(s.cards.len()) {
            return Err(errno(libc::EINVAL));
        }
        let delays = (s.sysfs_delay, s.node_delay, s.perm_delay);
        for _ in 0..count {
            Self::alloc_card(&mut s, false, delays, true);
        }
        Ok(())
    }

    fn open_card(&self, path: &Path) -> io::Result<Arc<dyn CardHandle>> {
        let mut s = self.lock();
        s.open_calls += 1;
        let minor = evdi_minor_from_node(path).ok_or_else(|| errno(libc::ENOENT))?;
        let now = s.now_ms;
        if s.foreign.contains_key(&minor) {
            s.opened_nodes.push(minor);
            let id = s.next_fd;
            s.next_fd += 1;
            return Ok(Arc::new(MockCardFd {
                shared: self.shared.clone(),
                minor,
                id,
            }));
        }
        let card = s.cards.get(&minor).ok_or_else(|| errno(libc::ENOENT))?;
        if !card.node_visible(now) {
            return Err(errno(libc::ENOENT));
        }
        if !card.accessible(now) {
            return Err(errno(libc::EACCES));
        }
        s.opened_nodes.push(minor);
        let id = s.next_fd;
        s.next_fd += 1;
        s.cards.get_mut(&minor).unwrap().open_fds += 1;
        Ok(Arc::new(MockCardFd {
            shared: self.shared.clone(),
            minor,
            id,
        }))
    }

    fn euid(&self) -> u32 {
        self.lock().euid
    }

    fn sleep(&self, duration: Duration) {
        let mut s = self.lock();
        s.now_ms += duration.as_millis() as u64;
    }
}

// ---------------------------------------------------------------------------------------
// Card fd
// ---------------------------------------------------------------------------------------

/// A modelled open fd. Dropping it is `close(2)`.
struct MockCardFd {
    shared: Arc<Shared>,
    minor: u32,
    id: u64,
}

impl MockCardFd {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().expect("mock state poisoned")
    }
}

impl Drop for MockCardFd {
    fn drop(&mut self) {
        let mut s = self.lock();
        if let Some(card) = s.cards.get_mut(&self.minor) {
            card.open_fds = card.open_fds.saturating_sub(1);
            // evdi_painter_close: disconnect only if this fd issued the CONNECT.
            if card.painter.filp == Some(self.id) {
                card.painter.disconnect();
            }
        }
        self.shared.cv.notify_all();
    }
}

impl CardHandle for MockCardFd {
    fn ioctl(&self, request: u32, arg: &mut [u8]) -> io::Result<i32> {
        if abi::ioc_size(request) != arg.len() {
            return Err(errno(libc::EINVAL));
        }
        let mut s = self.lock();
        let driver = match s.foreign.get(&self.minor) {
            Some(d) => d.clone(),
            None => s
                .cards
                .get(&self.minor)
                .map(|c| c.driver.clone())
                .ok_or_else(|| errno(libc::ENODEV))?,
        };

        match request {
            DRM_IOCTL_VERSION => {
                // SAFETY: arg is exactly size_of::<DrmVersion>() bytes (checked above) written
                // by the caller from a DrmVersion; the string pointers come from the caller's
                // live buffers.
                let mut v: DrmVersion = unsafe { read_struct(arg) };
                let (major, minor, patch) = if driver == "evdi" {
                    (1, 15, 1)
                } else {
                    (1, 6, 0)
                };
                v.version_major = major;
                v.version_minor = minor;
                v.version_patchlevel = patch;
                let name = driver.as_bytes();
                // SAFETY: caller supplied name_len writable bytes at name.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        name.as_ptr(),
                        v.name.cast::<u8>(),
                        name.len().min(v.name_len),
                    );
                }
                v.name_len = name.len();
                v.date_len = 0;
                v.desc_len = 0;
                // SAFETY: DrmVersion has no implicit padding; copying its bytes back.
                let out = unsafe { abi::as_bytes_mut(&mut v) };
                arg.copy_from_slice(out);
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => Ok(0),
            _ if abi::ioc_nr(request) >= abi::DRM_COMMAND_BASE && driver != "evdi" => {
                s.foreign_private_ioctls += 1;
                Err(errno(libc::EINVAL))
            }
            DRM_IOCTL_EVDI_CONNECT => self.connect(&mut s, arg),
            DRM_IOCTL_EVDI_REQUEST_UPDATE => {
                let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
                if p.was_update_requested {
                    Ok(0)
                } else if p.num_dirts > 0 {
                    Ok(1)
                } else {
                    p.was_update_requested = true;
                    Ok(0)
                }
            }
            DRM_IOCTL_EVDI_GRABPIX => {
                // SAFETY: arg has the exact size of DrmEvdiGrabpix.
                let cmd: DrmEvdiGrabpix = unsafe { read_struct(arg) };
                self.grab(&mut s, cmd)
            }
            other => {
                s.rejected_requests.push(other);
                Err(errno(libc::ENOTTY))
            }
        }
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut s = self.lock();
        let card = s
            .cards
            .get_mut(&self.minor)
            .ok_or_else(|| errno(libc::ENODEV))?;
        if card.painter.filp != Some(self.id) {
            return Err(errno(libc::EAGAIN));
        }
        let mut n = 0;
        while let Some(ev) = card.painter.events.front() {
            if ev.len() > buf.len() - n {
                if n == 0 {
                    return Err(errno(libc::EINVAL));
                }
                break;
            }
            buf[n..n + ev.len()].copy_from_slice(ev);
            n += ev.len();
            card.painter.events.pop_front();
        }
        if n == 0 {
            Err(errno(libc::EAGAIN))
        } else {
            Ok(n)
        }
    }

    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            let ready = s
                .cards
                .get(&self.minor)
                .is_some_and(|c| c.painter.filp == Some(self.id) && !c.painter.events.is_empty());
            if ready {
                return Ok(true);
            }
            if !s.cards.contains_key(&self.minor) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "card removed"));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            s = self
                .shared
                .cv
                .wait_timeout(s, deadline - now)
                .expect("mock state poisoned")
                .0;
        }
    }
}

impl MockCardFd {
    fn connect(&self, s: &mut State, arg: &mut [u8]) -> io::Result<i32> {
        // SAFETY: arg has the exact size of DrmEvdiConnect.
        let cmd: DrmEvdiConnect = unsafe { read_struct(arg) };
        let id = self.id;
        if cmd.connected != 0 {
            let len = cmd.edid_length as usize;
            if !(128..=MAX_EDID).contains(&len) {
                return Err(errno(libc::EINVAL));
            }
            if cmd.edid.is_null() {
                return Err(errno(libc::EFAULT));
            }
            // SAFETY: the caller promises `edid` points at `edid_length` readable bytes.
            let edid = unsafe { std::slice::from_raw_parts(cmd.edid, len) }.to_vec();
            let double = s.cards[&self.minor].painter.filp.is_some();
            if double {
                s.double_connects += 1;
            }
            let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
            p.filp = Some(id);
            p.edid = edid;
            p.area_limit = cmd.pixel_area_limit;
            p.per_second_limit = cmd.pixel_per_second_limit;
            p.is_connected = true;
            p.needs_full_modeset = true;
            self.shared.cv.notify_all();
            Ok(0)
        } else {
            let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
            if p.filp != Some(id) {
                return Err(errno(libc::EFAULT));
            }
            p.disconnect();
            self.shared.cv.notify_all();
            Ok(0)
        }
    }

    fn grab(&self, s: &mut State, cmd: DrmEvdiGrabpix) -> io::Result<i32> {
        if cmd.mode != GRABPIX_MODE_DIRTY {
            return Err(errno(libc::EINVAL));
        }
        if cmd.num_rects < 1 {
            return Err(errno(libc::EINVAL));
        }
        s.grabs.push((
            cmd.buf_width,
            cmd.buf_height,
            cmd.buf_byte_stride,
            cmd.num_rects,
        ));
        let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
        let Some((w, h)) = p.scanout else {
            return Err(errno(libc::EAGAIN));
        };
        p.num_dirts = 0;
        // After this point the vblank is released even on error (`err_fb:` path).
        let result = if cmd.buf_width as u32 != w || cmd.buf_height as u32 != h {
            Err(errno(libc::EINVAL))
        } else {
            // Report one full-frame dirty rect and paint the first pixel; the module cannot
            // know the buffer length, and neither can this model.
            // SAFETY: the caller supplied a buffer of at least one pixel and one rect.
            unsafe {
                *cmd.rects = abi::DrmClipRect {
                    x1: 0,
                    y1: 0,
                    x2: w as u16,
                    y2: h as u16,
                };
                std::ptr::write_bytes(cmd.buffer, 0xAB, 4);
            }
            Ok(0)
        };
        p.release_vblank();
        self.shared.cv.notify_all();
        result
    }
}
