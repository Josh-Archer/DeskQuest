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

#[cfg(test)]
mod tests {
    use super::*;

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
