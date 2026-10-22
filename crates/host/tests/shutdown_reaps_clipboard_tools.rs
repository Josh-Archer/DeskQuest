//! Issue #113: a hung `wl-paste` must not block daemon shutdown or outlive it.
//!
//! Both tests put stub `wl-copy`/`wl-paste` scripts first in `PATH`. The stub `wl-paste`
//! sleeps forever (and forks a grandchild that does too), like the real one on GNOME when it
//! waits for keyboard focus.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A sleep length unique to this test process, so its stub processes can be found in /proc.
fn marker(salt: u32) -> String {
    format!(
        "{}",
        8_000_000 + salt * 100_000 + (std::process::id() % 100_000)
    )
}

fn stub_dir(name: &str, marker: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("lq-113-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paste = dir.join("wl-paste");
    std::fs::write(
        &paste,
        format!("#!/bin/sh\nsleep {marker} &\nsleep {marker}\n"),
    )
    .unwrap();
    let copy = dir.join("wl-copy");
    std::fs::write(&copy, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    for f in [&paste, &copy] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(paths).unwrap()
}

/// Live (non-zombie) processes whose command line contains `marker`.
fn live_with_marker(marker: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        let zombie = stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
        if !zombie && String::from_utf8_lossy(&cmdline).contains(marker) {
            found.push(name);
        }
    }
    found
}

fn assert_no_stub_left(marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !live_with_marker(marker).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        live_with_marker(marker).is_empty(),
        "stub wl-paste processes outlived shutdown: {:?}",
        live_with_marker(marker)
    );
}

/// The clipboard service, stopped the way the daemon stops it, ends promptly with a hung
/// `wl-paste` and leaves none of its processes behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_service_stops_promptly_with_a_hung_wl_paste() {
    let marker = marker(1);
    let dir = stub_dir("service", &marker);
    // This test binary has only this test touching the environment.
    std::env::set_var("PATH", path_with(&dir));
    std::env::set_var("WAYLAND_DISPLAY", "lq-test-wayland");

    let mut service =
        linux_quest_host::HostClipboardService::bind_auto("127.0.0.1:0".parse().unwrap())
            .await
            .expect("control listener binds");
    let running = service.running_flag();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = service.run() => {}
            _ = shutdown_rx.recv() => {
                running.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });

    // Let it poll (and hang in) the stub a few times.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let started = Instant::now();
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("clipboard service did not stop within 3 s")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_no_stub_left(&marker);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real daemon exits on SIGTERM within a few seconds with a hung `wl-paste`, and no
/// `wl-paste` is left running. Skipped when the daemon cannot start here (control port busy).
#[test]
fn daemon_exits_on_sigterm_and_leaves_no_wl_paste_behind() {
    for (salt, signal) in [(2, libc::SIGTERM), (3, libc::SIGINT)] {
        let marker = marker(salt);
        let dir = stub_dir(&format!("daemon-{salt}"), &marker);
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_linux-quest-host"))
            .args(["daemon", "--no-video", "--no-advertise", "--no-adb"])
            .env("PATH", path_with(&dir))
            .env("WAYLAND_DISPLAY", "lq-test-wayland")
            .env("LINUX_QUEST_RUNTIME_DIR", &runtime)
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start daemon");

        // Wait until it is serving IPC and polling the clipboard.
        let sock_deadline = Instant::now() + Duration::from_secs(20);
        let mut ready = false;
        while Instant::now() < sock_deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if std::fs::read_dir(&runtime)
                .map(|mut d| {
                    d.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "sock")))
                })
                .unwrap_or(false)
                && !live_with_marker(&marker).is_empty()
            {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("daemon did not start in this environment; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let started = Instant::now();
        // SAFETY: kill(2) on the child we spawned.
        unsafe {
            libc::kill(child.id() as libc::pid_t, signal);
        }
        let mut exited = None;
        while started.elapsed() < Duration::from_secs(8) {
            if let Some(status) = child.try_wait().unwrap() {
                exited = Some(status);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let Some(status) = exited else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon ignored signal {signal} for 8 s");
        };
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "daemon took {:?} to exit",
            started.elapsed()
        );
        assert!(
            status.success(),
            "daemon exit status after signal {signal}: {status}"
        );
        assert_no_stub_left(&marker);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
