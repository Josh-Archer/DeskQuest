//! [`AutoCapture`]: the real screen capture when it works, the synthetic test pattern while it
//! does not.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinHandle};

use crate::screen_lock::{session_screen_lock, ScreenLock};
use crate::{
    CaptureBackendType, CaptureError, CaptureInfo, DisplayCapture, PipeWireCapture, RawFrame,
    ScreenCastError, SyntheticCapture,
};

/// Wait between losing the capture and the first restore. A lock screen that closed the
/// session is up by then, so the lock state read next is right.
const FIRST_RETRY: Duration = Duration::from_secs(1);
/// Longest wait between failed restores.
const MAX_RETRY: Duration = Duration::from_secs(30);
/// How often the lock state is read while the screen is locked.
const LOCKED_POLL: Duration = Duration::from_secs(2);
/// Wait after an unlock before restoring. GNOME allows screen casts again only once the unlock
/// has finished, and a restore it refuses opens its share dialog.
const UNLOCK_GRACE: Duration = Duration::from_secs(2);

/// Dynamically discovers and initializes the best available capture engine.
///
/// If the real capture cannot start at all, the synthetic test pattern replaces it for good.
/// Once it has started, a lost capture (the portal session closed, as GNOME does when the
/// screen locks) is replaced only for a while: the test pattern stands in while the capture is
/// restored in the background with [`DisplayCapture::restore`] (for the portal, the stored
/// restore token; the user is not asked), and the real screen is back as soon as that works.
///
/// * No restore is tried while the screen is locked (see [`crate::screen_lock`]); it starts
///   2 s after the unlock.
/// * Failed restores are retried after 1 s, 2 s, 4 s and so on, up to 30 s apart.
/// * A restore the user declined, or one that needs the user (no restore token), is not
///   retried. [`reselect`](DisplayCapture::reselect) shows the picker at any time and brings
///   the real capture back with the screen the user picks.
pub struct AutoCapture {
    /// The stand-in, and the only source when there is no real capture.
    pattern: SyntheticCapture,
    state: State,
    lock: Arc<dyn ScreenLock>,
    /// True once `init` ran.
    started: bool,
}

enum State {
    /// The real capture (before the first `init`, the one to try).
    Real(Box<dyn DisplayCapture>),
    /// The test pattern for good: there is no real capture.
    Pattern,
    /// The test pattern while the real capture is away.
    StandIn(Away),
}

enum Away {
    /// Being restored in the background.
    Restoring(Restoring),
    /// Not restored until the user picks a screen.
    Parked(Box<dyn DisplayCapture>),
}

/// How a background restore ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreEnd {
    Restored,
    /// Retrying would not help (the user declined, or restoring needs the user).
    GaveUp,
    Cancelled,
}

/// A background restore. Dropping it stops the task (and the capture it holds).
struct Restoring {
    task: JoinHandle<(Box<dyn DisplayCapture>, RestoreEnd)>,
    cancel: Option<oneshot::Sender<()>>,
}

impl Restoring {
    fn start(real: Box<dyn DisplayCapture>, lock: Arc<dyn ScreenLock>) -> Self {
        let (cancel, cancelled) = oneshot::channel();
        Self {
            task: tokio::spawn(restore_in_background(real, lock, cancelled)),
            cancel: Some(cancel),
        }
    }

    /// Stops restoring (an attempt in progress is abandoned) and hands the capture back.
    async fn stop(mut self) -> Option<Box<dyn DisplayCapture>> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        match (&mut self.task).await {
            Ok((real, _)) => Some(real),
            Err(e) => {
                tracing::error!(error = %e, "the screen capture restore task failed");
                None
            }
        }
    }
}

impl Drop for Restoring {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AutoCapture {
    /// The portal screen capture when this build and session can have one, otherwise the test
    /// pattern; `width`, `height` and `fps` are the capture's hints and the test pattern's size.
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        let real = PipeWireCapture::is_available().then(|| {
            Box::new(PipeWireCapture::new(display_id, width, height, fps))
                as Box<dyn DisplayCapture>
        });
        Self::with_capture(display_id, width, height, fps, real, session_screen_lock())
    }

    /// Uses `real` (none: only the test pattern) and asks `lock` whether the screen is locked
    /// before restoring it. `width`, `height` and `fps` are the test pattern's.
    pub fn with_capture(
        display_id: u16,
        width: u32,
        height: u32,
        fps: u32,
        real: Option<Box<dyn DisplayCapture>>,
        lock: Arc<dyn ScreenLock>,
    ) -> Self {
        Self {
            pattern: SyntheticCapture::new(display_id, width, height, fps),
            state: real.map_or(State::Pattern, State::Real),
            lock,
            started: false,
        }
    }

    /// Lets the test pattern stand in while `real` is restored in the background.
    fn restore_later(&mut self, real: Box<dyn DisplayCapture>) {
        self.state = State::StandIn(Away::Restoring(Restoring::start(
            real,
            Arc::clone(&self.lock),
        )));
    }

    /// Takes the real capture out, stopping a background restore, and leaves the test pattern
    /// in its place. `None` when there is no real capture.
    async fn take_real(&mut self) -> Option<Box<dyn DisplayCapture>> {
        match std::mem::replace(&mut self.state, State::Pattern) {
            State::Real(real) | State::StandIn(Away::Parked(real)) => Some(real),
            State::StandIn(Away::Restoring(restoring)) => restoring.stop().await,
            State::Pattern => None,
        }
    }

    fn finish_restore(&mut self, ended: Result<(Box<dyn DisplayCapture>, RestoreEnd), JoinError>) {
        self.state = match ended {
            Ok((real, RestoreEnd::Restored)) => State::Real(real),
            Ok((real, _)) => State::StandIn(Away::Parked(real)),
            Err(e) => {
                tracing::error!(error = %e, "the screen capture restore task failed; showing the test pattern from now on");
                State::Pattern
            }
        };
    }
}

/// True when the capture ended under us (the portal session closed, the node went away) and
/// can be started again.
fn capture_lost(e: &CaptureError) -> bool {
    matches!(
        e,
        CaptureError::ScreenCast(ScreenCastError::SessionClosed | ScreenCastError::NodeRemoved)
    )
}

/// True when restoring again would not help: the user declined, there is nothing to restore
/// without asking them, or this build cannot capture.
fn retry_cannot_help(e: &CaptureError) -> bool {
    matches!(
        e,
        CaptureError::ScreenCast(
            ScreenCastError::UserCancelled
                | ScreenCastError::NoRestoreToken
                | ScreenCastError::FeatureDisabled
        ) | CaptureError::BackendUnavailable(_)
    )
}

async fn restore_in_background(
    mut real: Box<dyn DisplayCapture>,
    lock: Arc<dyn ScreenLock>,
    mut cancelled: oneshot::Receiver<()>,
) -> (Box<dyn DisplayCapture>, RestoreEnd) {
    let end = tokio::select! {
        end = restore_until_done(real.as_mut(), lock.as_ref()) => end,
        _ = &mut cancelled => RestoreEnd::Cancelled,
    };
    (real, end)
}

async fn restore_until_done(real: &mut dyn DisplayCapture, lock: &dyn ScreenLock) -> RestoreEnd {
    let mut wait = FIRST_RETRY;
    let mut backoff = FIRST_RETRY;
    let mut locked = false;
    loop {
        tokio::time::sleep(wait).await;
        match lock.is_locked().await {
            Some(true) => {
                if !locked {
                    tracing::info!(
                        "the screen is locked; the screen capture is restored once it is unlocked"
                    );
                }
                locked = true;
                wait = LOCKED_POLL;
                continue;
            }
            _ if locked => {
                locked = false;
                backoff = FIRST_RETRY;
                wait = UNLOCK_GRACE;
                continue;
            }
            _ => {}
        }
        match real.restore().await {
            Ok(()) => {
                tracing::info!("screen capture restored; the screen replaces the test pattern");
                return RestoreEnd::Restored;
            }
            Err(e) if retry_cannot_help(&e) => {
                tracing::warn!(error = %e, "screen capture not restored; choose a screen to share it again");
                return RestoreEnd::GaveUp;
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "screen capture not restored yet");
                wait = backoff;
                backoff = (backoff * 2).min(MAX_RETRY);
            }
        }
    }
}

#[async_trait]
impl DisplayCapture for AutoCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        let first = !std::mem::replace(&mut self.started, true);
        let Some(mut real) = self.take_real().await else {
            return self.pattern.init().await;
        };
        match real.init().await {
            Ok(()) => {
                self.state = State::Real(real);
                Ok(())
            }
            Err(e) if first => {
                tracing::warn!(
                    "Primary capture backend failed to initialize: {e}. Gracefully demoting to SyntheticCapture"
                );
                self.pattern.init().await
            }
            Err(e) => {
                tracing::warn!(error = %e, "screen capture did not start again; the test pattern stands in");
                if retry_cannot_help(&e) {
                    self.state = State::StandIn(Away::Parked(real));
                } else {
                    self.restore_later(real);
                }
                self.pattern.init().await
            }
        }
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        loop {
            match &mut self.state {
                State::Real(real) => {
                    let lost = match real.capture_frame().await {
                        Err(e) if capture_lost(&e) => e,
                        other => return other,
                    };
                    tracing::warn!(error = %lost, "screen capture lost; the test pattern stands in until it is restored");
                    if let State::Real(real) = std::mem::replace(&mut self.state, State::Pattern) {
                        self.restore_later(real);
                    }
                }
                State::StandIn(Away::Restoring(restoring)) => {
                    // Both are cancel-safe: the task stays in `self`, and the pattern only
                    // advances once it returns a frame.
                    let ended = tokio::select! {
                        biased;
                        ended = &mut restoring.task => ended,
                        frame = self.pattern.capture_frame() => return frame,
                    };
                    self.finish_restore(ended);
                }
                State::Pattern | State::StandIn(Away::Parked(_)) => {
                    return self.pattern.capture_frame().await;
                }
            }
        }
    }

    fn backend_type(&self) -> CaptureBackendType {
        match &self.state {
            State::Real(real) => real.backend_type(),
            State::Pattern | State::StandIn(_) => CaptureBackendType::Synthetic,
        }
    }

    fn capture_info(&self) -> Option<CaptureInfo> {
        match &self.state {
            State::Real(real) => real.capture_info(),
            State::Pattern | State::StandIn(_) => self.pattern.capture_info(),
        }
    }

    /// Shows the real capture's picker, also while the test pattern stands in for it (a
    /// background restore is stopped first). If afterwards nothing is captured (the pick
    /// failed and the previous screen could not be restored), the test pattern stands in and
    /// the capture is restored in the background.
    async fn reselect(&mut self) -> Result<(), CaptureError> {
        let standing_in = matches!(self.state, State::StandIn(_));
        let Some(mut real) = self.take_real().await else {
            return Err(CaptureError::BackendUnavailable(
                "this capture source has no screen picker".into(),
            ));
        };
        if standing_in {
            tracing::info!("showing the screen picker in place of the test pattern");
        }
        let result = real.reselect().await;
        if result.is_ok() || real.is_running() {
            self.state = State::Real(real);
        } else {
            tracing::warn!(
                "no screen is captured after the pick; the test pattern stands in until it is restored"
            );
            self.restore_later(real);
        }
        result
    }

    fn is_running(&self) -> bool {
        match &self.state {
            State::Real(real) => real.is_running(),
            State::Pattern | State::StandIn(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;
    use bytes::Bytes;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tokio::time::Instant;

    /// Size of the fake screen's frames; the test pattern is `PATTERN`.
    const SCREEN: (u32, u32) = (32, 16);
    const PATTERN: (u32, u32) = (8, 8);

    /// What the fake screen does, and what was asked of it.
    #[derive(Default)]
    struct Script {
        /// Frames delivered per session before it closes; `None` never closes.
        frames_per_session: Option<u32>,
        /// Results of successive `restore` calls; `Ok` once they run out.
        restore_results: VecDeque<Result<(), CaptureError>>,
        /// `restore` never finishes.
        restore_hangs: bool,
        /// Results of successive `reselect` calls; `Ok` once they run out.
        reselect_results: VecDeque<Result<(), CaptureError>>,
        inits: u32,
        restores: u32,
        reselects: u32,
    }

    /// A portal screen capture stand-in: sessions close on cue, restores follow the script.
    struct FakeScreen {
        script: Arc<Mutex<Script>>,
        /// Set when a hanging restore was abandoned.
        abandoned: Arc<AtomicBool>,
        running: bool,
        frames: u32,
    }

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl FakeScreen {
        fn new(script: Script) -> (Self, Arc<Mutex<Script>>, Arc<AtomicBool>) {
            let script = Arc::new(Mutex::new(script));
            let abandoned = Arc::new(AtomicBool::new(false));
            let screen = Self {
                script: Arc::clone(&script),
                abandoned: Arc::clone(&abandoned),
                running: false,
                frames: 0,
            };
            (screen, script, abandoned)
        }

        fn started(&mut self, result: Result<(), CaptureError>) -> Result<(), CaptureError> {
            if result.is_ok() {
                self.running = true;
                self.frames = 0;
            }
            result
        }
    }

    #[async_trait]
    impl DisplayCapture for FakeScreen {
        async fn init(&mut self) -> Result<(), CaptureError> {
            self.script.lock().unwrap().inits += 1;
            self.started(Ok(()))
        }

        async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
            if !self.running {
                return Err(CaptureError::InitFailed("not started".into()));
            }
            let per_session = self.script.lock().unwrap().frames_per_session;
            if per_session.is_some_and(|n| self.frames >= n) {
                self.running = false;
                return Err(ScreenCastError::SessionClosed.into());
            }
            self.frames += 1;
            tokio::task::yield_now().await;
            Ok(frame(SCREEN))
        }

        fn backend_type(&self) -> CaptureBackendType {
            CaptureBackendType::PipeWire
        }

        async fn restore(&mut self) -> Result<(), CaptureError> {
            let (hangs, result) = {
                let mut s = self.script.lock().unwrap();
                s.restores += 1;
                (
                    s.restore_hangs,
                    s.restore_results.pop_front().unwrap_or(Ok(())),
                )
            };
            if hangs {
                let _flag = DropFlag(Arc::clone(&self.abandoned));
                std::future::pending::<()>().await;
            }
            self.started(result)
        }

        async fn reselect(&mut self) -> Result<(), CaptureError> {
            let result = {
                let mut s = self.script.lock().unwrap();
                s.reselects += 1;
                s.reselect_results.pop_front().unwrap_or(Ok(()))
            };
            if result.is_err() {
                // A failed pick whose previous screen is not restored.
                self.running = false;
            }
            self.started(result)
        }

        fn is_running(&self) -> bool {
            self.running
        }
    }

    /// A lock screen the test turns on and off.
    #[derive(Default)]
    struct FakeLock {
        locked: Mutex<Option<bool>>,
        queries: Mutex<u32>,
    }

    impl FakeLock {
        fn set(&self, locked: Option<bool>) {
            *self.locked.lock().unwrap() = locked;
        }
    }

    #[async_trait]
    impl ScreenLock for FakeLock {
        async fn is_locked(&self) -> Option<bool> {
            *self.queries.lock().unwrap() += 1;
            *self.locked.lock().unwrap()
        }
    }

    fn frame((width, height): (u32, u32)) -> RawFrame {
        RawFrame {
            display_id: 0,
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Bgra8,
            pts_us: 0,
            dma_buf: None,
            data: Bytes::new(),
        }
    }

    fn auto(screen: FakeScreen, lock: Arc<dyn ScreenLock>) -> AutoCapture {
        AutoCapture::with_capture(0, PATTERN.0, PATTERN.1, 10, Some(Box::new(screen)), lock)
    }

    fn size(frame: &RawFrame) -> (u32, u32) {
        (frame.width, frame.height)
    }

    /// Captures until a frame of the screen arrives; returns how many pattern frames came first.
    async fn until_screen(capture: &mut AutoCapture) -> u32 {
        let mut pattern_frames = 0;
        loop {
            let f = capture.capture_frame().await.expect("frames keep coming");
            if size(&f) == SCREEN {
                return pattern_frames;
            }
            assert_eq!(size(&f), PATTERN);
            pattern_frames += 1;
        }
    }

    /// Captures test pattern frames for `period` (paused clock).
    async fn pattern_for(capture: &mut AutoCapture, period: Duration) {
        let end = Instant::now() + period;
        while Instant::now() < end {
            let f = capture.capture_frame().await.unwrap();
            assert_eq!(size(&f), PATTERN, "the pattern stands in");
        }
    }

    /// Starts `capture` and lets its session close: the next frame is the test pattern.
    async fn lose_the_screen(capture: &mut AutoCapture) {
        capture.init().await.unwrap();
        assert_eq!(capture.backend_type(), CaptureBackendType::PipeWire);
        assert_eq!(size(&capture.capture_frame().await.unwrap()), SCREEN);
        assert_eq!(size(&capture.capture_frame().await.unwrap()), PATTERN);
        assert_eq!(capture.backend_type(), CaptureBackendType::Synthetic);
    }

    #[tokio::test(start_paused = true)]
    async fn the_pattern_stands_in_until_the_capture_is_restored() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_results: VecDeque::from([
                Err(ScreenCastError::NegotiationTimeout(Duration::from_secs(300)).into()),
                Err(ScreenCastError::Denied("portal response code 2".into()).into()),
            ]),
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        let lost = Instant::now();

        until_screen(&mut capture).await;
        // Attempts after 1 s, then 1 s and 2 s more: two failures, then the restore.
        let waited = lost.elapsed();
        assert!(
            waited >= Duration::from_secs(4) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert_eq!(capture.backend_type(), CaptureBackendType::PipeWire);
        let s = script.lock().unwrap();
        assert_eq!((s.inits, s.restores, s.reselects), (1, 3, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_back_off_to_thirty_seconds() {
        let failures = 8;
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_results: (0..failures)
                .map(|_| Err(ScreenCastError::Portal("try later".into()).into()))
                .collect(),
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        let lost = Instant::now();
        until_screen(&mut capture).await;
        // 1 s, then 1 + 2 + 4 + 8 + 16 + 30 + 30 + 30 s after the eight failures.
        let waited = lost.elapsed();
        assert!(
            waited >= Duration::from_secs(122) && waited < Duration::from_secs(123),
            "{waited:?}"
        );
        assert_eq!(script.lock().unwrap().restores, failures + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_restored_while_the_screen_is_locked() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            ..Script::default()
        });
        let lock = Arc::new(FakeLock::default());
        lock.set(Some(true));
        let mut capture = auto(screen, lock.clone());
        lose_the_screen(&mut capture).await;

        pattern_for(&mut capture, Duration::from_secs(600)).await;
        assert_eq!(
            script.lock().unwrap().restores,
            0,
            "no restore while locked"
        );
        assert!(*lock.queries.lock().unwrap() > 100, "the lock is polled");

        lock.set(Some(false));
        let unlocked = Instant::now();
        until_screen(&mut capture).await;
        let waited = unlocked.elapsed();
        assert!(
            waited >= UNLOCK_GRACE
                && waited <= LOCKED_POLL + UNLOCK_GRACE + Duration::from_millis(200),
            "restored {waited:?} after the unlock"
        );
        assert_eq!(script.lock().unwrap().restores, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_declined_restore_waits_for_the_user_to_choose_a_screen() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_results: VecDeque::from([Err(ScreenCastError::UserCancelled.into())]),
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        pattern_for(&mut capture, Duration::from_secs(300)).await;
        assert_eq!(script.lock().unwrap().restores, 1, "not retried");

        script.lock().unwrap().frames_per_session = None;
        capture.reselect().await.expect("the user picks a screen");
        assert_eq!(capture.backend_type(), CaptureBackendType::PipeWire);
        assert_eq!(size(&capture.capture_frame().await.unwrap()), SCREEN);
        assert_eq!(script.lock().unwrap().reselects, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_to_restore_is_not_retried() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_results: VecDeque::from([Err(ScreenCastError::NoRestoreToken.into())]),
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        pattern_for(&mut capture, Duration::from_secs(300)).await;
        assert_eq!(script.lock().unwrap().restores, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn choosing_a_screen_stops_a_pending_restore() {
        let (screen, script, abandoned) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_hangs: true,
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        pattern_for(&mut capture, Duration::from_secs(5)).await;
        assert_eq!(script.lock().unwrap().restores, 1, "one restore is pending");

        script.lock().unwrap().frames_per_session = None;
        capture.reselect().await.expect("picked");
        assert!(
            abandoned.load(Ordering::SeqCst),
            "the pending restore was dropped"
        );
        assert_eq!(capture.backend_type(), CaptureBackendType::PipeWire);
        assert_eq!(size(&capture.capture_frame().await.unwrap()), SCREEN);
        assert_eq!(script.lock().unwrap().reselects, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_pick_on_the_pattern_keeps_restoring() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            reselect_results: VecDeque::from([Err(ScreenCastError::UserCancelled.into())]),
            ..Script::default()
        });
        let lock = Arc::new(FakeLock::default());
        lock.set(Some(true));
        let mut capture = auto(screen, lock.clone());
        lose_the_screen(&mut capture).await;

        let err = capture.reselect().await.unwrap_err();
        assert!(matches!(
            err,
            CaptureError::ScreenCast(ScreenCastError::UserCancelled)
        ));
        assert_eq!(capture.backend_type(), CaptureBackendType::Synthetic);
        pattern_for(&mut capture, Duration::from_secs(30)).await;

        lock.set(Some(false));
        until_screen(&mut capture).await;
        let s = script.lock().unwrap();
        assert_eq!((s.reselects, s.restores), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn the_screen_can_be_lost_and_restored_again() {
        let (screen, script, _) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        for _ in 0..3 {
            until_screen(&mut capture).await;
            // The restored session closes after its one frame, too.
            assert_eq!(size(&capture.capture_frame().await.unwrap()), PATTERN);
        }
        assert_eq!(script.lock().unwrap().restores, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_capture_stops_restoring() {
        let (screen, _, abandoned) = FakeScreen::new(Script {
            frames_per_session: Some(1),
            restore_hangs: true,
            ..Script::default()
        });
        let mut capture = auto(screen, Arc::new(crate::screen_lock::UnknownScreenLock));
        lose_the_screen(&mut capture).await;
        pattern_for(&mut capture, Duration::from_secs(5)).await;
        drop(capture);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(abandoned.load(Ordering::SeqCst));
    }

    /// Fails `init` like a portal with nobody to answer.
    struct NeverStarts;

    #[async_trait]
    impl DisplayCapture for NeverStarts {
        async fn init(&mut self) -> Result<(), CaptureError> {
            Err(ScreenCastError::PortalUnavailable("no portal".into()).into())
        }
        async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
            unreachable!("never started")
        }
        fn backend_type(&self) -> CaptureBackendType {
            CaptureBackendType::PipeWire
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_that_never_started_is_replaced_for_good() {
        let mut capture = AutoCapture::with_capture(
            0,
            PATTERN.0,
            PATTERN.1,
            10,
            Some(Box::new(NeverStarts)),
            Arc::new(crate::screen_lock::UnknownScreenLock),
        );
        capture.init().await.expect("the test pattern works");
        assert_eq!(capture.backend_type(), CaptureBackendType::Synthetic);
        pattern_for(&mut capture, Duration::from_secs(60)).await;
        assert!(matches!(
            capture.reselect().await,
            Err(CaptureError::BackendUnavailable(_))
        ));
    }
}
