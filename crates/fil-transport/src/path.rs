//! The path manager: which transport carries the session, and when to try another
//! (DESIGN.md §4.1, §4.2, §8.3).
//!
//! It is pure logic. It owns no sockets, no tasks and no clock. The client feeds it
//! [`PathInput`]s (link events, dial results, timers, hints) together with the current time, and
//! carries out the [`PathAction`]s it returns. The client keeps one manager for the whole
//! process, across links.
//!
//! # The model
//!
//! There are two *lanes*, the pipe (an ssh login) and direct (QUIC), and at most one *carrier*:
//! the link the session currently runs on. Each lane is `Idle`, `Waiting` (a retry timer is
//! running), `Busy` (a pipe is starting, or a dial is in flight) or `Up` (a link exists).
//!
//! The states and transitions are drawn in `docs/m1/quic-link.md` ("The path manager").
//!
//! # How the client drives it
//!
//! | The client | reports |
//! |---|---|
//! | starts, or has no link and no bootstrap information | `Start` |
//! | has a pipe link (before it attaches over it) | `PipeUp`; the pipe run failed, or the link ended: `PipeDown` |
//! | received an attach reply | `DirectAvailable { port, fingerprint, fresh_attach }` or `DirectUnavailable`, after *every* reply |
//! | dialed direct | `DirectUp` or `DirectDialFailed`; the link ended: `DirectDown` |
//! | finished the resume on the link it was told to switch to | `Switched(kind)`, or `SwitchFailed(kind)` |
//! | the OS says the network changed, or a timer it was asked for fired | `NetworkChanged`, `Timer(..)` |
//!
//! A swap is a transaction: on `SwitchTo(kind)` the client keeps the old link open, resumes on
//! the new one, and only then reports `Switched`; the old pipe is closed by a later `ClosePipe`.
//! The manager owns all retry timing, so the client's own loop must not back off as well.
//!
//! # Settings
//!
//! - **`Pipe`**: never dial. The pipe is re-run (with backoff) whenever it is gone.
//! - **`Auto`**: start on the pipe, dial direct in the background, move to direct when it is up
//!   and close the pipe. If direct is lost, redial at once and, if nothing carries within
//!   [`PathConfig::fallback_after`], re-run the pipe too. Direct keeps being retried, so the
//!   session returns to it from the pipe.
//! - **`Direct`**: as `Auto`, except that re-running the pipe (a new ssh login) waits
//!   [`PathConfig::direct_patience`] instead, and a failed redial does not shorten that wait. The first attach still needs the pipe: it carries
//!   the bootstrap and the key registration.
//!
//! # Why it does not flap or storm
//!
//! - **One thing at a time.** At most one pipe run and one dial are in flight, and a lane is
//!   never restarted while it is `Busy`, `Up` or `Waiting`.
//! - **Failures back off.** Each lane counts consecutive failures. The delay before the next try
//!   doubles from `retry_min` to `retry_max` (direct) or `pipe_retry_min` to `pipe_retry_max`
//!   (pipe), with up to 20% deterministic jitter so that many clients of one restarted daemon
//!   do not retry in step. After a long outage the delays are at their maximum, so a laptop that
//!   was offline for an hour makes about two attempts a minute, not a burst.
//! - **A link that dies young counts as a failure.** Only a link that stayed up for
//!   [`PathConfig::stable_after`] clears its lane's failure count. A direct link that comes up and
//!   dies again every few seconds, or a pipe whose remote command exits at once, therefore slows
//!   down geometrically instead of cycling at full speed. A failed `SwitchTo` counts the same way.
//! - **The pipe is closed only after the switch.** `ClosePipe` follows `Switched(Direct)`, not
//!   `DirectUp`, so a direct link that comes up but cannot resume the session never costs the
//!   working pipe.
//! - **A network-change hint is a nudge, not a command.** It clears the backoff and tries direct
//!   at once, but at most once per [`PathConfig::hint_cooldown`] (hints inside the window are
//!   coalesced into one timer), it restarts a waiting pipe only if the last pipe run began at
//!   least [`PathConfig::hint_pipe_gap`] ago, and only [`PathConfig::hint_budget`] hints are
//!   honoured per [`PathConfig::hint_window`]. A network that reports a change every second for
//!   an hour therefore costs a few attempts a minute, not a few a second.
//! - **Direct goes first after a loss, the pipe second.** When direct was healthy until it died,
//!   the manager redials at once and gives it `fallback_after` before starting an ssh login. When
//!   direct has been failing, the pipe starts at once: waiting would only delay a session that
//!   cannot come back any other way.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::link::{DownReason, LinkKind};

/// The per-host transport choice (DESIGN.md §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSetting {
    Auto,
    Pipe,
    Direct,
}

/// Timing for the path manager. The defaults follow DESIGN.md §4.2 ("a few seconds") and the
/// real-network measurements in `docs/m1/realnet.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathConfig {
    pub setting: TransportSetting,
    /// With `Auto`: how long a lost direct link gets to come back before the pipe is re-run.
    pub fallback_after: Duration,
    /// With `Direct`: the same, since an ssh login is not wanted unless direct is really gone.
    pub direct_patience: Duration,
    /// Delay before the second direct dial (the first retry), doubling up to `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// The same for re-running the pipe.
    pub pipe_retry_min: Duration,
    pub pipe_retry_max: Duration,
    /// How long a link must stay up to count as healthy.
    pub stable_after: Duration,
    /// Network-change hints closer together than this are merged.
    pub hint_cooldown: Duration,
    /// A hint restarts a waiting pipe only if the last pipe run started this long ago.
    pub hint_pipe_gap: Duration,
    /// At most this many hints are honoured in any `hint_window`; the rest are ignored. It
    /// bounds the work a network that keeps flapping can cause.
    pub hint_budget: usize,
    pub hint_window: Duration,
    /// Seeds the retry jitter. Any value; the client passes a random one.
    pub jitter_seed: u64,
}

impl Default for PathConfig {
    fn default() -> Self {
        Self {
            setting: TransportSetting::Auto,
            fallback_after: Duration::from_secs(3),
            direct_patience: Duration::from_secs(10),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(120),
            pipe_retry_min: Duration::from_secs(1),
            pipe_retry_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(10),
            hint_cooldown: Duration::from_secs(2),
            hint_pipe_gap: Duration::from_secs(5),
            hint_budget: 4,
            hint_window: Duration::from_secs(60),
            jitter_seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Something that happened, as reported by the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathInput {
    /// The client wants a session and has no links and no bootstrap information.
    Start,
    /// The pipe link exists: [`PathAction::RunPipe`] got as far as a connected pipe. Reported
    /// before the attach over it, so that the attach reply (`DirectAvailable`) is seen as coming
    /// from a fresh bootstrap.
    PipeUp,
    /// A pipe run failed, or the pipe link ended.
    PipeDown(DownReason),
    /// An attach reply carried a UDP port and fingerprint, so direct can be dialed. Reported
    /// after every reply, over either transport.
    ///
    /// The manager compares it with the last one it saw. Direct's backoff is cleared (and a
    /// waiting dial starts at once) only when the bootstrap really changed: the port or
    /// fingerprint differ (the daemon restarted), or `fresh_attach` says this was a new attach
    /// with a new key rather than a resume. Clearing it on every reply would let a direct link that
    /// keeps dying young trigger an ssh login every few seconds.
    DirectAvailable {
        port: u16,
        fingerprint: Vec<u8>,
        /// True if the reply answered a fresh attach (a new key), false for a resume.
        fresh_attach: bool,
    },
    /// The daemon does not offer direct (no port in the attach reply).
    DirectUnavailable,
    /// [`PathAction::DialDirect`] succeeded: a direct link is `Up`.
    DirectUp,
    /// [`PathAction::DialDirect`] failed (`Refused`).
    DirectDialFailed(DownReason),
    /// The direct link ended.
    DirectDown(DownReason),
    /// [`PathAction::SwitchTo`] is done: the session runs on that link.
    Switched(LinkKind),
    /// [`PathAction::SwitchTo`] failed (for example the resume was refused on the new link). The
    /// client closes that link.
    SwitchFailed(LinkKind),
    /// The OS reports that the network changed (an address came or went, Wi-Fi changed, the
    /// machine woke up).
    NetworkChanged,
    /// A timer from [`PathAction::ScheduleRetry`] fired.
    Timer(PathTimer),
}

/// The timers the manager asks for. Scheduling one that is already pending replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathTimer {
    /// Dial direct again.
    DirectRetry,
    /// Run the pipe again.
    PipeRetry,
    /// Nothing has carried the session for a while: run the pipe.
    PipeFallback,
    /// A network-change hint that arrived inside the cooldown.
    Hint,
}

/// What the client should do. Actions are returned in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathAction {
    /// Start the pipe command and attach. Report [`PathInput::PipeUp`] or
    /// [`PathInput::PipeDown`].
    RunPipe,
    /// Dial direct with the latest bootstrap information. Report [`PathInput::DirectUp`] or
    /// [`PathInput::DirectDialFailed`].
    DialDirect,
    /// Move the session to this link (resume over it). Report [`PathInput::Switched`] or
    /// [`PathInput::SwitchFailed`].
    SwitchTo(LinkKind),
    /// Close the pipe link, or abandon a pipe run that is still starting.
    ClosePipe,
    /// Close the direct link.
    CloseDirect,
    /// Call back with [`PathInput::Timer`] after this long. Replaces a pending timer of the same
    /// kind.
    ScheduleRetry(PathTimer, Duration),
    /// Drop a pending timer.
    CancelTimer(PathTimer),
}

/// One transport's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Nothing running, nothing scheduled.
    Idle,
    /// A retry timer is running.
    Waiting,
    /// A pipe is starting, or a dial is in flight.
    Busy,
    /// A link exists (it may not be the carrier yet).
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Info {
    Unknown,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    direct_retry: bool,
    pipe_retry: bool,
    pipe_fallback: bool,
    hint: bool,
}

impl Pending {
    fn slot(&mut self, t: PathTimer) -> &mut bool {
        match t {
            PathTimer::DirectRetry => &mut self.direct_retry,
            PathTimer::PipeRetry => &mut self.pipe_retry,
            PathTimer::PipeFallback => &mut self.pipe_fallback,
            PathTimer::Hint => &mut self.hint,
        }
    }
}

/// A read-only view for the status line and for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathView {
    /// The link the session runs on.
    pub carrier: Option<LinkKind>,
    /// The link a switch is in progress to.
    pub switching: Option<LinkKind>,
    pub pipe: Lane,
    pub direct: Lane,
    /// Consecutive direct failures since the last healthy link or fresh bootstrap.
    pub direct_failures: u32,
    pub pipe_failures: u32,
}

/// See the module docs.
#[derive(Clone, Debug)]
pub struct PathManager {
    cfg: PathConfig,
    info: Info,
    /// The port and fingerprint of the last `DirectAvailable`.
    last_info: Option<(u16, Vec<u8>)>,
    pipe: Lane,
    direct: Lane,
    carrier: Option<LinkKind>,
    switching: Option<LinkKind>,
    pipe_up_at: Option<Instant>,
    direct_up_at: Option<Instant>,
    pipe_failures: u32,
    direct_failures: u32,
    pending: Pending,
    hints: VecDeque<Instant>,
    last_pipe_start: Option<Instant>,
    last_direct_error: Option<DownReason>,
    rng: u64,
}

impl PathManager {
    pub fn new(cfg: PathConfig) -> Self {
        Self {
            cfg,
            info: Info::Unknown,
            last_info: None,
            pipe: Lane::Idle,
            direct: Lane::Idle,
            carrier: None,
            switching: None,
            pipe_up_at: None,
            direct_up_at: None,
            pipe_failures: 0,
            direct_failures: 0,
            pending: Pending::default(),
            hints: VecDeque::new(),
            last_pipe_start: None,
            last_direct_error: None,
            rng: cfg.jitter_seed | 1,
        }
    }

    pub fn config(&self) -> &PathConfig {
        &self.cfg
    }

    pub fn view(&self) -> PathView {
        PathView {
            carrier: self.carrier,
            switching: self.switching,
            pipe: self.pipe,
            direct: self.direct,
            direct_failures: self.direct_failures,
            pipe_failures: self.pipe_failures,
        }
    }

    /// Why direct last failed, for the status line ("UDP blocked?").
    pub fn last_direct_error(&self) -> Option<&DownReason> {
        self.last_direct_error.as_ref()
    }

    /// Feeds one event in. `now` must not go backwards between calls.
    pub fn handle(&mut self, now: Instant, input: PathInput) -> Vec<PathAction> {
        let mut out = Vec::new();
        match input {
            PathInput::Start => self.on_start(now, &mut out),
            PathInput::PipeUp => self.on_pipe_up(now, &mut out),
            PathInput::PipeDown(_) => self.on_pipe_down(now, &mut out),
            PathInput::DirectAvailable {
                port,
                fingerprint,
                fresh_attach,
            } => {
                self.info = Info::Available;
                let seen = (port, fingerprint);
                let changed = self.last_info.as_ref() != Some(&seen);
                self.last_info = Some(seen);
                if changed || fresh_attach {
                    self.direct_failures = 0;
                    if self.direct == Lane::Waiting {
                        self.direct = Lane::Idle;
                        self.cancel(PathTimer::DirectRetry, &mut out);
                    }
                }
            }
            PathInput::DirectUnavailable => {
                self.info = Info::Unavailable;
                self.last_info = None;
                if self.direct == Lane::Waiting {
                    self.direct = Lane::Idle;
                    self.cancel(PathTimer::DirectRetry, &mut out);
                }
            }
            PathInput::DirectUp => self.on_direct_up(now, &mut out),
            PathInput::DirectDialFailed(reason) => self.on_dial_failed(reason, &mut out),
            PathInput::DirectDown(_) => self.on_direct_down(now, &mut out),
            PathInput::Switched(kind) => self.on_switched(kind, &mut out),
            PathInput::SwitchFailed(kind) => self.on_switch_failed(kind, &mut out),
            PathInput::NetworkChanged => self.on_hint(now, &mut out),
            PathInput::Timer(t) => self.on_timer(t, now, &mut out),
        }
        self.settle(now, &mut out);
        out
    }

    // -----------------------------------------------------------------------------------------
    // Events

    fn on_start(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if self.pipe == Lane::Idle && self.carrier.is_none() {
            self.start_pipe(now, out);
        }
    }

    fn on_pipe_up(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if self.pipe == Lane::Up {
            return; // a duplicate report
        }
        self.pipe = Lane::Up;
        self.pipe_up_at = Some(now);
        self.cancel(PathTimer::PipeRetry, out);
        self.cancel(PathTimer::PipeFallback, out);
        if self.carrier == Some(LinkKind::Direct) {
            // A run that finished after direct took over: not needed. (While a switch to direct
            // is only *in progress* the pipe stays: if that switch fails, it is the way back.
            // `on_switched` closes it when the switch succeeds.)
            self.pipe = Lane::Idle;
            out.push(PathAction::ClosePipe);
        }
        // Otherwise `settle` moves the session to the pipe if nothing carries it.
    }

    fn on_pipe_down(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        let lived = match self.pipe {
            Lane::Busy => Some(Duration::ZERO),
            Lane::Up => Some(
                self.pipe_up_at
                    .map_or(Duration::ZERO, |t| now.duration_since(t)),
            ),
            _ => None, // stale: nothing of ours is running
        };
        let Some(lived) = lived else { return };
        self.pipe_gone(lived, out);
    }

    /// The pipe run failed or the pipe link is gone, after `lived` of being up.
    fn pipe_gone(&mut self, lived: Duration, out: &mut Vec<PathAction>) {
        if self.carrier == Some(LinkKind::Pipe) {
            self.carrier = None;
        }
        if self.switching == Some(LinkKind::Pipe) {
            self.switching = None;
        }
        self.pipe_up_at = None;
        if lived >= self.cfg.stable_after {
            self.pipe_failures = 0;
            self.pipe = Lane::Idle;
        } else {
            self.pipe_failures += 1;
            self.pipe = Lane::Waiting;
            let after = self.backoff(
                self.cfg.pipe_retry_min,
                self.cfg.pipe_retry_max,
                self.pipe_failures,
            );
            self.schedule(PathTimer::PipeRetry, after, out);
        }
    }

    fn on_direct_up(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if self.direct == Lane::Up {
            return;
        }
        if self.cfg.setting == TransportSetting::Pipe {
            out.push(PathAction::CloseDirect); // never asked for; defensive
            self.direct = Lane::Idle;
            return;
        }
        self.direct = Lane::Up;
        self.direct_up_at = Some(now);
        self.cancel(PathTimer::DirectRetry, out);
    }

    fn on_dial_failed(&mut self, reason: DownReason, out: &mut Vec<PathAction>) {
        if self.direct != Lane::Busy {
            return;
        }
        self.last_direct_error = Some(reason);
        self.direct_failures += 1;
        self.direct = Lane::Waiting;
        let after = self.backoff(self.cfg.retry_min, self.cfg.retry_max, self.direct_failures);
        self.schedule(PathTimer::DirectRetry, after, out);
    }

    fn on_direct_down(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if self.direct != Lane::Up {
            return; // stale
        }
        let lived = self
            .direct_up_at
            .map_or(Duration::ZERO, |t| now.duration_since(t));
        self.direct_gone(lived, out);
    }

    fn direct_gone(&mut self, lived: Duration, out: &mut Vec<PathAction>) {
        if self.carrier == Some(LinkKind::Direct) {
            self.carrier = None;
        }
        if self.switching == Some(LinkKind::Direct) {
            self.switching = None;
        }
        self.direct_up_at = None;
        if lived >= self.cfg.stable_after {
            // It was healthy: try again at once.
            self.direct_failures = 0;
            self.direct = Lane::Idle;
        } else {
            self.direct_failures += 1;
            self.direct = Lane::Waiting;
            let after = self.backoff(self.cfg.retry_min, self.cfg.retry_max, self.direct_failures);
            self.schedule(PathTimer::DirectRetry, after, out);
        }
    }

    fn on_switched(&mut self, kind: LinkKind, out: &mut Vec<PathAction>) {
        if self.switching != Some(kind) {
            return; // stale: the link went away meanwhile
        }
        self.switching = None;
        self.carrier = Some(kind);
        if kind == LinkKind::Direct && matches!(self.pipe, Lane::Up | Lane::Busy) {
            self.pipe = Lane::Idle;
            out.push(PathAction::ClosePipe);
        }
        if kind == LinkKind::Direct && self.pipe == Lane::Waiting {
            self.pipe = Lane::Idle;
        }
        if kind == LinkKind::Direct {
            self.cancel(PathTimer::PipeRetry, out);
            self.cancel(PathTimer::PipeFallback, out);
        }
    }

    fn on_switch_failed(&mut self, kind: LinkKind, out: &mut Vec<PathAction>) {
        if self.switching != Some(kind) {
            return;
        }
        match kind {
            LinkKind::Pipe => self.pipe_gone(Duration::ZERO, out),
            LinkKind::Direct => {
                out.push(PathAction::CloseDirect);
                self.direct_gone(Duration::ZERO, out);
            }
        }
    }

    fn on_hint(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if !self.hint_budget_left(now) {
            return;
        }
        if let Some(last) = self.hints.back() {
            let ready = *last + self.cfg.hint_cooldown;
            if now < ready {
                if !self.pending.hint {
                    self.schedule(PathTimer::Hint, ready - now, out);
                }
                return;
            }
        }
        self.apply_hint(now, out);
    }

    fn hint_budget_left(&mut self, now: Instant) -> bool {
        while let Some(t) = self.hints.front() {
            if now.duration_since(*t) > self.cfg.hint_window {
                self.hints.pop_front();
            } else {
                break;
            }
        }
        self.hints.len() < self.cfg.hint_budget
    }

    /// A hint that is allowed through: forget the backoff and try again now.
    fn apply_hint(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        if !self.hint_budget_left(now) {
            return;
        }
        self.hints.push_back(now);
        self.direct_failures = 0;
        if self.direct == Lane::Waiting {
            self.direct = Lane::Idle;
            self.cancel(PathTimer::DirectRetry, out);
        }
        let pipe_gap_ok = self
            .last_pipe_start
            .is_none_or(|t| now.duration_since(t) >= self.cfg.hint_pipe_gap);
        if self.pipe == Lane::Waiting && pipe_gap_ok {
            self.pipe_failures = 0;
            self.pipe = Lane::Idle;
            self.cancel(PathTimer::PipeRetry, out);
        }
    }

    fn on_timer(&mut self, timer: PathTimer, now: Instant, out: &mut Vec<PathAction>) {
        *self.pending.slot(timer) = false;
        match timer {
            PathTimer::DirectRetry => {
                if self.direct == Lane::Waiting {
                    self.direct = Lane::Idle;
                }
            }
            PathTimer::PipeRetry => {
                if self.pipe == Lane::Waiting {
                    self.pipe = Lane::Idle;
                    if self.needs_carrier() {
                        self.start_pipe(now, out);
                    }
                }
            }
            PathTimer::PipeFallback => {
                if self.pipe == Lane::Idle && self.needs_carrier() {
                    self.start_pipe(now, out);
                }
            }
            PathTimer::Hint => self.apply_hint(now, out),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Policy

    /// After every event: start whatever should be running and is not.
    fn settle(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        // A direct link that exists but is not the carrier is moved to, whatever carries now.
        if self.direct == Lane::Up
            && self.switching.is_none()
            && self.carrier != Some(LinkKind::Direct)
        {
            self.switching = Some(LinkKind::Direct);
            out.push(PathAction::SwitchTo(LinkKind::Direct));
        }
        // A pipe link that exists, with nothing carrying the session, carries it.
        if self.pipe == Lane::Up && self.needs_carrier() {
            self.switching = Some(LinkKind::Pipe);
            out.push(PathAction::SwitchTo(LinkKind::Pipe));
        }
        // Direct is dialed whenever it is possible and not already running, backing off,
        // or carrying.
        if self.dialable() && self.direct == Lane::Idle {
            self.direct = Lane::Busy;
            self.cancel(PathTimer::DirectRetry, out);
            out.push(PathAction::DialDirect);
        }
        if self.needs_carrier() {
            if self.pipe == Lane::Idle && !self.pending.pipe_fallback {
                if self.direct_hopeful() {
                    // Direct was fine a moment ago: give it a few seconds before an ssh login.
                    self.schedule(PathTimer::PipeFallback, self.fallback_delay(), out);
                } else {
                    self.start_pipe(now, out);
                }
            }
            // With the fallback timer pending, the timer decides: a dial that fails meanwhile
            // does not start the pipe early (that is what `direct_patience` is for).
        } else {
            self.cancel(PathTimer::PipeFallback, out);
        }
    }

    /// No link carries the session and none is being switched to.
    fn needs_carrier(&self) -> bool {
        self.carrier.is_none() && self.switching.is_none()
    }

    fn dialable(&self) -> bool {
        self.cfg.setting != TransportSetting::Pipe && self.info == Info::Available
    }

    /// Direct can be tried and has not been failing.
    fn direct_hopeful(&self) -> bool {
        self.dialable() && self.direct_failures == 0
    }

    fn fallback_delay(&self) -> Duration {
        match self.cfg.setting {
            TransportSetting::Direct => self.cfg.direct_patience,
            _ => self.cfg.fallback_after,
        }
    }

    fn start_pipe(&mut self, now: Instant, out: &mut Vec<PathAction>) {
        self.pipe = Lane::Busy;
        self.last_pipe_start = Some(now);
        self.cancel(PathTimer::PipeRetry, out);
        self.cancel(PathTimer::PipeFallback, out);
        out.push(PathAction::RunPipe);
    }

    fn schedule(&mut self, timer: PathTimer, after: Duration, out: &mut Vec<PathAction>) {
        *self.pending.slot(timer) = true;
        out.push(PathAction::ScheduleRetry(timer, after));
    }

    fn cancel(&mut self, timer: PathTimer, out: &mut Vec<PathAction>) {
        let slot = self.pending.slot(timer);
        if *slot {
            *slot = false;
            out.push(PathAction::CancelTimer(timer));
        }
    }

    /// `min * 2^(failures-1)`, at most `max`, with up to 20% jitter either way (never above
    /// `max`, never below 80% of `min`).
    fn backoff(&mut self, min: Duration, max: Duration, failures: u32) -> Duration {
        let exp = failures.saturating_sub(1).min(20);
        let base = min.saturating_mul(1 << exp).min(max);
        // xorshift64: deterministic, and different for each call.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let unit = (self.rng >> 11) as f64 / (1u64 << 53) as f64; // [0, 1)
        base.mul_f64(0.8 + 0.4 * unit).min(max)
    }
}

#[cfg(test)]
mod tests;
