//! The epoch and takeover state machine for one session's subscriber (DESIGN.md section 5.2).
//!
//! Pure: no IO, no clock (the caller passes the time). Each call returns a value describing what
//! the daemon must now do ([`Effect`]); the machine never does it itself.
//!
//! Rules:
//! - A fresh attach (no resume state) to a session nobody holds bumps the epoch and becomes the
//!   subscriber.
//! - A fresh attach to a session that a subscriber holds (connected or not) takes it over only
//!   if it asks to: [`TakeOver::Any`], or [`TakeOver::Epoch`] of the epoch now in force (compare
//!   and swap: the holder the user was asked about, and nobody who came after). It bumps the
//!   epoch, the previous subscriber is told it was taken over, its key is revoked and its
//!   transport dropped. Otherwise it is refused with who holds the session and its epoch
//!   ([`AttachRefusal::Attached`]), and nothing changes.
//! - A resume must match the session's incarnation (else the session was replaced) and present
//!   the current subscriber's key; with the current epoch it is accepted and keeps the epoch.
//! - A resume with a stale epoch is refused with the takeover details. It never steals the
//!   session; to take it back the client attaches afresh.
//! - Detach bumps the epoch and revokes, so a late resume from the detached client is refused.

use crate::msg::{
    AttachRefusal, HostLabel, PublicKey, ResumeRequest, SubscriberSummary, TakeOver, TakenOver,
    TermSize,
};

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
    /// Resize the PTY (SIGWINCH) to the attaching client's size. The state machine always asks;
    /// the session applies it for a fresh attach, and for a resumed one only if the attach's size
    /// generation is newer than the size it already has (`Attach::size_gen`).
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
    /// Whether a fresh attach may take the session from its subscriber (`Attach::take_over`).
    pub take_over: TakeOver,
    /// The current subscriber's transport is up, for the refusal of a fresh attach without
    /// `take_over` (the machine does not know about transports; the caller does).
    pub holder_connected: bool,
    pub now_unix_ms: u64,
}

#[derive(Debug)]
pub struct SessionEpoch {
    incarnation: u128,
    epoch: u64,
    current: Option<Subscriber>,
    next_id: u64,
}

impl SessionEpoch {
    /// A new session. `incarnation` must be random per session creation (the daemon supplies it;
    /// this crate has no randomness). It goes into the attach reply and clients echo it on resume.
    pub fn new(incarnation: u128) -> Self {
        Self {
            incarnation,
            epoch: 0,
            current: None,
            next_id: 0,
        }
    }

    pub fn incarnation(&self) -> u128 {
        self.incarnation
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
    #[cfg(test)]
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
        // Someone holds the session, and this attach did not ask to take it from this holder:
        // refused before anything changes (no epoch, no revoke, no resize).
        let allowed = match req.take_over {
            TakeOver::Any => true,
            TakeOver::Epoch(e) => e == self.epoch,
            TakeOver::No => false,
        };
        if !allowed && let Some(cur) = &self.current {
            return AttachOutcome::Refused(AttachRefusal::Attached {
                holder: SubscriberSummary {
                    host: cur.host.clone(),
                    since_unix_ms: cur.since_unix_ms,
                },
                epoch: self.epoch,
                connected: req.holder_connected,
            });
        }
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
        // Epochs restart at 1 for every new session, so they only mean something within one
        // incarnation. Without this check a client from a dead session could match a new one.
        if r.incarnation != self.incarnation {
            return refuse(AttachRefusal::SessionReplaced);
        }
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
        // A legitimate resume is the same client process, which keeps its per-attach key in
        // memory across reconnects. Any other key is not the subscriber: refuse, change nothing.
        if cur.key != req.key {
            return refuse(AttachRefusal::WrongKey);
        }
        // The same client, on a new connection. Its old transport (if half-dead) must go.
        cur.host = req.host;
        let effects = vec![
            Effect::DropTransport { subscriber: cur.id },
            Effect::ResizePty(req.size),
        ];
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

    const INC: u128 = 0xABCD_0000_0000_0000_0000_0000_0000_1234;

    fn key(b: u8) -> PublicKey {
        PublicKey::new(vec![b; 32]).unwrap()
    }
    /// An attach that takes the session over if it must (the rule before `take_over` existed;
    /// most tests here are about what happens after that).
    fn input(k: u8, host: &str, resume: Option<ResumeRequest>, now: u64) -> AttachInput {
        AttachInput {
            key: key(k),
            host: host.parse().unwrap(),
            size: TermSize::new(80, 24).unwrap(),
            resume,
            take_over: TakeOver::Any,
            holder_connected: true,
            now_unix_ms: now,
        }
    }
    /// A fresh attach that does not take the session over (the client's default).
    fn asking(k: u8, host: &str, now: u64, holder_connected: bool) -> AttachInput {
        AttachInput {
            take_over: TakeOver::No,
            holder_connected,
            ..input(k, host, None, now)
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
        Some(ResumeRequest {
            incarnation: INC,
            epoch,
            seq: 100,
            want_snapshot: false,
        })
    }

    #[test]
    fn first_attach_starts_epoch_one() {
        let mut s = SessionEpoch::new(INC);
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
        let mut s = SessionEpoch::new(INC);
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
    fn a_fresh_attach_without_take_over_is_refused_and_changes_nothing() {
        let mut s = SessionEpoch::new(INC);
        let (_, id, _, _) = accepted(s.attach(input(1, "laptop", None, 10)));
        for connected in [true, false] {
            let out = s.attach(asking(2, "desktop", 99, connected));
            assert_eq!(
                out,
                AttachOutcome::Refused(AttachRefusal::Attached {
                    holder: SubscriberSummary {
                        host: "laptop".parse().unwrap(),
                        since_unix_ms: 10,
                    },
                    epoch: 1,
                    connected,
                })
            );
            // No new epoch, the subscriber and its key as they were, the asker's key nowhere.
            assert_eq!(s.epoch(), 1);
            assert!(s.is_current(1));
            let sub = s.subscriber().unwrap();
            assert_eq!(
                (
                    sub.id,
                    sub.key.clone(),
                    sub.host.as_str(),
                    sub.since_unix_ms
                ),
                (id, key(1), "laptop", 10)
            );
            assert!(s.key_authorized(&key(1)) && !s.key_authorized(&key(2)));
        }
        // The holder resumes as if nothing happened.
        let (e, id2, _, _) = accepted(s.attach(input(1, "laptop", resume(1), 120)));
        assert_eq!((e, id2), (1, id));
        // And the asker, told yes by its user, takes it over as before.
        let (e, _, mode, fx) = accepted(s.attach(input(2, "desktop", None, 130)));
        assert_eq!((e, mode), (2, AttachMode::Fresh));
        assert!(fx.contains(&Effect::RevokeKey(key(1))));
    }

    /// `TakeOver::Epoch` is a compare and swap: it takes the session from the holder of that
    /// epoch only. One who came after (a third client while the user was deciding, or a client
    /// that took it while this one was away) is asked about afresh.
    #[test]
    fn an_epoch_grant_takes_the_session_from_that_holder_only() {
        let mut s = SessionEpoch::new(INC);
        accepted(s.attach(input(1, "laptop", None, 10)));
        // Asked about laptop at epoch 1; meanwhile desktop took it (epoch 2).
        accepted(s.attach(input(2, "desktop", None, 20)));
        let stale = AttachInput {
            take_over: TakeOver::Epoch(1),
            ..input(3, "phone", None, 30)
        };
        assert_eq!(
            s.attach(stale),
            AttachOutcome::Refused(AttachRefusal::Attached {
                holder: SubscriberSummary {
                    host: "desktop".parse().unwrap(),
                    since_unix_ms: 20,
                },
                epoch: 2,
                connected: true,
            })
        );
        assert_eq!(s.epoch(), 2);
        assert!(s.key_authorized(&key(2)) && !s.key_authorized(&key(3)));
        // The grant for the holder now in force takes it.
        let fresh = AttachInput {
            take_over: TakeOver::Epoch(2),
            ..input(3, "phone", None, 40)
        };
        let (e, _, _, fx) = accepted(s.attach(fresh));
        assert_eq!(e, 3);
        assert!(fx.contains(&Effect::RevokeKey(key(2))));
        // A grant for a session nobody holds has nobody to check against: it attaches.
        assert!(matches!(s.detach(3), DetachOutcome::Detached { .. }));
        let late = AttachInput {
            take_over: TakeOver::Epoch(1),
            ..input(4, "laptop", None, 50)
        };
        assert_eq!(accepted(s.attach(late)).0, 5);
    }

    #[test]
    fn a_session_nobody_holds_attaches_without_asking() {
        // New: nobody has attached yet.
        let mut s = SessionEpoch::new(INC);
        let (e, _, _, fx) = accepted(s.attach(asking(1, "laptop", 10, false)));
        assert_eq!(e, 1);
        assert_eq!(fx, vec![Effect::ResizePty(TermSize::new(80, 24).unwrap())]);
        // Detached: the subscriber left.
        assert!(matches!(s.detach(1), DetachOutcome::Detached { .. }));
        let (e, _, _, fx) = accepted(s.attach(asking(2, "desktop", 20, false)));
        assert_eq!(e, 3);
        assert_eq!(fx, vec![Effect::ResizePty(TermSize::new(80, 24).unwrap())]);
    }

    #[test]
    fn take_over_does_not_change_a_resume() {
        let mut s = SessionEpoch::new(INC);
        accepted(s.attach(input(1, "laptop", None, 10)));
        // A resume with `take_over` unset is the subscriber coming back: accepted.
        let r = AttachInput {
            take_over: TakeOver::No,
            ..input(1, "laptop", resume(1), 20)
        };
        let (e, _, mode, _) = accepted(s.attach(r));
        assert_eq!((e, mode), (1, AttachMode::Resumed(resume(1).unwrap())));
        // And a stale one with it set still never steals the session.
        accepted(s.attach(input(2, "desktop", None, 30)));
        let stale = s.attach(input(1, "laptop", resume(1), 40));
        assert!(matches!(
            stale,
            AttachOutcome::Refused(AttachRefusal::TakenOver(_))
        ));
        assert!(s.key_authorized(&key(2)));
    }

    #[test]
    fn resume_with_current_epoch_is_accepted_and_keeps_the_epoch() {
        let mut s = SessionEpoch::new(INC);
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
    fn resume_with_the_wrong_key_is_refused_and_touches_nothing() {
        let mut s = SessionEpoch::new(INC);
        let (_, id, _, _) = accepted(s.attach(input(1, "laptop", None, 10)));
        // Right incarnation, right epoch, someone else's key: refused, no effects, and the
        // subscriber is exactly as before.
        let out = s.attach(input(5, "other", resume(1), 20));
        assert_eq!(out, AttachOutcome::Refused(AttachRefusal::WrongKey));
        assert_eq!(s.epoch(), 1);
        let sub = s.subscriber().unwrap();
        assert_eq!(
            (sub.id, sub.key.clone(), sub.host.as_str()),
            (id, key(1), "laptop")
        );
        assert!(s.key_authorized(&key(1)) && !s.key_authorized(&key(5)));
    }

    /// A session killed and recreated under the same name restarts at epoch 1. A client that
    /// returns from the old incarnation with `{epoch: 1}` must not be taken for the new subscriber.
    #[test]
    fn resume_from_a_replaced_session_is_refused_even_with_a_matching_epoch() {
        let mut first = SessionEpoch::new(INC);
        accepted(first.attach(input(1, "laptop", None, 10)));
        let late = resume(1); // what the laptop presents after waking up

        let mut second = SessionEpoch::new(INC + 1);
        accepted(second.attach(input(2, "desktop", None, 50)));
        assert_eq!(
            second.epoch(),
            first.epoch(),
            "same epoch number in both incarnations"
        );
        // Even with the right key it is a different session.
        let out = second.attach(input(1, "laptop", late, 60));
        assert_eq!(out, AttachOutcome::Refused(AttachRefusal::SessionReplaced));
        assert!(second.key_authorized(&key(2)) && !second.key_authorized(&key(1)));
        assert_eq!(second.subscriber().unwrap().host.as_str(), "desktop");
        // A new session nobody has attached to yet refuses it the same way.
        let mut empty = SessionEpoch::new(INC + 2);
        assert_eq!(
            empty.attach(input(1, "laptop", late, 70)),
            AttachOutcome::Refused(AttachRefusal::SessionReplaced)
        );
    }

    #[test]
    fn stale_resume_is_refused_with_takeover_details_and_changes_nothing() {
        let mut s = SessionEpoch::new(INC);
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
        let mut s = SessionEpoch::new(INC);
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
        let mut s = SessionEpoch::new(INC);
        accepted(s.attach(input(1, "laptop", None, 10)));
        assert_eq!(
            s.attach(input(1, "laptop", resume(9), 20)),
            AttachOutcome::Refused(AttachRefusal::BadResume)
        );
        // And resuming against a session nobody ever attached to (e.g. a restarted daemon).
        let mut fresh = SessionEpoch::new(INC);
        assert_eq!(
            fresh.attach(input(1, "laptop", resume(0), 20)),
            AttachOutcome::Refused(AttachRefusal::Detached)
        );
    }
}
