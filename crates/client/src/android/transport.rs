//! UDP packet reception shared by immersive and Home panel presentation.
use super::{
    accept_udp_source, set_receive_buffer, udp_config_warning, udp_source_filter, DropWarnLimiter,
    UDP_RECEIVE_BUFFER_BYTES,
};
use crate::{config::ClientConfig, input_sender::InputSenderControl};
use linux_quest_protocol::packet::Packet;
use std::{
    sync::{atomic::AtomicBool, mpsc::SyncSender, Arc},
    thread::JoinHandle,
    time::Duration,
};

pub(super) fn spawn_udp_receiver(
    config: &ClientConfig,
    bg_running: Arc<AtomicBool>,
    receiver_ack_control: Option<InputSenderControl>,
    packet_tx: SyncSender<Packet>,
) -> JoinHandle<()> {
    let udp_filter = udp_source_filter(config);
    if let Some(message) = udp_config_warning(config) {
        tracing::warn!("{message}");
    }
    std::thread::spawn(move || {
        // Let the previous presentation mode finish releasing its receive socket.
        let bind_until = std::time::Instant::now() + Duration::from_secs(5);
        let socket = loop {
            if !bg_running.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match std::net::UdpSocket::bind("0.0.0.0:48440") {
                Ok(s) => break s,
                Err(e)
                    if e.kind() == std::io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < bind_until =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "Failed to bind UDP socket on 0.0.0.0:48440");
                    return;
                }
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
                            if let Some(control) = &receiver_ack_control {
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
    })
}
