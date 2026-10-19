//! QUIC transport for the control channel. Requires the `quic` cargo feature.
//!
//! Each control session is one QUIC connection carrying one bidirectional stream. That stream is
//! wrapped in the existing [`ControlChannel`] framing, so clipboard, handshake and telemetry
//! messages use the same `ControlMessage` codec over QUIC as over TCP. QUIC streams are reliable
//! and ordered, so no application-level retransmission is needed.
//!
//! # Trust model (LAN, no CA, mutual TLS with pinned fingerprints)
//!
//! - The host generates a self-signed certificate with [`HostIdentity::generate`]. Nothing is
//!   stored on disk, so the identity changes every time the host process restarts.
//! - The client is configured with the host's [`Fingerprint`] (SHA-256 of the DER certificate)
//!   and accepts only a certificate with that exact fingerprint. The certificate's name is not
//!   checked. The client also verifies the TLS 1.3 `CertificateVerify` signature, so a party that
//!   has only seen the public certificate cannot impersonate the host.
//! - Each client has its own self-signed certificate ([`ClientIdentity`]). The host requires a
//!   client certificate and accepts only certificates whose fingerprint is in its
//!   [`AuthorizedClients`] set. Like the host pin, the name and validity period are ignored and
//!   the TLS 1.3 `CertificateVerify` signature is verified, so presenting someone else's public
//!   certificate without its private key fails. An empty set rejects every client.
//! - Fingerprints reach the other side out of band. The simplest option is to display the host
//!   fingerprint on the host and the client fingerprint on the client, and copy each to the other
//!   side over the ADB/USB channel or enter them manually. This module does not implement the
//!   pairing UX or trust-on-first-use (TOFU) storage.
//!
//! # Security notes
//!
//! Both directions are authenticated. A peer that can reach the UDP port but whose certificate
//! fingerprint is not in the [`AuthorizedClients`] set fails the TLS handshake and never obtains a
//! control stream, so it cannot use clipboard control. The client fingerprint of an accepted
//! session is available from [`QuicControlSession::client_fingerprint`] for logging and display.
//! There is no unauthenticated mode.
//!
//! What this does and does not protect:
//!
//! - Protected: a LAN peer that merely knows the host fingerprint, a peer replaying a client's
//!   public certificate, and a peer with no certificate.
//! - A client private key is a long-lived credential. Whoever holds it can connect for as long as
//!   its fingerprint is authorized. [`ClientIdentity::to_der`] output must be stored with
//!   owner-only permissions (for example mode 0600, or the Android keystore / app-private
//!   storage). This crate never writes files.
//! - Removing a fingerprint from [`AuthorizedClients`] stops new handshakes but does not close
//!   sessions that are already established.
//! - A rejected handshake is surfaced by [`QuicControlServer::accept`] as an error and costs the
//!   server only the work of one handshake from an address that already passed Retry validation.
//!
//! Still NOT done (follow-ups):
//!
//! - Pairing UX and a TOFU / trust store for either side's fingerprints.
//! - Persistence of the host and client identities (they are plain values the caller must save).
//! - Revocation UI, and terminating live sessions on revocation.
//! - Wiring QUIC into the host and client binaries (nothing uses this module yet).
//! - Rate limiting of authenticated clients, and per-client authorization scopes.
//!
//! Resource limits (one bidirectional stream, no unidirectional streams, no datagrams, bounded
//! flow-control windows, a 5 s deadline for the first stream, address validation with Retry)
//! bound what any peer can consume.
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

use crate::control::{ControlChannel, ControlError, ControlMessage};
use parking_lot::RwLock;
use quinn::rustls;
use quinn::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use quinn::rustls::crypto::CryptoProvider;
use quinn::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use quinn::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use quinn::rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// ALPN protocol identifier for the control channel.
pub const QUIC_ALPN: &[u8] = b"linux-quest-control/1";

/// Subject alternative name placed in the host certificate. The client sends it as SNI but does
/// not verify it, because pinning replaces name verification.
pub const QUIC_SERVER_NAME: &str = "linux-quest-host";

/// Subject alternative name placed in client certificates. The host does not verify it.
pub const QUIC_CLIENT_NAME: &str = "linux-quest-client";

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
/// Application close code used when an accepted connection unexpectedly has no client identity.
pub const QUIC_CLOSE_UNAUTHORIZED: u32 = 3;

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

    #[error("Invalid client identity: {0}")]
    InvalidIdentity(String),

    /// The peer was refused during the TLS handshake because of its certificate. On the client
    /// side this means the host did not authorize this client's certificate (or the client sent
    /// none). On the host side it is a client that failed authentication.
    #[error("client authentication failed: {0}")]
    Unauthorized(String),

    #[error("QUIC connect failed: {0}")]
    Connect(#[from] quinn::ConnectError),

    #[error("QUIC connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("QUIC connection failed ({quic}); TCP fallback failed: {tcp}")]
    TcpFallback {
        quic: String,
        #[source]
        tcp: std::io::Error,
    },

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

/// Client-side self-signed certificate and key. Create one per client installation with
/// [`ClientIdentity::generate`], persist it with [`ClientIdentity::to_der`] and reload it with
/// [`ClientIdentity::from_der`], so the client keeps the same [`Fingerprint`] across restarts.
/// The host authorizes the client by adding that fingerprint to its [`AuthorizedClients`].
///
/// The serialized form contains the private key. Store it with owner-only permissions (for
/// example `0600` on Linux) or in app-private / keystore-backed storage on Android. This crate
/// never writes it anywhere.
pub struct ClientIdentity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
}

/// Prefix of the [`ClientIdentity::to_der`] encoding: magic and format version.
const CLIENT_IDENTITY_MAGIC: &[u8; 5] = b"LQCI\x01";

impl ClientIdentity {
    /// Generates a fresh ECDSA P-256 self-signed client certificate.
    pub fn generate() -> Result<Self, QuicError> {
        let certified = rcgen::generate_simple_self_signed(vec![QUIC_CLIENT_NAME.to_string()])?;
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

    /// The fingerprint the host must add to its [`AuthorizedClients`].
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The DER certificate (public part only).
    pub fn certificate_der(&self) -> &[u8] {
        self.cert.as_ref()
    }

    /// Serializes certificate and private key into one opaque blob (magic/version, then the DER
    /// certificate and the PKCS#8 private key, each with a 4-byte big-endian length). **The
    /// result is secret key material**: persist it with owner-only file permissions and do not
    /// log it.
    pub fn to_der(&self) -> Vec<u8> {
        let key = match &self.key {
            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der(),
            // `generate` and `from_der` only ever produce PKCS#8 keys.
            _ => unreachable!("client identity keys are always PKCS#8"),
        };
        let mut out =
            Vec::with_capacity(CLIENT_IDENTITY_MAGIC.len() + 8 + self.cert.len() + key.len());
        out.extend_from_slice(CLIENT_IDENTITY_MAGIC);
        out.extend_from_slice(&(self.cert.len() as u32).to_be_bytes());
        out.extend_from_slice(self.cert.as_ref());
        out.extend_from_slice(&(key.len() as u32).to_be_bytes());
        out.extend_from_slice(key);
        out
    }

    /// Restores an identity written by [`Self::to_der`]. Rejects malformed input and a key that
    /// does not belong to the certificate.
    pub fn from_der(bytes: &[u8]) -> Result<Self, QuicError> {
        fn invalid(msg: &str) -> QuicError {
            QuicError::InvalidIdentity(msg.to_string())
        }
        fn take<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], QuicError> {
            if rest.len() < 4 {
                return Err(invalid("truncated"));
            }
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let body = &rest[4..];
            if body.len() < len {
                return Err(invalid("truncated"));
            }
            let (field, tail) = body.split_at(len);
            *rest = tail;
            Ok(field)
        }
        let mut rest = bytes
            .strip_prefix(CLIENT_IDENTITY_MAGIC.as_slice())
            .ok_or_else(|| invalid("bad magic or unsupported version"))?;
        let cert = take(&mut rest)?;
        let key = take(&mut rest)?;
        if !rest.is_empty() {
            return Err(invalid("trailing bytes"));
        }
        // The key must be a usable ECDSA key and its public half must appear in the certificate.
        let key_pair = rcgen::KeyPair::try_from(key)
            .map_err(|e| QuicError::InvalidIdentity(format!("private key: {e}")))?;
        let spki = rcgen::PublicKeyData::subject_public_key_info(&key_pair);
        if !cert.windows(spki.len()).any(|w| w == spki.as_slice()) {
            return Err(invalid("private key does not match the certificate"));
        }
        let cert = CertificateDer::from(cert.to_vec());
        let fingerprint = Fingerprint::of_der(cert.as_ref());
        Ok(Self {
            cert,
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
            fingerprint,
        })
    }
}

impl Clone for ClientIdentity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key: self.key.clone_key(),
            fingerprint: self.fingerprint,
        }
    }
}

impl fmt::Debug for ClientIdentity {
    /// Never prints the private key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// The set of client certificate fingerprints a host accepts. Cloning is cheap and clones share
/// the same set, so a pairing UI can keep a handle and [`add`](Self::add) entries while the
/// server runs. New handshakes see changes immediately. An empty set accepts nobody.
#[derive(Clone, Default)]
pub struct AuthorizedClients {
    inner: Arc<RwLock<HashSet<Fingerprint>>>,
}

impl AuthorizedClients {
    /// An empty set. With no entries every client is rejected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a fingerprint. Returns `false` if it was already present.
    pub fn add(&self, fingerprint: Fingerprint) -> bool {
        self.inner.write().insert(fingerprint)
    }

    /// Removes a fingerprint. Returns `false` if it was not present. Established sessions are not
    /// closed.
    pub fn remove(&self, fingerprint: &Fingerprint) -> bool {
        self.inner.write().remove(fingerprint)
    }

    pub fn contains(&self, fingerprint: &Fingerprint) -> bool {
        self.inner.read().contains(fingerprint)
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// A snapshot of the current entries, in no particular order.
    pub fn snapshot(&self) -> Vec<Fingerprint> {
        self.inner.read().iter().copied().collect()
    }
}

impl FromIterator<Fingerprint> for AuthorizedClients {
    fn from_iter<I: IntoIterator<Item = Fingerprint>>(iter: I) -> Self {
        Self {
            inner: Arc::new(RwLock::new(iter.into_iter().collect())),
        }
    }
}

impl fmt::Debug for AuthorizedClients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedClients")
            .field("len", &self.len())
            .finish()
    }
}

/// The control stream of one QUIC session, using the same framing as the TCP control path.
pub type QuicControl = ControlChannel<quinn::RecvStream, quinn::SendStream>;

/// Host side: accepts QUIC connections that carry control sessions.
pub struct QuicControlServer {
    endpoint: quinn::Endpoint,
    authorized: AuthorizedClients,
    /// Sessions (or handshake errors) produced by the per-connection tasks, waiting for `accept`.
    ready_tx: tokio::sync::mpsc::UnboundedSender<Result<QuicControlSession, QuicError>>,
    ready_rx: tokio::sync::Mutex<
        tokio::sync::mpsc::UnboundedReceiver<Result<QuicControlSession, QuicError>>,
    >,
}

impl QuicControlServer {
    /// Binds a UDP socket on `addr` that serves `identity` and accepts only clients whose
    /// certificate fingerprint is in `authorized`. Keep a clone of `authorized` to add entries
    /// later. Client authentication is mandatory: an empty set rejects every client.
    pub fn bind(
        addr: SocketAddr,
        identity: &HostIdentity,
        authorized: AuthorizedClients,
    ) -> Result<Self, QuicError> {
        let config = server_config(identity, authorized.clone())?;
        let endpoint = quinn::Endpoint::server(config, addr)?;
        let (ready_tx, ready_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            endpoint,
            authorized,
            ready_tx,
            ready_rx: tokio::sync::Mutex::new(ready_rx),
        })
    }

    /// A handle to the authorized set this server checks, for adding or removing entries.
    pub fn authorized_clients(&self) -> AuthorizedClients {
        self.authorized.clone()
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
    /// handshake, including a client that failed authentication, is returned as an error; call
    /// `accept` again to keep serving. A rejected handshake ends immediately and holds no
    /// resources afterwards.
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
    let remote = incoming.remote_address();
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(err) => {
            tracing::warn!(%remote, error = %err, "QUIC handshake failed");
            return Some(Err(map_connection_error(err, true)));
        }
    };
    // The verifier already authorized this certificate; extract its fingerprint for the session.
    let Some(client_fingerprint) = peer_fingerprint(&connection) else {
        connection.close(QUIC_CLOSE_UNAUTHORIZED.into(), b"no client certificate");
        return Some(Err(QuicError::Unauthorized(
            "no client certificate on an authenticated connection".into(),
        )));
    };
    match tokio::time::timeout(QUIC_FIRST_STREAM_TIMEOUT, connection.accept_bi()).await {
        Ok(Ok((send, recv))) => Some(Ok(QuicControlSession {
            connection,
            control: ControlChannel::new(recv, send),
            dial: None,
            client_fingerprint: Some(client_fingerprint),
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

/// Waits until the host confirms the handshake (HANDSHAKE_DONE, which a server sends only after it
/// has verified our certificate) or closes the connection. In TLS 1.3 the client's own handshake
/// completes before the host judges the client certificate, so without this a rejected client
/// would appear connected until its first read or write. Bounded by [`QUIC_MAX_IDLE_TIMEOUT`].
async fn wait_handshake_confirmed(
    connection: &quinn::Connection,
) -> Result<(), quinn::ConnectionError> {
    let confirmed = async {
        while connection.stats().frame_rx.handshake_done == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::select! {
        err = connection.closed() => Err(err),
        waited = tokio::time::timeout(QUIC_MAX_IDLE_TIMEOUT, confirmed) => {
            waited.map_err(|_| quinn::ConnectionError::TimedOut)
        }
    }
}

/// SHA-256 fingerprint of the peer's end-entity certificate, if it presented one.
fn peer_fingerprint(connection: &quinn::Connection) -> Option<Fingerprint> {
    let identity = connection.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| Fingerprint::of_der(c.as_ref()))
}

/// Converts a connection error, turning a TLS certificate rejection into
/// [`QuicError::Unauthorized`] with a readable message.
fn map_connection_error(err: quinn::ConnectionError, server_side: bool) -> QuicError {
    // A TLS alert is carried as the QUIC transport error code 0x100 + alert, either in a local
    // `TransportError` (we detected the problem) or in a CONNECTION_CLOSE from the peer.
    let (code, detail) = match &err {
        quinn::ConnectionError::TransportError(te) => (u64::from(te.code), te.to_string()),
        quinn::ConnectionError::ConnectionClosed(close) => (
            u64::from(close.error_code),
            String::from_utf8_lossy(&close.reason).into_owned(),
        ),
        _ => return QuicError::Connection(err),
    };
    if !(0x100..0x200).contains(&code) {
        return QuicError::Connection(err);
    }
    let alert = code - 0x100;
    // access_denied, bad/unknown/unsupported certificate (including unknown_ca), decrypt_error
    // (bad CertificateVerify signature) and certificate_required (no certificate sent).
    if !matches!(alert, 49 | 42 | 46 | 48 | 43 | 51 | 116) {
        return QuicError::Connection(err);
    }
    let what = if server_side {
        "client certificate rejected"
    } else {
        "host rejected this client: its certificate is not authorized or it sent none"
    };
    QuicError::Unauthorized(format!("{what} (TLS alert {alert}: {detail})"))
}

/// Client state needed to reconnect or rebind. Only sessions created by [`QuicControlSession::connect`]
/// have it.
struct Dial {
    endpoint: quinn::Endpoint,
    remote: SocketAddr,
    fingerprint: Fingerprint,
    identity: ClientIdentity,
}

/// One connected control session: a QUIC connection plus its control stream.
pub struct QuicControlSession {
    connection: quinn::Connection,
    /// The control stream. Use it like any other [`ControlChannel`].
    pub control: QuicControl,
    dial: Option<Dial>,
    /// Fingerprint of the authenticated client certificate. Set on the host side only.
    client_fingerprint: Option<Fingerprint>,
}

/// Client control session that prefers pinned QUIC and can use the legacy TCP control channel.
///
/// TCP fallback is attempted for connection failures only. Certificate, pin, and identity errors
/// are returned directly so a failed authentication cannot silently downgrade to plaintext TCP.
pub enum ControlClientSession {
    Quic(Box<QuicControlSession>),
    Tcp {
        remote: SocketAddr,
        control: ControlChannel<OwnedReadHalf, OwnedWriteHalf>,
    },
}

impl ControlClientSession {
    /// Connects over pinned QUIC, falling back to the existing TCP control channel when QUIC is
    /// unavailable. The host must expose both listeners for fallback to work.
    pub async fn connect_with_tcp_fallback(
        quic_remote: SocketAddr,
        tcp_remote: SocketAddr,
        expected: Fingerprint,
        identity: &ClientIdentity,
    ) -> Result<Self, QuicError> {
        match QuicControlSession::connect(quic_remote, expected, identity).await {
            Ok(session) => Ok(Self::Quic(Box::new(session))),
            Err(err) if is_authentication_error(&err) => Err(err),
            Err(quic_error) => {
                let stream =
                    TcpStream::connect(tcp_remote)
                        .await
                        .map_err(|tcp| QuicError::TcpFallback {
                            quic: quic_error.to_string(),
                            tcp,
                        })?;
                stream.set_nodelay(true)?;
                let (reader, writer) = stream.into_split();
                Ok(Self::Tcp {
                    remote: tcp_remote,
                    control: ControlChannel::new(reader, writer),
                })
            }
        }
    }

    /// Sends one message using the selected control transport.
    pub async fn send_message(&mut self, message: &ControlMessage) -> Result<(), ControlError> {
        match self {
            Self::Quic(session) => session.control.send_message(message).await,
            Self::Tcp { control, .. } => control.send_message(message).await,
        }
    }

    /// Receives one message using the selected control transport.
    pub async fn recv_message(&mut self) -> Result<ControlMessage, ControlError> {
        match self {
            Self::Quic(session) => session.control.recv_message().await,
            Self::Tcp { control, .. } => control.recv_message().await,
        }
    }

    /// True when this session is using QUIC rather than the TCP fallback.
    pub fn is_quic(&self) -> bool {
        matches!(self, Self::Quic(_))
    }

    /// Reconnects the selected transport. QUIC preserves its pin and client identity.
    pub async fn reconnect(&mut self) -> Result<(), QuicError> {
        match self {
            Self::Quic(session) => session.reconnect().await,
            Self::Tcp { remote, control } => {
                let stream = TcpStream::connect(*remote).await?;
                stream.set_nodelay(true)?;
                let (reader, writer) = stream.into_split();
                *control = ControlChannel::new(reader, writer);
                Ok(())
            }
        }
    }
}

fn is_authentication_error(error: &QuicError) -> bool {
    match error {
        QuicError::Tls(_)
        | QuicError::Unauthorized(_)
        | QuicError::InvalidFingerprint(_)
        | QuicError::InvalidIdentity(_)
        | QuicError::Certificate(_)
        | QuicError::Config(_) => true,
        QuicError::Connection(quinn::ConnectionError::TransportError(te)) => {
            (0x100..0x200).contains(&u64::from(te.code))
        }
        QuicError::Connection(quinn::ConnectionError::ConnectionClosed(close)) => {
            (0x100..0x200).contains(&u64::from(close.error_code))
        }
        _ => false,
    }
}

impl QuicControlSession {
    /// Dials `remote`, pins `expected` and authenticates with `identity`. The connection is
    /// accepted only if the host presents a certificate with that fingerprint, and the host
    /// accepts only if `identity.fingerprint()` is authorized there.
    ///
    /// In TLS 1.3 the client finishes its side of the handshake before the host has checked the
    /// client certificate, so a rejection by the host can surface either here or on the first
    /// control-stream read or write. [`Self::connect`] waits for the host's verdict, so a rejected
    /// client gets [`QuicError::Unauthorized`] from here rather than a later I/O error.
    pub async fn connect(
        remote: SocketAddr,
        expected: Fingerprint,
        identity: &ClientIdentity,
    ) -> Result<Self, QuicError> {
        let bind_addr: SocketAddr = if remote.is_ipv4() {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let mut endpoint = quinn::Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(client_config(expected, identity)?);

        let connection = endpoint
            .connect(remote, QUIC_SERVER_NAME)?
            .await
            .map_err(|e| map_connection_error(e, false))?;
        wait_handshake_confirmed(&connection)
            .await
            .map_err(|e| map_connection_error(e, false))?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|e| map_connection_error(e, false))?;
        Ok(Self {
            connection,
            control: ControlChannel::new(recv, send),
            dial: Some(Dial {
                endpoint,
                remote,
                fingerprint: expected,
                identity: identity.clone(),
            }),
            client_fingerprint: None,
        })
    }

    /// Fingerprint of the client certificate the host authenticated for this session. `Some` on
    /// sessions returned by [`QuicControlServer::accept`], `None` on the dialing side.
    pub fn client_fingerprint(&self) -> Option<Fingerprint> {
        self.client_fingerprint
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
        let (remote, fingerprint, identity) = match &self.dial {
            Some(dial) => (dial.remote, dial.fingerprint, dial.identity.clone()),
            None => return Err(QuicError::NotDialed),
        };
        let fresh = Self::connect(remote, fingerprint, &identity).await?;
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

fn server_config(
    identity: &HostIdentity,
    authorized: AuthorizedClients,
) -> Result<quinn::ServerConfig, QuicError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AuthorizedClientVerifier {
        authorized,
        provider: provider.clone(),
    });
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
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

fn client_config(
    expected: Fingerprint,
    identity: &ClientIdentity,
) -> Result<quinn::ClientConfig, QuicError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedFingerprintVerifier {
        expected,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
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

/// Accepts a client certificate only if its fingerprint is in the authorized set. Like the host
/// pin, name and validity are ignored. The TLS 1.3 `CertificateVerify` signature is verified with
/// the crypto provider, which proves the client holds the private key. Client authentication is
/// mandatory, TLS 1.2 is refused, and there is no fall-through that accepts unknown clients.
#[derive(Debug)]
struct AuthorizedClientVerifier {
    authorized: AuthorizedClients,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AuthorizedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // Self-signed certificates have no CA to hint at.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let fingerprint = Fingerprint::of_der(end_entity.as_ref());
        if self.authorized.contains(&fingerprint) {
            Ok(ClientCertVerified::assertion())
        } else {
            tracing::warn!(%fingerprint, "rejecting QUIC client with unauthorized certificate");
            // Sent to the peer as the TLS `access_denied` alert.
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not enabled, so this is unreachable. Fail closed regardless.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
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

    fn client_auth_mandatory(&self) -> bool {
        true
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
        identity: &ClientIdentity,
    ) -> (quinn::Endpoint, quinn::Connection) {
        let mut endpoint =
            quinn::Endpoint::client((std::net::Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        endpoint.set_default_client_config(client_config(fingerprint, identity).unwrap());
        let connection = endpoint
            .connect(addr, QUIC_SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }

    /// Server that authorizes exactly the returned client identity.
    fn test_server() -> (QuicControlServer, SocketAddr, Fingerprint, ClientIdentity) {
        let (server, addr, fingerprint, authorized) = test_server_with(AuthorizedClients::new());
        let client = ClientIdentity::generate().unwrap();
        authorized.add(client.fingerprint());
        (server, addr, fingerprint, client)
    }

    fn test_server_with(
        authorized: AuthorizedClients,
    ) -> (
        QuicControlServer,
        SocketAddr,
        Fingerprint,
        AuthorizedClients,
    ) {
        let identity = HostIdentity::generate().unwrap();
        let server = QuicControlServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            &identity,
            authorized.clone(),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        (server, addr, identity.fingerprint(), authorized)
    }

    /// Dials with `client` while the server accepts. Returns both results.
    async fn dial_and_accept(
        server: &QuicControlServer,
        addr: SocketAddr,
        fingerprint: Fingerprint,
        client: &ClientIdentity,
    ) -> (
        Result<QuicControlSession, QuicError>,
        Result<QuicControlSession, QuicError>,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                async {
                    let mut c = QuicControlSession::connect(addr, fingerprint, client).await?;
                    c.control.send_message(&heartbeat(1)).await?;
                    Ok(c)
                },
                server.accept()
            )
        })
        .await
        .expect("handshake must succeed or fail promptly, not hang")
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
        identity: &ClientIdentity,
    ) -> (QuicControlSession, QuicControlSession) {
        let (client, accepted) = tokio::join!(
            async {
                let mut c = QuicControlSession::connect(addr, fingerprint, identity)
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
        let (server, addr, fingerprint, ident) = test_server();
        // Both clients are in flight at once: the stalled one connects first and never opens a
        // stream, the well-behaved one must still be accepted well before the 5 s deadline.
        let run = async {
            let accepting = async {
                let mut session = server.accept().await.unwrap();
                let msg = session.control.recv_message().await.unwrap();
                (session, msg)
            };
            let dialing = async {
                let stalled = silent_client(addr, fingerprint, &ident).await;
                let mut good = QuicControlSession::connect(addr, fingerprint, &ident)
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
        let (server, addr, fingerprint, ident) = test_server();
        let accept_loop = tokio::spawn(async move {
            // Never returns a session for the stalled peer.
            let _ = server.accept().await;
            server
        });
        let (_ep, conn) = silent_client(addr, fingerprint, &ident).await;
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
        let (server, addr, fingerprint, ident) = test_server();
        let (mut client, mut accepted) = session_pair(&server, addr, fingerprint, &ident).await;

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
        let (server, addr, fingerprint, ident) = test_server();
        let (client, accepted) = session_pair(&server, addr, fingerprint, &ident).await;
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
        let (server, addr, fingerprint, ident) = test_server();
        let (client, _accepted) = session_pair(&server, addr, fingerprint, &ident).await;
        drop(server);
        let reason = tokio::time::timeout(Duration::from_secs(3), client.closed())
            .await
            .expect("dropping the server did not close the connection");
        assert!(matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(_)
        ));
    }

    fn assert_unauthorized(result: Result<QuicControlSession, QuicError>, what: &str) {
        match result {
            Err(QuicError::Unauthorized(_)) => {}
            Err(other) => panic!("{what}: expected Unauthorized, got {other:?}"),
            Ok(_) => panic!("{what}: connection must be rejected"),
        }
    }

    #[tokio::test]
    async fn authorized_client_connects_and_host_sees_its_fingerprint() {
        let (server, addr, fingerprint, ident) = test_server();
        let (client, accepted) = session_pair(&server, addr, fingerprint, &ident).await;
        assert_eq!(accepted.client_fingerprint(), Some(ident.fingerprint()));
        assert_eq!(client.client_fingerprint(), None);
    }

    #[tokio::test]
    async fn unauthorized_client_is_rejected_with_auth_error() {
        let (server, addr, fingerprint, _authorized_ident) = test_server();
        let stranger = ClientIdentity::generate().unwrap();
        let started = std::time::Instant::now();
        let (dial, accepted) = dial_and_accept(&server, addr, fingerprint, &stranger).await;
        assert_unauthorized(dial, "stranger");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "rejection was slow"
        );
        // The host reports the failed handshake too, and never yields a session.
        assert_unauthorized(accepted, "host side");
    }

    #[tokio::test]
    async fn empty_authorized_set_rejects_everyone() {
        let (server, addr, fingerprint, _) = test_server_with(AuthorizedClients::new());
        let client = ClientIdentity::generate().unwrap();
        let (dial, _) = dial_and_accept(&server, addr, fingerprint, &client).await;
        assert_unauthorized(dial, "empty set");
    }

    /// Certificate resolver that presents `cert` but signs with an unrelated key. rustls refuses
    /// to build such a pair with `with_client_auth_cert`, so a resolver is the only way to
    /// simulate a peer that replays someone else's public certificate.
    #[derive(Debug)]
    struct MismatchedKeyResolver(Arc<rustls::sign::CertifiedKey>);

    impl rustls::client::ResolvesClientCert for MismatchedKeyResolver {
        fn resolve(
            &self,
            _root_hint_subjects: &[&[u8]],
            _sigschemes: &[SignatureScheme],
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(self.0.clone())
        }

        fn has_certs(&self) -> bool {
            true
        }
    }

    /// Raw quinn client that pins `fingerprint` and uses `resolver` (or no certificate).
    fn raw_client_endpoint(
        fingerprint: Fingerprint,
        resolver: Option<Arc<dyn rustls::client::ResolvesClientCert>>,
    ) -> quinn::Endpoint {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedFingerprintVerifier {
                expected: fingerprint,
                provider,
            }));
        let mut tls = match resolver {
            Some(resolver) => builder.with_client_cert_resolver(resolver),
            None => builder.with_no_client_auth(),
        };
        tls.alpn_protocols = vec![QUIC_ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut endpoint =
            quinn::Endpoint::client((std::net::Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        endpoint
    }

    /// Dials with a raw client and returns what the client and the host each observe.
    async fn raw_dial_and_accept(
        server: &QuicControlServer,
        addr: SocketAddr,
        endpoint: &quinn::Endpoint,
    ) -> (
        Result<quinn::Connection, quinn::ConnectionError>,
        Result<QuicControlSession, QuicError>,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                async {
                    let connection = endpoint.connect(addr, QUIC_SERVER_NAME).unwrap().await?;
                    wait_handshake_confirmed(&connection).await?;
                    Ok(connection)
                },
                server.accept()
            )
        })
        .await
        .expect("must fail promptly, not hang")
    }

    #[tokio::test]
    async fn certificate_without_its_private_key_fails_signature_check() {
        let (server, addr, fingerprint, authorized_ident) = test_server();
        // Present the authorized certificate, but sign with a different key.
        let other = ClientIdentity::generate().unwrap();
        let signing_key = rustls::crypto::ring::sign::any_ecdsa_type(&other.key).unwrap();
        let certified =
            rustls::sign::CertifiedKey::new(vec![authorized_ident.cert.clone()], signing_key);
        let endpoint = raw_client_endpoint(
            fingerprint,
            Some(Arc::new(MismatchedKeyResolver(Arc::new(certified)))),
        );
        let (dial, accepted) = raw_dial_and_accept(&server, addr, &endpoint).await;
        let err = dial.expect_err("stolen certificate must be rejected");
        assert!(
            matches!(map_connection_error(err, false), QuicError::Unauthorized(_)),
            "expected an authentication error"
        );
        assert!(accepted.is_err(), "host must not yield a session");
    }

    #[tokio::test]
    async fn fingerprint_added_at_runtime_admits_previously_rejected_client() {
        let (server, addr, fingerprint, authorized) = test_server_with(AuthorizedClients::new());
        let client = ClientIdentity::generate().unwrap();
        let (dial, _) = dial_and_accept(&server, addr, fingerprint, &client).await;
        assert_unauthorized(dial, "before pairing");

        authorized.add(client.fingerprint());
        let (dial, accepted) = dial_and_accept(&server, addr, fingerprint, &client).await;
        let _client_session = dial.expect("client should be admitted after pairing");
        assert_eq!(
            accepted.unwrap().client_fingerprint(),
            Some(client.fingerprint())
        );

        // Removal blocks new handshakes again.
        assert!(authorized.remove(&client.fingerprint()));
        let (dial, _) = dial_and_accept(&server, addr, fingerprint, &client).await;
        assert_unauthorized(dial, "after removal");
    }

    #[tokio::test]
    async fn client_without_certificate_is_rejected() {
        let (server, addr, fingerprint, _ident) = test_server();
        let endpoint = raw_client_endpoint(fingerprint, None);
        let (dial, accepted) = raw_dial_and_accept(&server, addr, &endpoint).await;
        let err = dial.expect_err("client without a certificate must be rejected");
        assert!(
            matches!(map_connection_error(err, false), QuicError::Unauthorized(_)),
            "expected an authentication error"
        );
        assert!(accepted.is_err());
    }

    #[tokio::test]
    async fn rejected_handshakes_do_not_block_authorized_clients() {
        let (server, addr, fingerprint, ident) = test_server();
        let server = Arc::new(server);
        // Many unauthorized clients first, then an authorized one.
        let accept_loop = {
            let server = server.clone();
            tokio::spawn(async move {
                loop {
                    match server.accept().await {
                        Ok(session) => return session,
                        Err(QuicError::Unauthorized(_)) => continue,
                        Err(other) => panic!("unexpected accept error: {other:?}"),
                    }
                }
            })
        };
        let strangers: Vec<_> = (0..100)
            .map(|_| {
                tokio::spawn(async move {
                    let stranger = ClientIdentity::generate().unwrap();
                    QuicControlSession::connect(addr, fingerprint, &stranger).await
                })
            })
            .collect();
        for s in strangers {
            assert_unauthorized(s.await.unwrap(), "stranger");
        }
        // The server is still serving: an authorized client gets through promptly.
        let mut good = tokio::time::timeout(
            Duration::from_secs(5),
            QuicControlSession::connect(addr, fingerprint, &ident),
        )
        .await
        .expect("authorized client was blocked by rejected handshakes")
        .unwrap();
        good.control.send_message(&heartbeat(5)).await.unwrap();
        let mut accepted = tokio::time::timeout(Duration::from_secs(5), accept_loop)
            .await
            .expect("accept did not return the authorized session")
            .unwrap();
        assert_eq!(accepted.control.recv_message().await.unwrap(), heartbeat(5));
        assert_eq!(accepted.client_fingerprint(), Some(ident.fingerprint()));
    }

    #[tokio::test]
    async fn host_pin_mismatch_still_fails() {
        let (server, addr, _fingerprint, ident) = test_server();
        let wrong_pin = Fingerprint::of_der(b"not the host certificate");
        let (dial, _) = dial_and_accept(&server, addr, wrong_pin, &ident).await;
        match dial {
            Err(err) => assert!(
                err.to_string().contains("pinned fingerprint"),
                "expected a pin mismatch, got: {err}"
            ),
            Ok(_) => panic!("wrong pin must fail"),
        }
    }

    #[test]
    fn client_identity_round_trips_through_der() {
        let ident = ClientIdentity::generate().unwrap();
        let restored = ClientIdentity::from_der(&ident.to_der()).unwrap();
        assert_eq!(restored.fingerprint(), ident.fingerprint());
        assert_eq!(restored.certificate_der(), ident.certificate_der());
        assert!(!format!("{ident:?}").contains("key:"));
    }

    #[tokio::test]
    async fn restored_identity_authenticates() {
        let (server, addr, fingerprint, ident) = test_server();
        let restored = ClientIdentity::from_der(&ident.to_der()).unwrap();
        let (_c, accepted) = session_pair(&server, addr, fingerprint, &restored).await;
        assert_eq!(accepted.client_fingerprint(), Some(ident.fingerprint()));
    }

    #[test]
    fn client_identity_from_der_rejects_bad_input() {
        let a = ClientIdentity::generate().unwrap();
        let b = ClientIdentity::generate().unwrap();
        let good = a.to_der();
        assert!(ClientIdentity::from_der(b"").is_err());
        assert!(ClientIdentity::from_der(&good[..good.len() - 1]).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(ClientIdentity::from_der(&trailing).is_err());
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xff;
        assert!(ClientIdentity::from_der(&bad_magic).is_err());
        // Certificate of `a` with the key of `b`.
        let mismatched = ClientIdentity {
            cert: a.cert.clone(),
            key: b.key.clone_key(),
            fingerprint: a.fingerprint(),
        };
        assert!(ClientIdentity::from_der(&mismatched.to_der()).is_err());
    }

    #[test]
    fn authorized_clients_clones_share_state() {
        let set = AuthorizedClients::new();
        let handle = set.clone();
        let fp = Fingerprint::of_der(b"x");
        assert!(set.is_empty() && !set.contains(&fp));
        assert!(handle.add(fp));
        assert!(!handle.add(fp));
        assert!(set.contains(&fp));
        assert_eq!(set.len(), 1);
        assert_eq!(set.snapshot(), vec![fp]);
        assert!(set.remove(&fp));
        assert!(handle.is_empty());
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
