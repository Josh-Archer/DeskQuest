//! Dedicated low-latency UDP input channel for mouse, keyboard, and controller events.
//!
//! Operates independently of the video streaming pipeline:
//! - Small datagrams (< 1KB)
//! - No pacing or FEC delay
//! - Drop-stale-by-sequence for ephemeral mouse motion
//! - Redundant transmission of the last N keyboard events to prevent stuck keys across UDP packet loss

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

use linux_quest_protocol::{InputBatch, InputEvent, RedundantKeyEvent};

use crate::TransportError;

pub const DEFAULT_INPUT_PORT: u16 = 48442;
pub const DEFAULT_KEY_REDUNDANCY: usize = 4;
pub const MAX_INPUT_PACKET_SIZE: usize = 1024;

/// Configuration for the low-latency input channel.
#[derive(Debug, Clone)]
pub struct InputChannelConfig {
    pub bind_addr: SocketAddr,
    pub remote_addr: Option<SocketAddr>,
    pub redundancy_count: usize,
    pub drop_stale: bool,
}

impl Default for InputChannelConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], DEFAULT_INPUT_PORT)),
            remote_addr: None,
            redundancy_count: DEFAULT_KEY_REDUNDANCY,
            drop_stale: true,
        }
    }
}

/// Sender for low-latency remote input events over UDP.
pub struct InputChannelSender {
    socket: Arc<UdpSocket>,
    remote_addr: Option<SocketAddr>,
    sequence: u64,
    redundancy_count: usize,
    key_history: VecDeque<RedundantKeyEvent>,
    next_key_id: u64,
}

impl InputChannelSender {
    pub fn new(
        socket: UdpSocket,
        remote_addr: Option<SocketAddr>,
        redundancy_count: usize,
    ) -> Self {
        Self {
            socket: Arc::new(socket),
            remote_addr,
            sequence: 0,
            redundancy_count,
            key_history: VecDeque::with_capacity(redundancy_count + 1),
            next_key_id: 0,
        }
    }

    pub async fn bind(
        bind_addr: &str,
        remote_addr: Option<SocketAddr>,
        redundancy_count: usize,
    ) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(Self::new(socket, remote_addr, redundancy_count))
    }

    pub fn set_remote_addr(&mut self, remote_addr: SocketAddr) {
        self.remote_addr = Some(remote_addr);
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.socket.local_addr()?)
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Transmit a batch of input events immediately with sequence numbering and key redundancy.
    pub async fn send_events(
        &mut self,
        events: Vec<InputEvent>,
        timestamp_us: u64,
    ) -> Result<u64, TransportError> {
        self.sequence += 1;
        let seq = self.sequence;

        let mut first_key_id = 0;
        let mut key_count = 0;

        // Collect new keyboard events in this batch and add to redundant history
        for ev in &events {
            if matches!(ev, InputEvent::KeyboardKey { .. }) {
                self.next_key_id += 1;
                let key_id = self.next_key_id;
                if key_count == 0 {
                    first_key_id = key_id;
                }
                key_count += 1;

                if self.redundancy_count > 0 {
                    if self.key_history.len() >= self.redundancy_count {
                        self.key_history.pop_front();
                    }
                    self.key_history.push_back(RedundantKeyEvent {
                        key_id,
                        event: ev.clone(),
                    });
                }
            }
        }

        // Build redundant keys slice (historical keys preceding this batch)
        let redundant_keys: Vec<RedundantKeyEvent> = if self.redundancy_count > 0 {
            self.key_history
                .iter()
                .filter(|rk| rk.key_id < first_key_id || first_key_id == 0)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        let batch = InputBatch::new(seq, timestamp_us)
            .with_first_key_id(first_key_id)
            .with_events(events)
            .with_redundant_keys(redundant_keys);

        let wire_bytes = batch.encode();
        if wire_bytes.len() > MAX_INPUT_PACKET_SIZE {
            return Err(TransportError::PacketTooLarge {
                size: wire_bytes.len(),
                mtu: MAX_INPUT_PACKET_SIZE,
            });
        }

        if let Some(target) = self.remote_addr {
            self.socket.send_to(&wire_bytes, target).await?;
        } else {
            self.socket.send(&wire_bytes).await?;
        }

        Ok(seq)
    }
}

/// Receiver for low-latency remote input events over UDP.
pub struct InputChannelReceiver {
    socket: Arc<UdpSocket>,
    remote_addr: Option<SocketAddr>,
    highest_sequence_seen: u64,
    drop_stale: bool,
    seen_key_ids: HashSet<u64>,
    max_seen_history: usize,
    /// When the current peer last sent a valid packet (or when the peer was set).
    last_peer_packet: Instant,
    /// A different sender may take over once the current peer has been silent this long.
    relatch_after: Duration,
    /// Gets the payload of a `Ping` packet sent on this channel (clock sync, #102).
    ping_handler: Option<PingHandler>,
}

/// Callback for `Ping` packets received on the input channel.
pub type PingHandler = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Default silence after which the input peer may be replaced by a new sender.
/// Matches the host's link liveness window.
pub const DEFAULT_RELATCH_AFTER: Duration = Duration::from_secs(5);

impl InputChannelReceiver {
    pub fn new(socket: UdpSocket, remote_addr: Option<SocketAddr>, drop_stale: bool) -> Self {
        Self {
            socket: Arc::new(socket),
            remote_addr,
            highest_sequence_seen: 0,
            drop_stale,
            seen_key_ids: HashSet::with_capacity(128),
            max_seen_history: 1024,
            last_peer_packet: Instant::now(),
            relatch_after: DEFAULT_RELATCH_AFTER,
            ping_handler: None,
        }
    }

    /// Hands `Ping` packets (a serialized `Packet`, not an input batch) from the current peer, or
    /// from anyone before a peer is latched, to `handler`. Without a handler they are dropped as
    /// undecodable input.
    pub fn with_ping_handler(mut self, handler: PingHandler) -> Self {
        self.ping_handler = Some(handler);
        self
    }

    /// Like [`Self::with_ping_handler`] on an existing receiver.
    pub fn set_ping_handler(&mut self, handler: PingHandler) {
        self.ping_handler = Some(handler);
    }

    /// Overrides how long the current peer must be silent before a new sender can replace it.
    pub fn with_relatch_after(mut self, relatch_after: Duration) -> Self {
        self.relatch_after = relatch_after;
        self
    }

    pub async fn bind(
        bind_addr: &str,
        remote_addr: Option<SocketAddr>,
        drop_stale: bool,
    ) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(Self::new(socket, remote_addr, drop_stale))
    }

    pub fn set_remote_addr(&mut self, remote_addr: SocketAddr) {
        self.remote_addr = Some(remote_addr);
        self.last_peer_packet = Instant::now();
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.socket.local_addr()?)
    }

    pub fn highest_sequence_seen(&self) -> u64 {
        self.highest_sequence_seen
    }

    /// Receive the next batch of input events from the socket.
    ///
    /// Deduplicates redundant keys and drops stale mouse movements.
    pub async fn recv_events(&mut self) -> Result<Vec<InputEvent>, TransportError> {
        let mut buf = [0u8; 2048];
        loop {
            let (len, sender_addr) = self.socket.recv_from(&mut buf).await?;

            if let Some(handler) = &self.ping_handler {
                if let Ok(packet) = linux_quest_protocol::Packet::from_bytes(&buf[..len]) {
                    if packet.header.packet_type == linux_quest_protocol::PacketType::Ping
                        && self.remote_addr.is_none_or(|peer| peer == sender_addr)
                    {
                        handler(&packet.payload);
                    }
                    continue;
                }
            }

            let mut relatch = false;
            if let Some(expected) = self.remote_addr {
                if sender_addr != expected {
                    // Only a peer that has gone quiet can be replaced, and only by a
                    // datagram that decodes as a valid input packet.
                    if self.last_peer_packet.elapsed() < self.relatch_after
                        || InputBatch::decode(&buf[..len]).is_err()
                    {
                        continue;
                    }
                    relatch = true;
                }
            }

            let batch = InputBatch::decode(&buf[..len])?;
            if relatch || self.remote_addr.is_none() {
                self.remote_addr = Some(sender_addr);
            }
            if relatch {
                // New sender: its sequence and key ids restart.
                self.highest_sequence_seen = 0;
                self.seen_key_ids.clear();
            }
            self.last_peer_packet = Instant::now();
            let is_stale = batch.sequence <= self.highest_sequence_seen;

            let mut out_events = Vec::new();

            // 1. Recover any missed keys from the redundant key history
            for rk in batch.redundant_keys {
                if self.seen_key_ids.insert(rk.key_id) {
                    out_events.push(rk.event);
                }
            }

            // 2. Process events in the batch
            if is_stale && self.drop_stale {
                // For stale packets, discard mouse motion/scroll, but recover any missed keys
                let mut current_key_id = batch.first_key_id;
                for ev in batch.events {
                    if matches!(ev, InputEvent::KeyboardKey { .. }) {
                        if self.seen_key_ids.insert(current_key_id) {
                            out_events.push(ev);
                        }
                        current_key_id += 1;
                    }
                }
            } else {
                // Not stale: advance sequence cursor
                self.highest_sequence_seen = batch.sequence;
                let mut current_key_id = batch.first_key_id;
                for ev in batch.events {
                    if matches!(ev, InputEvent::KeyboardKey { .. }) {
                        if self.seen_key_ids.insert(current_key_id) {
                            out_events.push(ev);
                        }
                        current_key_id += 1;
                    } else {
                        out_events.push(ev);
                    }
                }
            }

            // Prune seen_key_ids history if it grows too large
            if self.seen_key_ids.len() > self.max_seen_history {
                let min_id = self.highest_sequence_seen.saturating_sub(500);
                self.seen_key_ids.retain(|&id| id >= min_id);
            }

            // If stale packet had no new keys, wait for next packet
            if out_events.is_empty() && is_stale {
                continue;
            }

            return Ok(out_events);
        }
    }
}

/// Create an interconnected pair of UDP input channel sender and receiver on localhost for testing.
pub async fn create_input_channel_pair(
    redundancy_count: usize,
) -> Result<(InputChannelSender, InputChannelReceiver), TransportError> {
    let s1 = UdpSocket::bind("127.0.0.1:0").await?;
    let s2 = UdpSocket::bind("127.0.0.1:0").await?;

    let addr1 = s1.local_addr()?;
    let addr2 = s2.local_addr()?;

    let sender = InputChannelSender::new(s1, Some(addr2), redundancy_count);
    let receiver = InputChannelReceiver::new(s2, Some(addr1), true);

    Ok((sender, receiver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{ElementState, KeyModifiers, MouseButton};

    #[tokio::test]
    async fn test_input_channel_loopback_roundtrip() {
        let (mut sender, mut receiver) = create_input_channel_pair(4).await.unwrap();

        let events = vec![
            InputEvent::MouseMoveRelative { dx: 15, dy: -25 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
            },
            InputEvent::KeyboardKey {
                keycode: 30, // KEY_A
                state: ElementState::Pressed,
                modifiers: KeyModifiers::SHIFT,
            },
        ];

        let seq = sender
            .send_events(events.clone(), 100_000)
            .await
            .expect("Send failed");
        assert_eq!(seq, 1);

        let received = receiver.recv_events().await.expect("Recv failed");
        assert_eq!(received.len(), 3);
        assert_eq!(received, events);
        assert_eq!(receiver.highest_sequence_seen(), 1);
    }

    async fn sender_to(target: SocketAddr) -> (InputChannelSender, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = s.local_addr().unwrap();
        (InputChannelSender::new(s, Some(target), 4), addr)
    }

    fn mv(dx: i32) -> Vec<InputEvent> {
        vec![InputEvent::MouseMoveRelative { dx, dy: 0 }]
    }

    #[tokio::test]
    async fn test_relatches_to_new_port_after_peer_silence() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr = rx_sock.local_addr().unwrap();
        let mut receiver = InputChannelReceiver::new(rx_sock, None, true)
            .with_relatch_after(Duration::from_millis(50));

        let (mut old, old_addr) = sender_to(rx_addr).await;
        old.send_events(mv(1), 1).await.unwrap();
        receiver.recv_events().await.unwrap();
        assert_eq!(receiver.remote_addr(), Some(old_addr));

        // Reconnect from a new port (sequence restarts at 1) after the silence window.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (mut new, new_addr) = sender_to(rx_addr).await;
        new.send_events(mv(7), 2).await.unwrap();
        let ev = receiver.recv_events().await.unwrap();
        assert_eq!(ev, mv(7));
        assert_eq!(receiver.remote_addr(), Some(new_addr));
    }

    #[tokio::test]
    async fn test_active_peer_is_not_displaced_by_other_sender() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr = rx_sock.local_addr().unwrap();
        let mut receiver = InputChannelReceiver::new(rx_sock, None, true);

        let (mut a, a_addr) = sender_to(rx_addr).await;
        a.send_events(mv(1), 1).await.unwrap();
        receiver.recv_events().await.unwrap();

        let (mut b, _) = sender_to(rx_addr).await;
        b.send_events(mv(9), 2).await.unwrap();
        a.send_events(mv(2), 3).await.unwrap();
        let ev = receiver.recv_events().await.unwrap();
        assert_eq!(ev, mv(2));
        assert_eq!(receiver.remote_addr(), Some(a_addr));
    }

    #[tokio::test]
    async fn test_garbage_datagram_does_not_pin_peer() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr = rx_sock.local_addr().unwrap();
        let mut receiver = InputChannelReceiver::new(rx_sock, None, true)
            .with_relatch_after(Duration::from_millis(50));

        // Stray garbage while unlatched: error, and the peer stays unset.
        let stray = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        stray.send_to(&[0xFF; 16], rx_addr).await.unwrap();
        assert!(receiver.recv_events().await.is_err());
        assert_eq!(receiver.remote_addr(), None);

        // The real client still latches afterwards.
        let (mut real, real_addr) = sender_to(rx_addr).await;
        real.send_events(mv(1), 1).await.unwrap();
        receiver.recv_events().await.unwrap();
        assert_eq!(receiver.remote_addr(), Some(real_addr));

        // Garbage from another source after the silence window does not displace the peer.
        tokio::time::sleep(Duration::from_millis(80)).await;
        stray.send_to(&[0xFF; 16], rx_addr).await.unwrap();
        real.send_events(mv(2), 2).await.unwrap();
        let ev = receiver.recv_events().await.unwrap();
        assert_eq!(ev, mv(2));
        assert_eq!(receiver.remote_addr(), Some(real_addr));
    }

    #[tokio::test]
    async fn test_drop_stale_by_sequence() {
        let (mut sender, mut receiver) = create_input_channel_pair(4).await.unwrap();

        // Send packet 1
        sender
            .send_events(vec![InputEvent::MouseMoveRelative { dx: 10, dy: 10 }], 100)
            .await
            .unwrap();
        // Send packet 2
        sender
            .send_events(vec![InputEvent::MouseMoveRelative { dx: 20, dy: 20 }], 200)
            .await
            .unwrap();

        // Receive packet 1 and 2 in order
        let ev1 = receiver.recv_events().await.unwrap();
        assert_eq!(ev1[0], InputEvent::MouseMoveRelative { dx: 10, dy: 10 });

        let ev2 = receiver.recv_events().await.unwrap();
        assert_eq!(ev2[0], InputEvent::MouseMoveRelative { dx: 20, dy: 20 });
        assert_eq!(receiver.highest_sequence_seen(), 2);
    }

    #[tokio::test]
    async fn test_redundant_key_recovery_on_packet_loss() {
        let (mut sender, mut receiver) = create_input_channel_pair(4).await.unwrap();

        // Packet 1: User presses Key A
        let key_a = InputEvent::KeyboardKey {
            keycode: 30,
            state: ElementState::Pressed,
            modifiers: KeyModifiers::empty(),
        };
        sender.send_events(vec![key_a.clone()], 1000).await.unwrap();

        // Packet 2: User presses Key B (SIMULATE DROPPED ON NETWORK)
        let key_b = InputEvent::KeyboardKey {
            keycode: 48, // KEY_B
            state: ElementState::Pressed,
            modifiers: KeyModifiers::empty(),
        };
        // We bypass receiver for packet 2 by receiving into a throwaway buffer
        let dummy_rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_save = sender.remote_addr();
        sender.set_remote_addr(dummy_rx.local_addr().unwrap());
        sender.send_events(vec![key_b.clone()], 2000).await.unwrap();

        // Receive packet 1 on the actual receiver
        let ev1 = receiver.recv_events().await.unwrap();
        assert_eq!(ev1.len(), 1);
        assert_eq!(ev1[0], key_a);

        // Packet 3: User moves mouse (sent to actual receiver, carries redundant Key B)
        sender.set_remote_addr(target_save.unwrap());
        let mouse_ev = InputEvent::MouseMoveRelative { dx: 5, dy: 5 };
        sender
            .send_events(vec![mouse_ev.clone()], 3000)
            .await
            .unwrap();

        // Receiver gets packet 3: it should recover Key B from redundant history!
        let ev3 = receiver.recv_events().await.unwrap();
        assert_eq!(ev3.len(), 2);
        assert_eq!(ev3[0], key_b); // Recovered Key B prepended
        assert_eq!(ev3[1], mouse_ev); // Mouse move
    }

    #[tokio::test]
    async fn ping_packets_reach_the_handler_and_input_still_flows() {
        use linux_quest_protocol::{Packet, PacketHeader, PacketType};
        use std::sync::Mutex;
        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr = rx_sock.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = seen.clone();
        let mut receiver = InputChannelReceiver::new(rx_sock, None, true).with_ping_handler(
            Arc::new(move |p: &[u8]| sink.lock().unwrap().push(p.to_vec())),
        );

        let (mut sender, sender_addr) = sender_to(rx_addr).await;
        let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = bytes::Bytes::from_static(b"LQCK-ish");
        let ping = Packet::new(
            PacketHeader::new(PacketType::Ping, 0, 0, 1, &payload),
            payload,
        );
        raw.send_to(&ping.to_bytes(), rx_addr).await.unwrap();
        sender.send_events(mv(3), 1).await.unwrap();

        // The ping is consumed; the next input batch comes through.
        assert_eq!(receiver.recv_events().await.unwrap(), mv(3));
        assert_eq!(seen.lock().unwrap().as_slice(), &[b"LQCK-ish".to_vec()]);
        assert_eq!(receiver.remote_addr(), Some(sender_addr));

        // Once latched, pings from another address are ignored.
        raw.send_to(&ping.to_bytes(), rx_addr).await.unwrap();
        sender.send_events(mv(4), 2).await.unwrap();
        assert_eq!(receiver.recv_events().await.unwrap(), mv(4));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
