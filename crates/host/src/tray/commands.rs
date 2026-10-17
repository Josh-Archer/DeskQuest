//! Builds the external commands behind the tray menu, and runs them.

use async_trait::async_trait;

/// systemd user unit the tray restarts.
pub const SERVICE_NAME: &str = "linux-quest-host";

/// Terminal used when `$TERMINAL` is unset or blank (Debian/Ubuntu alternatives).
pub const FALLBACK_TERMINAL: &str = "x-terminal-emulator";

/// A program plus arguments. Kept as data so the argument lists can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Opens `<host_exe> dashboard` in a terminal emulator.
///
/// `terminal_env` is the value of `$TERMINAL`. The terminal must accept `-e <command>`.
pub fn dashboard_command(terminal_env: Option<&str>, host_exe: &str) -> LaunchCommand {
    let program = terminal_env
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(FALLBACK_TERMINAL);
    LaunchCommand {
        program: program.to_string(),
        args: vec![
            "-e".to_string(),
            host_exe.to_string(),
            "dashboard".to_string(),
        ],
    }
}

/// Queues a restart of the user service.
///
/// `--no-block` returns once systemd has accepted the job. A blocking restart would
/// wait for the old daemon to stop, and that daemon's cgroup could take this
/// `systemctl` process down with it.
pub fn restart_command() -> LaunchCommand {
    LaunchCommand {
        program: "systemctl".to_string(),
        args: vec![
            "--user".to_string(),
            "--no-block".to_string(),
            "restart".to_string(),
            SERVICE_NAME.to_string(),
        ],
    }
}

/// Runs launch commands. The trait lets the controller be tested without processes.
#[async_trait]
pub trait CommandRunner: Send {
    /// Starts the command and does not wait for it (used for the dashboard terminal).
    async fn spawn_detached(&mut self, cmd: &LaunchCommand) -> Result<(), String>;

    /// Runs the command to completion. A spawn failure or non-zero exit is an error.
    async fn run_to_completion(&mut self, cmd: &LaunchCommand) -> Result<(), String>;
}

/// Runs commands as real child processes through tokio.
pub struct ProcessRunner;

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn spawn_detached(&mut self, cmd: &LaunchCommand) -> Result<(), String> {
        tokio::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_child| ())
            .map_err(|e| format!("could not start {}: {e}", cmd.program))
    }

    async fn run_to_completion(&mut self, cmd: &LaunchCommand) -> Result<(), String> {
        let output = tokio::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("could not run {}: {e}", cmd.program))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} exited with {}: {}",
                cmd.program,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_uses_fallback_terminal_when_env_unset() {
        let cmd = dashboard_command(None, "/usr/bin/linux-quest-host");
        assert_eq!(cmd.program, FALLBACK_TERMINAL);
        assert_eq!(
            cmd.args,
            ["-e", "/usr/bin/linux-quest-host", "dashboard"].map(String::from)
        );
    }

    #[test]
    fn dashboard_uses_terminal_env_when_set() {
        let cmd = dashboard_command(Some("kitty"), "/home/u/.cargo/bin/linux-quest-host");
        assert_eq!(cmd.program, "kitty");
        assert_eq!(cmd.args[0], "-e");
        assert_eq!(cmd.args[2], "dashboard");
    }

    #[test]
    fn blank_terminal_env_falls_back() {
        for blank in ["", "   "] {
            let cmd = dashboard_command(Some(blank), "linux-quest-host");
            assert_eq!(cmd.program, FALLBACK_TERMINAL);
        }
    }

    #[test]
    fn restart_targets_user_unit_without_blocking() {
        let cmd = restart_command();
        assert_eq!(cmd.program, "systemctl");
        assert_eq!(
            cmd.args,
            ["--user", "--no-block", "restart", "linux-quest-host"].map(String::from)
        );
    }
}
