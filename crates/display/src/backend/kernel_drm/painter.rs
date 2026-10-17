//! Minimum event servicing an EVDI "painter" client must do.
//!
//! # Why servicing is required (read from `module/evdi_painter.c` at the pinned commit)
//!
//! After `DRM_IOCTL_EVDI_CONNECT` the connecting process is the *painter*: the module hands it
//! every frame the compositor scans out. The compositor's atomic commit does not finish on
//! its own:
//!
//! - `evdi_painter_set_vblank()` holds the commit's vblank (page-flip-complete) event in
//!   `painter->vblank` whenever dirty rectangles are pending and the painter is connected. It
//!   is only released by (a) `DRM_IOCTL_EVDI_GRABPIX` (`evdi_send_vblank(crtc, vblank)` on the
//!   `err_fb:` path, which runs for success and for most errors), (b) the *next* commit, or
//!   (c) disconnect.
//! - Compositors wait for flip completion before issuing the next commit, so a painter that
//!   never grabs leaves the first frame's flip pending forever: the output looks connected
//!   but never presents (Mutter/Sway stall that CRTC, or time out and disable it).
//! - `EVDI_REQUEST_UPDATE` arms `update_ready` events: the module only sends
//!   `DRM_EVDI_EVENT_UPDATE_READY` when `was_update_requested` is set, and
//!   `DRM_IOCTL_EVDI_REQUEST_UPDATE` returns `1` instead of arming if dirty rectangles are
//!   already queued ("grab immediately").
//! - `GRABPIX` requires a destination buffer whose `buf_width`/`buf_height` equal the
//!   scanout framebuffer, so the buffer is sized from `DRM_EVDI_EVENT_MODE_CHANGED`.
//!
//! This mirrors libevdi's client contract (`evdi_request_update`, `evdi_handle_events`,
//! `evdi_grab_pixels` in `library/evdi_lib.c`). The servicing loop here:
//!
//! 1. `request_update`; if it returns 1 grab now, else wait for `UPDATE_READY`.
//! 2. On `MODE_CHANGED`: (re)allocate a `width * height * 4` byte buffer (the module copies
//!    4 bytes per pixel; stride is `width * 4`). Events arrive in batches (one `read`); within
//!    a batch the *last* `MODE_CHANGED` is applied before any other event is handled, so an
//!    `UPDATE_READY` that sits in front of a `MODE_CHANGED` in the same read is not answered
//!    with a grab sized for the old mode.
//! 3. On `UPDATE_READY`: `GRABPIX` with `EVDI_GRABPIX_MODE_DIRTY` into that buffer, discard
//!    the pixels, `request_update` again.
//! 4. Ignore `DPMS`, `CRTC_STATE`, cursor and DDC/CI events. Cursor events are only sent
//!    after `EVDI_ENABLE_CURSOR_EVENTS` (never called here); without them the module blends
//!    the cursor into the grabbed pixels itself. The events are skipped by length.
//!
//! The pixels are discarded: this backend only makes the compositor treat the output as a
//! live monitor. Capturing the monitor's contents is the capture crate's job (PipeWire /
//! compositor screencast), exactly as with a physical display.
//!
//! Cost: every presented frame costs a kernel copy of the dirty region into a buffer that is
//! thrown away. That is the price of EVDI's design (there is no "don't care" mode in the
//! header), not an implementation choice.
//!
//! Nothing here has run against a real module. The loop is exercised against the in-memory
//! model in [`super::mock`], which implements the behaviour quoted above.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, error, warn};

use super::abi::{
    as_bytes_mut, parse_events, DrmClipRect, DrmEvdiGrabpix, DrmEvdiRequestUpdate, EvdiEvent,
    DRM_IOCTL_EVDI_GRABPIX, DRM_IOCTL_EVDI_REQUEST_UPDATE, GRABPIX_MODE_DIRTY,
};
use super::CardHandle;

/// How long the thread blocks in `poll(2)` before re-checking the stop flag.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Delay before retrying after a failed `request_update`/`grabpix`.
const RETRY_DELAY: Duration = Duration::from_millis(100);
/// `MAX_DIRTS` in the module: the most rectangles it ever reports.
const MAX_RECTS: usize = 16;
/// Largest width or height accepted from a `MODE_CHANGED` event.
pub const MAX_DIMENSION: i32 = 8192;
/// Largest buffer the painter will allocate for a mode (256 MiB, i.e. 8192x8192 at 4 bytes per
/// pixel). A mode that needs more is refused with an error log instead of allocating.
pub const MAX_BUFFER_BYTES: u64 = 256 * 1024 * 1024;
/// How long [`PainterService::stop`] waits for the thread before giving up and detaching it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const READ_BUF: usize = 4096;

/// Counters exposed for tests and diagnostics.
#[derive(Debug, Default)]
pub struct PainterStats {
    pub grabs: AtomicU64,
    pub grab_failures: AtomicU64,
    pub mode_changes: AtomicU64,
    pub events: AtomicU64,
    /// `MODE_CHANGED` events refused as implausible or too large.
    pub rejected_modes: AtomicU64,
    mode: Mutex<Option<(i32, i32)>>,
    /// Why the thread died (fatal I/O error or panic); `None` while healthy or stopped on
    /// request.
    failure: Mutex<Option<String>>,
}

/// Point-in-time copy of [`PainterStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PainterSnapshot {
    pub grabs: u64,
    pub grab_failures: u64,
    pub mode_changes: u64,
    pub events: u64,
    pub rejected_modes: u64,
    /// Last mode the compositor set, as `(width, height)`.
    pub mode: Option<(i32, i32)>,
}

impl PainterStats {
    pub fn snapshot(&self) -> PainterSnapshot {
        PainterSnapshot {
            grabs: self.grabs.load(Ordering::Relaxed),
            grab_failures: self.grab_failures.load(Ordering::Relaxed),
            mode_changes: self.mode_changes.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            rejected_modes: self.rejected_modes.load(Ordering::Relaxed),
            mode: self.mode.lock().map(|m| *m).unwrap_or(None),
        }
    }

    fn set_failure(&self, reason: String) {
        if let Ok(mut f) = self.failure.lock() {
            f.get_or_insert(reason);
        }
    }
}

/// Background thread servicing one EVDI card fd. Stops and joins on drop.
pub struct PainterService {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<PainterStats>,
}

impl PainterService {
    /// Starts servicing `card`. The thread holds its own `Arc` to the fd, so the fd outlives
    /// the thread; the owner must still keep its own `Arc` for the monitor's lifetime and drop
    /// it *after* this service.
    pub fn spawn(card: Arc<dyn CardHandle>, label: String) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PainterStats::default());
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name(format!("evdi-painter-{label}"))
                .spawn(move || {
                    let (failure_stats, failure_label) = (stats.clone(), label.clone());
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Painter::new(card, stats, label).run(&stop)
                    }));
                    if let Err(payload) = outcome {
                        let what = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        error!("EVDI painter {failure_label} panicked: {what}");
                        failure_stats.set_failure(format!("painter thread panicked: {what}"));
                    }
                })?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
            stats,
        })
    }

    pub fn stats(&self) -> PainterSnapshot {
        self.stats.snapshot()
    }

    /// Whether the servicing thread is still running (it exits on a fatal read/poll error).
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Why the thread died on its own (fatal read/poll error or panic), if it did.
    pub fn failure(&self) -> Option<String> {
        self.stats.failure.lock().ok().and_then(|f| f.clone())
    }

    /// Stops the thread and waits for it, at most [`STOP_TIMEOUT`]. Idempotent; never panics.
    pub fn stop(&mut self) {
        self.stop_within(STOP_TIMEOUT);
    }

    /// Like [`stop`](Self::stop) with an explicit bound. Returns `false` if the thread did
    /// not exit in time (e.g. it is stuck inside a kernel call); it is then detached and
    /// keeps its clone of the card fd open until it returns, so the monitor stays connected
    /// until then. Normally the thread notices the stop flag within [`POLL_INTERVAL`].
    pub fn stop_within(&mut self, timeout: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                error!(
                    "EVDI painter thread did not stop within {}ms; detaching it (the card fd \
                     stays open until it returns)",
                    timeout.as_millis()
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if thread.join().is_err() {
            warn!("EVDI painter thread panicked");
        }
        true
    }
}

impl Drop for PainterService {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Painter {
    card: Arc<dyn CardHandle>,
    stats: Arc<PainterStats>,
    label: String,
    buf: Vec<u8>,
    width: i32,
    height: i32,
    /// `request_update` returned 0, so the module will send `UPDATE_READY`.
    requested: bool,
    retry_at: Option<Instant>,
    failures_logged: u64,
    /// Events read while checking for a pending mode change, handled on the next loop turn.
    queued: Vec<EvdiEvent>,
}

impl Painter {
    fn new(card: Arc<dyn CardHandle>, stats: Arc<PainterStats>, label: String) -> Self {
        Self {
            card,
            stats,
            label,
            buf: Vec::new(),
            width: 0,
            height: 0,
            requested: false,
            retry_at: None,
            failures_logged: 0,
            queued: Vec::new(),
        }
    }

    fn run(mut self, stop: &AtomicBool) {
        debug!("EVDI painter {} started", self.label);
        // Arm update_ready before the first commit so it is not missed.
        self.pump();
        let mut events = vec![0u8; READ_BUF];
        while !stop.load(Ordering::Acquire) {
            if !self.queued.is_empty() {
                let queued = std::mem::take(&mut self.queued);
                self.handle_batch(queued);
                continue;
            }
            match self.card.wait_readable(POLL_INTERVAL) {
                Ok(true) => match self.card.read(&mut events) {
                    Ok(0) => {}
                    Ok(n) => self.handle_batch(parse_events(&events[..n])),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => {
                        self.fatal(format!("read failed: {e}"));
                        break;
                    }
                },
                Ok(false) => {}
                Err(e) => {
                    self.fatal(format!("poll failed: {e}"));
                    break;
                }
            }
            if self.retry_at.is_some_and(|t| Instant::now() >= t) {
                self.retry_at = None;
                self.pump();
            }
        }
        debug!("EVDI painter {} stopped", self.label);
    }

    /// The thread cannot continue: nobody will service this card any more, so say so loudly
    /// and record why for the owner's health check.
    fn fatal(&self, reason: String) {
        error!(
            "EVDI painter {} stopped, the monitor is no longer serviced: {reason}",
            self.label
        );
        self.stats.set_failure(format!("painter stopped: {reason}"));
    }

    /// Handles the events of one `read`. The last `MODE_CHANGED` of the batch is applied
    /// first, so no `UPDATE_READY` in the same batch is answered with a stale-sized buffer.
    fn handle_batch(&mut self, events: Vec<EvdiEvent>) {
        let last_mode = events.iter().rev().find_map(|e| match e {
            EvdiEvent::ModeChanged {
                hdisplay, vdisplay, ..
            } => Some((*hdisplay, *vdisplay)),
            _ => None,
        });
        let mode_events = events
            .iter()
            .filter(|e| matches!(e, EvdiEvent::ModeChanged { .. }))
            .count() as u64;
        if let Some((w, h)) = last_mode {
            // Counted per event even though only the last one allocates. Bumped before any
            // grab so an observer that saw the flip also sees the mode.
            self.stats
                .mode_changes
                .fetch_add(mode_events, Ordering::Relaxed);
            self.set_mode(w, h);
        }
        for event in events {
            self.handle(event);
        }
        if last_mode.is_some() {
            self.pump();
        }
    }

    fn handle(&mut self, event: EvdiEvent) {
        self.stats.events.fetch_add(1, Ordering::Relaxed);
        match event {
            EvdiEvent::ModeChanged { .. } => {} // applied by `handle_batch`
            EvdiEvent::UpdateReady => {
                self.requested = false;
                self.grab();
                self.pump();
            }
            EvdiEvent::Dpms(_) | EvdiEvent::CrtcState(_) | EvdiEvent::Other(_) => {}
        }
    }

    fn set_mode(&mut self, width: i32, height: i32) {
        let bytes = width.max(0) as u64 * height.max(0) as u64 * 4;
        if !(1..=MAX_DIMENSION).contains(&width)
            || !(1..=MAX_DIMENSION).contains(&height)
            || bytes > MAX_BUFFER_BYTES
        {
            error!(
                "EVDI painter {}: refusing mode {width}x{height} (limit {MAX_DIMENSION} per \
                 side and {} MiB per buffer); this monitor will not be serviced until the \
                 compositor picks a smaller mode",
                self.label,
                MAX_BUFFER_BYTES / (1024 * 1024)
            );
            self.stats.rejected_modes.fetch_add(1, Ordering::Relaxed);
            self.buf = Vec::new();
            self.width = 0;
            self.height = 0;
            if let Ok(mut m) = self.stats.mode.lock() {
                *m = None;
            }
            return;
        }
        self.width = width;
        self.height = height;
        self.buf = vec![0u8; width as usize * height as usize * 4];
        if let Ok(mut m) = self.stats.mode.lock() {
            *m = Some((width, height));
        }
        debug!("EVDI painter {}: mode {width}x{height}", self.label);
    }

    /// Issues `request_update`; grabs while the module says dirty pixels are waiting.
    fn pump(&mut self) {
        if self.requested {
            return;
        }
        for _ in 0..8 {
            let mut req = DrmEvdiRequestUpdate::default();
            // SAFETY: DrmEvdiRequestUpdate is a single i32, no padding.
            let bytes = unsafe { as_bytes_mut(&mut req) };
            match self.card.ioctl(DRM_IOCTL_EVDI_REQUEST_UPDATE, bytes) {
                Ok(1) => {
                    self.absorb_pending_mode();
                    if !self.grab() {
                        self.retry_at = Some(Instant::now() + RETRY_DELAY);
                        return;
                    }
                }
                Ok(_) => {
                    self.requested = true;
                    return;
                }
                Err(e) => {
                    self.log_failure("request_update", &e);
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    return;
                }
            }
        }
        // Still dirty after a burst of grabs: yield to the poll loop, retry immediately.
        self.retry_at = Some(Instant::now());
    }

    /// Before grabbing on `request_update == 1`: a `MODE_CHANGED` may already be queued (the
    /// compositor changed mode while this thread was busy), and a grab sized for the old mode
    /// would be refused. Reads what is pending, applies the last mode change now and keeps the
    /// other events for the next loop turn. A mode change that lands between this read and the
    /// ioctl is still possible; the module then answers `EINVAL` and servicing continues.
    fn absorb_pending_mode(&mut self) {
        let mut buf = [0u8; READ_BUF];
        let n = match self.card.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => return,
        };
        let mut events = parse_events(&buf[..n]);
        let last_mode = events.iter().rev().find_map(|e| match e {
            EvdiEvent::ModeChanged {
                hdisplay, vdisplay, ..
            } => Some((*hdisplay, *vdisplay)),
            _ => None,
        });
        if let Some((w, h)) = last_mode {
            self.set_mode(w, h);
            let before = events.len();
            events.retain(|e| !matches!(e, EvdiEvent::ModeChanged { .. }));
            let removed = (before - events.len()) as u64;
            self.stats.events.fetch_add(removed, Ordering::Relaxed);
            self.stats
                .mode_changes
                .fetch_add(removed, Ordering::Relaxed);
        }
        self.queued.extend(events);
    }

    /// One `GRABPIX`; the pixels are discarded. Returns whether the module accepted it.
    fn grab(&mut self) -> bool {
        if self.buf.is_empty() {
            // No mode yet: the module has no scanout buffer and would answer EAGAIN.
            return false;
        }
        let mut rects = [DrmClipRect::default(); MAX_RECTS];
        // Declare the destination allocation so a model can bounds-check what the kernel
        // would write (the real kernel cannot know it; a short buffer is our memory bug).
        self.card
            .register_buffer(self.buf.as_mut_ptr(), self.buf.len());
        let mut args = DrmEvdiGrabpix {
            mode: GRABPIX_MODE_DIRTY,
            buf_width: self.width,
            buf_height: self.height,
            buf_byte_stride: self.width * 4,
            buffer: self.buf.as_mut_ptr(),
            num_rects: MAX_RECTS as i32,
            _pad: 0,
            rects: rects.as_mut_ptr(),
        };
        // SAFETY: DrmEvdiGrabpix has explicit padding only. `buffer` points to
        // `width*height*4` bytes and `rects` to `MAX_RECTS` entries, both alive across the
        // synchronous call.
        let bytes = unsafe { as_bytes_mut(&mut args) };
        match self.card.ioctl(DRM_IOCTL_EVDI_GRABPIX, bytes) {
            Ok(_) => {
                self.stats.grabs.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(e) => {
                self.stats.grab_failures.fetch_add(1, Ordering::Relaxed);
                self.log_failure("grabpix", &e);
                false
            }
        }
    }

    fn log_failure(&mut self, what: &str, err: &io::Error) {
        // First failure and then every 100th: a mode change in flight makes a few grabs fail
        // legitimately (libevdi: "Ignore if caused by change of mode").
        if self.failures_logged.is_multiple_of(100) {
            warn!("EVDI painter {}: {what} failed: {err}", self.label);
        }
        self.failures_logged += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card whose ioctls panic.
    struct PanicCard;

    impl CardHandle for PanicCard {
        fn ioctl(&self, _request: u32, _arg: &mut [u8]) -> io::Result<i32> {
            panic!("boom from ioctl");
        }
        fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn wait_readable(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(false)
        }
    }

    /// A card whose `poll` blocks far longer than any stop timeout used here, like a thread
    /// stuck in the kernel.
    struct StuckCard;

    impl CardHandle for StuckCard {
        fn ioctl(&self, _request: u32, _arg: &mut [u8]) -> io::Result<i32> {
            Ok(0)
        }
        fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn wait_readable(&self, _timeout: Duration) -> io::Result<bool> {
            std::thread::sleep(Duration::from_millis(800));
            Ok(false)
        }
    }

    /// Healthy card: nothing to do.
    struct IdleCard;

    impl CardHandle for IdleCard {
        fn ioctl(&self, _request: u32, _arg: &mut [u8]) -> io::Result<i32> {
            Ok(0)
        }
        fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
            std::thread::sleep(timeout.min(Duration::from_millis(10)));
            Ok(false)
        }
    }

    /// A scripted card: `request_update` answers 1 (dirty) twice, then 0; the second answer
    /// also queues a `MODE_CHANGED`, as if the compositor changed mode while the thread was
    /// busy. Records the size of every GRABPIX.
    #[derive(Default)]
    struct ScriptCard {
        state: Mutex<Script>,
    }

    #[derive(Default)]
    struct Script {
        requests: u32,
        queue: std::collections::VecDeque<Vec<u8>>,
        grabs: Vec<(i32, i32)>,
    }

    fn mode_event(w: i32, h: i32) -> Vec<u8> {
        let mut ev = Vec::new();
        ev.extend_from_slice(
            &crate::backend::kernel_drm::abi::DRM_EVDI_EVENT_MODE_CHANGED.to_ne_bytes(),
        );
        ev.extend_from_slice(&28u32.to_ne_bytes());
        for v in [w, h, 60, 32] {
            ev.extend_from_slice(&v.to_ne_bytes());
        }
        ev.extend_from_slice(&0u32.to_ne_bytes());
        ev
    }

    impl CardHandle for ScriptCard {
        fn ioctl(&self, request: u32, arg: &mut [u8]) -> io::Result<i32> {
            let mut st = self.state.lock().unwrap();
            if request == DRM_IOCTL_EVDI_REQUEST_UPDATE {
                st.requests += 1;
                return Ok(match st.requests {
                    1 => 1,
                    2 => {
                        st.queue.push_back(mode_event(640, 480));
                        1
                    }
                    _ => 0,
                });
            }
            if request == DRM_IOCTL_EVDI_GRABPIX {
                // SAFETY: the painter passes exactly a DrmEvdiGrabpix.
                let g: DrmEvdiGrabpix =
                    unsafe { crate::backend::kernel_drm::abi::read_struct(arg) };
                st.grabs.push((g.buf_width, g.buf_height));
            }
            Ok(0)
        }
        fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut st = self.state.lock().unwrap();
            let Some(ev) = st.queue.pop_front() else {
                return Err(io::ErrorKind::WouldBlock.into());
            };
            buf[..ev.len()].copy_from_slice(&ev);
            Ok(ev.len())
        }
        fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
            if !self.state.lock().unwrap().queue.is_empty() {
                return Ok(true);
            }
            std::thread::sleep(timeout.min(Duration::from_millis(10)));
            Ok(false)
        }
    }

    #[test]
    fn a_mode_change_pending_when_request_update_says_dirty_is_applied_before_grabbing() {
        let card = Arc::new(ScriptCard::default());
        card.state
            .lock()
            .unwrap()
            .queue
            .push_back(mode_event(800, 600));
        let mut service = PainterService::spawn(card.clone(), "test".into()).unwrap();
        assert!(wait_for(|| card.state.lock().unwrap().requests >= 3));
        service.stop();
        let grabs = card.state.lock().unwrap().grabs.clone();
        assert_eq!(
            grabs,
            vec![(800, 600), (640, 480)],
            "second grab used a stale size"
        );
    }

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn a_panicking_painter_thread_is_reported_as_failed() {
        let service = PainterService::spawn(Arc::new(PanicCard), "test".into()).unwrap();
        assert!(wait_for(|| !service.is_running()));
        let why = service.failure().expect("failure recorded");
        assert!(
            why.contains("panicked") && why.contains("boom from ioctl"),
            "{why}"
        );
    }

    #[test]
    fn stop_is_bounded_when_the_thread_is_stuck() {
        let mut service = PainterService::spawn(Arc::new(StuckCard), "test".into()).unwrap();
        // Let the thread enter its blocking poll first.
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert!(!service.stop_within(Duration::from_millis(100)));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "stop must not wait for the stuck thread: {:?}",
            started.elapsed()
        );
        assert!(!service.is_running(), "detached");
        // Idempotent.
        assert!(service.stop_within(Duration::from_millis(10)));
    }

    #[test]
    fn normal_stop_joins_within_a_poll_interval() {
        let mut service = PainterService::spawn(Arc::new(IdleCard), "test".into()).unwrap();
        let started = Instant::now();
        assert!(service.stop_within(Duration::from_secs(2)));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(service.failure().is_none());
    }
}
