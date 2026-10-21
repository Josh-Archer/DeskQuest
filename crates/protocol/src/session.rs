//! Session identity used to resume a stream after a link drop (Wi-Fi loss, headset sleep,
//! USB <-> Wi-Fi handover) without renegotiating monitors, layout or the capture session.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Length of a session token in bytes.
pub const SESSION_TOKEN_LEN: usize = 16;

/// Default time the host keeps a dropped session alive, in milliseconds.
pub const DEFAULT_RESUME_GRACE_MS: u32 = 30_000;

/// Opaque, host-issued session identifier. Generated from OS randomness by the host.
///
/// It is a bearer secret for resuming one session, so `Debug` and `Display` only show a short
/// prefix to keep full tokens out of logs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionToken([u8; SESSION_TOKEN_LEN]);

impl SessionToken {
    pub const fn from_bytes(bytes: [u8; SESSION_TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; SESSION_TOKEN_LEN] {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionToken({self})")
    }
}

impl fmt::Display for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}…",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

/// How the host answered the client's resume request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeStatus {
    /// A new session was created (no token offered, or the host had nothing to resume).
    New,
    /// The offered token matched; monitors, layout, capture and encoders were kept.
    Resumed,
    /// A token was offered but the session had already been torn down or never existed. The
    /// client must treat this as a fresh session and re-request its monitors.
    Expired,
}

impl ResumeStatus {
    /// True when the session survived the drop, so both sides must re-send their current
    /// clipboard (changes made while the link was down were never delivered).
    pub const fn needs_clipboard_resync(self) -> bool {
        matches!(self, ResumeStatus::Resumed)
    }
}

/// Client-side memory of the session token, kept across reconnects.
#[derive(Debug, Default, Clone)]
pub struct ClientSession {
    token: Option<SessionToken>,
}

impl ClientSession {
    /// Token to put in `ClientHandshake::resume_token` for the next connect attempt.
    pub fn resume_token(&self) -> Option<SessionToken> {
        self.token
    }

    /// Records the host's answer. Always adopts the issued token (a fresh session after an
    /// `Expired` answer has a new one). Returns the status so the caller can decide whether it
    /// must re-request monitors (`New`/`Expired`) or just resync the clipboard (`Resumed`).
    pub fn on_server_hello(&mut self, hello: &crate::ServerHandshake) -> ResumeStatus {
        self.token = Some(hello.session_token);
        hello.resume_status
    }

    /// Forgets the token, e.g. when the user explicitly disconnects.
    pub fn clear(&mut self) {
        self.token = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(token: u8, status: ResumeStatus) -> crate::ServerHandshake {
        crate::ServerHandshake {
            server_version: 1,
            selected_codec: crate::VideoCodec::Av1,
            negotiated_bitrate_mbps: 30,
            displays: vec![],
            session_token: SessionToken::from_bytes([token; SESSION_TOKEN_LEN]),
            resume_status: status,
            resume_grace_ms: DEFAULT_RESUME_GRACE_MS,
        }
    }

    #[test]
    fn client_session_adopts_token_and_reports_status() {
        let mut c = ClientSession::default();
        assert_eq!(c.resume_token(), None);
        assert_eq!(
            c.on_server_hello(&hello(1, ResumeStatus::New)),
            ResumeStatus::New
        );
        assert_eq!(c.resume_token().unwrap().as_bytes()[0], 1);
        // Host expired the old session and issued a new token.
        assert_eq!(
            c.on_server_hello(&hello(2, ResumeStatus::Expired)),
            ResumeStatus::Expired
        );
        assert_eq!(c.resume_token().unwrap().as_bytes()[0], 2);
        c.clear();
        assert_eq!(c.resume_token(), None);
    }

    #[test]
    fn only_resumed_needs_clipboard_resync() {
        assert!(ResumeStatus::Resumed.needs_clipboard_resync());
        assert!(!ResumeStatus::New.needs_clipboard_resync());
        assert!(!ResumeStatus::Expired.needs_clipboard_resync());
    }

    #[test]
    fn token_roundtrips_through_bincode() {
        let t = SessionToken::from_bytes([7; SESSION_TOKEN_LEN]);
        let bytes = bincode::serialize(&t).unwrap();
        assert_eq!(bincode::deserialize::<SessionToken>(&bytes).unwrap(), t);
    }

    #[test]
    fn token_debug_does_not_leak_full_secret() {
        let t = SessionToken::from_bytes([0xab; SESSION_TOKEN_LEN]);
        let shown = format!("{t:?} {t}");
        assert!(!shown.contains(&"ab".repeat(8)));
    }
}
