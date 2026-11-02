//! The epoch and takeover state machine for one session's subscriber (DESIGN.md section 5.2).
//!
//! Pure: no IO, no clock (the caller passes the time). Each call returns a value describing what
//! the daemon must now do ([`Effect`]); the machine never does it itself.
//!
//! Rules:
//! - A fresh attach (no resume state) bumps the epoch and becomes the subscriber. Any previous
//!   subscriber is told it was taken over, its key is revoked and its transport dropped.
//! - A resume with the current epoch is accepted and keeps the epoch.
//! - A resume with a stale epoch is refused with the takeover details. It never steals the
//!   session; to take it back the client attaches afresh.
//! - Detach bumps the epoch and revokes, so a late resume from the detached client is refused.

use crate::msg::{AttachRefusal, HostLabel, PublicKey, ResumeRequest, TakenOver, TermSize};

/// Identifies one subscriber across transports within an epoch. The daemon maps it to the live
/// connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscriber {
    pub id: SubscriberId,
    pub key: PublicKey,
    pub host: HostLabel,
    pub since_unix_ms: u64,
}

/// What the daemon must do as a result of a transition. Order matters: notify before dropping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Remove this key from the set the QUIC listener accepts.
    RevokeKey(PublicKey),
    /// Tell the old subscriber it was taken over (over whatever transport it still has).
    NotifyTakenOver {
        subscriber: SubscriberId,
        info: TakenOver,
    },
    /// Close the subscriber's transport.
    DropTransport { subscriber: SubscriberId },
    /// Resize the PTY (SIGWINCH) to the attaching client's size.
    ResizePty(TermSize),
}

/// How the accepted attach starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// A new subscriber: send a snapshot sized to its terminal
    /// ([`crate::msg::SnapshotReason::NewClient`]).
    Fresh,
    /// The same client reconnecting with the current epoch: feed `resume.seq` to
    /// [`crate::log::decide_start`] to choose between replay and snapshot.
    Resumed(ResumeRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachOutcome {
    Accepted {
        epoch: u64,
        subscriber: SubscriberId,
        mode: AttachMode,
        effects: Vec<Effect>,
    },
    Refused(AttachRefusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetachOutcome {
    Detached {
        epoch: u64,
        effects: Vec<Effect>,
    },
    /// Not the current subscriber (a stale client): nothing changes.
    Ignored,
}

/// An attach request, reduced to what the state machine needs.
#[derive(Clone, Debug)]
pub struct AttachInput {
    pub key: PublicKey,
    pub host: HostLabel,
    pub size: TermSize,
    pub resume: Option<ResumeRequest>,
    pub now_unix_ms: u64,
}

#[derive(Debug, Default)]
pub struct SessionEpoch {
    epoch: u64,
    current: Option<Subscriber>,
    next_id: u64,
}

impl SessionEpoch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn subscriber(&self) -> Option<&Subscriber> {
        self.current.as_ref()
    }

    /// Fencing check for traffic arriving on a connection that belongs to `epoch`.
    pub fn is_current(&self, epoch: u64) -> bool {
        self.current.is_some() && epoch == self.epoch
    }

    /// May this key open a direct connection to the session right now?
    pub fn key_authorized(&self, key: &PublicKey) -> bool {
        self.current.as_ref().is_some_and(|s| &s.key == key)
    }

    pub fn attach(&mut self, req: AttachInput) -> AttachOutcome {
        match req.resume {
            None => self.attach_fresh(req),
            Some(r) => self.attach_resume(req, r),
        }
    }

    fn attach_fresh(&mut self, req: AttachInput) -> AttachOutcome {
        self.epoch += 1;
        let sub = self.install(&req);
        let mut effects = Vec::new();
        if let Some(old) = self.current.replace(sub.clone()) {
            effects.push(Effect::NotifyTakenOver {
                subscriber: old.id,
                info: TakenOver {
                    by: req.host,
                    at_unix_ms: req.now_unix_ms,
                    epoch: self.epoch,
                },
            });
            effects.push(Effect::RevokeKey(old.key));
            effects.push(Effect::DropTransport { subscriber: old.id });
        }
        effects.push(Effect::ResizePty(req.size));
        AttachOutcome::Accepted {
            epoch: self.epoch,
            subscriber: sub.id,
            mode: AttachMode::Fresh,
            effects,
        }
    }

    fn attach_resume(&mut self, req: AttachInput, r: ResumeRequest) -> AttachOutcome {
        let refuse = |a| AttachOutcome::Refused(a);
        if r.epoch > self.epoch {
            return refuse(AttachRefusal::BadResume);
        }
        let Some(cur) = self.current.as_mut() else {
            return refuse(AttachRefusal::Detached);
        };
        if r.epoch < self.epoch {
            return refuse(AttachRefusal::TakenOver(TakenOver {
                by: cur.host.clone(),
                at_unix_ms: cur.since_unix_ms,
                epoch: self.epoch,
            }));
        }
        // The same client, on a new connection. Its old transport (if half-dead) must go.
        let mut effects = vec![Effect::DropTransport { subscriber: cur.id }];
        if cur.key != req.key {
            effects.push(Effect::RevokeKey(std::mem::replace(&mut cur.key, req.key)));
        }
        cur.host = req.host;
        effects.push(Effect::ResizePty(req.size));
        AttachOutcome::Accepted {
            epoch: self.epoch,
            subscriber: cur.id,
            mode: AttachMode::Resumed(r),
            effects,
        }
    }

    fn install(&mut self, req: &AttachInput) -> Subscriber {
        self.next_id += 1;
        Subscriber {
            id: SubscriberId(self.next_id),
            key: req.key.clone(),
            host: req.host.clone(),
            since_unix_ms: req.now_unix_ms,
        }
    }

    /// The subscriber of `epoch` detaches. Only the current subscriber can.
    pub fn detach(&mut self, epoch: u64) -> DetachOutcome {
        if !self.is_current(epoch) {
            return DetachOutcome::Ignored;
        }
        let old = self.current.take().expect("checked by is_current");
        self.epoch += 1;
        DetachOutcome::Detached {
            epoch: self.epoch,
            effects: vec![
                Effect::RevokeKey(old.key),
                Effect::DropTransport { subscriber: old.id },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> PublicKey {
        PublicKey::new(vec![b; 32]).unwrap()
    }
    fn input(k: u8, host: &str, resume: Option<ResumeRequest>, now: u64) -> AttachInput {
        AttachInput {
            key: key(k),
            host: host.parse().unwrap(),
            size: TermSize::new(80, 24).unwrap(),
            resume,
            now_unix_ms: now,
        }
    }
    fn accepted(o: AttachOutcome) -> (u64, SubscriberId, AttachMode, Vec<Effect>) {
        match o {
            AttachOutcome::Accepted {
                epoch,
                subscriber,
                mode,
                effects,
            } => (epoch, subscriber, mode, effects),
            other => panic!("expected accepted, got {other:?}"),
        }
    }
    fn resume(epoch: u64) -> Option<ResumeRequest> {
        Some(ResumeRequest { epoch, seq: 100 })
    }

    #[test]
    fn first_attach_starts_epoch_one() {
        let mut s = SessionEpoch::new();
        assert_eq!(s.epoch(), 0);
        assert!(!s.is_current(0));
        let (e, id, mode, fx) = accepted(s.attach(input(1, "laptop", None, 10)));
        assert_eq!((e, mode), (1, AttachMode::Fresh));
        assert_eq!(fx, vec![Effect::ResizePty(TermSize::new(80, 24).unwrap())]);
        assert!(s.is_current(1));
        assert!(s.key_authorized(&key(1)));
        assert_eq!(s.subscriber().unwrap().id, id);
    }

    #[test]
    fn takeover_revokes_notifies_and_bumps() {
        let mut s = SessionEpoch::new();
        let (_, old, _, _) = accepted(s.attach(input(1, "laptop", None, 10)));
        let (e, new, mode, fx) = accepted(s.attach(input(2, "desktop", None, 99)));
        assert_eq!((e, mode), (2, AttachMode::Fresh));
        assert_ne!(old, new);
        assert_eq!(
            fx,
            vec![
                Effect::NotifyTakenOver {
                    subscriber: old,
                    info: TakenOver {
                        by: "desktop".parse().unwrap(),
                        at_unix_ms: 99,
                        epoch: 2
                    },
                },
                Effect::RevokeKey(key(1)),
                Effect::DropTransport { subscriber: old },
                Effect::ResizePty(TermSize::new(80, 24).unwrap()),
            ]
        );
        assert!(!s.key_authorized(&key(1)));
        assert!(s.key_authorized(&key(2)));
        assert!(!s.is_current(1));
    }

    #[test]
    fn resume_with_current_epoch_is_accepted_and_keeps_the_epoch() {
        let mut s = SessionEpoch::new();
        let (_, id, _, _) = accepted(s.attach(input(1, "laptop", None, 10)));
        let r = resume(1);
        let (e, id2, mode, fx) = accepted(s.attach(input(1, "laptop", r, 20)));
        assert_eq!((e, id2), (1, id));
        assert_eq!(mode, AttachMode::Resumed(r.unwrap()));
        assert!(fx.contains(&Effect::DropTransport { subscriber: id }));
        assert!(
            !fx.iter().any(|f| matches!(f, Effect::RevokeKey(_))),
            "same key stays valid"
        );
        assert_eq!(s.epoch(), 1);
    }

    #[test]
    fn resume_with_a_new_key_swaps_and_revokes_the_old_one() {
        let mut s = SessionEpoch::new();
        accepted(s.attach(input(1, "laptop", None, 10)));
        let (_, _, _, fx) = accepted(s.attach(input(5, "laptop", resume(1), 20)));
        assert!(fx.contains(&Effect::RevokeKey(key(1))));
        assert!(s.key_authorized(&key(5)));
        assert!(!s.key_authorized(&key(1)));
    }

    #[test]
    fn stale_resume_is_refused_with_takeover_details_and_changes_nothing() {
        let mut s = SessionEpoch::new();
        accepted(s.attach(input(1, "laptop", None, 10)));
        accepted(s.attach(input(2, "desktop", None, 99)));
        let out = s.attach(input(1, "laptop", resume(1), 500));
        assert_eq!(
            out,
            AttachOutcome::Refused(AttachRefusal::TakenOver(TakenOver {
                by: "desktop".parse().unwrap(),
                at_unix_ms: 99,
                epoch: 2,
            }))
        );
        assert_eq!(s.epoch(), 2);
        assert!(s.key_authorized(&key(2)));
        assert!(!s.key_authorized(&key(1)));
        // Taking it back is a fresh attach.
        let (e, _, mode, _) = accepted(s.attach(input(1, "laptop", None, 600)));
        assert_eq!((e, mode), (3, AttachMode::Fresh));
    }

    #[test]
    fn detach_revokes_and_fences() {
        let mut s = SessionEpoch::new();
        let (_, id, _, _) = accepted(s.attach(input(1, "laptop", None, 10)));
        // A stale detach does nothing.
        assert_eq!(s.detach(7), DetachOutcome::Ignored);
        assert!(s.subscriber().is_some());
        let out = s.detach(1);
        assert_eq!(
            out,
            DetachOutcome::Detached {
                epoch: 2,
                effects: vec![
                    Effect::RevokeKey(key(1)),
                    Effect::DropTransport { subscriber: id }
                ],
            }
        );
        assert!(s.subscriber().is_none());
        assert!(!s.key_authorized(&key(1)));
        // Detaching twice, or resuming after detaching, is refused.
        assert_eq!(s.detach(2), DetachOutcome::Ignored);
        assert_eq!(
            s.attach(input(1, "laptop", resume(1), 30)),
            AttachOutcome::Refused(AttachRefusal::Detached)
        );
        assert_eq!(
            s.attach(input(1, "laptop", resume(2), 30)),
            AttachOutcome::Refused(AttachRefusal::Detached)
        );
        // A fresh attach after a detach has nobody to notify.
        let (e, _, _, fx) = accepted(s.attach(input(1, "laptop", None, 40)));
        assert_eq!(e, 3);
        assert_eq!(fx, vec![Effect::ResizePty(TermSize::new(80, 24).unwrap())]);
    }

    #[test]
    fn resume_from_the_future_is_bad() {
        let mut s = SessionEpoch::new();
        accepted(s.attach(input(1, "laptop", None, 10)));
        assert_eq!(
            s.attach(input(1, "laptop", resume(9), 20)),
            AttachOutcome::Refused(AttachRefusal::BadResume)
        );
        // And resuming against a session nobody ever attached to (e.g. a restarted daemon).
        let mut fresh = SessionEpoch::new();
        assert_eq!(
            fresh.attach(input(1, "laptop", resume(0), 20)),
            AttachOutcome::Refused(AttachRefusal::Detached)
        );
    }
}
