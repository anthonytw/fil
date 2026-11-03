//! The forward table's rules (DESIGN.md section 7.2), as a pure value the daemon owns.
//!
//! - A spec's identity is `(kind, bind, port)`, except that port 0 (an ephemeral `-R`) has
//!   none: each is a listener of its own, as with `ssh -R 0`, and never clashes.
//! - An add equal to an entry is answered with that entry (an equal add from the client makes
//!   a management-origin entry the client's own).
//! - An add from the attached client with the identity of an entry and another target
//!   **replaces** it under a new id (the old entry's open channels run until they close). From a
//!   management request it is a `Conflict`: a process on the remote must not redirect the user's
//!   own forwards.
//! - A management request (`Origin::Request`, any process of the user's uid) may not bind `-L`
//!   or `-D` beyond loopback (that binds wide on the laptop), nor aim `-R` at a non-loopback
//!   target (a pivot into the laptop's network): `NotPermitted`. This is for the user's
//!   experience and the daemon's honesty; the client's own check is the security boundary.
//! - `-R` beyond loopback is refused from a management request (it would expose a port of the
//!   remote host on a request any local process can make). From the attached client's own CLI
//!   or config it is the host's call: the daemon honours it where server policy allows it
//!   (`GatewayPorts`, DESIGN.md section 11.2; OD-7, settled by the owner on 2026-11-03).
//! - At most [`MAX_FORWARDS`] entries. Ids are never reused.

use crate::limits::MAX_FORWARDS;
use crate::mux::wire::{ForwardId, ForwardKind, ForwardRefusal, ForwardSpec, Origin};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: ForwardId,
    pub spec: ForwardSpec,
    pub origin: Origin,
}

/// What an add did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// The same spec was already there: answer with that entry.
    Existing(ForwardId),
    /// A new entry.
    New(ForwardId),
    /// A new entry that took the place of `old` (same identity, another target). `old`'s
    /// listener closes; its open channels run until they close.
    Replaced { id: ForwardId, old: ForwardId },
}

impl Added {
    pub fn id(self) -> ForwardId {
        match self {
            Added::Existing(id) | Added::New(id) | Added::Replaced { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ForwardTable {
    entries: Vec<Entry>,
    next: u32,
}

impl ForwardTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `spec` may be added by `origin` at all, whatever the table holds.
    pub fn permitted(spec: &ForwardSpec, origin: Origin) -> Result<(), ForwardRefusal> {
        let target_loopback = spec.target().is_none_or(|t| t.host.is_loopback());
        match spec.kind() {
            // OD-7: a wide -R bind only from the client's own CLI or config (and then where server
            // policy allows it, which the daemon checks).
            ForwardKind::Remote
                if origin == Origin::Request && !(target_loopback && spec.bind().is_loopback()) =>
            {
                Err(ForwardRefusal::NotPermitted)
            }
            ForwardKind::Local | ForwardKind::Dynamic
                if origin == Origin::Request && !spec.bind().is_loopback() =>
            {
                Err(ForwardRefusal::NotPermitted)
            }
            _ => Ok(()),
        }
    }

    pub fn add(&mut self, spec: ForwardSpec, origin: Origin) -> Result<Added, ForwardRefusal> {
        Self::permitted(&spec, origin)?;
        if let Some(e) = self.entries.iter_mut().find(|e| e.spec == spec) {
            if origin == Origin::Client {
                e.origin = Origin::Client;
            }
            return Ok(Added::Existing(e.id));
        }
        let clash = match spec.port() {
            0 => None,
            _ => self
                .entries
                .iter()
                .position(|e| e.spec.identity() == spec.identity()),
        };
        if clash.is_some() && origin == Origin::Request {
            return Err(ForwardRefusal::Conflict);
        }
        if clash.is_none() && self.entries.len() >= MAX_FORWARDS {
            return Err(ForwardRefusal::TooMany);
        }
        let id = self.allocate()?;
        let entry = Entry { id, spec, origin };
        Ok(match clash {
            Some(i) => {
                let old = std::mem::replace(&mut self.entries[i], entry).id;
                Added::Replaced { id, old }
            }
            None => {
                self.entries.push(entry);
                Added::New(id)
            }
        })
    }

    fn allocate(&mut self) -> Result<ForwardId, ForwardRefusal> {
        let id = ForwardId(self.next);
        self.next = self.next.checked_add(1).ok_or(ForwardRefusal::TooMany)?;
        Ok(id)
    }

    pub fn remove(&mut self, id: ForwardId) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(i))
    }

    /// `fil forward --rm [KIND] PORT`: the one entry with that port (the actual bound port is the
    /// caller's to match for `-R 0`), of that kind if given.
    pub fn find_port(
        &self,
        kind: Option<ForwardKind>,
        port: u16,
    ) -> Result<ForwardId, ForwardRefusal> {
        let mut hits = self
            .entries
            .iter()
            .filter(|e| e.spec.port() == port && kind.is_none_or(|k| e.spec.kind() == k));
        let first = hits.next().ok_or(ForwardRefusal::NoSuchForward)?;
        if hits.next().is_some() {
            return Err(ForwardRefusal::Ambiguous);
        }
        Ok(first.id)
    }

    pub fn get(&self, id: ForwardId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::wire::{BindAddr, Target, TargetHost};
    use std::num::NonZeroU16;

    fn t(host: &str, port: u16) -> Option<Target> {
        let host = match host.parse() {
            Ok(ip) => TargetHost::Ip(ip),
            Err(_) => TargetHost::Name(host.parse().unwrap()),
        };
        Some(Target {
            host,
            port: NonZeroU16::new(port).unwrap(),
        })
    }

    fn local(bind: BindAddr, port: u16, target: Option<Target>) -> ForwardSpec {
        ForwardSpec::new(ForwardKind::Local, bind, port, target).unwrap()
    }

    fn remote(bind: BindAddr, port: u16, target: Option<Target>) -> ForwardSpec {
        ForwardSpec::new(ForwardKind::Remote, bind, port, target).unwrap()
    }

    #[test]
    fn equal_is_existing_conflict_replaces_for_the_client_only() {
        let mut tab = ForwardTable::new();
        let a = local(BindAddr::Loopback, 8080, t("localhost", 80));
        let id = tab.add(a.clone(), Origin::Client).unwrap();
        assert_eq!(id, Added::New(ForwardId(0)));
        assert_eq!(
            tab.add(a.clone(), Origin::Request),
            Ok(Added::Existing(ForwardId(0)))
        );
        let b = local(BindAddr::Loopback, 8080, t("localhost", 81));
        assert_eq!(
            tab.add(b.clone(), Origin::Request),
            Err(ForwardRefusal::Conflict)
        );
        assert_eq!(
            tab.add(b.clone(), Origin::Client),
            Ok(Added::Replaced {
                id: ForwardId(1),
                old: ForwardId(0)
            })
        );
        assert_eq!(tab.len(), 1);
        assert_eq!(tab.get(ForwardId(1)).unwrap().spec, b);
        // Another bind is another identity.
        let c = local(
            BindAddr::Ip([127, 0, 0, 2].into()),
            8080,
            t("localhost", 80),
        );
        assert_eq!(tab.add(c, Origin::Client), Ok(Added::New(ForwardId(2))));
    }

    /// Every `-R 0` is a listener of its own (ssh allows any number); an equal one still dedups.
    #[test]
    fn ephemeral_remote_forwards_never_clash() {
        let mut tab = ForwardTable::new();
        let a = remote(BindAddr::Loopback, 0, t("localhost", 80));
        let b = remote(BindAddr::Loopback, 0, t("localhost", 81));
        assert_eq!(
            tab.add(a.clone(), Origin::Client),
            Ok(Added::New(ForwardId(0)))
        );
        assert_eq!(
            tab.add(b.clone(), Origin::Client),
            Ok(Added::New(ForwardId(1)))
        );
        assert_eq!(
            tab.add(b, Origin::Request),
            Ok(Added::Existing(ForwardId(1)))
        );
        assert_eq!(
            tab.add(a, Origin::Client),
            Ok(Added::Existing(ForwardId(0)))
        );
        let c = remote(BindAddr::Loopback, 0, t("localhost", 82));
        assert_eq!(tab.add(c, Origin::Request), Ok(Added::New(ForwardId(2))));
        assert_eq!(tab.len(), 3);
    }

    /// The client's own add of an entry management made takes it over.
    #[test]
    fn an_equal_client_add_makes_the_entry_the_clients() {
        let mut tab = ForwardTable::new();
        let s = local(BindAddr::Loopback, 5173, t("localhost", 5173));
        tab.add(s.clone(), Origin::Request).unwrap();
        assert_eq!(tab.get(ForwardId(0)).unwrap().origin, Origin::Request);
        assert_eq!(
            tab.add(s.clone(), Origin::Client),
            Ok(Added::Existing(ForwardId(0)))
        );
        assert_eq!(tab.get(ForwardId(0)).unwrap().origin, Origin::Client);
        // ...and a later management add does not take it back.
        tab.add(s, Origin::Request).unwrap();
        assert_eq!(tab.get(ForwardId(0)).unwrap().origin, Origin::Client);
    }

    #[test]
    fn management_may_not_bind_wide_or_aim_remote_off_host() {
        let mut tab = ForwardTable::new();
        let wide_l = local(BindAddr::Any, 8080, t("localhost", 80));
        assert_eq!(
            tab.add(wide_l.clone(), Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        assert!(tab.add(wide_l, Origin::Client).is_ok());
        let wide_d = ForwardSpec::new(
            ForwardKind::Dynamic,
            BindAddr::Ip([0, 0, 0, 0].into()),
            1080,
            None,
        )
        .unwrap();
        assert_eq!(
            tab.add(wide_d, Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        let pivot = remote(BindAddr::Loopback, 19001, t("192.0.2.10", 22));
        assert_eq!(
            tab.add(pivot.clone(), Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        assert!(tab.add(pivot, Origin::Client).is_ok());
        let named = remote(BindAddr::Loopback, 19002, t("db.example.com", 5432));
        assert_eq!(
            tab.add(named, Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        let fine = remote(BindAddr::Loopback, 9000, t("localhost", 9000));
        assert!(tab.add(fine, Origin::Request).is_ok());
        // A -L from management to anywhere is fine: it connects out from the remote, where the
        // requester already is; what matters is the laptop-side bind.
        let out = local(BindAddr::Loopback, 5432, t("db.example.com", 5432));
        assert!(tab.add(out, Origin::Request).is_ok());
    }

    #[test]
    fn wide_remote_bind_only_from_the_client() {
        // OD-7: the client's own wide -R is the host's call (server policy, checked by the
        // daemon); a management request never binds one.
        let mut tab = ForwardTable::new();
        let s = remote(BindAddr::Any, 9000, t("localhost", 9000));
        assert_eq!(
            tab.add(s.clone(), Origin::Request),
            Err(ForwardRefusal::NotPermitted)
        );
        assert!(tab.add(s, Origin::Client).is_ok());
    }

    #[test]
    fn the_table_is_capped_and_ids_are_never_reused() {
        let mut tab = ForwardTable::new();
        for p in 1..=MAX_FORWARDS as u16 {
            tab.add(
                local(BindAddr::Loopback, p, t("localhost", p)),
                Origin::Client,
            )
            .unwrap();
        }
        let more = local(BindAddr::Loopback, 999, t("localhost", 1));
        assert_eq!(
            tab.add(more.clone(), Origin::Client),
            Err(ForwardRefusal::TooMany)
        );
        // A replacement does not grow the table, so it is allowed when full.
        let repl = local(BindAddr::Loopback, 1, t("localhost", 2));
        assert!(matches!(
            tab.add(repl, Origin::Client),
            Ok(Added::Replaced { .. })
        ));
        let gone = tab.remove(ForwardId(5)).unwrap();
        assert_eq!(gone.id, ForwardId(5));
        assert_eq!(
            tab.add(more, Origin::Client),
            Ok(Added::New(ForwardId(MAX_FORWARDS as u32 + 1)))
        );
        assert!(tab.remove(ForwardId(5)).is_none());
    }

    #[test]
    fn removal_by_port() {
        let mut tab = ForwardTable::new();
        tab.add(
            local(BindAddr::Loopback, 5173, t("localhost", 5173)),
            Origin::Client,
        )
        .unwrap();
        tab.add(
            remote(BindAddr::Loopback, 5173, t("localhost", 5173)),
            Origin::Client,
        )
        .unwrap();
        assert_eq!(tab.find_port(None, 5173), Err(ForwardRefusal::Ambiguous));
        assert_eq!(
            tab.find_port(Some(ForwardKind::Remote), 5173),
            Ok(ForwardId(1))
        );
        assert_eq!(
            tab.find_port(Some(ForwardKind::Dynamic), 5173),
            Err(ForwardRefusal::NoSuchForward)
        );
        assert_eq!(tab.find_port(None, 1), Err(ForwardRefusal::NoSuchForward));
    }
}
