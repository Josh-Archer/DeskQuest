//! Persistence of the portal `restore_token`, so the monitor picker is shown once and later
//! runs reuse the grant.
//!
//! * Location: `$XDG_STATE_HOME/linux-quest/screencast-restore-token`, falling back to
//!   `~/.local/state/linux-quest/...`.
//! * Directories are created `0700`, the file `0600`.
//! * Writes are atomic: a temp file in the same directory is written, synced and renamed over
//!   the target, so a crash never leaves a half-written token.
//! * A missing, unreadable or corrupt file simply means "no token" (and a corrupt file is
//!   removed); this module never fails the capture because of its own state.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAGIC: &str = "linux-quest-screencast-restore-token v1";
const MAX_TOKEN_LEN: usize = 4096;
const FILE_NAME: &str = "screencast-restore-token";

/// Reads and writes one restore token file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreTokenStore {
    path: PathBuf,
}

impl RestoreTokenStore {
    /// A store at an explicit file path (tests, custom deployments).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default per-user location, or `None` when neither `XDG_STATE_HOME` nor `HOME` is
    /// usable.
    pub fn default_location() -> Option<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
            _ => {
                let home = std::env::var_os("HOME")?;
                if home.is_empty() {
                    return None;
                }
                PathBuf::from(home).join(".local").join("state")
            }
        };
        Some(Self::at(base.join("linux-quest").join(FILE_NAME)))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the stored token, or `None` if there is none or the file is unusable.
    pub fn load(&self) -> Option<String> {
        let raw = match fs::read(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("cannot read restore token {}: {e}", self.path.display());
                return None;
            }
        };
        match parse(&raw) {
            Some(token) => Some(token),
            None => {
                tracing::warn!(
                    "restore token file {} is corrupt; ignoring and removing it",
                    self.path.display()
                );
                let _ = fs::remove_file(&self.path);
                None
            }
        }
    }

    /// Atomically replaces the stored token.
    pub fn save(&self, token: &str) -> io::Result<()> {
        if !valid_token(token) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "restore token is empty, too long or contains whitespace/control characters",
            ));
        }
        let dir = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "token path has no parent")
        })?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

        let tmp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            // `mode` is subject to the umask; make sure it is exactly 0600.
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(format!("{MAGIC}\n{token}\n").as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        // Persist the rename itself.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Deletes the stored token (e.g. because the portal rejected it).
    pub fn clear(&self) {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("cannot remove restore token {}: {e}", self.path.display()),
        }
    }
}

fn valid_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= MAX_TOKEN_LEN && t.bytes().all(|b| b.is_ascii_graphic())
}

fn parse(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let token = lines.next()?;
    if lines.any(|l| !l.is_empty()) {
        return None;
    }
    valid_token(token).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lq-token-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn save_then_load_round_trips_with_private_modes() {
        let root = tmpdir("rt");
        let store = RestoreTokenStore::at(root.join("a/b/linux-quest").join(FILE_NAME));
        assert_eq!(store.load(), None);
        store.save("tok-123_ABC").unwrap();
        assert_eq!(store.load().as_deref(), Some("tok-123_ABC"));

        let file_mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let dir_mode = fs::metadata(store.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        // Overwrite replaces, and leaves no temp files behind.
        store.save("second").unwrap();
        assert_eq!(store.load().as_deref(), Some("second"));
        let leftovers: Vec<_> = fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from(FILE_NAME)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_file_is_ignored_and_removed() {
        let root = tmpdir("corrupt");
        fs::create_dir_all(&root).unwrap();
        let store = RestoreTokenStore::at(root.join(FILE_NAME));
        for garbage in [
            &b""[..],
            b"no header\ntoken\n",
            b"linux-quest-screencast-restore-token v1\n",
            b"linux-quest-screencast-restore-token v1\nwith space\n",
            b"linux-quest-screencast-restore-token v1\ntok\nextra\n",
            b"\xff\xfe\x00binary",
        ] {
            fs::write(store.path(), garbage).unwrap();
            assert_eq!(store.load(), None, "{garbage:?}");
            assert!(!store.path().exists(), "corrupt file must be removed");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_tokens_are_refused() {
        let root = tmpdir("invalid");
        let store = RestoreTokenStore::at(root.join(FILE_NAME));
        assert!(store.save("").is_err());
        assert!(store.save("has space").is_err());
        assert!(store.save("new\nline").is_err());
        assert!(store.save(&"x".repeat(MAX_TOKEN_LEN + 1)).is_err());
        assert!(!store.path().exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_is_idempotent() {
        let root = tmpdir("clear");
        let store = RestoreTokenStore::at(root.join(FILE_NAME));
        store.clear();
        store.save("t").unwrap();
        store.clear();
        assert_eq!(store.load(), None);
        store.clear();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_tightens_a_world_readable_predecessor() {
        let root = tmpdir("tighten");
        fs::create_dir_all(&root).unwrap();
        let store = RestoreTokenStore::at(root.join(FILE_NAME));
        fs::write(store.path(), "old").unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o666)).unwrap();
        store.save("fresh").unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn default_location_honours_xdg_state_home() {
        // Environment is process-global; only assert on the pure path computation by setting
        // and restoring within one test.
        let old = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state-test");
        let s = RestoreTokenStore::default_location().unwrap();
        assert_eq!(
            s.path(),
            Path::new("/tmp/xdg-state-test/linux-quest/screencast-restore-token")
        );
        match old {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}
