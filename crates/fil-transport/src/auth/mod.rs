//! Mutual authentication for direct (QUIC) mode, DESIGN.md §8.3 and §11.
//!
//! - The daemon makes a [`DaemonIdentity`] at start. The bootstrap sends its [`Fingerprint`] over
//!   the pipe, and the client pins it ([`PinnedServerVerifier`]).
//! - The client makes a [`ClientKey`] per attach and sends the public half over the pipe. The
//!   daemon registers it in its [`KeyRegistry`] and accepts only registered keys
//!   ([`RegisteredClientVerifier`]).
//! - [`server_config`] and [`client_config`] wire these into quinn.
//!
//! Nothing here touches the disk. Design notes and the threat-table rows covered:
//! `docs/m1/quic-auth.md`.

mod identity;
mod registry;
mod verify;

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{IdleTimeout, TransportConfig};
use rustls::client::{AlwaysResolvesClientRawPublicKeys, Resumption};
use rustls::crypto::ring as provider;
use rustls::pki_types::CertificateDer;
use rustls::server::NoServerSessionStorage;
use rustls::sign::SingleCertAndKey;

pub use identity::{ClientKey, DaemonIdentity, Fingerprint, PublicKey};
pub use registry::KeyRegistry;
pub use verify::{PinnedServerVerifier, RegisteredClientVerifier};

/// The ALPN protocol both sides must agree on.
pub const ALPN: &[u8] = b"fil/1";

/// The name the client passes to `Endpoint::connect`. It is sent as SNI and never checked, so the
/// real host name does not leak into the handshake. Any valid DNS name works.
pub const SERVER_NAME: &str = "fil.invalid";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("key generation failed: {0}")]
    KeyGen(rcgen::Error),
    #[error("TLS configuration failed: {0}")]
    Tls(rustls::Error),
    #[error("invalid fingerprint {0:?}: expected 64 hex characters")]
    BadFingerprint(String),
    #[error("configuration failed: {0}")]
    Config(String),
}

/// Liveness settings for the QUIC connection. Defaults are DESIGN.md §4.2: heartbeats every 1 s,
/// dead after 5 s of silence. Spike B found `max_idle_timeout` plus `keep_alive_interval` to be
/// the dead-link knob.
#[derive(Clone, Copy, Debug)]
pub struct LivenessParams {
    pub keep_alive_interval: Duration,
    pub max_idle_timeout: Duration,
}

impl Default for LivenessParams {
    fn default() -> Self {
        Self {
            keep_alive_interval: Duration::from_secs(1),
            max_idle_timeout: Duration::from_secs(5),
        }
    }
}

impl LivenessParams {
    fn transport(&self) -> Result<Arc<TransportConfig>, AuthError> {
        let idle = IdleTimeout::try_from(self.max_idle_timeout)
            .map_err(|e| AuthError::Config(format!("max_idle_timeout: {e}")))?;
        let mut t = TransportConfig::default();
        t.max_idle_timeout(Some(idle));
        t.keep_alive_interval(Some(self.keep_alive_interval));
        Ok(Arc::new(t))
    }
}

/// The quinn server config for the daemon: presents `identity`, requires a client key from
/// `registry`.
///
/// TLS 1.3 only, no session tickets or 0-RTT. Resumption is off so that every connection runs
/// the full client check and a revoked key cannot come back through a cached session.
pub fn server_config(
    identity: &DaemonIdentity,
    registry: &KeyRegistry,
    liveness: LivenessParams,
) -> Result<quinn::ServerConfig, AuthError> {
    let tls = server_tls(
        SingleCertAndKey::from(identity.certified_key()),
        Arc::new(RegisteredClientVerifier::new(registry.clone())),
    )?;
    finish_server(tls, liveness)
}

/// The same, with a caller-supplied rustls config. Public so tests can build deliberately broken
/// servers; production code uses [`server_config`].
pub fn server_config_from_tls(
    tls: rustls::ServerConfig,
    liveness: LivenessParams,
) -> Result<quinn::ServerConfig, AuthError> {
    finish_server(tls, liveness)
}

fn finish_server(
    tls: rustls::ServerConfig,
    liveness: LivenessParams,
) -> Result<quinn::ServerConfig, AuthError> {
    let quic = QuicServerConfig::try_from(tls)
        .map_err(|e| AuthError::Config(format!("not usable for QUIC: {e}")))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    cfg.transport_config(liveness.transport()?);
    Ok(cfg)
}

/// The rustls server config behind [`server_config`], for callers (tests) that need to substitute
/// the certificate resolver.
pub fn server_tls(
    resolver: SingleCertAndKey,
    client_verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<rustls::ServerConfig, AuthError> {
    let mut tls =
        rustls::ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(AuthError::Tls)?
            .with_client_cert_verifier(client_verifier)
            .with_cert_resolver(Arc::new(resolver));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.session_storage = Arc::new(NoServerSessionStorage {});
    tls.send_tls13_tickets = 0;
    Ok(tls)
}

/// The quinn client config: trusts only the certificate whose SHA-256 is `pin`, and presents
/// `key` as an RFC 7250 raw public key.
///
/// Connect with [`SERVER_NAME`]. `connect().await` succeeding does NOT mean the daemon accepted
/// the client key: in TLS 1.3 the client finishes first, and the server's verdict arrives as a
/// connection close. Treat the connection as authenticated only after the first reply from the
/// daemon.
pub fn client_config(
    pin: Fingerprint,
    key: &ClientKey,
    liveness: LivenessParams,
) -> Result<quinn::ClientConfig, AuthError> {
    let tls = client_tls(Arc::new(PinnedServerVerifier::new(pin)), Some(key))?;
    let quic = QuicClientConfig::try_from(tls)
        .map_err(|e| AuthError::Config(format!("not usable for QUIC: {e}")))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(liveness.transport()?);
    Ok(cfg)
}

/// The rustls client config behind [`client_config`]. `key: None` presents no client
/// certificate at all (for tests of the server's refusal).
pub fn client_tls(
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    key: Option<&ClientKey>,
) -> Result<rustls::ClientConfig, AuthError> {
    let builder =
        rustls::ClientConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(AuthError::Tls)?
            .dangerous()
            .with_custom_certificate_verifier(verifier);
    let mut tls = match key {
        Some(key) => builder.with_client_cert_resolver(Arc::new(
            AlwaysResolvesClientRawPublicKeys::new(Arc::new(key.certified_key())),
        )),
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.resumption = Resumption::disabled();
    tls.enable_early_data = false;
    Ok(tls)
}

/// The client key a connection authenticated with, on the server side. `None` on a client-side
/// connection, or if the identity is not in the expected form.
///
/// The daemon uses this to find live connections to close when a key is revoked.
pub fn peer_key(conn: &quinn::Connection) -> Option<PublicKey> {
    let chain = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    match chain.as_slice() {
        [spki] => Some(PublicKey::from_spki_der(spki.as_ref())),
        _ => None,
    }
}
