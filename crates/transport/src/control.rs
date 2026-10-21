use linux_quest_protocol::{
    ClientHandshake, ClipboardMessage, DisplayInfo, HeadsetSettings, HeadsetStatus,
    ScreenPlacement, ServerHandshake,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Error, Debug)]
pub enum ControlError {
    #[error("Connection closed")]
    ConnectionClosed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    /// A whole frame arrived but did not decode, typically a message added in a later version.
    /// The stream is still in step, so a reader can skip it and go on.
    #[error("Unknown control message ({0}); skipped")]
    UnknownMessage(String),
}

impl ControlError {
    /// True when the connection can still be read after this error.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::UnknownMessage(_))
    }
}

/// Messages on the control channel. New variants go at the end: bincode encodes the variant
/// by its index, so reordering breaks every peer of another version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    ClientHello(ClientHandshake),
    ServerHello(ServerHandshake),
    DisplayConfigUpdate(DisplayInfo),
    BitrateAdjustment {
        target_mbps: u32,
        min_mbps: u32,
        max_mbps: u32,
    },
    ReferencePictureInvalidation {
        pts_us: u64,
    },
    Heartbeat {
        sequence: u64,
        timestamp_us: u64,
    },
    HeartbeatAck {
        sequence: u64,
        timestamp_us: u64,
    },
    Disconnect {
        reason: String,
    },
    Clipboard(ClipboardMessage),
    /// Host to headset: report a [`HeadsetStatus`]. Sent when a control connection opens; a
    /// headset sends the messages below only after receiving it, so an older host never sees
    /// them. See `linux_quest_protocol::headset`.
    HeadsetStatusRequest,
    /// Headset to host: its current status, layout and settings.
    HeadsetStatus(Box<HeadsetStatus>),
    /// Host to headset: place these screens (screens not listed keep their place).
    SetHeadsetLayout(Vec<ScreenPlacement>),
    /// Host to headset: apply these settings.
    SetHeadsetSettings(HeadsetSettings),
}

/// Default TCP port for the out-of-band reliable signaling and control channel.
pub const DEFAULT_CONTROL_PORT: u16 = 48444;

/// Maximum allowed size for a control signaling message (1MB).
pub const MAX_CONTROL_MESSAGE_SIZE: usize = 1024 * 1024;

/// Out-of-band reliable signaling and control channel.
/// Manages connection negotiation, display updates, adaptive bitrate feedback, and heartbeats.
pub struct ControlChannel<R, W> {
    reader: R,
    writer: W,
}

/// Maps an I/O error that means "the peer or the connection is gone" to `ConnectionClosed`.
///
/// QUIC streams report a lost connection as `NotConnected` (see quinn's `ReadError`/`WriteError`
/// conversions), so it is treated the same as a reset or an EOF.
fn map_io_error(e: std::io::Error) -> ControlError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::UnexpectedEof
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe
        | ErrorKind::NotConnected => ControlError::ConnectionClosed,
        _ => ControlError::Io(e),
    }
}

impl<R, W> ControlChannel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Consumes the channel and returns its reader and writer, e.g. to split them across tasks.
    pub fn into_parts(self) -> (R, W) {
        (self.reader, self.writer)
    }

    pub async fn send_message(&mut self, msg: &ControlMessage) -> Result<(), ControlError> {
        // Single buffer + single write: two small writes on a TCP socket trigger the
        // Nagle / delayed-ACK interaction (~40+ ms stalls per round trip).
        let frame = encode_control_frame(msg)?;
        self.writer.write_all(&frame).await.map_err(map_io_error)?;
        self.writer.flush().await.map_err(map_io_error)?;
        Ok(())
    }

    pub async fn recv_message(&mut self) -> Result<ControlMessage, ControlError> {
        let mut len_buf = [0u8; 4];
        self.reader
            .read_exact(&mut len_buf)
            .await
            .map_err(map_io_error)?;

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_CONTROL_MESSAGE_SIZE {
            return Err(ControlError::Deserialization(format!(
                "Control message size {len} exceeds limit {MAX_CONTROL_MESSAGE_SIZE}"
            )));
        }
        let mut msg_buf = vec![0u8; len];
        self.reader
            .read_exact(&mut msg_buf)
            .await
            .map_err(map_io_error)?;

        decode_control_message(&msg_buf)
    }
}

/// Serializes `msg` as one length-prefixed frame (the wire format of [`ControlChannel`]).
pub fn encode_control_frame(msg: &ControlMessage) -> Result<Vec<u8>, ControlError> {
    let serialized =
        bincode::serialize(msg).map_err(|e| ControlError::Serialization(e.to_string()))?;
    let mut frame = Vec::with_capacity(4 + serialized.len());
    frame.extend_from_slice(&(serialized.len() as u32).to_be_bytes());
    frame.extend_from_slice(&serialized);
    Ok(frame)
}

fn decode_control_message(body: &[u8]) -> Result<ControlMessage, ControlError> {
    bincode::deserialize(body).map_err(|e| ControlError::UnknownMessage(e.to_string()))
}

/// Splits a byte stream into control messages, for blocking sockets read with a timeout:
/// feed whatever `read` returned with [`push`](Self::push), then drain
/// [`next_message`](Self::next_message). Partial frames wait for more bytes.
#[derive(Debug, Default)]
pub struct ControlFrameReader {
    buf: Vec<u8>,
}

impl ControlFrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, `None` when more bytes are needed. A frame larger than
    /// [`MAX_CONTROL_MESSAGE_SIZE`] is an error after which the stream is out of step.
    pub fn next_message(&mut self) -> Option<Result<ControlMessage, ControlError>> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_CONTROL_MESSAGE_SIZE {
            self.buf.clear();
            return Some(Err(ControlError::Deserialization(format!(
                "Control message size {len} exceeds limit {MAX_CONTROL_MESSAGE_SIZE}"
            ))));
        }
        if self.buf.len() < 4 + len {
            return None;
        }
        let result = decode_control_message(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        Some(result)
    }
}

impl ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf> {
    /// Creates an interconnected pair of TCP-backed control channels for testing.
    pub async fn create_connected_pair() -> Result<
        (
            ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
            ControlChannel<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>,
        ),
        ControlError,
    > {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let client_fut = tokio::net::TcpStream::connect(addr);
        let server_fut = listener.accept();

        let (client_res, server_res) = tokio::join!(client_fut, server_fut);
        let client_stream = client_res?;
        let (server_stream, _) = server_res?;
        client_stream.set_nodelay(true)?;
        server_stream.set_nodelay(true)?;

        let (cr, cw) = client_stream.into_split();
        let (sr, sw) = server_stream.into_split();

        Ok((ControlChannel::new(cr, cw), ControlChannel::new(sr, sw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    /// AsyncWrite that records the size of every write call.
    struct RecordingWriter(Arc<Mutex<Vec<usize>>>);

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().push(buf.len());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_send_message_issues_single_write() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut ch = ControlChannel::new(tokio::io::empty(), RecordingWriter(writes.clone()));
        let msg = ControlMessage::Heartbeat {
            sequence: 1,
            timestamp_us: 2,
        };
        ch.send_message(&msg).await.unwrap();
        let body_len = bincode::serialize(&msg).unwrap().len();
        assert_eq!(*writes.lock().unwrap(), vec![4 + body_len]);
    }

    #[tokio::test]
    async fn test_connected_pair_has_nodelay() {
        let (client, server) = ControlChannel::create_connected_pair().await.unwrap();
        for ch in [client, server] {
            assert!(ch.writer.as_ref().nodelay().unwrap());
            assert!(ch.reader.as_ref().nodelay().unwrap());
        }
    }

    #[tokio::test]
    async fn test_control_ping_pong_latency() {
        let (mut client, mut server) = ControlChannel::create_connected_pair().await.unwrap();
        let echo = tokio::spawn(async move {
            while let Ok(m) = server.recv_message().await {
                if server.send_message(&m).await.is_err() {
                    break;
                }
            }
        });
        let msg = ControlMessage::Heartbeat {
            sequence: 7,
            timestamp_us: 9,
        };
        let mut rtts = Vec::new();
        for _ in 0..50 {
            let t = std::time::Instant::now();
            client.send_message(&msg).await.unwrap();
            assert_eq!(client.recv_message().await.unwrap(), msg);
            rtts.push(t.elapsed());
        }
        drop(client);
        let _ = echo.await;
        rtts.sort();
        let median = rtts[rtts.len() / 2];
        // Stalled Nagle/delayed-ACK behaviour measured ~88 ms; healthy loopback is < 1 ms.
        assert!(
            median < std::time::Duration::from_millis(40),
            "median control RTT too high: {median:?}"
        );
    }

    #[tokio::test]
    async fn test_control_channel_roundtrip() {
        let (mut client, mut server) = ControlChannel::create_connected_pair().await.unwrap();

        let msg = ControlMessage::BitrateAdjustment {
            target_mbps: 150,
            min_mbps: 50,
            max_mbps: 200,
        };

        client.send_message(&msg).await.unwrap();
        let received = server.recv_message().await.unwrap();

        assert_eq!(received, msg);
    }

    /// A frame holding a variant index no version knows, as a newer peer would send.
    fn future_variant_frame() -> Vec<u8> {
        let body = 9_999u32.to_le_bytes();
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        frame
    }

    fn headset_status() -> HeadsetStatus {
        use linux_quest_protocol::{HeadsetCodec, HeadsetStats, ScreenCurve};
        HeadsetStatus {
            device: "Quest 3".into(),
            app_version: "test".into(),
            settings: HeadsetSettings {
                bitrate_mbps: 60,
                adaptive_bitrate: false,
                codec: HeadsetCodec::Hevc,
                refresh_rate_hz: 120.0,
                curve: ScreenCurve::Flat,
                hud: true,
                passthrough: false,
            },
            screens: vec![ScreenPlacement::facing_user(
                1,
                -0.45,
                0.0,
                1.25,
                1.4,
                1.6,
                0.9,
                ScreenCurve::Curved { radius_m: 2.5 },
            )],
            showing: vec![1],
            refresh_rates_hz: vec![72.0, 90.0, 120.0],
            stats: HeadsetStats::default(),
        }
    }

    #[tokio::test]
    async fn headset_messages_round_trip() {
        let (mut client, mut server) = ControlChannel::create_connected_pair().await.unwrap();
        let status = headset_status();
        let messages = [
            ControlMessage::HeadsetStatusRequest,
            ControlMessage::HeadsetStatus(Box::new(status.clone())),
            ControlMessage::SetHeadsetLayout(status.screens.clone()),
            ControlMessage::SetHeadsetSettings(status.settings),
        ];
        for msg in &messages {
            client.send_message(msg).await.unwrap();
        }
        for msg in &messages {
            assert_eq!(&server.recv_message().await.unwrap(), msg);
        }
    }

    #[test]
    fn existing_variants_keep_their_wire_index() {
        // Older peers decode by variant index; the new variants must not shift the old ones.
        let index = |m: &ControlMessage| {
            let bytes = bincode::serialize(m).unwrap();
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        };
        let clip = ClipboardMessage::single(1, 1, "x".into()).unwrap();
        assert_eq!(index(&ControlMessage::Clipboard(clip)), 8);
        assert_eq!(index(&ControlMessage::HeadsetStatusRequest), 9);
        assert_eq!(
            index(&ControlMessage::SetHeadsetSettings(
                headset_status().settings
            )),
            12
        );
    }

    #[tokio::test]
    async fn an_unknown_message_is_skipped_and_the_stream_stays_in_step() {
        let (client, mut server) = ControlChannel::create_connected_pair().await.unwrap();
        let (_, mut raw) = client.into_parts();
        raw.write_all(&future_variant_frame()).await.unwrap();
        let mut client = ControlChannel::new(tokio::io::empty(), raw);
        let after = ControlMessage::Heartbeat {
            sequence: 3,
            timestamp_us: 4,
        };
        client.send_message(&after).await.unwrap();

        let err = server.recv_message().await.unwrap_err();
        assert!(err.is_recoverable(), "{err}");
        assert_eq!(server.recv_message().await.unwrap(), after);
        assert!(!ControlError::ConnectionClosed.is_recoverable());
    }

    #[test]
    fn frame_reader_handles_split_frames_unknown_messages_and_oversize() {
        let first = encode_control_frame(&ControlMessage::HeadsetStatusRequest).unwrap();
        let second =
            encode_control_frame(&ControlMessage::SetHeadsetLayout(headset_status().screens))
                .unwrap();
        let mut stream = first.clone();
        stream.extend(future_variant_frame());
        stream.extend(&second);

        let mut reader = ControlFrameReader::new();
        let mut got = Vec::new();
        // One byte at a time, as a slow socket might deliver it.
        for byte in &stream {
            reader.push(std::slice::from_ref(byte));
            while let Some(msg) = reader.next_message() {
                got.push(msg);
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0].as_ref().unwrap(),
            &ControlMessage::HeadsetStatusRequest
        );
        assert!(got[1].as_ref().unwrap_err().is_recoverable());
        assert!(matches!(
            got[2].as_ref().unwrap(),
            ControlMessage::SetHeadsetLayout(s) if s.len() == 1
        ));
        assert!(reader.next_message().is_none());

        reader.push(&((MAX_CONTROL_MESSAGE_SIZE as u32) + 1).to_be_bytes());
        let err = reader.next_message().unwrap().unwrap_err();
        assert!(!err.is_recoverable(), "an oversize frame cannot be skipped");
    }

    #[tokio::test]
    async fn test_control_channel_clipboard_roundtrip() {
        let (mut client, mut server) = ControlChannel::create_connected_pair().await.unwrap();

        let clip_msg = ClipboardMessage::single(1, 42, "Rust on Meta Quest 3".to_string()).unwrap();
        let msg = ControlMessage::Clipboard(clip_msg);

        client.send_message(&msg).await.unwrap();
        let received = server.recv_message().await.unwrap();

        assert_eq!(received, msg);
    }
}
