//! QUIC transport (DESIGN.md §3.1, §4, M3 Phase B).
//!
//! Real two-process networking for lockstep. The deterministic core
//! ([`LockstepSession`](crate::LockstepSession)) is sync; quinn is async.
//! This module bridges the two: a background thread owns a single-thread
//! tokio runtime, a quinn endpoint, and one bidirectional QUIC stream,
//! and shuttles encoded [`NetMessage`]s between that stream and a pair of
//! tokio mpsc channels. [`QuicTransport`] holds the sync ends of those
//! channels and implements [`Transport`], so the session never sees
//! async, quinn, or TLS.
//!
//! Reliable, ordered delivery is exactly what lockstep needs (an input
//! bundle must not be dropped or reordered within a player's stream), so
//! we use a QUIC *stream*, not unreliable datagrams.
//!
//! Security posture is **dev/LAN**: the server mints a self-signed cert
//! and the client accepts any cert. DESIGN.md §7's matchmaker + real
//! cert story is owed later; this unblocks two hosts on a trusted link.

use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use quinn::crypto::rustls::QuicClientConfig;
use quinn::rustls;
use quinn::{ClientConfig, Endpoint, RecvStream, SendStream, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::mpsc as tokio_mpsc;

use crate::transport::Transport;
use crate::wire::{decode, encode, NetMessage};

/// A QUIC setup failure (bind, connect, handshake, or cert error).
#[derive(Debug)]
pub struct QuicError(String);

impl fmt::Display for QuicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "quic transport error: {}", self.0)
    }
}

impl std::error::Error for QuicError {}

impl QuicError {
    fn from<E: fmt::Display>(e: E) -> QuicError {
        QuicError(e.to_string())
    }
}

/// A lockstep [`Transport`] over a QUIC bidirectional stream. Send/poll
/// are non-blocking and talk to a background networking thread; the
/// connection tears down when this value is dropped.
pub struct QuicTransport {
    to_net: tokio_mpsc::UnboundedSender<NetMessage>,
    from_net: tokio_mpsc::UnboundedReceiver<NetMessage>,
    /// `true` while the connection's I/O loops are running; the network
    /// thread clears it when the stream closes (peer gone, decode
    /// mismatch, or local drop). Read by [`Transport::connected`].
    alive: Arc<AtomicBool>,
    /// Kept so the networking thread's lifetime is tied to ours (detached
    /// on drop; the channels closing is what stops its loops).
    _thread: JoinHandle<()>,
}

impl QuicTransport {
    /// Bind a server endpoint on `addr` and **block until a peer
    /// connects** (player 0 / host side).
    ///
    /// # Errors
    /// Returns [`QuicError`] if binding, the TLS setup, or the handshake
    /// fails.
    pub fn listen(addr: SocketAddr) -> Result<QuicTransport, QuicError> {
        Self::spawn(Role::Server(addr))
    }

    /// Connect to a server at `addr` (player 1 / client side). Retries
    /// briefly so start order between the two hosts does not matter.
    ///
    /// # Errors
    /// Returns [`QuicError`] if the endpoint, TLS setup, or handshake
    /// fails after the retry window.
    pub fn connect(addr: SocketAddr) -> Result<QuicTransport, QuicError> {
        Self::spawn(Role::Client(addr))
    }

    fn spawn(role: Role) -> Result<QuicTransport, QuicError> {
        let (to_net_tx, to_net_rx) = tokio_mpsc::unbounded_channel();
        let (from_net_tx, from_net_rx) = tokio_mpsc::unbounded_channel();
        // Carries the connection result back so the constructor blocks
        // until the link is up (or fails).
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = Arc::clone(&alive);

        let thread = thread::Builder::new()
            .name("monada-quic".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(QuicError::from(e)));
                        alive_thread.store(false, Ordering::Relaxed);
                        return;
                    }
                };
                rt.block_on(async move {
                    match establish(role).await {
                        Ok((send, recv)) => {
                            let _ = ready_tx.send(Ok(()));
                            run_loops(send, recv, to_net_rx, from_net_tx).await;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                });
                // The connection's loops have ended (peer gone, decode
                // mismatch, or local drop): the link is down.
                alive_thread.store(false, Ordering::Relaxed);
            })
            .map_err(QuicError::from)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(QuicTransport {
                to_net: to_net_tx,
                from_net: from_net_rx,
                alive,
                _thread: thread,
            }),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(QuicError::from(e)),
        }
    }
}

impl Transport for QuicTransport {
    fn send(&mut self, msg: NetMessage) {
        // A closed channel means the network thread is gone; `connected`
        // reports that. (Disconnect is terminal in M3 — no reconnect yet.)
        let _ = self.to_net.send(msg);
    }

    fn poll(&mut self) -> Vec<NetMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = self.from_net.try_recv() {
            out.push(msg);
        }
        out
    }

    fn connected(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

enum Role {
    Server(SocketAddr),
    Client(SocketAddr),
}

/// Establish the connection + bi-stream for our role, returning the
/// stream halves.
async fn establish(role: Role) -> Result<(SendStream, RecvStream), QuicError> {
    // rustls 0.23 needs a process-default crypto provider; install ring
    // once (idempotent — ignore "already installed").
    let _ = rustls::crypto::ring::default_provider().install_default();
    match role {
        Role::Server(addr) => establish_server(addr).await,
        Role::Client(addr) => establish_client(addr).await,
    }
}

async fn establish_server(addr: SocketAddr) -> Result<(SendStream, RecvStream), QuicError> {
    let endpoint = Endpoint::server(server_config()?, addr).map_err(QuicError::from)?;
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| QuicError("endpoint closed before a peer connected".to_string()))?;
    let connection = incoming.await.map_err(QuicError::from)?;
    // The client opens the stream and writes a prime frame; accept it.
    let (send, recv) = connection.accept_bi().await.map_err(QuicError::from)?;
    Ok((send, recv))
}

async fn establish_client(addr: SocketAddr) -> Result<(SendStream, RecvStream), QuicError> {
    let mut endpoint =
        Endpoint::client("0.0.0.0:0".parse().expect("valid bind addr")).map_err(QuicError::from)?;
    endpoint.set_default_client_config(client_config()?);

    // Retry: the listener may not be bound yet, so either start order
    // works. ~3s total.
    let mut last = QuicError("no connection attempt".to_string());
    let mut connection = None;
    for _ in 0..30 {
        match endpoint.connect(addr, "localhost") {
            Ok(connecting) => match connecting.await {
                Ok(c) => {
                    connection = Some(c);
                    break;
                }
                Err(e) => last = QuicError::from(e),
            },
            Err(e) => last = QuicError::from(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let connection = connection.ok_or(last)?;

    let (mut send, recv) = connection.open_bi().await.map_err(QuicError::from)?;
    // Prime the stream so the server's `accept_bi` resolves (QUIC opens a
    // stream lazily on first write). A zero-length frame is a no-op the
    // reader skips.
    send.write_all(&0u32.to_le_bytes())
        .await
        .map_err(QuicError::from)?;
    Ok((send, recv))
}

/// Pump messages both ways until either side closes (session dropped or
/// peer gone).
async fn run_loops(
    mut send: SendStream,
    mut recv: RecvStream,
    mut to_net: tokio_mpsc::UnboundedReceiver<NetMessage>,
    from_net: tokio_mpsc::UnboundedSender<NetMessage>,
) {
    let writer = async {
        while let Some(msg) = to_net.recv().await {
            let Ok(bytes) = encode(&msg) else {
                continue;
            };
            let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            if send.write_all(&len.to_le_bytes()).await.is_err() {
                break;
            }
            if send.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    };

    let reader = async {
        let mut len_buf = [0u8; 4];
        loop {
            if recv.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 {
                // Prime / keepalive frame.
                continue;
            }
            let mut buf = vec![0u8; len];
            if recv.read_exact(&mut buf).await.is_err() {
                break;
            }
            // On a reliable ordered stream, a decode failure is a
            // protocol/version mismatch, not transient corruption. Silently
            // dropping the frame would turn it into a permanent, unexplained
            // lockstep stall; tearing the connection instead makes the
            // failure total and observable (via `connected()`).
            let Ok(msg) = decode(&buf) else {
                break;
            };
            if from_net.send(msg).is_err() {
                break;
            }
        }
    };

    // Stop as soon as either direction ends.
    tokio::select! {
        () = writer => {},
        () = reader => {},
    }
}

/// Server TLS config from a freshly-minted self-signed cert (dev/LAN).
fn server_config() -> Result<ServerConfig, QuicError> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .map_err(QuicError::from)?;
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    ServerConfig::with_single_cert(vec![cert_der], key_der).map_err(QuicError::from)
}

/// Client config that accepts any server cert (dev/LAN only).
fn client_config() -> Result<ClientConfig, QuicError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification(provider)))
        .with_no_client_auth();
    let quic_crypto = QuicClientConfig::try_from(crypto).map_err(QuicError::from)?;
    Ok(ClientConfig::new(Arc::new(quic_crypto)))
}

/// A `rustls` verifier that trusts any certificate. Acceptable only for
/// the dev/LAN posture above; delegates signature checks to the real
/// provider so the handshake still completes correctly.
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
