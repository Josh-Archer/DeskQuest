//! Wayland clipboard backend shelling out to `wl-copy` and `wl-paste`.

use crate::clipboard::backend::ClipboardBackend;
use crate::clipboard::cmd::{ClipboardCommandRunner, SystemCommandRunner};
use crate::error::InputError;

/// Wayland clipboard backend using `wl-clipboard` utilities.
pub struct WaylandClipboardBackend<R: ClipboardCommandRunner = SystemCommandRunner> {
    runner: R,
}

impl WaylandClipboardBackend<SystemCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: SystemCommandRunner::new(),
        }
    }
}

impl Default for WaylandClipboardBackend<SystemCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ClipboardCommandRunner> WaylandClipboardBackend<R> {
    pub fn with_runner(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: ClipboardCommandRunner> ClipboardBackend for WaylandClipboardBackend<R> {
    fn get_text(&self) -> Result<String, InputError> {
        let output = self
            .runner
            .run_command("wl-paste", &["--no-newline"], None)
            .map_err(|e| InputError::ClipboardError(format!("Failed to execute wl-paste: {e}")))?;

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
