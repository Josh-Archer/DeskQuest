//! Host-side LAN advertisement, so a Quest client can find the daemon without an IP address.
//!
//! The daemon publishes one DNS-SD service through a [`DiscoveryBackend`] on startup and
//! withdraws it on shutdown. Advertising is best effort: every failure is logged as a warning
//! and the daemon keeps running without being discoverable.
//!
//! Real mDNS needs the `mdns` cargo feature. Without it, [`advertise_daemon`] logs once and
//! does nothing.

use linux_quest_protocol::VideoCodec;
use linux_quest_transport::{
    DiscoveryBackend, DiscoveryError, HostAdvertisement, DEFAULT_CONTROL_PORT, DEFAULT_INPUT_PORT,
};
use tracing::{info, warn};

#[cfg(feature = "mdns")]
use linux_quest_transport::MdnsDiscoveryBackend;

/// Video port advertised in the TXT record. The daemon does not bind a video listener yet.
pub const ADVERTISED_VIDEO_PORT: u16 = 48445;

/// Codecs advertised in preference order.
pub const ADVERTISED_CODECS: [VideoCodec; 2] = [VideoCodec::Av1, VideoCodec::Hevc];

/// Fallback instance name when the kernel hostname is unavailable or unusable.
const FALLBACK_INSTANCE_NAME: &str = "linux-quest-host";

/// DNS labels are limited to 63 bytes.
const MAX_INSTANCE_NAME_BYTES: usize = 63;

/// Backend used when the `mdns` feature is off. It accepts every call and touches no network.
#[cfg(not(feature = "mdns"))]
#[derive(Debug, Default)]
pub struct DisabledDiscoveryBackend;

#[cfg(not(feature = "mdns"))]
#[async_trait::async_trait]
impl DiscoveryBackend for DisabledDiscoveryBackend {
    fn name(&self) -> &'static str {
        "disabled (built without mdns)"
    }

    async fn advertise(&mut self, advertisement: &HostAdvertisement) -> Result<(), DiscoveryError> {
        advertisement.validate()
    }

    async fn withdraw(&mut self) -> Result<(), DiscoveryError> {
        Ok(())
    }

    async fn browse(
        &mut self,
        _window: std::time::Duration,
    ) -> Result<Vec<linux_quest_transport::DiscoveredHost>, DiscoveryError> {
        Ok(Vec::new())
    }
}

/// Backend type used by the daemon in this build.
#[cfg(feature = "mdns")]
pub type DefaultDiscoveryBackend = MdnsDiscoveryBackend;
/// Backend type used by the daemon in this build.
#[cfg(not(feature = "mdns"))]
pub type DefaultDiscoveryBackend = DisabledDiscoveryBackend;

fn create_default_backend() -> Result<DefaultDiscoveryBackend, DiscoveryError> {
    #[cfg(feature = "mdns")]
    {
        MdnsDiscoveryBackend::new()
    }
    #[cfg(not(feature = "mdns"))]
    {
        Ok(DisabledDiscoveryBackend)
    }
}

/// Holds an active advertisement. Dropping it without calling [`HostAdvertiser::stop`] still
/// lets the backend withdraw (the mDNS backend sends goodbye packets on drop).
pub struct HostAdvertiser<B: DiscoveryBackend> {
    backend: B,
}

impl<B: DiscoveryBackend> HostAdvertiser<B> {
    /// Publishes `advertisement` through `backend`. Returns `None` and logs a warning if the
    /// advertisement is invalid or cannot be published; the backend is dropped in that case.
    pub async fn start(mut backend: B, advertisement: &HostAdvertisement) -> Option<Self> {
        match backend.advertise(advertisement).await {
            Ok(()) => {
                info!(
                    backend = backend.name(),
                    instance = %advertisement.name,
                    control_port = advertisement.control_port,
                    "Advertising host on the LAN"
                );
                Some(Self { backend })
            }
            Err(e) => {
                warn!(
                    backend = backend.name(),
                    error = %e,
                    "Could not advertise host on the LAN; Quest clients need manual IP entry"
                );
                None
            }
        }
    }

    /// Withdraws the advertisement. Failures are logged, not returned.
    pub async fn stop(mut self) {
        if let Err(e) = self.backend.withdraw().await {
            warn!(error = %e, "Failed to withdraw LAN advertisement");
        }
    }

    /// The backend this advertiser publishes through.
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

/// Builds the advertisement for this daemon: the given instance name, the fixed ports and codecs.
pub fn daemon_advertisement(instance_name: impl Into<String>) -> HostAdvertisement {
    HostAdvertisement {
        name: instance_name.into(),
        control_port: DEFAULT_CONTROL_PORT,
        video_port: ADVERTISED_VIDEO_PORT,
        input_port: DEFAULT_INPUT_PORT,
        codecs: ADVERTISED_CODECS.to_vec(),
    }
}

/// Advertises the daemon on startup when `enabled` is true. Never fails: problems are logged.
pub async fn advertise_daemon(
    enabled: bool,
    instance_name: impl Into<String>,
) -> Option<HostAdvertiser<DefaultDiscoveryBackend>> {
    if !enabled {
        info!("LAN advertisement disabled by --no-advertise");
        return None;
    }
    if !cfg!(feature = "mdns") {
        info!("LAN advertisement unavailable: built without the `mdns` feature");
        return None;
    }
    let backend = match create_default_backend() {
        Ok(backend) => backend,
        Err(e) => {
            warn!(
                error = %e,
                "Could not start mDNS; Quest clients need manual IP entry"
            );
            return None;
        }
    };
    HostAdvertiser::start(backend, &daemon_advertisement(instance_name)).await
}

/// Turns a raw hostname into a DNS-SD instance name.
///
/// Keeps the first DNS label, replaces dots, backslashes and control characters (which the
/// client cannot round-trip) with `-`, and truncates to 63 bytes on a character boundary.
/// Falls back to `linux-quest-host` if nothing usable remains.
pub fn instance_name_from_hostname(raw: &str) -> String {
    let label = raw.trim().split('.').next().unwrap_or("");
    let mut name: String = label
        .chars()
        .map(|c| {
            if c == '.' || c == '\\' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    while name.len() > MAX_INSTANCE_NAME_BYTES {
        name.pop();
    }
    if name.is_empty() {
        FALLBACK_INSTANCE_NAME.to_string()
    } else {
        name
    }
}

/// Instance name for this machine, derived from `/proc/sys/kernel/hostname`.
pub fn system_instance_name() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    instance_name_from_hostname(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_transport::MockDiscoveryBackend;

    #[test]
    fn instance_name_keeps_first_label() {
        assert_eq!(
            instance_name_from_hostname("living-room.lan\n"),
            "living-room"
        );
    }

    #[test]
    fn instance_name_replaces_unsafe_characters() {
        assert_eq!(instance_name_from_hostname("a\\b\tc"), "a-b-c");
    }

    #[test]
    fn instance_name_truncates_on_char_boundary() {
        let long = "é".repeat(40); // 80 bytes
        let name = instance_name_from_hostname(&long);
        assert!(name.len() <= MAX_INSTANCE_NAME_BYTES);
        assert_eq!(name.chars().count(), 31);
    }

    #[test]
    fn instance_name_falls_back_when_empty() {
        assert_eq!(instance_name_from_hostname(""), FALLBACK_INSTANCE_NAME);
        assert_eq!(instance_name_from_hostname(".lan"), FALLBACK_INSTANCE_NAME);
    }

    #[test]
    fn daemon_advertisement_uses_daemon_ports_and_validates() {
        let ad = daemon_advertisement("den");
        ad.validate().expect("daemon advertisement should be valid");
        assert_eq!(ad.control_port, DEFAULT_CONTROL_PORT);
        assert_eq!(ad.input_port, DEFAULT_INPUT_PORT);
        assert_eq!(ad.video_port, ADVERTISED_VIDEO_PORT);
        assert_eq!(ad.codecs, ADVERTISED_CODECS.to_vec());
    }

    #[tokio::test]
    async fn start_publishes_and_stop_withdraws() {
        let ad = daemon_advertisement("den");
        let advertiser = HostAdvertiser::start(MockDiscoveryBackend::new(), &ad)
            .await
            .expect("mock advertise should succeed");
        assert_eq!(advertiser.backend().advertised(), Some(&ad));

        advertiser.stop().await;
    }

    #[tokio::test]
    async fn start_failure_returns_none_instead_of_erroring() {
        let ad = daemon_advertisement("den");
        let backend = MockDiscoveryBackend::new().with_multicast_available(false);
        assert!(HostAdvertiser::start(backend, &ad).await.is_none());
    }

    #[tokio::test]
    async fn invalid_advertisement_returns_none() {
        let mut ad = daemon_advertisement("den");
        ad.name.clear();
        assert!(HostAdvertiser::start(MockDiscoveryBackend::new(), &ad)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn disabled_flag_skips_advertising() {
        assert!(advertise_daemon(false, "den").await.is_none());
    }
}
