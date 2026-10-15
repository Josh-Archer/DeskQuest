use async_trait::async_trait;
use linux_quest_protocol::Packet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

pub mod adb;
pub mod control;
pub mod fec;
pub mod input_channel;
pub mod jitter_buffer;
pub mod pacing;
pub mod tcp;
pub mod udp;

pub use adb::{
    parse_devices_output, parse_reverse_list_output, AdbBridge, AdbBridgeConfig, AdbBridgeEvent,
    AdbBridgeHandle, AdbBridgeState, AdbCommandRunner, AdbDevice, AdbDeviceState, AdbError,
    AdbReverseRule, MockAdbRunner, SystemAdbRunner,
};
pub use control::{ControlChannel, ControlError, ControlMessage};
pub use fec::{FecError, FecHeader, ReedSolomonFec, XorFec};
pub use input_channel::{
    create_input_channel_pair, InputChannelConfig, InputChannelReceiver, InputChannelSender,
    DEFAULT_INPUT_PORT, DEFAULT_KEY_REDUNDANCY, MAX_INPUT_PACKET_SIZE,
};
pub use jitter_buffer::{AdaptiveJitterBuffer, JitterBufferConfig, JitterBufferStats};
pub use pacing::PacketPacer;
pub use tcp::TcpEndpoint;
pub use udp::{PacedUdpEndpoint, DEFAULT_UDP_MTU};

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("Connection closed")]
    ConnectionClosed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(#[from] linux_quest_protocol::ProtocolError),

    #[error("Packet too large: {size} bytes exceeds MTU {mtu}")]
    PacketTooLarge { size: usize, mtu: usize },

    #[error("FEC error: {0}")]
    Fec(#[from] fec::FecError),

    #[error("ADB error: {0}")]
    Adb(#[from] adb::AdbError),

    #[error("Control channel error: {0}")]
    Control(#[from] control::ControlError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    UsbAdb,
    UdpPaced,
    Loopback,
}

#[derive(Debug, Default)]
pub struct TransportStats {
    pub packets_sent: AtomicU64,
    pub packets_recv: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_recv: AtomicU64,
    pub fec_recovered_packets: AtomicU64,
}

#[async_trait]
pub trait TransportEndpoint: Send + Sync {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError>;
    async fn recv_packet(&mut self) -> Result<Packet, TransportError>;
    fn mode(&self) -> TransportMode;
}

/// In-memory loopback transport pair for testing pipeline latency and error recovery.
pub struct LoopbackEndpoint {
    sender: mpsc::UnboundedSender<Packet>,
    receiver: mpsc::UnboundedReceiver<Packet>,
    stats: Arc<TransportStats>,
}

impl Default for LoopbackEndpoint {
    fn default() -> Self {
        let (ep1, _) = Self::create_pair();
        ep1
    }
}

impl LoopbackEndpoint {
    pub fn create_pair() -> (Self, Self) {
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let stats1 = Arc::new(TransportStats::default());
        let stats2 = Arc::new(TransportStats::default());

        let ep1 = Self {
            sender: tx1,
            receiver: rx2,
            stats: stats1,
        };
        let ep2 = Self {
            sender: tx2,
            receiver: rx1,
            stats: stats2,
        };
        (ep1, ep2)
    }

    pub fn stats(&self) -> &TransportStats {
        &self.stats
    }
}

#[async_trait]
impl TransportEndpoint for LoopbackEndpoint {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError> {
        let len = packet.payload.len() as u64;
        self.sender
            .send(packet)
            .map_err(|_| TransportError::ConnectionClosed)?;

        self.stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    async fn recv_packet(&mut self) -> Result<Packet, TransportError> {
        let packet = self
            .receiver
            .recv()
            .await
            .ok_or(TransportError::ConnectionClosed)?;

        let len = packet.payload.len() as u64;
        self.stats.packets_recv.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_recv.fetch_add(len, Ordering::Relaxed);
        Ok(packet)
    }

    fn mode(&self) -> TransportMode {
        TransportMode::Loopback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[tokio::test]
    async fn test_loopback_transport_send_recv() {
        let (mut ep1, mut ep2) = LoopbackEndpoint::create_pair();

        let payload = Bytes::from_static(b"ping-packet");
        let header = PacketHeader::new(PacketType::Ping, 0, 1, 100, &payload);
        let packet = Packet::new(header, payload);

        ep1.send_packet(packet.clone()).await.expect("Send failed");
        let received = ep2.recv_packet().await.expect("Recv failed");

        assert_eq!(received.header.packet_type, PacketType::Ping);
        assert_eq!(received.payload, Bytes::from_static(b"ping-packet"));
        assert_eq!(ep1.stats().packets_sent.load(Ordering::Relaxed), 1);
        assert_eq!(ep2.stats().packets_recv.load(Ordering::Relaxed), 1);
    }
}
