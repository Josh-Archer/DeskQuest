//! Startup auto-connect: find a host over LAN discovery and point the config at it, so the user
//! never types an IP address.
//!
//! The decisions live here as plain functions so they test against the mock backend;
//! `android::discover_lan_host` supplies the real mDNS backend and the multicast lock, and
//! runs in the background so the USB tunnel never waits for it.

use tracing::{info, warn};

use linux_quest_transport::{DiscoveredHost, DiscoveryBackend, DiscoveryError};

use super::{apply_selection, ClientDiscovery, MulticastLock};
use crate::config::{ClientConfig, TransportMode};

/// How many browses a startup attempt makes before giving up. Each listens for
/// [`super::DEFAULT_BROWSE_WINDOW`], so a host that answers late still has a second chance
/// while startup is delayed by about four seconds at most.
pub const STARTUP_SCAN_ATTEMPTS: u32 = 2;

/// How a startup auto-connect attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoConnectOutcome {
    /// `config` now targets the named host over Wi-Fi (see [`connect_config_to`]).
    Connected { host: String, other_hosts: usize },
    /// Browsing worked but nothing answered. `config` is unchanged.
    NoHosts,
    /// Discovery cannot run here (multicast blocked, mDNS daemon gone). `config` is unchanged
    /// and the caller should fall back to manual entry.
    Unavailable(String),
}

/// Whether the client should look for its host on the LAN: its mode has a Wi-Fi path and no
/// host is set (`host_addr` is loopback, the default). A host set by the user
/// (`debug.linuxquest.host`) is never overridden, and USB-only mode needs no LAN address.
/// An unspecified host (`0.0.0.0`) is a deliberate "accept video from anyone" and is left
/// alone too.
pub fn needs_lan_discovery(config: &ClientConfig) -> bool {
    config.transport_mode.uses_wifi() && config.host_addr.ip().to_canonical().is_loopback()
}

/// Picks the host to connect to when several answer. Hosts that are streaming video (a video
/// listener is advertised) beat idle ones, then the order is by name so the choice is stable
/// from one launch to the next. Hosts without a usable address are skipped.
pub fn choose_host<'a>(hosts: &[&'a DiscoveredHost]) -> Option<&'a DiscoveredHost> {
    hosts
        .iter()
        .copied()
        .filter(|h| h.control_addr().is_some())
        .min_by(|a, b| {
            b.video_available()
                .cmp(&a.video_available())
                .then_with(|| a.name.cmp(&b.name))
        })
}

/// Targets `host` over Wi-Fi: control address, the host's other addresses for the video source
/// filter, and the input port it advertised. `Auto` stays `Auto` (the cable still wins when it
/// is plugged in); USB-only mode switches to `Udp`, since the host was found on the LAN.
pub fn connect_config_to(
    config: &mut ClientConfig,
    host: &DiscoveredHost,
) -> crate::ClientResult<()> {
    apply_selection(config, host)?;
    if !config.transport_mode.uses_wifi() {
        config.transport_mode = TransportMode::Udp;
    }
    config.input_port = host.input_port;
    Ok(())
}

/// Browses up to `attempts` times, stopping at the first browse that finds a host, and applies
/// the chosen one to `config`.
pub async fn autoconnect<B: DiscoveryBackend, L: MulticastLock>(
    discovery: &mut ClientDiscovery<B, L>,
    config: &mut ClientConfig,
    attempts: u32,
) -> AutoConnectOutcome {
    for attempt in 1..=attempts.max(1) {
        if let Err(e) = discovery.scan().await {
            return match e {
                DiscoveryError::MulticastUnavailable(reason) => {
                    warn!(%reason, "LAN discovery unavailable; manual host entry is needed");
                    AutoConnectOutcome::Unavailable(reason)
                }
                other => {
                    warn!(error = %other, "LAN discovery failed");
                    AutoConnectOutcome::Unavailable(other.to_string())
                }
            };
        }
        let hosts = discovery.hosts();
        let Some(chosen) = choose_host(&hosts) else {
            info!(attempt, "no host answered LAN discovery");
            continue;
        };
        let other_hosts = hosts.len() - 1;
        let name = chosen.name.clone();
        if let Err(e) = connect_config_to(config, chosen) {
            warn!(error = %e, host = %name, "could not use discovered host");
            continue;
        }
        info!(host = %name, addr = %config.host_addr, other_hosts, "connecting to discovered host");
        return AutoConnectOutcome::Connected {
            host: name,
            other_hosts,
        };
    }
    AutoConnectOutcome::NoHosts
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use linux_quest_protocol::VideoCodec;
    use linux_quest_transport::{MockDiscoveryBackend, DEFAULT_CONTROL_PORT};

    use super::*;
    use crate::discovery::MockMulticastLock;

    fn host(name: &str, octet: u8, video: u16) -> DiscoveredHost {
        DiscoveredHost {
            name: name.to_string(),
            addresses: vec![IpAddr::from([192, 168, 1, octet])],
            control_port: DEFAULT_CONTROL_PORT,
            video_port: video,
            input_port: 48999,
            codecs: vec![VideoCodec::Hevc],
        }
    }

    fn discovery(
        backend: MockDiscoveryBackend,
    ) -> ClientDiscovery<MockDiscoveryBackend, MockMulticastLock> {
        ClientDiscovery::new(backend, MockMulticastLock::new())
    }

    #[test]
    fn discovery_runs_only_for_a_wifi_path_without_a_host() {
        let auto = ClientConfig::default();
        assert!(needs_lan_discovery(&auto), "Auto with no host set");
        let udp = ClientConfig {
            transport_mode: TransportMode::Udp,
            ..Default::default()
        };
        assert!(needs_lan_discovery(&udp));
        let usb = ClientConfig {
            transport_mode: TransportMode::AdbReverseTcp,
            ..Default::default()
        };
        assert!(!needs_lan_discovery(&usb), "USB only needs no LAN address");
        let set = ClientConfig {
            host_addr: "192.168.1.20:48444".parse().unwrap(),
            ..Default::default()
        };
        assert!(!needs_lan_discovery(&set), "a host set by the user wins");
        let open = ClientConfig {
            host_addr: "0.0.0.0:48444".parse().unwrap(),
            ..Default::default()
        };
        assert!(!needs_lan_discovery(&open));
    }

    #[test]
    fn choose_prefers_streaming_hosts_then_name() {
        let idle = host("alpha", 10, 0);
        let busy_b = host("beta", 11, 48445);
        let busy_c = host("charlie", 12, 48445);
        assert_eq!(
            choose_host(&[&idle, &busy_c, &busy_b]).unwrap().name,
            "beta"
        );
        assert_eq!(choose_host(&[&busy_c, &idle]).unwrap().name, "charlie");
        assert_eq!(choose_host(&[&idle]).unwrap().name, "alpha");
        assert!(choose_host(&[]).is_none());

        let mut unreachable = host("aaa", 1, 48445);
        unreachable.addresses.clear();
        assert_eq!(choose_host(&[&unreachable, &idle]).unwrap().name, "alpha");
        assert!(choose_host(&[&unreachable]).is_none());
    }

    #[tokio::test]
    async fn connects_to_the_only_host_over_udp() {
        let mut d = discovery(MockDiscoveryBackend::new().with_peers([host("den", 20, 48445)]));
        let mut config = ClientConfig {
            transport_mode: TransportMode::AdbReverseTcp,
            ..Default::default()
        };

        let outcome = autoconnect(&mut d, &mut config, 2).await;

        assert_eq!(
            outcome,
            AutoConnectOutcome::Connected {
                host: "den".into(),
                other_hosts: 0
            }
        );
        assert_eq!(config.transport_mode, TransportMode::Udp);
        assert_eq!(config.host_addr.to_string(), "192.168.1.20:48444");
        assert_eq!(config.input_port, 48999);
    }

    #[test]
    fn connection_uses_advertised_ports_and_prefers_the_lan_address() {
        let mut discovered = host("studio", 20, 48445);
        discovered.addresses = vec![
            IpAddr::from([172, 17, 0, 1]),
            IpAddr::from([192, 168, 1, 20]),
            IpAddr::from([10, 0, 0, 4]),
        ];
        discovered.control_port = 50123;
        discovered.input_port = 50124;

        let mut config = ClientConfig::default();
        connect_config_to(&mut config, &discovered).unwrap();

        assert_eq!(
            config.transport_mode,
            TransportMode::Auto,
            "Auto keeps the cable as well"
        );
        assert_eq!(config.host_addr, "192.168.1.20:50123".parse().unwrap());
        assert_eq!(config.input_port, 50124);
        assert_eq!(
            config.extra_host_ips,
            vec![IpAddr::from([10, 0, 0, 4]), IpAddr::from([172, 17, 0, 1])]
        );
    }

    #[tokio::test]
    async fn multiple_hosts_choose_deterministically_and_report_the_rest() {
        let mut d = discovery(
            MockDiscoveryBackend::new().with_peers([host("zed", 30, 0), host("amy", 31, 0)]),
        );
        let mut config = ClientConfig::default();

        let outcome = autoconnect(&mut d, &mut config, 1).await;

        assert_eq!(
            outcome,
            AutoConnectOutcome::Connected {
                host: "amy".into(),
                other_hosts: 1
            }
        );
        assert_eq!(config.host_addr.ip().to_string(), "192.168.1.31");
    }

    #[tokio::test]
    async fn empty_network_leaves_config_untouched_and_retries() {
        let backend = MockDiscoveryBackend::new();
        let mut d = discovery(backend);
        let mut config = ClientConfig::default();
        let before = config.host_addr;

        assert_eq!(
            autoconnect(&mut d, &mut config, 3).await,
            AutoConnectOutcome::NoHosts
        );
        assert_eq!(d.backend().browse_calls(), 3);
        assert_eq!(config.host_addr, before);
        assert_eq!(config.transport_mode, TransportMode::Auto);
    }

    #[tokio::test]
    async fn blocked_multicast_reports_unavailable_without_retrying() {
        let mut d = discovery(MockDiscoveryBackend::new().with_multicast_available(false));
        let mut config = ClientConfig::default();

        let outcome = autoconnect(&mut d, &mut config, 3).await;

        assert!(matches!(outcome, AutoConnectOutcome::Unavailable(_)));
        assert_eq!(config.transport_mode, TransportMode::Auto);
    }
}
