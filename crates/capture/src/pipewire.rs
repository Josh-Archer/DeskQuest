use async_trait::async_trait;
use libloading::Library;
use std::path::Path;
use std::sync::Arc;

use crate::screencast::{CaptureHub, CaptureStats, ScreenCastConfig, ScreenCastError};
use crate::{CaptureBackendType, CaptureError, CaptureInfo, DisplayCapture, RawFrame};

/// SPA data types for buffer allocation.
pub const SPA_DATA_MEM_PTR: u32 = 1;
pub const SPA_DATA_MEM_FD: u32 = 2;
pub const SPA_DATA_DMA_BUF: u32 = 3;

/// Screen capture through the xdg-desktop-portal ScreenCast interface and PipeWire.
///
/// `init` runs the portal flow (the user picks a monitor once; later runs reuse the stored
/// restore token), connects a PipeWire stream and waits for the format to be negotiated.
/// After that [`capture_info`](DisplayCapture::capture_info) reports the real size and frame
/// rate, and `capture_frame` yields the newest frame (DMA-BUF when the compositor allows it,
/// otherwise a CPU copy). When the portal session closes or the node disappears,
/// `capture_frame` returns `CaptureError::ScreenCast(..)` and `init` can be called again to
/// restart.
///
/// Requires the `pipewire-screencast` cargo feature; without it `init` returns
/// `BackendUnavailable` and `AutoCapture` falls back to the synthetic source.
///
/// The constructor's `width`, `height` and `fps` are only hints (initial values returned by
/// the getters below; `fps` is also the frame rate requested from the compositor). The
/// negotiated values come from `capture_info`.
pub struct PipeWireCapture {
    display_id: u16,
    window_source: bool,
    width: u32,
    height: u32,
    fps: u32,
    initialized: bool,
    #[cfg_attr(not(feature = "pipewire-screencast"), allow(dead_code))]
    config: ScreenCastConfig,
    hub: Arc<CaptureHub>,
    #[cfg(feature = "pipewire-screencast")]
    session: Option<crate::screencast::session::ScreenCastSession>,
}

impl PipeWireCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        Self::with_config(display_id, width, height, fps, ScreenCastConfig::default())
    }

    pub fn with_config(
        display_id: u16,
        width: u32,
        height: u32,
        fps: u32,
        mut config: ScreenCastConfig,
    ) -> Self {
        config.offer.preferred_fps = fps.max(1);
        config.offer.max_fps = config.offer.max_fps.max(fps);
        Self {
            display_id,
            window_source: false,
            width,
            height,
            fps,
            initialized: false,
            hub: CaptureHub::new(config.queue_depth),
            config,
            #[cfg(feature = "pipewire-screencast")]
            session: None,
        }
    }

    /// Creates a capture whose portal picker offers application windows.
    pub fn new_window(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        let mut capture = Self::new(display_id, width, height, fps);
        capture.window_source = true;
        capture
    }

    /// True when this build can capture the screen and a PipeWire daemon socket is present
    /// and libpipewire is loadable. Whether a portal is available is only known at `init`.
    pub fn is_available() -> bool {
        if !cfg!(feature = "pipewire-screencast") {
            return false;
        }
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let socket_path = Path::new(&runtime_dir).join("pipewire-0");
            if socket_path.exists() {
                return unsafe {
                    Library::new("libpipewire-0.3.so.0")
                        .or_else(|_| Library::new("libpipewire-0.3.so"))
                        .is_ok()
                };
            }
        }
        false
    }

    pub fn display_id(&self) -> u16 {
        self.display_id
    }

    /// The width hint given at construction (see [`capture_info`](DisplayCapture::capture_info)
    /// for the real one).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The height hint given at construction.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The frame rate requested from the compositor.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// True while a capture is initialized and has not ended.
    pub fn is_running(&self) -> bool {
        self.initialized && self.hub.ended().is_none()
    }

    /// Frame counters (queued / delivered / dropped / unchanged / renegotiations).
    pub fn stats(&self) -> CaptureStats {
        self.hub.stats()
    }

    /// Increases whenever the negotiated format changes (initial negotiation counts). A
    /// consumer that caches `capture_info()` re-reads it when this moves.
    pub fn info_generation(&self) -> u64 {
        self.hub.info_generation()
    }

    /// Why the capture ended, if it did (`None` while running).
    pub fn ended_reason(&self) -> Option<ScreenCastError> {
        self.hub.ended()
    }

    /// Stops the stream and closes the portal session. Safe to call repeatedly; `init`
    /// starts a fresh capture afterwards.
    pub async fn stop(&mut self) {
        self.initialized = false;
        #[cfg(feature = "pipewire-screencast")]
        if let Some(session) = self.session.take() {
            session.stop().await;
        }
        self.hub.end(ScreenCastError::Stopped);
    }

    /// Synchronous variant of [`stop`](Self::stop) used by `Drop`: stops the PipeWire thread;
    /// the portal session closes when the D-Bus connection is dropped.
    pub fn cleanup(&mut self) {
        self.initialized = false;
        #[cfg(feature = "pipewire-screencast")]
        {
            self.session = None;
        }
    }

    /// Starts the capture over an already-built portal client (tests, alternative
    /// transports). `init` uses the D-Bus portal.
    #[cfg(feature = "pipewire-screencast")]
    pub async fn init_with_portal<P>(&mut self, portal: P) -> Result<(), CaptureError>
    where
        P: crate::screencast::portal::ScreenCastPortal + 'static,
    {
        self.cleanup();
        let session = if self.window_source {
            crate::screencast::session::ScreenCastSession::start_window(
                portal,
                &self.config,
                Arc::clone(&self.hub),
                self.display_id,
            )
            .await?
        } else {
            crate::screencast::session::ScreenCastSession::start(
                portal,
                &self.config,
                Arc::clone(&self.hub),
                self.display_id,
            )
            .await?
        };
        self.session = Some(session);
        self.initialized = true;
        Ok(())
    }

    /// Replaces the running capture with one the user picks in the portal's dialog, over an
    /// already-built portal client. On failure (for example the user cancelled) the previous
    /// source is captured again through `restore` (normally a new portal client, which reuses
    /// the stored restore token, so no dialog is shown for that).
    #[cfg(feature = "pipewire-screencast")]
    pub async fn reselect_with_portals<P, R, F>(
        &mut self,
        fresh: P,
        restore: F,
    ) -> Result<(), CaptureError>
    where
        P: crate::screencast::portal::ScreenCastPortal + 'static,
        R: crate::screencast::portal::ScreenCastPortal + 'static,
        F: std::future::Future<Output = Result<R, CaptureError>>,
    {
        // The portal hands out a new session; the old one is closed first so the two do not
        // compete for the hub.
        self.stop().await;
        let picked = crate::screencast::session::ScreenCastSession::start_fresh(
            fresh,
            &self.config,
            Arc::clone(&self.hub),
            self.display_id,
            self.window_source,
        )
        .await;
        match picked {
            Ok(session) => {
                self.session = Some(session);
                self.initialized = true;
                tracing::info!("screencast: the user picked a new source");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "screencast: no new source picked; restoring the previous one");
                match restore.await {
                    Ok(portal) => {
                        if let Err(restore_err) = self.init_with_portal(portal).await {
                            tracing::warn!(error = %restore_err, "screencast: could not restore the previous source");
                        }
                    }
                    Err(restore_err) => {
                        tracing::warn!(error = %restore_err, "screencast: could not reach the portal to restore the previous source");
                    }
                }
                Err(e.into())
            }
        }
    }
}

#[async_trait]
impl DisplayCapture for PipeWireCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        #[cfg(feature = "pipewire-screencast")]
        {
            let portal = crate::screencast::portal_zbus::ZbusPortal::connect_session().await?;
            self.init_with_portal(portal).await
        }
        #[cfg(not(feature = "pipewire-screencast"))]
        {
            self.cleanup();
            Err(CaptureError::BackendUnavailable(
                ScreenCastError::FeatureDisabled.to_string(),
            ))
        }
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        if !self.initialized {
            return Err(CaptureError::InitFailed(
                "PipeWireCapture not initialized".into(),
            ));
        }
        self.hub.next_frame().await
    }

    fn backend_type(&self) -> CaptureBackendType {
        CaptureBackendType::PipeWire
    }

    fn capture_info(&self) -> Option<CaptureInfo> {
        if self.initialized {
            self.hub.info()
        } else {
            None
        }
    }

    async fn reselect(&mut self) -> Result<(), CaptureError> {
        #[cfg(feature = "pipewire-screencast")]
        {
            use crate::screencast::portal_zbus::ZbusPortal;
            let fresh = ZbusPortal::connect_session().await?;
            let restore = async {
                ZbusPortal::connect_session()
                    .await
                    .map_err(CaptureError::from)
            };
            self.reselect_with_portals(fresh, restore).await
        }
        #[cfg(not(feature = "pipewire-screencast"))]
        {
            Err(CaptureError::BackendUnavailable(
                ScreenCastError::FeatureDisabled.to_string(),
            ))
        }
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uninitialized_capture_reports_it() {
        let mut pw = PipeWireCapture::new(3, 1920, 1080, 60);
        assert_eq!(pw.display_id(), 3);
        assert_eq!(pw.width(), 1920);
        assert_eq!(pw.height(), 1080);
        assert_eq!(pw.fps(), 60);
        assert!(!pw.is_running());
        assert!(pw.capture_info().is_none());
        assert!(matches!(
            pw.capture_frame().await,
            Err(CaptureError::InitFailed(_))
        ));
        assert_eq!(pw.backend_type(), CaptureBackendType::PipeWire);
    }

    #[cfg(not(feature = "pipewire-screencast"))]
    #[tokio::test]
    async fn without_the_feature_init_is_backend_unavailable() {
        let mut pw = PipeWireCapture::new(0, 1920, 1080, 60);
        assert!(!PipeWireCapture::is_available());
        assert!(matches!(
            pw.init().await,
            Err(CaptureError::BackendUnavailable(_))
        ));
    }

    #[test]
    fn fps_hint_becomes_the_requested_rate() {
        let pw = PipeWireCapture::new(0, 1920, 1080, 90);
        assert_eq!(pw.config.offer.preferred_fps, 90);
        assert!(pw.config.offer.max_fps >= 90);
    }
}
