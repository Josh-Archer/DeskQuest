//! Periodic LAN re-scan while the client is not connected to a host.
//!
//! [`DiscoveryScheduler`] calls [`ClientDiscovery::scan`] on a fixed period, but only while the
//! `connected` signal is false. Connection state belongs to the caller. Scanning stops for as
//! long as the client is connected, and resumes on the next tick after it disconnects.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;

use linux_quest_transport::DiscoveryBackend;

use super::{ClientDiscovery, MulticastLock};

/// How often to re-scan while disconnected.
pub const DEFAULT_RESCAN_PERIOD: Duration = Duration::from_secs(5);

/// Shortest period accepted. A zero period would make the tick interval panic, and very short
/// periods would hold the multicast lock almost continuously.
pub const MIN_RESCAN_PERIOD: Duration = Duration::from_millis(100);

/// Runs [`ClientDiscovery::scan`] on a period while disconnected.
pub struct DiscoveryScheduler<B: DiscoveryBackend, L: MulticastLock> {
    discovery: ClientDiscovery<B, L>,
    period: Duration,
}

impl<B: DiscoveryBackend, L: MulticastLock> DiscoveryScheduler<B, L> {
    /// `period` is clamped to at least [`MIN_RESCAN_PERIOD`].
    pub fn new(discovery: ClientDiscovery<B, L>, period: Duration) -> Self {
        Self {
            discovery,
            period: period.max(MIN_RESCAN_PERIOD),
        }
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    pub fn discovery(&self) -> &ClientDiscovery<B, L> {
        &self.discovery
    }

    pub fn into_discovery(self) -> ClientDiscovery<B, L> {
        self.discovery
    }

    /// Scans every `period` while `connected` is false, until `stop` completes.
    ///
    /// A tick that arrives while connected does nothing. A scan that fails (for example, the
    /// backend lost multicast) is logged at debug level and the next tick tries again. A
    /// browse that is running when `stop` completes is cancelled. Returns how many scans were
    /// attempted.
    pub async fn run(
        &mut self,
        connected: &watch::Receiver<bool>,
        stop: impl Future<Output = ()>,
    ) -> usize {
        let mut ticker = interval(self.period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tokio::pin!(stop);

        let mut attempts = 0;
        loop {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                _ = ticker.tick() => {}
            }
            if *connected.borrow() {
                continue;
            }
            attempts += 1;
            tokio::select! {
                biased;
                _ = &mut stop => break,
                result = self.discovery.scan() => {
                    if let Err(e) = result {
                        debug!(error = %e, "LAN discovery scan failed; retrying on next tick");
                    }
                }
            }
        }
        attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::MockMulticastLock;
    use linux_quest_transport::{DiscoveredHost, MockDiscoveryBackend, DEFAULT_CONTROL_PORT};
    use std::net::IpAddr;
    use tokio::time::sleep;

    fn host(name: &str) -> DiscoveredHost {
        DiscoveredHost {
            name: name.to_string(),
            addresses: vec![IpAddr::from([192, 168, 1, 10])],
            control_port: DEFAULT_CONTROL_PORT,
            video_port: 0,
            input_port: 48442,
            codecs: vec![linux_quest_protocol::VideoCodec::Av1],
        }
    }

    fn scheduler(period: Duration) -> DiscoveryScheduler<MockDiscoveryBackend, MockMulticastLock> {
        let backend = MockDiscoveryBackend::new().with_peers([host("den")]);
        DiscoveryScheduler::new(
            ClientDiscovery::new(backend, MockMulticastLock::new()),
            period,
        )
    }

    // The tests run on tokio's paused clock: sleeps advance virtual time instantly, and the
    // mock backend's browse completes without waiting, so tick counts are exact.

    #[tokio::test(start_paused = true)]
    async fn scans_once_per_period_while_disconnected() {
        let mut sched = scheduler(Duration::from_secs(5));
        let (_tx, rx) = watch::channel(false);

        // Ticks at 0s, 5s, 10s, 15s before the stop at 16s.
        let attempts = sched.run(&rx, sleep(Duration::from_secs(16))).await;

        assert_eq!(attempts, 4);
        assert_eq!(sched.discovery().backend().browse_calls(), 4);
        assert_eq!(sched.discovery().hosts().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_scan_while_connected() {
        let mut sched = scheduler(Duration::from_secs(5));
        let (_tx, rx) = watch::channel(true);

        let attempts = sched.run(&rx, sleep(Duration::from_secs(60))).await;

        assert_eq!(attempts, 0);
        assert_eq!(sched.discovery().backend().browse_calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn pauses_while_connected_and_resumes_after_disconnect() {
        let mut sched = scheduler(Duration::from_secs(5));
        let (tx, rx) = watch::channel(false);

        // Scans at 0s and 5s. Connected from 6s to 21s, so the ticks at 10s, 15s and 20s are
        // skipped. Scans resume at 25s and 30s. Stop at 31s.
        let controller = async {
            sleep(Duration::from_secs(6)).await;
            tx.send(true).unwrap();
            sleep(Duration::from_secs(15)).await;
            tx.send(false).unwrap();
        };
        let (attempts, ()) =
            tokio::join!(sched.run(&rx, sleep(Duration::from_secs(31))), controller);

        assert_eq!(attempts, 4);
        assert_eq!(sched.discovery().backend().browse_calls(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_before_first_tick_scans_nothing() {
        let mut sched = scheduler(Duration::from_secs(5));
        let (_tx, rx) = watch::channel(false);

        let attempts = sched.run(&rx, std::future::ready(())).await;

        assert_eq!(attempts, 0);
        assert_eq!(sched.discovery().backend().browse_calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn failing_scans_keep_being_attempted() {
        let backend = MockDiscoveryBackend::new().with_multicast_available(false);
        let mut sched = DiscoveryScheduler::new(
            ClientDiscovery::new(backend, MockMulticastLock::new()),
            Duration::from_secs(5),
        );
        let (_tx, rx) = watch::channel(false);

        let attempts = sched.run(&rx, sleep(Duration::from_secs(16))).await;

        assert_eq!(attempts, 4);
        assert!(sched.discovery().hosts().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn lock_is_not_left_held_after_stop() {
        let lock = MockMulticastLock::new();
        let backend = MockDiscoveryBackend::new().with_peers([host("den")]);
        let mut sched = DiscoveryScheduler::new(
            ClientDiscovery::new(backend, lock.clone()),
            Duration::from_secs(5),
        );
        let (_tx, rx) = watch::channel(false);

        sched.run(&rx, sleep(Duration::from_secs(7))).await;

        assert!(!lock.is_held());
    }

    #[test]
    fn zero_period_is_clamped() {
        let sched = scheduler(Duration::ZERO);
        assert_eq!(sched.period(), MIN_RESCAN_PERIOD);
        assert_eq!(
            scheduler(DEFAULT_RESCAN_PERIOD).period(),
            DEFAULT_RESCAN_PERIOD
        );
    }
}
