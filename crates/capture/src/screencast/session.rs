//! One running screen capture: portal grant + PipeWire thread + the task that turns a portal
//! session close into a clean end of the stream.

use std::sync::Arc;

use tokio::task::JoinHandle;

use super::error::ScreenCastError;
use super::hub::CaptureHub;
use super::portal::{
    negotiate_source, negotiate_source_fresh, ScreenCastPortal, SessionHandle, SOURCE_MONITOR,
    SOURCE_WINDOW,
};
use super::stream::{self, StreamHandle, StreamParams};
use super::ScreenCastConfig;
use crate::CaptureInfo;

/// A live capture. Stop it with [`stop`](Self::stop) (also closes the portal session) or just
/// drop it (the PipeWire thread is stopped and joined; the D-Bus connection going away makes
/// the portal close the session).
pub struct ScreenCastSession {
    portal: Box<dyn ScreenCastPortal>,
    session: SessionHandle,
    stream: StreamHandle,
    watcher: JoinHandle<()>,
    info: CaptureInfo,
}

impl ScreenCastSession {
    /// Runs the portal flow on `portal`, connects PipeWire and waits for the first format.
    ///
    /// `hub` is reset first, so a hub can be reused across restarts.
    pub async fn start<P>(
        portal: P,
        cfg: &ScreenCastConfig,
        hub: Arc<CaptureHub>,
        display_id: u16,
    ) -> Result<Self, ScreenCastError>
    where
        P: ScreenCastPortal + 'static,
    {
        Self::start_source(portal, cfg, hub, display_id, SOURCE_MONITOR, false).await
    }

    /// Like [`start`](Self::start), but the portal always shows its picker: the stored
    /// restore token is not offered. A grant replaces the stored token.
    pub async fn start_fresh<P>(
        portal: P,
        cfg: &ScreenCastConfig,
        hub: Arc<CaptureHub>,
        display_id: u16,
        window: bool,
    ) -> Result<Self, ScreenCastError>
    where
        P: ScreenCastPortal + 'static,
    {
        let source = if window {
            SOURCE_WINDOW
        } else {
            SOURCE_MONITOR
        };
        Self::start_source(portal, cfg, hub, display_id, source, true).await
    }

    /// Starts a session that asks the portal to select one application window.
    pub async fn start_window<P>(
        portal: P,
        cfg: &ScreenCastConfig,
        hub: Arc<CaptureHub>,
        display_id: u16,
    ) -> Result<Self, ScreenCastError>
    where
        P: ScreenCastPortal + 'static,
    {
        Self::start_source(portal, cfg, hub, display_id, SOURCE_WINDOW, false).await
    }

    async fn start_source<P>(
        mut portal: P,
        cfg: &ScreenCastConfig,
        hub: Arc<CaptureHub>,
        display_id: u16,
        source_types: u32,
        fresh: bool,
    ) -> Result<Self, ScreenCastError>
    where
        P: ScreenCastPortal + 'static,
    {
        hub.reset();
        let store = cfg.resolve_token_store();
        let negotiation = async {
            if fresh {
                negotiate_source_fresh(&mut portal, store.as_ref(), source_types).await
            } else {
                negotiate_source(&mut portal, store.as_ref(), source_types).await
            }
        };
        let grant = match tokio::time::timeout(cfg.portal_timeout, negotiation).await {
            Ok(r) => r?,
            Err(_) => return Err(ScreenCastError::NegotiationTimeout(cfg.portal_timeout)),
        };
        tracing::info!(
            "screencast: portal granted node {} (restore token {})",
            grant.stream.node_id,
            if grant.used_restore_token {
                "reused, no prompt"
            } else {
                "not used, user was prompted"
            }
        );

        let session = grant.session.clone();
        let stream = match stream::spawn(StreamParams {
            fd: Some(grant.pipewire_fd),
            node_id: Some(grant.stream.node_id),
            display_id,
            offer: cfg.offer.clone(),
            hub: Arc::clone(&hub),
        }) {
            Ok(s) => s,
            Err(e) => {
                portal.close_session(&session).await;
                return Err(e);
            }
        };

        let closed = grant.closed;
        let watch_hub = Arc::clone(&hub);
        let watcher = tokio::spawn(async move {
            if closed.await.is_ok() {
                tracing::warn!("screencast: portal session closed");
                watch_hub.end(ScreenCastError::SessionClosed);
            }
        });

        let mut this = Self {
            portal: Box::new(portal),
            session,
            stream,
            watcher,
            // Replaced below once PipeWire reports the format.
            info: placeholder_info(),
        };
        match hub.wait_for_info(cfg.negotiation_timeout).await {
            Ok(info) => {
                this.info = info;
                Ok(this)
            }
            Err(e) => {
                this.stop().await;
                Err(e)
            }
        }
    }

    /// The format negotiated when the session started. The live value (it can change on a
    /// mode switch) is `CaptureHub::info`.
    pub fn initial_info(&self) -> CaptureInfo {
        self.info
    }

    pub fn is_stream_running(&self) -> bool {
        self.stream.is_running()
    }

    /// Stops PipeWire, then closes the portal session.
    pub async fn stop(mut self) {
        self.watcher.abort();
        self.stream.stop();
        self.portal.close_session(&self.session).await;
    }
}

impl Drop for ScreenCastSession {
    fn drop(&mut self) {
        self.watcher.abort();
        self.stream.stop();
    }
}

fn placeholder_info() -> CaptureInfo {
    CaptureInfo {
        width: 0,
        height: 0,
        fps: 0,
        format: crate::PixelFormat::Bgra8,
        dmabuf: false,
        modifier: None,
    }
}
