//! QUIC transport for the control channel. Requires the `quic` cargo feature.
//!
//! Each control session is one QUIC connection carrying one bidirectional stream. That stream is
//! wrapped in the existing [`ControlChannel`] framing, so clipboard, handshake and telemetry
//! messages use the same `ControlMessage` codec over QUIC as over TCP. QUIC streams are reliable
//! and ordered, so no application-level retransmission is needed.
//!
//! # Trust model (LAN, no CA)
//!
//! - The host generates a self-signed certificate with [`HostIdentity::generate`]. Nothing is
//!   stored on disk, so the identity changes every time the host process restarts.
//! - The client is configured with the host's [`Fingerprint`] (SHA-256 of the DER certificate)
//!   and accepts only a certificate with that exact fingerprint. The certificate's name is not
//!   checked. The client also verifies the TLS 1.3 `CertificateVerify` signature, so a party that
//!   has only seen the public certificate cannot impersonate the host.
//! - The fingerprint reaches the client out of band. The simplest option is to display it on the
//!   host and copy it to the client over the ADB/USB channel or enter it manually. This module
//!   does not implement trust-on-first-use (TOFU) storage or the UI for either side.
//! - The host does not authenticate clients. Any peer that reaches the UDP port can open a control
//!   session, the same as the existing TCP control port.
//!
//! # Security notes
//!
//! There is no client authentication. Pinning only authenticates the host to the client. Anyone on
//! the LAN who can reach the host's UDP port and knows its certificate fingerprint can connect and
//! use clipboard control. The fingerprint is not a secret in the way a password is, because it is
//! displayed and copied around by design. Resource limits (one bidirectional stream, no
//! unidirectional streams, no datagrams, bounded flow-control windows, a 5 s deadline for the first
//! stream, address validation with Retry) limit what an unauthenticated peer can consume, but they
//! do not authenticate it.
//!
//! TODO(security): add client authentication (for example a paired client certificate or a
//! pairing token bound to the TLS session) before QUIC becomes the default control transport.
//! Until then QUIC must stay opt-in behind the `quic` feature.
//!
//! # Connection management
//!
//! - The host's [`QuicControlServer::accept`] only completes the QUIC handshake inline. Waiting for
//!   the client's control stream happens in a per-connection task with a
//!   [`QUIC_FIRST_STREAM_TIMEOUT`] deadline, so a peer that handshakes and then goes quiet cannot
//!   block other clients. Such a connection is closed.
//! - Dropping a [`QuicControlSession`] or a [`QuicControlServer`] closes the connection(s) with an
//!   application error code, best effort. Dropping the server therefore also ends sessions it
//!   accepted.
//! - Keepalive pings are sent every [`QUIC_KEEP_ALIVE_INTERVAL`], and an idle connection is
//!   dropped after [`QUIC_MAX_IDLE_TIMEOUT`]. Both values are tuned for a LAN, so a silent peer
//!   is detected within a few seconds.
//! - Address migration is handled by quinn. [`QuicControlSession::rebind_socket`] moves the
//!   client's UDP socket, and quinn validates the new path and keeps the existing connection and
//!   streams.
//! - Reconnect is explicit. When the control channel returns [`ControlError::ConnectionClosed`],
//!   call [`QuicControlSession::reconnect`] to dial the same address with the same pin again.

use crate::control::{ControlChannel, ControlError};
use quinn::rustls;
use quinn::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use quinn::rustls::crypto::CryptoProvider;
use quinn::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use quinn::rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// ALPN protocol identifier for the control channel.
pub const QUIC_ALPN: &[u8] = b"linux-quest-control/1";

/// Subject alternative name placed in the host certificate. The client sends it as SNI but does
/// not verify it, because pinning replaces name verification.
pub const QUIC_SERVER_NAME: &str = "linux-quest-host";

/// Interval between keepalive PINGs on an otherwise idle connection. Short enough to keep NAT and
/// Wi-Fi paths warm and to notice a dead peer quickly on a LAN.
pub const QUIC_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Time without any packet from the peer before the connection is dropped. Must be longer than
/// [`QUIC_KEEP_ALIVE_INTERVAL`] so that a healthy idle connection is never dropped.
pub const QUIC_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial RTT estimate, used before the first sample. quinn's default is 333 ms, which is far
/// above a LAN round trip and delays early retransmission timers.
pub const QUIC_INITIAL_RTT: Duration = Duration::from_millis(20);

/// Time a connection may take, after its handshake, to open the control stream. A peer that does
/// not open it within this time is disconnected.
pub const QUIC_FIRST_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Application close code for a normal, requested shutdown ([`QuicControlSession::close`],
/// [`QuicControlServer::close`]).
pub const QUIC_CLOSE_SHUTDOWN: u32 = 0;
/// Application close code used when a session or server handle is dropped.
pub const QUIC_CLOSE_DROPPED: u32 = 1;
/// Application close code used when a peer did not open the control stream in time.
pub const QUIC_CLOSE_FIRST_STREAM_TIMEOUT: u32 = 2;

/// Connection-level flow-control window (bytes the peer may have in flight across all streams).
const QUIC_RECEIVE_WINDOW: u32 = 4 * 1024 * 1024;
/// Per-stream flow-control window. The control stream is the only stream.
const QUIC_STREAM_RECEIVE_WINDOW: u32 = 1024 * 1024;
/// Bytes we may have unacknowledged in flight.
const QUIC_SEND_WINDOW: u64 = 4 * 1024 * 1024;
/// Handshakes the server tracks at once before refusing new ones.
const QUIC_MAX_INCOMING: usize = 64;

#[derive(Error, Debug)]
pub enum QuicError {
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),

    #[error("QUIC configuration error: {0}")]
    Config(String),

    #[error("Certificate generation failed: {0}")]
    Certificate(#[from] rcgen::Error),

    #[error("Invalid fingerprint: {0}")]
    InvalidFingerprint(String),

    #[error("QUIC connect failed: {0}")]
    Connect(#[from] quinn::ConnectError),

    #[error("QUIC connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Control channel error: {0}")]
    Control(#[from] ControlError),

    #[error("QUIC endpoint is closed")]
    EndpointClosed,

    #[error("Session was not created by a client connect and cannot be reconnected or rebound")]
    NotDialed,
}

/// SHA-256 fingerprint of a DER-encoded certificate. This is the value the client pins.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Computes the fingerprint of a DER-encoded certificate.
    pub fn of_der(cert_der: &[u8]) -> Self {
        let digest = Sha256::digest(cert_der);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    /// Writes 64 lowercase hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = QuicError;

    /// Parses 64 hex digits. Colons, dashes and whitespace are ignored, and case does not matter.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex: String = s
            .chars()
            .filter(|c| !(c.is_whitespace() || *c == ':' || *c == '-'))
            .collect();
        if hex.len() != 64 {
            return Err(QuicError::InvalidFingerprint(format!(
                "expected 64 hex digits, got {}",
                hex.len()
            )));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .map_err(|e| QuicError::InvalidFingerprint(e.to_string()))?;
        }
        Ok(Self(out))
    }
}

/// Host-side self-signed certificate and key.
pub struct HostIdentity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
}

impl HostIdentity {
    /// Generates a fresh ECDSA P-256 self-signed certificate for this process.
    pub fn generate() -> Result<Self, QuicError> {
        let certified = rcgen::generate_simple_self_signed(vec![QUIC_SERVER_NAME.to_string()])?;
        let cert = certified.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let fingerprint = Fingerprint::of_der(cert.as_ref());
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    /// The fingerprint a client must pin to reach this identity.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }
}

/// The control stream of one QUIC session, using the same framing as the TCP control path.
pub type QuicControl = ControlChannel<quinn::RecvStream, quinn::SendStream>;

/// Host side: accepts QUIC connections that carry control sessions.
pub struct QuicControlServer {
    endpoint: quinn::Endpoint,
    /// Sessions (or handshake errors) produced by the per-connection tasks, waiting for `accept`.
    ready_tx: tokio::sync::mpsc::UnboundedSender<Result<QuicControlSession, QuicError>>,
    ready_rx: tokio::sync::Mutex<
        tokio::sync::mpsc::UnboundedReceiver<Result<QuicControlSession, QuicError>>,
    >,
}

impl QuicControlServer {
    /// Binds a UDP socket on `addr` that serves `identity`.
    pub fn bind(addr: SocketAddr, identity: &HostIdentity) -> Result<Self, QuicError> {
        let config = server_config(identity)?;
        let endpoint = quinn::Endpoint::server(config, addr)?;
        let (ready_tx, ready_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            endpoint,
            ready_tx,
            ready_rx: tokio::sync::Mutex::new(ready_rx),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, QuicError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for a client to connect and open its control stream.
    ///
    /// quinn only hands out an incoming connection when this method is polled, so a client's
    /// `connect` completes only while `accept` is running. Run this in a loop or concurrently
    /// with the dialing side. The stream becomes visible only once the client writes its first
    /// control message (ClientHello).
    ///
    /// Each incoming connection is driven by its own task (handshake, then waiting up to
    /// [`QUIC_FIRST_STREAM_TIMEOUT`] for the control stream), so one slow or malicious peer never
    /// delays another. Connections that miss the deadline are closed and never returned. A failed
    /// handshake is returned as an error; call `accept` again to keep serving.
    ///
    /// Cancel-safe: dropping the future loses nothing. Connections that finish while no `accept`
    /// is running are queued and returned by the next call. Concurrent calls are serialized.
    pub async fn accept(&self) -> Result<QuicControlSession, QuicError> {
        let mut ready = self.ready_rx.lock().await;
        loop {
            tokio::select! {
                biased;
                Some(result) = ready.recv() => return result,
                incoming = self.endpoint.accept() => {
                    let incoming = incoming.ok_or(QuicError::EndpointClosed)?;
                    // Make unvalidated sources prove they own their address with a Retry first,
                    // so spoofed packets cannot make us allocate handshake state.
                    let incoming = if incoming.remote_address_validated() {
                        incoming
                    } else {
                        // Retry only fails for an already validated address, excluded here.
                        let _ = incoming.retry();
                        continue;
                    };
                    let tx = self.ready_tx.clone();
                    tokio::spawn(async move {
                        if let Some(result) = establish(incoming).await {
                            // The receiver is gone only if the server was dropped.
                            let _ = tx.send(result);
                        }
                    });
                }
            }
        }
    }

    /// Closes the endpoint and all of its connections.
    pub fn close(&self) {
        self.endpoint
            .close(QUIC_CLOSE_SHUTDOWN.into(), b"server shutdown");
    }
}

impl Drop for QuicControlServer {
    fn drop(&mut self) {
        // Best effort and never panics: `close` only queues CONNECTION_CLOSE frames.
        self.endpoint
            .close(QUIC_CLOSE_DROPPED.into(), b"server dropped");
    }
}

/// Completes the handshake of `incoming` and waits for its control stream. Returns `None` when
/// the peer is dropped for not opening a stream in time.
async fn establish(incoming: quinn::Incoming) -> Option<Result<QuicControlSession, QuicError>> {
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(err) => return Some(Err(err.into())),
    };
    match tokio::time::timeout(QUIC_FIRST_STREAM_TIMEOUT, connection.accept_bi()).await {
        Ok(Ok((send, recv))) => Some(Ok(QuicControlSession {
            connection,
            control: ControlChannel::new(recv, send),
            dial: None,
        })),
        Ok(Err(err)) => Some(Err(err.into())),
        Err(_elapsed) => {
            tracing::warn!(
                remote = %connection.remote_address(),
                "closing QUIC connection that did not open the control stream in time"
            );
            connection.close(QUIC_CLOSE_FIRST_STREAM_TIMEOUT.into(), b"no control stream");
            None
        }
    }
}

/// Client state needed to reconnect or rebind. Only sessions created by [`QuicControlSession::connect`]
/// have it.
struct Dial {
    endpoint: quinn::Endpoint,
    remote: SocketAddr,
    fingerprint: Fingerprint,
}

/// One connected control session: a QUIC connection plus its control stream.
pub struct QuicControlSession {
    connection: quinn::Connection,
    /// The control stream. Use it like any other [`ControlChannel`].
    pub control: QuicControl,
    dial: Option<Dial>,
}

impl QuicControlSession {
    /// Dials `remote` and pins `expected`. The connection is accepted only if the host presents
    /// a certificate with that fingerprint.
    pub async fn connect(remote: SocketAddr, expected: Fingerprint) -> Result<Self, QuicError> {
        let bind_addr: SocketAddr = if remote.is_ipv4() {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let mut endpoint = quinn::Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(client_config(expected)?);

        let connection = endpoint.connect(remote, QUIC_SERVER_NAME)?.await?;
        let (send, recv) = connection.open_bi().await?;
        Ok(Self {
            connection,
            control: ControlChannel::new(recv, send),
            dial: Some(Dial {
                endpoint,
                remote,
                fingerprint: expected,
            }),
        })
    }

    /// Current remote address. It changes after a successful [`Self::rebind_socket`] or a
    /// migration by the peer.
    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    pub fn connection(&self) -> &quinn::Connection {
        &self.connection
    }

    /// Resolves when the connection is closed or lost. Use it to detect a dropped control
    /// session without waiting for the next message.
    pub async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    /// Replaces this session with a new connection to the same remote address and pin. Only
    /// valid for sessions created by [`Self::connect`].
    pub async fn reconnect(&mut self) -> Result<(), QuicError> {
        let (remote, fingerprint) = match &self.dial {
            Some(dial) => (dial.remote, dial.fingerprint),
            None => return Err(QuicError::NotDialed),
        };
        let fresh = Self::connect(remote, fingerprint).await?;
        // Dropping the old session closes the stale connection and its socket.
        *self = fresh;
        Ok(())
    }

    /// Moves the client's UDP socket, for example after a Wi-Fi change. quinn validates the new
    /// path and migrates the existing connection without a new handshake. The new socket must
    /// use the same address family as the original.
    pub fn rebind_socket(&self, socket: std::net::UdpSocket) -> Result<(), QuicError> {
        match &self.dial {
            Some(dial) => Ok(dial.endpoint.rebind(socket)?),
            None => Err(QuicError::NotDialed),
        }
    }

    /// Closes the connection immediately with an application code, so the peer sees the close
    /// without waiting for the idle timeout.
    pub fn close(&self, reason: &str) {
        self.connection
            .close(QUIC_CLOSE_SHUTDOWN.into(), reason.as_bytes());
    }
}

impl Drop for QuicControlSession {
    fn drop(&mut self) {
        // Best effort and never panics. Closing an already closed connection is a no-op.
        self.connection
            .close(QUIC_CLOSE_DROPPED.into(), b"session dropped");
    }
}

fn transport_config() -> Result<Arc<quinn::TransportConfig>, QuicError> {
    let mut config = quinn::TransportConfig::default();
    config.keep_alive_interval(Some(QUIC_KEEP_ALIVE_INTERVAL));
    config.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(QUIC_MAX_IDLE_TIMEOUT)
            .map_err(|e| QuicError::Config(e.to_string()))?,
    ));
    config.initial_rtt(QUIC_INITIAL_RTT);
    // The protocol uses exactly one bidirectional stream, opened by the client. Refuse everything
    // else and bound the memory a peer can make us buffer.
    config.max_concurrent_bidi_streams(1u32.into());
    config.max_concurrent_uni_streams(0u32.into());
    config.datagram_receive_buffer_size(None);
    config.receive_window(QUIC_RECEIVE_WINDOW.into());
    config.stream_receive_window(QUIC_STREAM_RECEIVE_WINDOW.into());
    config.send_window(QUIC_SEND_WINDOW);
    Ok(Arc::new(config))
}

fn server_config(identity: &HostIdentity) -> Result<quinn::ServerConfig, QuicError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    tls.alpn_protocols = vec![QUIC_ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport_config()?);
    // Validate the client's address before spending handshake work, and bound pending handshakes.
    config.retry_token_lifetime(Duration::from_secs(15));
    config.max_incoming(QUIC_MAX_INCOMING);
    Ok(config)
}

fn client_config(expected: Fingerprint) -> Result<quinn::ClientConfig, QuicError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedFingerprintVerifier {
        expected,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![QUIC_ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport_config()?);
    Ok(config)
}

/// Accepts a server certificate only if its fingerprint matches the pin. The name and validity
/// period are not checked. Handshake signatures are still verified with the crypto provider, which
/// proves that the peer holds the private key for the pinned certificate.
#[derive(Debug)]
struct PinnedFingerprintVerifier {
    expected: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Fingerprint::of_der(end_entity.as_ref()) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate does not match the pinned fingerprint".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlMessage;

    /// Connects a raw quinn client that pins `fingerprint` but never opens a stream.
    async fn silent_client(
        addr: SocketAddr,
        fingerprint: Fingerprint,
    ) -> (quinn::Endpoint, quinn::Connection) {
        let mut endpoint =
            quinn::Endpoint::client((std::net::Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        endpoint.set_default_client_config(client_config(fingerprint).unwrap());
        let connection = endpoint
            .connect(addr, QUIC_SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }

    fn test_server() -> (QuicControlServer, SocketAddr, Fingerprint) {
        let identity = HostIdentity::generate().unwrap();
        let server = QuicControlServer::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        (server, addr, identity.fingerprint())
    }

    fn heartbeat(sequence: u64) -> ControlMessage {
        ControlMessage::Heartbeat {
            sequence,
            timestamp_us: 0,
        }
    }

    /// Dials, sends one heartbeat and accepts on the host.
    async fn session_pair(
        server: &QuicControlServer,
        addr: SocketAddr,
        fingerprint: Fingerprint,
    ) -> (QuicControlSession, QuicControlSession) {
        let (client, accepted) = tokio::join!(
            async {
                let mut c = QuicControlSession::connect(addr, fingerprint)
                    .await
                    .unwrap();
                c.control.send_message(&heartbeat(1)).await.unwrap();
                c
            },
            server.accept()
        );
        let mut accepted = accepted.unwrap();
        assert_eq!(accepted.control.recv_message().await.unwrap(), heartbeat(1));
        (client, accepted)
    }

    #[tokio::test]
    async fn stalled_peer_without_stream_does_not_block_accept() {
        let (server, addr, fingerprint) = test_server();
        // Both clients are in flight at once: the stalled one connects first and never opens a
        // stream, the well-behaved one must still be accepted well before the 5 s deadline.
        let run = async {
            let accepting = async {
                let mut session = server.accept().await.unwrap();
                let msg = session.control.recv_message().await.unwrap();
                (session, msg)
            };
            let dialing = async {
                let stalled = silent_client(addr, fingerprint).await;
                let mut good = QuicControlSession::connect(addr, fingerprint)
                    .await
                    .unwrap();
                good.control.send_message(&heartbeat(9)).await.unwrap();
                (stalled, good)
            };
            tokio::join!(accepting, dialing)
        };
        let ((_session, msg), ((_ep, stalled_conn), _good)) =
            tokio::time::timeout(QUIC_FIRST_STREAM_TIMEOUT - Duration::from_secs(1), run)
                .await
                .expect("well-behaved client was blocked by a stalled peer");
        assert_eq!(msg, heartbeat(9));
        assert!(stalled_conn.close_reason().is_none());
    }

    #[tokio::test]
    async fn stalled_peer_is_disconnected_after_first_stream_timeout() {
        let (server, addr, fingerprint) = test_server();
        let accept_loop = tokio::spawn(async move {
            // Never returns a session for the stalled peer.
            let _ = server.accept().await;
            server
        });
        let (_ep, conn) = silent_client(addr, fingerprint).await;
        let reason = tokio::time::timeout(
            QUIC_FIRST_STREAM_TIMEOUT + Duration::from_secs(3),
            conn.closed(),
        )
        .await
        .expect("stalled connection was not closed");
        match reason {
            quinn::ConnectionError::ApplicationClosed(close) => {
                assert_eq!(close.error_code, QUIC_CLOSE_FIRST_STREAM_TIMEOUT.into());
            }
            other => panic!("expected application close, got {other:?}"),
        }
        accept_loop.abort();
    }

    #[tokio::test]
    async fn extra_streams_from_client_are_refused() {
        let (server, addr, fingerprint) = test_server();
        let (mut client, mut accepted) = session_pair(&server, addr, fingerprint).await;

        // The one allowed bidirectional stream is in use and unidirectional streams are not
        // allowed at all, so neither open can complete, and nothing reaches the host.
        let short = Duration::from_millis(500);
        let conn = client.connection().clone();
        assert!(tokio::time::timeout(short, conn.open_bi()).await.is_err());
        assert!(tokio::time::timeout(short, conn.open_uni()).await.is_err());
        let host_conn = accepted.connection().clone();
        assert!(tokio::time::timeout(short, host_conn.accept_bi())
            .await
            .is_err());
        assert!(tokio::time::timeout(short, host_conn.accept_uni())
            .await
            .is_err());

        // The control stream is unaffected.
        client.control.send_message(&heartbeat(2)).await.unwrap();
        assert_eq!(accepted.control.recv_message().await.unwrap(), heartbeat(2));
    }

    #[tokio::test]
    async fn dropping_session_closes_connection_with_error_code() {
        let (server, addr, fingerprint) = test_server();
        let (client, accepted) = session_pair(&server, addr, fingerprint).await;
        let conn = accepted.connection().clone();
        drop(client);
        let reason = tokio::time::timeout(Duration::from_secs(3), conn.closed())
            .await
            .expect("drop did not close the connection");
        match reason {
            quinn::ConnectionError::ApplicationClosed(close) => {
                assert_eq!(close.error_code, QUIC_CLOSE_DROPPED.into());
            }
            other => panic!("expected application close, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropping_server_closes_its_connections() {
        let (server, addr, fingerprint) = test_server();
        let (client, _accepted) = session_pair(&server, addr, fingerprint).await;
        drop(server);
        let reason = tokio::time::timeout(Duration::from_secs(3), client.closed())
            .await
            .expect("dropping the server did not close the connection");
        assert!(matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(_)
        ));
    }

    #[test]
    fn fingerprint_display_and_parse_round_trip() {
        let fp = Fingerprint::of_der(b"example certificate bytes");
        let text = fp.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Fingerprint>().unwrap(), fp);
        let grouped: String = text
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap().to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(grouped.parse::<Fingerprint>().unwrap(), fp);
    }

    #[test]
    fn fingerprint_parse_rejects_bad_input() {
        assert!("abcd".parse::<Fingerprint>().is_err());
        assert!("zz".repeat(32).parse::<Fingerprint>().is_err());
    }

    #[test]
    fn generated_identity_fingerprint_matches_its_certificate() {
        let identity = HostIdentity::generate().unwrap();
        assert_eq!(
            identity.fingerprint(),
            Fingerprint::of_der(identity.cert.as_ref())
        );
    }
}
