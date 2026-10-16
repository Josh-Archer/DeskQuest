//! Command execution abstraction for shelling out to Wayland and X11 clipboard tools.

use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;

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

/// Real system process execution runner.
#[derive(Debug, Default, Clone)]
pub struct SystemCommandRunner;

impl SystemCommandRunner {
    pub fn new() -> Self {
        Self
    }
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

        if let Some(data) = input {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data);
            }
        }

        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            status_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
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
