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
//! # Connection management
//!
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
}

impl QuicControlServer {
    /// Binds a UDP socket on `addr` that serves `identity`.
    pub fn bind(addr: SocketAddr, identity: &HostIdentity) -> Result<Self, QuicError> {
        let config = server_config(identity)?;
        let endpoint = quinn::Endpoint::server(config, addr)?;
        Ok(Self { endpoint })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, QuicError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for a client to connect and open its control stream.
    ///
    /// quinn only starts an incoming handshake when this method is polled, so a client's
    /// `connect` completes only while `accept` is running. Run this in a loop or concurrently
    /// with the dialing side. The stream also becomes visible only once the client writes its
    /// first control message (ClientHello). Wrap the call in a timeout if stalled peers are a
    /// concern.
    pub async fn accept(&self) -> Result<QuicControlSession, QuicError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(QuicError::EndpointClosed)?;
        let connection = incoming.await?;
        let (send, recv) = connection.accept_bi().await?;
        Ok(QuicControlSession {
            connection,
            control: ControlChannel::new(recv, send),
            dial: None,
        })
    }

    /// Closes the endpoint and all of its connections.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"server shutdown");
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
        self.connection.close(0u32.into(), reason.as_bytes());
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
