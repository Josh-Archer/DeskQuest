use linux_quest_protocol::{ClientHandshake, ClipboardMessage, DisplayInfo, ServerHandshake};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl<R, W> ControlChannel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    pub async fn send_message(&mut self, msg: &ControlMessage) -> Result<(), ControlError> {
        let serialized =
            bincode::serialize(msg).map_err(|e| ControlError::Serialization(e.to_string()))?;

        // Single buffer + single write: two small writes on a TCP socket trigger the
        // Nagle / delayed-ACK interaction (~40+ ms stalls per round trip).
        let mut frame = Vec::with_capacity(4 + serialized.len());
        frame.extend_from_slice(&(serialized.len() as u32).to_be_bytes());
        frame.extend_from_slice(&serialized);
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn recv_message(&mut self) -> Result<ControlMessage, ControlError> {
        let mut len_buf = [0u8; 4];
        self.reader
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                    ControlError::ConnectionClosed
                }
                _ => ControlError::Io(e),
            })?;

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
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                    ControlError::ConnectionClosed
                }
                _ => ControlError::Io(e),
            })?;

        let msg: ControlMessage = bincode::deserialize(&msg_buf)
            .map_err(|e| ControlError::Deserialization(e.to_string()))?;

        Ok(msg)
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
