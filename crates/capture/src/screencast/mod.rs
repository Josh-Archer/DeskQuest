//! Real screen capture on Wayland (GNOME, KDE, ...) and X11 desktops through the
//! xdg-desktop-portal ScreenCast interface and a PipeWire video stream.
//!
//! # Layers
//!
//! | module | needs | purpose |
//! |---|---|---|
//! | [`error`], [`mod@format`], [`hub`], [`token_store`], [`portal`] | nothing | typed errors, format model, latest-wins queue, restore-token file, portal state machine |
//! | `portal_zbus` | feature `portal` | the portal client over D-Bus (`zbus`) |
//! | `pod`, `stream`, `session` | feature `pipewire-screencast` | SPA pod building/parsing, the PipeWire main-loop thread, session orchestration |
//!
//! The pure layers compile and are tested in the default build; only the last two need
//! `libpipewire-0.3-dev` (and libclang for bindgen).
//!
//! # Flow
//!
//! 1. `CreateSession`, `SelectSources` (monitors, single selection, cursor embedded in the
//!    frames, `persist_mode = 2` and the stored `restore_token`), `Start` (the user sees the
//!    monitor picker once), `OpenPipeWireRemote` -> (fd, node id).
//! 2. A thread connects to PipeWire on that fd, offers formats (DMA-BUF with modifiers first,
//!    SHM second), reads the negotiated size / frame rate, and pushes frames into a
//!    [`CaptureHub`]. `capture_frame().await` drains it, newest frame wins.
//! 3. Portal session close, node removal and stream errors end the hub with a typed
//!    [`ScreenCastError`]; calling `init` again restarts (reusing the restore token).
//!
//! # Choosing the captured monitor
//!
//! The portal decides which output is shared: **the user picks it in the compositor's
//! dialog**. There is no API to request "the virtual monitor created by the display crate"
//! (EVDI / X11 / Mutter virtual output). The user must select it in the picker once; with a
//! restore token the choice is remembered. A fully automatic / headless route needs
//! compositor-specific APIs, deliberately not implemented here:
//!
//! * GNOME/Mutter: `org.gnome.Mutter.ScreenCast` + `RemoteDesktop` (no prompt, but only
//!   reachable from privileged/allow-listed callers),
//! * wlroots (Sway, Hyprland...): `wlr-screencopy` / `ext-image-copy-capture-v1`,
//! * KDE: KWin's own screen cast / remote-control APIs with a pre-authorised application.

pub mod error;
pub mod format;
pub mod hub;
#[cfg(feature = "pipewire-screencast")]
pub mod pod;
pub mod portal;
#[cfg(feature = "portal")]
pub mod portal_zbus;
#[cfg(feature = "pipewire-screencast")]
pub mod session;
#[cfg(feature = "pipewire-screencast")]
pub mod stream;
pub mod token_store;

use std::time::Duration;

pub use error::ScreenCastError;
pub use format::{NegotiatedFormat, OfferConfig, VideoFormatKind};
pub use hub::{CaptureHub, CaptureStats};
pub use token_store::RestoreTokenStore;

/// Settings for a screencast capture. `Default` is what the daemon should use.
#[derive(Debug, Clone)]
pub struct ScreenCastConfig {
    /// Which formats / modifiers to offer, and the accepted size and rate range.
    pub offer: OfferConfig,
    /// Frames that may wait for the consumer before the oldest is dropped.
    pub queue_depth: usize,
    /// Where to keep the restore token; `None` uses `$XDG_STATE_HOME/linux-quest`. Ignored
    /// when `persist_token` is false.
    pub token_store: Option<RestoreTokenStore>,
    /// Ask the portal for a restore token and reuse it on later runs.
    pub persist_token: bool,
    /// How long the whole portal flow (including the user's picker interaction) may take.
    pub portal_timeout: Duration,
    /// How long to wait for PipeWire to negotiate a format after the portal granted access.
    pub negotiation_timeout: Duration,
}

impl Default for ScreenCastConfig {
    fn default() -> Self {
        Self {
            offer: OfferConfig::default(),
            queue_depth: 2,
            token_store: None,
            persist_token: true,
            portal_timeout: Duration::from_secs(300),
            negotiation_timeout: Duration::from_secs(10),
        }
    }
}

impl ScreenCastConfig {
    /// The token store to use, resolving the default location.
    pub fn resolve_token_store(&self) -> Option<RestoreTokenStore> {
        if !self.persist_token {
            return None;
        }
        self.token_store
            .clone()
            .or_else(RestoreTokenStore::default_location)
    }
}

/// Restoring the shared screen without asking (`negotiate_source_restore`), against the
/// in-memory fake.
#[cfg(test)]
mod restore_tests {
    use super::portal::fake::{ok_start, FakePortal};
    use super::portal::{negotiate_source_restore, SOURCE_MONITOR};
    use super::{RestoreTokenStore, ScreenCastError};

    fn store(tag: &str) -> (RestoreTokenStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("lq-restore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (RestoreTokenStore::at(root.join("token")), root)
    }

    #[tokio::test]
    async fn restoring_sends_the_stored_token_and_keeps_the_new_one() {
        let (store, root) = store("ok");
        store.save("old-token").unwrap();
        let mut fake = FakePortal::new();
        fake.start_results.push_back(ok_start(4, Some("new-token")));

        let grant = negotiate_source_restore(&mut fake, Some(&store), SOURCE_MONITOR)
            .await
            .unwrap();
        assert!(grant.used_restore_token);
        assert_eq!(grant.stream.node_id, 4);
        assert_eq!(fake.select_seen.len(), 1);
        assert_eq!(
            fake.select_seen[0].restore_token.as_deref(),
            Some("old-token")
        );
        assert!(fake.select_seen[0].persist_mode.is_some());
        assert_eq!(store.load().as_deref(), Some("new-token"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn without_a_token_nothing_is_asked() {
        let (store, root) = store("none");
        let mut fake = FakePortal::new();
        let err = negotiate_source_restore(&mut fake, Some(&store), SOURCE_MONITOR)
            .await
            .unwrap_err();
        assert_eq!(err, ScreenCastError::NoRestoreToken);
        assert_eq!(fake.calls, ["version"], "no session, so no picker");

        // Nor without a store, or on a version that cannot restore.
        let err = negotiate_source_restore(&mut fake, None, SOURCE_MONITOR)
            .await
            .unwrap_err();
        assert_eq!(err, ScreenCastError::NoRestoreToken);
        store.save("token").unwrap();
        let mut old = FakePortal::new();
        old.version = 3;
        let err = negotiate_source_restore(&mut old, Some(&store), SOURCE_MONITOR)
            .await
            .unwrap_err();
        assert_eq!(err, ScreenCastError::NoRestoreToken);
        assert!(old.select_seen.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_failed_restore_keeps_the_token_and_does_not_prompt() {
        // `negotiate` would drop the token and show the picker on each of these.
        for failure in [
            ScreenCastError::Denied("portal response code 2".into()),
            ScreenCastError::Portal("invalid restore_token".into()),
            ScreenCastError::NoStreams,
        ] {
            let (store, root) = store("fail");
            store.save("keep-me").unwrap();
            let mut fake = FakePortal::new();
            if failure == ScreenCastError::NoStreams {
                fake.start_results
                    .push_back(Ok(super::portal::StartResponse {
                        streams: vec![],
                        restore_token: None,
                    }));
            } else {
                fake.start_results.push_back(Err(failure.clone()));
            }

            let err = match negotiate_source_restore(&mut fake, Some(&store), SOURCE_MONITOR).await
            {
                Ok(_) => panic!("{failure:?} must fail the restore"),
                Err(e) => e,
            };
            assert_eq!(err, failure);
            assert_eq!(fake.select_seen.len(), 1, "one attempt, with the token");
            assert_eq!(store.load().as_deref(), Some("keep-me"));
            assert_eq!(
                fake.closed_sessions.len(),
                1,
                "the failed session is closed"
            );
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// Choosing another screen (`negotiate_source_fresh`), against the in-memory fake.
#[cfg(test)]
mod reselect_tests {
    use super::portal::fake::{ok_start, FakePortal};
    use super::portal::{negotiate_source_fresh, SOURCE_MONITOR};
    use super::{RestoreTokenStore, ScreenCastError};

    fn store(tag: &str) -> (RestoreTokenStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("lq-reselect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (RestoreTokenStore::at(root.join("token")), root)
    }

    #[tokio::test]
    async fn choosing_again_shows_the_picker_and_replaces_the_stored_token() {
        let (store, root) = store("fresh");
        store.save("old-token").unwrap();
        let mut fake = FakePortal::new();
        fake.start_results.push_back(ok_start(9, Some("new-token")));

        let grant = negotiate_source_fresh(&mut fake, Some(&store), SOURCE_MONITOR)
            .await
            .unwrap();
        assert!(!grant.used_restore_token);
        assert_eq!(grant.stream.node_id, 9);
        assert_eq!(fake.select_seen.len(), 1);
        assert_eq!(
            fake.select_seen[0].restore_token, None,
            "no token: the picker shows"
        );
        assert!(
            fake.select_seen[0].persist_mode.is_some(),
            "the new grant is kept"
        );
        assert_eq!(store.load().as_deref(), Some("new-token"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_cancelled_choice_keeps_the_stored_token() {
        let (store, root) = store("cancel");
        store.save("old-token").unwrap();
        let mut fake = FakePortal::new();
        fake.start_results
            .push_back(Err(ScreenCastError::UserCancelled));

        let err = match negotiate_source_fresh(&mut fake, Some(&store), SOURCE_MONITOR).await {
            Ok(_) => panic!("a cancelled pick must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, ScreenCastError::UserCancelled));
        assert_eq!(store.load().as_deref(), Some("old-token"));
        assert_eq!(
            fake.closed_sessions.len(),
            1,
            "the failed session is closed"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
