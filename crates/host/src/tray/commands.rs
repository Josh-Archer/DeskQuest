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

/// Splits `$TERMINAL` into a program and leading arguments, like a shell would but
/// without running one: quotes and backslashes group words, nothing is expanded.
/// Returns `None` when the value is blank or has unbalanced quotes.
fn split_terminal(value: &str) -> Option<Vec<String>> {
    let words = shlex::split(value)?;
    if words.is_empty() || words[0].is_empty() {
        None
    } else {
        Some(words)
    }
}

/// Opens `<host_exe> dashboard` in a terminal emulator.
///
/// `terminal_env` is the value of `$TERMINAL`. It may carry arguments (`kitty --class x`);
/// they are split shell-words style and passed directly, never through a shell. A blank or
/// unparsable value falls back to [`FALLBACK_TERMINAL`].
///
/// The command is appended as `-e <host_exe> dashboard`. `-e` is deprecated in some
/// terminals (for example gnome-terminal wants `--`) but is still accepted by most of
/// them (xterm, kitty, alacritty, konsole, x-terminal-emulator). A terminal that needs a
/// different flag can be wrapped in a script and pointed to by `$TERMINAL`.
pub fn dashboard_command(terminal_env: Option<&str>, host_exe: &str) -> LaunchCommand {
    let mut words = terminal_env
        .and_then(split_terminal)
        .unwrap_or_else(|| vec![FALLBACK_TERMINAL.to_string()]);
    let program = words.remove(0);
    words.extend([
        "-e".to_string(),
        host_exe.to_string(),
        "dashboard".to_string(),
    ]);
    LaunchCommand {
        program,
        args: words,
    }
}

/// Name of the desktop app's binary.
pub const APP_BINARY: &str = "linux-quest";

/// Starts the desktop app: the `linux-quest` next to `host_exe` when there is one (a cargo
/// build or an install puts both in one directory), else the one on `$PATH`. `exists` is
/// injected for testing.
pub fn app_command(host_exe: &str, exists: impl Fn(&std::path::Path) -> bool) -> LaunchCommand {
    let sibling = std::path::Path::new(host_exe)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(APP_BINARY))
        .filter(|path| exists(path));
    LaunchCommand {
        program: sibling.map_or_else(|| APP_BINARY.to_string(), |path| path.display().to_string()),
        args: Vec::new(),
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
            .map(|mut child| {
                // Wait on the child in the background so it does not stay a zombie
                // after the terminal is closed.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            })
            .map_err(|e| format!("could not start {}: {e}", cmd.program))
    }

    async fn run_to_completion(&mut self, cmd: &LaunchCommand) -> Result<(), String> {
        let output = tokio::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            // The caller may time out and drop this future; do not leave the child behind.
            .kill_on_drop(true)
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
    fn terminal_with_arguments_is_split_without_a_shell() {
        let cmd = dashboard_command(Some("kitty --class x"), "/h");
        assert_eq!(cmd.program, "kitty");
        assert_eq!(
            cmd.args,
            ["--class", "x", "-e", "/h", "dashboard"].map(String::from)
        );
    }

    #[test]
    fn terminal_quotes_group_words_and_nothing_expands() {
        let cmd = dashboard_command(Some("'/opt/my term/kitty' --title \"a b\" $HOME"), "/h");
        assert_eq!(cmd.program, "/opt/my term/kitty");
        assert_eq!(
            cmd.args,
            ["--title", "a b", "$HOME", "-e", "/h", "dashboard"].map(String::from)
        );
    }

    #[test]
    fn unparsable_terminal_falls_back() {
        let cmd = dashboard_command(Some("kitty 'oops"), "/h");
        assert_eq!(cmd.program, FALLBACK_TERMINAL);
        assert_eq!(cmd.args, ["-e", "/h", "dashboard"].map(String::from));
    }

    #[test]
    fn the_app_next_to_the_host_binary_wins_over_path() {
        let cmd = app_command("/home/u/.local/bin/linux-quest-host", |p| {
            p == std::path::Path::new("/home/u/.local/bin/linux-quest")
        });
        assert_eq!(cmd.program, "/home/u/.local/bin/linux-quest");
        assert!(cmd.args.is_empty());
        assert_eq!(
            app_command("/usr/bin/linux-quest-host", |_| false).program,
            "linux-quest"
        );
        assert_eq!(
            app_command("linux-quest-host", |_| true).program,
            "linux-quest"
        );
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
