//! Quest client LAN discovery of hosts, with manual IP entry as the fallback.
//!
//! [`ClientDiscovery`] browses through a transport [`DiscoveryBackend`] while holding a
//! [`MulticastLock`], merges what answers into a [`HostDirectory`], and lets the caller connect
//! to a host by name. Nothing here renders UI. A caller that wants a list shows
//! [`ClientDiscovery::hosts`] and passes the chosen name to [`ClientDiscovery::select`] or
//! [`apply_selection`].
//!
//! Manual entry is independent of discovery: [`parse_manual_host`] turns typed text into an
//! address, and `ClientConfig::host_addr` can always be set directly.
//!
//! Port rule. Wi-Fi/LAN hosts (manual entry and discovery) use the host's control port,
//! [`MANUAL_DEFAULT_PORT`] = 48444, the port the daemon's control listener binds. The USB/ADB
//! path uses the same port: `adb reverse tcp:48444 tcp:48444` makes the host control port
//! reachable as `127.0.0.1:48444` (the `ClientConfig` default).
//!
//! [`scheduler::DiscoveryScheduler`] re-scans on a period while the caller reports the client as
//! disconnected.
//!
//! mDNS cannot reliably detect blocked multicast (see `linux_quest_transport::discovery`). An
//! empty browse therefore does not prove that no host exists, and the UI should always keep the
//! manual entry path visible.

pub mod android_jni;
pub mod multicast_lock;
pub mod scheduler;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use tracing::warn;

use linux_quest_transport::{
    refresh_with_clock, DiscoveredHost, DiscoveryBackend, DiscoveryError, HostDirectory,
    DEFAULT_CONTROL_PORT,
};

use crate::config::ClientConfig;
use crate::error::{ClientError, ClientResult};

pub use android_jni::AndroidMulticastLock;
pub use multicast_lock::{MockMulticastLock, MulticastLock};

#[cfg(feature = "mdns")]
pub use linux_quest_transport::MdnsDiscoveryBackend;

/// How long one browse listens for answers. mDNS responders reply within about a second, so
/// two seconds covers them while keeping the multicast lock (and the Wi-Fi radio awake) short.
/// With [`scheduler::DEFAULT_RESCAN_PERIOD`] of 15 s the lock is held about 13% of the time
/// while disconnected, down from about 60% at 3 s every 5 s.
pub const DEFAULT_BROWSE_WINDOW: Duration = Duration::from_secs(2);

/// How long a host stays listed after it last answered. It must span several scans: the
/// transport default (15 s) equals the default re-scan period, so hosts would drop out of the
/// list between scans. Three periods tolerates a missed scan or a lost mDNS answer.
pub const HOST_STALE_AFTER: Duration = Duration::from_secs(45);

/// Browses for hosts and remembers the ones that answer.
///
/// Generic over the backend and the lock so tests use the mock backend and mock lock, and
/// Android uses `MdnsDiscoveryBackend` and [`AndroidMulticastLock`].
pub struct ClientDiscovery<B: DiscoveryBackend, L: MulticastLock> {
    backend: B,
    lock: L,
    lock_held: bool,
    window: Duration,
    directory: HostDirectory,
}

impl<B: DiscoveryBackend, L: MulticastLock> ClientDiscovery<B, L> {
    pub fn new(backend: B, lock: L) -> Self {
        Self {
            backend,
            lock,
            lock_held: false,
            window: DEFAULT_BROWSE_WINDOW,
            directory: HostDirectory::new(HOST_STALE_AFTER),
        }
    }

    /// Overrides how long each [`scan`](Self::scan) listens.
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Runs one browse and returns how many hosts answered.
    ///
    /// The multicast lock is held for the duration of the browse and released afterwards on
    /// every path. A failed acquire is logged and the browse still runs. Some Wi-Fi stacks
    /// deliver multicast anyway, and the browse simply finds nothing on the rest.
    ///
    /// Returns `Err` only when the backend cannot browse. `MulticastUnavailable` means the
    /// caller should rely on manual entry.
    pub async fn scan(&mut self) -> Result<usize, DiscoveryError> {
        self.scan_with_clock(Instant::now).await
    }

    /// Like [`scan`](Self::scan), with the observation time supplied by `clock` (for tests).
    pub async fn scan_with_clock(
        &mut self,
        clock: impl FnOnce() -> Instant,
    ) -> Result<usize, DiscoveryError> {
        if !self.lock_held {
            match self.lock.acquire() {
                Ok(()) => self.lock_held = true,
                Err(e) => {
                    warn!(error = %e, "Could not take multicast lock; discovery may find nothing")
                }
            }
        }

        let result =
            refresh_with_clock(&mut self.backend, &mut self.directory, self.window, clock).await;

        self.release_lock();
        result
    }

    fn release_lock(&mut self) {
        if !self.lock_held {
            return;
        }
        // Mark released even if the platform call fails, so the next scan can acquire again.
        self.lock_held = false;
        if let Err(e) = self.lock.release() {
            warn!(error = %e, "Failed to release multicast lock");
        }
    }

    /// Hosts that answered recently, ordered by name.
    pub fn hosts(&self) -> Vec<&DiscoveredHost> {
        self.hosts_at(Instant::now())
    }

    /// Hosts still fresh at `now`, ordered by name.
    pub fn hosts_at(&self, now: Instant) -> Vec<&DiscoveredHost> {
        self.directory.hosts(now)
    }

    /// Looks up one fresh host by its instance name.
    pub fn host(&self, name: &str) -> Option<&DiscoveredHost> {
        self.directory.get(name, Instant::now())
    }

    /// Returns the control address of a fresh host, for connecting to it.
    pub fn select(&self, name: &str) -> ClientResult<SocketAddr> {
        let host = self
            .host(name)
            .ok_or_else(|| ClientError::Transport(format!("no discovered host named {name:?}")))?;
        host.control_addr().ok_or_else(|| {
            ClientError::Transport(format!("discovered host {name:?} has no address"))
        })
    }

    /// Whether this discovery currently holds the multicast lock.
    pub fn is_lock_held(&self) -> bool {
        self.lock_held
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn directory(&self) -> &HostDirectory {
        &self.directory
    }
}

impl<B: DiscoveryBackend, L: MulticastLock> Drop for ClientDiscovery<B, L> {
    fn drop(&mut self) {
        // Covers a scan future that was cancelled while the lock was held.
        self.release_lock();
    }
}

/// Builds a discovery that browses over real mDNS. Fails with `MulticastUnavailable` if the mDNS
/// daemon cannot start. Requires the `mdns` feature.
#[cfg(feature = "mdns")]
pub fn mdns_discovery<L: MulticastLock>(
    lock: L,
) -> Result<ClientDiscovery<MdnsDiscoveryBackend, L>, DiscoveryError> {
    Ok(ClientDiscovery::new(MdnsDiscoveryBackend::new()?, lock))
}

/// Points `config` at a discovered host: `host_addr` becomes its control address (port 48444),
/// chosen by [`DiscoveredHost::ordered_addresses`] so a LAN address beats a Docker bridge.
///
/// The host's other addresses go to `extra_host_ips`, so the UDP source filter accepts video
/// from any interface the host answered on, not only the one chosen for control.
///
/// This does not change `transport_mode`. In `Udp` mode the receive path accepts video only from
/// those IPs (`android::udp_source_filter`), so selecting a host there makes it the video
/// source. ADB mode is unchanged and does not use discovery.
pub fn apply_selection(config: &mut ClientConfig, host: &DiscoveredHost) -> ClientResult<()> {
    let addr = host
        .control_addr()
        .ok_or_else(|| ClientError::Transport(format!("host {:?} has no address", host.name)))?;
    config.host_addr = addr;
    config.extra_host_ips = host
        .ordered_addresses()
        .into_iter()
        .filter(|ip| *ip != addr.ip())
        .collect();
    Ok(())
}

/// Parses a host typed by hand. Accepts `192.168.1.20`, `192.168.1.20:48444`, `[fe80::1]:48444`,
/// `[fe80::1]` or a bare IPv6 address. Without a port, `default_port` is used.
pub fn parse_manual_host(input: &str, default_port: u16) -> ClientResult<SocketAddr> {
    let text = input.trim();
    if text.is_empty() {
        return Err(ClientError::Config("host address is empty".into()));
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return checked_port(addr);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return checked_port(SocketAddr::new(ip, default_port));
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        if let Ok(ip) = inner.parse::<std::net::Ipv6Addr>() {
            return checked_port(SocketAddr::new(IpAddr::V6(ip), default_port));
        }
    }
    Err(ClientError::Config(format!(
        "invalid host address {text:?}; expected an IP address with an optional port"
    )))
}

fn checked_port(addr: SocketAddr) -> ClientResult<SocketAddr> {
    if addr.port() == 0 {
        return Err(ClientError::Config("host port must be non-zero".into()));
    }
    Ok(addr)
}

/// Default port for hosts entered by hand. It is the host's control listener port
/// (`DEFAULT_CONTROL_PORT`, 48444), the same port discovery advertises. The USB/ADB path uses the
/// same port (`adb reverse tcp:48444 tcp:48444`).
pub const MANUAL_DEFAULT_PORT: u16 = DEFAULT_CONTROL_PORT;

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::VideoCodec;
    use linux_quest_transport::MockDiscoveryBackend;

    fn host(name: &str, octet: u8) -> DiscoveredHost {
        DiscoveredHost {
            name: name.to_string(),
            addresses: vec![IpAddr::from([192, 168, 1, octet])],
            control_port: DEFAULT_CONTROL_PORT,
            video_port: 48445,
            input_port: 48442,
            codecs: vec![VideoCodec::Av1],
        }
    }

    #[tokio::test]
    async fn scan_lists_hosts_and_selects_by_name() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10), host("attic", 11)]);
        let lock = MockMulticastLock::new();
        let mut discovery = ClientDiscovery::new(backend, lock.clone());

        assert_eq!(discovery.scan().await.unwrap(), 2);

        let names: Vec<_> = discovery.hosts().iter().map(|h| h.name.clone()).collect();
        assert_eq!(names, ["attic", "den"]);
        assert_eq!(
            discovery.select("den").unwrap(),
            SocketAddr::from(([192, 168, 1, 10], DEFAULT_CONTROL_PORT))
        );
        assert!(discovery.select("missing").is_err());
    }

    #[tokio::test]
    async fn lock_is_held_during_browse_and_released_after() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let lock = MockMulticastLock::new();
        let mut discovery = ClientDiscovery::new(backend, lock.clone());

        discovery.scan().await.unwrap();

        assert_eq!(lock.acquire_count(), 1);
        assert_eq!(lock.release_count(), 1);
        assert!(!lock.is_held());
        assert!(!discovery.is_lock_held());
    }

    #[tokio::test]
    async fn lock_is_released_when_browse_fails() {
        let backend = MockDiscoveryBackend::new().with_multicast_available(false);
        let lock = MockMulticastLock::new();
        let mut discovery = ClientDiscovery::new(backend, lock.clone());

        let err = discovery.scan().await.unwrap_err();
        assert!(matches!(err, DiscoveryError::MulticastUnavailable(_)));
        assert!(!lock.is_held());
    }

    #[tokio::test]
    async fn failed_lock_acquire_still_browses() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let lock = MockMulticastLock::new();
        lock.fail_acquire("no WifiManager");
        let mut discovery = ClientDiscovery::new(backend, lock.clone());

        assert_eq!(discovery.scan().await.unwrap(), 1);
        assert_eq!(lock.acquire_count(), 0);
        assert_eq!(lock.release_count(), 0);
    }

    #[test]
    fn dropping_discovery_releases_held_lock() {
        let lock = MockMulticastLock::new();
        let mut owned = lock.clone();
        owned.acquire().unwrap();
        // Simulate a scan cancelled mid-browse: the lock is held, and the flag says so.
        let mut discovery = ClientDiscovery::new(MockDiscoveryBackend::new(), owned);
        discovery.lock_held = true;
        drop(discovery);
        assert!(!lock.is_held());
    }

    #[tokio::test]
    async fn hosts_stay_listed_across_the_default_rescan_period() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let mut discovery = ClientDiscovery::new(backend, MockMulticastLock::new());
        let t0 = Instant::now();
        discovery.scan_with_clock(|| t0).await.unwrap();
        let next_scan = t0 + scheduler::DEFAULT_RESCAN_PERIOD + DEFAULT_BROWSE_WINDOW;
        assert_eq!(discovery.hosts_at(next_scan).len(), 1);
    }

    #[tokio::test]
    async fn hosts_that_stop_answering_expire() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let mut discovery = ClientDiscovery::new(backend, MockMulticastLock::new());
        let t0 = Instant::now();
        discovery.scan_with_clock(|| t0).await.unwrap();
        assert_eq!(discovery.hosts_at(t0).len(), 1);

        discovery.backend.set_peers(Vec::new());
        let later = t0 + Duration::from_secs(60);
        discovery.scan_with_clock(|| later).await.unwrap();
        assert!(discovery.hosts_at(later).is_empty());
    }

    #[tokio::test]
    async fn multiple_hosts_with_same_name_collapse_to_latest() {
        let backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let mut discovery = ClientDiscovery::new(backend, MockMulticastLock::new());
        discovery.scan().await.unwrap();

        discovery.backend.set_peers([host("den", 42)]);
        discovery.scan().await.unwrap();

        assert_eq!(discovery.hosts().len(), 1);
        assert_eq!(
            discovery.select("den").unwrap().ip(),
            IpAddr::from([192, 168, 1, 42])
        );
    }

    #[tokio::test]
    async fn selection_applies_control_address_to_config() {
        let mut config = ClientConfig::default();
        let before_mode = config.transport_mode;
        apply_selection(&mut config, &host("den", 10)).unwrap();
        assert_eq!(
            config.host_addr,
            SocketAddr::from(([192, 168, 1, 10], DEFAULT_CONTROL_PORT))
        );
        assert_eq!(config.transport_mode, before_mode);
    }

    #[test]
    fn selection_prefers_lan_address_over_docker_and_keeps_all_for_filter() {
        let mut config = ClientConfig::default();
        let mut multi = host("den", 10);
        multi.addresses = vec![
            IpAddr::from([172, 17, 0, 1]),
            IpAddr::from([192, 168, 1, 10]),
        ];
        apply_selection(&mut config, &multi).unwrap();
        assert_eq!(
            config.host_addr,
            SocketAddr::from(([192, 168, 1, 10], DEFAULT_CONTROL_PORT))
        );
        assert_eq!(config.extra_host_ips, vec![IpAddr::from([172, 17, 0, 1])]);

        config.transport_mode = crate::config::TransportMode::Udp;
        let filter = crate::android::udp_source_filter(&config).unwrap();
        let accepts = |ip: [u8; 4]| {
            crate::android::accept_udp_source(Some(&filter), SocketAddr::from((ip, 50000)))
        };
        assert!(accepts([192, 168, 1, 10]));
        assert!(accepts([172, 17, 0, 1]));
        assert!(!accepts([192, 168, 1, 99]));
    }

    #[test]
    fn selection_rejects_host_without_address() {
        let mut config = ClientConfig::default();
        let mut no_addr = host("den", 10);
        no_addr.addresses.clear();
        assert!(apply_selection(&mut config, &no_addr).is_err());
    }

    #[test]
    fn manual_entry_accepts_ip_with_and_without_port() {
        assert_eq!(
            parse_manual_host("192.168.1.20", MANUAL_DEFAULT_PORT).unwrap(),
            SocketAddr::from(([192, 168, 1, 20], MANUAL_DEFAULT_PORT))
        );
        assert_eq!(
            parse_manual_host("  192.168.1.20:50000 ", MANUAL_DEFAULT_PORT).unwrap(),
            SocketAddr::from(([192, 168, 1, 20], 50000))
        );
        assert_eq!(
            parse_manual_host("[fe80::1]:48444", MANUAL_DEFAULT_PORT).unwrap(),
            "[fe80::1]:48444".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_manual_host("[fe80::1]", MANUAL_DEFAULT_PORT).unwrap(),
            SocketAddr::new("fe80::1".parse().unwrap(), MANUAL_DEFAULT_PORT)
        );
        assert!(parse_manual_host("[192.168.1.1]", MANUAL_DEFAULT_PORT).is_err());
        assert!(parse_manual_host("[fe80::1", MANUAL_DEFAULT_PORT).is_err());
        assert_eq!(
            parse_manual_host("fe80::1", MANUAL_DEFAULT_PORT)
                .unwrap()
                .port(),
            MANUAL_DEFAULT_PORT
        );
    }

    #[test]
    fn manual_entry_rejects_garbage_and_zero_port() {
        assert!(parse_manual_host("", MANUAL_DEFAULT_PORT).is_err());
        assert!(parse_manual_host("living-room", MANUAL_DEFAULT_PORT).is_err());
        assert!(parse_manual_host("192.168.1.20:0", MANUAL_DEFAULT_PORT).is_err());
    }
}
