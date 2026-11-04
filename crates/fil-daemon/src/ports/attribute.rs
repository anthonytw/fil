//! Which of the user's processes holds a listening socket, what it is called, where it runs, and
//! which session started it.
//!
//! Everything here reads a `/proc` root (the real one, or a fake one in tests) and looks only at
//! the user's own processes: [`ProcRoot::open`] checks the owner of `ROOT/<pid>` (the process's)
//! on the open directory before anything inside it is opened, and everything else is read
//! relative to that directory. Another user's processes
//! are not read at all, even where the permissions would allow it. The scan is bounded in
//! processes, file descriptors and time ([`ScanLimits`]): a process with a million descriptors, or
//! a host with a hundred thousand processes, costs a bounded amount and leaves the rest
//! unattributed.
//!
//! These functions block: call them from `spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use std::cell::Cell;
use std::rc::Rc;

use rustix::fs::{CWD, Dir, Mode, OFlags, fstat, openat, readlinkat};
use rustix::io::Errno;

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
pub struct ProcRoot {
    path: PathBuf,
    /// An open failed for want of descriptors (`EMFILE`, `ENFILE`): what was not opened is
    /// missing from the answer, which is then incomplete.
    exhausted: Rc<Cell<bool>>,
}

/// `r`, noting in `exhausted` a failure for want of descriptors.
fn opened<T>(r: rustix::io::Result<T>, exhausted: &Cell<bool>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            if e == Errno::MFILE || e == Errno::NFILE {
                exhausted.set(true);
            }
            None
        }
    }
}

impl ProcRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            exhausted: Rc::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an open failed for want of descriptors since this root was made.
    pub fn exhausted(&self) -> bool {
        self.exhausted.get()
    }

    /// `ROOT/<pid>`, opened, if it is `uid`'s. This is the filter: the owner is read from the
    /// open directory (`fstat`) before anything in it is opened, and everything after is read
    /// relative to it. A `/proc/<pid>` descriptor stays bound to the process it was opened for
    /// (reads fail once it exits), so a pid reused by another user in the meantime is never read
    /// in its place.
    pub fn open(&self, uid: u32, pid: u32) -> Option<ProcDir> {
        let fd = opened(
            openat(
                CWD,
                self.path.join(pid.to_string()),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            ),
            &self.exhausted,
        )?;
        (fstat(&fd).ok()?.st_uid == uid).then(|| ProcDir {
            fd,
            pid,
            exhausted: self.exhausted.clone(),
        })
    }
}

/// One of the user's processes' `/proc/<pid>`, open. Keep it only while reading that process:
/// a scan holds a few of these at a time, never one per process.
#[derive(Debug)]
pub struct ProcDir {
    fd: OwnedFd,
    pid: u32,
    exhausted: Rc<Cell<bool>>,
}

impl ProcDir {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// At most `cap` bytes of the file `name` in it.
    fn read(&self, name: &str, cap: u64) -> Option<Vec<u8>> {
        let fd = opened(
            openat(
                &self.fd,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            ),
            &self.exhausted,
        )?;
        let mut v = Vec::new();
        std::fs::File::from(fd).take(cap).read_to_end(&mut v).ok()?;
        Some(v)
    }

    /// The target of the link `name` in it (never followed).
    fn readlink(&self, name: &str) -> Option<PathBuf> {
        let target = readlinkat(&self.fd, name, Vec::new()).ok()?;
        Some(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }

    /// Its `fd` directory, open, and an iterator over its entries.
    fn fds(&self) -> Option<(OwnedFd, Dir)> {
        let fd = opened(
            openat(
                &self.fd,
                "fd",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            ),
            &self.exhausted,
        )?;
        let dir = opened(Dir::read_from(&fd), &self.exhausted)?;
        Some((fd, dir))
    }
}

/// What [`inode_owners`] found.
#[derive(Debug, Default)]
pub struct Owners {
    /// Socket inode to the pid holding it (the first found).
    pub by_inode: HashMap<u64, u32>,
    /// A bound was reached before every wanted inode was found.
    pub truncated: bool,
}

/// Finds which of `uid`'s processes hold the sockets in `wanted`, looking first at `first`
/// (a session's known processes), then at every other process of `uid`, until every inode is
/// found, a bound is reached or `deadline` passes. `holder` is called with the open directory of
/// each process found holding one, while the scan is on it: what is read about the process is
/// read from the same one, and its descriptor is closed before the scan moves on (so a scan has a
/// handful open, whatever the number of processes).
pub fn inode_owners(
    root: &ProcRoot,
    uid: u32,
    wanted: &HashSet<u64>,
    first: &[u32],
    limits: &ScanLimits,
    deadline: Instant,
    mut holder: impl FnMut(&ProcDir),
) -> Owners {
    let mut out = Owners::default();
    if wanted.is_empty() {
        return out;
    }
    let mut visited = HashSet::new();
    let mut procs = 0usize;
    let mut fds = 0usize;
    let listing = std::fs::read_dir(root.path());
    if let Err(e) = &listing
        && [Errno::MFILE, Errno::NFILE]
            .iter()
            .any(|x| e.raw_os_error() == Some(x.raw_os_error()))
    {
        root.exhausted.set(true);
    }
    let rest = listing
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
        let Some(dir) = root.open(uid, pid) else {
            continue;
        };
        procs += 1;
        if procs > limits.max_procs {
            out.truncated = true;
            break;
        }
        let Some((fd_dir, entries)) = dir.fds() else {
            continue;
        };
        let mut holds = false;
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
                .filter(|e| !e.file_name().to_bytes().starts_with(b"."))
                .and_then(|e| readlinkat(&fd_dir, e.file_name(), Vec::new()).ok())
                .and_then(|l| socket_inode(l.as_bytes()))
            else {
                continue;
            };
            if wanted.contains(&inode) && !out.by_inode.contains_key(&inode) {
                out.by_inode.insert(inode, pid);
                holds = true;
            }
        }
        drop(fd_dir);
        if holds {
            holder(&dir);
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
fn socket_inode(link: &[u8]) -> Option<u64> {
    std::str::from_utf8(link)
        .ok()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// The name (`comm`) and working directory (with `~` for `home`) of the process; `None` if it
/// is gone.
pub fn process_info(dir: &ProcDir, home: Option<&Path>) -> Option<ProcessInfo> {
    let comm = dir.read("comm", MAX_COMM)?;
    let comm = String::from_utf8_lossy(&comm);
    let cwd = dir
        .readlink("cwd")
        .map(|p| PortsPath::capped(&tilde(&p, home)));
    Some(ProcessInfo {
        pid: dir.pid(),
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

    /// The session the process was started in, if any.
    pub fn of(&mut self, dir: &ProcDir) -> Option<SessionName> {
        if !self.units.is_empty()
            && let Some(text) = dir.read("cgroup", MAX_CGROUP)
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
        let mut p = dir.pid();
        if let Some(name) = self.pids.get(&p) {
            return Some((*name).clone());
        }
        p = self.parent(p, Some(dir))?;
        for _ in 0..MAX_DEPTH {
            if let Some(name) = self.pids.get(&p) {
                return Some((*name).clone());
            }
            p = self.parent(p, None)?;
        }
        None
    }

    /// The parent of the user's process `pid` (read from `dir` when it is open already); `None`
    /// at the top, for another user's process (the walk stops there) and for anything
    /// unreadable.
    fn parent(&mut self, pid: u32, dir: Option<&ProcDir>) -> Option<u32> {
        if let Some(p) = self.parent.get(&pid) {
            return *p;
        }
        let read = |d: &ProcDir| d.read("stat", MAX_STAT);
        let stat = match dir {
            Some(d) => read(d),
            None => self.root.open(self.uid, pid).and_then(|d| read(&d)),
        };
        let ppid = stat
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
            Self::at(std::env::temp_dir().join(format!(
                "fil-ports-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            )))
        }

        /// In `dir` (made, and removed on drop): a child test process builds its fake `/proc`
        /// where its parent, which removes it whatever the child did, said.
        pub fn at(dir: PathBuf) -> Self {
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

    /// The cleanup fails loudly (it used to hide an `EMFILE` and leave the directory behind),
    /// except while a test is already failing, where a second panic would abort.
    impl Drop for FakeProc {
        fn drop(&mut self) {
            match std::fs::remove_dir_all(&self.dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if !std::thread::panicking() => {
                    panic!("cannot remove the fake /proc {}: {e}", self.dir.display())
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeProc;
    use super::*;

    fn info(root: &ProcRoot, uid: u32, pid: u32, home: Option<&Path>) -> Option<ProcessInfo> {
        process_info(&root.open(uid, pid)?, home)
    }

    fn of_pid(s: &mut Sessions<'_>, pid: u32) -> Option<SessionName> {
        let d = s.root.open(s.uid, pid)?;
        s.of(&d)
    }

    /// Everything about a process is read from the directory that passed the owner check: a new
    /// directory at the same path (a reused pid) is not read in its place.
    #[test]
    fn reads_stay_with_the_directory_that_was_checked() {
        let f = FakeProc::new("reuse");
        f.process(100, 1, "first", "/a", &[]);
        let dir = f.root().open(me(), 100).unwrap();
        std::fs::rename(f.dir.join("100"), f.dir.join("gone")).unwrap();
        f.process(100, 1, "second", "/b", &[]);
        let i = process_info(&dir, None).unwrap();
        assert_eq!((i.name.as_str(), i.cwd.unwrap().as_str()), ("first", "/a"));
    }

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
            |_| {},
        );
        assert_eq!(o.by_inode, HashMap::from([(40127, 100), (40200, 200)]));
    }

    /// The uid filter on processes (mutation: skip the owner check in `ProcRoot::open`, and this
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
            |_| {},
        );
        assert!(o.by_inode.is_empty(), "{o:?}");
        assert_eq!(info(&f.root(), them, 100, None), None);
        let sessions = [(
            SessionName::new("s").unwrap(),
            Membership::Unit("fil-1".into()),
        )];
        f.cgroup(100, "/user.slice/fil-1.scope");
        let root = f.root();
        assert_eq!(
            of_pid(&mut Sessions::new(&root, them, &sessions), 100),
            None
        );
        assert!(root.open(them, 100).is_none());
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
        let i = info(&f.root(), me(), 100, Some(home)).unwrap();
        assert_eq!((i.pid, i.name.as_str()), (100, "vite"));
        assert_eq!(i.cwd.unwrap().as_str(), "~/app");
        let i = info(&f.root(), me(), 101, Some(home)).unwrap();
        assert_eq!(i.name.as_str().len(), 32);
        assert_eq!(i.cwd.unwrap().as_str(), "~");
        let i = info(&f.root(), me(), 102, Some(home)).unwrap();
        assert_eq!(i.cwd.unwrap().as_str(), "/srv/data (deleted)");
        assert_eq!(info(&f.root(), me(), 999, Some(home)), None);
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
        assert_eq!(of_pid(&mut s, 100), Some(a.clone()));
        assert_eq!(of_pid(&mut s, 110), Some(a));
        assert_eq!(of_pid(&mut s, 200), Some(b.clone()));
        assert_eq!(of_pid(&mut s, 220), Some(b));
        assert_eq!(of_pid(&mut s, 300), None);
        assert_eq!(of_pid(&mut s, 400), None);
        assert_eq!(of_pid(&mut s, 500), None);
        assert_eq!(of_pid(&mut s, 999), None);
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
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later(), |_| {});
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
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later(), |_| {});
        assert!(o.truncated);
        assert_eq!(o.by_inode, HashMap::from([(50, 100)]));
        let limits = ScanLimits {
            max_fds: 20,
            ..ScanLimits::default()
        };
        let o = inode_owners(&f.root(), me(), &wanted, &[100], &limits, later(), |_| {});
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
            |_| {},
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
        assert_eq!(socket_inode(b"socket:[12]"), Some(12));
        assert_eq!(socket_inode(b"socket:[]"), None);
        assert_eq!(socket_inode(b"socket:[-1]"), None);
        assert_eq!(socket_inode(b"anon_inode:[eventfd]"), None);
        let f = FakeProc::new("odd");
        f.process(100, 1, "x", "/", &[]);
        std::fs::write(f.dir.join("100/stat"), b"\xff\xfe garbage").unwrap();
        std::fs::write(f.dir.join("100/comm"), b"\xff\x1b[31m\n").unwrap();
        std::fs::write(f.dir.join("100/cgroup"), b"no colons here\n::\n").unwrap();
        std::fs::create_dir_all(f.dir.join("notapid")).unwrap();
        let root = f.root();
        let i = info(&root, me(), 100, None).unwrap();
        assert_eq!(
            i.name.as_str(),
            "\u{fffd}\u{1b}[31m",
            "kept as data, cut, never trusted"
        );
        let list = [(
            SessionName::new("a").unwrap(),
            Membership::Unit("fil-aa".into()),
        )];
        assert_eq!(of_pid(&mut Sessions::new(&root, me(), &list), 100), None);
        let list = [(SessionName::new("a").unwrap(), Membership::Pids(vec![5]))];
        assert_eq!(of_pid(&mut Sessions::new(&root, me(), &list), 100), None);
    }
}
