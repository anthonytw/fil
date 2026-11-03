//! QUIC authentication over `fil-netsim`'s simulated datagram link.
//!
//! All tests run on paused time, so they finish in real milliseconds.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use fil_netsim::Net;
use fil_transport::auth::{
    self, ClientKey, DaemonIdentity, Fingerprint, KeyRegistry, LivenessParams,
    PinnedServerVerifier, PublicKey, RegisteredClientVerifier, SERVER_NAME,
};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, TokioRuntime};
use rustls::client::AlwaysResolvesClientRawPublicKeys;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use tokio::sync::mpsc;

/// What the server saw for one incoming connection.
#[derive(Debug)]
enum Seen {
    /// The handshake completed; the key the server authenticated.
    Accepted(Option<PublicKey>),
    /// The handshake failed on the server side.
    Failed,
}

struct Server {
    addr: SocketAddr,
    seen: mpsc::UnboundedReceiver<Seen>,
    /// Live accepted connections, so tests can inspect them.
    conns: mpsc::UnboundedReceiver<Connection>,
}

fn spawn_server(net: &Net, addr: &str, cfg: quinn::ServerConfig) -> Server {
    let addr: SocketAddr = addr.parse().unwrap();
    let sock = net.bind(addr).unwrap();
    let ep = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(cfg),
        sock,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let (conn_tx, conns) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let seen_tx = seen_tx.clone();
            let conn_tx = conn_tx.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        let _ = seen_tx.send(Seen::Accepted(auth::peer_key(&conn)));
                        let _ = conn_tx.send(conn.clone());
                        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                            tokio::spawn(async move {
                                if let Ok(data) = recv.read_to_end(1 << 20).await {
                                    let _ = send.write_all(&data).await;
                                    let _ = send.finish();
                                }
                            });
                        }
                    }
                    Err(_) => {
                        let _ = seen_tx.send(Seen::Failed);
                    }
                }
            });
        }
    });
    Server { addr, seen, conns }
}

fn client_endpoint(net: &Net, addr: &str) -> Endpoint {
    let sock = net.bind(addr.parse().unwrap()).unwrap();
    Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        sock,
        Arc::new(TokioRuntime),
    )
    .unwrap()
}

/// Connect and do one echo. The echo matters: in TLS 1.3 the client's `connect()` can succeed
/// before the server has judged the client key, so only a round trip shows real acceptance.
async fn try_echo(ep: &Endpoint, server: SocketAddr, cfg: ClientConfig) -> Result<(), String> {
    let work = async {
        let conn = ep
            .connect_with(cfg, server, SERVER_NAME)
            .map_err(|e| e.to_string())?
            .await
            .map_err(|e| e.to_string())?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(b"hello fil")
            .await
            .map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())?;
        let back = recv.read_to_end(1024).await.map_err(|e| e.to_string())?;
        assert_eq!(back, b"hello fil");
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .unwrap_or_else(|_| Err("test timeout".into()))
}

struct Fixture {
    net: Net,
    identity: DaemonIdentity,
    registry: KeyRegistry,
    server: Server,
    client_ep: Endpoint,
}

fn fixture() -> Fixture {
    let net = Net::new(7);
    let identity = DaemonIdentity::generate().unwrap();
    let registry = KeyRegistry::new();
    let cfg = auth::server_config(&identity, &registry, LivenessParams::default()).unwrap();
    let server = spawn_server(&net, "192.0.2.10:4433", cfg);
    let client_ep = client_endpoint(&net, "192.0.2.20:5000");
    Fixture {
        net,
        identity,
        registry,
        server,
        client_ep,
    }
}

impl Fixture {
    fn client_cfg(&self, key: &ClientKey) -> ClientConfig {
        auth::client_config(self.identity.fingerprint(), key, LivenessParams::default()).unwrap()
    }
}

#[tokio::test(start_paused = true)]
async fn pinned_and_registered_connects_and_echoes() {
    let mut f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    try_echo(&f.client_ep, f.server.addr, f.client_cfg(&key))
        .await
        .unwrap();

    // The server reports which key the connection authenticated with.
    match f.server.seen.recv().await.unwrap() {
        Seen::Accepted(Some(k)) => assert_eq!(&k, key.public_key()),
        other => panic!("unexpected {other:?}"),
    }
    let _ = &f.net;
}

#[tokio::test(start_paused = true)]
async fn wrong_pin_fails() {
    let mut f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    let wrong = Fingerprint::of_certificate(b"not the daemon");
    let cfg = auth::client_config(wrong, &key, LivenessParams::default()).unwrap();
    let err = try_echo(&f.client_ep, f.server.addr, cfg)
        .await
        .unwrap_err();
    assert!(err.contains("the pinned fingerprint"), "{err}");
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));
}

#[tokio::test(start_paused = true)]
async fn man_in_the_middle_with_another_certificate_fails() {
    let f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    // The attacker runs a full, valid server of its own at the address the client dials, and
    // happily accepts any key. Its certificate is not the pinned one.
    let mitm_identity = DaemonIdentity::generate().unwrap();
    let open_registry = KeyRegistry::new();
    open_registry.register(key.public_key().clone());
    let cfg =
        auth::server_config(&mitm_identity, &open_registry, LivenessParams::default()).unwrap();
    let mut mitm = spawn_server(&f.net, "192.0.2.66:4433", cfg);

    let err = try_echo(&f.client_ep, mitm.addr, f.client_cfg(&key))
        .await
        .unwrap_err();
    assert!(err.contains("the pinned fingerprint"), "{err}");
    // The client aborted the handshake, so the attacker never got an authenticated connection
    // and so never learned anything it could replay.
    assert!(matches!(mitm.seen.recv().await.unwrap(), Seen::Failed));
}

#[tokio::test(start_paused = true)]
async fn unregistered_client_key_is_rejected() {
    let mut f = fixture();
    let registered = ClientKey::generate().unwrap();
    f.registry.register(registered.public_key().clone());
    let stranger = ClientKey::generate().unwrap();

    let err = try_echo(&f.client_ep, f.server.addr, f.client_cfg(&stranger))
        .await
        .unwrap_err();
    assert!(err.contains("client key is not registered"), "{err}");
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));
}

#[tokio::test(start_paused = true)]
async fn revoked_key_cannot_start_a_new_handshake() {
    let mut f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());
    let cfg = f.client_cfg(&key);

    try_echo(&f.client_ep, f.server.addr, cfg.clone())
        .await
        .unwrap();
    assert!(matches!(
        f.server.seen.recv().await.unwrap(),
        Seen::Accepted(_)
    ));
    // The daemon can find the live connection by key and close it.
    let live = f.server.conns.recv().await.unwrap();
    assert_eq!(auth::peer_key(&live).as_ref(), Some(key.public_key()));

    assert!(f.registry.revoke(key.public_key()));
    // Same client config, same endpoint: no resumption shortcut.
    let err = try_echo(&f.client_ep, f.server.addr, cfg.clone())
        .await
        .unwrap_err();
    assert!(err.contains("client key is not registered"), "{err}");
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));

    // Registering the key again restores access.
    f.registry.register(key.public_key().clone());
    try_echo(&f.client_ep, f.server.addr, cfg).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn missing_client_certificate_is_rejected() {
    let mut f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    let tls = auth::client_tls(
        Arc::new(PinnedServerVerifier::new(f.identity.fingerprint())),
        None,
    )
    .unwrap();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let cfg = ClientConfig::new(Arc::new(quic));

    // The server insists on a raw public key, so a client with no key at all fails negotiation.
    let err = try_echo(&f.client_ep, f.server.addr, cfg)
        .await
        .unwrap_err();
    assert!(err.contains("IncorrectCertificateTypeExtension"), "{err}");
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));
}

/// The server presents the real certificate (so the fingerprint matches) but signs the
/// handshake with a different key. Only the signature check can catch this.
#[tokio::test(start_paused = true)]
async fn right_certificate_wrong_signing_key_fails() {
    let f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    let other = rcgen::KeyPair::generate().unwrap();
    let other_key = rustls::crypto::ring::default_provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            other.serialize_der(),
        )))
        .unwrap();
    let forged = CertifiedKey::new(vec![f.identity.certificate_der().clone()], other_key);
    let tls = auth::server_tls(
        SingleCertAndKey::from(forged),
        Arc::new(RegisteredClientVerifier::new(f.registry.clone())),
    )
    .unwrap();
    let cfg = auth::server_config_from_tls(tls, LivenessParams::default()).unwrap();
    let impostor = spawn_server(&f.net, "192.0.2.77:4433", cfg);

    // Pin matches the certificate the impostor sends...
    let err = try_echo(&f.client_ep, impostor.addr, f.client_cfg(&key))
        .await
        .unwrap_err();
    // ...so this fails on the signature, not the pin.
    assert!(
        err.contains("BadSignature"),
        "expected a signature failure, got: {err}"
    );
}

/// Someone who has only seen a registered public key (it crosses the pipe, and shows up in
/// logs) claims it but signs with their own key.
#[tokio::test(start_paused = true)]
async fn registered_public_key_without_its_private_key_is_rejected() {
    let mut f = fixture();
    let victim = ClientKey::generate().unwrap();
    f.registry.register(victim.public_key().clone());

    let attacker = rcgen::KeyPair::generate().unwrap();
    let attacker_key = rustls::crypto::ring::default_provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            attacker.serialize_der(),
        )))
        .unwrap();
    let spoofed = CertifiedKey::new(
        vec![CertificateDer::from(
            victim.public_key().as_spki_der().to_vec(),
        )],
        attacker_key,
    );
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier::new(
        f.identity.fingerprint(),
    )))
    .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(Arc::new(
        spoofed,
    ))));
    tls.alpn_protocols = vec![auth::alpn()];
    let cfg = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));

    let err = try_echo(&f.client_ep, f.server.addr, cfg)
        .await
        .unwrap_err();
    assert!(err.contains("BadSignature"), "{err}");
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));
}

/// A client of another wire revision is refused by the handshake itself: the ALPN names the
/// revision (`fil/<major>.<revision>`), so no stream is opened and no message decoded. The
/// matching client still works against the same server.
#[tokio::test(start_paused = true)]
async fn another_wire_revision_fails_the_handshake() {
    let mut f = fixture();
    let key = ClientKey::generate().unwrap();
    f.registry.register(key.public_key().clone());

    let ours = fil_proto::version::wire_revision();
    assert_eq!(auth::alpn(), auth::alpn_for(ours));
    assert_ne!(auth::alpn_for(ours), auth::alpn_for(ours + 1));

    let mut tls = auth::client_tls(
        Arc::new(PinnedServerVerifier::new(f.identity.fingerprint())),
        Some(&key),
    )
    .unwrap();
    tls.alpn_protocols = vec![auth::alpn_for(ours + 1)];
    let cfg = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));
    let err = f
        .client_ep
        .connect_with(cfg, f.server.addr, SERVER_NAME)
        .unwrap()
        .await
        .map(|_| ())
        .unwrap_err();
    // TLS alert no_application_protocol (120) as a QUIC crypto error (0x100 + 120): the code
    // `QuicLink::connect_on` turns into "the daemon speaks a different wire revision".
    assert!(
        matches!(&err, quinn::ConnectionError::ConnectionClosed(c) if u64::from(c.error_code) == 0x178),
        "expected a no-application-protocol failure, got: {err:?}"
    );
    assert!(matches!(f.server.seen.recv().await.unwrap(), Seen::Failed));

    // The same server, a client of its own revision: fine.
    try_echo(&f.client_ep, f.server.addr, f.client_cfg(&key))
        .await
        .unwrap();
}
