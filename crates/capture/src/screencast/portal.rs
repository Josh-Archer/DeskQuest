//! The xdg-desktop-portal ScreenCast flow, independent of the D-Bus transport.
//!
//! [`ScreenCastPortal`] is the narrow interface the flow needs; `portal_zbus.rs` implements
//! it over D-Bus and tests implement it with a fake. [`negotiate`] is the state machine:
//!
//! ```text
//! CreateSession -> SelectSources(restore_token?) -> Start (picker, unless token valid)
//!               -> OpenPipeWireRemote -> (fd, node id)
//! ```
//!
//! A stored restore token that the portal rejects (or silently ignores into a failure) is
//! deleted and the whole flow is retried once without it, which shows the picker again.

use std::os::fd::OwnedFd;

use async_trait::async_trait;
use tokio::sync::oneshot;

use super::error::ScreenCastError;
use super::token_store::RestoreTokenStore;

/// `org.freedesktop.portal.ScreenCast` source types.
pub const SOURCE_MONITOR: u32 = 1;
pub const SOURCE_WINDOW: u32 = 2;
/// Cursor modes (bit flags in `AvailableCursorModes`).
pub const CURSOR_HIDDEN: u32 = 1;
pub const CURSOR_EMBEDDED: u32 = 2;
pub const CURSOR_METADATA: u32 = 4;
/// `persist_mode`: remember the grant until the user revokes it.
pub const PERSIST_UNTIL_REVOKED: u32 = 2;
/// First portal interface version that understands `persist_mode` / `restore_token`.
pub const RESTORE_TOKEN_MIN_VERSION: u32 = 4;

/// Opaque identifier of a portal session (a D-Bus object path in practice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHandle(pub String);

/// What `SelectSources` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOptions {
    /// Bitmask of source types; always monitors.
    pub source_types: u32,
    pub multiple: bool,
    /// One of `CURSOR_*`, or `None` to leave the portal default.
    pub cursor_mode: Option<u32>,
    /// `Some(PERSIST_UNTIL_REVOKED)` to ask the portal for a restore token.
    pub persist_mode: Option<u32>,
    pub restore_token: Option<String>,
}

/// One granted stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStream {
    pub node_id: u32,
    /// Logical size in the compositor's coordinate space, if reported.
    pub size: Option<(i32, i32)>,
    pub position: Option<(i32, i32)>,
    pub source_type: Option<u32>,
}

/// Result of `Start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartResponse {
    pub streams: Vec<PortalStream>,
    /// The (possibly new) restore token, when persistence was requested and granted.
    pub restore_token: Option<String>,
}

/// What a portal backend must provide. Every method corresponds to one portal call.
#[async_trait]
pub trait ScreenCastPortal: Send + Sync {
    /// Interface `version` property.
    async fn version(&mut self) -> Result<u32, ScreenCastError>;
    /// `AvailableCursorModes` property.
    async fn available_cursor_modes(&mut self) -> Result<u32, ScreenCastError>;
    async fn create_session(&mut self) -> Result<SessionHandle, ScreenCastError>;
    async fn select_sources(
        &mut self,
        session: &SessionHandle,
        options: &SelectOptions,
    ) -> Result<(), ScreenCastError>;
    /// Shows the picker (unless a restore token covers it) and returns the granted streams.
    async fn start(&mut self, session: &SessionHandle) -> Result<StartResponse, ScreenCastError>;
    /// The PipeWire socket fd for the session.
    async fn open_pipewire_remote(
        &mut self,
        session: &SessionHandle,
    ) -> Result<OwnedFd, ScreenCastError>;
    /// Resolves (with `()`) when the portal reports the session closed. The sender is
    /// dropped if the watch itself fails.
    async fn watch_closed(
        &mut self,
        session: &SessionHandle,
    ) -> Result<oneshot::Receiver<()>, ScreenCastError>;
    async fn close_session(&mut self, session: &SessionHandle);
}

/// Everything the PipeWire side needs from a successful negotiation.
#[derive(Debug)]
pub struct PortalGrant {
    pub session: SessionHandle,
    pub stream: PortalStream,
    pub pipewire_fd: OwnedFd,
    /// Resolves when the portal closes the session.
    pub closed: oneshot::Receiver<()>,
    /// True when a stored token was sent and the flow succeeded with it.
    pub used_restore_token: bool,
    /// True when the portal handed out a restore token that is now stored.
    pub token_persisted: bool,
}

/// Picks the cursor mode: embedded in the frames when possible, since the encoder consumes
/// plain pictures. Falls back to metadata (the cursor is then not drawn into the stream)
/// and finally to the portal default.
pub fn choose_cursor_mode(available: u32) -> Option<u32> {
    if available & CURSOR_EMBEDDED != 0 {
        Some(CURSOR_EMBEDDED)
    } else if available & CURSOR_METADATA != 0 {
        Some(CURSOR_METADATA)
    } else if available & CURSOR_HIDDEN != 0 {
        Some(CURSOR_HIDDEN)
    } else {
        None
    }
}

/// Runs the portal flow for a monitor, reusing and refreshing the stored restore token.
pub async fn negotiate<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
) -> Result<PortalGrant, ScreenCastError> {
    negotiate_source(portal, store, SOURCE_MONITOR).await
}

/// Like [`negotiate`] for a window: the portal picker offers a single application window.
/// Each window panel needs its own session (and its own restore token store).
pub async fn negotiate_window<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
) -> Result<PortalGrant, ScreenCastError> {
    negotiate_source(portal, store, SOURCE_WINDOW).await
}

/// Runs the portal flow for `source_types` (a `SOURCE_*` bitmask).
pub async fn negotiate_source<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
    source_types: u32,
) -> Result<PortalGrant, ScreenCastError> {
    let version = portal.version().await?;
    let can_persist = version >= RESTORE_TOKEN_MIN_VERSION;
    let cursor_mode = choose_cursor_mode(portal.available_cursor_modes().await.unwrap_or(0));

    let stored = if can_persist {
        store.and_then(|s| s.load())
    } else {
        None
    };

    if let Some(token) = stored {
        tracing::info!("screencast: reusing stored restore token");
        match attempt(
            portal,
            store,
            Some(token),
            cursor_mode,
            can_persist,
            source_types,
        )
        .await
        {
            Ok(grant) => return Ok(grant),
            Err(e) if e.token_may_be_stale() => {
                tracing::warn!(
                    "screencast: stored restore token was rejected ({e}); prompting again"
                );
                if let Some(s) = store {
                    s.clear();
                }
            }
            Err(e) => return Err(e),
        }
    }
    attempt(portal, store, None, cursor_mode, can_persist, source_types).await
}

/// Like [`negotiate_source`], but always shows the picker: the stored restore token is not
/// offered, so the user chooses again. A successful grant replaces the stored token; a
/// cancelled one leaves it as it was.
pub async fn negotiate_source_fresh<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
    source_types: u32,
) -> Result<PortalGrant, ScreenCastError> {
    let version = portal.version().await?;
    let can_persist = version >= RESTORE_TOKEN_MIN_VERSION;
    let cursor_mode = choose_cursor_mode(portal.available_cursor_modes().await.unwrap_or(0));
    attempt(portal, store, None, cursor_mode, can_persist, source_types).await
}

async fn attempt<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
    token: Option<String>,
    cursor_mode: Option<u32>,
    can_persist: bool,
    source_types: u32,
) -> Result<PortalGrant, ScreenCastError> {
    let used_restore_token = token.is_some();
    let session = portal.create_session().await?;
    let options = SelectOptions {
        source_types,
        multiple: false,
        cursor_mode,
        persist_mode: can_persist.then_some(PERSIST_UNTIL_REVOKED),
        restore_token: token,
    };
    match run_session(portal, store, &session, options, used_restore_token).await {
        Ok(grant) => Ok(grant),
        Err(e) => {
            portal.close_session(&session).await;
            Err(e)
        }
    }
}

async fn run_session<P: ScreenCastPortal + ?Sized>(
    portal: &mut P,
    store: Option<&RestoreTokenStore>,
    session: &SessionHandle,
    options: SelectOptions,
    used_restore_token: bool,
) -> Result<PortalGrant, ScreenCastError> {
    // Subscribe to Closed first so a close racing the rest of the flow is not missed.
    let closed = portal.watch_closed(session).await?;
    portal.select_sources(session, &options).await?;
    let started = portal.start(session).await?;
    let stream = started
        .streams
        .into_iter()
        .next()
        .ok_or(ScreenCastError::NoStreams)?;

    let mut token_persisted = false;
    if let (Some(store), Some(new_token)) = (store, started.restore_token.as_deref()) {
        match store.save(new_token) {
            Ok(()) => token_persisted = true,
            Err(e) => tracing::warn!("screencast: could not store restore token: {e}"),
        }
    }

    let pipewire_fd = portal.open_pipewire_remote(session).await?;
    Ok(PortalGrant {
        session: session.clone(),
        stream,
        pipewire_fd,
        closed,
        used_restore_token,
        token_persisted,
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scriptable in-memory portal for the state machine tests.

    use super::*;
    use std::collections::VecDeque;
    use std::os::fd::FromRawFd;

    #[derive(Default)]
    pub struct FakePortal {
        pub version: u32,
        pub cursor_modes: u32,
        /// Results for successive `start` calls.
        pub start_results: VecDeque<Result<StartResponse, ScreenCastError>>,
        /// Results for successive `select_sources` calls.
        pub select_results: VecDeque<Result<(), ScreenCastError>>,
        pub create_error: Option<ScreenCastError>,
        pub calls: Vec<String>,
        pub select_seen: Vec<SelectOptions>,
        pub closed_sessions: Vec<String>,
        pub close_tx: Vec<oneshot::Sender<()>>,
        next_session: u32,
    }

    impl FakePortal {
        pub fn new() -> Self {
            Self {
                version: 5,
                cursor_modes: CURSOR_HIDDEN | CURSOR_EMBEDDED | CURSOR_METADATA,
                ..Default::default()
            }
        }
    }

    pub fn ok_start(node: u32, token: Option<&str>) -> Result<StartResponse, ScreenCastError> {
        Ok(StartResponse {
            streams: vec![PortalStream {
                node_id: node,
                size: Some((2560, 1440)),
                position: Some((0, 0)),
                source_type: Some(SOURCE_MONITOR),
            }],
            restore_token: token.map(str::to_owned),
        })
    }

    fn memfd() -> OwnedFd {
        let fd = unsafe { libc::memfd_create(c"fake-pw-remote".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[async_trait]
    impl ScreenCastPortal for FakePortal {
        async fn version(&mut self) -> Result<u32, ScreenCastError> {
            self.calls.push("version".into());
            Ok(self.version)
        }
        async fn available_cursor_modes(&mut self) -> Result<u32, ScreenCastError> {
            Ok(self.cursor_modes)
        }
        async fn create_session(&mut self) -> Result<SessionHandle, ScreenCastError> {
            self.calls.push("create_session".into());
            if let Some(e) = self.create_error.clone() {
                return Err(e);
            }
            self.next_session += 1;
            Ok(SessionHandle(format!("/session/{}", self.next_session)))
        }
        async fn select_sources(
            &mut self,
            _s: &SessionHandle,
            options: &SelectOptions,
        ) -> Result<(), ScreenCastError> {
            self.calls.push("select_sources".into());
            self.select_seen.push(options.clone());
            self.select_results.pop_front().unwrap_or(Ok(()))
        }
        async fn start(&mut self, _s: &SessionHandle) -> Result<StartResponse, ScreenCastError> {
            self.calls.push("start".into());
            self.start_results
                .pop_front()
                .unwrap_or_else(|| ok_start(7, None))
        }
        async fn open_pipewire_remote(
            &mut self,
            _s: &SessionHandle,
        ) -> Result<OwnedFd, ScreenCastError> {
            self.calls.push("open_pipewire_remote".into());
            Ok(memfd())
        }
        async fn watch_closed(
            &mut self,
            _s: &SessionHandle,
        ) -> Result<oneshot::Receiver<()>, ScreenCastError> {
            self.calls.push("watch_closed".into());
            let (tx, rx) = oneshot::channel();
            self.close_tx.push(tx);
            Ok(rx)
        }
        async fn close_session(&mut self, s: &SessionHandle) {
            self.calls.push("close_session".into());
            self.closed_sessions.push(s.0.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use std::path::PathBuf;

    fn store(tag: &str) -> (RestoreTokenStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("lq-portal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (RestoreTokenStore::at(root.join("tok")), root)
    }

    #[test]
    fn cursor_mode_prefers_embedded() {
        assert_eq!(choose_cursor_mode(7), Some(CURSOR_EMBEDDED));
        assert_eq!(
            choose_cursor_mode(CURSOR_HIDDEN | CURSOR_METADATA),
            Some(CURSOR_METADATA)
        );
        assert_eq!(choose_cursor_mode(CURSOR_HIDDEN), Some(CURSOR_HIDDEN));
        assert_eq!(choose_cursor_mode(0), None);
    }

    #[tokio::test]
    async fn first_run_prompts_and_stores_the_token() {
        let (store, root) = store("first");
        let mut p = FakePortal::new();
        p.start_results.push_back(ok_start(42, Some("tok-1")));
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert_eq!(grant.stream.node_id, 42);
        assert!(!grant.used_restore_token);
        assert!(grant.token_persisted);
        assert_eq!(store.load().as_deref(), Some("tok-1"));
        assert_eq!(
            p.calls,
            [
                "version",
                "create_session",
                "watch_closed",
                "select_sources",
                "start",
                "open_pipewire_remote"
            ]
        );
        let o = &p.select_seen[0];
        assert_eq!(o.source_types, SOURCE_MONITOR);
        assert!(!o.multiple);
        assert_eq!(o.cursor_mode, Some(CURSOR_EMBEDDED));
        assert_eq!(o.persist_mode, Some(PERSIST_UNTIL_REVOKED));
        assert_eq!(o.restore_token, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn window_negotiation_requests_the_window_source_type() {
        let mut p = FakePortal::new();
        p.start_results.push_back(ok_start(11, None));
        negotiate_window(&mut p, None).await.unwrap();
        assert_eq!(p.select_seen[0].source_types, SOURCE_WINDOW);
        assert!(!p.select_seen[0].multiple);
    }

    #[tokio::test]
    async fn second_run_sends_the_stored_token_and_refreshes_it() {
        let (store, root) = store("second");
        store.save("tok-1").unwrap();
        let mut p = FakePortal::new();
        p.start_results.push_back(ok_start(9, Some("tok-2")));
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert!(grant.used_restore_token);
        assert_eq!(p.select_seen.len(), 1);
        assert_eq!(p.select_seen[0].restore_token.as_deref(), Some("tok-1"));
        assert_eq!(store.load().as_deref(), Some("tok-2"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejected_token_is_dropped_and_the_flow_retries_without_it() {
        let (store, root) = store("stale");
        store.save("stale").unwrap();
        let mut p = FakePortal::new();
        p.select_results
            .push_back(Err(ScreenCastError::Portal("invalid restore_token".into())));
        p.start_results.push_back(ok_start(3, Some("fresh")));
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert!(!grant.used_restore_token);
        assert_eq!(p.select_seen.len(), 2);
        assert_eq!(p.select_seen[0].restore_token.as_deref(), Some("stale"));
        assert_eq!(p.select_seen[1].restore_token, None);
        assert_eq!(store.load().as_deref(), Some("fresh"));
        // The failed first session was closed.
        assert_eq!(p.closed_sessions, ["/session/1"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn token_that_yields_no_streams_also_retries() {
        let (store, root) = store("nostreams");
        store.save("old").unwrap();
        let mut p = FakePortal::new();
        p.start_results.push_back(Ok(StartResponse {
            streams: vec![],
            restore_token: None,
        }));
        p.start_results.push_back(ok_start(5, None));
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert_eq!(grant.stream.node_id, 5);
        assert_eq!(store.load(), None, "stale token must not survive");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_picker_is_not_retried_and_keeps_the_token() {
        let (store, root) = store("cancel");
        store.save("keep").unwrap();
        let mut p = FakePortal::new();
        p.start_results
            .push_back(Err(ScreenCastError::UserCancelled));
        let err = negotiate(&mut p, Some(&store)).await.unwrap_err();
        assert_eq!(err, ScreenCastError::UserCancelled);
        assert_eq!(p.select_seen.len(), 1);
        assert_eq!(store.load().as_deref(), Some("keep"));
        assert_eq!(p.closed_sessions.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn absent_portal_is_a_typed_error_not_a_panic() {
        let mut p = FakePortal::new();
        p.create_error = Some(ScreenCastError::PortalUnavailable("no bus".into()));
        let err = negotiate(&mut p, None).await.unwrap_err();
        assert!(matches!(err, ScreenCastError::PortalUnavailable(_)));
        // Not retried: only one create_session.
        assert_eq!(p.calls.iter().filter(|c| *c == "create_session").count(), 1);
    }

    #[tokio::test]
    async fn denied_without_a_token_is_final() {
        let mut p = FakePortal::new();
        p.start_results
            .push_back(Err(ScreenCastError::Denied("policy".into())));
        let err = negotiate(&mut p, None).await.unwrap_err();
        assert!(matches!(err, ScreenCastError::Denied(_)));
        assert_eq!(p.select_seen.len(), 1);
    }

    #[tokio::test]
    async fn old_portals_get_no_persistence_options() {
        let (store, root) = store("old");
        store.save("ignored").unwrap();
        let mut p = FakePortal::new();
        p.version = 3;
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert!(!grant.used_restore_token);
        assert_eq!(p.select_seen[0].persist_mode, None);
        assert_eq!(p.select_seen[0].restore_token, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn session_close_is_observable_through_the_grant() {
        let mut p = FakePortal::new();
        let grant = negotiate(&mut p, None).await.unwrap();
        let tx = p.close_tx.pop().unwrap();
        tx.send(()).unwrap();
        assert!(grant.closed.await.is_ok());
    }

    #[tokio::test]
    async fn unwritable_token_store_does_not_fail_the_capture() {
        let store = RestoreTokenStore::at("/proc/definitely/not/writable/tok");
        let mut p = FakePortal::new();
        p.start_results.push_back(ok_start(1, Some("t")));
        let grant = negotiate(&mut p, Some(&store)).await.unwrap();
        assert!(!grant.token_persisted);
    }
}
