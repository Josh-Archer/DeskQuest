//! Pure, host-testable Android activity + OpenXR session lifecycle state machine.
//!
//! On Horizon OS the runtime refuses `xrCreateSession`/`xrCreateSwapchainAndroidSurfaceKHR`
//! ("Activity is not yet in the ready state") until the NativeActivity has a window and is
//! resumed. This module holds no handles: the Android event loop feeds it
//! [`LifecycleEvent`]s and facts about the runtime, and executes the single
//! [`NextAction`] that [`AppLifecycle::decide_next_action`] returns. Every transition is
//! logged at info level with the tag [`LOG_TAG`] so it can be read in logcat
//! (`adb logcat | grep lq-lifecycle`).

/// Greppable tag included in every lifecycle log line.
pub const LOG_TAG: &str = "lq-lifecycle";

/// Android activity events relevant to startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// `MainEvent::InitWindow`: the activity now has a native window.
    InitWindow,
    /// `MainEvent::TerminateWindow`: the native window is going away.
    TermWindow,
    /// `MainEvent::Resume`.
    Resume,
    /// `MainEvent::Pause`.
    Pause,
    /// `MainEvent::GainedFocus`.
    GainedFocus,
    /// `MainEvent::LostFocus`.
    LostFocus,
    /// `MainEvent::Destroy`.
    Destroy,
}

/// What the event loop must do on this iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextAction {
    /// Not ready: do nothing (no error), block briefly in the event poll and ask again.
    Wait,
    /// Window exists and the activity is resumed: create the runtime and OpenXR session.
    CreateSession,
    /// Session exists but is not running (or the app is paused): only drain OpenXR events so
    /// READY/STOPPING/EXITING are handled.
    PollEvents,
    /// Session is running: create the swapchains (and the decoder that needs their surface).
    CreateSwapchains,
    /// Session running, swapchains exist, app visible: step a frame.
    RenderFrame,
    /// Leave the main loop and shut down.
    Exit,
}

/// Activity and OpenXR session facts that gate startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLifecycle {
    window_ready: bool,
    resumed: bool,
    focused: bool,
    destroyed: bool,
    session_created: bool,
    /// Between `xrBeginSession` (SessionState::READY handled) and `xrEndSession`.
    session_running: bool,
    swapchains_created: bool,
    /// EXITING / LOSS_PENDING seen.
    exit_requested: bool,
}

impl AppLifecycle {
    /// Fresh state: nothing ready.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the activity currently has a native window.
    pub fn window_ready(&self) -> bool {
        self.window_ready
    }

    /// Whether the activity is resumed.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Whether the OpenXR session has been created.
    pub fn session_created(&self) -> bool {
        self.session_created
    }

    /// Whether swapchains have been created.
    pub fn swapchains_created(&self) -> bool {
        self.swapchains_created
    }

    /// Applies an Android activity event.
    pub fn on_event(&mut self, event: LifecycleEvent) {
        match event {
            LifecycleEvent::InitWindow => self.window_ready = true,
            LifecycleEvent::TermWindow => self.window_ready = false,
            LifecycleEvent::Resume => self.resumed = true,
            LifecycleEvent::Pause => self.resumed = false,
            LifecycleEvent::GainedFocus => self.focused = true,
            LifecycleEvent::LostFocus => self.focused = false,
            LifecycleEvent::Destroy => {
                self.destroyed = true;
                self.resumed = false;
                self.window_ready = false;
                self.focused = false;
            }
        }
        tracing::info!(
            tag = LOG_TAG,
            ?event,
            window_ready = self.window_ready,
            resumed = self.resumed,
            focused = self.focused,
            "{LOG_TAG}: activity event"
        );
    }

    /// Records that the runtime and OpenXR session were created successfully.
    pub fn on_session_created(&mut self) {
        self.session_created = true;
        tracing::info!(tag = LOG_TAG, "{LOG_TAG}: OpenXR session created");
    }

    /// Records the OpenXR session running state (true after `xrBeginSession`, false after
    /// `xrEndSession`). Ending the session also invalidates nothing else: swapchains are kept
    /// so a later READY can resume without recreating them.
    pub fn on_session_running(&mut self, running: bool) {
        if self.session_running != running {
            self.session_running = running;
            tracing::info!(
                tag = LOG_TAG,
                running,
                "{LOG_TAG}: OpenXR session running changed"
            );
        }
    }

    /// Records that the swapchains and decoder were created.
    pub fn on_swapchains_created(&mut self) {
        self.swapchains_created = true;
        tracing::info!(tag = LOG_TAG, "{LOG_TAG}: swapchains created");
    }

    /// Records that OpenXR requested exit (EXITING or LOSS_PENDING).
    pub fn on_exit_requested(&mut self) {
        if !self.exit_requested {
            self.exit_requested = true;
            tracing::info!(tag = LOG_TAG, "{LOG_TAG}: OpenXR requested exit");
        }
    }

    /// Decides the single next step. Pure: the same state always gives the same action.
    pub fn decide_next_action(&self) -> NextAction {
        if self.destroyed || self.exit_requested {
            return NextAction::Exit;
        }
        if !self.session_created {
            return if self.window_ready && self.resumed {
                NextAction::CreateSession
            } else {
                NextAction::Wait
            };
        }
        if !self.session_running {
            return NextAction::PollEvents;
        }
        if !self.swapchains_created {
            return NextAction::CreateSwapchains;
        }
        if self.resumed && self.window_ready {
            NextAction::RenderFrame
        } else {
            NextAction::PollEvents
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LifecycleEvent::*;

    fn run(events: &[LifecycleEvent]) -> AppLifecycle {
        let mut s = AppLifecycle::new();
        for e in events {
            s.on_event(*e);
        }
        s
    }

    #[test]
    fn nothing_ready_waits() {
        assert_eq!(AppLifecycle::new().decide_next_action(), NextAction::Wait);
    }

    #[test]
    fn window_without_resume_waits() {
        assert_eq!(run(&[InitWindow]).decide_next_action(), NextAction::Wait);
    }

    #[test]
    fn resume_without_window_waits() {
        assert_eq!(run(&[Resume]).decide_next_action(), NextAction::Wait);
    }

    #[test]
    fn both_orderings_of_window_and_resume_create_session() {
        assert_eq!(
            run(&[InitWindow, Resume]).decide_next_action(),
            NextAction::CreateSession
        );
        assert_eq!(
            run(&[Resume, InitWindow]).decide_next_action(),
            NextAction::CreateSession
        );
        // Focus events and extra noise do not matter.
        assert_eq!(
            run(&[GainedFocus, Resume, LostFocus, InitWindow]).decide_next_action(),
            NextAction::CreateSession
        );
    }

    #[test]
    fn pause_or_term_window_before_creation_cancels_it() {
        assert_eq!(
            run(&[InitWindow, Resume, Pause]).decide_next_action(),
            NextAction::Wait
        );
        assert_eq!(
            run(&[InitWindow, Resume, TermWindow]).decide_next_action(),
            NextAction::Wait
        );
        // ...and coming back re-arms it (the 150 ms create/destroy churn case).
        assert_eq!(
            run(&[InitWindow, Resume, Pause, Resume]).decide_next_action(),
            NextAction::CreateSession
        );
    }

    #[test]
    fn destroy_always_exits_in_every_phase() {
        assert_eq!(run(&[Destroy]).decide_next_action(), NextAction::Exit);
        assert_eq!(
            run(&[InitWindow, Resume, Destroy]).decide_next_action(),
            NextAction::Exit
        );
        // A destroyed activity never creates a session even if events arrive afterwards.
        assert_eq!(
            run(&[Destroy, InitWindow, Resume]).decide_next_action(),
            NextAction::Exit
        );
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        s.on_session_running(true);
        s.on_swapchains_created();
        s.on_event(Destroy);
        assert_eq!(s.decide_next_action(), NextAction::Exit);
    }

    #[test]
    fn session_not_running_only_polls() {
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        // IDLE / not yet READY: never create swapchains, never render.
        assert_eq!(s.decide_next_action(), NextAction::PollEvents);
        // Even if the app is paused we keep draining events.
        s.on_event(Pause);
        assert_eq!(s.decide_next_action(), NextAction::PollEvents);
    }

    #[test]
    fn swapchains_only_after_session_running() {
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        assert_ne!(s.decide_next_action(), NextAction::CreateSwapchains);
        s.on_session_running(true);
        assert_eq!(s.decide_next_action(), NextAction::CreateSwapchains);
        s.on_swapchains_created();
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
    }

    #[test]
    fn render_only_while_visible_and_polls_when_paused() {
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        s.on_session_running(true);
        s.on_swapchains_created();
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
        s.on_event(Pause);
        assert_eq!(s.decide_next_action(), NextAction::PollEvents);
        s.on_event(Resume);
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
        s.on_event(TermWindow);
        assert_eq!(s.decide_next_action(), NextAction::PollEvents);
        s.on_event(InitWindow);
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
        // Focus changes never stop rendering or recreate anything.
        s.on_event(LostFocus);
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
    }

    #[test]
    fn session_stopping_returns_to_polling_and_ready_again_resumes() {
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        s.on_session_running(true);
        s.on_swapchains_created();
        s.on_session_running(false); // STOPPING handled
        assert_eq!(s.decide_next_action(), NextAction::PollEvents);
        s.on_session_running(true); // READY again
        assert_eq!(s.decide_next_action(), NextAction::RenderFrame);
        assert!(s.swapchains_created(), "swapchains are not recreated");
    }

    #[test]
    fn exiting_or_loss_pending_exits() {
        let mut s = run(&[InitWindow, Resume]);
        s.on_session_created();
        s.on_session_running(true);
        s.on_exit_requested();
        assert_eq!(s.decide_next_action(), NextAction::Exit);
    }

    #[test]
    fn exhaustive_orderings_never_create_before_ready() {
        // Every permutation of up to 4 events from the full alphabet: CreateSession must only
        // be returned when the window exists and the activity is resumed, and swapchains or
        // rendering must never be returned without a created, running session.
        let alphabet = [
            InitWindow,
            TermWindow,
            Resume,
            Pause,
            GainedFocus,
            LostFocus,
            Destroy,
        ];
        fn rec(depth: usize, alphabet: &[LifecycleEvent], s: AppLifecycle) {
            let a = s.decide_next_action();
            match a {
                NextAction::CreateSession => {
                    assert!(s.window_ready && s.resumed && !s.destroyed && !s.session_created)
                }
                NextAction::CreateSwapchains | NextAction::RenderFrame => {
                    assert!(s.session_created && s.session_running)
                }
                _ => {}
            }
            if depth == 0 {
                return;
            }
            for e in alphabet {
                let mut n = s;
                n.on_event(*e);
                rec(depth - 1, alphabet, n);
            }
        }
        rec(4, &alphabet, AppLifecycle::new());
    }
}
