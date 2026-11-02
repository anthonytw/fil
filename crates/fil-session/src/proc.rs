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

/// The transient unit name for a session: `fil-<name>-<8 hex digits of the incarnation>`.
pub fn unit_name(name: &str, incarnation: u128) -> String {
    format!("fil-{name}-{:08x}", incarnation as u32)
}

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

/// Blocking: runs `loginctl`. Call from a blocking context.
fn linger_enabled() -> bool {
    let uid = nix::unistd::getuid();
    Command::new("loginctl")
        .args(["show-user", &uid.to_string(), "-p", "Linger"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| parse_linger(&String::from_utf8_lossy(&o.stdout)))
}

/// Decides the scope for a new session. Blocking (it may run `loginctl`).
pub fn resolve_scope(mode: ScopeMode, name: &str, incarnation: u128) -> Scope {
    let unit = || Scope::Systemd {
        unit: unit_name(name, incarnation),
    };
    match mode {
        ScopeMode::Tree => Scope::Tree,
        ScopeMode::Systemd => unit(),
        ScopeMode::Auto => {
            if cfg!(target_os = "linux")
                && on_path("systemd-run")
                && std::env::var_os("XDG_RUNTIME_DIR").is_some()
                && linger_enabled()
            {
                unit()
            } else {
                Scope::Tree
            }
        }
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

/// Parses `/proc/<pid>/stat`. The command name may contain anything, including spaces and
/// parentheses, so the fields are counted from the *last* `)`.
pub fn parse_proc_stat(pid: u32, stat: &str) -> Option<ProcRow> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let state = f.next()?;
    let ppid: u32 = f.next()?.parse().ok()?;
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

/// The process table. Blocking.
pub fn list_processes() -> Vec<ProcRow> {
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
    Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat=,lstart="])
        .env("LC_ALL", "C")
        .output()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
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
    let rows = list_processes();
    let victims = tracker.victims(root, &rows);
    let pgid = Pid::from_raw(root as i32);
    let _ = killpg(pgid, Signal::SIGHUP);
    let _ = killpg(pgid, Signal::SIGCONT);
    signal_all(&victims, Signal::SIGHUP);
    signal_all(&victims, Signal::SIGCONT);

    let deadline = Instant::now() + grace;
    loop {
        let table = list_processes();
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
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "kill",
                "--signal=SIGKILL",
                &format!("{unit}.scope"),
            ])
            .output();
    }
}

/// The hangup a terminal sends when it closes, to the group and the recorded tree. Used when the
/// shell exits by itself: what it left running in the background is told, not killed.
pub fn hang_up(root: u32, tracker: &Tracker) {
    let rows = list_processes();
    let victims = tracker.victims(root, &rows);
    let _ = killpg(Pid::from_raw(root as i32), Signal::SIGHUP);
    signal_all(&victims, Signal::SIGHUP);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, start: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            start: start.into(),
            zombie: false,
        }
    }

    #[test]
    fn unit_names_are_valid_and_distinct_per_incarnation() {
        assert_eq!(unit_name("main", 0xdead_beef_1234), "fil-main-beef1234");
        assert_ne!(unit_name("main", 1), unit_name("main", 2));
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
            "fil-main-00000001",
            "fil session main",
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
                "--unit=fil-main-00000001",
                "--description=fil session main",
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
        let rows = list_processes();
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
}
