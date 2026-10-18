//! A real loopback TCP connection to the real control listener is reported as USB only while
//! the (mock) ADB reverse tunnel is up. No adb binary or headset is involved.

use std::sync::Arc;
use std::time::Duration;

use linux_quest_host::adb_link::{spawn_adb_supervisor, AdbSupervisorConfig};
use linux_quest_host::link_status::{new_shared_link_tracker, LinkTransport, SharedLinkTracker};
use linux_quest_host::HostClipboardService;
use linux_quest_protocol::ClientConnectionMode;
use linux_quest_transport::{AdbDevice, AdbDeviceState, MockAdbRunner};

fn quest() -> AdbDevice {
    AdbDevice {
        serial: "QUEST3_ABC".into(),
        state: AdbDeviceState::Device,
        product: Some("eureka".into()),
        model: Some("Quest_3".into()),
        device_name: Some("eureka".into()),
        transport_id: Some("1".into()),
    }
}

async fn wait_for(link: &SharedLinkTracker, want: ClientConnectionMode) {
    for _ in 0..200 {
        if link
            .lock()
            .status(std::time::Instant::now())
            .connection_mode()
            == want
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "link never became {want:?}; is {:?}",
        link.lock().status(std::time::Instant::now())
    );
}

#[tokio::test]
async fn loopback_control_connection_is_usb_only_while_tunnel_is_up() {
    let link = new_shared_link_tracker(Duration::from_secs(5));

    // Real control listener on an ephemeral loopback port.
    let service = HostClipboardService::bind_auto("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let mut service = service.with_link_tracker(link.clone());
    let control_addr = service.local_addr().unwrap();
    let running = service.running_flag();
    let server = tokio::spawn(async move {
        let _ = service.run().await;
    });

    let runner = MockAdbRunner::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let mut cfg = AdbSupervisorConfig::default();
    cfg.bridge.poll_interval = Duration::from_millis(20);
    cfg.bridge.max_reconnect_attempts = 2;
    let adb = spawn_adb_supervisor(Arc::new(runner.clone()), cfg, link.clone(), shutdown_rx);

    // Loopback connection while no tunnel exists: not classifiable, so disconnected.
    let client = tokio::net::TcpStream::connect(control_addr).await.unwrap();
    for _ in 0..200 {
        if link.lock().status(std::time::Instant::now()).peer.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Give the supervisor several polls to (wrongly) mark the tunnel up.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let status = link.lock().status(std::time::Instant::now());
    assert!(!status.connected, "loopback without tunnel must not count");
    assert_eq!(status.connection_mode(), ClientConnectionMode::Disconnected);
    assert!(status.peer.is_some_and(|p| p.ip().is_loopback()));

    // Headset plugged in: tunnel up, the same connection is now USB.
    runner.add_device(quest());
    wait_for(&link, ClientConnectionMode::UsbAdb).await;
    assert_eq!(
        link.lock().status(std::time::Instant::now()).transport,
        Some(LinkTransport::UsbAdb)
    );
    assert_eq!(runner.get_rules("QUEST3_ABC").len(), 1);

    // Cable pulled: tunnel down, connection no longer reported.
    runner.remove_device("QUEST3_ABC");
    wait_for(&link, ClientConnectionMode::Disconnected).await;

    // Replugged: USB again.
    runner.add_device(quest());
    wait_for(&link, ClientConnectionMode::UsbAdb).await;

    // Daemon shutdown removes the mapping and clears the USB state.
    let _ = shutdown_tx.send(());
    adb.await.unwrap();
    assert!(runner.get_rules("QUEST3_ABC").is_empty());
    wait_for(&link, ClientConnectionMode::Disconnected).await;

    running.store(false, std::sync::atomic::Ordering::SeqCst);
    drop(client);
    server.abort();
}
