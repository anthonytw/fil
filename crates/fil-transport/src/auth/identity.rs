//! Key material: the daemon's certificate and the client's per-attach key.
//!
//! Everything here lives in memory only. Nothing is written to disk (DESIGN.md §11, principle 3).

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use rustls::crypto::ring as provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SigningKey};

use super::AuthError;

/// A SHA-256 digest of a certificate's DER encoding. This is what the pipe carries from the
/// daemon to the client, and what the client pins.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// The fingerprint of a DER-encoded certificate.
    pub fn of_certificate(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        Self(out)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Lowercase hex, 64 characters.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&fil_proto::text::hex(&self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = AuthError;

    /// Parses 64 hex characters, either case.
    fn from_str(s: &str) -> Result<Self, AuthError> {
        let bad = || AuthError::BadFingerprint(s.chars().take(80).collect());
        if s.len() != 64 || !s.is_ascii() {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        Ok(Self(out))
    }
}

/// A public key as DER `SubjectPublicKeyInfo`. The daemon's registry is a set of these, and the
/// client sends one over the pipe. Compared byte for byte, so a differently encoded SPKI for the
/// same key does not match (fail closed).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PublicKey(Vec<u8>);

impl PublicKey {
    pub fn from_spki_der(der: impl Into<Vec<u8>>) -> Self {
        Self(der.into())
    }

    pub fn as_spki_der(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short, log-safe identifier: the first 8 bytes of the SPKI's SHA-256.
        let d = ring::digest::digest(&ring::digest::SHA256, &self.0);
        write!(f, "PublicKey(")?;
        write!(f, "{})", fil_proto::text::hex(&d.as_ref()[..8]))
    }
}

/// Generate a fresh ECDSA P-256 key (rcgen with ring) and load it through the rustls ring
/// provider. Returns the PKCS#8 bytes and the signing key.
fn generate_signing_key() -> Result<(PrivatePkcs8KeyDer<'static>, Arc<dyn SigningKey>), AuthError> {
    let pair = rcgen::KeyPair::generate().map_err(AuthError::KeyGen)?;
    let pkcs8 = PrivatePkcs8KeyDer::from(pair.serialize_der());
    let key = provider::default_provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(pkcs8.clone_key()))
        .map_err(AuthError::Tls)?;
    Ok((pkcs8, key))
}

/// The daemon's identity for one run: a self-signed certificate and its key, made at start.
///
/// The certificate carries a placeholder name only so it is well formed. Clients never check it
/// (see [`PinnedServerVerifier`](super::PinnedServerVerifier)).
pub struct DaemonIdentity {
    cert: CertificateDer<'static>,
    key: Arc<dyn SigningKey>,
    fingerprint: Fingerprint,
}

impl DaemonIdentity {
    pub fn generate() -> Result<Self, AuthError> {
        let pair = rcgen::KeyPair::generate().map_err(AuthError::KeyGen)?;
        let params = rcgen::CertificateParams::new(vec!["fil.invalid".to_owned()])
            .map_err(AuthError::KeyGen)?;
        let cert = params.self_signed(&pair).map_err(AuthError::KeyGen)?;
        let key = provider::default_provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                pair.serialize_der(),
            )))
            .map_err(AuthError::Tls)?;
        let cert = cert.der().clone();
        let fingerprint = Fingerprint::of_certificate(cert.as_ref());
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    /// The value the bootstrap sends over the pipe.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn certificate_der(&self) -> &CertificateDer<'static> {
        &self.cert
    }

    pub(super) fn certified_key(&self) -> CertifiedKey {
        CertifiedKey::new(vec![self.cert.clone()], self.key.clone())
    }
}

impl fmt::Debug for DaemonIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DaemonIdentity({})", self.fingerprint)
    }
}

/// A client's per-attach key pair. Generated in memory; only [`ClientKey::public_key`] leaves the
/// process (over the pipe). Dropped at detach.
pub struct ClientKey {
    public: PublicKey,
    key: Arc<dyn SigningKey>,
}

impl ClientKey {
    pub fn generate() -> Result<Self, AuthError> {
        let (_pkcs8, key) = generate_signing_key()?;
        let spki = key
            .public_key()
            .ok_or_else(|| AuthError::Config("signing key exposes no public key".into()))?;
        Ok(Self {
            public: PublicKey::from_spki_der(spki.as_ref()),
            key,
        })
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    /// RFC 7250 raw public key: the "certificate" is the bare SPKI.
    pub(super) fn certified_key(&self) -> CertifiedKey {
        CertifiedKey::new(
            vec![CertificateDer::from(self.public.as_spki_der().to_vec())],
            self.key.clone(),
        )
    }
}

impl fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientKey({:?})", self.public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_hex_round_trip() {
        let fp = Fingerprint::of_certificate(b"abc");
        // SHA-256("abc"), a published test vector.
        assert_eq!(
            fp.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(fp.to_string().parse::<Fingerprint>().unwrap(), fp);
        assert_eq!(
            fp.to_string()
                .to_uppercase()
                .parse::<Fingerprint>()
                .unwrap(),
            fp
        );
        assert!("abc".parse::<Fingerprint>().is_err());
        assert!("zz".repeat(32).parse::<Fingerprint>().is_err());
        assert!("é".repeat(32).parse::<Fingerprint>().is_err());
    }

    #[test]
    fn generated_keys_are_distinct() {
        let a = ClientKey::generate().unwrap();
        let b = ClientKey::generate().unwrap();
        assert_ne!(a.public_key(), b.public_key());
        let d1 = DaemonIdentity::generate().unwrap();
        let d2 = DaemonIdentity::generate().unwrap();
        assert_ne!(d1.fingerprint(), d2.fingerprint());
    }
}
