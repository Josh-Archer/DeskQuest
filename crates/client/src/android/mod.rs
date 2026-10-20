//! Android NativeActivity integration and main entry point.

pub mod lifecycle;

#[cfg(target_os = "android")]
use std::time::Duration;

#[cfg(target_os = "android")]
use android_activity::{AndroidApp, MainEvent, PollEvent};

#[cfg(target_os = "android")]
use self::lifecycle::{AppLifecycle, LifecycleEvent, NextAction, LOG_TAG};

/// JNI reference to the running `NativeActivity`, set at the top of [`android_main`].
///
/// `ndk_context::android_context().context()` is the `Application` (android-activity stores
/// that), but `XR_KHR_android_create_instance` needs the `Activity` that drives the session.
/// Given the `Application`, Horizon OS never considers the activity ready and the session
/// stays IDLE.
#[cfg(target_os = "android")]
static ACTIVITY: std::sync::atomic::AtomicPtr<std::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The `Activity` jobject for OpenXR instance creation, or null before `android_main` ran.
#[cfg(target_os = "android")]
pub(crate) fn activity_jobject() -> *mut std::ffi::c_void {
    ACTIVITY.load(std::sync::atomic::Ordering::Acquire)
}

use std::net::{IpAddr, SocketAddr};

use crate::config::{ClientConfig, TransportMode};
use crate::input_sender::{input_sender_config, InputSenderThread, KeyframeRequestSource};
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

/// Starts the UDP input/keepalive sender for `config`, or returns `None` when none should run.
///
/// Only `Udp` mode sends: `adb reverse` forwards TCP only, so in `AdbReverseTcp` mode the client
/// sends no input (and no keepalive) at all. Sending real controller input is not mapped yet,
/// so the source is keepalive-only: enough for the host to latch this headset and stream video
/// to it. See `crate::input_sender` for the design.
/// Starts the UDP input/keepalive sender (Udp mode only). Returns it with the flag that, once
/// raised (then [`crate::input_sender::InputSenderControl::notify`]), sends a keyframe request.
pub fn start_input_sender(
    config: &ClientConfig,
) -> Option<(
    InputSenderThread,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
)> {
    let Some(sender_config) = input_sender_config(config) else {
        match config.transport_mode {
            TransportMode::AdbReverseTcp => tracing::info!(
                "input sender not started: adb reverse carries TCP only, so no UDP input is sent \
                 in USB mode"
            ),
            TransportMode::Udp => tracing::warn!(
                host = %config.host_addr,
                "input sender not started: no usable host address; the host cannot learn this \
                 headset's address, so it will send no video"
            ),
        }
        return None;
    };
    tracing::info!(targets = ?sender_config.targets, "starting UDP input/keepalive sender");
    let source = KeyframeRequestSource::new();
    let keyframe_flag = source.requester();
    match InputSenderThread::spawn(sender_config, Box::new(source)) {
        Ok(sender) => Some((sender, keyframe_flag)),
        Err(e) => {
            tracing::warn!(error = ?e, "failed to start UDP input sender");
            None
        }
    }
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

/// Datagrams buffered between the receive thread and the render loop. 512 was ~54 ms of an
/// 80 Mbit/s stream, less than one stalled render-loop iteration.
#[cfg(target_os = "android")]
const PACKET_CHANNEL_DEPTH: usize = 4096;

/// Requested `SO_RCVBUF` for the UDP video socket (the kernel may cap it at `rmem_max`).
#[cfg(target_os = "android")]
const UDP_RECEIVE_BUFFER_BYTES: libc::c_int = 4 * 1024 * 1024;

/// Best-effort `SO_RCVBUF`; logs the size the kernel actually granted.
#[cfg(target_os = "android")]
fn set_receive_buffer(socket: &std::net::UdpSocket, bytes: libc::c_int) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    // SAFETY: valid socket fd and a c_int option value of the right size.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &bytes as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    let mut granted: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: as above, with out-pointers to locals of the declared size.
    unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &mut granted as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        );
    }
    tracing::info!(
        requested = bytes,
        granted,
        ok = set == 0,
        "UDP video socket receive buffer"
    );
}

/// Reads an Android system property, `None` if unset or empty.
#[cfg(target_os = "android")]
fn system_property(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buf = [0 as std::ffi::c_char; 92]; // PROP_VALUE_MAX
                                               // SAFETY: `buf` is PROP_VALUE_MAX bytes, the most __system_property_get writes.
    let len = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
    if len <= 0 {
        return None;
    }
    // SAFETY: the call NUL-terminates the value inside `buf`.
    let value = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(value.to_string_lossy().into_owned())
}

/// The default config with the `debug.linuxquest.*` overrides applied (see
/// [`ClientConfig::with_launch_overrides`]), and whether any override was given. Bad values are
/// logged and ignored.
#[cfg(target_os = "android")]
fn launch_config() -> (ClientConfig, bool) {
    use crate::config::{HOST_PROPERTY, TRANSPORT_PROPERTY};
    let host = system_property(HOST_PROPERTY);
    let transport = system_property(TRANSPORT_PROPERTY);
    let explicit = host.as_deref().is_some_and(|v| !v.trim().is_empty())
        || transport.as_deref().is_some_and(|v| !v.trim().is_empty());
    match ClientConfig::default().with_launch_overrides(host.as_deref(), transport.as_deref()) {
        Ok(config) => {
            tracing::info!(
                host = %config.host_addr,
                transport = ?config.transport_mode,
                "client transport configured"
            );
            (config, explicit)
        }
        Err(e) => {
            tracing::warn!("ignoring launch override: {e}");
            (ClientConfig::default(), false)
        }
    }
}

/// `true` when something accepts connections on the USB/ADB tunnel's loopback port, i.e.
/// `adb reverse tcp:48444 tcp:48444` is set up. The cable is the lower-latency path, so it
/// takes priority over LAN discovery.
#[cfg(target_os = "android")]
fn usb_tunnel_present(config: &ClientConfig) -> bool {
    std::net::TcpStream::connect_timeout(&config.host_addr, Duration::from_millis(300)).is_ok()
}

/// Finds a host over mDNS and points `config` at it. Blocks for at most a few seconds. The
/// config is returned unchanged when discovery is off, blocked or finds nothing.
#[cfg(target_os = "android")]
#[cfg_attr(not(feature = "mdns"), allow(unused_mut))]
fn autodiscover_config(mut config: ClientConfig) -> ClientConfig {
    #[cfg(not(feature = "mdns"))]
    {
        tracing::warn!(
            "built without the `mdns` feature: LAN discovery is off, set debug.linuxquest.host"
        );
        config
    }
    #[cfg(feature = "mdns")]
    {
        use crate::discovery::autoconnect::{
            autoconnect, AutoConnectOutcome, STARTUP_SCAN_ATTEMPTS,
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!(error = ?e, "no async runtime for LAN discovery");
                return config;
            }
        };
        let mut discovery =
            match crate::discovery::mdns_discovery(crate::discovery::AndroidMulticastLock::new()) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS unavailable; set debug.linuxquest.host");
                    return config;
                }
            };
        match runtime.block_on(autoconnect(
            &mut discovery,
            &mut config,
            STARTUP_SCAN_ATTEMPTS,
        )) {
            AutoConnectOutcome::Connected { host, other_hosts } => {
                tracing::info!(%host, other_hosts, "LAN discovery selected a host");
            }
            AutoConnectOutcome::NoHosts => tracing::warn!(
                "no host found on the LAN; is linux-quest-host (built with --features mdns) \
                 running on the same Wi-Fi? Set debug.linuxquest.host to bypass discovery"
            ),
            AutoConnectOutcome::Unavailable(reason) => {
                tracing::warn!(%reason, "LAN discovery unavailable; set debug.linuxquest.host")
            }
        }
        config
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
    ACTIVITY.store(app.activity_as_ptr(), std::sync::atomic::Ordering::Release);

    // android-activity initialises `ndk_context` (JavaVM + activity) before calling
    // `android_main`, for OpenXR, MediaCodec and JNI users. Initialising it again here
    // trips the "exactly once" assertion in ndk-context and aborts the app at startup.

    let (mut config, explicit_override) = launch_config();
    if crate::discovery::autoconnect::should_autodiscover(
        explicit_override,
        usb_tunnel_present(&config),
    ) {
        config = autodiscover_config(config);
    }

    // Background transport packet receiver (bounded channel: 512 packets, drops on overflow)
    let (packet_tx, packet_rx) =
        std::sync::mpsc::sync_channel::<linux_quest_protocol::packet::Packet>(PACKET_CHANNEL_DEPTH);
    let running_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let bg_running = running_flag.clone();
    let host_addr = config.host_addr;
    let transport_mode = config.transport_mode;
    let udp_filter = udp_source_filter(&config);
    if let Some(message) = udp_config_warning(&config) {
        tracing::warn!("{message}");
    }

    // Input/keepalive sender (Udp mode only). The receiver below acks it for each accepted
    // host datagram so it stays on the host address that works.
    let (input_sender, keyframe_flag) = match start_input_sender(&config) {
        Some((sender, flag)) => (Some(sender), Some(flag)),
        None => (None, None),
    };
    let ack_control: Option<crate::input_sender::InputSenderControl> =
        input_sender.as_ref().map(|s| s.control().clone());
    // Raises the keyframe request on the input channel and sends it right away.
    let requester_control = ack_control.clone();
    let keyframe_requester = move || -> Option<Box<dyn Fn() + Send>> {
        let flag = keyframe_flag.clone()?;
        let control = requester_control.clone()?;
        Some(Box::new(move || {
            tracing::info!("video lost; requesting a keyframe from the host");
            flag.store(true, std::sync::atomic::Ordering::Release);
            control.notify();
        }))
    };

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
                    let _ = stream.set_nodelay(true);
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
                // The default receive buffer holds ~10 ms of an 80 Mbit/s stream; any stall of
                // this thread then drops datagrams in the kernel without a trace.
                set_receive_buffer(&socket, UDP_RECEIVE_BUFFER_BYTES);
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
                                    if let Some(control) = &ack_control {
                                        control.ack();
                                    }
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

    // The XR session is created only once the activity has a window and is resumed (Horizon OS
    // rejects xrCreateSession / xrCreateSwapchainAndroidSurfaceKHR with "Activity is not yet in
    // the ready state" before that), and swapchains only once the session is running.
    // See `lifecycle` for the pure decision logic; every transition logs with tag `lq-lifecycle`.
    let mut lifecycle = AppLifecycle::new();
    if app.native_window().is_some() {
        lifecycle.on_event(LifecycleEvent::InitWindow);
    }
    let mut runtime: Option<QuestClientRuntime> = None;
    let mut create_failures = 0u32;
    let mut next_create_attempt = std::time::Instant::now();
    let mut swapchain_failures = 0u32;
    const MAX_STARTUP_FAILURES: u32 = 5;

    loop {
        let action = lifecycle.decide_next_action();
        app.poll_events(
            Some(if action == NextAction::RenderFrame {
                Duration::ZERO
            } else {
                Duration::from_millis(50)
            }),
            |event| {
                if let PollEvent::Main(main_event) = event {
                    let translated = match main_event {
                        MainEvent::InitWindow { .. } => Some(LifecycleEvent::InitWindow),
                        MainEvent::TerminateWindow { .. } => Some(LifecycleEvent::TermWindow),
                        MainEvent::Resume { .. } => Some(LifecycleEvent::Resume),
                        MainEvent::Pause => Some(LifecycleEvent::Pause),
                        MainEvent::GainedFocus => Some(LifecycleEvent::GainedFocus),
                        MainEvent::LostFocus => Some(LifecycleEvent::LostFocus),
                        MainEvent::Destroy => Some(LifecycleEvent::Destroy),
                        _ => None,
                    };
                    if let Some(ev) = translated {
                        lifecycle.on_event(ev);
                    }
                }
            },
        );

        // Ingest any received transport packets (dropped until the runtime exists)
        while let Ok(pkt) = packet_rx.try_recv() {
            if let Some(rt) = runtime.as_mut() {
                rt.ingest_packet(pkt, std::time::Instant::now());
            }
        }

        match lifecycle.decide_next_action() {
            NextAction::Wait => {}
            NextAction::CreateSession => {
                if std::time::Instant::now() >= next_create_attempt {
                    tracing::info!("{LOG_TAG}: window ready and resumed, creating OpenXR session");
                    match QuestClientRuntime::new(config.clone()) {
                        Ok(mut rt) => {
                            if let Some(request) = keyframe_requester() {
                                rt.set_keyframe_requester(request);
                            }
                            runtime = Some(rt);
                            lifecycle.on_session_created();
                        }
                        Err(e) => {
                            create_failures += 1;
                            tracing::error!(
                                error = ?e,
                                attempt = create_failures,
                                "{LOG_TAG}: failed to initialize Quest client runtime"
                            );
                            if create_failures >= MAX_STARTUP_FAILURES {
                                break;
                            }
                            next_create_attempt =
                                std::time::Instant::now() + Duration::from_millis(500);
                        }
                    }
                }
            }
            NextAction::DiscardSession => {
                runtime = None;
                lifecycle.on_session_discarded();
            }
            NextAction::PollEvents => {
                if let Some(rt) = runtime.as_mut() {
                    if let Err(e) = rt.poll_events() {
                        tracing::warn!(error = ?e, "{LOG_TAG}: error polling OpenXR events");
                    }
                }
            }
            NextAction::CreateSwapchains => {
                if let Some(rt) = runtime.as_mut() {
                    match rt.create_swapchains() {
                        Ok(()) => lifecycle.on_swapchains_created(),
                        Err(e) => {
                            swapchain_failures += 1;
                            tracing::error!(
                                error = ?e,
                                attempt = swapchain_failures,
                                "{LOG_TAG}: failed to create swapchains"
                            );
                            if swapchain_failures >= MAX_STARTUP_FAILURES {
                                break;
                            }
                        }
                    }
                }
            }
            NextAction::RenderFrame => {
                if let Some(rt) = runtime.as_mut() {
                    if let Err(e) = rt.step_frame() {
                        tracing::warn!(error = ?e, "Error stepping client frame");
                    }
                }
            }
            NextAction::Exit => break,
        }

        // Mirror the OpenXR session state into the lifecycle machine.
        if let Some(rt) = runtime.as_ref() {
            lifecycle.on_session_running(rt.is_session_running());
            if rt.should_exit() {
                lifecycle.on_exit_requested();
            }
        }
    }
    tracing::info!("{LOG_TAG}: leaving main loop");
    drop(runtime);

    running_flag.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = receiver_handle.join();
    if let Some(sender) = input_sender {
        sender.stop();
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
