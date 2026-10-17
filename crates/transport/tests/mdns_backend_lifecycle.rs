//! Regression tests for `MdnsDiscoveryBackend` resource cleanup. They never need multicast:
//! `mdns-sd` starts its daemon thread without touching the network until work arrives.
//!
//! This file deliberately holds a single `#[test]` so no other test in the same process starts
//! daemon threads and perturbs the thread counts.
#![cfg(all(feature = "mdns", target_os = "linux"))]

use linux_quest_protocol::VideoCodec;
use linux_quest_transport::{
    DiscoveryBackend, DiscoveryError, HostAdvertisement, MdnsDiscoveryBackend,
};
use std::time::{Duration, Instant};

/// Number of live threads in this process named `mDNS_daemon` (the `mdns-sd` daemon thread).
fn daemon_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("/proc/self/task")
        .filter_map(Result::ok)
        .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
        .filter(|name| name.trim() == "mDNS_daemon")
        .count()
}

async fn wait_for_daemon_threads(expected: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = daemon_threads();
        if n == expected || Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn advertisement() -> HostAdvertisement {
    HostAdvertisement {
        name: "lifecycle-test".to_string(),
        control_port: 48444,
        video_port: 48445,
        input_port: 48446,
        codecs: vec![VideoCodec::Av1],
    }
}

#[tokio::test]
async fn dropping_or_shutting_down_a_backend_stops_its_daemon_thread() {
    assert_eq!(daemon_threads(), 0, "precondition: no daemon threads yet");

    // Drop, with and without a registered service.
    let idle = MdnsDiscoveryBackend::new().expect("daemon should start");
    let mut advertising = MdnsDiscoveryBackend::new().expect("daemon should start");
    advertising.advertise(&advertisement()).await.unwrap();
    assert_eq!(daemon_threads(), 2);
    drop(idle);
    drop(advertising);
    assert_eq!(
        wait_for_daemon_threads(0).await,
        0,
        "daemon threads leaked after drop"
    );

    // Many instances must not accumulate threads.
    for _ in 0..5 {
        drop(MdnsDiscoveryBackend::new().expect("daemon should start"));
    }
    assert_eq!(wait_for_daemon_threads(0).await, 0);

    // Explicit shutdown: waits for the daemon to exit, is idempotent, and later calls report
    // that discovery is unavailable instead of hanging or panicking.
    let mut backend = MdnsDiscoveryBackend::new().expect("daemon should start");
    backend.advertise(&advertisement()).await.unwrap();
    backend.shutdown().await.expect("shutdown");
    backend.shutdown().await.expect("shutdown is idempotent");
    assert_eq!(wait_for_daemon_threads(0).await, 0);
    assert!(matches!(
        backend.browse(Duration::from_millis(10)).await,
        Err(DiscoveryError::MulticastUnavailable(_))
    ));
    assert!(matches!(
        backend.advertise(&advertisement()).await,
        Err(DiscoveryError::MulticastUnavailable(_))
    ));
    backend
        .withdraw()
        .await
        .expect("withdraw with nothing registered");
    drop(backend);
}
