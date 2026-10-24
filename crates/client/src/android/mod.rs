//! Android NativeActivity integration and main entry point.

#[cfg(target_os = "android")]
mod home_panel;
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
use crate::runtime::QuestClientRuntime;

/// Source IPs that UDP video datagrams must come from, or `None` to accept any source.
///
/// Only the UDP path filters, and only by IP: the host sends from an ephemeral port, so
/// its port is not known. The ADB/USB path connects to `host_addr` and never filters.
/// An unspecified IP (`0.0.0.0`) in `host_addr` means "unset", so no filter is applied.
/// The list is `host_addr` plus `extra_host_ips`, all in canonical form (IPv4-mapped IPv6 is
/// folded to IPv4). In `Auto` mode a loopback `host_addr` (the default) means the LAN host is
/// not known yet, so only loopback is accepted until discovery sets it.
pub fn udp_source_filter(config: &ClientConfig) -> Option<Vec<IpAddr>> {
    match config.transport_mode {
        TransportMode::Udp | TransportMode::Auto if !config.host_addr.ip().is_unspecified() => {
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

/// Datagrams buffered between the receive thread and the render loop. 512 was ~54 ms of an
/// 80 Mbit/s stream, less than one stalled render-loop iteration.
#[cfg(target_os = "android")]
const PACKET_CHANNEL_DEPTH: usize = 4096;

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

#[cfg(target_os = "android")]
fn pump_android_keyboard_events(
    app: &AndroidApp,
    queue: &crate::input_sender::InputEventQueue,
    waker: &crate::transport::InputWaker,
    virtual_keyboard_visible: &mut bool,
    last_virtual_text: &mut String,
) {
    use android_activity::{input::InputEvent, InputStatus};
    use linux_quest_protocol::{ElementState, InputEvent as ProtocolEvent};

    let Ok(mut events) = app.input_events_iter() else {
        return;
    };
    let mut queued = false;
    while events.next(|event| match event {
        InputEvent::KeyEvent(key) => {
            let modifiers = crate::input::android_meta_to_modifiers(key.meta_state().0 as i32);
            let keycode: u32 = key.key_code().into();
            let action: u32 = key.action().into();
            if keycode == crate::input::keycodes::AKEYCODE_K as u32 && modifiers.is_meta() {
                if action == 0 && key.repeat_count() == 0 {
                    if *virtual_keyboard_visible {
                        set_android_ime_visible(app, false);
                        last_virtual_text.clear();
                    } else {
                        set_android_ime_visible(app, true);
                        last_virtual_text.clear();
                    }
                    *virtual_keyboard_visible = !*virtual_keyboard_visible;
                }
                return InputStatus::Handled;
            }
            if configured_escape_chord().is_some_and(|(chord_modifiers, chord_key)| {
                keycode == chord_key && modifiers.contains(chord_modifiers)
            }) {
                if action == 0 {
                    set_android_ime_visible(app, false);
                    *virtual_keyboard_visible = false;
                    last_virtual_text.clear();
                }
                return InputStatus::Handled;
            }
            let reserved_system_shortcut = modifiers.is_alt()
                && keycode == crate::input::keycodes::AKEYCODE_TAB as u32
                || modifiers.is_ctrl()
                    && modifiers.is_alt()
                    && ((crate::input::keycodes::AKEYCODE_F1
                        ..=crate::input::keycodes::AKEYCODE_F12)
                        .contains(&(keycode as i32)));
            if reserved_system_shortcut {
                return InputStatus::Unhandled;
            }
            let state = match action {
                0 => ElementState::Pressed,
                1 => ElementState::Released,
                _ => return InputStatus::Unhandled,
            };
            if let Some(keycode) = crate::input::android_keycode_to_evdev(keycode as i32) {
                queue.push(ProtocolEvent::KeyboardKey {
                    keycode,
                    state,
                    modifiers,
                });
                queued = true;
                InputStatus::Handled
            } else {
                InputStatus::Unhandled
            }
        }
        // Android's IME reports committed UTF-8 text separately from physical key events.
        InputEvent::TextEvent(state) if !state.text.is_empty() => {
            queue.push(ProtocolEvent::TextCommit {
                text: state.text.clone(),
            });
            queued = true;
            InputStatus::Handled
        }
        _ => InputStatus::Unhandled,
    }) {}
    if *virtual_keyboard_visible {
        if let Some(text) = android_ime_committed_text(app) {
            queued |= queue_committed_text_delta(last_virtual_text, &text, queue);
        }
    }
    if queued {
        waker.wake();
    }
}

#[cfg(target_os = "android")]
fn with_ime_bridge<R>(
    app: &AndroidApp,
    call: impl FnOnce(
        &mut jni::JNIEnv<'_>,
        &jni::objects::JObject<'_>,
        &jni::objects::JClass<'_>,
    ) -> jni::errors::Result<R>,
) -> jni::errors::Result<R> {
    use jni::objects::{JClass, JObject};
    use std::mem::ManuallyDrop;

    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    // AndroidApp owns this global reference; do not delete it when the wrapper drops.
    let activity = ManuallyDrop::new(unsafe { JObject::from_raw(app.activity_as_ptr().cast()) });
    let loader = env
        .call_method(
            &*activity,
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )?
        .l()?;
    let class_name = env.new_string("com.linuxquest.client.ImeBridge")?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&class_name).into()],
        )?
        .l()?;
    let class = JClass::from(class);
    call(&mut env, &activity, &class)
}

#[cfg(target_os = "android")]
fn set_android_ime_visible(app: &AndroidApp, visible: bool) {
    let result = with_ime_bridge(app, |env, activity, class| {
        env.call_static_method(
            class,
            "setVisible",
            "(Landroid/app/Activity;Z)V",
            &[activity.into(), visible.into()],
        )?;
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(?error, "failed to show or hide the Android text input");
    }
}

#[cfg(target_os = "android")]
fn android_ime_committed_text(app: &AndroidApp) -> Option<String> {
    with_ime_bridge(app, |env, activity, class| {
        let value = env
            .call_static_method(
                class,
                "committedText",
                "(Landroid/app/Activity;)Ljava/lang/String;",
                &[activity.into()],
            )?
            .l()?;
        let value = jni::objects::JString::from(value);
        let text: String = env.get_string(&value)?.into();
        Ok(text)
    })
    .map_err(|error| tracing::warn!(?error, "failed to read committed Android IME text"))
    .ok()
}

#[cfg(any(target_os = "android", test))]
fn queue_committed_text_delta(
    previous: &mut String,
    current: &str,
    queue: &crate::input_sender::InputEventQueue,
) -> bool {
    use linux_quest_protocol::{ElementState, InputEvent, KeyModifiers};

    if previous == current {
        return false;
    }
    let old: Vec<char> = previous.chars().collect();
    let new: Vec<char> = current.chars().collect();
    let common = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    for _ in common..old.len() {
        for state in [ElementState::Pressed, ElementState::Released] {
            queue.push(InputEvent::KeyboardKey {
                keycode: crate::input::keycodes::KEY_BACKSPACE,
                state,
                modifiers: KeyModifiers::empty(),
            });
        }
    }
    if common < new.len() {
        queue.push(InputEvent::TextCommit {
            text: new[common..].iter().collect(),
        });
    }
    previous.clear();
    previous.push_str(current);
    true
}

/// Escape chord format is modifier names followed by an Android key name, for example
/// `CTRL+ALT+ESC` or `META+BACK`. Invalid values disable the chord instead of capturing keys.
#[cfg(target_os = "android")]
fn configured_escape_chord() -> Option<(linux_quest_protocol::KeyModifiers, u32)> {
    configured_escape_chord_from(
        system_property("debug.linuxquest.keyboard_escape_chord")
            .or_else(|| std::env::var("LINUX_QUEST_KEYBOARD_ESCAPE_CHORD").ok())
            .as_deref(),
    )
}

#[cfg(any(target_os = "android", test))]
fn configured_escape_chord_from(
    configured: Option<&str>,
) -> Option<(linux_quest_protocol::KeyModifiers, u32)> {
    use linux_quest_protocol::KeyModifiers;
    let chord = configured.unwrap_or("CTRL+ALT+ESC");
    let mut parts = chord.split('+').map(str::trim);
    let (mut modifiers, key) = (KeyModifiers::empty(), parts.next_back()?);
    for modifier in parts {
        let flag = match modifier.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => KeyModifiers::CTRL,
            "ALT" => KeyModifiers::ALT,
            "SHIFT" => KeyModifiers::SHIFT,
            "META" | "SUPER" => KeyModifiers::META,
            _ => return None,
        };
        modifiers.insert(flag);
    }
    if modifiers == KeyModifiers::empty() {
        return None;
    }
    let key = match key.to_ascii_uppercase().as_str() {
        "ESC" | "ESCAPE" => crate::input::keycodes::AKEYCODE_ESCAPE as u32,
        "BACK" => crate::input::keycodes::AKEYCODE_BACK as u32,
        "F1" => crate::input::keycodes::AKEYCODE_F1 as u32,
        "F2" => crate::input::keycodes::AKEYCODE_F2 as u32,
        "F3" => crate::input::keycodes::AKEYCODE_F3 as u32,
        "F4" => crate::input::keycodes::AKEYCODE_F4 as u32,
        "F5" => crate::input::keycodes::AKEYCODE_F5 as u32,
        "F6" => crate::input::keycodes::AKEYCODE_F6 as u32,
        "F7" => crate::input::keycodes::AKEYCODE_F7 as u32,
        "F8" => crate::input::keycodes::AKEYCODE_F8 as u32,
        "F9" => crate::input::keycodes::AKEYCODE_F9 as u32,
        "F10" => crate::input::keycodes::AKEYCODE_F10 as u32,
        "F11" => crate::input::keycodes::AKEYCODE_F11 as u32,
        "F12" => crate::input::keycodes::AKEYCODE_F12 as u32,
        _ => return None,
    };
    Some((modifiers, key))
}

#[cfg(test)]
mod keyboard_shortcut_tests {
    use super::{configured_escape_chord_from, queue_committed_text_delta};
    use crate::input::keycodes;
    use crate::input_sender::InputSource;
    use linux_quest_protocol::{ElementState, InputEvent, KeyModifiers};

    #[test]
    fn escape_chord_defaults_and_accepts_android_property_syntax() {
        assert_eq!(
            configured_escape_chord_from(None),
            Some((
                KeyModifiers::CTRL | KeyModifiers::ALT,
                keycodes::AKEYCODE_ESCAPE as u32,
            ))
        );
        assert_eq!(
            configured_escape_chord_from(Some("super + back")),
            Some((KeyModifiers::META, keycodes::AKEYCODE_BACK as u32))
        );
    }

    #[test]
    fn invalid_or_unmodified_escape_chords_are_disabled() {
        assert_eq!(configured_escape_chord_from(Some("ESC")), None);
        assert_eq!(configured_escape_chord_from(Some("CTRL+F13")), None);
        assert_eq!(
            configured_escape_chord_from(Some("CTRL+ALT+ESC+SHIFT")),
            None
        );
    }

    #[test]
    fn ime_commits_unicode_and_turns_edits_into_backspaces() {
        let mut source = crate::input_sender::KeyframeRequestSource::new();
        let queue = source.input_queue();
        let mut previous = String::new();
        assert!(queue_committed_text_delta(
            &mut previous,
            "naïve 東京",
            &queue
        ));
        assert!(!queue_committed_text_delta(
            &mut previous,
            "naïve 東京",
            &queue
        ));
        assert!(queue_committed_text_delta(
            &mut previous,
            "naïve 東",
            &queue
        ));

        assert_eq!(
            source.poll(),
            vec![
                InputEvent::TextCommit {
                    text: "naïve 東京".into()
                },
                InputEvent::KeyboardKey {
                    keycode: keycodes::KEY_BACKSPACE,
                    state: ElementState::Pressed,
                    modifiers: KeyModifiers::empty(),
                },
                InputEvent::KeyboardKey {
                    keycode: keycodes::KEY_BACKSPACE,
                    state: ElementState::Released,
                    modifiers: KeyModifiers::empty(),
                },
            ]
        );
    }
}

/// The default config with the `debug.linuxquest.*` overrides applied (see
/// [`ClientConfig::with_launch_overrides`]). Bad values are logged and ignored.
#[cfg(target_os = "android")]
fn launch_config() -> ClientConfig {
    use crate::config::{HOST_PROPERTY, TRANSPORT_PROPERTY};
    let host = system_property(HOST_PROPERTY);
    let transport = system_property(TRANSPORT_PROPERTY);
    match ClientConfig::default().with_launch_overrides(host.as_deref(), transport.as_deref()) {
        Ok(config) => {
            tracing::info!(
                host = %config.host_addr,
                transport = ?config.transport_mode,
                "client transport configured"
            );
            config
        }
        Err(e) => {
            tracing::warn!("ignoring launch override: {e}");
            ClientConfig::default()
        }
    }
}

/// One LAN discovery round: the host to use, with its addresses and input port, or why none.
/// Blocks for a few seconds at most.
#[cfg(target_os = "android")]
fn discover_lan_host() -> Result<ClientConfig, String> {
    #[cfg(not(feature = "mdns"))]
    {
        Err("built without the `mdns` feature: set debug.linuxquest.host".into())
    }
    #[cfg(feature = "mdns")]
    {
        use crate::discovery::autoconnect::{
            autoconnect, AutoConnectOutcome, STARTUP_SCAN_ATTEMPTS,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("no async runtime for LAN discovery: {e}"))?;
        let mut discovery =
            crate::discovery::mdns_discovery(crate::discovery::AndroidMulticastLock::new())
                .map_err(|e| format!("mDNS unavailable: {e}"))?;
        let mut config = ClientConfig::default();
        match runtime.block_on(autoconnect(
            &mut discovery,
            &mut config,
            STARTUP_SCAN_ATTEMPTS,
        )) {
            AutoConnectOutcome::Connected { host, other_hosts } => {
                tracing::info!(%host, addr = %config.host_addr, other_hosts, "LAN discovery found the host");
                Ok(config)
            }
            AutoConnectOutcome::NoHosts => Err(
                "no host answered on the LAN; is linux-quest-host (built with --features mdns) \
                 running on the same Wi-Fi? debug.linuxquest.host bypasses discovery"
                    .into(),
            ),
            AutoConnectOutcome::Unavailable(reason) => {
                Err(format!("LAN discovery unavailable: {reason}"))
            }
        }
    }
}

/// How long LAN discovery waits between rounds that found nothing.
#[cfg(target_os = "android")]
const LAN_DISCOVERY_RETRY: Duration = Duration::from_secs(10);

/// Looks for the host on the LAN in the background until it is found or `running` clears, and
/// points the Wi-Fi path at it. The USB tunnel does not wait for this: with the cable plugged
/// in, streaming starts at once, and Wi-Fi is ready by the time the cable comes out.
#[cfg(target_os = "android")]
fn spawn_lan_discovery(
    wifi: crate::transport::WifiHandle,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let spawned = std::thread::Builder::new()
        .name("lq-lan-discovery".into())
        .spawn(move || {
            let mut failures = 0u32;
            while running.load(Ordering::Acquire) && !wifi.has_host() {
                match discover_lan_host() {
                    Ok(found) => {
                        wifi.set_host(
                            found.host_addr.ip(),
                            &found.extra_host_ips,
                            found.input_port,
                        );
                        return;
                    }
                    Err(reason) if cfg!(not(feature = "mdns")) => {
                        tracing::warn!("{reason}");
                        return;
                    }
                    Err(reason) => {
                        if failures == 0 {
                            tracing::warn!(
                                retry_s = LAN_DISCOVERY_RETRY.as_secs(),
                                "{reason}; Wi-Fi waits for the host (USB works meanwhile)"
                            );
                        } else {
                            tracing::debug!("{reason}");
                        }
                        failures = failures.saturating_add(1);
                    }
                }
                let until = std::time::Instant::now() + LAN_DISCOVERY_RETRY;
                while running.load(Ordering::Acquire) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = ?e, "could not start LAN discovery");
    }
}

/// Starts the transport `config` asks for, and LAN discovery when its Wi-Fi path has no host.
/// Every packet from the host goes to `packets`.
#[cfg(target_os = "android")]
fn start_transport(
    config: &ClientConfig,
    packets: std::sync::mpsc::SyncSender<linux_quest_protocol::packet::Packet>,
    source: crate::input_sender::CombinedInputSource,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<crate::transport::ClientTransport> {
    use crate::transport::{ClientTransport, TransportConfig};
    let transport = match ClientTransport::spawn_with_source(
        TransportConfig::for_client(config),
        source,
        packets,
    ) {
        Ok(transport) => transport,
        Err(e) => {
            tracing::warn!(error = ?e, "could not start the transport to the host");
            return None;
        }
    };
    if crate::discovery::autoconnect::needs_lan_discovery(config) {
        if let Some(wifi) = transport.wifi() {
            spawn_lan_discovery(wifi, running);
        }
    }
    Some(transport)
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
    // Opt-in, once-per-second export of the existing HUD and decoder counters.
    // Keep this separate from optical motion-to-photon measurements.
    let validation_metrics = system_property("debug.linuxquest.metrics").as_deref() == Some("1");
    let mut next_metrics = std::time::Instant::now() + Duration::from_secs(1);
    ACTIVITY.store(app.activity_as_ptr(), std::sync::atomic::Ordering::Release);

    // android-activity initialises `ndk_context` (JavaVM + activity) before calling
    // `android_main`, for OpenXR, MediaCodec and JNI users. Initialising it again here
    // trips the "exactly once" assertion in ndk-context and aborts the app at startup.

    // In Auto mode (the default) the USB tunnel and Wi-Fi run side by side; LAN discovery runs
    // in the background, so nothing here waits for it.
    let mut config = launch_config();

    // Settings and screen layout persist in the app's internal files directory. Without this
    // every change was lost on restart (issue #114).
    let storage = match crate::storage::ClientStorage::for_android(app.internal_data_path()) {
        Ok(storage) => {
            tracing::info!(dir = ?storage.dir(), "persisting settings and layout");
            storage.configure(&mut config);
            Some(storage)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "no storage for settings and layout; changes will not persist");
            None
        }
    };

    // Settings channel to the host: it can read and change the layout and settings. Kept
    // across OpenXR session restarts.
    let host_link = match crate::host_control::HostLink::spawn(
        crate::host_control::LinkConfig::new(crate::host_control::control_candidates(&config)),
    ) {
        Ok(link) => Some(link),
        Err(e) => {
            tracing::warn!(error = ?e, "could not start the host settings channel");
            None
        }
    };
    let device_name = system_property("ro.product.model").unwrap_or_else(|| "Meta Quest".into());
    let mut next_host_status = std::time::Instant::now();

    // Packets from the host, over USB or Wi-Fi (bounded channel, drops on overflow).
    let (packet_tx, packet_rx) =
        std::sync::mpsc::sync_channel::<linux_quest_protocol::packet::Packet>(PACKET_CHANNEL_DEPTH);
    let running_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    // USB tunnel, Wi-Fi, or both (see `crate::transport`). Input goes over whichever path is
    // live; each path reopens itself when it dies.
    let transport = start_transport(
        &config,
        packet_tx,
        crate::input_sender::CombinedInputSource::new(),
        running_flag.clone(),
    );
    let input_queue = transport.as_ref().map(|t| t.keyboard_queue());
    let pointer_events = transport.as_ref().map(|t| t.pointer_events());
    let waker = transport.as_ref().map(|t| t.waker()).unwrap_or_default();
    let keyframe_flag = transport.as_ref().map(|t| t.keyframe_flags());
    let tunnel_reconnect = transport.as_ref().and_then(|t| t.tunnel_reconnect());
    // Raises the keyframe request on the input channel and sends it right away.
    let requester_waker = waker.clone();
    let keyframe_requester = move || -> Option<Box<dyn Fn(u16) + Send>> {
        let flags = keyframe_flag.clone()?;
        let waker = requester_waker.clone();
        Some(Box::new(move |stream_id| {
            tracing::info!(stream_id, "video lost; requesting a keyframe from the host");
            flags.raise(stream_id);
            waker.wake();
        }))
    };

    // The XR session is created only once the activity has a window and is resumed (Horizon OS
    // rejects xrCreateSession / xrCreateSwapchainAndroidSurfaceKHR with "Activity is not yet in
    // the ready state" before that), and swapchains only once the session is running.
    // See `lifecycle` for the pure decision logic; every transition logs with tag `lq-lifecycle`.
    let mut lifecycle = AppLifecycle::new();
    let mut virtual_keyboard_visible = false;
    let mut last_virtual_text = String::new();
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

        // After a pause (the headset slept, or the app was in the background) the USB tunnel can
        // look open on both ends while it carries nothing, so it is reopened.
        if lifecycle.take_resumed_after_pause() {
            if let Some(reconnect) = &tunnel_reconnect {
                tracing::info!("{LOG_TAG}: resumed after a pause; reopening the USB tunnel");
                reconnect.request();
            }
        }

        // Drain Bluetooth/USB keyboard input on the NativeActivity thread and forward it over
        // the live transport, like the controllers' input.
        if let Some(queue) = &input_queue {
            pump_android_keyboard_events(
                &app,
                queue,
                &waker,
                &mut virtual_keyboard_visible,
                &mut last_virtual_text,
            );
        }

        // Ingest any received transport packets (dropped until the runtime exists)
        while let Ok(pkt) = packet_rx.try_recv() {
            if let Some(rt) = runtime.as_mut() {
                rt.ingest_packet(pkt, std::time::Instant::now());
            }
        }

        // Layout and settings from the host, and the status it is told in return.
        if let (Some(link), Some(rt)) = (&host_link, runtime.as_mut()) {
            while let Some(command) = link.try_command() {
                rt.apply_host_command(command);
            }
            let now = std::time::Instant::now();
            if link.is_connected() && (rt.take_host_status_dirty() || now >= next_host_status) {
                link.publish(rt.headset_status());
                next_host_status = now + crate::host_control::STATUS_INTERVAL;
            }
        }

        match lifecycle.decide_next_action() {
            NextAction::Wait => {}
            NextAction::CreateSession => {
                if std::time::Instant::now() >= next_create_attempt {
                    tracing::info!("{LOG_TAG}: window ready and resumed, creating OpenXR session");
                    match QuestClientRuntime::new(config.clone()) {
                        Ok(mut rt) => {
                            if let Some(storage) = &storage {
                                storage.attach(&mut rt);
                            }
                            rt.set_device_name(device_name.clone());
                            if let Some(request) = keyframe_requester() {
                                rt.set_keyframe_requester(request);
                            }
                            if let Some(events) = pointer_events.clone() {
                                let latency_events = events.clone();
                                let waker = waker.clone();
                                rt.set_pointer_event_sink(Box::new(move |event| {
                                    events.push(event);
                                    waker.wake();
                                }));
                                rt.set_pointer_queue_latency_source(Box::new(move || {
                                    latency_events.last_queue_latency_us()
                                }));
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
                    if validation_metrics && std::time::Instant::now() >= next_metrics {
                        tracing::info!("LQ_METRICS {}", rt.validation_metrics());
                        next_metrics = std::time::Instant::now() + Duration::from_secs(1);
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

    running_flag.store(false, std::sync::atomic::Ordering::Release);
    if let Some(transport) = transport {
        transport.stop();
    }
    if let Some(link) = host_link {
        link.stop();
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
