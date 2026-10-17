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
//! - **GRABPIX writes the whole dirty rectangle.** The model validates `buf_width/height`
//!   against the current scanout mode (as the module does), then writes every row of the
//!   reported rectangle (`y * stride + x1*4 .. y * stride + x2*4`) into the caller's buffer.
//!   The kernel cannot know a user buffer's length, so the model learns it from
//!   [`CardHandle::register_buffer`] (the painter declares its real allocation before each
//!   grab); a write that would leave that allocation is `EFAULT` (counted in
//!   [`MockKernel::buffer_faults`], nothing is written), an unregistered buffer is `EFAULT`,
//!   and `stride < width*4` is `EINVAL` (a model rule, not quoted from upstream).
//!   Limits of this: the registered length is self-reported by the code under test, and the
//!   model rejects before writing while a real kernel would fault part-way.
//! - **DRM master.** A freshly opened card fd is master if the card has no master (like
//!   `drm_master_open`); `DRM_IOCTL_DROP_MASTER` by the master releases it (`EINVAL` for a
//!   non-master); `DRM_IOCTL_AUTH_MAGIC(0)` is `EINVAL` for the master and `EACCES` otherwise
//!   (what libevdi's `drm_is_master` relies on); closing the master fd releases master. The
//!   compositor model cannot commit while any fd is master ([`CommitOutcome::MasterHeld`]).
//!   Knobs make the drop fail or silently do nothing.
//! - **Mode changes.** [`MockKernel::compositor_modeset`] sends `MODE_CHANGED` and commits at
//!   the new mode in one step (events land in the same read); GRABPIX with the old size is
//!   then `EINVAL` and counted in [`MockKernel::stale_grabs`].
//! - **Selection lock.** [`KernelOps::lock_selection`] is an exclusive in-process lock (a stand
//!   in for the cross-process `flock`; the real file handling is tested separately).
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
    DRM_EVDI_EVENT_UPDATE_READY, DRM_IOCTL_AUTH_MAGIC, DRM_IOCTL_DROP_MASTER,
    DRM_IOCTL_EVDI_CONNECT, DRM_IOCTL_EVDI_GRABPIX, DRM_IOCTL_EVDI_REQUEST_UPDATE,
    DRM_IOCTL_VERSION, GRABPIX_MODE_DIRTY,
};
use super::{CardHandle, KernelOps, SelectionLock, EVDI_ROOT};

const MAX_DEVICES: usize = 16;
const MAX_EDID: usize = 256 * 128;
const MAX_DIRTS: usize = 16;
/// Real-time bound on waiting for the selection lock.
const LOCK_WAIT_REAL: Duration = Duration::from_secs(10);

/// How `DRM_IOCTL_DROP_MASTER` behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterDrop {
    Works,
    /// Fails with `EPERM`.
    Fails,
    /// Returns success but the fd stays master.
    Ineffective,
}

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
    /// This card's connector status cannot be read.
    status_unreadable: bool,
    /// The `device` symlink of a USB card appears at this virtual time.
    usb_marker_at: u64,
    /// Fd id that is DRM master of this card.
    master: Option<u64>,
    /// Every wait/poll on the card fd fails with `EIO` (simulated fatal fd error).
    fatal_io: bool,
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
    all_status_unreadable: bool,
    master_drop: MasterDrop,
    /// `(major, minor, patchlevel)` the evdi nodes report in `DRM_IOCTL_VERSION`.
    module_version: (i32, i32, i32),
    /// On the next `add`, also create a USB card whose marker shows after this many ms.
    usb_on_add: Option<u64>,
    /// Real-time pause inside every status read, to widen selection races in tests.
    race_window_ms: u64,
    selection_locked: bool,
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
    lock_acquisitions: u32,
    stale_grabs: u32,
    buffer_faults: u32,
    last_grab_buffer_len: Option<usize>,
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
    /// Some fd is still DRM master of the card, so the compositor cannot take it.
    MasterHeld,
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
                    all_status_unreadable: false,
                    master_drop: MasterDrop::Works,
                    module_version: (1, 15, 1),
                    usb_on_add: None,
                    race_window_ms: 0,
                    selection_locked: false,
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
                    lock_acquisitions: 0,
                    stale_grabs: 0,
                    buffer_faults: 0,
                    last_grab_buffer_len: None,
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

    /// The connector `status` of every card that exists *now* cannot be read (cards created
    /// later are readable).
    pub fn with_unreadable_status(self) -> Self {
        for card in self.lock().cards.values_mut() {
            card.status_unreadable = true;
        }
        self
    }

    /// No connector `status` can be read, now or for cards created later.
    pub fn with_all_status_unreadable(self) -> Self {
        self.lock().all_status_unreadable = true;
        self
    }

    /// The evdi module reports this version in `DRM_IOCTL_VERSION`.
    pub fn with_module_version(self, major: i32, minor: i32) -> Self {
        self.lock().module_version = (major, minor, 0);
        self
    }

    /// `DRM_IOCTL_DROP_MASTER` fails with `EPERM`.
    pub fn with_master_drop_failing(self) -> Self {
        self.lock().master_drop = MasterDrop::Fails;
        self
    }

    /// `DRM_IOCTL_DROP_MASTER` reports success but the fd stays master.
    pub fn with_master_drop_ineffective(self) -> Self {
        self.lock().master_drop = MasterDrop::Ineffective;
        self
    }

    /// The next `add` also creates a USB-attached card (as if a dock was plugged in at the
    /// same moment) that is visible at once but whose `device` marker only appears `ms`
    /// virtual milliseconds later.
    pub fn with_usb_card_appearing_on_add(self, marker_delay_ms: u64) -> Self {
        self.lock().usb_on_add = Some(marker_delay_ms);
        self
    }

    /// Every connector status read pauses this many real milliseconds *after* reading, so two
    /// processes scanning at the same time both see the same state.
    pub fn with_status_read_delay(self, ms: u64) -> Self {
        self.lock().race_window_ms = ms;
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
        // The marker of an existing USB card is long visible.
        if connected_by_other {
            let fd = MockCardFd {
                shared: self.shared.clone(),
                minor,
                id,
                registered: Mutex::new(None),
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
                status_unreadable: false,
                usb_marker_at: now + sysfs,
                master: None,
                fatal_io: false,
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

    /// Whether some fd is DRM master of the card.
    pub fn has_master(&self, minor: u32) -> bool {
        self.lock()
            .cards
            .get(&minor)
            .is_some_and(|c| c.master.is_some())
    }

    /// Whether the client has armed `UPDATE_READY` (its last `REQUEST_UPDATE` returned 0 and no
    /// frame has been signalled since): a servicing thread in this state is idle in `poll`.
    pub fn update_armed(&self, minor: u32) -> bool {
        self.lock()
            .cards
            .get(&minor)
            .is_some_and(|c| c.painter.was_update_requested)
    }

    /// GRABPIX calls whose buffer size differed from the scanout mode at that moment.
    pub fn stale_grabs(&self) -> u32 {
        self.lock().stale_grabs
    }

    /// GRABPIX calls that would have written outside the registered allocation (or into an
    /// unregistered one).
    pub fn buffer_faults(&self) -> u32 {
        self.lock().buffer_faults
    }

    /// Length of the allocation registered for the most recent GRABPIX.
    pub fn last_grab_buffer_len(&self) -> Option<usize> {
        self.lock().last_grab_buffer_len
    }

    /// Times the selection lock was taken.
    pub fn lock_acquisitions(&self) -> u32 {
        self.lock().lock_acquisitions
    }

    /// Makes every later wait/read on the card's fd fail with `EIO`, like a dying fd.
    pub fn inject_fatal_io(&self, minor: u32) {
        if let Some(c) = self.lock().cards.get_mut(&minor) {
            c.fatal_io = true;
        }
        self.shared.cv.notify_all();
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
        let outcome = commit_locked(card, width, height);
        self.shared.cv.notify_all();
        outcome
    }

    /// Models the compositor switching the output to `width`x`height` while running: the module
    /// sends `MODE_CHANGED` and the modeset commit follows (and, if the client armed
    /// `update_ready`, `UPDATE_READY`), all queued under one lock so the events land in the
    /// same `read`. The mode need not be the EDID's preferred one (the model does not parse
    /// the full mode list). The following commits may use the new size.
    pub fn compositor_modeset(&self, minor: u32, width: u32, height: u32) -> CommitOutcome {
        let mut s = self.lock();
        let Some(card) = s.cards.get_mut(&minor) else {
            return CommitOutcome::NotConnected;
        };
        let outcome = modeset_locked(card, width, height);
        self.shared.cv.notify_all();
        outcome
    }

    /// Like [`compositor_modeset`](Self::compositor_modeset), but a commit at the *old* mode
    /// is queued first, so an `UPDATE_READY` precedes the `MODE_CHANGED` in the same read.
    pub fn compositor_commit_then_modeset(
        &self,
        minor: u32,
        width: u32,
        height: u32,
    ) -> CommitOutcome {
        let mut s = self.lock();
        let Some(card) = s.cards.get_mut(&minor) else {
            return CommitOutcome::NotConnected;
        };
        if let Some((w, h)) = card.painter.scanout {
            let first = commit_locked(card, w, h);
            if !matches!(first, CommitOutcome::Committed(_)) {
                self.shared.cv.notify_all();
                return first;
            }
        }
        let outcome = modeset_locked(card, width, height);
        self.shared.cv.notify_all();
        outcome
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

fn push_event(p: &mut Painter, ty: u32, payload: &[i32], len: u32) {
    let mut ev = Vec::new();
    ev.extend_from_slice(&ty.to_ne_bytes());
    ev.extend_from_slice(&len.to_ne_bytes());
    for v in payload {
        ev.extend_from_slice(&v.to_ne_bytes());
    }
    p.events.push_back(ev);
}

/// The module's commit path for one frame at `width`x`height`.
fn commit_locked(card: &mut Card, width: u32, height: u32) -> CommitOutcome {
    if card.master.is_some() {
        return CommitOutcome::MasterHeld;
    }
    let p = &mut card.painter;
    if !p.is_connected {
        return CommitOutcome::NotConnected;
    }
    match edid_preferred_mode(&p.edid) {
        None => return CommitOutcome::NoModes,
        // A mode set earlier by a modeset is accepted as is.
        Some(m) if m != (width, height) && p.scanout != Some((width, height)) => {
            return CommitOutcome::ModeNotInEdid
        }
        Some(_) => {}
    }
    p.scanout = Some((width, height));
    if p.needs_full_modeset {
        p.needs_full_modeset = false;
        push_event(
            p,
            DRM_EVDI_EVENT_MODE_CHANGED,
            &[width as i32, height as i32, 60, 32, 0x3432_5258],
            28,
        );
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
        push_event(p, DRM_EVDI_EVENT_UPDATE_READY, &[], 8);
        p.was_update_requested = false;
    }
    CommitOutcome::Committed(p.commits)
}

fn modeset_locked(card: &mut Card, width: u32, height: u32) -> CommitOutcome {
    if card.master.is_some() {
        return CommitOutcome::MasterHeld;
    }
    let p = &mut card.painter;
    if !p.is_connected {
        return CommitOutcome::NotConnected;
    }
    p.scanout = Some((width, height));
    p.needs_full_modeset = false;
    push_event(
        p,
        DRM_EVDI_EVENT_MODE_CHANGED,
        &[width as i32, height as i32, 60, 32, 0x3432_5258],
        28,
    );
    commit_locked(card, width, height)
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
                return s.cards.values().any(|c| {
                    c.platform == name
                        && c.usb
                        && c.sysfs_visible(s.now_ms)
                        && s.now_ms >= c.usb_marker_at
                });
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
                    if s.all_status_unreadable {
                        return Err(errno(libc::EACCES));
                    }
                    if s.foreign.contains_key(&minor) {
                        return Ok("disconnected\n".into());
                    }
                    if let Some(c) = s.cards.get(&minor).filter(|c| c.sysfs_visible(s.now_ms)) {
                        if c.status_unreadable {
                            return Err(errno(libc::EACCES));
                        }
                        let text: String = if c.painter.is_connected {
                            "connected\n"
                        } else {
                            "disconnected\n"
                        }
                        .into();
                        // Widen the scan-to-connect race: the value read above stays
                        // stale while this reader pauses without holding the state lock.
                        let pause = s.race_window_ms;
                        drop(s);
                        if pause > 0 {
                            std::thread::sleep(Duration::from_millis(pause));
                        }
                        return Ok(text);
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
        if let Some(marker_delay) = s.usb_on_add.take() {
            let minor = Self::alloc_card(&mut s, true, delays, true);
            let now = s.now_ms;
            s.cards.get_mut(&minor).unwrap().usb_marker_at = now + marker_delay;
        }
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
                registered: Mutex::new(None),
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
        let card = s.cards.get_mut(&minor).unwrap();
        card.open_fds += 1;
        // drm_master_open: the first opener of a master-less node becomes master.
        if card.master.is_none() {
            card.master = Some(id);
        }
        Ok(Arc::new(MockCardFd {
            shared: self.shared.clone(),
            minor,
            id,
            registered: Mutex::new(None),
        }))
    }

    fn lock_selection(&self) -> io::Result<Box<dyn SelectionLock>> {
        let deadline = std::time::Instant::now() + LOCK_WAIT_REAL;
        let mut s = self.lock();
        while s.selection_locked {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "selection lock held too long",
                ));
            }
            s = self
                .shared
                .cv
                .wait_timeout(s, deadline - now)
                .expect("mock state poisoned")
                .0;
        }
        s.selection_locked = true;
        s.lock_acquisitions += 1;
        Ok(Box::new(MockSelectionLock {
            shared: self.shared.clone(),
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

struct MockSelectionLock {
    shared: Arc<Shared>,
}

impl SelectionLock for MockSelectionLock {}

impl Drop for MockSelectionLock {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.state.lock() {
            s.selection_locked = false;
        }
        self.shared.cv.notify_all();
    }
}

/// A modelled open fd. Dropping it is `close(2)`.
struct MockCardFd {
    shared: Arc<Shared>,
    minor: u32,
    id: u64,
    /// `(address, length)` of the allocation declared via `register_buffer`.
    registered: Mutex<Option<(usize, usize)>>,
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
            if card.master == Some(self.id) {
                card.master = None;
            }
            // evdi_painter_close: disconnect only if this fd issued the CONNECT.
            if card.painter.filp == Some(self.id) {
                card.painter.disconnect();
                card.fatal_io = false;
            }
        }
        self.shared.cv.notify_all();
    }
}

impl CardHandle for MockCardFd {
    fn register_buffer(&self, ptr: *mut u8, len: usize) {
        *self.registered.lock().expect("registered poisoned") = Some((ptr as usize, len));
    }

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
                    s.module_version
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
            DRM_IOCTL_DROP_MASTER => {
                let mode = s.master_drop;
                match s.cards.get_mut(&self.minor) {
                    None => Ok(0), // non-evdi GPU: master is not modelled
                    Some(card) => match mode {
                        MasterDrop::Fails => Err(errno(libc::EPERM)),
                        MasterDrop::Ineffective => Ok(0),
                        MasterDrop::Works if card.master == Some(self.id) => {
                            card.master = None;
                            Ok(0)
                        }
                        MasterDrop::Works => Err(errno(libc::EINVAL)),
                    },
                }
            }
            DRM_IOCTL_AUTH_MAGIC => {
                // Needs master; magic 0 is invalid. A non-master fd is refused first.
                let is_master = s
                    .cards
                    .get(&self.minor)
                    .is_some_and(|c| c.master == Some(self.id));
                Err(errno(if is_master {
                    libc::EINVAL
                } else {
                    libc::EACCES
                }))
            }
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
            if s.cards.get(&self.minor).is_some_and(|c| c.fatal_io) {
                return Err(errno(libc::EIO));
            }
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
        let registered = *self.registered.lock().expect("registered poisoned");
        s.last_grab_buffer_len = registered.map(|(_, len)| len);
        let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
        let Some((w, h)) = p.scanout else {
            return Err(errno(libc::EAGAIN));
        };
        p.num_dirts = 0;
        // After this point the vblank is released even on error (`err_fb:` path).
        let result = if cmd.buf_width as u32 != w || cmd.buf_height as u32 != h {
            s.stale_grabs += 1;
            Err(errno(libc::EINVAL))
        } else if cmd.buf_byte_stride < (w as i32) * 4 {
            Err(errno(libc::EINVAL))
        } else {
            // The dirty rectangle reported is the full frame. Write it row by row at the
            // caller's stride, after checking the whole extent against the allocation the
            // caller declared.
            let stride = cmd.buf_byte_stride as usize;
            let row_bytes = w as usize * 4;
            let extent = (h as usize - 1) * stride + row_bytes;
            let in_bounds = registered.is_some_and(|(base, len)| {
                let addr = cmd.buffer as usize;
                addr >= base && (addr - base).checked_add(extent).is_some_and(|e| e <= len)
            });
            if in_bounds {
                // SAFETY: `in_bounds` proved [buffer, buffer + extent) lies inside the live
                // allocation the caller registered for this call; `rects` holds at least
                // one entry (`num_rects >= 1` checked above).
                unsafe {
                    for y in 0..h as usize {
                        std::ptr::write_bytes(cmd.buffer.add(y * stride), 0xAB, row_bytes);
                    }
                    *cmd.rects = abi::DrmClipRect {
                        x1: 0,
                        y1: 0,
                        x2: w as u16,
                        y2: h as u16,
                    };
                }
                Ok(0)
            } else {
                s.buffer_faults += 1;
                Err(errno(libc::EFAULT))
            }
        };
        let p = &mut s.cards.get_mut(&self.minor).unwrap().painter;
        p.release_vblank();
        self.shared.cv.notify_all();
        result
    }
}
