use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;
// tokio's clock, so the listener's sweep and tests on paused time agree on "how long ago".
use tokio::time::Instant;

use super::PublicKey;

/// The longest any revocation is remembered, whoever prunes it. Far beyond any backstop the
/// listener uses, it bounds `revoked_at` even when no listener runs (direct mode off).
const REVOCATION_MEMORY: Duration = Duration::from_secs(600);

/// The keys and the moments recent revocations happened.
#[derive(Debug, Default)]
struct State {
    keys: HashSet<PublicKey>,
    /// When each recently revoked key was removed: the listener closes the live connections of a
    /// revoked key a backstop delay after this (see `QuicListener`), not at once, so the layer
    /// above can still send its last message on them.
    revoked_at: HashMap<PublicKey, Instant>,
}

/// The set of client public keys the daemon currently accepts. Cheap to clone; clones share state.
///
/// Registered by the bootstrap (over the pipe), revoked at detach and takeover. The server's
/// [`RegisteredClientVerifier`](super::RegisteredClientVerifier) consults it on every handshake,
/// so [`revoke`](Self::revoke) blocks new handshakes immediately. It does not touch connections
/// that already completed their handshake: the listener closes those, a backstop delay after the
/// revocation (or at once through `QuicListener::revoke`), using [`peer_key`](super::peer_key) to
/// find the ones that authenticated with the revoked key.
#[derive(Clone, Debug)]
pub struct KeyRegistry {
    state: Arc<Mutex<State>>,
    /// Bumped after each revocation, so the listener's sweep wakes for one instead of polling.
    revocations: Arc<watch::Sender<()>>,
}

impl Default for KeyRegistry {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            revocations: Arc::new(watch::Sender::new(())),
        }
    }
}

impl KeyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Changes after every revocation that removed a key (`revoke`, `revoke_all`). A receiver
    /// marked seen before it reads the registry misses none (the listener's sweep).
    pub(crate) fn revocations(&self) -> watch::Receiver<()> {
        self.revocations.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock still holds a consistent state (every critical section is a few
        // plain map operations).
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns `true` if the key was not already registered.
    pub fn register(&self, key: PublicKey) -> bool {
        let mut st = self.lock();
        st.revoked_at.remove(&key);
        st.keys.insert(key)
    }

    /// Returns `true` if the key was registered. New handshakes with it fail from now on.
    pub fn revoke(&self, key: &PublicKey) -> bool {
        let mut st = self.lock();
        let was = st.keys.remove(key);
        if was {
            st.revoked_at.retain(|_, t| t.elapsed() < REVOCATION_MEMORY);
            st.revoked_at.insert(key.clone(), Instant::now());
        }
        drop(st);
        if was {
            self.revocations.send_replace(());
        }
        was
    }

    /// Revokes every key, as [`revoke`](Self::revoke) each: the daemon's QUIC port closed, so no
    /// key registered for it so far may authenticate on a port opened later. Returns how many.
    pub fn revoke_all(&self) -> usize {
        let mut st = self.lock();
        let keys: Vec<PublicKey> = st.keys.drain().collect();
        st.revoked_at.retain(|_, t| t.elapsed() < REVOCATION_MEMORY);
        let now = Instant::now();
        for k in &keys {
            st.revoked_at.insert(k.clone(), now);
        }
        drop(st);
        if !keys.is_empty() {
            self.revocations.send_replace(());
        }
        keys.len()
    }

    pub fn contains(&self, key: &PublicKey) -> bool {
        self.lock().keys.contains(key)
    }

    pub fn len(&self) -> usize {
        self.lock().keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().keys.is_empty()
    }

    /// How long ago `key` was revoked, if it was recently (and not registered again since).
    pub fn revoked_ago(&self, key: &PublicKey) -> Option<Duration> {
        self.lock().revoked_at.get(key).map(Instant::elapsed)
    }

    /// Forgets revocations older than `age` (the listener calls this; connections of keys
    /// revoked longer ago than its backstop are already closed).
    pub(crate) fn forget_revocations_older_than(&self, age: Duration) {
        self.lock().revoked_at.retain(|_, t| t.elapsed() < age);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revoke_all_revokes_every_key() {
        let reg = KeyRegistry::new();
        let (a, b) = (
            PublicKey::from_spki_der(vec![1]),
            PublicKey::from_spki_der(vec![2]),
        );
        reg.register(a.clone());
        reg.register(b.clone());
        assert_eq!(reg.revoke_all(), 2);
        assert!(reg.is_empty());
        assert!(reg.revoked_ago(&a).is_some() && reg.revoked_ago(&b).is_some());
    }

    #[test]
    fn register_lookup_revoke() {
        let reg = KeyRegistry::new();
        let k = PublicKey::from_spki_der(vec![1, 2, 3]);
        assert!(!reg.contains(&k));
        assert!(reg.register(k.clone()));
        assert!(!reg.register(k.clone()));
        assert!(reg.clone().contains(&k), "clones share state");
        assert!(reg.revoked_ago(&k).is_none());
        assert!(reg.revoke(&k));
        assert!(!reg.revoke(&k));
        assert!(reg.is_empty());
        assert!(
            reg.revoked_ago(&k).is_some(),
            "the revocation is remembered"
        );
        assert!(reg.register(k.clone()));
        assert!(reg.revoked_ago(&k).is_none(), "registering again clears it");
        assert!(reg.revoke(&k));
        reg.forget_revocations_older_than(Duration::ZERO);
        assert!(reg.revoked_ago(&k).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn old_revocations_are_forgotten_without_a_listener() {
        let reg = KeyRegistry::new();
        let old = PublicKey::from_spki_der(vec![1]);
        reg.register(old.clone());
        reg.revoke(&old);
        tokio::time::advance(REVOCATION_MEMORY + Duration::from_secs(1)).await;
        let new = PublicKey::from_spki_der(vec![2]);
        reg.register(new.clone());
        reg.revoke(&new);
        assert_eq!(reg.revoked_ago(&old), None, "pruned by the next revoke");
        assert!(reg.revoked_ago(&new).is_some());
        assert_eq!(reg.lock().revoked_at.len(), 1);
    }
}
