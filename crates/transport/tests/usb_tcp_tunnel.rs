use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType};
use linux_quest_transport::{
    negotiate_client, negotiate_server, recv_frame, send_frame, ControlMessage, TunnelFrame,
};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn quest_client_and_daemon_negotiate_and_multiplex_packets_and_control() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        negotiate_server(&mut reader, &mut writer).await.unwrap();

        let incoming = recv_frame(&mut reader).await.unwrap();
        let TunnelFrame::Packet(wire) = incoming else {
            panic!("expected input packet frame")
        };
        let input = Packet::from_bytes(&wire).unwrap();
        assert_eq!(input.header.packet_type, PacketType::InputEvent);
        assert_eq!(input.payload, Bytes::from_static(b"input"));

        send_frame(
            &mut writer,
            &TunnelFrame::Control(ControlMessage::Heartbeat {
                sequence: 3,
                timestamp_us: 42,
            }),
        )
        .await
        .unwrap();
        let video = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, 43, b"video"),
            Bytes::from_static(b"video"),
        );
        send_frame(&mut writer, &TunnelFrame::Packet(video.to_bytes().to_vec()))
            .await
            .unwrap();
    });

    // This is the same connection shape used by the Quest AdbReverseTcp client.
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    negotiate_client(&mut reader, &mut writer).await.unwrap();
    let input = Packet::new(
        PacketHeader::new(PacketType::InputEvent, 0, 1, 41, b"input"),
        Bytes::from_static(b"input"),
    );
    send_frame(&mut writer, &TunnelFrame::Packet(input.to_bytes().to_vec()))
        .await
        .unwrap();

    assert_eq!(
        recv_frame(&mut reader).await.unwrap(),
        TunnelFrame::Control(ControlMessage::Heartbeat {
            sequence: 3,
            timestamp_us: 42,
        })
    );
    let TunnelFrame::Packet(wire) = recv_frame(&mut reader).await.unwrap() else {
        panic!("expected video packet frame")
    };
    let video = Packet::from_bytes(&wire).unwrap();
    assert_eq!(video.header.packet_type, PacketType::VideoFrameChunk);
    assert_eq!(video.payload, Bytes::from_static(b"video"));
    daemon.await.unwrap();
}

#[tokio::test]
async fn negotiation_rejects_wrong_protocol_version() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let daemon = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut hello = [0; 8];
        stream.read_exact(&mut hello).await.unwrap();
        stream.write_all(b"NOPE\0\x01\n\n").await.unwrap();
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    assert!(negotiate_client(&mut reader, &mut writer).await.is_err());
    daemon.await.unwrap();
}
