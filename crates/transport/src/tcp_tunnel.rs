//! Negotiated, multiplexed protocol for the USB ADB reverse TCP tunnel.
//!
//! The client starts every connection with [`USB_TUNNEL_HELLO`]. The daemon replies with
//! [`USB_TUNNEL_ACCEPT`]. Only after this exchange may either side send a typed frame. Each
//! frame is a four-byte big-endian body length followed by a bincode encoded [`TunnelFrame`].
//! Keeping packets and control messages in separate variants prevents their independent wire
//! formats from being confused on the same TCP stream.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::control::{ControlMessage, MAX_CONTROL_MESSAGE_SIZE};

pub const USB_TUNNEL_HELLO: &[u8; 8] = b"LQUSB\0\x01\n";
pub const USB_TUNNEL_ACCEPT: &[u8; 8] = b"LQOK\0\x01\n\0";
pub const MAX_TUNNEL_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelFrame {
    /// Exact protocol packet bytes, including the validated 32-byte packet header.
    Packet(Vec<u8>),
    Control(ControlMessage),
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("USB tunnel I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("USB tunnel negotiation failed: {0}")]
    Negotiation(&'static str),
    #[error("USB tunnel frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("USB tunnel frame serialization failed: {0}")]
    Serialization(String),
}

/// Performs the client half of protocol negotiation.
pub async fn negotiate_client<R, W>(reader: &mut R, writer: &mut W) -> Result<(), TunnelError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer.write_all(USB_TUNNEL_HELLO).await?;
    writer.flush().await?;
    let mut response = [0; USB_TUNNEL_ACCEPT.len()];
    reader.read_exact(&mut response).await?;
    if &response != USB_TUNNEL_ACCEPT {
        return Err(TunnelError::Negotiation(
            "daemon rejected or misversioned tunnel",
        ));
    }
    Ok(())
}

/// Performs the daemon half of protocol negotiation.
pub async fn negotiate_server<R, W>(reader: &mut R, writer: &mut W) -> Result<(), TunnelError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hello = [0; USB_TUNNEL_HELLO.len()];
    reader.read_exact(&mut hello).await?;
    if &hello != USB_TUNNEL_HELLO {
        return Err(TunnelError::Negotiation(
            "unsupported client tunnel protocol",
        ));
    }
    writer.write_all(USB_TUNNEL_ACCEPT).await?;
    writer.flush().await?;
    Ok(())
}

/// Writes one typed tunnel frame.
pub async fn send_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &TunnelFrame,
) -> Result<(), TunnelError> {
    let body = bincode::serialize(frame).map_err(|e| TunnelError::Serialization(e.to_string()))?;
    let max = MAX_TUNNEL_FRAME_SIZE.max(MAX_CONTROL_MESSAGE_SIZE);
    if body.len() > max {
        return Err(TunnelError::FrameTooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| TunnelError::FrameTooLarge(body.len()))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one typed tunnel frame.
pub async fn recv_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<TunnelFrame, TunnelError> {
    let mut len = [0; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    let max = MAX_TUNNEL_FRAME_SIZE.max(MAX_CONTROL_MESSAGE_SIZE);
    if len > max {
        return Err(TunnelError::FrameTooLarge(len));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    bincode::deserialize(&body).map_err(|e| TunnelError::Serialization(e.to_string()))
}
