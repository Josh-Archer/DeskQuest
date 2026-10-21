//! End to end over loopback TCP: the host's control service and headset hub on one side, the
//! headset's control link and a simulated headset runtime on the other. The host reads the
//! headset's layout and settings, changes them, and gets back what the headset applied.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use linux_quest_client::host_control::{HostLink, LinkConfig};
use linux_quest_client::{ClientConfig, QuestClientRuntime};
use linux_quest_host::{HeadsetError, HeadsetHub, HostClipboardService, HOST_CLIPBOARD_ORIGIN_ID};
use linux_quest_input::MockClipboardBackend;
use linux_quest_protocol::{ClipboardSyncEngine, HeadsetStatus, ScreenCurve, ScreenPlacement};

/// What the host side observed.
struct HostSide {
    first: HeadsetStatus,
    after_layout: HeadsetStatus,
    after_settings: HeadsetStatus,
}

fn quick_link(addr: std::net::SocketAddr) -> LinkConfig {
    LinkConfig {
        retry_interval: Duration::from_millis(50),
        poll_interval: Duration::from_millis(10),
        ..LinkConfig::new(vec![addr])
    }
}

#[test]
fn the_host_reads_and_changes_the_headset_layout_and_settings() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let hub = Arc::new(HeadsetHub::new());
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();

    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let (_in_tx, in_rx) = mpsc::unbounded_channel();
    let mut service = HostClipboardService::new(
        Arc::new(Mutex::new(MockClipboardBackend::new())),
        Arc::new(Mutex::new(ClipboardSyncEngine::new(
            HOST_CLIPBOARD_ORIGIN_ID,
        ))),
        out_tx,
        in_rx,
    )
    .with_listener(listener)
    .with_headset_hub(hub.clone());
    let running = service.running_flag();
    rt.spawn(async move {
        let _ = service.run().await;
    });

    // The host's script runs on the tokio runtime; the headset's render loop on this thread.
    let host_hub = hub.clone();
    let host = rt.spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(10);
        let first = loop {
            if let Some((status, _, _)) = host_hub.status() {
                break status;
            }
            assert!(Instant::now() < deadline, "the headset never reported");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let moved =
            ScreenPlacement::facing_user(0, -0.35, 0.1, 1.6, 1.3, 2.2, 1.2, ScreenCurve::Flat);
        let after_layout = host_hub
            .apply_layout(vec![moved], Duration::from_secs(5))
            .await
            .expect("layout applied");
        let mut settings = after_layout.settings;
        settings.hud = !settings.hud;
        settings.refresh_rate_hz = 72.0;
        settings.curve = ScreenCurve::Curved { radius_m: 2.0 };
        let after_settings = host_hub
            .apply_settings(settings, Duration::from_secs(5))
            .await
            .expect("settings applied");
        HostSide {
            first,
            after_layout,
            after_settings,
        }
    });

    let link = HostLink::spawn(quick_link(addr)).unwrap();
    let mut headset = QuestClientRuntime::new(ClientConfig::default()).unwrap();
    headset.set_device_name("Simulated Quest");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !host.is_finished() {
        assert!(Instant::now() < deadline, "the exchange did not finish");
        while let Some(command) = link.try_command() {
            headset.apply_host_command(command);
        }
        if link.is_connected() && headset.take_host_status_dirty() {
            link.publish(headset.headset_status());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let seen = rt.block_on(host).unwrap();

    assert_eq!(seen.first.device, "Simulated Quest");
    assert_eq!(seen.first.screens[0].display_id, 0);

    let primary = seen.after_layout.screen(0).unwrap();
    assert!((primary.yaw_rad() + 0.35).abs() < 1e-3, "{primary:?}");
    assert_eq!(primary.curve, ScreenCurve::Flat);
    assert!((primary.width_m - 2.2).abs() < 1e-3);
    // The headset itself moved: its runtime agrees with what it reported.
    assert!((headset.screen_transform().yaw + 0.35).abs() < 1e-3);

    let applied = seen.after_settings.settings;
    assert_eq!(applied.hud, !seen.after_layout.settings.hud);
    assert_eq!(applied.refresh_rate_hz, 72.0);
    assert_eq!(applied.curve, ScreenCurve::Curved { radius_m: 2.0 });
    let bent = seen.after_settings.screen(0).unwrap();
    assert!((bent.width_m - 2.2).abs() < 1e-2, "bending keeps the width");

    // The headset goes away: the hub forgets it and requests fail fast.
    link.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.is_connected() {
        assert!(Instant::now() < deadline, "the hub kept a closed headset");
        std::thread::sleep(Duration::from_millis(10));
    }
    let err = rt
        .block_on(hub.apply_settings(applied, Duration::from_millis(100)))
        .unwrap_err();
    assert_eq!(err, HeadsetError::NotConnected);
    running.store(false, std::sync::atomic::Ordering::SeqCst);
}
