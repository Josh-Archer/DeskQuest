//! Typed errors for the portal + PipeWire screen capture path.

use thiserror::Error;

/// Why screen capture through the xdg-desktop-portal could not start or had to stop.
///
/// Callers can match on this to decide between "ask again", "tell the user to enable the
/// portal" and "give up".
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ScreenCastError {
    /// No session bus, no `org.freedesktop.portal.Desktop` owner, or a portal without the
    /// ScreenCast interface (or a version too old to use).
    #[error("screen capture portal unavailable: {0}")]
    PortalUnavailable(String),

    /// The user dismissed the monitor picker (portal response code 1).
    #[error("screen capture was cancelled in the portal dialog")]
    UserCancelled,

    /// The portal refused the request (response code 2, or a D-Bus access error).
    #[error("screen capture was denied by the portal: {0}")]
    Denied(String),

    /// The portal answered but did not grant any stream.
    #[error("the portal did not return a screen cast stream")]
    NoStreams,

    /// A malformed or unexpected portal reply.
    #[error("portal protocol error: {0}")]
    Portal(String),

    /// The PipeWire connection or stream failed.
    #[error("PipeWire stream error: {0}")]
    Stream(String),

    /// The portal session was closed (the user stopped sharing, or the compositor did).
    #[error("the portal session was closed")]
    SessionClosed,

    /// The PipeWire node that was being captured disappeared.
    #[error("the captured PipeWire node was removed")]
    NodeRemoved,

    /// The capture was stopped by the caller.
    #[error("capture stopped")]
    Stopped,

    /// The stream connected but no video format was negotiated in time.
    #[error("no video format was negotiated within {0:?}")]
    NegotiationTimeout(std::time::Duration),

    /// This build has no PipeWire screencast support.
    #[error("built without the `pipewire-screencast` feature")]
    FeatureDisabled,

    /// Local I/O failure (restore-token file, fd handling).
    #[error("I/O error: {0}")]
    Io(String),
}

impl ScreenCastError {
    /// True when retrying without the stored restore token might succeed: the portal
    /// rejected or ignored the token rather than the user declining or the portal being
    /// absent.
    pub(crate) fn token_may_be_stale(&self) -> bool {
        matches!(self, Self::Denied(_) | Self::NoStreams | Self::Portal(_))
    }
}
