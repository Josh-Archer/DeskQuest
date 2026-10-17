//! Multicast lock seam for LAN discovery.
//!
//! On Android, Wi-Fi drivers drop multicast packets unless an app holds
//! `WifiManager.MulticastLock`. The discovery browse must run while that lock is held. The
//! platform implementation lives in `android_jni`; tests use [`MockMulticastLock`].

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{ClientError, ClientResult};

/// Holds the platform's multicast reception permission while discovery browses.
pub trait MulticastLock: Send {
    /// Takes the lock. Calling it while already held is a no-op.
    fn acquire(&mut self) -> ClientResult<()>;

    /// Releases the lock. Calling it while not held is a no-op.
    fn release(&mut self) -> ClientResult<()>;

    /// `true` while this handle holds the lock.
    fn is_held(&self) -> bool;
}

#[derive(Debug, Default)]
struct MockLockState {
    held: bool,
    acquire_calls: usize,
    release_calls: usize,
    fail_acquire: Option<String>,
}

/// In-memory multicast lock. Clones share state, so a test can keep one handle while the
/// discovery owns another and inspect what happened.
#[derive(Debug, Clone, Default)]
pub struct MockMulticastLock {
    state: Arc<Mutex<MockLockState>>,
}

impl MockMulticastLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every subsequent `acquire` fail with `message`.
    pub fn fail_acquire(&self, message: impl Into<String>) {
        self.state.lock().fail_acquire = Some(message.into());
    }

    /// Number of successful `acquire` calls that actually took the lock.
    pub fn acquire_count(&self) -> usize {
        self.state.lock().acquire_calls
    }

    /// Number of `release` calls that actually dropped the lock.
    pub fn release_count(&self) -> usize {
        self.state.lock().release_calls
    }
}

impl MulticastLock for MockMulticastLock {
    fn acquire(&mut self) -> ClientResult<()> {
        let mut state = self.state.lock();
        if state.held {
            return Ok(());
        }
        if let Some(message) = &state.fail_acquire {
            return Err(ClientError::Android(message.clone()));
        }
        state.held = true;
        state.acquire_calls += 1;
        Ok(())
    }

    fn release(&mut self) -> ClientResult<()> {
        let mut state = self.state.lock();
        if state.held {
            state.held = false;
            state.release_calls += 1;
        }
        Ok(())
    }

    fn is_held(&self) -> bool {
        self.state.lock().held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_are_idempotent() {
        let mut lock = MockMulticastLock::new();
        lock.acquire().unwrap();
        lock.acquire().unwrap();
        assert!(lock.is_held());
        assert_eq!(lock.acquire_count(), 1);

        lock.release().unwrap();
        lock.release().unwrap();
        assert!(!lock.is_held());
        assert_eq!(lock.release_count(), 1);
    }

    #[test]
    fn clones_share_state() {
        let mut owner = MockMulticastLock::new();
        let observer = owner.clone();
        owner.acquire().unwrap();
        assert!(observer.is_held());
    }

    #[test]
    fn injected_failure_leaves_lock_released() {
        let mut lock = MockMulticastLock::new();
        lock.fail_acquire("WifiManager unavailable");
        assert!(lock.acquire().is_err());
        assert!(!lock.is_held());
    }
}
