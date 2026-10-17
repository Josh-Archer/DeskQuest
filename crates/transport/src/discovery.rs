//! LAN discovery of the host so the Quest client can find it without an IP address.
//!
//! The host advertises one DNS-SD service instance of type [`DISCOVERY_SERVICE_TYPE`].
//! The SRV port is the control port; the TXT record carries the remaining ports and the
//! supported video codecs:
//!
//! | Field          | Where     | Example       |
//! |----------------|-----------|---------------|
//! | instance name  | DNS-SD    | `living-room` |
//! | control port   | SRV port  | `48444`       |
//! | `proto`        | TXT       | `1`           |
//! | `video`        | TXT       | `48445` (example), omitted when the host has no video listener |
//! | `input`        | TXT       | `48442`       |
//! | `codecs`       | TXT       | `av1,hevc`    |
//!
//! The client browses for the same type, merges what it sees into a [`HostDirectory`] and
//! expires hosts that stop answering. The network-facing part lives behind [`DiscoveryBackend`]
//! so tests run against [`MockDiscoveryBackend`]; the real mDNS backend is compiled only with
//! the `mdns` cargo feature.
//!
//! [`DiscoveryError::MulticastUnavailable`] means "discovery cannot work, fall back to manual
//! entry". The mock produces it on demand. The real backend produces it only when the `mdns-sd`
//! daemon thread is gone (see `MdnsDiscoveryBackend` for exactly what is and is not detected);
//! a blocked or unjoinable multicast socket is *not* reliably reported, so a browse that finds
//! nothing is not proof that no hosts exist.

use async_trait::async_trait;
use linux_quest_protocol::VideoCodec;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use thiserror::Error;

/// DNS-SD service type used by linux-quest hosts (`_<name>._tcp`, name <= 15 chars).
pub const DISCOVERY_SERVICE_TYPE: &str = "_linuxquest._tcp.local.";

/// Version of the TXT record layout. Records advertising any other version are ignored.
pub const DISCOVERY_PROTOCOL_VERSION: u8 = 1;

/// Video port value meaning "no video listener". A host in this state omits the `video` TXT key,
/// and a client reads a parsed record with this value as "video not available".
pub const VIDEO_PORT_UNAVAILABLE: u16 = 0;

/// A host that has not been seen for this long is considered gone.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(15);

/// DNS labels are limited to 63 bytes.
const MAX_INSTANCE_NAME_BYTES: usize = 63;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    #[error("Multicast DNS is unavailable on this network: {0}")]
    MulticastUnavailable(String),

    #[error("Invalid host advertisement: {0}")]
    InvalidAdvertisement(String),

    #[error("Malformed discovery record: {0}")]
    MalformedRecord(String),

    #[error("Discovery backend error: {0}")]
    Backend(String),
}

/// What a host publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAdvertisement {
    /// Human-readable instance name shown in the client's host list.
    pub name: String,
    pub control_port: u16,
    pub video_port: u16,
    pub input_port: u16,
    /// Codecs the host can encode, in preference order.
    pub codecs: Vec<VideoCodec>,
}

impl HostAdvertisement {
    /// Rejects advertisements that cannot be published as a valid DNS-SD instance.
    pub fn validate(&self) -> Result<(), DiscoveryError> {
        let invalid = |msg: &str| Err(DiscoveryError::InvalidAdvertisement(msg.to_string()));
        if self.name.is_empty() {
            return invalid("instance name is empty");
        }
        if self.name.len() > MAX_INSTANCE_NAME_BYTES {
            return invalid("instance name exceeds 63 bytes");
        }
        // Dots and backslashes are escaped by DNS-SD, and the client does not un-escape the
        // instance name it browses back, so such a name would not round-trip.
        if self.name.contains(['.', '\\']) || self.name.chars().any(char::is_control) {
            return invalid(
                "instance name must not contain dots, backslashes or control characters",
            );
        }
        // The video port may be VIDEO_PORT_UNAVAILABLE; control and input must be real ports.
        if self.control_port == 0 || self.input_port == 0 {
            return invalid("control and input ports must be non-zero");
        }
        if self.codecs.is_empty() {
            return invalid("at least one codec must be advertised");
        }
        Ok(())
    }

    /// TXT key/value pairs for this advertisement (the control port lives in the SRV record).
    ///
    /// The `video` key is left out when `video_port` is [`VIDEO_PORT_UNAVAILABLE`], so a TXT
    /// record never names a port the host does not listen on.
    pub fn txt_properties(&self) -> HashMap<String, String> {
        let codecs = self
            .codecs
            .iter()
            .map(|c| codec_token(*c))
            .collect::<Vec<_>>()
            .join(",");
        let mut txt = HashMap::from([
            ("proto".to_string(), DISCOVERY_PROTOCOL_VERSION.to_string()),
            ("input".to_string(), self.input_port.to_string()),
            ("codecs".to_string(), codecs),
        ]);
        if self.video_port != VIDEO_PORT_UNAVAILABLE {
            txt.insert("video".to_string(), self.video_port.to_string());
        }
        txt
    }
}

/// A host seen on the LAN, as reconstructed from its DNS-SD records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub name: String,
    /// Sorted, de-duplicated. IPv4 sorts before IPv6.
    pub addresses: Vec<IpAddr>,
    pub control_port: u16,
    pub video_port: u16,
    pub input_port: u16,
    pub codecs: Vec<VideoCodec>,
}

impl DiscoveredHost {
    /// Builds a host from the parts of a resolved DNS-SD service.
    pub fn from_parts(
        name: &str,
        addresses: impl IntoIterator<Item = IpAddr>,
        control_port: u16,
        txt: &HashMap<String, String>,
    ) -> Result<Self, DiscoveryError> {
        let malformed = |msg: String| Err(DiscoveryError::MalformedRecord(msg));

        let proto = txt.get("proto").map(String::as_str).unwrap_or("");
        if proto != DISCOVERY_PROTOCOL_VERSION.to_string() {
            return malformed(format!("unsupported protocol version {proto:?}"));
        }
        if control_port == 0 {
            return malformed("control port is zero".to_string());
        }
        let video_port = parse_optional_video_port(txt)?;
        let input_port = parse_port(txt, "input")?;

        let codecs = txt
            .get("codecs")
            .map(String::as_str)
            .unwrap_or("")
            .split(',')
            .filter(|t| !t.is_empty())
            .map(|t| {
                parse_codec_token(t)
                    .ok_or_else(|| DiscoveryError::MalformedRecord(format!("unknown codec {t:?}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if codecs.is_empty() {
            return malformed("no codecs advertised".to_string());
        }

        let addresses: std::collections::BTreeSet<IpAddr> = addresses.into_iter().collect();
        if addresses.is_empty() {
            return malformed("no addresses resolved".to_string());
        }

        Ok(Self {
            name: name.to_string(),
            addresses: addresses.into_iter().collect(),
            control_port,
            video_port,
            input_port,
            codecs,
        })
    }

    /// Whether the host advertised a video listener. False when the host has none.
    pub fn video_available(&self) -> bool {
        self.video_port != VIDEO_PORT_UNAVAILABLE
    }

    /// Resolved addresses, best first, for choosing which one to connect to.
    ///
    /// A multi-homed host (Docker, VPN, virtual bridges) resolves to several addresses, and the
    /// sorted order alone can put `172.17.0.1` ahead of the LAN address. Ranking:
    /// private LAN ranges (`192.168/16`, `10/8`, `172.16/12` except the Docker bridge
    /// `172.17/16`), then other routable IPv4, then IPv6 (global or unique-local), then the
    /// Docker bridge, then link-local and loopback. Order within a rank is preserved.
    pub fn ordered_addresses(&self) -> Vec<IpAddr> {
        let mut ordered = self.addresses.clone();
        ordered.sort_by_key(|ip| address_rank(*ip));
        ordered
    }

    /// Address to open the control connection to: the best-ranked resolved address (see
    /// [`ordered_addresses`](Self::ordered_addresses)) and the control port.
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.ordered_addresses()
            .first()
            .map(|ip| SocketAddr::new(*ip, self.control_port))
    }
}

fn address_rank(ip: IpAddr) -> u8 {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
                4
            } else if a == 172 && b == 17 {
                3
            } else if v4.is_private() {
                0
            } else {
                1
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80 {
                4
            } else {
                2
            }
        }
    }
}

/// The `video` key is optional. Absent means the host has no video listener, which maps to
/// [`VIDEO_PORT_UNAVAILABLE`]. A present value must parse as a port; zero is accepted and also
/// means unavailable.
fn parse_optional_video_port(txt: &HashMap<String, String>) -> Result<u16, DiscoveryError> {
    match txt.get("video") {
        None => Ok(VIDEO_PORT_UNAVAILABLE),
        Some(raw) => raw
            .parse::<u16>()
            .map_err(|_| DiscoveryError::MalformedRecord(format!("invalid video port {raw:?}"))),
    }
}

fn parse_port(txt: &HashMap<String, String>, key: &str) -> Result<u16, DiscoveryError> {
    let raw = txt
        .get(key)
        .ok_or_else(|| DiscoveryError::MalformedRecord(format!("missing {key} port")))?;
    match raw.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(DiscoveryError::MalformedRecord(format!(
            "invalid {key} port {raw:?}"
        ))),
    }
}

fn codec_token(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "av1",
        VideoCodec::Hevc => "hevc",
        VideoCodec::H264 => "h264",
    }
}

fn parse_codec_token(token: &str) -> Option<VideoCodec> {
    match token {
        "av1" => Some(VideoCodec::Av1),
        "hevc" => Some(VideoCodec::Hevc),
        "h264" => Some(VideoCodec::H264),
        _ => None,
    }
}

/// Network-facing discovery. Implementations must be safe to call from the async runtime.
#[async_trait]
pub trait DiscoveryBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Publishes (or replaces) this host's advertisement.
    async fn advertise(&mut self, advertisement: &HostAdvertisement) -> Result<(), DiscoveryError>;

    /// Stops publishing the advertisement. Succeeds if nothing is advertised.
    async fn withdraw(&mut self) -> Result<(), DiscoveryError>;

    /// Listens for `window` and returns every valid host that answered.
    async fn browse(&mut self, window: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError>;
}

/// Tracks hosts across repeated browses and expires the ones that stop answering.
#[derive(Debug)]
pub struct HostDirectory {
    stale_after: Duration,
    /// Keyed by instance name. Each entry keeps the time it was last observed.
    entries: BTreeMap<String, (DiscoveredHost, Instant)>,
}

impl Default for HostDirectory {
    fn default() -> Self {
        Self::new(DEFAULT_STALE_AFTER)
    }
}

impl HostDirectory {
    pub fn new(stale_after: Duration) -> Self {
        Self {
            stale_after,
            entries: BTreeMap::new(),
        }
    }

    /// Records hosts seen at `now`. A host seen again replaces its old entry and resets its age.
    pub fn observe(&mut self, hosts: impl IntoIterator<Item = DiscoveredHost>, now: Instant) {
        for host in hosts {
            self.entries.insert(host.name.clone(), (host, now));
        }
    }

    /// Drops entries not observed within `stale_after` of `now`. Returns how many were removed.
    pub fn prune(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        let stale_after = self.stale_after;
        self.entries
            .retain(|_, (_, seen)| now.saturating_duration_since(*seen) < stale_after);
        before - self.entries.len()
    }

    /// Hosts still fresh at `now`, ordered by name. Stale entries are hidden even before `prune`.
    pub fn hosts(&self, now: Instant) -> Vec<&DiscoveredHost> {
        self.entries
            .values()
            .filter(|(_, seen)| now.saturating_duration_since(*seen) < self.stale_after)
            .map(|(host, _)| host)
            .collect()
    }

    /// Looks up one fresh host by instance name.
    pub fn get(&self, name: &str, now: Instant) -> Option<&DiscoveredHost> {
        self.entries
            .get(name)
            .filter(|(_, seen)| now.saturating_duration_since(*seen) < self.stale_after)
            .map(|(host, _)| host)
    }
}

/// One browse cycle: query the backend, record what answered, then expire stale hosts.
/// Returns how many hosts answered this cycle.
///
/// Entries are stamped with the time the browse *finished* (`Instant::now()` read after
/// `backend.browse` returns), not when it started, so a long browse window does not make
/// fresh hosts look older than they are. Use [`refresh_with_clock`] to inject a clock in tests.
pub async fn refresh<B: DiscoveryBackend + ?Sized>(
    backend: &mut B,
    directory: &mut HostDirectory,
    window: Duration,
) -> Result<usize, DiscoveryError> {
    refresh_with_clock(backend, directory, window, Instant::now).await
}

/// Like [`refresh`], but `clock` supplies the observation time. It is called exactly once,
/// after the browse has completed, and not at all if the browse failed.
pub async fn refresh_with_clock<B: DiscoveryBackend + ?Sized>(
    backend: &mut B,
    directory: &mut HostDirectory,
    window: Duration,
    clock: impl FnOnce() -> Instant,
) -> Result<usize, DiscoveryError> {
    let found = backend.browse(window).await?;
    let now = clock();
    let count = found.len();
    directory.observe(found, now);
    directory.prune(now);
    Ok(count)
}

/// Deterministic in-memory backend for tests and headless CI.
///
/// `peers` stands in for hosts answering on the LAN. Change them between browses to simulate
/// hosts appearing or disappearing.
#[derive(Debug)]
pub struct MockDiscoveryBackend {
    advertised: Option<HostAdvertisement>,
    peers: Vec<DiscoveredHost>,
    multicast_available: bool,
    browse_calls: usize,
}

impl Default for MockDiscoveryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDiscoveryBackend {
    pub fn new() -> Self {
        Self {
            advertised: None,
            peers: Vec::new(),
            multicast_available: true,
            browse_calls: 0,
        }
    }

    pub fn with_peers(mut self, peers: impl IntoIterator<Item = DiscoveredHost>) -> Self {
        self.peers = peers.into_iter().collect();
        self
    }

    /// `false` simulates a network stack that drops multicast. Every call then fails.
    pub fn with_multicast_available(mut self, available: bool) -> Self {
        self.multicast_available = available;
        self
    }

    pub fn set_peers(&mut self, peers: impl IntoIterator<Item = DiscoveredHost>) {
        self.peers = peers.into_iter().collect();
    }

    pub fn advertised(&self) -> Option<&HostAdvertisement> {
        self.advertised.as_ref()
    }

    pub fn browse_calls(&self) -> usize {
        self.browse_calls
    }

    fn ensure_multicast(&self) -> Result<(), DiscoveryError> {
        if self.multicast_available {
            Ok(())
        } else {
            Err(DiscoveryError::MulticastUnavailable(
                "mock: multicast disabled".to_string(),
            ))
        }
    }
}

#[async_trait]
impl DiscoveryBackend for MockDiscoveryBackend {
    fn name(&self) -> &'static str {
        "Mock (Deterministic)"
    }

    async fn advertise(&mut self, advertisement: &HostAdvertisement) -> Result<(), DiscoveryError> {
        advertisement.validate()?;
        self.ensure_multicast()?;
        self.advertised = Some(advertisement.clone());
        Ok(())
    }

    async fn withdraw(&mut self) -> Result<(), DiscoveryError> {
        self.ensure_multicast()?;
        self.advertised = None;
        Ok(())
    }

    async fn browse(&mut self, _window: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError> {
        self.ensure_multicast()?;
        self.browse_calls += 1;
        Ok(self.peers.clone())
    }
}

/// Real mDNS / DNS-SD backend built on `mdns-sd`. Requires the `mdns` cargo feature.
///
/// # Lifecycle
///
/// `mdns_sd::ServiceDaemon` owns a thread and sockets but has no `Drop`. This type shuts the
/// daemon down on drop (which also makes the daemon send mDNS goodbye packets for a still
/// registered service). Prefer calling [`MdnsDiscoveryBackend::shutdown`] when you can await
/// it: it waits (bounded) for the goodbye and for the daemon thread to acknowledge.
///
/// # Error reporting
///
/// `mdns-sd` opens its multicast sockets lazily on the daemon thread and only logs
/// (at debug level) when binding or joining the multicast group fails. It does **not** report
/// that through `ServiceDaemon::monitor` either. So:
///
/// * Detected, as [`DiscoveryError::MulticastUnavailable`]: the daemon thread cannot be
///   started in [`new`](Self::new), or has exited (`Error::DaemonShutdown`), for example after
///   [`shutdown`](Self::shutdown).
/// * Detected, as [`DiscoveryError::Backend`]: `DaemonEvent::Error` events from the daemon's
///   monitor channel (currently only rejected service registrations), reported by the next
///   `advertise`/`browse` call, or immediately while a `browse` is waiting; and errors
///   returned synchronously by the `mdns-sd` API.
/// * **Not** detected: blocked multicast, failure to bind port 5353 or to join the group, or no
///   usable network interface. `advertise` then succeeds without anything being sent and
///   `browse` returns an empty list.
#[cfg(feature = "mdns")]
pub struct MdnsDiscoveryBackend {
    daemon: mdns_sd::ServiceDaemon,
    monitor: mdns_sd::Receiver<mdns_sd::DaemonEvent>,
    hostname: String,
    registered_fullname: Option<String>,
    shut_down: bool,
}

/// Stops the browse on drop, including when the `browse` future is cancelled mid-await.
#[cfg(feature = "mdns")]
struct BrowseGuard<'a> {
    daemon: &'a mdns_sd::ServiceDaemon,
}

#[cfg(feature = "mdns")]
impl Drop for BrowseGuard<'_> {
    fn drop(&mut self) {
        // Ignore the error: the daemon may already be shut down.
        let _ = self.daemon.stop_browse(DISCOVERY_SERVICE_TYPE);
    }
}

/// How long [`MdnsDiscoveryBackend::shutdown`] waits for each daemon acknowledgement.
#[cfg(feature = "mdns")]
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(feature = "mdns")]
impl MdnsDiscoveryBackend {
    /// Starts the mDNS daemon thread.
    ///
    /// Fails with `MulticastUnavailable` only if the daemon thread or its control socket cannot
    /// be created. The multicast sockets themselves are opened later by the daemon and their
    /// failure is not reported; see the type-level docs.
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()
            .map_err(|e| DiscoveryError::MulticastUnavailable(e.to_string()))?;
        let monitor = match daemon.monitor() {
            Ok(monitor) => monitor,
            Err(e) => {
                let _ = daemon.shutdown();
                return Err(map_error(e));
            }
        };
        Ok(Self {
            daemon,
            monitor,
            hostname: local_mdns_hostname(),
            registered_fullname: None,
            shut_down: false,
        })
    }

    /// Withdraws any advertisement and stops the daemon thread, waiting (bounded) for the
    /// goodbye packets and the daemon's acknowledgement. Idempotent. Afterwards `advertise`
    /// and `browse` fail with `MulticastUnavailable`.
    pub async fn shutdown(&mut self) -> Result<(), DiscoveryError> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;

        if let Some(fullname) = self.registered_fullname.take() {
            if let Ok(ack) = self.daemon.unregister(&fullname) {
                let _ = tokio::time::timeout(SHUTDOWN_ACK_TIMEOUT, ack.recv_async()).await;
            }
        }
        match self.daemon.shutdown() {
            Ok(ack) => {
                let _ = tokio::time::timeout(SHUTDOWN_ACK_TIMEOUT, ack.recv_async()).await;
                Ok(())
            }
            // Already gone is the state we wanted.
            Err(mdns_sd::Error::DaemonShutdown) => Ok(()),
            Err(e) => Err(map_error(e)),
        }
    }

    /// Surfaces the first queued `DaemonEvent::Error`, discarding other events.
    fn check_monitor(&self) -> Result<(), DiscoveryError> {
        while let Ok(event) = self.monitor.try_recv() {
            if let mdns_sd::DaemonEvent::Error(e) = event {
                return Err(map_error(e));
            }
        }
        Ok(())
    }
}

#[cfg(feature = "mdns")]
impl Drop for MdnsDiscoveryBackend {
    fn drop(&mut self) {
        // Always try, even after `shutdown()`: a cancelled `shutdown()` future may have set
        // `shut_down` without ever sending the command, and shutting down a daemon that is
        // already gone just returns an ignored error.
        // Best effort: the daemon also sends goodbyes for registered services when it exits.
        if let Some(fullname) = self.registered_fullname.take() {
            let _ = self.daemon.unregister(&fullname);
        }
        let _ = self.daemon.shutdown();
    }
}

#[cfg(feature = "mdns")]
fn local_mdns_hostname() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .and_then(|s| s.trim().split('.').next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux-quest-host".to_string());
    format!("{raw}.local.")
}

/// `DaemonShutdown` means the daemon thread is gone, so no discovery can happen: report it as
/// `MulticastUnavailable` so callers fall back to manual entry. Everything else is `Backend`.
#[cfg(feature = "mdns")]
fn map_error(e: mdns_sd::Error) -> DiscoveryError {
    match e {
        mdns_sd::Error::DaemonShutdown => {
            DiscoveryError::MulticastUnavailable("mDNS daemon is not running".to_string())
        }
        other => DiscoveryError::Backend(other.to_string()),
    }
}

#[cfg(feature = "mdns")]
#[async_trait]
impl DiscoveryBackend for MdnsDiscoveryBackend {
    fn name(&self) -> &'static str {
        "mDNS (mdns-sd)"
    }

    async fn advertise(&mut self, advertisement: &HostAdvertisement) -> Result<(), DiscoveryError> {
        advertisement.validate()?;
        self.check_monitor()?;
        self.withdraw().await?;

        let info = mdns_sd::ServiceInfo::new(
            DISCOVERY_SERVICE_TYPE,
            &advertisement.name,
            &self.hostname,
            (),
            advertisement.control_port,
            advertisement.txt_properties(),
        )
        .map_err(map_error)?
        .enable_addr_auto();

        let fullname = info.get_fullname().to_string();
        self.daemon.register(info).map_err(map_error)?;
        self.registered_fullname = Some(fullname);
        Ok(())
    }

    async fn withdraw(&mut self) -> Result<(), DiscoveryError> {
        if let Some(fullname) = self.registered_fullname.take() {
            self.daemon.unregister(&fullname).map_err(map_error)?;
        }
        Ok(())
    }

    async fn browse(&mut self, window: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError> {
        self.check_monitor()?;
        let receiver = self
            .daemon
            .browse(DISCOVERY_SERVICE_TYPE)
            .map_err(map_error)?;
        // Dropped on every exit path, including cancellation of this future.
        let _guard = BrowseGuard {
            daemon: &self.daemon,
        };

        let suffix = format!(".{DISCOVERY_SERVICE_TYPE}");
        // Keyed by instance name: a service can resolve more than once (new addresses, TXT
        // updates), and the latest resolution wins. Removal evicts.
        let mut found: BTreeMap<String, DiscoveredHost> = BTreeMap::new();
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let event = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                daemon_event = self.monitor.recv_async() => {
                    match daemon_event {
                        Ok(mdns_sd::DaemonEvent::Error(e)) => return Err(map_error(e)),
                        Ok(_) => continue,
                        // Monitor closed: the daemon thread is gone.
                        Err(_) => return Err(map_error(mdns_sd::Error::DaemonShutdown)),
                    }
                }
                event = receiver.recv_async() => match event {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            match event {
                mdns_sd::ServiceEvent::ServiceResolved(resolved) => {
                    let Some(name) = resolved.get_fullname().strip_suffix(&suffix) else {
                        tracing::warn!(
                            fullname = resolved.get_fullname(),
                            "ignoring unexpected service type"
                        );
                        continue;
                    };
                    let addresses = resolved
                        .get_addresses()
                        .iter()
                        .map(|a| a.to_ip_addr())
                        .collect::<Vec<_>>();
                    let txt: HashMap<String, String> = resolved
                        .get_properties()
                        .iter()
                        .map(|p| (p.key().to_string(), p.val_str().to_string()))
                        .collect();
                    match DiscoveredHost::from_parts(name, addresses, resolved.get_port(), &txt) {
                        Ok(host) => {
                            found.insert(host.name.clone(), host);
                        }
                        Err(e) => {
                            // The newest record for this name is unusable, so drop any older one.
                            found.remove(name);
                            tracing::warn!(instance = name, error = %e, "skipping discovered host");
                        }
                    }
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    if let Some(name) = fullname.strip_suffix(&suffix) {
                        found.remove(name);
                    }
                }
                _ => {}
            }
        }

        Ok(found.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advertisement() -> HostAdvertisement {
        HostAdvertisement {
            name: "office-tower".to_string(),
            control_port: 48444,
            video_port: 48445,
            input_port: 48446,
            codecs: vec![VideoCodec::Av1, VideoCodec::Hevc],
        }
    }

    fn host(name: &str, last_octet: u8) -> DiscoveredHost {
        DiscoveredHost {
            name: name.to_string(),
            addresses: vec![IpAddr::from([192, 168, 1, last_octet])],
            control_port: 48444,
            video_port: 48445,
            input_port: 48446,
            codecs: vec![VideoCodec::Av1],
        }
    }

    #[test]
    fn advertisement_round_trips_through_txt_record() {
        let ad = advertisement();
        let txt = ad.txt_properties();
        let parsed = DiscoveredHost::from_parts(
            &ad.name,
            [IpAddr::from([10, 0, 0, 7])],
            ad.control_port,
            &txt,
        )
        .expect("valid record should parse");

        assert_eq!(parsed.name, ad.name);
        assert_eq!(parsed.control_port, 48444);
        assert_eq!(parsed.video_port, 48445);
        assert_eq!(parsed.input_port, 48446);
        assert_eq!(parsed.codecs, vec![VideoCodec::Av1, VideoCodec::Hevc]);
        assert_eq!(
            parsed.control_addr(),
            Some("10.0.0.7:48444".parse().unwrap())
        );
    }

    #[test]
    fn advertisement_without_video_listener_omits_video_key() {
        let mut ad = advertisement();
        ad.video_port = VIDEO_PORT_UNAVAILABLE;
        ad.validate()
            .expect("a host with no video listener is still publishable");

        let txt = ad.txt_properties();
        assert!(!txt.contains_key("video"));

        let parsed =
            DiscoveredHost::from_parts(&ad.name, [IpAddr::from([10, 0, 0, 7])], 48444, &txt)
                .expect("record without video key should parse");
        assert_eq!(parsed.video_port, VIDEO_PORT_UNAVAILABLE);
        assert!(!parsed.video_available());
        assert_eq!(parsed.input_port, 48446);
    }

    #[test]
    fn video_port_zero_in_record_means_unavailable() {
        let mut txt = advertisement().txt_properties();
        txt.insert("video".into(), "0".into());
        let parsed =
            DiscoveredHost::from_parts("h", [IpAddr::from([10, 0, 0, 7])], 48444, &txt).unwrap();
        assert!(!parsed.video_available());
        assert!(DiscoveredHost::from_parts(
            "h",
            [IpAddr::from([10, 0, 0, 7])],
            48444,
            &advertisement().txt_properties()
        )
        .unwrap()
        .video_available());
    }

    #[test]
    fn advertisement_validation_rejects_unpublishable_values() {
        let mut ad = advertisement();
        ad.validate().expect("baseline advertisement is valid");

        ad.name = String::new();
        assert!(matches!(
            ad.validate(),
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));

        ad.name = "x".repeat(64);
        assert!(matches!(
            ad.validate(),
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));

        ad.name = "has.dot".to_string();
        assert!(matches!(
            ad.validate(),
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));

        ad = advertisement();
        ad.input_port = 0;
        assert!(matches!(
            ad.validate(),
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));

        ad = advertisement();
        ad.codecs.clear();
        assert!(matches!(
            ad.validate(),
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));
    }

    #[test]
    fn malformed_records_are_rejected() {
        let good = advertisement().txt_properties();
        let addr = [IpAddr::from([10, 0, 0, 7])];

        let mut wrong_version = good.clone();
        wrong_version.insert("proto".into(), "2".into());
        assert!(matches!(
            DiscoveredHost::from_parts("h", addr, 48444, &wrong_version),
            Err(DiscoveryError::MalformedRecord(_))
        ));

        let mut unknown_codec = good.clone();
        unknown_codec.insert("codecs".into(), "vp9".into());
        assert!(matches!(
            DiscoveredHost::from_parts("h", addr, 48444, &unknown_codec),
            Err(DiscoveryError::MalformedRecord(_))
        ));

        let mut bad_port = good.clone();
        bad_port.insert("video".into(), "not-a-port".into());
        assert!(matches!(
            DiscoveredHost::from_parts("h", addr, 48444, &bad_port),
            Err(DiscoveryError::MalformedRecord(_))
        ));

        let mut missing_input = good.clone();
        missing_input.remove("input");
        assert!(matches!(
            DiscoveredHost::from_parts("h", addr, 48444, &missing_input),
            Err(DiscoveryError::MalformedRecord(_))
        ));

        assert!(matches!(
            DiscoveredHost::from_parts("h", Vec::<IpAddr>::new(), 48444, &good),
            Err(DiscoveryError::MalformedRecord(_))
        ));
        assert!(matches!(
            DiscoveredHost::from_parts("h", addr, 0, &good),
            Err(DiscoveryError::MalformedRecord(_))
        ));
    }

    #[test]
    fn parsed_addresses_are_sorted_and_deduplicated() {
        let parsed = DiscoveredHost::from_parts(
            "h",
            [
                IpAddr::from([10, 0, 0, 9]),
                IpAddr::from([10, 0, 0, 2]),
                IpAddr::from([10, 0, 0, 9]),
            ],
            48444,
            &advertisement().txt_properties(),
        )
        .unwrap();
        assert_eq!(
            parsed.addresses,
            vec![IpAddr::from([10, 0, 0, 2]), IpAddr::from([10, 0, 0, 9])]
        );
    }

    #[tokio::test]
    async fn mock_advertise_and_withdraw() {
        let mut backend = MockDiscoveryBackend::new();
        assert!(backend.advertised().is_none());

        backend.advertise(&advertisement()).await.unwrap();
        assert_eq!(backend.advertised(), Some(&advertisement()));

        backend.withdraw().await.unwrap();
        assert!(backend.advertised().is_none());
    }

    #[tokio::test]
    async fn mock_advertise_rejects_invalid_advertisement() {
        let mut backend = MockDiscoveryBackend::new();
        let mut ad = advertisement();
        ad.name.clear();
        assert!(matches!(
            backend.advertise(&ad).await,
            Err(DiscoveryError::InvalidAdvertisement(_))
        ));
        assert!(backend.advertised().is_none());
    }

    #[tokio::test]
    async fn mock_browse_returns_multiple_hosts() {
        let mut backend = MockDiscoveryBackend::new().with_peers([
            host("den", 10),
            host("studio", 11),
            host("loft", 12),
        ]);

        let found = backend.browse(Duration::from_millis(10)).await.unwrap();
        let names: Vec<_> = found.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["den", "studio", "loft"]);
        assert_eq!(backend.browse_calls(), 1);
    }

    #[tokio::test]
    async fn multicast_unavailable_is_reported_for_every_operation() {
        let mut backend = MockDiscoveryBackend::new().with_multicast_available(false);

        assert!(matches!(
            backend.advertise(&advertisement()).await,
            Err(DiscoveryError::MulticastUnavailable(_))
        ));
        assert!(matches!(
            backend.withdraw().await,
            Err(DiscoveryError::MulticastUnavailable(_))
        ));
        assert!(matches!(
            backend.browse(Duration::from_millis(10)).await,
            Err(DiscoveryError::MulticastUnavailable(_))
        ));
        assert_eq!(backend.browse_calls(), 0);
    }

    #[tokio::test]
    async fn refresh_surfaces_multicast_error_without_touching_directory() {
        let mut backend = MockDiscoveryBackend::new().with_multicast_available(false);
        let mut directory = HostDirectory::default();
        let now = Instant::now();

        let result = refresh_with_clock(
            &mut backend,
            &mut directory,
            Duration::from_millis(10),
            || now,
        )
        .await;
        assert!(matches!(
            result,
            Err(DiscoveryError::MulticastUnavailable(_))
        ));
        assert!(directory.hosts(now).is_empty());
    }

    #[test]
    fn directory_expires_hosts_that_stop_answering() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(15);
        let mut directory = HostDirectory::new(ttl);

        directory.observe([host("den", 10), host("studio", 11)], t0);
        assert_eq!(directory.hosts(t0).len(), 2);

        // "studio" answers again; "den" goes silent.
        let later = t0 + Duration::from_secs(10);
        directory.observe([host("studio", 11)], later);

        let just_after_ttl = t0 + ttl + Duration::from_millis(1);
        // Hidden from readers even before prune runs.
        let visible: Vec<_> = directory
            .hosts(just_after_ttl)
            .iter()
            .map(|h| h.name.clone())
            .collect();
        assert_eq!(visible, ["studio"]);
        assert!(directory.get("den", just_after_ttl).is_none());

        assert_eq!(directory.prune(just_after_ttl), 1);
        assert_eq!(directory.hosts(just_after_ttl).len(), 1);
    }

    #[test]
    fn directory_host_exactly_at_ttl_is_stale() {
        let t0 = Instant::now();
        let mut directory = HostDirectory::new(Duration::from_secs(5));
        directory.observe([host("den", 10)], t0);

        assert!(directory.get("den", t0 + Duration::from_secs(4)).is_some());
        assert!(directory.get("den", t0 + Duration::from_secs(5)).is_none());
    }

    #[tokio::test]
    async fn refresh_tracks_multiple_hosts_and_drops_one_that_leaves() {
        let mut backend =
            MockDiscoveryBackend::new().with_peers([host("den", 10), host("studio", 11)]);
        let mut directory = HostDirectory::new(Duration::from_secs(15));
        let t0 = Instant::now();

        let seen = refresh_with_clock(
            &mut backend,
            &mut directory,
            Duration::from_millis(10),
            || t0,
        )
        .await
        .unwrap();
        assert_eq!(seen, 2);
        assert_eq!(directory.hosts(t0).len(), 2);

        // "den" powers off. A later browse only sees "studio", past the TTL for "den".
        backend.set_peers([host("studio", 11)]);
        let t1 = t0 + Duration::from_secs(20);
        let seen = refresh_with_clock(
            &mut backend,
            &mut directory,
            Duration::from_millis(10),
            || t1,
        )
        .await
        .unwrap();
        assert_eq!(seen, 1);

        let names: Vec<_> = directory.hosts(t1).iter().map(|h| h.name.clone()).collect();
        assert_eq!(names, ["studio"]);
        assert!(directory.get("den", t1).is_none());
    }

    #[test]
    fn advertisement_validation_rejects_backslash_and_control_characters() {
        for name in ["back\\slash", "tab\tname", "new\nline"] {
            let mut ad = advertisement();
            ad.name = name.to_string();
            assert!(
                matches!(ad.validate(), Err(DiscoveryError::InvalidAdvertisement(_))),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn reobserved_host_replaces_stale_data() {
        let t0 = Instant::now();
        let mut directory = HostDirectory::new(Duration::from_secs(15));
        directory.observe([host("den", 10)], t0);

        let mut moved = host("den", 99);
        moved.control_port = 50000;
        moved.codecs = vec![VideoCodec::H264];
        let t1 = t0 + Duration::from_secs(5);
        directory.observe([moved.clone()], t1);

        assert_eq!(directory.hosts(t1).len(), 1);
        assert_eq!(directory.get("den", t1), Some(&moved));
        assert_eq!(
            directory.get("den", t1).unwrap().control_addr(),
            Some("192.168.1.99:50000".parse().unwrap())
        );
        // Age was reset by the re-observation, not inherited from t0.
        let t2 = t0 + Duration::from_secs(16);
        assert!(directory.get("den", t2).is_some());
    }

    #[test]
    fn prune_returns_zero_when_nothing_is_stale() {
        let t0 = Instant::now();
        let mut directory = HostDirectory::new(Duration::from_secs(15));
        assert_eq!(directory.prune(t0), 0, "empty directory");

        directory.observe([host("den", 10), host("studio", 11)], t0);
        assert_eq!(directory.prune(t0 + Duration::from_secs(14)), 0);
        assert_eq!(directory.hosts(t0).len(), 2);
    }

    #[test]
    fn zero_ttl_makes_every_host_immediately_stale() {
        let t0 = Instant::now();
        let mut directory = HostDirectory::new(Duration::ZERO);
        directory.observe([host("den", 10)], t0);

        assert!(directory.hosts(t0).is_empty());
        assert!(directory.get("den", t0).is_none());
        assert_eq!(directory.prune(t0), 1);
        assert_eq!(directory.prune(t0), 0);
    }

    #[tokio::test]
    async fn refresh_stamps_entries_after_the_browse_completes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Backend whose browse "takes" a recorded step before returning.
        struct Recording<'a>(&'a AtomicBool);

        #[async_trait]
        impl DiscoveryBackend for Recording<'_> {
            fn name(&self) -> &'static str {
                "recording"
            }
            async fn advertise(&mut self, _: &HostAdvertisement) -> Result<(), DiscoveryError> {
                Ok(())
            }
            async fn withdraw(&mut self) -> Result<(), DiscoveryError> {
                Ok(())
            }
            async fn browse(&mut self, _: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError> {
                self.0.store(true, Ordering::SeqCst);
                Ok(vec![host("den", 10)])
            }
        }

        let browsed = AtomicBool::new(false);
        let mut backend = Recording(&browsed);
        let mut directory = HostDirectory::default();
        let mut browsed_before_clock = false;
        refresh_with_clock(&mut backend, &mut directory, Duration::ZERO, || {
            browsed_before_clock = browsed.load(Ordering::SeqCst);
            Instant::now()
        })
        .await
        .unwrap();
        assert!(browsed_before_clock, "clock must be read after the browse");

        // A failed browse never reads the clock.
        let mut failing = MockDiscoveryBackend::new().with_multicast_available(false);
        let mut called = false;
        let result = refresh_with_clock(&mut failing, &mut directory, Duration::ZERO, || {
            called = true;
            Instant::now()
        })
        .await;
        assert!(result.is_err());
        assert!(!called);
    }

    #[tokio::test]
    async fn refresh_uses_the_real_clock() {
        let mut backend = MockDiscoveryBackend::new().with_peers([host("den", 10)]);
        let mut directory = HostDirectory::default();
        assert_eq!(
            refresh(&mut backend, &mut directory, Duration::ZERO)
                .await
                .unwrap(),
            1
        );
        assert!(directory.get("den", Instant::now()).is_some());
    }

    fn host_with(addresses: &[IpAddr]) -> DiscoveredHost {
        DiscoveredHost::from_parts(
            "h",
            addresses.iter().copied(),
            48444,
            &advertisement().txt_properties(),
        )
        .unwrap()
    }

    #[test]
    fn lan_address_beats_docker_bridge() {
        // 172.17.0.1 sorts before 192.168.1.10, so the plain BTreeSet order would pick docker.
        let docker = IpAddr::from([172, 17, 0, 1]);
        let lan = IpAddr::from([192, 168, 1, 10]);
        let host = host_with(&[docker, lan]);
        assert_eq!(host.addresses[0], docker);
        assert_eq!(host.ordered_addresses(), vec![lan, docker]);
        assert_eq!(host.control_addr(), Some(SocketAddr::new(lan, 48444)));
    }

    #[test]
    fn docker_bridge_is_used_when_it_is_the_only_candidate() {
        let docker = IpAddr::from([172, 17, 0, 1]);
        assert_eq!(
            host_with(&[docker]).control_addr(),
            Some(SocketAddr::new(docker, 48444))
        );
    }

    #[test]
    fn private_ranges_rank_above_others() {
        let ten = IpAddr::from([10, 0, 0, 5]);
        let corp = IpAddr::from([172, 20, 1, 1]);
        let public = IpAddr::from([8, 8, 8, 8]);
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let link_local = IpAddr::from([169, 254, 1, 1]);
        let docker = IpAddr::from([172, 17, 0, 1]);
        let host = host_with(&[link_local, v6, public, docker, corp, ten]);
        // 10.0.0.5 < 172.20.1.1 numerically, so the stable sort keeps ten first.
        assert_eq!(
            host.ordered_addresses(),
            vec![ten, corp, public, v6, docker, link_local]
        );
    }

    #[test]
    fn control_addr_is_none_without_addresses() {
        let mut parsed = DiscoveredHost::from_parts(
            "h",
            [IpAddr::from([10, 0, 0, 1])],
            48444,
            &advertisement().txt_properties(),
        )
        .unwrap();
        parsed.addresses.clear();
        assert_eq!(parsed.control_addr(), None);
    }
}
