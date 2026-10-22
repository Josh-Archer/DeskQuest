//! Whether the user's screen is locked.
//!
//! [`AutoCapture`](crate::AutoCapture) asks before it restores a lost screen capture. GNOME
//! refuses new screen casts while the screen is locked ("Session creation inhibited"), and its
//! portal then queues the share dialog behind the lock screen, where it waits until our request
//! times out or the user unlocks and sees it. So a restore is only tried once the screen is
//! unlocked, when the stored restore token works without a dialog.

use std::sync::Arc;

use async_trait::async_trait;

/// Tells whether the screen is locked.
#[async_trait]
pub trait ScreenLock: Send + Sync {
    /// `Some(true)` while the screen is locked, `Some(false)` while it is not, `None` when this
    /// cannot be told (then callers act as if it were unlocked).
    async fn is_locked(&self) -> Option<bool>;
}

/// A [`ScreenLock`] that never knows.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnknownScreenLock;

#[async_trait]
impl ScreenLock for UnknownScreenLock {
    async fn is_locked(&self) -> Option<bool> {
        None
    }
}

/// The lock state of the user's desktop session: the D-Bus screensaver service when this
/// build has D-Bus support, otherwise [`UnknownScreenLock`].
pub fn session_screen_lock() -> Arc<dyn ScreenLock> {
    #[cfg(feature = "portal")]
    {
        Arc::new(dbus::ScreenSaverLock::default())
    }
    #[cfg(not(feature = "portal"))]
    {
        Arc::new(UnknownScreenLock)
    }
}

#[cfg(feature = "portal")]
mod dbus {
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::OnceCell;
    use zbus::Connection;

    use super::ScreenLock;

    /// Services answering `GetActive() -> b` (true while the lock screen or screen shield is
    /// up): GNOME Shell's own first, then the freedesktop one (KDE and others).
    const SERVICES: [(&str, &str); 2] = [
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
        (
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
        ),
    ];

    /// How long one query may take before the state counts as unknown.
    const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

    /// Asks the session's screensaver service over D-Bus. The connection is opened on first
    /// use and kept, so polling costs one method call.
    #[derive(Default)]
    pub struct ScreenSaverLock {
        conn: OnceCell<Option<Connection>>,
    }

    impl ScreenSaverLock {
        async fn connection(&self) -> Option<&Connection> {
            self.conn
                .get_or_init(|| async {
                    match Connection::session().await {
                        Ok(conn) => Some(conn),
                        Err(e) => {
                            tracing::debug!("screen lock state unknown: no session bus: {e}");
                            None
                        }
                    }
                })
                .await
                .as_ref()
        }
    }

    #[async_trait]
    impl ScreenLock for ScreenSaverLock {
        async fn is_locked(&self) -> Option<bool> {
            let conn = self.connection().await?;
            for (service, path) in SERVICES {
                let call = conn.call_method(Some(service), path, Some(service), "GetActive", &());
                let reply = match tokio::time::timeout(QUERY_TIMEOUT, call).await {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(e)) => {
                        tracing::trace!("{service}.GetActive: {e}");
                        continue;
                    }
                    Err(_) => continue,
                };
                if let Ok(active) = reply.body().deserialize::<bool>() {
                    return Some(active);
                }
            }
            None
        }
    }
}
