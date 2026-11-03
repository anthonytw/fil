use std::collections::BTreeSet;

use super::*;
use PathAction::*;
use PathInput as I;

const S: fn(u64) -> Duration = Duration::from_secs;

struct T {
    m: PathManager,
    t0: Instant,
    at: Duration,
}

impl T {
    fn new(setting: TransportSetting) -> Self {
        Self::with(PathConfig {
            setting,
            ..PathConfig::default()
        })
    }

    fn with(cfg: PathConfig) -> Self {
        Self {
            m: PathManager::new(cfg),
            t0: Instant::now(),
            at: Duration::ZERO,
        }
    }

    fn advance(&mut self, d: Duration) {
        self.at += d;
    }

    fn feed(&mut self, input: PathInput) -> Vec<PathAction> {
        self.m.handle(self.t0 + self.at, input)
    }

    /// Feeds an input after advancing the clock.
    fn after(&mut self, d: Duration, input: PathInput) -> Vec<PathAction> {
        self.advance(d);
        self.feed(input)
    }

    fn view(&self) -> PathView {
        self.m.view()
    }

    /// The common start: pipe up and carrying, the attach reply offering direct.
    fn on_pipe_with_direct_info(setting: TransportSetting) -> Self {
        let mut t = Self::new(setting);
        assert_eq!(t.feed(I::Start), [RunPipe]);
        assert_eq!(t.feed(I::PipeUp), [SwitchTo(LinkKind::Pipe)]);
        assert_eq!(t.feed(I::Switched(LinkKind::Pipe)), []);
        t
    }

    /// Direct up and carrying, pipe closed. `t` is 30 s into the session.
    fn on_direct() -> Self {
        let mut t = Self::on_pipe_with_direct_info(TransportSetting::Auto);
        assert_eq!(t.feed(I::DirectAvailable), [DialDirect]);
        t.advance(S(1));
        assert_eq!(t.feed(I::DirectUp), [SwitchTo(LinkKind::Direct)]);
        assert_eq!(t.feed(I::Switched(LinkKind::Direct)), [ClosePipe]);
        t.advance(S(29));
        t
    }
}

fn refused() -> DownReason {
    DownReason::Refused("test".into())
}

fn delay_of(actions: &[PathAction], timer: PathTimer) -> Option<Duration> {
    actions.iter().find_map(|a| match a {
        ScheduleRetry(t, d) if *t == timer => Some(*d),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// The main path

#[test]
fn auto_starts_on_the_pipe_upgrades_to_direct_and_closes_the_pipe() {
    let mut t = T::new(TransportSetting::Auto);
    assert_eq!(t.feed(I::Start), [RunPipe]);
    assert_eq!(t.view().pipe, Lane::Busy);
    assert_eq!(t.feed(I::PipeUp), [SwitchTo(LinkKind::Pipe)]);
    assert_eq!(t.feed(I::Switched(LinkKind::Pipe)), []);
    assert_eq!(t.view().carrier, Some(LinkKind::Pipe));

    // The attach reply says where direct is: dial in the background.
    assert_eq!(t.feed(I::DirectAvailable), [DialDirect]);
    assert_eq!(
        t.view().carrier,
        Some(LinkKind::Pipe),
        "still on the pipe while dialing"
    );

    // Up: move there. Only once the session runs on it is the pipe closed.
    assert_eq!(t.feed(I::DirectUp), [SwitchTo(LinkKind::Direct)]);
    assert_eq!(t.view().carrier, Some(LinkKind::Pipe));
    assert_eq!(t.feed(I::Switched(LinkKind::Direct)), [ClosePipe]);
    let v = t.view();
    assert_eq!(
        (v.carrier, v.pipe, v.direct),
        (Some(LinkKind::Direct), Lane::Idle, Lane::Up)
    );
}

#[test]
fn pipe_setting_never_dials() {
    let mut t = T::new(TransportSetting::Pipe);
    assert_eq!(t.feed(I::Start), [RunPipe]);
    t.feed(I::PipeUp);
    t.feed(I::Switched(LinkKind::Pipe));
    assert_eq!(t.feed(I::DirectAvailable), []);
    assert_eq!(t.feed(I::NetworkChanged), []);
    // A stray direct link is closed.
    assert_eq!(t.feed(I::DirectUp), [CloseDirect]);
    assert_eq!(t.view().carrier, Some(LinkKind::Pipe));
}

#[test]
fn a_daemon_without_direct_stays_on_the_pipe() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    assert_eq!(t.feed(I::DirectUnavailable), []);
    assert_eq!(t.feed(I::NetworkChanged), []);
    assert_eq!(t.after(S(100), I::Timer(PathTimer::DirectRetry)), []);
    assert_eq!(t.view().direct, Lane::Idle);
}

#[test]
fn reconnect_after_a_pipe_kill_goes_direct_first_without_ssh() {
    // On the pipe, direct info known but the first dial has not happened yet (hopeful).
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable); // dial in flight
    t.feed(I::DirectDialFailed(refused())); // UDP blocked: waiting
    // Direct had been failing, so a killed ssh is re-run at once.
    let a = t.after(S(60), I::PipeDown(DownReason::Io("killed".into())));
    assert!(a.contains(&RunPipe), "{a:?}");
}

#[test]
fn a_lost_direct_link_is_redialed_at_once_and_the_pipe_follows_after_a_few_seconds() {
    let mut t = T::on_direct();
    let a = t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(a[0], DialDirect, "{a:?}");
    assert_eq!(delay_of(&a, PathTimer::PipeFallback), Some(S(3)));
    assert!(!a.contains(&RunPipe));
    assert_eq!(t.view().carrier, None);

    // Still nothing after 3 s: the pipe (a new ssh login), while the dial continues.
    let a = t.after(S(3), I::Timer(PathTimer::PipeFallback));
    assert_eq!(a, [RunPipe]);
    assert_eq!(t.view().direct, Lane::Busy);
    assert_eq!(t.feed(I::PipeUp), [SwitchTo(LinkKind::Pipe)]);
    t.feed(I::Switched(LinkKind::Pipe));

    // The dial that was in flight lands later: the session moves back to direct.
    t.advance(S(2));
    assert_eq!(t.feed(I::DirectUp), [SwitchTo(LinkKind::Direct)]);
    assert_eq!(t.feed(I::Switched(LinkKind::Direct)), [ClosePipe]);
}

#[test]
fn direct_back_within_the_grace_period_means_no_ssh_login() {
    let mut t = T::on_direct();
    let a = t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(a[0], DialDirect);
    t.advance(Duration::from_millis(400));
    let a = t.feed(I::DirectUp);
    assert_eq!(
        a,
        [
            SwitchTo(LinkKind::Direct),
            CancelTimer(PathTimer::PipeFallback)
        ]
    );
    assert_eq!(t.feed(I::Switched(LinkKind::Direct)), []);
    // The fallback timer, if it fires anyway, does nothing.
    assert_eq!(t.after(S(3), I::Timer(PathTimer::PipeFallback)), []);
}

#[test]
fn a_refused_dial_leaves_the_pipe_to_the_fallback_timer() {
    let mut t = T::on_direct();
    t.feed(I::DirectDown(DownReason::Dead)); // dialing, pipe fallback pending (3 s)
    let a = t.after(Duration::from_millis(500), I::DirectDialFailed(refused()));
    assert!(!a.contains(&RunPipe), "{a:?}");
    assert!(delay_of(&a, PathTimer::DirectRetry).is_some());
    assert_eq!(
        t.after(
            Duration::from_millis(2500),
            I::Timer(PathTimer::PipeFallback)
        ),
        [RunPipe]
    );
}

#[test]
fn the_direct_setting_waits_out_its_patience_after_a_failed_redial() {
    // An 8 s outage with `transport = direct`: dead at 5 s, the redial fails at 7 s (the connect
    // timeout), and the ssh login must still wait for the 10 s patience, not start at 7 s.
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Direct);
    t.feed(I::DirectAvailable);
    t.advance(S(1));
    t.feed(I::DirectUp);
    t.feed(I::Switched(LinkKind::Direct));
    t.advance(S(60));
    let a = t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(delay_of(&a, PathTimer::PipeFallback), Some(S(10)));
    let a = t.after(S(2), I::DirectDialFailed(refused()));
    assert!(
        !a.contains(&RunPipe) && !a.contains(&CancelTimer(PathTimer::PipeFallback)),
        "{a:?}"
    );
    // The retry fires and dials again; still no pipe.
    let d = delay_of(&a, PathTimer::DirectRetry).unwrap();
    assert_eq!(t.after(d, I::Timer(PathTimer::DirectRetry)), [DialDirect]);
    // At 10 s after the loss, the pipe.
    let a = t.after(S(10) - S(2) - d, I::Timer(PathTimer::PipeFallback));
    assert_eq!(a, [RunPipe]);
}

#[test]
fn the_pipe_is_not_closed_while_a_switch_to_direct_is_still_to_prove_itself() {
    let mut t = T::on_direct();
    t.feed(I::DirectDown(DownReason::Dead)); // redial in flight, pipe fallback pending
    t.after(S(3), I::Timer(PathTimer::PipeFallback)); // RunPipe
    // The redial lands first and the session starts moving to it.
    assert_eq!(
        t.after(Duration::from_millis(100), I::DirectUp),
        [SwitchTo(LinkKind::Direct)]
    );
    // The ssh login finishes meanwhile: the pipe is kept as the way back.
    assert_eq!(t.after(Duration::from_millis(100), I::PipeUp), []);
    assert_eq!(t.view().pipe, Lane::Up);
    // The resume on direct fails: the session goes to the pipe that is already there.
    let a = t.feed(I::SwitchFailed(LinkKind::Direct));
    assert_eq!(a[0], CloseDirect);
    assert!(a.contains(&SwitchTo(LinkKind::Pipe)), "{a:?}");
    assert!(!a.contains(&RunPipe), "no second ssh login: {a:?}");
    assert_eq!(t.feed(I::Switched(LinkKind::Pipe)), []);
    assert_eq!(t.view().carrier, Some(LinkKind::Pipe));
}

#[test]
fn a_reply_over_direct_does_not_clear_the_flapping_backoff() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable);
    t.advance(S(1));
    t.feed(I::DirectUp);
    t.feed(I::Switched(LinkKind::Direct)); // pipe closed
    t.advance(S(2));
    t.feed(I::DirectDown(DownReason::Dead)); // young: failure 1
    assert_eq!(t.view().direct_failures, 1);
    // The client reports `attached` after every reply, including one received over direct.
    t.feed(I::PipeUp);
    t.feed(I::DirectAvailable); // over the fresh pipe: a new bootstrap, clears it
    assert_eq!(t.view().direct_failures, 0);
    t.feed(I::Switched(LinkKind::Pipe));
    t.feed(I::DirectUp);
    t.feed(I::Switched(LinkKind::Direct));
    t.advance(S(2));
    t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(t.view().direct_failures, 1);
    t.feed(I::DirectAvailable); // no pipe link exists: this reply came over direct
    assert_eq!(t.view().direct_failures, 1);
}

#[test]
fn a_daemon_restart_needs_a_new_bootstrap_and_dials_the_new_port() {
    let mut t = T::on_direct();
    t.feed(I::DirectDown(DownReason::Dead));
    t.after(Duration::from_millis(500), I::DirectDialFailed(refused())); // key unknown now
    t.after(S(3), I::Timer(PathTimer::PipeFallback)); // RunPipe
    t.feed(I::PipeUp);
    t.feed(I::Switched(LinkKind::Pipe));
    // Fresh bootstrap over the new pipe: new key, new port. The backoff is forgotten.
    assert_eq!(t.view().direct, Lane::Waiting);
    let a = t.after(S(1), I::DirectAvailable);
    assert_eq!(a, [CancelTimer(PathTimer::DirectRetry), DialDirect]);
    assert_eq!(t.view().direct_failures, 0);
}

#[test]
fn direct_setting_waits_longer_before_an_ssh_login() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Direct);
    t.feed(I::DirectAvailable);
    t.advance(S(1));
    t.feed(I::DirectUp);
    t.feed(I::Switched(LinkKind::Direct));
    t.advance(S(30));
    let a = t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(a[0], DialDirect);
    assert_eq!(delay_of(&a, PathTimer::PipeFallback), Some(S(10)));
}

// ---------------------------------------------------------------------------------------------
// Failure, backoff, flapping

#[test]
fn udp_blocked_keeps_the_pipe_and_retries_with_growing_delays() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    assert_eq!(t.feed(I::DirectAvailable), [DialDirect]);
    let mut delays = vec![];
    for _ in 0..12 {
        let a = t.after(S(2), I::DirectDialFailed(refused()));
        let d = delay_of(&a, PathTimer::DirectRetry).expect("a retry is scheduled");
        delays.push(d);
        assert!(!a.contains(&RunPipe), "the pipe already carries: {a:?}");
        assert_eq!(t.view().carrier, Some(LinkKind::Pipe));
        let a = t.after(d, I::Timer(PathTimer::DirectRetry));
        assert_eq!(a, [DialDirect]);
    }
    // 1, 2, 4, ... with +-20% jitter, then capped at 120 s.
    for (i, d) in delays.iter().enumerate() {
        let nominal = (1u64 << i.min(20)).min(120) as f64;
        let secs = d.as_secs_f64();
        assert!(
            secs >= nominal * 0.8 - 1e-9 && secs <= (nominal * 1.2).min(120.0) + 1e-9,
            "retry {i}: {secs} s, nominal {nominal}"
        );
    }
    assert!(
        delays
            .windows(2)
            .skip(8)
            .all(|w| w[0] >= S(90) && w[1] >= S(90)),
        "{delays:?}"
    );
    assert_eq!(
        *delays.iter().max().unwrap().min(&S(120)),
        *delays.iter().max().unwrap()
    );
}

#[test]
fn a_flapping_direct_link_slows_down_and_a_stable_one_resets() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable);
    let mut delays = vec![];
    for _ in 0..5 {
        t.advance(Duration::from_millis(300));
        let a = t.feed(I::DirectUp);
        assert_eq!(a[0], SwitchTo(LinkKind::Direct));
        t.feed(I::Switched(LinkKind::Direct)); // closes the pipe
        t.advance(S(2)); // ... and it dies after two seconds
        let a = t.feed(I::DirectDown(DownReason::Dead));
        delays.push(delay_of(&a, PathTimer::DirectRetry).expect("backoff, not an instant redial"));
        assert!(!a.contains(&DialDirect));
        // Nothing carries, direct is failing: the pipe comes back at once.
        assert!(a.contains(&RunPipe), "{a:?}");
        t.feed(I::PipeUp);
        t.feed(I::Switched(LinkKind::Pipe));
        t.advance(*delays.last().unwrap());
        assert_eq!(t.feed(I::Timer(PathTimer::DirectRetry)), [DialDirect]);
    }
    assert!(delays.windows(2).all(|w| w[1] > w[0]), "{delays:?}");

    // A link that stays up for a minute clears the count.
    t.advance(Duration::from_millis(300));
    t.feed(I::DirectUp);
    t.feed(I::Switched(LinkKind::Direct));
    t.advance(S(60));
    let a = t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(a[0], DialDirect, "{a:?}");
    assert_eq!(t.view().direct_failures, 0);
}

#[test]
fn a_switch_that_fails_keeps_the_pipe_and_counts_as_a_failure() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable);
    assert_eq!(t.feed(I::DirectUp), [SwitchTo(LinkKind::Direct)]);
    let a = t.feed(I::SwitchFailed(LinkKind::Direct));
    assert_eq!(a[0], CloseDirect);
    assert!(!a.contains(&ClosePipe));
    assert!(delay_of(&a, PathTimer::DirectRetry).is_some());
    let v = t.view();
    assert_eq!(
        (v.carrier, v.pipe, v.direct),
        (Some(LinkKind::Pipe), Lane::Up, Lane::Waiting)
    );
}

#[test]
fn a_pipe_that_keeps_dying_young_backs_off() {
    let mut t = T::new(TransportSetting::Pipe);
    t.feed(I::Start);
    let mut delays = vec![];
    for _ in 0..6 {
        t.advance(Duration::from_millis(200));
        t.feed(I::PipeUp);
        t.feed(I::Switched(LinkKind::Pipe));
        t.advance(Duration::from_millis(300)); // the remote command exited right away
        let a = t.feed(I::PipeDown(DownReason::Io("eof".into())));
        let d = delay_of(&a, PathTimer::PipeRetry).expect("backoff");
        delays.push(d);
        t.advance(d);
        assert_eq!(t.feed(I::Timer(PathTimer::PipeRetry)), [RunPipe]);
    }
    assert!(delays.windows(2).all(|w| w[1] > w[0]), "{delays:?}");
    // A healthy pipe that ends is re-run at once.
    t.feed(I::PipeUp);
    t.feed(I::Switched(LinkKind::Pipe));
    t.advance(S(60));
    assert_eq!(
        t.feed(I::PipeDown(DownReason::Io("killed".into()))),
        [RunPipe]
    );
}

#[test]
fn a_failed_pipe_run_is_retried_with_backoff_and_direct_is_tried_meanwhile() {
    // Reconnecting with direct known: the dial starts, the pipe is the fallback.
    let mut t = T::on_direct();
    t.feed(I::DirectDown(DownReason::Dead));
    t.after(S(3), I::Timer(PathTimer::PipeFallback)); // RunPipe
    let a = t.after(S(1), I::PipeDown(DownReason::Io("no route".into())));
    assert_eq!(
        delay_of(&a, PathTimer::PipeRetry).map(|d| d.as_secs()),
        Some(1)
    );
    let a = t.after(S(1), I::Timer(PathTimer::PipeRetry));
    assert_eq!(a, [RunPipe]);
    let a = t.after(S(1), I::PipeDown(DownReason::Io("no route".into())));
    let d2 = delay_of(&a, PathTimer::PipeRetry).unwrap();
    assert!(d2 >= Duration::from_millis(1500), "{d2:?}");
}

// ---------------------------------------------------------------------------------------------
// Hints

#[test]
fn a_network_change_dials_direct_immediately_and_forgets_the_backoff() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable);
    for _ in 0..6 {
        let a = t.after(S(2), I::DirectDialFailed(refused()));
        let d = delay_of(&a, PathTimer::DirectRetry).unwrap();
        t.advance(d);
        t.feed(I::Timer(PathTimer::DirectRetry));
    }
    let a = t.after(S(2), I::DirectDialFailed(refused()));
    assert!(delay_of(&a, PathTimer::DirectRetry).unwrap() > S(20));

    let a = t.after(S(1), I::NetworkChanged);
    assert_eq!(a, [CancelTimer(PathTimer::DirectRetry), DialDirect]);
    assert_eq!(t.view().direct_failures, 0);
    // Not a flood: the dial is in flight, so another hint inside the cooldown is deferred...
    let a = t.after(Duration::from_millis(100), I::NetworkChanged);
    assert_eq!(
        a,
        [ScheduleRetry(PathTimer::Hint, Duration::from_millis(1900))]
    );
    // ...once, however many arrive...
    assert_eq!(t.after(Duration::from_millis(100), I::NetworkChanged), []);
    // ...and when it fires with the dial still running, nothing new starts.
    assert_eq!(t.after(S(2), I::Timer(PathTimer::Hint)), []);
}

#[test]
fn hints_are_budgeted() {
    let mut t = T::on_pipe_with_direct_info(TransportSetting::Auto);
    t.feed(I::DirectAvailable);
    t.feed(I::DirectDialFailed(refused()));
    let mut dials = 0;
    // A hint every 2.5 s for ten minutes, with every dial failing at once.
    for _ in 0..240 {
        let a = t.after(Duration::from_millis(2500), I::NetworkChanged);
        dials += a.iter().filter(|x| **x == DialDirect).count();
        if a.contains(&DialDirect) {
            t.feed(I::DirectDialFailed(refused()));
        }
    }
    // The 4-per-minute budget gives about 40 hint-driven dials, plus the backoff's own few.
    assert!(dials <= 55, "{dials} dials in ten minutes");
    assert!(dials >= 30, "{dials}");
}

#[test]
fn a_hint_restarts_a_waiting_pipe_only_if_the_last_run_was_not_recent() {
    let mut t = T::new(TransportSetting::Pipe);
    t.feed(I::Start);
    let a = t.after(
        Duration::from_millis(500),
        I::PipeDown(DownReason::Io("net down".into())),
    );
    assert!(delay_of(&a, PathTimer::PipeRetry).is_some());
    // 1 s after the run began: too soon.
    assert_eq!(t.after(Duration::from_millis(500), I::NetworkChanged), []);
    assert_eq!(t.view().pipe, Lane::Waiting);
    // 6 s after: go.
    let a = t.after(S(5), I::NetworkChanged);
    assert!(a.contains(&RunPipe), "{a:?}");
}

#[test]
fn a_hint_while_on_direct_does_nothing() {
    let mut t = T::on_direct();
    assert_eq!(t.feed(I::NetworkChanged), []);
}

// ---------------------------------------------------------------------------------------------
// Stale events

#[test]
fn late_reports_from_links_that_are_already_gone_are_ignored() {
    let mut t = T::on_direct();
    // The pipe was closed; its report arrives late.
    assert_eq!(t.feed(I::PipeDown(DownReason::Closed)), []);
    // A switch result for a link that has since died.
    t.feed(I::DirectDown(DownReason::Dead));
    assert_eq!(t.feed(I::Switched(LinkKind::Direct)), []);
    assert_eq!(t.view().carrier, None);
    assert_eq!(t.feed(I::SwitchFailed(LinkKind::Direct)), []);
    // A dial result with no dial running.
    t.after(S(1), I::DirectDialFailed(refused()));
    let before = t.view();
    assert_eq!(t.feed(I::DirectDialFailed(refused())), []);
    assert_eq!(t.view(), before);
}

#[test]
fn a_pipe_run_that_finishes_after_direct_took_over_is_closed() {
    let mut t = T::on_direct();
    t.feed(I::DirectDown(DownReason::Dead));
    t.after(S(3), I::Timer(PathTimer::PipeFallback)); // RunPipe
    t.advance(Duration::from_millis(100));
    t.feed(I::DirectUp);
    assert_eq!(t.feed(I::Switched(LinkKind::Direct)), [ClosePipe]); // abandons the run
    assert_eq!(t.feed(I::PipeUp), [ClosePipe]); // ...which finished anyway
    assert_eq!(t.view().pipe, Lane::Idle);
}

// ---------------------------------------------------------------------------------------------
// Properties: invariants over random event sequences, and bounded work

/// A model of the client, for the manager to act on. It checks every action against what could
/// really be done.
struct World {
    t: T,
    rng: u64,
    pipe_running: bool,
    pipe_up: bool,
    dialing: bool,
    direct_up: bool,
    /// Switch results the client will report: the link, whether it works, and which link
    /// generation it was (a result for a link that has since gone is dropped, as a real client
    /// would).
    switches: Vec<(LinkKind, bool, u64)>,
    pipe_gen: u64,
    direct_gen: u64,
    timers: BTreeSet<(Duration, u8)>, // (due, timer index)
    runs: u64,
    dials: u64,
    cfg: PathConfig,
}

const TIMERS: [PathTimer; 4] = [
    PathTimer::DirectRetry,
    PathTimer::PipeRetry,
    PathTimer::PipeFallback,
    PathTimer::Hint,
];

fn timer_index(t: PathTimer) -> u8 {
    TIMERS.iter().position(|x| *x == t).unwrap() as u8
}

impl World {
    fn new(cfg: PathConfig, seed: u64) -> Self {
        Self {
            t: T::with(cfg),
            rng: seed | 1,
            pipe_running: false,
            pipe_up: false,
            dialing: false,
            direct_up: false,
            switches: vec![],
            pipe_gen: 0,
            direct_gen: 0,
            timers: BTreeSet::new(),
            runs: 0,
            dials: 0,
            cfg,
        }
    }

    fn rand(&mut self, n: u64) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng % n
    }

    fn feed(&mut self, input: PathInput) {
        let actions = self.t.feed(input);
        for a in actions {
            match a {
                RunPipe => {
                    assert!(
                        !self.pipe_running && !self.pipe_up,
                        "RunPipe with a pipe present"
                    );
                    self.pipe_running = true;
                    self.runs += 1;
                }
                DialDirect => {
                    assert_ne!(self.cfg.setting, TransportSetting::Pipe);
                    assert!(
                        !self.dialing && !self.direct_up,
                        "DialDirect with direct present"
                    );
                    self.dialing = true;
                    self.dials += 1;
                }
                SwitchTo(k) => {
                    let exists = match k {
                        LinkKind::Pipe => self.pipe_up,
                        LinkKind::Direct => self.direct_up,
                    };
                    assert!(exists, "SwitchTo({k:?}) but that link does not exist");
                    let ok = self.rand(4) != 0;
                    let id = match k {
                        LinkKind::Pipe => self.pipe_gen,
                        LinkKind::Direct => self.direct_gen,
                    };
                    self.switches.push((k, ok, id));
                }
                ClosePipe => {
                    assert!(self.pipe_running || self.pipe_up, "ClosePipe with no pipe");
                    assert_eq!(
                        self.t.view().carrier,
                        Some(LinkKind::Direct),
                        "ClosePipe while direct does not carry the session"
                    );
                    self.pipe_running = false;
                    self.pipe_up = false;
                }
                CloseDirect => {
                    self.dialing = false;
                    self.direct_up = false;
                }
                ScheduleRetry(timer, after) => {
                    let cap = self.cfg.retry_max.max(self.cfg.pipe_retry_max).max(S(60));
                    assert!(after <= cap, "{timer:?} after {after:?}");
                    self.timers.retain(|(_, i)| *i != timer_index(timer));
                    self.timers.insert((self.t.at + after, timer_index(timer)));
                }
                CancelTimer(timer) => {
                    self.timers.retain(|(_, i)| *i != timer_index(timer));
                }
            }
        }
        let v = self.t.view();
        match v.carrier {
            Some(LinkKind::Pipe) => assert_eq!(v.pipe, Lane::Up, "{v:?}"),
            Some(LinkKind::Direct) => assert_eq!(v.direct, Lane::Up, "{v:?}"),
            None => {}
        }
        if self.cfg.setting == TransportSetting::Pipe {
            assert_eq!(v.direct, Lane::Idle);
        }
        // The lanes agree with the world, except where the world holds a link the manager has
        // not been told about yet.
        if v.direct == Lane::Busy {
            assert!(self.dialing, "{v:?}");
        }
        if v.pipe == Lane::Busy {
            assert!(self.pipe_running, "{v:?}");
        }
    }

    /// One random step.
    fn step(&mut self) {
        let mut choices: Vec<u8> = vec![0, 0]; // 0: hint, 1: let time pass
        if self.pipe_running {
            choices.extend([2, 3]);
        }
        if self.pipe_up {
            choices.push(4);
        }
        if self.dialing {
            choices.extend([5, 6]);
        }
        if self.direct_up {
            choices.push(7);
        }
        if !self.switches.is_empty() {
            choices.extend([8, 8]);
        }
        if !self.timers.is_empty() {
            choices.extend([9, 9]);
        }
        choices.push(10); // info changes
        let pick = choices[self.rand(choices.len() as u64) as usize];
        let wait = Duration::from_millis(self.rand(3000));
        match pick {
            0 => {
                self.t.advance(wait);
                self.feed(I::NetworkChanged);
            }
            1 => self.t.advance(wait * 4),
            2 => {
                self.t.advance(wait);
                self.pipe_running = false;
                self.pipe_up = true;
                self.pipe_gen += 1;
                self.feed(I::PipeUp);
            }
            3 => {
                self.t.advance(wait);
                self.pipe_running = false;
                self.feed(I::PipeDown(DownReason::Io("x".into())));
            }
            4 => {
                self.t.advance(wait * 3);
                self.pipe_up = false;
                self.feed(I::PipeDown(DownReason::Io("x".into())));
            }
            5 => {
                self.t.advance(wait);
                self.dialing = false;
                self.direct_up = true;
                self.direct_gen += 1;
                self.feed(I::DirectUp);
            }
            6 => {
                self.t.advance(wait);
                self.dialing = false;
                self.feed(I::DirectDialFailed(refused()));
            }
            7 => {
                self.t.advance(wait * 6);
                self.direct_up = false;
                self.feed(I::DirectDown(DownReason::Dead));
            }
            8 => {
                self.t.advance(wait / 2);
                let i = self.rand(self.switches.len() as u64) as usize;
                let (k, ok, id) = self.switches.remove(i);
                let (alive, now_id) = match k {
                    LinkKind::Pipe => (self.pipe_up, self.pipe_gen),
                    LinkKind::Direct => (self.direct_up, self.direct_gen),
                };
                if !alive || id != now_id {
                    // The link went away first; nothing is reported.
                } else if ok {
                    self.feed(I::Switched(k));
                } else {
                    // The client closes the link it could not switch to.
                    match k {
                        LinkKind::Pipe => self.pipe_up = false,
                        LinkKind::Direct => self.direct_up = false,
                    }
                    self.feed(I::SwitchFailed(k));
                }
            }
            9 => {
                let (due, i) = *self.timers.iter().next().unwrap();
                self.timers.remove(&(due, i));
                if due > self.t.at {
                    self.t.at = due;
                }
                self.feed(I::Timer(TIMERS[i as usize]));
            }
            _ => {
                let r = self.rand(8);
                self.feed(if r == 0 {
                    I::DirectUnavailable
                } else {
                    I::DirectAvailable
                });
            }
        }
    }
}

#[test]
fn random_event_sequences_never_break_the_invariants() {
    for setting in [
        TransportSetting::Auto,
        TransportSetting::Pipe,
        TransportSetting::Direct,
    ] {
        for seed in 0..60u64 {
            let mut w = World::new(
                PathConfig {
                    setting,
                    jitter_seed: seed + 1,
                    ..PathConfig::default()
                },
                seed * 7919 + 13,
            );
            w.feed(I::Start);
            for _ in 0..400 {
                w.step();
            }
        }
    }
}

/// A world where everything fails at once: UDP blocked and ssh failing, with the network
/// reporting a change every `hint_every`. Counts the attempts over `hours`.
fn storm(setting: TransportSetting, hint_every: Option<Duration>, hours: u64) -> (u64, u64) {
    let mut w = World::new(
        PathConfig {
            setting,
            ..PathConfig::default()
        },
        42,
    );
    w.feed(I::Start);
    w.feed(I::DirectAvailable);
    let end = S(3600 * hours);
    let mut next_hint = hint_every;
    while w.t.at < end {
        // Whatever is running fails after a short time.
        if w.pipe_running {
            w.t.advance(Duration::from_millis(300));
            w.pipe_running = false;
            w.feed(I::PipeDown(DownReason::Io("no route".into())));
        }
        if w.dialing {
            w.t.advance(S(2));
            w.dialing = false;
            w.feed(I::DirectDialFailed(refused()));
        }
        let timer = w.timers.iter().next().copied();
        let hint = next_hint;
        match (timer, hint) {
            (Some((due, i)), h) if h.is_none_or(|h| due <= h) => {
                w.timers.remove(&(due, i));
                if due > w.t.at {
                    w.t.at = due;
                }
                w.feed(I::Timer(TIMERS[i as usize]));
            }
            (_, Some(h)) => {
                if h > w.t.at {
                    w.t.at = h;
                }
                next_hint = Some(h + hint_every.unwrap());
                w.feed(I::NetworkChanged);
            }
            (None, None) => break,
            _ => unreachable!(),
        }
    }
    (w.runs, w.dials)
}

#[test]
fn a_long_outage_makes_few_attempts() {
    // Nothing works for a day; no hints.
    for setting in [TransportSetting::Auto, TransportSetting::Direct] {
        let (runs, dials) = storm(setting, None, 24);
        eprintln!("{setting:?}: {runs} ssh attempts and {dials} dials in a simulated day");
        // About 3 attempts a minute at the start, then one per 30 s (pipe) and per 2 min (direct).
        assert!(
            runs <= 24 * 60 * 22 / 10,
            "{setting:?}: {runs} ssh logins in a day"
        );
        assert!(dials <= 24 * 32 + 20, "{setting:?}: {dials} dials in a day");
        assert!(runs >= 24 * 60, "it keeps trying: {runs}");
        assert!(dials >= 24 * 25, "it keeps trying: {dials}");
    }
}

#[test]
fn a_network_that_flaps_all_day_is_bounded_by_the_hint_budget() {
    // A change every second for an hour, with nothing working.
    for setting in [
        TransportSetting::Auto,
        TransportSetting::Direct,
        TransportSetting::Pipe,
    ] {
        let (runs, dials) = storm(setting, Some(S(1)), 1);
        eprintln!(
            "{setting:?}: {runs} ssh attempts and {dials} dials in a simulated hour of hints"
        );
        // Four honoured hints a minute; each restarts the backoff, so each is worth a handful of
        // attempts before the next one. Measured: about 6 ssh logins and 9 dials a minute.
        assert!(runs <= 60 * 7, "{setting:?}: {runs} ssh logins in an hour");
        assert!(dials <= 60 * 10, "{setting:?}: {dials} dials in an hour");
        if setting == TransportSetting::Pipe {
            assert_eq!(dials, 0);
        }
    }
}

#[test]
fn a_healthy_world_converges_to_direct_and_stays_there() {
    for seed in 0..20u64 {
        let mut w = World::new(
            PathConfig {
                jitter_seed: seed + 1,
                ..PathConfig::default()
            },
            seed + 5,
        );
        w.feed(I::Start);
        for _ in 0..200 {
            // Everything succeeds, quickly, except for dials which fail 1 time in 3 at first.
            if w.pipe_running {
                w.t.advance(Duration::from_millis(800));
                w.pipe_running = false;
                w.pipe_up = true;
                w.pipe_gen += 1;
                w.feed(I::PipeUp);
            }
            if w.dialing {
                w.t.advance(Duration::from_millis(150));
                w.dialing = false;
                if w.t.at < S(20) && w.rand(3) == 0 {
                    w.feed(I::DirectDialFailed(refused()));
                } else {
                    w.direct_up = true;
                    w.direct_gen += 1;
                    w.feed(I::DirectUp);
                }
            }
            while let Some((k, _, _)) = w.switches.pop() {
                w.t.advance(Duration::from_millis(50));
                w.feed(I::Switched(k));
            }
            if w.t.view().carrier == Some(LinkKind::Pipe) {
                w.feed(I::DirectAvailable);
            }
            if let Some((due, i)) = w.timers.iter().next().copied() {
                w.timers.remove(&(due, i));
                if due > w.t.at {
                    w.t.at = due;
                }
                w.feed(I::Timer(TIMERS[i as usize]));
            }
        }
        let v = w.t.view();
        assert_eq!(v.carrier, Some(LinkKind::Direct), "seed {seed}: {v:?}");
        assert_eq!(v.pipe, Lane::Idle, "seed {seed}: {v:?}");
        assert!(!w.pipe_up && !w.pipe_running, "the pipe was closed");
    }
}
