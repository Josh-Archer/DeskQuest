//! X11 clipboard backend shelling out to `xclip` or `xsel`.

use crate::clipboard::backend::ClipboardBackend;
use crate::clipboard::cmd::{ClipboardCommandRunner, SystemCommandRunner};
use crate::error::InputError;

/// Which X11 CLI utility to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11ClipboardTool {
    Xclip,
    Xsel,
}

/// X11 clipboard backend using `xclip` or `xsel`.
pub struct X11ClipboardBackend<R: ClipboardCommandRunner = SystemCommandRunner> {
    runner: R,
    preferred_tool: Option<X11ClipboardTool>,
}

impl X11ClipboardBackend<SystemCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: SystemCommandRunner::new(),
            preferred_tool: None,
        }
    }
}

impl Default for X11ClipboardBackend<SystemCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ClipboardCommandRunner> X11ClipboardBackend<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            preferred_tool: None,
        }
    }

    pub fn with_tool(runner: R, tool: X11ClipboardTool) -> Self {
        Self {
            runner,
            preferred_tool: Some(tool),
        }
    }

    fn detect_tool(&self) -> Result<X11ClipboardTool, InputError> {
        if let Some(tool) = self.preferred_tool {
            return Ok(tool);
        }

        if self.runner.is_program_available("xclip") {
            Ok(X11ClipboardTool::Xclip)
        } else if self.runner.is_program_available("xsel") {
            Ok(X11ClipboardTool::Xsel)
        } else {
            Err(InputError::ClipboardError(
                "Neither 'xclip' nor 'xsel' is installed or available in PATH".to_string(),
            ))
        }
    }
}

impl<R: ClipboardCommandRunner> ClipboardBackend for X11ClipboardBackend<R> {
    fn get_text(&self) -> Result<String, InputError> {
        let tool = self.detect_tool()?;
        let (prog, args) = match tool {
            X11ClipboardTool::Xclip => ("xclip", vec!["-selection", "clipboard", "-o"]),
            X11ClipboardTool::Xsel => ("xsel", vec!["--clipboard", "--output"]),
        };

        let output = self
            .runner
            .run_command(prog, &args, None)
            .map_err(|e| InputError::ClipboardError(format!("Failed to execute {prog}: {e}")))?;

        if output.status_code != 0 {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            // xclip exits with 1 when clipboard target is empty or not yet set
            if output.status_code == 1
                && (stderr_str.contains("target STRING not available")
                    || stderr_str.contains("target UTF8_STRING not available")
                    || stderr_str.is_empty())
            {
                return Ok(String::new());
            }
            return Err(InputError::ClipboardError(format!(
                "{prog} exited with status {}: {stderr_str}",
                output.status_code
            )));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| InputError::ClipboardError(format!("Invalid UTF-8 in X11 clipboard: {e}")))
    }

    fn set_text(&mut self, text: &str) -> Result<(), InputError> {
        let tool = self.detect_tool()?;
        let (prog, args) = match tool {
            X11ClipboardTool::Xclip => ("xclip", vec!["-selection", "clipboard"]),
            X11ClipboardTool::Xsel => ("xsel", vec!["--clipboard", "--input"]),
        };

        let output = self
            .runner
            .run_command(prog, &args, Some(text.as_bytes()))
            .map_err(|e| InputError::ClipboardError(format!("Failed to execute {prog}: {e}")))?;

        if output.status_code != 0 {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            return Err(InputError::ClipboardError(format!(
                "{prog} exited with status {}: {stderr_str}",
                output.status_code
            )));
        }

        Ok(())
    }

    fn is_available(&self) -> bool {
        let has_display = std::env::var_os("DISPLAY")
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        has_display
            && (self.runner.is_program_available("xclip")
                || self.runner.is_program_available("xsel"))
    }

    fn name(&self) -> &'static str {
        match self.preferred_tool {
            Some(X11ClipboardTool::Xclip) => "x11 (xclip)",
            Some(X11ClipboardTool::Xsel) => "x11 (xsel)",
            None => "x11 (auto: xclip/xsel)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::cmd::{CommandOutput, MockCommandRunner};

    #[test]
    fn test_x11_xclip_get_and_set() {
        let runner = MockCommandRunner::new();
        runner.set_available("xclip", true);
        runner.set_response(
            "xclip",
            &["-selection", "clipboard", "-o"],
            CommandOutput::success(b"X11 clipboard data"),
        );
        runner.set_response(
            "xclip",
            &["-selection", "clipboard"],
            CommandOutput::success(b""),
        );

        let mut backend = X11ClipboardBackend::with_tool(runner, X11ClipboardTool::Xclip);
        let text = backend.get_text().unwrap();
        assert_eq!(text, "X11 clipboard data");

        backend.set_text("Updated X11 text").unwrap();
        let calls = backend.runner.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "xclip");
        assert_eq!(calls[1].2, Some(b"Updated X11 text".to_vec()));
    }

    #[test]
    fn test_x11_xsel_fallback() {
        let runner = MockCommandRunner::new();
        runner.set_available("xclip", false);
        runner.set_available("xsel", true);
        runner.set_response(
            "xsel",
            &["--clipboard", "--output"],
            CommandOutput::success(b"xsel text"),
        );

        let backend = X11ClipboardBackend::with_runner(runner);
        let text = backend.get_text().unwrap();
        assert_eq!(text, "xsel text");
    }

    #[test]
    fn test_x11_empty_clipboard() {
        let runner = MockCommandRunner::new();
        runner.set_available("xclip", true);
        runner.set_response(
            "xclip",
            &["-selection", "clipboard", "-o"],
            CommandOutput::failure(1, b"Error: target STRING not available\n"),
        );

        let backend = X11ClipboardBackend::with_tool(runner, X11ClipboardTool::Xclip);
        let text = backend.get_text().unwrap();
        assert_eq!(text, "");
    }
}
