//! Command execution abstraction for shelling out to Wayland and X11 clipboard tools.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// Result of a command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    pub fn failure(code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code: code,
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }
}

/// Interface for executing host system processes or mock equivalents.
pub trait ClipboardCommandRunner: Send + Sync {
    /// Executes a command with optional standard input bytes.
    fn run_command(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<CommandOutput, io::Error>;

    /// Checks if a program is present in `PATH`.
    fn is_program_available(&self, program: &str) -> bool;
}

/// How long a clipboard tool may run before it is killed. `wl-paste` can wait forever for
/// keyboard focus on compositors without a data-control protocol (GNOME), and a caller that
/// polls the clipboard must not hang with it.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

/// Real system process execution runner. A command that runs longer than its timeout is
/// killed and reported as `ErrorKind::TimedOut`.
#[derive(Debug, Clone)]
pub struct SystemCommandRunner {
    timeout: Duration,
}

impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemCommandRunner {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

/// Reads `pipe` to the end on a thread and sends what it read.
fn drain<P: Read + Send + 'static>(pipe: Option<P>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    } else {
        let _ = tx.send(Vec::new());
    }
    rx
}

impl ClipboardCommandRunner for SystemCommandRunner {
    fn run_command(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<CommandOutput, io::Error> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if input.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        let mut child = cmd.spawn()?;
        let deadline = Instant::now() + self.timeout;

        if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
            // A thread, so a child that never reads cannot block us on a full pipe.
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            });
        }
        // Read both pipes while waiting, so a chatty child cannot fill one and stall.
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());

        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{program} did not finish within {:?}", self.timeout),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // A tool that forks into the background (wl-copy) may leave its pipes open; take what
        // arrived instead of waiting for them to close.
        let grace = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(50));
        Ok(CommandOutput {
            status_code: status.code().unwrap_or(-1),
            stdout: stdout.recv_timeout(grace).unwrap_or_default(),
            stderr: stderr.recv_timeout(grace).unwrap_or_default(),
        })
    }

    fn is_program_available(&self, program: &str) -> bool {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let bin = dir.join(program);
                if bin.is_file() {
                    return true;
                }
            }
        }
        false
    }
}

/// Recorded call tuple: `(program, args, optional_input_bytes)`.
pub type CommandCallRecord = (String, Vec<String>, Option<Vec<u8>>);

/// Mock command runner for unit tests.
#[derive(Debug, Default)]
pub struct MockCommandRunner {
    pub available_programs: Mutex<HashMap<String, bool>>,
    pub responses: Mutex<HashMap<(String, Vec<String>), CommandOutput>>,
    pub recorded_calls: Mutex<Vec<CommandCallRecord>>,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_available(&self, program: impl Into<String>, available: bool) {
        self.available_programs
            .lock()
            .unwrap()
            .insert(program.into(), available);
    }

    pub fn set_response(&self, program: impl Into<String>, args: &[&str], output: CommandOutput) {
        let args_vec: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.responses
            .lock()
            .unwrap()
            .insert((program.into(), args_vec), output);
    }

    pub fn recorded_calls(&self) -> Vec<(String, Vec<String>, Option<Vec<u8>>)> {
        self.recorded_calls.lock().unwrap().clone()
    }
}

impl ClipboardCommandRunner for MockCommandRunner {
    fn run_command(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<CommandOutput, io::Error> {
        let args_vec: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.recorded_calls.lock().unwrap().push((
            program.to_string(),
            args_vec.clone(),
            input.map(|b| b.to_vec()),
        ));

        if let Some(resp) = self
            .responses
            .lock()
            .unwrap()
            .get(&(program.to_string(), args_vec))
        {
            return Ok(resp.clone());
        }

        Ok(CommandOutput::failure(
            127,
            format!("Mock: no response registered for {program}"),
        ))
    }

    fn is_program_available(&self, program: &str) -> bool {
        self.available_programs
            .lock()
            .unwrap()
            .get(program)
            .copied()
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod system_runner_tests {
    use super::*;

    #[test]
    fn output_and_status_are_returned() {
        let out = SystemCommandRunner::new()
            .run_command("sh", &["-c", "echo hi; echo oops >&2; exit 3"], None)
            .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"oops\n");
        assert_eq!(out.status_code, 3);
        let echoed = SystemCommandRunner::new()
            .run_command("cat", &[], Some(b"piped"))
            .unwrap();
        assert_eq!(echoed.stdout, b"piped");
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_timeout() {
        let started = Instant::now();
        let err = SystemCommandRunner::with_timeout(Duration::from_millis(150))
            .run_command("sleep", &["30"], None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}
