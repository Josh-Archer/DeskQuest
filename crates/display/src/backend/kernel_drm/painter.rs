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
//!    4 bytes per pixel; stride is `width * 4`).
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

use tracing::{debug, warn};

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
/// Refuse absurd modes instead of allocating gigabytes.
const MAX_DIMENSION: i32 = 16384;
const READ_BUF: usize = 4096;

/// Counters exposed for tests and diagnostics.
#[derive(Debug, Default)]
pub struct PainterStats {
    pub grabs: AtomicU64,
    pub grab_failures: AtomicU64,
    pub mode_changes: AtomicU64,
    pub events: AtomicU64,
    mode: Mutex<Option<(i32, i32)>>,
}

/// Point-in-time copy of [`PainterStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PainterSnapshot {
    pub grabs: u64,
    pub grab_failures: u64,
    pub mode_changes: u64,
    pub events: u64,
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
            mode: self.mode.lock().map(|m| *m).unwrap_or(None),
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
                .spawn(move || Painter::new(card, stats, label).run(&stop))?
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

    /// Stops the thread and waits for it. Idempotent; never panics.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                warn!("EVDI painter thread panicked");
            }
        }
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
        }
    }

    fn run(mut self, stop: &AtomicBool) {
        debug!("EVDI painter {} started", self.label);
        // Arm update_ready before the first commit so it is not missed.
        self.pump();
        let mut events = vec![0u8; READ_BUF];
        while !stop.load(Ordering::Acquire) {
            match self.card.wait_readable(POLL_INTERVAL) {
                Ok(true) => match self.card.read(&mut events) {
                    Ok(0) => {}
                    Ok(n) => {
                        for event in parse_events(&events[..n]) {
                            self.handle(event);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => {
                        warn!("EVDI painter {}: read failed, stopping: {e}", self.label);
                        break;
                    }
                },
                Ok(false) => {}
                Err(e) => {
                    warn!("EVDI painter {}: poll failed, stopping: {e}", self.label);
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

    fn handle(&mut self, event: EvdiEvent) {
        self.stats.events.fetch_add(1, Ordering::Relaxed);
        match event {
            EvdiEvent::ModeChanged {
                hdisplay, vdisplay, ..
            } => {
                self.set_mode(hdisplay, vdisplay);
                self.pump();
            }
            EvdiEvent::UpdateReady => {
                self.requested = false;
                self.grab();
                self.pump();
            }
            EvdiEvent::Dpms(_) | EvdiEvent::CrtcState(_) | EvdiEvent::Other(_) => {}
        }
    }

    fn set_mode(&mut self, width: i32, height: i32) {
        self.stats.mode_changes.fetch_add(1, Ordering::Relaxed);
        if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
            warn!(
                "EVDI painter {}: ignoring implausible mode {width}x{height}",
                self.label
            );
            self.buf = Vec::new();
            self.width = 0;
            self.height = 0;
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

    /// One `GRABPIX`; the pixels are discarded. Returns whether the module accepted it.
    fn grab(&mut self) -> bool {
        if self.buf.is_empty() {
            // No mode yet: the module has no scanout buffer and would answer EAGAIN.
            return false;
        }
        let mut rects = [DrmClipRect::default(); MAX_RECTS];
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
