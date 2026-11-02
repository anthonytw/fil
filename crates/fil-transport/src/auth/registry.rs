use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use super::PublicKey;

/// The set of client public keys the daemon currently accepts. Cheap to clone; clones share state.
///
/// Registered by the bootstrap (over the pipe), revoked at detach and takeover. The server's
/// [`RegisteredClientVerifier`](super::RegisteredClientVerifier) consults it on every handshake,
/// so [`revoke`](Self::revoke) blocks new handshakes immediately. It does not touch connections
/// that already completed their handshake: the daemon closes those itself, using
/// [`peer_key`](super::peer_key) to find the ones that authenticated with the revoked key.
#[derive(Clone, Default, Debug)]
pub struct KeyRegistry {
    keys: Arc<Mutex<HashSet<PublicKey>>>,
}

impl KeyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<PublicKey>> {
        // A poisoned lock still holds a consistent set (every critical section is one call).
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns `true` if the key was not already registered.
    pub fn register(&self, key: PublicKey) -> bool {
        self.lock().insert(key)
    }

    /// Returns `true` if the key was registered.
    pub fn revoke(&self, key: &PublicKey) -> bool {
        self.lock().remove(key)
    }

    pub fn contains(&self, key: &PublicKey) -> bool {
        self.lock().contains(key)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_lookup_revoke() {
        let reg = KeyRegistry::new();
        let k = PublicKey::from_spki_der(vec![1, 2, 3]);
        assert!(!reg.contains(&k));
        assert!(reg.register(k.clone()));
        assert!(!reg.register(k.clone()));
        assert!(reg.clone().contains(&k), "clones share state");
        assert!(reg.revoke(&k));
        assert!(!reg.revoke(&k));
        assert!(reg.is_empty());
    }
}
