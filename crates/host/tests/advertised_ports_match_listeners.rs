//! The advertised ports must be the ports the daemon's listeners hold.

use std::net::SocketAddr;

use linux_quest_host::advertise::{daemon_advertisement, ListeningPorts};
use linux_quest_host::{HostClipboardService, HostInputReceiver};
use linux_quest_transport::VIDEO_PORT_UNAVAILABLE;

#[tokio::test]
async fn advertised_ports_are_the_bound_listener_ports() {
    let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let control = HostClipboardService::bind_auto(loopback)
        .await
        .expect("control listener binds");
    let input = HostInputReceiver::bind_auto(loopback)
        .await
        .expect("input listener binds");
    let control_port = control
        .local_addr()
        .expect("control listener has an address")
        .port();
    let input_port = input
        .local_addr()
        .expect("input listener has an address")
        .port();

    let ad = daemon_advertisement(
        "den",
        ListeningPorts {
            control: control_port,
            input: input_port,
        },
    );
    ad.validate().expect("advertisement is publishable");

    assert_eq!(ad.control_port, control_port);
    assert_eq!(ad.input_port, input_port);
    // The advertised control port really accepts connections, which is what a client does next.
    tokio::net::TcpStream::connect(("127.0.0.1", ad.control_port))
        .await
        .expect("advertised control port accepts TCP");

    // No video listener exists, so no video port is advertised.
    assert_eq!(ad.video_port, VIDEO_PORT_UNAVAILABLE);
    assert!(!ad.txt_properties().contains_key("video"));
}
