//! Android NativeActivity integration and main entry point.

#[cfg(target_os = "android")]
use std::time::Duration;

#[cfg(target_os = "android")]
use android_activity::{AndroidApp, MainEvent, PollEvent};

use std::net::{IpAddr, SocketAddr};

use crate::config::{ClientConfig, TransportMode};
use crate::runtime::QuestClientRuntime;

/// Source IPs that UDP video datagrams must come from, or `None` to accept any source.
///
/// Only the UDP transport filters, and only by IP: the host sends from an ephemeral port, so
/// its port is not known. The ADB/USB path connects to `host_addr` and never filters.
/// An unspecified IP (`0.0.0.0`) in `host_addr` means "unset", so no filter is applied.
/// The list is `host_addr` plus `extra_host_ips`, all in canonical form (IPv4-mapped IPv6 is
/// folded to IPv4).
pub fn udp_source_filter(config: &ClientConfig) -> Option<Vec<IpAddr>> {
    match config.transport_mode {
        TransportMode::Udp if !config.host_addr.ip().is_unspecified() => {
            let mut ips = vec![config.host_addr.ip().to_canonical()];
            for ip in &config.extra_host_ips {
                let ip = ip.to_canonical();
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
            Some(ips)
        }
        _ => None,
    }
}

/// Whether a datagram from `source` may be passed to the packet parser. Addresses are compared
/// in canonical form, so a dual-stack socket's `::ffff:192.168.1.20` matches `192.168.1.20`.
pub fn accept_udp_source(filter: Option<&[IpAddr]>, source: SocketAddr) -> bool {
    filter.is_none_or(|ips| {
        let source = source.ip().to_canonical();
        ips.iter().any(|ip| ip.to_canonical() == source)
    })
}

/// A warning to log at startup when the configuration will silently drop all LAN video, or
/// `None`. Applies to `Udp` mode with a loopback `host_addr` (the default). An unspecified
/// address means no filter and is fine.
pub fn udp_config_warning(config: &ClientConfig) -> Option<String> {
    (config.transport_mode == TransportMode::Udp && config.host_addr.ip().is_loopback()).then(
        || {
            format!(
                "UDP mode with loopback host_addr {}: every LAN video datagram will be dropped \
             until discovery or manual entry sets the host address (or set 0.0.0.0 to disable \
             the source filter)",
                config.host_addr
            )
        },
    )
}

/// Longest gap between repeated drop warnings for one source.
const DROP_WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Most sources tracked at once, so spoofed sources cannot grow the table without bound.
const DROP_WARN_MAX_SOURCES: usize = 64;

/// Rate limiter for "dropped datagram from unexpected source" warnings: one warning the first
/// time a source is seen, and at most one more per [`DROP_WARN_INTERVAL`] after that.
#[derive(Debug, Default)]
pub struct DropWarnLimiter {
    last_warned: std::collections::HashMap<IpAddr, std::time::Instant>,
}

impl DropWarnLimiter {
    /// `true` when a drop from `source` should be logged at warn now.
    pub fn should_warn(&mut self, source: IpAddr, now: std::time::Instant) -> bool {
        let source = source.to_canonical();
        if let Some(last) = self.last_warned.get(&source) {
            if now.saturating_duration_since(*last) < DROP_WARN_INTERVAL {
                return false;
            }
        } else if self.last_warned.len() >= DROP_WARN_MAX_SOURCES {
            self.last_warned.clear();
        }
        self.last_warned.insert(source, now);
        true
    }
}

/// Native entry point for Android NativeActivity on Meta Quest.
#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(app: AndroidApp) {
    // Initialize tracing logging for Android logcat
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    tracing::info!("linux-quest Quest client starting up on Android / Horizon OS");

    // Initialize Android context for OpenXR and MediaCodec
    unsafe {
        ndk_context::initialize_android_context(app.vm_as_ptr(), app.activity_as_ptr());
    }

    let config = ClientConfig::default();
    let mut runtime = match QuestClientRuntime::new(config.clone()) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to initialize Quest client runtime");
            unsafe {
                ndk_context::release_android_context();
            }
            return;
        }
    };

    // Background transport packet receiver (bounded channel: 512 packets, drops on overflow)
    let (packet_tx, packet_rx) =
        std::sync::mpsc::sync_channel::<linux_quest_protocol::packet::Packet>(512);
    let running_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let bg_running = running_flag.clone();
    let host_addr = config.host_addr;
    let transport_mode = config.transport_mode;
    let udp_filter = udp_source_filter(&config);
    if let Some(message) = udp_config_warning(&config) {
        tracing::warn!("{message}");
    }

    let receiver_handle = std::thread::spawn(move || {
        match transport_mode {
            crate::config::TransportMode::AdbReverseTcp => {
                use std::io::Read;
                let mut temp_buf = [0u8; 16384];

                while bg_running.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(host = ?host_addr, "Attempting TCP connection to host for USB/ADB streaming");
                    let stream = match std::net::TcpStream::connect_timeout(
                        &host_addr,
                        Duration::from_millis(1000),
                    ) {
                        Ok(s) => s,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                    let mut stream = stream;
                    let mut read_buf = Vec::with_capacity(65536);

                    while bg_running.load(std::sync::atomic::Ordering::Relaxed) {
                        match stream.read(&mut temp_buf) {
                            Ok(0) => {
                                tracing::info!("TCP streaming connection closed by host");
                                break;
                            }
                            Ok(n) => {
                                read_buf.extend_from_slice(&temp_buf[..n]);
                                while read_buf.len() >= linux_quest_protocol::packet::HEADER_SIZE {
                                    let mut header_slice =
                                        &read_buf[..linux_quest_protocol::packet::HEADER_SIZE];
                                    match linux_quest_protocol::packet::PacketHeader::decode(
                                        &mut header_slice,
                                    ) {
                                        Ok(header) => {
                                            // Reject unreasonable packet payload lengths (> 1MB) to prevent unbounded memory growth
                                            if header.payload_len > 1_048_576 {
                                                read_buf.remove(0);
                                                continue;
                                            }
                                            let total_size =
                                                linux_quest_protocol::packet::HEADER_SIZE
                                                    + header.payload_len as usize;
                                            if read_buf.len() >= total_size {
                                                let pkt_bytes: Vec<u8> =
                                                    read_buf.drain(..total_size).collect();
                                                match linux_quest_protocol::packet::Packet::from_bytes(
                                                    &pkt_bytes,
                                                ) {
                                                    Ok(packet) => {
                                                        let _ = packet_tx.try_send(packet);
                                                    }
                                                    Err(e) => {
                                                        tracing::trace!(
                                                            error = ?e,
                                                            "Failed to parse incoming packet from TCP"
                                                        );
                                                    }
                                                }
                                            } else {
                                                break;
                                            }
                                        }
                                        Err(_) => {
                                            // Bad header magic / corrupt byte: discard 1 byte and resynchronize
                                            read_buf.remove(0);
                                        }
                                    }
                                }
                            }
                            Err(ref e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::TimedOut => {}
                            Err(e) => {
                                tracing::trace!(error = ?e, "TCP stream read error");
                                break;
                            }
                        }
                    }
                }
            }
            crate::config::TransportMode::Udp => {
                let socket = match std::net::UdpSocket::bind("0.0.0.0:48440") {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = ?e, "Failed to bind UDP socket on 0.0.0.0:48440");
                        return;
                    }
                };
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 65536];
                let mut drop_warn = DropWarnLimiter::default();

                while bg_running.load(std::sync::atomic::Ordering::Relaxed) {
                    match socket.recv_from(&mut buf) {
                        Ok((len, source)) => {
                            if !accept_udp_source(udp_filter.as_deref(), source) {
                                if drop_warn.should_warn(source.ip(), std::time::Instant::now()) {
                                    tracing::warn!(
                                        %source,
                                        allowed = ?udp_filter,
                                        "Dropping UDP datagram from a source that is not the host \
                                         (further drops from this source are not logged for 60s)"
                                    );
                                }
                                continue;
                            }
                            match linux_quest_protocol::packet::Packet::from_bytes(&buf[..len]) {
                                Ok(packet) => {
                                    let _ = packet_tx.try_send(packet);
                                }
                                Err(e) => {
                                    tracing::trace!(error = ?e, "Failed to parse incoming packet");
                                }
                            }
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(e) => {
                            tracing::trace!(error = ?e, "UDP socket recv error");
                        }
                    }
                }
            }
        }
    });

    let mut is_active = false;

    // Main Android event and render loop
    while runtime.is_running() && !runtime.should_exit() {
        app.poll_events(
            Some(if is_active {
                Duration::ZERO
            } else {
                Duration::from_millis(50)
            }),
            |event| {
                if let PollEvent::Main(main_event) = event {
                    match main_event {
                        MainEvent::Resume { .. } => {
                            tracing::info!("Android activity resumed");
                            is_active = true;
                        }
                        MainEvent::Pause => {
                            tracing::info!("Android activity paused");
                            is_active = false;
                        }
                        MainEvent::Destroy => {
                            tracing::info!("Android activity destroying");
                            runtime.stop();
                        }
                        _ => {}
                    }
                }
            },
        );

        // Ingest any received transport packets
        while let Ok(pkt) = packet_rx.try_recv() {
            runtime.ingest_packet(pkt, std::time::Instant::now());
        }

        if is_active {
            if let Err(e) = runtime.step_frame() {
                tracing::warn!(error = ?e, "Error stepping client frame");
            }
        } else {
            // While paused, continue polling OpenXR events so state machine can transition
            if let Err(e) = runtime.poll_events() {
                tracing::warn!(error = ?e, "Error polling OpenXR events while paused");
            }
        }
    }

    running_flag.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = receiver_handle.join();

    unsafe {
        ndk_context::release_android_context();
    }

    tracing::info!("linux-quest client runtime exited cleanly");
}

/// Simulated Android entry helper for cross-platform unit tests.
#[cfg(not(target_os = "android"))]
pub fn simulated_android_entry(
    config: ClientConfig,
) -> crate::error::ClientResult<QuestClientRuntime> {
    QuestClientRuntime::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_filter_uses_host_ip_only_in_udp_mode() {
        let lan = SocketAddr::from(([192, 168, 1, 20], 48444));
        let udp = ClientConfig {
            transport_mode: TransportMode::Udp,
            host_addr: lan,
            ..Default::default()
        };
        assert_eq!(
            udp_source_filter(&udp),
            Some(vec![IpAddr::from([192, 168, 1, 20])])
        );

        let adb = ClientConfig {
            transport_mode: TransportMode::AdbReverseTcp,
            host_addr: lan,
            ..Default::default()
        };
        assert_eq!(udp_source_filter(&adb), None);
    }

    #[test]
    fn udp_filter_unspecified_host_means_no_filter() {
        let config = ClientConfig {
            transport_mode: TransportMode::Udp,
            host_addr: SocketAddr::from(([0, 0, 0, 0], 48444)),
            ..Default::default()
        };
        assert_eq!(udp_source_filter(&config), None);
    }

    #[test]
    fn udp_filter_accepts_any_extra_host_address() {
        let config = ClientConfig {
            transport_mode: TransportMode::Udp,
            host_addr: SocketAddr::from(([192, 168, 1, 20], 48444)),
            extra_host_ips: vec![IpAddr::from([10, 0, 0, 7]), IpAddr::from([192, 168, 1, 20])],
            ..Default::default()
        };
        let filter = udp_source_filter(&config).unwrap();
        assert_eq!(filter.len(), 2, "duplicates are folded");
        let from = |ip: [u8; 4]| SocketAddr::from((ip, 40000));
        assert!(accept_udp_source(Some(&filter), from([192, 168, 1, 20])));
        assert!(accept_udp_source(Some(&filter), from([10, 0, 0, 7])));
        assert!(!accept_udp_source(Some(&filter), from([172, 17, 0, 1])));
    }

    #[test]
    fn udp_filter_compares_canonical_addresses() {
        let v4 = IpAddr::from([192, 168, 1, 20]);
        let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        assert!(accept_udp_source(
            Some(&[v4]),
            SocketAddr::new(mapped, 40000)
        ));
        assert!(accept_udp_source(
            Some(&[mapped]),
            SocketAddr::new(v4, 40000)
        ));

        let config = ClientConfig {
            transport_mode: TransportMode::Udp,
            host_addr: SocketAddr::new(mapped, 48444),
            ..Default::default()
        };
        assert_eq!(udp_source_filter(&config), Some(vec![v4]));
    }

    #[test]
    fn warns_only_for_udp_with_loopback_host() {
        let udp_default = ClientConfig {
            transport_mode: TransportMode::Udp,
            ..Default::default()
        };
        assert!(udp_config_warning(&udp_default).is_some());

        let unspecified = ClientConfig {
            host_addr: SocketAddr::from(([0, 0, 0, 0], 48444)),
            ..udp_default.clone()
        };
        assert!(udp_config_warning(&unspecified).is_none());

        let lan = ClientConfig {
            host_addr: SocketAddr::from(([192, 168, 1, 20], 48444)),
            ..udp_default
        };
        assert!(udp_config_warning(&lan).is_none());
        assert!(udp_config_warning(&ClientConfig::default()).is_none());
    }

    #[test]
    fn drop_warnings_are_rate_limited_per_source() {
        let mut limiter = DropWarnLimiter::default();
        let t0 = std::time::Instant::now();
        let a = IpAddr::from([172, 17, 0, 1]);
        let b = IpAddr::from([10, 1, 1, 1]);
        assert!(limiter.should_warn(a, t0));
        assert!(!limiter.should_warn(a, t0 + std::time::Duration::from_secs(1)));
        assert!(limiter.should_warn(b, t0 + std::time::Duration::from_secs(1)));
        assert!(!limiter.should_warn(a, t0 + std::time::Duration::from_secs(59)));
        assert!(limiter.should_warn(a, t0 + std::time::Duration::from_secs(61)));
    }

    #[test]
    fn udp_loopback_datagrams_are_filtered_by_source() {
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .send_to(b"datagram", receiver.local_addr().unwrap())
            .unwrap();

        let mut buf = [0u8; 64];
        let (_, source) = receiver.recv_from(&mut buf).unwrap();
        let loopback = IpAddr::from([127, 0, 0, 1]);
        assert!(accept_udp_source(None, source));
        assert!(accept_udp_source(Some(&[loopback]), source));
        assert!(!accept_udp_source(
            Some(&[IpAddr::from([10, 9, 9, 9])]),
            source
        ));
    }

    #[test]
    fn test_simulated_android_entry() {
        let config = ClientConfig::default();
        #[cfg(not(target_os = "android"))]
        {
            let runtime = simulated_android_entry(config).unwrap();
            assert!(runtime.is_running());
        }
    }
}
