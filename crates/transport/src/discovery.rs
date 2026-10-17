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
//! | `video`        | TXT       | `48445`       |
//! | `input`        | TXT       | `48446`       |
//! | `codecs`       | TXT       | `av1,hevc`    |
//!
//! The client browses for the same type, merges what it sees into a [`HostDirectory`] and
//! expires hosts that stop answering. The network-facing part lives behind [`DiscoveryBackend`]
//! so tests run against [`MockDiscoveryBackend`]; the real mDNS backend is compiled only with
//! the `mdns` cargo feature. When multicast is blocked, backends return
//! [`DiscoveryError::MulticastUnavailable`] and the caller should fall back to manual entry.

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
        // Dots would need DNS-SD escaping, which the client does not parse back.
        if self.name.contains('.') || self.name.chars().any(char::is_control) {
            return invalid("instance name must not contain dots or control characters");
        }
        if self.control_port == 0 || self.video_port == 0 || self.input_port == 0 {
            return invalid("ports must be non-zero");
        }
        if self.codecs.is_empty() {
            return invalid("at least one codec must be advertised");
        }
        Ok(())
    }

    /// TXT key/value pairs for this advertisement (the control port lives in the SRV record).
    pub fn txt_properties(&self) -> HashMap<String, String> {
        let codecs = self
            .codecs
            .iter()
            .map(|c| codec_token(*c))
            .collect::<Vec<_>>()
            .join(",");
        HashMap::from([
            ("proto".to_string(), DISCOVERY_PROTOCOL_VERSION.to_string()),
            ("video".to_string(), self.video_port.to_string()),
            ("input".to_string(), self.input_port.to_string()),
            ("codecs".to_string(), codecs),
        ])
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
        let video_port = parse_port(txt, "video")?;
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

    /// Address to open the control connection to: the first resolved address and the control port.
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.addresses
            .first()
            .map(|ip| SocketAddr::new(*ip, self.control_port))
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

/// One browse cycle: query the backend, record what answered at `now`, then expire stale hosts.
/// Returns how many hosts answered this cycle.
pub async fn refresh<B: DiscoveryBackend + ?Sized>(
    backend: &mut B,
    directory: &mut HostDirectory,
    window: Duration,
    now: Instant,
) -> Result<usize, DiscoveryError> {
    let found = backend.browse(window).await?;
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
#[cfg(feature = "mdns")]
pub struct MdnsDiscoveryBackend {
    daemon: mdns_sd::ServiceDaemon,
    hostname: String,
    registered_fullname: Option<String>,
}

#[cfg(feature = "mdns")]
impl MdnsDiscoveryBackend {
    /// Starts the mDNS daemon. Fails with `MulticastUnavailable` if the socket cannot be bound
    /// or joined to the multicast group.
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()
            .map_err(|e| DiscoveryError::MulticastUnavailable(e.to_string()))?;
        Ok(Self {
            daemon,
            hostname: local_mdns_hostname(),
            registered_fullname: None,
        })
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

#[cfg(feature = "mdns")]
fn backend_error(e: mdns_sd::Error) -> DiscoveryError {
    DiscoveryError::Backend(e.to_string())
}

#[cfg(feature = "mdns")]
#[async_trait]
impl DiscoveryBackend for MdnsDiscoveryBackend {
    fn name(&self) -> &'static str {
        "mDNS (mdns-sd)"
    }

    async fn advertise(&mut self, advertisement: &HostAdvertisement) -> Result<(), DiscoveryError> {
        advertisement.validate()?;
        self.withdraw().await?;

        let info = mdns_sd::ServiceInfo::new(
            DISCOVERY_SERVICE_TYPE,
            &advertisement.name,
            &self.hostname,
            (),
            advertisement.control_port,
            advertisement.txt_properties(),
        )
        .map_err(backend_error)?
        .enable_addr_auto();

        let fullname = info.get_fullname().to_string();
        self.daemon.register(info).map_err(backend_error)?;
        self.registered_fullname = Some(fullname);
        Ok(())
    }

    async fn withdraw(&mut self) -> Result<(), DiscoveryError> {
        if let Some(fullname) = self.registered_fullname.take() {
            self.daemon.unregister(&fullname).map_err(backend_error)?;
        }
        Ok(())
    }

    async fn browse(&mut self, window: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError> {
        let receiver = self
            .daemon
            .browse(DISCOVERY_SERVICE_TYPE)
            .map_err(backend_error)?;

        let suffix = format!(".{DISCOVERY_SERVICE_TYPE}");
        let mut found = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, receiver.recv_async()).await {
            let mdns_sd::ServiceEvent::ServiceResolved(resolved) = event else {
                continue;
            };
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
                Ok(host) => found.push(host),
                Err(e) => tracing::warn!(instance = name, error = %e, "skipping discovered host"),
            }
        }

        // Ignore the error if the browse was already torn down.
        let _ = self.daemon.stop_browse(DISCOVERY_SERVICE_TYPE);
        Ok(found)
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

        let result = refresh(&mut backend, &mut directory, Duration::from_millis(10), now).await;
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

        let seen = refresh(&mut backend, &mut directory, Duration::from_millis(10), t0)
            .await
            .unwrap();
        assert_eq!(seen, 2);
        assert_eq!(directory.hosts(t0).len(), 2);

        // "den" powers off. A later browse only sees "studio", past the TTL for "den".
        backend.set_peers([host("studio", 11)]);
        let t1 = t0 + Duration::from_secs(20);
        let seen = refresh(&mut backend, &mut directory, Duration::from_millis(10), t1)
            .await
            .unwrap();
        assert_eq!(seen, 1);

        let names: Vec<_> = directory.hosts(t1).iter().map(|h| h.name.clone()).collect();
        assert_eq!(names, ["studio"]);
        assert!(directory.get("den", t1).is_none());
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
