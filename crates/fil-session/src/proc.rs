//! Process grouping (DESIGN.md section 5.1): where a session's processes live, and tearing them
//! all down.
//!
//! - **Systemd scope.** On Linux with lingering enabled the child runs under
//!   `systemd-run --user --scope`, so systemd tracks every process the session starts, even
//!   ones that leave the process group or the tree, and a kill ends the scope.
//! - **Process tree.** Everywhere else the session follows the tree under the child: the child
//!   is a session leader (its own process group), and the core periodically records the
//!   descendants (a background job that was started under job control has a group of its own;
//!   one that was orphaned has been reparented to init), so a kill reaches those too. A process
//!   that double-forks and then dies between two scans escapes. That is the limit of the
//!   fallback; the scope has no such limit.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

use crate::config::ScopeMode;

/// Where a session's processes are tracked, once decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `systemd-run --user --scope --unit=<unit>`.
    Systemd { unit: String },
    /// The child's process group and tree.
    Tree,
}

impl Scope {
    pub fn is_systemd(&self) -> bool {
        matches!(self, Scope::Systemd { .. })
    }
}

/// The transient unit name for a session: `fil-<the incarnation in 32 hex digits>`.
///
/// The session's name is deliberately not part of it (DESIGN.md section 11.1): unit names show up
/// in `/proc/<pid>/cgroup` (world-readable) and in the journal, where other users and log
/// collectors can read them. The incarnation is random per session, so it is as unique as the
/// name was and tells a reader nothing.
pub fn unit_name(incarnation: u128) -> String {
    format!("fil-{incarnation:032x}")
}

/// The description of every session scope. It carries no session name, for the same reason as
/// [`unit_name`].
pub const SCOPE_DESCRIPTION: &str = "fil session";

/// Reads `loginctl show-user -p Linger` output.
pub fn parse_linger(show_user_output: &str) -> bool {
    show_user_output
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("Linger=yes"))
}

/// The command line that runs `inner` in a transient user scope called `unit`.
///
/// `--collect` lets systemd forget the scope even if it ends in a failed state, `--quiet`
/// keeps its chatter out of the terminal.
pub fn systemd_run_argv(unit: &str, description: &str, inner: &[String]) -> Vec<String> {
    let mut v: Vec<String> = [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        &format!("--unit={unit}"),
        &format!("--description={description}"),
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.extend_from_slice(inner);
    v
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

/// How long the probes (`loginctl`, the trial scope) may take. A user manager that does not
/// answer, common right after logout without lingering, must not hang session creation.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of each of a probe's output streams is kept. The rest is read and thrown away.
const PROBE_OUTPUT_CAP: usize = 64 * 1024;

/// How long to wait for the output readers once the probe has exited. A pipe held open by a
/// process the probe left behind would otherwise keep them going.
const READER_GRACE: Duration = Duration::from_millis(500);

type Kept = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Reads `r` to the end on its own thread, keeping the first `cap` bytes. Reading continues past
/// the cap so that the writer never blocks on a full pipe. Fails if the thread cannot start.
fn drain<R: std::io::Read + Send + 'static>(
    mut r: R,
    cap: usize,
) -> std::io::Result<(Kept, std::thread::JoinHandle<()>)> {
    let kept = Kept::default();
    let out = kept.clone();
    let t = std::thread::Builder::new()
        .name("fil-probe-drain".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut v = out.lock().unwrap_or_else(|e| e.into_inner());
                        let room = cap.saturating_sub(v.len());
                        v.extend_from_slice(&buf[..n.min(room)]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;
    Ok((kept, t))
}

/// Kills `child` and reaps it.
fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs `cmd` with its output captured and kills it after `timeout`.
///
/// Both output streams are drained while waiting, so a probe that prints more than a pipe holds
/// is not mistaken for a hung one. At most [`PROBE_OUTPUT_CAP`] bytes of each are kept.
///
/// If a process the probe left behind still holds its pipes after the probe exited, the readers
/// outlive this call by up to as long as that process lives (each keeps at most
/// [`PROBE_OUTPUT_CAP`] bytes, so the memory they hold is bounded); what they have read by
/// the end of the grace period is what is returned.
fn output_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    fil_transport::fdlimit::keep_for_std_child(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let readers = (|| {
        let out = drain(
            child.stdout.take().expect("stdout was piped"),
            PROBE_OUTPUT_CAP,
        )?;
        let err = drain(
            child.stderr.take().expect("stderr was piped"),
            PROBE_OUTPUT_CAP,
        )?;
        Ok::<_, std::io::Error>((out, err))
    })();
    let ((stdout, t_out), (stderr, t_err)) = match readers {
        Ok(r) => r,
        Err(e) => {
            // The pipes close with the child gone, which ends a reader that did start.
            kill_and_reap(&mut child);
            return Err(e);
        }
    };
    let end = Instant::now() + timeout;
    loop {
        let waited = match child.try_wait() {
            Ok(w) => w,
            Err(e) => {
                kill_and_reap(&mut child);
                return Err(e);
            }
        };
        if let Some(status) = waited {
            let grace = Instant::now() + READER_GRACE;
            while !(t_out.is_finished() && t_err.is_finished()) && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(2));
            }
            let take = |m: &Kept| std::mem::take(&mut *m.lock().unwrap_or_else(|e| e.into_inner()));
            return Ok(std::process::Output {
                status,
                stdout: take(&stdout),
                stderr: take(&stderr),
            });
        }
        if Instant::now() >= end {
            kill_and_reap(&mut child);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("did not answer within {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether user `uid` lingers (their user manager and processes outlive their last login), from
/// `loginctl show-user <uid> -p Linger`.
///
/// `None` when it cannot be told: there is no `loginctl`, it failed (no systemd, no such user), or
/// it did not answer within 5 seconds. This is the one linger probe; the daemon's logout warning
/// (`fil_daemon::linger`) uses it too.
///
/// Blocking: runs `loginctl`. Call it from a blocking context, never on an async worker.
pub fn lingering(uid: u32) -> Option<bool> {
    let mut cmd = Command::new("loginctl");
    cmd.args(["show-user", &uid.to_string(), "-p", "Linger"]);
    let out = output_with_timeout(cmd, PROBE_TIMEOUT).ok()?;
    out.status
        .success()
        .then(|| parse_linger(&String::from_utf8_lossy(&out.stdout)))
}

/// `lingering` for the user this process runs as; "cannot tell" counts as no.
fn linger_enabled() -> bool {
    lingering(nix::unistd::getuid().as_raw()).unwrap_or(false)
}

/// What is known about the host before choosing a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub linux: bool,
    pub systemd_run_on_path: bool,
    pub xdg_runtime_dir_set: bool,
}

/// The outcome of choosing a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Systemd,
    Tree,
    /// Systemd looked usable but the trial scope failed: the tree, and say so.
    TreeWithWarning(String),
    /// `ScopeMode::Systemd` was asked for and cannot be had.
    Fail(String),
}

/// The decision logic, with the expensive probes passed in so they run only when needed.
///
/// - `Tree`: the tree.
/// - `Systemd`: the scope, or a failure naming what is missing or what the trial said.
/// - `Auto`: the scope when this is Linux with `systemd-run`, a user runtime directory, lingering
///   enabled and a trial scope that starts; the tree otherwise. A failed trial after all the other
///   conditions held is worth a warning, because lingering is on and the user expects a scope.
pub fn decide_scope(
    mode: ScopeMode,
    facts: Facts,
    linger: impl FnOnce() -> bool,
    trial: impl FnOnce() -> Result<(), String>,
) -> Decision {
    match mode {
        ScopeMode::Tree => Decision::Tree,
        ScopeMode::Systemd => {
            if !facts.systemd_run_on_path {
                return Decision::Fail("systemd-run is not on PATH".into());
            }
            if !facts.xdg_runtime_dir_set {
                return Decision::Fail("XDG_RUNTIME_DIR is not set".into());
            }
            match trial() {
                Ok(()) => Decision::Systemd,
                Err(e) => Decision::Fail(e),
            }
        }
        ScopeMode::Auto => {
            if !(facts.linux && facts.systemd_run_on_path && facts.xdg_runtime_dir_set) || !linger()
            {
                return Decision::Tree;
            }
            match trial() {
                Ok(()) => Decision::Systemd,
                Err(e) => Decision::TreeWithWarning(e),
            }
        }
    }
}

/// Starts a throwaway scope to see that the user manager answers. Blocking.
fn trial_scope() -> Result<(), String> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unit = format!("fil-trial-{}-{n}", std::process::id());
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        .args(["--", "true"]);
    let out = output_with_timeout(cmd, PROBE_TIMEOUT)
        .map_err(|e| format!("systemd-run did not run: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "systemd-run --user --scope failed: {}",
        err.lines().next().unwrap_or("no output").trim()
    ))
}

/// Decides the scope for a new session. Blocking (it may run `loginctl` and `systemd-run`).
pub fn resolve_scope(
    mode: ScopeMode,
    name: &str,
    incarnation: u128,
) -> Result<Scope, crate::pty::SpawnError> {
    let facts = Facts {
        linux: cfg!(target_os = "linux"),
        systemd_run_on_path: on_path("systemd-run"),
        xdg_runtime_dir_set: std::env::var_os("XDG_RUNTIME_DIR").is_some(),
    };
    match decide_scope(mode, facts, linger_enabled, trial_scope) {
        Decision::Systemd => Ok(Scope::Systemd {
            unit: unit_name(incarnation),
        }),
        Decision::Tree => Ok(Scope::Tree),
        Decision::TreeWithWarning(why) => {
            tracing::warn!(%why, "no systemd scope; tracking the process tree instead");
            tracing::debug!(session = name, "no systemd scope for");
            Ok(Scope::Tree)
        }
        Decision::Fail(why) => Err(crate::pty::SpawnError::Scope(why)),
    }
}

/// One row of the process table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    /// When the process started, as an opaque string. The same pid with another start time is a
    /// different process, which stops a stale record from signalling a recycled pid.
    pub start: String,
    pub zombie: bool,
}

/// The fields of `/proc/<pid>/stat` after the command name: the state, the parent pid, and the
/// rest. The command name may contain anything, including spaces and parentheses, so the fields
/// are counted from the *last* `)`.
fn stat_fields(stat: &str) -> Option<(&str, u32, impl Iterator<Item = &str>)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let state = f.next()?;
    let ppid: u32 = f.next()?.parse().ok()?;
    Some((state, ppid, f))
}

/// The parent pid in `/proc/<pid>/stat`, and nothing else of it.
pub fn parse_ppid(stat: &str) -> Option<u32> {
    stat_fields(stat).map(|(_, ppid, _)| ppid)
}

/// Parses `/proc/<pid>/stat`.
pub fn parse_proc_stat(pid: u32, stat: &str) -> Option<ProcRow> {
    let (state, ppid, mut f) = stat_fields(stat)?;
    // Field 22 is the start time; state and ppid were fields 3 and 4, so 17 more to skip.
    let start = f.nth(17)?.to_string();
    Some(ProcRow {
        pid,
        ppid,
        start,
        zombie: state == "Z",
    })
}

/// Parses `ps -A -o pid=,ppid=,stat=,lstart=`.
pub fn parse_ps(out: &str) -> Vec<ProcRow> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ppid = f.next()?.parse().ok()?;
            let stat = f.next()?;
            let start = f.collect::<Vec<_>>().join(" ");
            Some(ProcRow {
                pid,
                ppid,
                start,
                zombie: stat.starts_with('Z'),
            })
        })
        .collect()
}

/// How old [`list_processes`] may let the table it returns be.
///
/// The tree fallback records descendants every few seconds, per session. Each scan reads every
/// process (`/proc/*/stat` on Linux, one `sysctl` on macOS, `ps` elsewhere), so N sessions scanning on their own
/// clocks cost N scans per interval for the same table. Sessions that ask within this age share
/// one scan. It is well under the recording interval, so it costs the recording a little
/// freshness at most; teardown never uses it.
pub const SHARED_TABLE_MAX_AGE: Duration = Duration::from_secs(2);

type TableSlot = Mutex<Option<(Instant, std::sync::Arc<Vec<ProcRow>>)>>;

static SHARED_TABLE: TableSlot = Mutex::new(None);

/// The table in `slot` if it is no older than `max_age` at `now`; otherwise `scan()`'s result,
/// which is stored. The slot stays locked during the scan, so concurrent callers wait for it and
/// share its result instead of each scanning.
#[cfg(test)]
fn table_from(
    slot: &TableSlot,
    max_age: Duration,
    now: Instant,
    scan: impl FnOnce() -> Vec<ProcRow>,
) -> std::sync::Arc<Vec<ProcRow>> {
    table_from_since(slot, max_age, None, now, scan)
}

/// [`table_from`], but a stored table taken before `not_before` is not good enough.
fn table_from_since(
    slot: &TableSlot,
    max_age: Duration,
    not_before: Option<Instant>,
    now: Instant,
    scan: impl FnOnce() -> Vec<ProcRow>,
) -> std::sync::Arc<Vec<ProcRow>> {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, rows)) = slot.as_ref()
        && now.saturating_duration_since(*at) <= max_age
        && not_before.is_none_or(|t| *at >= t)
    {
        return rows.clone();
    }
    let rows = std::sync::Arc::new(scan());
    *slot = Some((now, rows.clone()));
    rows
}

/// The process table for the periodic recording of a session's descendants, shared between
/// sessions: it may be up to [`SHARED_TABLE_MAX_AGE`] old. Blocking.
///
/// Use [`scan_processes`] where the table must be current (teardown).
pub fn list_processes() -> Vec<ProcRow> {
    list_processes_since(None)
}

/// [`list_processes`], but never a table taken before `not_before`. A session's first recording
/// passes the instant it was spawned: a shared table from before that cannot hold the session's
/// processes, and a job that leaves the tree before the next recording (5 s) would then escape a
/// kill.
pub fn list_processes_since(not_before: Option<Instant>) -> Vec<ProcRow> {
    table_from_since(
        &SHARED_TABLE,
        SHARED_TABLE_MAX_AGE,
        not_before,
        Instant::now(),
        scan_processes,
    )
    .as_ref()
    .clone()
}

/// The process table, read now. Blocking.
pub fn scan_processes() -> Vec<ProcRow> {
    if let Ok(dir) = std::fs::read_dir("/proc")
        && std::path::Path::new("/proc/self/stat").exists()
    {
        return dir
            .filter_map(|e| {
                let e = e.ok()?;
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                parse_proc_stat(pid, &stat)
            })
            .collect();
    }
    scan_without_proc()
}

/// macOS: the kernel's process table, read in-process with `sysctl(KERN_PROC_ALL)`, which is
/// what `ps` reads. Forking `ps` for it took about 25 to 50 ms a scan (on 1,400 processes), every
/// 5 s for a tree session's whole life and every 10 to 20 ms during a kill; this takes about
/// 0.15 ms. `proc_pidinfo` would be the documented alternative, but it refuses other users'
/// processes, and a session's tree can pass through one (`sudo -u $USER ...`).
#[cfg(target_os = "macos")]
fn scan_without_proc() -> Vec<ProcRow> {
    match kinfo::read_all() {
        Ok(buf) => kinfo::parse(&buf),
        Err(e) => {
            tracing::debug!(error = %e, "sysctl(KERN_PROC_ALL) failed");
            Vec::new()
        }
    }
}

/// Elsewhere (no `/proc`, not macOS): `ps`.
#[cfg(not(target_os = "macos"))]
fn scan_without_proc() -> Vec<ProcRow> {
    let mut ps = Command::new("ps");
    ps.args(["-A", "-o", "pid=,ppid=,stat=,lstart="])
        .env("LC_ALL", "C");
    fil_transport::fdlimit::keep_for_std_child(&mut ps);
    ps.output()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// The macOS process table as `sysctl(CTL_KERN, KERN_PROC, KERN_PROC_ALL)` returns it: an array
/// of `struct kinfo_proc` (`<sys/sysctl.h>`). libc does not define that struct, and rustix has no
/// `sysctl`, so the buffer is read as bytes at the offsets of the three fields used; the
/// offsets are the same on arm64 and x86_64 (checked against the SDK headers with
/// `_Static_assert` for both, and by `the_macos_table_matches_ps` below at run time).
#[cfg(any(target_os = "macos", test))]
mod kinfo {
    use super::ProcRow;

    /// `sizeof(struct kinfo_proc)`.
    pub(super) const SIZE: usize = 648;
    /// `kp_proc.p_un.__p_starttime`, a `struct timeval`: `tv_sec` (i64), then `tv_usec` (i32).
    const START_SEC: usize = 0;
    const START_USEC: usize = 8;
    /// `kp_proc.p_stat`, a `char`.
    const STAT: usize = 36;
    /// `kp_proc.p_pid`, an `int`.
    const PID: usize = 40;
    /// `kp_eproc.e_ppid`, an `int`.
    const PPID: usize = 560;
    /// `SZOMB` in `<sys/proc.h>`.
    const SZOMB: u8 = 5;

    /// Rows from the buffer `sysctl` filled. A trailing partial entry (there is none from the
    /// kernel) is ignored.
    pub(super) fn parse(buf: &[u8]) -> Vec<ProcRow> {
        let i32_at = |k: &[u8], o: usize| i32::from_ne_bytes(k[o..o + 4].try_into().expect("4"));
        let (entries, _partial) = buf.as_chunks::<SIZE>();
        entries
            .iter()
            .filter_map(|k| {
                let pid = u32::try_from(i32_at(k, PID)).ok()?;
                let ppid = u32::try_from(i32_at(k, PPID)).ok()?;
                let sec = i64::from_ne_bytes(k[START_SEC..START_SEC + 8].try_into().expect("8"));
                let usec = i32_at(k, START_USEC);
                Some(ProcRow {
                    pid,
                    ppid,
                    start: format!("{sec}.{usec:06}"),
                    zombie: k[STAT] == SZOMB,
                })
            })
            .collect()
    }

    /// The raw table. The size is asked for first; the table can grow before the second call,
    /// so there is headroom, and `ENOMEM` (it grew past that) is retried a few times.
    #[cfg(target_os = "macos")]
    pub(super) fn read_all() -> std::io::Result<Vec<u8>> {
        let mut last = std::io::Error::from_raw_os_error(libc::ENOMEM);
        for _ in 0..4 {
            let need = sysctl_proc_all(None)?;
            let mut buf = vec![0u8; need + need / 8 + 16 * SIZE];
            match sysctl_proc_all(Some(&mut buf)) {
                Ok(len) => {
                    buf.truncate(len);
                    return Ok(buf);
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOMEM) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// One `sysctl(KERN_PROC_ALL)` call: with no buffer, the size the table needs; with one, the
    /// bytes written to it.
    ///
    /// The `unsafe` block is the FFI call, with:
    ///
    /// - `mib`, a live local array of the three names passed as its length;
    /// - `oldp` either null (a size query) or `buf`'s pointer, and `oldlenp` pointing at a live
    ///   local holding `buf`'s length, so the kernel writes at most that many bytes into it and
    ///   stores the count it wrote;
    /// - no new value (`newp` null, `newlen` 0): this only reads.
    ///
    /// Nothing is read from `buf` here; [`parse`] reads it as plain bytes, so no layout is
    /// assumed by the type system and a wrong offset can only give a wrong number, never
    /// undefined behavior.
    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn sysctl_proc_all(buf: Option<&mut [u8]>) -> std::io::Result<usize> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
        let (ptr, mut len) = match buf {
            Some(b) => (b.as_mut_ptr().cast::<libc::c_void>(), b.len()),
            None => (std::ptr::null_mut(), 0),
        };
        // SAFETY: see the function's documentation.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                ptr,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 {
            Ok(len)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Every process below `root` in the table (not `root` itself).
pub fn descendants(root: u32, rows: &[ProcRow]) -> Vec<ProcRow> {
    let mut children: HashMap<u32, Vec<&ProcRow>> = HashMap::new();
    for r in rows {
        children.entry(r.ppid).or_default().push(r);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        for c in children.get(&p).into_iter().flatten() {
            if seen.insert(c.pid) {
                out.push((*c).clone());
                stack.push(c.pid);
            }
        }
    }
    out
}

/// The processes recorded under a session's child over its life.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    /// pid to start time.
    known: HashMap<u32, String>,
}

impl Tracker {
    /// Records the current descendants of `root`.
    pub fn refresh(&mut self, root: u32, rows: &[ProcRow]) {
        for d in descendants(root, rows) {
            self.known.insert(d.pid, d.start);
        }
        // Forget the dead, so the map follows the live set and a recycled pid is not kept.
        let live: HashMap<u32, &str> = rows.iter().map(|r| (r.pid, r.start.as_str())).collect();
        self.known
            .retain(|pid, start| live.get(pid).is_some_and(|s| s == start));
    }

    /// The recorded processes.
    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.known.keys().copied()
    }

    /// Every process to signal: the current descendants of `root`, plus the recorded ones that
    /// are still the same process (they may have been orphaned since).
    pub fn victims(&self, root: u32, rows: &[ProcRow]) -> Vec<u32> {
        let mut v: HashSet<u32> = descendants(root, rows)
            .into_iter()
            .filter(|r| !r.zombie)
            .map(|r| r.pid)
            .collect();
        for r in rows {
            if !r.zombie && self.known.get(&r.pid).is_some_and(|s| *s == r.start) {
                v.insert(r.pid);
            }
        }
        v.remove(&root);
        v.into_iter().collect()
    }
}

fn alive(pid: u32) -> bool {
    // Signal 0 only checks that the process exists (a zombie counts as existing, which the
    // callers account for with the table).
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

fn signal_all(pids: &[u32], sig: Signal) {
    for &p in pids {
        let _ = kill(Pid::from_raw(p as i32), sig);
    }
}

/// How long a kill waits for the hangup to work before it uses SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_millis(600);

/// Tears down a session's processes. Blocking: call from a blocking context.
///
/// A hangup first, as closing a terminal would send, so shells and their jobs exit cleanly;
/// then SIGKILL to whatever is left, in the group, in the tree and in the scope.
pub fn kill_session(root: u32, scope: &Scope, tracker: &Tracker, grace: Duration) {
    let rows = scan_processes();
    let victims = tracker.victims(root, &rows);
    let pgid = Pid::from_raw(root as i32);
    let _ = killpg(pgid, Signal::SIGHUP);
    let _ = killpg(pgid, Signal::SIGCONT);
    signal_all(&victims, Signal::SIGHUP);
    signal_all(&victims, Signal::SIGCONT);

    let deadline = Instant::now() + grace;
    loop {
        let table = scan_processes();
        let gone = victims
            .iter()
            .all(|v| table.iter().find(|r| r.pid == *v).is_none_or(|r| r.zombie));
        let group_gone = nix::sys::signal::killpg(pgid, None).is_err()
            || table.iter().filter(|r| r.pid == root).all(|r| r.zombie);
        if (gone && group_gone) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = killpg(pgid, Signal::SIGKILL);
    for v in &victims {
        if alive(*v) {
            let _ = kill(Pid::from_raw(*v as i32), Signal::SIGKILL);
        }
    }
    if let Scope::Systemd { unit } = scope {
        systemctl_kill("systemctl", unit, "SIGKILL", KILL_CALL_TIMEOUT);
    }
    // `kill(2)` returning says the signal was queued, not that the process has died: on a loaded
    // or swapping machine a victim can take a while to be scheduled and torn down. Callers read
    // this function's return as "the leftovers are gone" (`SessionHandle::kill`), so wait until
    // the table shows they are, or give up after `KILL_CONFIRM` (a process in uninterruptible
    // sleep can outlast any wait, and a stuck victim must not hang the teardown).
    let deadline = Instant::now() + KILL_CONFIRM;
    loop {
        let table = scan_processes();
        let left: Vec<u32> = victims
            .iter()
            .copied()
            .filter(|v| {
                table
                    .iter()
                    .find(|r| r.pid == *v)
                    .is_some_and(|r| !r.zombie)
            })
            .collect();
        if left.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(?left, "processes survived SIGKILL");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How long [`kill_session`] waits, after SIGKILL, for the processes it killed to leave the
/// process table (or become zombies).
const KILL_CONFIRM: Duration = Duration::from_secs(2);

/// How long `systemctl --user kill` may take. A user manager that does not answer must not hang
/// the teardown (it runs on a blocking thread the session waits for).
const KILL_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends `signal` to every process in the scope of `unit` with `<systemctl> --user kill`, giving
/// up after `timeout`. Failures are logged, not returned: the callers have already signalled the
/// group and the recorded tree, and there is nothing else to do.
fn systemctl_kill(systemctl: &str, unit: &str, signal: &str, timeout: Duration) {
    let mut cmd = Command::new(systemctl);
    cmd.args(["--user", "kill"])
        .arg(format!("--signal={signal}"))
        .arg(format!("{unit}.scope"));
    match output_with_timeout(cmd, timeout) {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(status = %out.status, signal, "systemctl kill failed"),
        Err(e) => tracing::warn!(error = %e, signal, "systemctl kill did not complete"),
    }
}

/// The hangup a terminal sends when it closes, to the group, the recorded tree and the scope.
/// Used when the shell exits by itself: what it left running in the background is told, not
/// killed.
pub fn hang_up(root: u32, scope: &Scope, tracker: &Tracker) {
    let rows = scan_processes();
    let victims = tracker.victims(root, &rows);
    let _ = killpg(Pid::from_raw(root as i32), Signal::SIGHUP);
    signal_all(&victims, Signal::SIGHUP);
    if let Scope::Systemd { unit } = scope {
        systemctl_kill("systemctl", unit, "SIGHUP", KILL_CALL_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_that_hangs_is_killed_at_the_timeout() {
        let mut slow = Command::new("sleep");
        slow.arg("30");
        let t = Instant::now();
        let err = output_with_timeout(slow, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5));
        let mut quick = Command::new("echo");
        quick.arg("hi");
        let out = output_with_timeout(quick, Duration::from_secs(5)).unwrap();
        assert!(out.status.success() && out.stdout.starts_with(b"hi"));
    }

    #[test]
    fn a_probe_that_prints_more_than_a_pipe_holds_is_drained_and_capped() {
        // Well over any pipe buffer on both streams. Undrained, the probe blocks on its write
        // and looks hung.
        let mut noisy = Command::new("sh");
        noisy.args([
            "-c",
            "head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2; echo done >&2",
        ]);
        let t = Instant::now();
        let out = output_with_timeout(noisy, Duration::from_secs(20)).expect("not a timeout");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), PROBE_OUTPUT_CAP, "kept up to the cap");
        assert_eq!(out.stderr.len(), PROBE_OUTPUT_CAP);
        assert!(t.elapsed() < Duration::from_secs(15));
    }

    #[test]
    fn a_probe_that_leaves_a_process_holding_its_pipes_still_returns() {
        // The background `sleep` inherits the pipes and outlives the probe.
        let mut leaky = Command::new("sh");
        leaky.args(["-c", "echo out; sleep 3 & exit 0"]);
        let t = Instant::now();
        let out = output_with_timeout(leaky, Duration::from_secs(20)).unwrap();
        assert!(out.status.success() && out.stdout.starts_with(b"out"));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    fn row(pid: u32, ppid: u32, start: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            start: start.into(),
            zombie: false,
        }
    }

    #[test]
    fn unit_names_are_valid_and_distinct_per_incarnation_and_hold_no_session_name() {
        assert_eq!(
            unit_name(0xdead_beef_1234),
            "fil-00000000000000000000deadbeef1234"
        );
        assert_ne!(unit_name(1), unit_name(2));
        // Incarnations that differ only in their high bits must not collide.
        assert_ne!(unit_name(1), unit_name(1 | (1 << 100)));
        let u = unit_name(u128::MAX);
        assert!(u.starts_with("fil-") && u[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn linger_probe_gives_up_quietly_when_it_cannot_tell() {
        // No such user (or no loginctl at all): "cannot tell", not a panic or a hang.
        assert_eq!(lingering(u32::MAX - 1), None);
    }

    #[test]
    fn a_systemctl_that_hangs_does_not_hang_the_kill() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("systemctl");
        let marker = dir.join("ran");
        // The fake records that it ran, then hangs. Without the marker a fake that could not be
        // started (ETXTBSY from a concurrent fork, a noexec temp dir) would look like a pass.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" > '{}'\nexec sleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut took = Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            systemctl_kill(
                fake.to_str().unwrap(),
                "fil-x",
                "SIGKILL",
                Duration::from_millis(300),
            );
            took = t.elapsed();
            if marker.exists() {
                break;
            }
        }
        let args = std::fs::read_to_string(&marker);
        assert_eq!(
            args.expect("the fake systemctl never ran").trim(),
            "--user kill --signal=SIGKILL fil-x.scope"
        );
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }

    const OWN_GROUP_JOB: &str = "FIL_TEST_JOB_IN_ITS_OWN_GROUP";

    /// Not a test by itself: when the test binary is started as the job below, it moves into a
    /// process group of its own (what job control does to a background job), prints its pid and
    /// waits to be killed.
    #[test]
    fn job_in_its_own_group_helper() {
        if std::env::var_os(OWN_GROUP_JOB).is_none() {
            return;
        }
        nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).unwrap();
        println!("JOB {}", std::process::id());
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Spawns `sh` as a process group leader with a background job (this test binary, running
    /// `job_in_its_own_group_helper`) in a group of its own: the shape the tree fallback exists
    /// for. Returns the shell and the job's pid.
    fn shell_with_a_job_in_its_own_group() -> (std::process::Child, u32) {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut sh = Command::new("sh")
            .args([
                "-c",
                "\"$0\" --exact proc::tests::job_in_its_own_group_helper --nocapture & wait",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(OWN_GROUP_JOB, "1")
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut job = None;
        for line in std::io::BufReader::new(sh.stdout.as_mut().unwrap()).lines() {
            if let Some(pid) = line.unwrap().strip_prefix("JOB ") {
                job = Some(pid.trim().parse::<u32>().unwrap());
                break;
            }
        }
        let job = job.expect("the job did not report its pid");
        let pgid = |pid: u32| {
            let out = Command::new("ps")
                .args(["-o", "pgid=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_ne!(
            pgid(job),
            pgid(sh.id()),
            "the job must be in a group of its own"
        );
        (sh, job)
    }

    /// Teardown must act on the table as it is now. A job that moved into a group of its own is
    /// only found through the table; `killpg` of the shell's group does not reach it. The shared
    /// table is poisoned with an empty one that is still "fresh", so a teardown that read it
    /// (`list_processes`) would find nobody.
    fn teardown_ignores_the_shared_table(teardown: impl Fn(u32)) {
        let (mut sh, job) = shell_with_a_job_in_its_own_group();
        *SHARED_TABLE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), std::sync::Arc::new(Vec::new())));
        teardown(sh.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(job) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survived = alive(job);
        let _ = kill(Pid::from_raw(job as i32), Signal::SIGKILL);
        let _ = sh.kill();
        let _ = sh.wait();
        *SHARED_TABLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        assert!(!survived, "the job in its own group was not reached");
    }

    #[test]
    fn hang_up_reads_a_fresh_process_table() {
        teardown_ignores_the_shared_table(|root| {
            hang_up(root, &Scope::Tree, &Tracker::default());
        });
    }

    #[test]
    fn kill_session_reads_a_fresh_process_table() {
        teardown_ignores_the_shared_table(|root| {
            kill_session(
                root,
                &Scope::Tree,
                &Tracker::default(),
                Duration::from_millis(200),
            );
        });
    }

    fn table(n: u32) -> Vec<ProcRow> {
        vec![row(n, 1, "a")]
    }

    #[test]
    fn the_shared_process_table_is_scanned_once_per_max_age() {
        let slot: TableSlot = Mutex::new(None);
        let age = Duration::from_secs(2);
        let t0 = Instant::now();
        let mut scans = 0;
        let mut ask = |at: Instant| {
            table_from(&slot, age, at, || {
                scans += 1;
                table(scans)
            })
            .first()
            .unwrap()
            .pid
        };
        assert_eq!(ask(t0), 1, "the first caller scans");
        assert_eq!(
            ask(t0 + Duration::from_millis(1900)),
            1,
            "a second session shares it"
        );
        assert_eq!(
            ask(t0 + Duration::from_secs(2)),
            1,
            "the limit itself is still fresh"
        );
        assert_eq!(
            ask(t0 + Duration::from_millis(2100)),
            2,
            "past the age it is scanned again"
        );
        assert_eq!(ask(t0 + Duration::from_millis(2200)), 2);
        // A caller whose clock reading predates the stored scan (it waited on the lock) shares.
        assert_eq!(ask(t0), 2);
    }

    #[test]
    fn a_table_from_before_the_session_is_not_shared_with_its_first_recording() {
        let slot: TableSlot = Mutex::new(None);
        let age = Duration::from_secs(2);
        let t0 = Instant::now();
        // Another session scanned at t0; this session was spawned 500 ms later.
        let first = table_from(&slot, age, t0, || table(1));
        assert_eq!(first[0].pid, 1);
        let spawned = t0 + Duration::from_millis(500);
        let now = t0 + Duration::from_millis(600);
        let got = table_from_since(&slot, age, Some(spawned), now, || table(2));
        assert_eq!(
            got[0].pid, 2,
            "the old table predates the session: scan again"
        );
        // That scan is after the spawn, so a second session spawned before it may share it.
        let again = table_from_since(&slot, age, Some(spawned), now, || table(3));
        assert_eq!(again[0].pid, 2);
        // Without a floor the old behavior stands.
        let plain = table_from(&slot, age, now, || table(4));
        assert_eq!(plain[0].pid, 2);
    }

    #[test]
    fn concurrent_sessions_share_one_scan() {
        let slot: TableSlot = Mutex::new(None);
        let scans = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    table_from(&slot, Duration::from_secs(60), Instant::now(), || {
                        scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        table(1)
                    });
                });
            }
        });
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn linger_output() {
        assert!(parse_linger("Linger=yes\n"));
        assert!(parse_linger("Name=x\nLinger=yes\nState=active\n"));
        assert!(!parse_linger("Linger=no\n"));
        assert!(!parse_linger(""));
    }

    #[test]
    fn systemd_run_command_line() {
        let argv = systemd_run_argv(
            &unit_name(1),
            SCOPE_DESCRIPTION,
            &["/bin/zsh".into(), "-l".into()],
        );
        assert_eq!(
            argv,
            [
                "systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit=fil-00000000000000000000000000000001",
                "--description=fil session",
                "--",
                "/bin/zsh",
                "-l"
            ]
        );
    }

    #[test]
    fn proc_stat_survives_awkward_command_names() {
        let stat = "1234 (my (weird) cmd) S 99 1234 1234 34816 1234 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 100 18446744073709551615";
        let r = parse_proc_stat(1234, stat).unwrap();
        assert_eq!((r.ppid, r.start.as_str(), r.zombie), (99, "987654", false));
        let z = parse_proc_stat(5, "5 (x) Z 1 5 5 0 -1 4 0 0 0 0 0 0 0 0 20 0 1 0 42 0 0").unwrap();
        assert!(z.zombie);
        assert_eq!(parse_proc_stat(1, "garbage"), None);
        // The parent pid alone needs no start time.
        assert_eq!(parse_ppid("1 (a) b) c) S 7 1"), Some(7));
        assert_eq!(parse_ppid("1 (a"), None);
    }

    #[test]
    fn ps_output() {
        let out = "    1     0 Ss   Mon Nov  2 10:00:00 2026\n  500     1 S    Mon Nov  2 10:01:02 2026\n  501   500 Z    Mon Nov  2 10:01:03 2026\nnot a row\n";
        let rows = parse_ps(out);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].start, "Mon Nov 2 10:01:02 2026");
        assert!(rows[2].zombie);
    }

    #[test]
    fn kinfo_proc_entries_are_read_at_their_offsets() {
        let entry = |pid: i32, ppid: i32, stat: u8, sec: i64, usec: i32| {
            let mut k = vec![0xAAu8; kinfo::SIZE];
            k[0..8].copy_from_slice(&sec.to_ne_bytes());
            k[8..12].copy_from_slice(&usec.to_ne_bytes());
            k[36] = stat;
            k[40..44].copy_from_slice(&pid.to_ne_bytes());
            k[560..564].copy_from_slice(&ppid.to_ne_bytes());
            k
        };
        let mut buf = entry(500, 1, 2, 1_794_000_000, 42);
        buf.extend(entry(501, 500, 5, 1_794_000_001, 999_999));
        // A negative pid is not a process; a trailing partial entry is ignored.
        buf.extend(entry(-1, 1, 2, 0, 0));
        buf.extend([0u8; 10]);
        let rows = kinfo::parse(&buf);
        assert_eq!(
            rows,
            [
                ProcRow {
                    pid: 500,
                    ppid: 1,
                    start: "1794000000.000042".into(),
                    zombie: false
                },
                ProcRow {
                    pid: 501,
                    ppid: 500,
                    start: "1794000001.999999".into(),
                    zombie: true
                },
            ]
        );
    }

    /// The offsets in `kinfo` against the real kernel: this process, a running child and a
    /// zombie child are found with the right parents and states, and the table is the one `ps`
    /// shows.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_table_matches_ps() {
        let mut zombie = Command::new("true").spawn().unwrap();
        let mut running = Command::new("sleep").arg("30").spawn().unwrap();
        let me = std::process::id();
        // `true` has exited (and is a zombie until waited for) once `ps` no longer shows it
        // running; give it up to 5 s on a loaded machine.
        let deadline = Instant::now() + Duration::from_secs(5);
        let rows = loop {
            let rows = scan_processes();
            if rows.iter().any(|r| r.pid == zombie.id() && r.zombie) || Instant::now() > deadline {
                break rows;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let find = |pid: u32| rows.iter().find(|r| r.pid == pid).cloned();
        let z = find(zombie.id()).expect("the zombie child is listed");
        assert!(z.zombie && z.ppid == me, "{z:?}");
        let r = find(running.id()).expect("the running child is listed");
        assert!(!r.zombie && r.ppid == me, "{r:?}");
        let mine = find(me).expect("this process is listed");
        assert!(!mine.zombie);
        // The start time is this process's: within a minute of now, by the wall clock.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let started: f64 = r.start.parse().unwrap();
        assert!((now - started).abs() < 60.0, "{} vs {now}", r.start);
        // Every pid `ps` shows a moment later that also is in the table has the same parent.
        let out = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,stat=,lstart="])
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let ps = parse_ps(&String::from_utf8_lossy(&out.stdout));
        let ours: HashMap<u32, u32> = rows.iter().map(|r| (r.pid, r.ppid)).collect();
        let common: Vec<_> = ps.iter().filter(|p| ours.contains_key(&p.pid)).collect();
        assert!(
            common.len() * 10 >= ps.len() * 9,
            "{} of {} ps rows",
            common.len(),
            ps.len()
        );
        let moved = common.iter().filter(|p| ours[&p.pid] != p.ppid).count();
        assert!(moved * 100 <= common.len(), "{moved} parents differ");
        let _ = running.kill();
        let _ = running.wait();
        let _ = zombie.wait();
    }

    #[test]
    fn descendants_walk_the_tree_and_survive_cycles() {
        let rows = [
            row(10, 1, "a"),
            row(11, 10, "a"),
            row(12, 11, "a"),
            row(13, 10, "a"),
            row(20, 1, "a"),
            row(21, 20, "a"),
        ];
        let mut d: Vec<u32> = descendants(10, &rows).iter().map(|r| r.pid).collect();
        d.sort();
        assert_eq!(d, [11, 12, 13]);
        // A table with a cycle (a pid reused mid-scan) must not loop.
        let cyc = [row(1, 2, "a"), row(2, 1, "a")];
        assert_eq!(descendants(1, &cyc).len(), 1);
    }

    #[test]
    fn tracker_remembers_orphans_but_not_recycled_pids() {
        let mut t = Tracker::default();
        t.refresh(
            10,
            &[row(10, 1, "a"), row(11, 10, "s11"), row(12, 11, "s12")],
        );
        // 11 died; 12 was orphaned to init and is still the same process.
        let table = [row(10, 1, "a"), row(12, 1, "s12"), row(11, 777, "other")];
        let mut v = t.victims(10, &table);
        v.sort();
        assert_eq!(v, [12], "11 is a different process now");
        // An unrelated process is never a victim.
        assert!(
            !t.victims(10, &[row(10, 1, "a"), row(99, 1, "x")])
                .contains(&99)
        );
    }

    #[test]
    fn the_process_table_includes_this_process_and_its_child() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let rows = scan_processes();
        let me = std::process::id();
        let found = rows
            .iter()
            .find(|r| r.pid == child.id())
            .expect("child listed");
        assert_eq!(found.ppid, me);
        assert!(descendants(me, &rows).iter().any(|r| r.pid == child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    const LINUX: Facts = Facts {
        linux: true,
        systemd_run_on_path: true,
        xdg_runtime_dir_set: true,
    };

    #[test]
    fn scope_decisions() {
        let yes = || true;
        let no = || false;
        let ok = || Ok(());
        let bad = || Err("no bus".to_string());
        let never = || -> Result<(), String> { panic!("the trial must not run") };

        assert_eq!(
            decide_scope(ScopeMode::Tree, LINUX, yes, ok),
            Decision::Tree
        );
        // Auto: only with everything in place.
        assert_eq!(
            decide_scope(ScopeMode::Auto, LINUX, yes, ok),
            Decision::Systemd
        );
        assert_eq!(
            decide_scope(ScopeMode::Auto, LINUX, no, never),
            Decision::Tree
        );
        let mac = Facts {
            linux: false,
            ..LINUX
        };
        assert_eq!(
            decide_scope(ScopeMode::Auto, mac, || panic!("no probe off Linux"), never),
            Decision::Tree
        );
        let no_run = Facts {
            systemd_run_on_path: false,
            ..LINUX
        };
        assert_eq!(
            decide_scope(ScopeMode::Auto, no_run, yes, never),
            Decision::Tree
        );
        let no_dir = Facts {
            xdg_runtime_dir_set: false,
            ..LINUX
        };
        assert_eq!(
            decide_scope(ScopeMode::Auto, no_dir, yes, never),
            Decision::Tree
        );
        // Lingering on but the manager does not answer: the tree, with a warning.
        assert_eq!(
            decide_scope(ScopeMode::Auto, LINUX, yes, bad),
            Decision::TreeWithWarning("no bus".into())
        );
        // Explicit: no silent fallback.
        assert_eq!(
            decide_scope(ScopeMode::Systemd, LINUX, no, ok),
            Decision::Systemd
        );
        assert_eq!(
            decide_scope(ScopeMode::Systemd, LINUX, yes, bad),
            Decision::Fail("no bus".into())
        );
        assert!(matches!(
            decide_scope(ScopeMode::Systemd, no_run, yes, never),
            Decision::Fail(_)
        ));
        assert!(matches!(
            decide_scope(ScopeMode::Systemd, no_dir, yes, never),
            Decision::Fail(_)
        ));
    }
}
