//! Real mDNS round trip over the local network stack. Needs multicast on a loopback-capable
//! interface, so it only compiles with `--features mdns` and is `#[ignore]`d by default.
//! Run with: `cargo test -p linux-quest-transport --features mdns -- --ignored`
#![cfg(feature = "mdns")]

use linux_quest_protocol::VideoCodec;
use linux_quest_transport::{DiscoveryBackend, HostAdvertisement, MdnsDiscoveryBackend};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires multicast networking"]
async fn advertised_host_is_discovered_over_mdns() {
    let mut host = MdnsDiscoveryBackend::new().expect("mDNS daemon should start");
    let mut client = MdnsDiscoveryBackend::new().expect("mDNS daemon should start");

    let advertisement = HostAdvertisement {
        name: format!("linux-quest-test-{}", std::process::id()),
        control_port: 48444,
        video_port: 48445,
        input_port: 48446,
        codecs: vec![VideoCodec::Av1, VideoCodec::Hevc],
    };
    host.advertise(&advertisement)
        .await
        .expect("advertise should succeed");

    let found = client
        .browse(Duration::from_secs(5))
        .await
        .expect("browse should succeed")
        .into_iter()
        .find(|h| h.name == advertisement.name)
        .expect("advertised host should be discovered");

    assert_eq!(found.control_port, 48444);
    assert_eq!(found.video_port, 48445);
    assert_eq!(found.input_port, 48446);
    assert_eq!(found.codecs, advertisement.codecs);
    assert!(found.control_addr().is_some());

    host.withdraw().await.expect("withdraw should succeed");
}
