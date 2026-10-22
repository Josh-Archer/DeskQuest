//! Negotiated, multiplexed protocol for the USB ADB reverse TCP tunnel.
//!
//! The client starts every connection with [`USB_TUNNEL_HELLO`]. The daemon replies with
//! [`USB_TUNNEL_ACCEPT`]. Only after this exchange may either side send a typed frame. Each
//! frame is a four-byte big-endian body length followed by a bincode encoded [`TunnelFrame`].
//! Keeping packets and control messages in separate variants prevents their independent wire
//! formats from being confused on the same TCP stream.
//!
//! Liveness: `adb reverse` can leave a connection open on both ends while it carries nothing
//! (seen after the headset sleeps and wakes). The client sends an input keepalive every 100 ms
//! and the daemon a [`ControlMessage::Heartbeat`] every [`TUNNEL_HEARTBEAT_INTERVAL`], so either
//! end can take [`TUNNEL_IDLE_TIMEOUT`] of silence as a dead tunnel, close it and start over.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::control::{ControlMessage, MAX_CONTROL_MESSAGE_SIZE};

pub const USB_TUNNEL_HELLO: &[u8; 8] = b"LQUSB\0\x01\n";
pub const USB_TUNNEL_ACCEPT: &[u8; 8] = b"LQOK\0\x01\n\0";
pub const MAX_TUNNEL_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// A peer must complete the protocol greeting promptly after connecting.
pub const TUNNEL_NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(2);
/// The daemon sends a heartbeat at least this often, so the tunnel carries data even when the
/// screen is static and no video is sent.
pub const TUNNEL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Silence after which either end takes the tunnel for dead. A write that cannot finish within
/// this time also ends the connection.
pub const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest frame body either end accepts.
const MAX_FRAME_BODY: usize = if MAX_TUNNEL_FRAME_SIZE > MAX_CONTROL_MESSAGE_SIZE {
    MAX_TUNNEL_FRAME_SIZE
} else {
    MAX_CONTROL_MESSAGE_SIZE
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl TunnelError {
    /// True when the stream is still in step after this error, so a reader can skip the frame
    /// and go on (a whole frame arrived but did not decode, for example a message from a newer
    /// peer). Any other error leaves the connection unusable.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Serialization(_))
    }
}

/// Performs the client half of protocol negotiation.
pub async fn negotiate_client<R, W>(reader: &mut R, writer: &mut W) -> Result<(), TunnelError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::time::timeout(TUNNEL_NEGOTIATION_TIMEOUT, async {
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
    })
    .await
    .map_err(|_| TunnelError::Negotiation("client negotiation timed out"))?
}

/// Performs the daemon half of protocol negotiation.
pub async fn negotiate_server<R, W>(reader: &mut R, writer: &mut W) -> Result<(), TunnelError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::time::timeout(TUNNEL_NEGOTIATION_TIMEOUT, async {
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
    })
    .await
    .map_err(|_| TunnelError::Negotiation("server negotiation timed out"))?
}

/// Writes one typed tunnel frame.
pub async fn send_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &TunnelFrame,
) -> Result<(), TunnelError> {
    let wire = encode_frame(frame)?;
    writer.write_all(&wire).await?;
    writer.flush().await?;
    Ok(())
}

/// Encodes one typed tunnel frame with its length prefix, for blocking writers.
pub fn encode_frame(frame: &TunnelFrame) -> Result<Vec<u8>, TunnelError> {
    let body = bincode::serialize(frame).map_err(|e| TunnelError::Serialization(e.to_string()))?;
    if body.len() > MAX_FRAME_BODY {
        return Err(TunnelError::FrameTooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| TunnelError::FrameTooLarge(body.len()))?;
    let mut wire = Vec::with_capacity(4 + body.len());
    wire.extend_from_slice(&len.to_be_bytes());
    wire.extend_from_slice(&body);
    Ok(wire)
}

/// Reads one typed tunnel frame.
pub async fn recv_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<TunnelFrame, TunnelError> {
    let mut len = [0; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_BODY {
        return Err(TunnelError::FrameTooLarge(len));
    }
    // Reject untrusted lengths before allocating the body buffer.
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    bincode::deserialize(&body).map_err(|e| TunnelError::Serialization(e.to_string()))
}

/// Performs the client half of protocol negotiation on a blocking stream. The caller sets the
/// stream's read and write timeouts (see [`TUNNEL_NEGOTIATION_TIMEOUT`]).
pub fn negotiate_client_blocking<S>(stream: &mut S) -> Result<(), TunnelError>
where
    S: std::io::Read + std::io::Write,
{
    stream.write_all(USB_TUNNEL_HELLO)?;
    stream.flush()?;
    let mut response = [0; USB_TUNNEL_ACCEPT.len()];
    stream.read_exact(&mut response)?;
    if &response != USB_TUNNEL_ACCEPT {
        return Err(TunnelError::Negotiation(
            "daemon rejected or misversioned tunnel",
        ));
    }
    Ok(())
}

/// Splits a byte stream into tunnel frames, for blocking readers: [`push`](Self::push) bytes
/// as they arrive and take whole frames with [`next_frame`](Self::next_frame).
#[derive(Debug, Default)]
pub struct TunnelFrameDecoder {
    buf: Vec<u8>,
    start: usize,
}

impl TunnelFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, or `None` until more bytes arrive.
    ///
    /// A [`TunnelError::FrameTooLarge`] means the stream is out of step: drop the connection.
    /// A [`recoverable`](TunnelError::is_recoverable) error skipped one frame that did not
    /// decode; the next call goes on with the frame after it.
    pub fn next_frame(&mut self) -> Option<Result<TunnelFrame, TunnelError>> {
        let pending = &self.buf[self.start..];
        let len = u32::from_be_bytes(pending.get(..4)?.try_into().ok()?) as usize;
        if len > MAX_FRAME_BODY {
            return Some(Err(TunnelError::FrameTooLarge(len)));
        }
        let body = pending.get(4..4 + len)?;
        let frame =
            bincode::deserialize(body).map_err(|e| TunnelError::Serialization(e.to_string()));
        self.start += 4 + len;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        Some(frame)
    }

    /// Bytes received that are not yet part of a whole frame.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn decoder_reassembles_frames_split_across_reads() {
        let heartbeat = TunnelFrame::Control(ControlMessage::Heartbeat {
            sequence: 9,
            timestamp_us: 77,
        });
        let packet = TunnelFrame::Packet(vec![1, 2, 3, 4, 5]);
        let mut wire = encode_frame(&heartbeat).unwrap();
        wire.extend(encode_frame(&packet).unwrap());

        let mut decoder = TunnelFrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &wire {
            decoder.push(std::slice::from_ref(byte));
            while let Some(frame) = decoder.next_frame() {
                frames.push(frame.unwrap());
            }
        }
        assert_eq!(frames, vec![heartbeat, packet]);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn decoder_skips_an_undecodable_frame_and_stays_in_step() {
        let mut wire = 3u32.to_be_bytes().to_vec();
        wire.extend([0xff, 0xff, 0xff]); // no such TunnelFrame variant
        let packet = TunnelFrame::Packet(vec![7]);
        wire.extend(encode_frame(&packet).unwrap());

        let mut decoder = TunnelFrameDecoder::new();
        decoder.push(&wire);
        let skipped = decoder.next_frame().unwrap().unwrap_err();
        assert!(skipped.is_recoverable());
        assert_eq!(decoder.next_frame().unwrap().unwrap(), packet);
        assert!(decoder.next_frame().is_none());
    }

    #[test]
    fn decoder_reports_an_oversized_length_as_fatal() {
        let mut decoder = TunnelFrameDecoder::new();
        decoder.push(&((MAX_FRAME_BODY as u32) + 1).to_be_bytes());
        let error = decoder.next_frame().unwrap().unwrap_err();
        assert!(matches!(error, TunnelError::FrameTooLarge(_)));
        assert!(!error.is_recoverable());
    }

    #[test]
    fn blocking_negotiation_matches_the_async_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            runtime.block_on(async {
                let stream = tokio::net::TcpStream::from_std(stream).unwrap();
                let (mut reader, mut writer) = stream.into_split();
                negotiate_server(&mut reader, &mut writer).await
            })
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(TUNNEL_NEGOTIATION_TIMEOUT))
            .unwrap();
        negotiate_client_blocking(&mut client).unwrap();
        server.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn recv_frame_rejects_oversized_length_before_body_arrives() {
        let (mut writer, mut reader) = duplex(8);
        writer
            .write_all(&((MAX_TUNNEL_FRAME_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(
            matches!(recv_frame(&mut reader).await, Err(TunnelError::FrameTooLarge(n)) if n == MAX_TUNNEL_FRAME_SIZE + 1)
        );
    }
}
