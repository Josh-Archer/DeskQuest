//! Host-side session registry: issues resume tokens and keeps a dropped session's resources
//! (virtual monitors, portal session, encoders) alive for a grace period.
//!
//! Pure state machine with caller-supplied time, so it is testable without a network or clock.

use std::time::{Duration, Instant};

use linux_quest_protocol::{ResumeStatus, SessionToken, SESSION_TOKEN_LEN};

/// What the host keeps per session; implemented over the real display/capture/encoder handles.
pub trait SessionResources {
    /// Release every held key/button so nothing stays stuck while the client is away.
    fn release_input(&mut self);
    /// Tear down monitors, the portal session and encoders.
    fn teardown(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Connected,
    Dropped { since: Instant },
}

struct Entry<R> {
    token: SessionToken,
    link: Link,
    resources: R,
}

/// Generates a token from OS randomness.
pub fn random_token() -> std::io::Result<SessionToken> {
    use std::io::Read;
    let mut b = [0u8; SESSION_TOKEN_LEN];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(SessionToken::from_bytes(b))
}

/// Single-client registry (the host serves one headset at a time).
pub struct SessionRegistry<R: SessionResources> {
    grace: Duration,
    next_token: Box<dyn FnMut() -> SessionToken + Send>,
    session: Option<Entry<R>>,
}

impl<R: SessionResources> SessionRegistry<R> {
    pub fn new(grace: Duration, next_token: Box<dyn FnMut() -> SessionToken + Send>) -> Self {
        Self {
            grace,
            next_token,
            session: None,
        }
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    pub fn resources(&mut self) -> Option<&mut R> {
        self.session.as_mut().map(|e| &mut e.resources)
    }

    pub fn is_dropped(&self) -> bool {
        matches!(
            self.session.as_ref().map(|e| e.link),
            Some(Link::Dropped { .. })
        )
    }

    /// Handles a client hello. A matching token resumes (even if the old link was never seen to
    /// drop, as in a USB -> Wi-Fi handover) and keeps the resources; anything else tears down the
    /// previous session and builds a new one with `create`.
    pub fn handshake(
        &mut self,
        offered: Option<SessionToken>,
        now: Instant,
        create: impl FnOnce() -> R,
    ) -> (SessionToken, ResumeStatus) {
        self.expire(now);
        if let (Some(offered), Some(entry)) = (offered, self.session.as_mut()) {
            if tokens_equal(&offered, &entry.token) {
                if entry.link == Link::Connected {
                    // Handover: the previous link is stale, drop whatever it was holding.
                    entry.resources.release_input();
                }
                entry.link = Link::Connected;
                return (entry.token, ResumeStatus::Resumed);
            }
        }
        if let Some(mut old) = self.session.take() {
            old.resources.teardown();
        }
        let token = (self.next_token)();
        self.session = Some(Entry {
            token,
            link: Link::Connected,
            resources: create(),
        });
        let status = if offered.is_some() {
            ResumeStatus::Expired
        } else {
            ResumeStatus::New
        };
        (token, status)
    }

    /// The link went away: release held input at once and start the grace timer.
    pub fn link_dropped(&mut self, now: Instant) {
        if let Some(e) = self.session.as_mut() {
            if e.link == Link::Connected {
                e.resources.release_input();
                e.link = Link::Dropped { since: now };
            }
        }
    }

    /// Tears the session down if it has been dropped for at least the grace period.
    /// Returns true if it did.
    pub fn expire(&mut self, now: Instant) -> bool {
        let expired = matches!(
            self.session.as_ref().map(|e| e.link),
            Some(Link::Dropped { since }) if now.saturating_duration_since(since) >= self.grace
        );
        if expired {
            if let Some(mut e) = self.session.take() {
                e.resources.teardown();
            }
        }
        expired
    }

    /// Ends the session immediately (explicit disconnect).
    pub fn close(&mut self) {
        if let Some(mut e) = self.session.take() {
            e.resources.teardown();
        }
    }
}

/// Constant-time compare so token guessing cannot use timing.
fn tokens_equal(a: &SessionToken, b: &SessionToken) -> bool {
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Debug)]
    struct Log {
        released: u32,
        torn_down: u32,
    }

    struct Fake(Arc<Mutex<Log>>, u32);

    impl SessionResources for Fake {
        fn release_input(&mut self) {
            self.0.lock().unwrap().released += 1;
        }
        fn teardown(&mut self) {
            self.0.lock().unwrap().torn_down += 1;
        }
    }

    fn registry(log: &Arc<Mutex<Log>>) -> (SessionRegistry<Fake>, Arc<Mutex<u32>>) {
        let counter = Arc::new(Mutex::new(0u32));
        let c = counter.clone();
        let _ = log;
        let reg = SessionRegistry::new(
            Duration::from_secs(30),
            Box::new(move || {
                let mut n = c.lock().unwrap();
                *n += 1;
                SessionToken::from_bytes([*n as u8; SESSION_TOKEN_LEN])
            }),
        );
        (reg, counter)
    }

    #[test]
    fn new_session_then_resume_keeps_resources() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        let t0 = Instant::now();
        let (tok, st) = reg.handshake(None, t0, || Fake(log.clone(), 7));
        assert_eq!(st, ResumeStatus::New);
        reg.link_dropped(t0);
        assert!(reg.is_dropped());
        assert_eq!(log.lock().unwrap().released, 1);

        let (tok2, st) = reg.handshake(Some(tok), t0 + Duration::from_secs(2), || {
            panic!("must not rebuild monitors on resume")
        });
        assert_eq!(st, ResumeStatus::Resumed);
        assert_eq!(tok2, tok);
        assert_eq!(reg.resources().unwrap().1, 7);
        assert_eq!(log.lock().unwrap().torn_down, 0);
        assert!(!reg.is_dropped());
    }

    #[test]
    fn grace_timeout_tears_down_and_token_expires() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        let t0 = Instant::now();
        let (tok, _) = reg.handshake(None, t0, || Fake(log.clone(), 1));
        reg.link_dropped(t0);
        assert!(!reg.expire(t0 + Duration::from_secs(29)));
        assert!(reg.expire(t0 + Duration::from_secs(30)));
        assert_eq!(log.lock().unwrap().torn_down, 1);

        let (tok2, st) = reg.handshake(Some(tok), t0 + Duration::from_secs(31), || {
            Fake(log.clone(), 2)
        });
        assert_eq!(st, ResumeStatus::Expired);
        assert_ne!(tok2, tok);
    }

    #[test]
    fn wrong_token_replaces_session() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        let t0 = Instant::now();
        reg.handshake(None, t0, || Fake(log.clone(), 1));
        let bogus = SessionToken::from_bytes([0xee; SESSION_TOKEN_LEN]);
        let (_, st) = reg.handshake(Some(bogus), t0, || Fake(log.clone(), 2));
        assert_eq!(st, ResumeStatus::Expired);
        assert_eq!(log.lock().unwrap().torn_down, 1);
        assert_eq!(reg.resources().unwrap().1, 2);
    }

    #[test]
    fn handover_resumes_while_old_link_still_looks_up() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        let t0 = Instant::now();
        let (tok, _) = reg.handshake(None, t0, || Fake(log.clone(), 1));
        let (_, st) = reg.handshake(Some(tok), t0, || panic!("no rebuild"));
        assert_eq!(st, ResumeStatus::Resumed);
        assert_eq!(log.lock().unwrap().released, 1);
        assert_eq!(log.lock().unwrap().torn_down, 0);
    }

    #[test]
    fn repeated_drop_does_not_restart_grace_timer() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        let t0 = Instant::now();
        reg.handshake(None, t0, || Fake(log.clone(), 1));
        reg.link_dropped(t0);
        reg.link_dropped(t0 + Duration::from_secs(20));
        assert_eq!(log.lock().unwrap().released, 1);
        assert!(reg.expire(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn close_tears_down() {
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut reg, _) = registry(&log);
        reg.handshake(None, Instant::now(), || Fake(log.clone(), 1));
        reg.close();
        assert_eq!(log.lock().unwrap().torn_down, 1);
        assert!(reg.resources().is_none());
    }

    #[test]
    fn random_tokens_differ() {
        assert_ne!(random_token().unwrap(), random_token().unwrap());
    }
}
