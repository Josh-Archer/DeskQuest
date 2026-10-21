//! Installs the daemon (and optionally the tray) as systemd **user** services, so the daemon
//! starts with the graphical session and is restarted if it crashes.
//!
//! `linux-quest-host service install` writes `~/.config/systemd/user/linux-quest-host.service`
//! from the packaged unit, with `ExecStart` pointing at the running binary and the daemon
//! options given after `--`. It then runs `systemctl --user daemon-reload`. It only enables or
//! starts the unit when asked to (`--enable`, `--start`).
//!
//! Everything that decides what to write is pure and tested here; [`SystemdRunner`] is the
//! only part that runs `systemctl`.

use std::path::{Path, PathBuf};

/// The daemon's unit name.
pub const DAEMON_UNIT: &str = "linux-quest-host.service";
/// The tray's unit name.
pub const TRAY_UNIT: &str = "linux-quest-tray.service";

/// The unit shipped in the packages (`packaging/systemd/linux-quest-host.service`). Its
/// `ExecStart` points at `/usr/bin/linux-quest-host`.
pub const PACKAGED_DAEMON_UNIT: &str =
    include_str!("../../../packaging/systemd/linux-quest-host.service");

/// The tray unit (`packaging/systemd/linux-quest-tray.service`).
pub const PACKAGED_TRAY_UNIT: &str =
    include_str!("../../../packaging/systemd/linux-quest-tray.service");

/// Quotes one `ExecStart` word for systemd.
///
/// Plain words are left as they are. Anything with whitespace, quotes, backslashes or an
/// empty word is wrapped in double quotes with `"` and `\` escaped. `%` (specifiers) and `$`
/// (environment expansion) are always doubled so they reach the program literally.
pub fn systemd_quote(word: &str) -> String {
    let literal = word.replace('%', "%%").replace('$', "$$");
    let needs_quotes = literal.is_empty()
        || literal
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'));
    if !needs_quotes {
        return literal;
    }
    let mut quoted = String::with_capacity(literal.len() + 2);
    quoted.push('"');
    for c in literal.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The `ExecStart=` value for `exe` followed by `args`.
pub fn exec_line(exe: &str, args: &[String]) -> String {
    std::iter::once(exe)
        .chain(args.iter().map(String::as_str))
        .map(systemd_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replaces every `ExecStart=` line of `template` with one that runs `exec`.
///
/// Comment lines that mention the packaged path are kept: they explain how to override it.
fn with_exec_start(template: &str, exec: &str) -> String {
    let mut out = String::with_capacity(template.len() + exec.len());
    for line in template.lines() {
        if line.trim_start().starts_with("ExecStart=") {
            out.push_str("ExecStart=");
            out.push_str(exec);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The daemon unit that runs `exe daemon <daemon_args>`.
pub fn daemon_unit(exe: &str, daemon_args: &[String]) -> String {
    let mut args = vec!["daemon".to_string()];
    args.extend(daemon_args.iter().cloned());
    with_exec_start(PACKAGED_DAEMON_UNIT, &exec_line(exe, &args))
}

/// The tray unit that runs `exe tray`.
pub fn tray_unit(exe: &str) -> String {
    with_exec_start(PACKAGED_TRAY_UNIT, &exec_line(exe, &["tray".to_string()]))
}

/// `$XDG_CONFIG_HOME/systemd/user`, or `~/.config/systemd/user`.
pub fn user_unit_dir_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let config = match xdg_config_home.filter(|v| Path::new(v).is_absolute()) {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(home.filter(|h| !h.is_empty())?).join(".config"),
    };
    Some(config.join("systemd").join("user"))
}

/// [`user_unit_dir_from`] with the process environment.
pub fn user_unit_dir() -> Option<PathBuf> {
    user_unit_dir_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// One unit file to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    pub name: &'static str,
    pub path: PathBuf,
    pub contents: String,
}

/// What `service install` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub units: Vec<UnitFile>,
    /// Run `systemctl --user enable` on the units.
    pub enable: bool,
    /// Start (or restart) the units now.
    pub start: bool,
}

/// Options of `service install`, already parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// Also install the tray unit.
    pub tray: bool,
    pub enable: bool,
    pub start: bool,
    /// Replace unit files that differ from what would be written.
    pub force: bool,
    /// Options passed to `linux-quest-host daemon`.
    pub daemon_args: Vec<String>,
}

/// Builds the install plan. `exe` is the absolute path of the host binary.
pub fn plan_install(unit_dir: &Path, exe: &str, options: &InstallOptions) -> InstallPlan {
    let mut units = vec![UnitFile {
        name: DAEMON_UNIT,
        path: unit_dir.join(DAEMON_UNIT),
        contents: daemon_unit(exe, &options.daemon_args),
    }];
    if options.tray {
        units.push(UnitFile {
            name: TRAY_UNIT,
            path: unit_dir.join(TRAY_UNIT),
            contents: tray_unit(exe),
        });
    }
    InstallPlan {
        units,
        enable: options.enable,
        start: options.start,
    }
}

/// Daemon options that cannot work under systemd: the TUI needs a terminal.
pub fn check_daemon_args(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "--tui") {
        return Err("--tui needs a terminal and cannot run as a service".into());
    }
    if args.iter().any(|a| a == "daemon") {
        return Err(
            "pass only the daemon's options after `--` (the `daemon` subcommand is added for you)"
                .into(),
        );
    }
    Ok(())
}

/// A path is safe to put in a unit when it is absolute; a relative one would be resolved by
/// systemd from `/`, not from where the command was run.
pub fn check_exe(exe: &Path) -> Result<(), String> {
    if !exe.is_absolute() {
        return Err(format!("{} is not an absolute path", exe.display()));
    }
    Ok(())
}

/// Hint printed when the binary lives in a cargo build directory: `cargo clean` deletes it,
/// and the unit then fails to start.
pub fn build_dir_warning(exe: &Path) -> Option<String> {
    let in_target = exe.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c == "target" || c.ends_with("-target")
    });
    in_target.then(|| {
        format!(
            "{} is in a cargo build directory. `cargo clean` removes it and the service then \
             fails to start. For a lasting install, run \
             `cargo install --path crates/host --root ~/.local` and install the service from \
             ~/.local/bin/linux-quest-host",
            exe.display()
        )
    })
}

/// The `systemctl --user` argument lists `plan` needs, in order.
pub fn systemctl_steps(plan: &InstallPlan) -> Vec<Vec<String>> {
    let names: Vec<String> = plan.units.iter().map(|u| u.name.to_string()).collect();
    let mut steps = vec![vec!["daemon-reload".to_string()]];
    if plan.enable {
        let mut enable = vec!["enable".to_string()];
        enable.extend(names.iter().cloned());
        steps.push(enable);
    }
    if plan.start {
        // `restart` also starts a stopped unit, and picks up a changed ExecStart.
        let mut restart = vec!["restart".to_string()];
        restart.extend(names);
        steps.push(restart);
    }
    steps
}

/// The `systemctl --user` steps that undo an install of `units`.
pub fn uninstall_steps(units: &[&str]) -> Vec<Vec<String>> {
    let mut disable = vec!["disable".to_string(), "--now".to_string()];
    disable.extend(units.iter().map(|u| u.to_string()));
    vec![disable, vec!["daemon-reload".to_string()]]
}

/// Runs `systemctl --user`. A trait so the install flow is tested without systemd.
pub trait SystemdRunner {
    fn systemctl(&mut self, args: &[String]) -> Result<(), String>;
}

/// Runs the real `systemctl --user`.
pub struct SystemctlRunner;

impl SystemdRunner for SystemctlRunner {
    fn systemctl(&mut self, args: &[String]) -> Result<(), String> {
        let output = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("could not run systemctl: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// What happened to one unit file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

/// Writes the plan's unit files and runs its `systemctl` steps. Refuses to replace a unit
/// file with different contents unless `force`.
pub fn apply_install(
    plan: &InstallPlan,
    force: bool,
    runner: &mut dyn SystemdRunner,
) -> Result<Vec<(PathBuf, WriteOutcome)>, String> {
    // Check every file before writing any, so a refusal leaves nothing half-installed.
    for unit in &plan.units {
        if let Ok(existing) = std::fs::read_to_string(&unit.path) {
            if existing != unit.contents && !force {
                return Err(format!(
                    "{} already exists with different contents; pass --force to replace it \
                     (`service print` shows what would be written)",
                    unit.path.display()
                ));
            }
        }
    }
    let mut outcomes = Vec::with_capacity(plan.units.len());
    for unit in &plan.units {
        let outcome = match std::fs::read_to_string(&unit.path) {
            Ok(existing) if existing == unit.contents => WriteOutcome::Unchanged,
            Ok(_) => WriteOutcome::Updated,
            Err(_) => WriteOutcome::Created,
        };
        if outcome != WriteOutcome::Unchanged {
            write_atomically(&unit.path, &unit.contents)
                .map_err(|e| format!("could not write {}: {e}", unit.path.display()))?;
        }
        outcomes.push((unit.path.clone(), outcome));
    }
    for step in systemctl_steps(plan) {
        runner.systemctl(&step)?;
    }
    Ok(outcomes)
}

/// Disables and stops the units, then removes their files. Missing files are not an error.
pub fn apply_uninstall(
    unit_dir: &Path,
    runner: &mut dyn SystemdRunner,
) -> Result<Vec<PathBuf>, String> {
    let present: Vec<&str> = [DAEMON_UNIT, TRAY_UNIT]
        .into_iter()
        .filter(|name| unit_dir.join(name).exists())
        .collect();
    if present.is_empty() {
        return Ok(Vec::new());
    }
    let steps = uninstall_steps(&present);
    // Disable and stop first, while systemd still knows the units.
    runner.systemctl(&steps[0])?;
    let mut removed = Vec::new();
    for name in &present {
        let path = unit_dir.join(name);
        std::fs::remove_file(&path)
            .map_err(|e| format!("could not remove {}: {e}", path.display()))?;
        removed.push(path);
    }
    runner.systemctl(&steps[1])?;
    Ok(removed)
}

fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "unit path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn exec_start(unit: &str) -> Vec<&str> {
        unit.lines()
            .filter_map(|l| l.strip_prefix("ExecStart="))
            .collect()
    }

    #[test]
    fn plain_words_are_not_quoted_and_specials_are_escaped() {
        assert_eq!(
            systemd_quote("/usr/bin/linux-quest-host"),
            "/usr/bin/linux-quest-host"
        );
        assert_eq!(systemd_quote("--streams"), "--streams");
        assert_eq!(systemd_quote("/opt/my apps/lq"), "\"/opt/my apps/lq\"");
        assert_eq!(systemd_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(systemd_quote("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(systemd_quote(""), "\"\"");
        assert_eq!(systemd_quote("50%"), "50%%");
        assert_eq!(systemd_quote("$HOME"), "$$HOME");
    }

    #[test]
    fn daemon_unit_runs_this_binary_with_the_given_options() {
        let unit = daemon_unit(
            "/home/u/.local/bin/linux-quest-host",
            &strings(&["--streams", "3"]),
        );
        assert_eq!(
            exec_start(&unit),
            vec!["/home/u/.local/bin/linux-quest-host daemon --streams 3"]
        );
        // Everything else is the packaged unit, unchanged.
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
        assert!(unit.contains("KillSignal=SIGTERM"));
        assert_eq!(
            unit.lines().count(),
            PACKAGED_DAEMON_UNIT.lines().count(),
            "only the ExecStart line is replaced"
        );
    }

    #[test]
    fn packaged_units_have_exactly_one_exec_start() {
        assert_eq!(
            exec_start(PACKAGED_DAEMON_UNIT),
            vec!["/usr/bin/linux-quest-host daemon"]
        );
        assert_eq!(
            exec_start(PACKAGED_TRAY_UNIT),
            vec!["/usr/bin/linux-quest-host tray"]
        );
    }

    #[test]
    fn tray_unit_starts_after_the_daemon_and_runs_tray() {
        let unit = tray_unit("/x/linux-quest-host");
        assert_eq!(exec_start(&unit), vec!["/x/linux-quest-host tray"]);
        assert!(unit.contains("After=graphical-session.target linux-quest-host.service"));
    }

    #[test]
    fn unit_dir_follows_xdg_then_home() {
        assert_eq!(
            user_unit_dir_from(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/systemd/user"))
        );
        // A relative XDG_CONFIG_HOME is invalid by the spec and ignored.
        assert_eq!(
            user_unit_dir_from(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/systemd/user"))
        );
        assert_eq!(user_unit_dir_from(None, None), None);
        assert_eq!(user_unit_dir_from(None, Some("")), None);
    }

    #[test]
    fn daemon_args_reject_tui_and_a_repeated_subcommand() {
        assert!(check_daemon_args(&strings(&["--streams", "3", "--bitrate", "40"])).is_ok());
        assert!(check_daemon_args(&strings(&["--tui"])).is_err());
        assert!(check_daemon_args(&strings(&["daemon", "--streams", "3"])).is_err());
    }

    #[test]
    fn build_directories_are_flagged() {
        assert!(build_dir_warning(Path::new("/w/target/release/linux-quest-host")).is_some());
        assert!(
            build_dir_warning(Path::new("/w/wt-integ-target/release/linux-quest-host")).is_some()
        );
        assert!(build_dir_warning(Path::new("/home/u/.local/bin/linux-quest-host")).is_none());
        assert!(check_exe(Path::new("linux-quest-host")).is_err());
    }

    #[test]
    fn steps_enable_and_start_only_when_asked() {
        let dir = Path::new("/u");
        let only_write = plan_install(dir, "/b/lq", &InstallOptions::default());
        assert_eq!(
            systemctl_steps(&only_write),
            vec![strings(&["daemon-reload"])]
        );

        let all = plan_install(
            dir,
            "/b/lq",
            &InstallOptions {
                tray: true,
                enable: true,
                start: true,
                ..Default::default()
            },
        );
        assert_eq!(
            systemctl_steps(&all),
            vec![
                strings(&["daemon-reload"]),
                strings(&["enable", DAEMON_UNIT, TRAY_UNIT]),
                strings(&["restart", DAEMON_UNIT, TRAY_UNIT]),
            ]
        );
        assert_eq!(
            uninstall_steps(&[DAEMON_UNIT]),
            vec![
                strings(&["disable", "--now", DAEMON_UNIT]),
                strings(&["daemon-reload"])
            ]
        );
    }

    #[derive(Default)]
    struct Recorder(Vec<Vec<String>>);

    impl SystemdRunner for Recorder {
        fn systemctl(&mut self, args: &[String]) -> Result<(), String> {
            self.0.push(args.to_vec());
            Ok(())
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lq-service-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn install_writes_units_reloads_and_refuses_to_clobber_without_force() {
        let dir = temp_dir("install");
        let options = InstallOptions {
            tray: true,
            daemon_args: strings(&["--streams", "3"]),
            ..Default::default()
        };
        let plan = plan_install(&dir, "/b/linux-quest-host", &options);
        let mut runner = Recorder::default();

        let outcomes = apply_install(&plan, false, &mut runner).unwrap();
        assert_eq!(
            outcomes.iter().map(|(_, o)| *o).collect::<Vec<_>>(),
            vec![WriteOutcome::Created, WriteOutcome::Created]
        );
        assert_eq!(runner.0, vec![strings(&["daemon-reload"])]);
        let written = std::fs::read_to_string(dir.join(DAEMON_UNIT)).unwrap();
        assert!(written.contains("ExecStart=/b/linux-quest-host daemon --streams 3"));

        // Same plan again: nothing to write.
        let again = apply_install(&plan, false, &mut runner).unwrap();
        assert!(again.iter().all(|(_, o)| *o == WriteOutcome::Unchanged));

        // Different options: refused, and nothing is touched.
        let other = plan_install(&dir, "/b/linux-quest-host", &InstallOptions::default());
        let err = apply_install(&other, false, &mut runner).unwrap_err();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.join(DAEMON_UNIT)).unwrap(),
            written
        );
        let forced = apply_install(&other, true, &mut runner).unwrap();
        assert_eq!(forced[0].1, WriteOutcome::Updated);

        // Uninstall stops, removes both files and reloads.
        let mut runner = Recorder::default();
        let removed = apply_uninstall(&dir, &mut runner).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!dir.join(DAEMON_UNIT).exists() && !dir.join(TRAY_UNIT).exists());
        assert_eq!(
            runner.0,
            vec![
                strings(&["disable", "--now", DAEMON_UNIT, TRAY_UNIT]),
                strings(&["daemon-reload"])
            ]
        );
        // Nothing left: uninstall is a no-op.
        assert!(apply_uninstall(&dir, &mut Recorder::default())
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
