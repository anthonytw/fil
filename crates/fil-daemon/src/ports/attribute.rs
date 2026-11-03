//! Which of the user's processes holds a listening socket, what it is called, where it runs, and
//! which session started it.
//!
//! Everything here reads a `/proc` root (the real one, or a fake one in tests) and looks only at
//! the user's own processes: [`ProcRoot::owner`] (the owner of `ROOT/<pid>`, which is the
//! process's) is checked before anything inside `ROOT/<pid>` is opened. Another user's processes
//! are not read at all, even where the permissions would allow it. The scan is bounded in
//! processes, file descriptors and time ([`ScanLimits`]): a process with a million descriptors, or
//! a host with a hundred thousand processes, costs a bounded amount and leaves the rest
//! unattributed.
//!
//! These functions block: call them from `spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fil_proto::msg::{PortsPath, ProcessInfo, ProcessName, SessionName};

/// The bounds of one scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    /// The user's processes whose descriptors are looked at.
    pub max_procs: usize,
    /// Descriptors looked at in one process.
    pub max_fds_per_proc: usize,
    /// Descriptors looked at over the whole scan.
    pub max_fds: usize,
    /// Time for the whole discovery (the socket tables, the scan, the names).
    pub budget: Duration,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_procs: 4096,
            max_fds_per_proc: 4096,
            max_fds: 65536,
            budget: Duration::from_millis(250),
        }
    }
}

/// How deep the process-tree walk from a listener to a session's process goes.
const MAX_DEPTH: usize = 64;
/// The most read of `comm`, `cgroup` and `stat`.
const MAX_COMM: u64 = 64;
const MAX_CGROUP: u64 = 4096;
const MAX_STAT: u64 = 4096;

/// A `/proc` directory.
#[derive(Clone, Debug)]
pub struct ProcRoot(PathBuf);

impl ProcRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The uid that owns process `pid` (the owner of `ROOT/<pid>`). This is the filter: nothing
    /// inside `ROOT/<pid>` is read unless it is the user's.
    fn owner(&self, pid: u32) -> Option<u32> {
        std::fs::metadata(self.0.join(pid.to_string()))
            .ok()
            .map(|m| m.uid())
    }

    /// `ROOT/<pid>/<file>`, if `pid` is `uid`'s.
    fn own(&self, uid: u32, pid: u32, file: &str) -> Option<PathBuf> {
        (self.owner(pid) == Some(uid)).then(|| self.0.join(pid.to_string()).join(file))
    }
}

/// What [`inode_owners`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Owners {
    /// Socket inode to the pid holding it (the first found).
    pub by_inode: HashMap<u64, u32>,
    /// A bound was reached before every wanted inode was found.
    pub truncated: bool,
}

/// Finds which of `uid`'s processes hold the sockets in `wanted`, looking first at `first`
/// (a session's known processes), then at every other process of `uid`, until every inode is
/// found, a bound is reached or `deadline` passes.
pub fn inode_owners(
    root: &ProcRoot,
    uid: u32,
    wanted: &HashSet<u64>,
    first: &[u32],
    limits: &ScanLimits,
    deadline: Instant,
) -> Owners {
    let mut out = Owners::default();
    if wanted.is_empty() {
        return out;
    }
    let mut visited = HashSet::new();
    let mut procs = 0usize;
    let mut fds = 0usize;
    let rest = std::fs::read_dir(root.path())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok());
    for pid in first.iter().copied().chain(rest) {
        if !visited.insert(pid) {
            continue;
        }
        if Instant::now() >= deadline {
            out.truncated = true;
            break;
        }
        // The uid filter, before anything in the process's directory is opened.
        let Some(fd_dir) = root.own(uid, pid, "fd") else {
            continue;
        };
        procs += 1;
        if procs > limits.max_procs {
            out.truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for (n, e) in entries.enumerate() {
            if n == limits.max_fds_per_proc || fds == limits.max_fds {
                out.truncated = true;
                break;
            }
            fds += 1;
            if n % 64 == 63 && Instant::now() >= deadline {
                out.truncated = true;
                break;
            }
            let Some(inode) = e
                .ok()
                .and_then(|e| std::fs::read_link(e.path()).ok())
                .and_then(|l| socket_inode(&l))
            else {
                continue;
            };
            if wanted.contains(&inode) {
                out.by_inode.entry(inode).or_insert(pid);
            }
        }
        if out.by_inode.len() == wanted.len() || fds == limits.max_fds {
            break;
        }
    }
    if out.by_inode.len() == wanted.len() {
        out.truncated = false;
    }
    out
}

/// `socket:[12345]` to 12345.
fn socket_inode(link: &Path) -> Option<u64> {
    link.to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// At most `cap` bytes of a file.
fn read_capped(path: &Path, cap: u64) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(cap)
        .read_to_end(&mut v)
        .ok()?;
    Some(v)
}

/// The name (`comm`) and working directory (with `~` for `home`) of `uid`'s process `pid`;
/// `None` if it is not `uid`'s or is gone.
pub fn process_info(
    root: &ProcRoot,
    uid: u32,
    pid: u32,
    home: Option<&Path>,
) -> Option<ProcessInfo> {
    let comm = read_capped(&root.own(uid, pid, "comm")?, MAX_COMM)?;
    let comm = String::from_utf8_lossy(&comm);
    let cwd = root
        .own(uid, pid, "cwd")
        .and_then(|p| std::fs::read_link(p).ok())
        .map(|p| PortsPath::capped(&tilde(&p, home)));
    Some(ProcessInfo {
        pid,
        name: ProcessName::capped(comm.trim_end_matches('\n')),
        cwd,
    })
}

/// `path` with `home` written as `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|h| h.is_absolute() && h.parent().is_some())
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.to_string_lossy())
        };
    }
    path.to_string_lossy().into_owned()
}

/// How a session's processes are told apart from the user's others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Membership {
    /// The session runs in a systemd scope: this unit (`fil-<hex>`, without `.scope`). A process
    /// is in it when its `cgroup` path runs through `<unit>.scope`.
    Unit(String),
    /// The session's processes as recorded (its child and the descendants seen so far,
    /// `SessionHandle::pids()`). A process is in the session when it or one of its ancestors is
    /// one of them.
    Pids(Vec<u32>),
}

/// Session membership for one scan: the pid walk and the cgroup reads are cached across the
/// listeners of a scan.
pub struct Sessions<'a> {
    root: &'a ProcRoot,
    uid: u32,
    units: Vec<(String, &'a SessionName)>,
    pids: HashMap<u32, &'a SessionName>,
    parent: HashMap<u32, Option<u32>>,
}

impl<'a> Sessions<'a> {
    pub fn new(root: &'a ProcRoot, uid: u32, sessions: &'a [(SessionName, Membership)]) -> Self {
        let mut units = Vec::new();
        let mut pids = HashMap::new();
        for (name, m) in sessions {
            match m {
                Membership::Unit(u) => units.push((format!("{u}.scope"), name)),
                Membership::Pids(p) => {
                    for pid in p {
                        pids.entry(*pid).or_insert(name);
                    }
                }
            }
        }
        Self {
            root,
            uid,
            units,
            pids,
            parent: HashMap::new(),
        }
    }

    /// The pids that the scan should look at first.
    pub fn known_pids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.pids.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// The session `uid`'s process `pid` was started in, if any.
    pub fn of(&mut self, pid: u32) -> Option<SessionName> {
        if !self.units.is_empty()
            && let Some(path) = self.root.own(self.uid, pid, "cgroup")
            && let Some(text) = read_capped(&path, MAX_CGROUP)
        {
            let text = String::from_utf8_lossy(&text);
            for line in text.lines() {
                // `hierarchy:controllers:path`; the path may itself contain ':'.
                let Some(path) = line.splitn(3, ':').nth(2) else {
                    continue;
                };
                for (unit, name) in &self.units {
                    if path.split('/').any(|c| c == unit) {
                        return Some((*name).clone());
                    }
                }
            }
        }
        if self.pids.is_empty() {
            return None;
        }
        let mut p = pid;
        for _ in 0..MAX_DEPTH {
            if let Some(name) = self.pids.get(&p) {
                return Some((*name).clone());
            }
            p = self.parent(p)?;
        }
        None
    }

    /// The parent of `uid`'s process `pid`; `None` at the top, for another user's process (the
    /// walk stops there) and for anything unreadable.
    fn parent(&mut self, pid: u32) -> Option<u32> {
        if let Some(p) = self.parent.get(&pid) {
            return *p;
        }
        let ppid = self
            .root
            .own(self.uid, pid, "stat")
            .and_then(|p| read_capped(&p, MAX_STAT))
            .and_then(|s| parse_ppid(&String::from_utf8_lossy(&s)))
            .filter(|&pp| pp != 0 && pp != pid);
        self.parent.insert(pid, ppid);
        ppid
    }
}

/// The parent pid in `/proc/<pid>/stat`. The name may hold anything, spaces and parentheses
/// included, so the fields are counted from the last `)`.
fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_ascii_whitespace();
    let _state = f.next()?;
    f.next()?.parse().ok()
}

/// A fake `/proc` for tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::ProcRoot;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fake `/proc` in a temporary directory, owned (as every file a test makes is) by the user
    /// running the test.
    pub(crate) struct FakeProc {
        pub dir: PathBuf,
    }

    impl FakeProc {
        pub fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "fil-ports-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("net")).unwrap();
            Self { dir }
        }

        pub fn process(&self, pid: u32, ppid: u32, comm: &str, cwd: &str, sockets: &[u64]) {
            let p = self.dir.join(pid.to_string());
            std::fs::create_dir_all(p.join("fd")).unwrap();
            std::fs::write(p.join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(
                p.join("stat"),
                format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 12345 0 0\n"),
            )
            .unwrap();
            std::fs::write(
                p.join("cgroup"),
                "0::/user.slice/user-1000.slice/session-3.scope\n",
            )
            .unwrap();
            symlink(cwd, p.join("cwd")).unwrap();
            symlink("/dev/null", p.join("fd/0")).unwrap();
            symlink("pipe:[999]", p.join("fd/1")).unwrap();
            for (i, s) in sockets.iter().enumerate() {
                symlink(format!("socket:[{s}]"), p.join(format!("fd/{}", i + 3))).unwrap();
            }
        }

        pub fn cgroup(&self, pid: u32, path: &str) {
            std::fs::write(
                self.dir.join(pid.to_string()).join("cgroup"),
                format!("0::{path}\n"),
            )
            .unwrap();
        }

        /// `net/tcp` or `net/tcp6`.
        pub fn net(&self, file: &str, text: &str) {
            std::fs::write(self.dir.join("net").join(file), text).unwrap();
        }

        pub fn root(&self) -> ProcRoot {
            ProcRoot::new(&self.dir)
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeProc;
    use super::*;

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn inodes_map_to_the_processes_that_hold_them() {
        let f = FakeProc::new("map");
        f.process(100, 1, "vite", "/home/u/app", &[40127, 40128]);
        f.process(200, 1, "python3", "/home/u/tmp", &[40200]);
        f.process(300, 1, "bash", "/home/u", &[]);
        let wanted = HashSet::from([40127, 40200, 77777]);
        let o = inode_owners(
            &f.root(),
            me(),
            &wanted,
            &[],
            &ScanLimits::default(),
            later(),
        );
        assert_eq!(o.by_inode, HashMap::from([(40127, 100), (40200, 200)]));
    }

    /// The uid filter on processes (mutation: skip the owner check in `ProcRoot::own`, and this
    /// fails). The fake processes are the test user's, so asking as anyone else must find, and
    /// read, nothing.
    #[test]
    fn another_users_processes_are_not_read() {
        let f = FakeProc::new("uid");
        f.process(100, 1, "vite", "/home/u/app", &[40127]);
        let them = me().wrapping_add(1);
        let wanted = HashSet::from([40127]);
        let o = inode_owners(
            &f.root(),
            them,
            &wanted,
            &[100],
            &ScanLimits::default(),
            later(),
        );
        assert!(o.by_inode.is_empty(), "{o:?}");
        assert_eq!(process_info(&f.root(), them, 100, None), None);
        let sessions = [(
            SessionName::new("s").unwrap(),
            Membership::Unit("fil-1".into()),
        )];
        f.cgroup(100, "/user.slice/fil-1.scope");
        assert_eq!(Sessions::new(&f.root(), them, &sessions).of(100), None);
    }

    #[test]
    fn name_and_working_directory_with_tilde() {
        let f = FakeProc::new("info");
        f.process(100, 1, "vite", "/home/u/app", &[]);
        f.process(
            101,
            1,
            "a-name-far-longer-than-the-kernel-allows-here",
            "/home/u",
            &[],
        );
        f.process(102, 1, "x", "/srv/data (deleted)", &[]);
        let home = Path::new("/home/u");
        let i = process_info(&f.root(), me(), 100, Some(home)).unwrap();
        assert_eq!((i.pid, i.name.as_str()), (100, "vite"));
        assert_eq!(i.cwd.unwrap().as_str(), "~/app");
        let i = process_info(&f.root(), me(), 101, Some(home)).unwrap();
        assert_eq!(i.name.as_str().len(), 32);
        assert_eq!(i.cwd.unwrap().as_str(), "~");
        let i = process_info(&f.root(), me(), 102, Some(home)).unwrap();
        assert_eq!(i.cwd.unwrap().as_str(), "/srv/data (deleted)");
        assert_eq!(process_info(&f.root(), me(), 999, Some(home)), None);
        // `/home/user2` is not under `/home/u`.
        assert_eq!(tilde(Path::new("/home/u2/x"), Some(home)), "/home/u2/x");
        assert_eq!(tilde(Path::new("/x"), Some(Path::new("/"))), "/x");
    }

    #[test]
    fn sessions_by_systemd_scope_and_by_process_tree() {
        let f = FakeProc::new("sess");
        // Session a: scope fil-aa. Session b: tree rooted at 200.
        f.process(100, 1, "vite", "/", &[]);
        f.cgroup(
            100,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/fil-aa.scope",
        );
        f.process(110, 100, "esbuild", "/", &[]);
        f.cgroup(
            110,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/fil-aa.scope/sub",
        );
        f.process(200, 1, "bash", "/", &[]);
        f.process(210, 200, "make", "/", &[]);
        f.process(220, 210, "server", "/", &[]);
        f.process(300, 1, "postgres", "/", &[]);
        // A parent loop, and a unit name that only prefixes the real one.
        f.process(400, 401, "loop", "/", &[]);
        f.process(401, 400, "loop", "/", &[]);
        f.process(500, 1, "other", "/", &[]);
        f.cgroup(500, "/user.slice/fil-aaa.scope");
        let a = SessionName::new("a").unwrap();
        let b = SessionName::new("b").unwrap();
        let list = [
            (a.clone(), Membership::Unit("fil-aa".into())),
            (b.clone(), Membership::Pids(vec![200])),
        ];
        let root = f.root();
        let mut s = Sessions::new(&root, me(), &list);
        assert_eq!(s.known_pids(), vec![200]);
        assert_eq!(s.of(100), Some(a.clone()));
        assert_eq!(s.of(110), Some(a));
        assert_eq!(s.of(200), Some(b.clone()));
        assert_eq!(s.of(220), Some(b));
        assert_eq!(s.of(300), None);
        assert_eq!(s.of(400), None);
        assert_eq!(s.of(500), None);
        assert_eq!(s.of(999), None);
    }

    #[test]
    fn the_scan_is_bounded() {
        let f = FakeProc::new("bound");
        let socks: Vec<u64> = (1..=50).collect();
        f.process(100, 1, "many", "/", &socks);
        f.process(200, 1, "late", "/", &[9000]);
        // 77777 is held by no one, so the scan never ends early by finding everything.
        let wanted = HashSet::from([9000, 77777]);
        let limits = ScanLimits {
            max_fds_per_proc: 10,
            ..ScanLimits::default()
        };
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later());
        assert!(o.truncated);
        assert_eq!(
            o.by_inode.get(&9000),
            Some(&200),
            "the next process is still scanned"
        );
        let wanted = HashSet::from([50, 9000]);
        let limits = ScanLimits {
            max_procs: 1,
            ..ScanLimits::default()
        };
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later());
        assert!(o.truncated);
        assert_eq!(o.by_inode, HashMap::from([(50, 100)]));
        let limits = ScanLimits {
            max_fds: 20,
            ..ScanLimits::default()
        };
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later());
        assert!(o.truncated);
        assert!(!o.by_inode.contains_key(&9000));
        // A deadline already past reads nothing.
        let o = inode_owners(
            &f.root(),
            me(),
            &wanted,
            &[],
            &ScanLimits::default(),
            Instant::now(),
        );
        assert!(o.truncated && o.by_inode.is_empty());
    }

    #[test]
    fn odd_proc_content_never_panics() {
        assert_eq!(parse_ppid("1 (a) S 7 1"), Some(7));
        assert_eq!(parse_ppid("1 (a) b) c) S 7 1"), Some(7));
        assert_eq!(parse_ppid("1 (a"), None);
        assert_eq!(parse_ppid(")"), None);
        assert_eq!(parse_ppid("1 (a) S x"), None);
        assert_eq!(socket_inode(Path::new("socket:[12]")), Some(12));
        assert_eq!(socket_inode(Path::new("socket:[]")), None);
        assert_eq!(socket_inode(Path::new("socket:[-1]")), None);
        assert_eq!(socket_inode(Path::new("anon_inode:[eventfd]")), None);
        let f = FakeProc::new("odd");
        f.process(100, 1, "x", "/", &[]);
        std::fs::write(f.dir.join("100/stat"), b"\xff\xfe garbage").unwrap();
        std::fs::write(f.dir.join("100/comm"), b"\xff\x1b[31m\n").unwrap();
        std::fs::write(f.dir.join("100/cgroup"), b"no colons here\n::\n").unwrap();
        std::fs::create_dir_all(f.dir.join("notapid")).unwrap();
        let root = f.root();
        let i = process_info(&root, me(), 100, None).unwrap();
        assert_eq!(
            i.name.as_str(),
            "\u{fffd}\u{1b}[31m",
            "kept as data, cut, never trusted"
        );
        let list = [(
            SessionName::new("a").unwrap(),
            Membership::Unit("fil-aa".into()),
        )];
        assert_eq!(Sessions::new(&root, me(), &list).of(100), None);
        let list = [(SessionName::new("a").unwrap(), Membership::Pids(vec![5]))];
        assert_eq!(Sessions::new(&root, me(), &list).of(100), None);
    }
}
