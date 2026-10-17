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
        let serialized =
            bincode::serialize(msg).map_err(|e| ControlError::Serialization(e.to_string()))?;

        let len = (serialized.len() as u32).to_be_bytes();
        self.writer.write_all(&len).await.map_err(map_io_error)?;
        self.writer
            .write_all(&serialized)
            .await
            .map_err(map_io_error)?;
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

        let (cr, cw) = client_stream.into_split();
        let (sr, sw) = server_stream.into_split();

        Ok((ControlChannel::new(cr, cw), ControlChannel::new(sr, sw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
