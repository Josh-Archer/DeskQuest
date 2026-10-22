//! Wayland clipboard backend shelling out to `wl-copy` and `wl-paste`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::clipboard::backend::ClipboardBackend;
use crate::clipboard::cmd::{ClipboardCommandRunner, SystemCommandRunner};
use crate::error::InputError;

/// After `wl-paste` times out, the clipboard is not read again for this long. On GNOME it
/// waits for keyboard focus it never gets, so every poll would otherwise cost the timeout.
pub const UNRESPONSIVE_BACKOFF: Duration = Duration::from_secs(60);

/// Set to `1` to read the clipboard with `wl-paste` on GNOME anyway (see
/// [`reads_take_focus`]).
pub const READS_ON_GNOME_ENV: &str = "LINUX_QUEST_WAYLAND_CLIPBOARD_READS";

/// Whether reading the clipboard takes keyboard focus in this session. GNOME's compositor has
/// no data-control protocol, so `wl-paste` gets the clipboard by popping up a tiny surface to
/// take keyboard focus. Polled every 200 ms, that swallows keys the user is typing. `env`
/// reads an environment variable.
pub fn reads_take_focus(env: impl Fn(&str) -> Option<String>) -> bool {
    let gnome = ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
        .iter()
        .filter_map(|key| env(key))
        .any(|value| value.to_ascii_lowercase().contains("gnome"));
    gnome && env(READS_ON_GNOME_ENV).as_deref() != Some("1")
}

/// Wayland clipboard backend using `wl-clipboard` utilities.
pub struct WaylandClipboardBackend<R: ClipboardCommandRunner = SystemCommandRunner> {
    runner: R,
    /// Whether `get_text` may run `wl-paste`. Off where that takes keyboard focus, so the
    /// host's clipboard poll does not eat the user's typing; writes still work.
    reads: bool,
    /// `wl-paste` timed out; reads fail fast until then.
    unresponsive_until: Mutex<Option<Instant>>,
    /// The timeout was logged (once per process run).
    warned: std::sync::atomic::AtomicBool,
}

impl WaylandClipboardBackend<SystemCommandRunner> {
    /// Reads the clipboard only where that leaves keyboard focus alone (not on GNOME, unless
    /// [`READS_ON_GNOME_ENV`] is `1`).
    pub fn new() -> Self {
        let reads = !reads_take_focus(|key| std::env::var(key).ok());
        if !reads {
            tracing::info!(
                "GNOME gives wl-paste the clipboard only with keyboard focus, which would steal \
                 keystrokes every poll: not reading the desktop clipboard (headset-to-desktop \
                 copies still work; set {READS_ON_GNOME_ENV}=1 to read it anyway)"
            );
        }
        Self::with_runner(SystemCommandRunner::new()).with_reads(reads)
    }
}

impl Default for WaylandClipboardBackend<SystemCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ClipboardCommandRunner> WaylandClipboardBackend<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            reads: true,
            unresponsive_until: Mutex::new(None),
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether `get_text` may run `wl-paste` at all.
    pub fn with_reads(mut self, reads: bool) -> Self {
        self.reads = reads;
        self
    }
}

impl<R: ClipboardCommandRunner> ClipboardBackend for WaylandClipboardBackend<R> {
    fn get_text(&self) -> Result<String, InputError> {
        if !self.reads {
            return Err(InputError::ClipboardError(
                "reading the clipboard would take keyboard focus here; reads are off".into(),
            ));
        }
        let resting = self
            .unresponsive_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until);
        if resting {
            return Err(InputError::ClipboardError(
                "wl-paste did not answer recently; not reading the clipboard for now".into(),
            ));
        }
        let output = match self.runner.run_command("wl-paste", &["--no-newline"], None) {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if let Ok(mut until) = self.unresponsive_until.lock() {
                    *until = Some(Instant::now() + UNRESPONSIVE_BACKOFF);
                }
                if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        "wl-paste does not answer (the compositor gives it no clipboard access \
                         without focus, as GNOME does); reading the clipboard is retried once a \
                         minute"
                    );
                }
                return Err(InputError::ClipboardError(format!("wl-paste: {e}")));
            }
            Err(e) => {
                return Err(InputError::ClipboardError(format!(
                    "Failed to execute wl-paste: {e}"
                )))
            }
        };

        if output.status_code != 0 {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            // wl-paste returns exit code 1 when no selection exists / nothing copied
            if output.status_code == 1
                && (stderr_str.contains("Nothing is copied")
                    || stderr_str.contains("no selection")
                    || stderr_str.is_empty())
            {
                return Ok(String::new());
            }
            return Err(InputError::ClipboardError(format!(
                "wl-paste exited with status {}: {stderr_str}",
                output.status_code
            )));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| InputError::ClipboardError(format!("Invalid UTF-8 in clipboard: {e}")))
    }

    fn set_text(&mut self, text: &str) -> Result<(), InputError> {
        let output = self
            .runner
            .run_command("wl-copy", &[], Some(text.as_bytes()))
            .map_err(|e| InputError::ClipboardError(format!("Failed to execute wl-copy: {e}")))?;

        if output.status_code != 0 {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            return Err(InputError::ClipboardError(format!(
                "wl-copy exited with status {}: {stderr_str}",
                output.status_code
            )));
        }

        Ok(())
    }

    fn is_available(&self) -> bool {
        let has_wayland_display = std::env::var_os("WAYLAND_DISPLAY")
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        has_wayland_display
            && self.runner.is_program_available("wl-copy")
            && self.runner.is_program_available("wl-paste")
    }

    fn name(&self) -> &'static str {
        "wayland (wl-clipboard)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::cmd::{CommandOutput, MockCommandRunner};

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn reads_take_focus_on_gnome_only() {
        assert!(reads_take_focus(env(&[(
            "XDG_CURRENT_DESKTOP",
            "ubuntu:GNOME"
        )])));
        assert!(reads_take_focus(env(&[("XDG_SESSION_DESKTOP", "gnome")])));
        assert!(!reads_take_focus(env(&[("XDG_CURRENT_DESKTOP", "KDE")])));
        assert!(!reads_take_focus(env(&[("XDG_CURRENT_DESKTOP", "sway")])));
        assert!(!reads_take_focus(env(&[])));
        assert!(!reads_take_focus(env(&[
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            (READS_ON_GNOME_ENV, "1"),
        ])));
    }

    #[test]
    fn with_reads_off_wl_paste_never_runs_but_writes_still_do() {
        let runner = MockCommandRunner::new();
        runner.set_response(
            "wl-paste",
            &["--no-newline"],
            CommandOutput::success(b"would steal focus"),
        );
        runner.set_response("wl-copy", &[], CommandOutput::success(b""));
        let mut backend = WaylandClipboardBackend::with_runner(runner).with_reads(false);
        assert!(backend.get_text().is_err());
        assert!(backend.set_text("from the headset").is_ok());
    }

    #[test]
    fn test_wayland_clipboard_get_text_success() {
        let runner = MockCommandRunner::new();
        runner.set_response(
            "wl-paste",
            &["--no-newline"],
            CommandOutput::success(b"Wayland content"),
        );

        let backend = WaylandClipboardBackend::with_runner(runner);
        let text = backend.get_text().unwrap();
        assert_eq!(text, "Wayland content");
    }

    #[test]
    fn test_wayland_clipboard_empty_nothing_copied() {
        let runner = MockCommandRunner::new();
        runner.set_response(
            "wl-paste",
            &["--no-newline"],
            CommandOutput::failure(1, b"Nothing is copied\n"),
        );

        let backend = WaylandClipboardBackend::with_runner(runner);
        let text = backend.get_text().unwrap();
        assert_eq!(text, "");
    }

    /// Times out every call, counting them.
    struct HangingRunner(std::sync::atomic::AtomicU32);

    impl ClipboardCommandRunner for HangingRunner {
        fn run_command(
            &self,
            _program: &str,
            _args: &[&str],
            _input: Option<&[u8]>,
        ) -> Result<CommandOutput, std::io::Error> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "hung"))
        }
        fn is_program_available(&self, _program: &str) -> bool {
            true
        }
    }

    #[test]
    fn a_hanging_wl_paste_is_not_run_again_until_the_backoff_ends() {
        let backend = WaylandClipboardBackend::with_runner(HangingRunner(Default::default()));
        assert!(backend.get_text().is_err());
        assert!(backend.get_text().is_err());
        assert!(backend.get_text().is_err());
        assert_eq!(
            backend.runner.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "later reads fail fast without running it"
        );
    }

    #[test]
    fn test_wayland_clipboard_set_text_success() {
        let runner = MockCommandRunner::new();
        runner.set_response("wl-copy", &[], CommandOutput::success(b""));

        let mut backend = WaylandClipboardBackend::with_runner(runner);
        backend.set_text("New clipboard text").unwrap();

        let calls = backend.runner.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "wl-copy");
        assert_eq!(calls[0].2, Some(b"New clipboard text".to_vec()));
    }

    #[test]
    fn test_wayland_clipboard_error_handling() {
        let runner = MockCommandRunner::new();
        runner.set_response(
            "wl-copy",
            &[],
            CommandOutput::failure(2, b"Compositor rejected copy\n"),
        );

        let mut backend = WaylandClipboardBackend::with_runner(runner);
        let err = backend.set_text("Sample").unwrap_err();
        match err {
            InputError::ClipboardError(msg) => {
                assert!(msg.contains("Compositor rejected copy"));
            }
            other => panic!("Unexpected error: {:?}", other),
        }
    }
}
