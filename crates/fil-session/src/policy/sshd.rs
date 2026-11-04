//! The sshd mirror (DESIGN.md section 11.2): the forwarding settings of the host's
//! `sshd_config`, as sshd would apply them to the daemon's user, as far as fil can tell.
//!
//! - **Settings:** `DisableForwarding`, `AllowTcpForwarding`, `AllowAgentForwarding`,
//!   `PermitOpen`, `PermitListen`, `GatewayPorts`. Every other keyword is skipped.
//! - **Parsing as sshd does:** keywords are case-insensitive, `keyword value` or
//!   `keyword=value`, a word starting with `#` ends the line; quotes and escapes are read as
//!   every OpenSSH release reads them, and where releases differ (single quotes, a quote inside
//!   a word, backslashes) a value is one fil does not know, a `Match` might apply and an
//!   `Include` cannot be evaluated; a keyword sshd reads differently (`"Match"`, which
//!   `strdelim` unquotes) cannot either, where it is one fil mirrors; the first value of a
//!   setting wins; `Include` takes globs,
//!   relative to the main config's directory (`/etc/ssh`), in sorted order, at most 16 deep,
//!   and a file included from a `Match` block is inside it.
//! - **Two pattern languages** ([`super::pattern`]): `Match` criteria use OpenSSH's `match_pattern`
//!   (`*`, `?` one byte, `!` in comma lists; no brackets), `Include` uses `glob(3)` (brackets, escapes).
//!   A `Match` list fil cannot read as sshd does might apply. A line fil cannot evaluate as sshd
//!   would (an `Include` glob whose meaning differs between `glob(3)` implementations or that is
//!   `PATH_MAX` bytes or longer, an `Include` line OpenSSH releases split differently, one with a
//!   byte that is not UTF-8, a file with a NUL byte) makes the settings unknown
//!   ([`Settings::unevaluable`]): the mirror refuses everything unless fil's policy file says.
//!   A `PermitOpen` or `PermitListen` entry with a byte that is not UTF-8 is one fil cannot
//!   read: the list permits nothing.
//! - **`Match`:** `User` and `Group` (pattern lists with `*`, `?` and `!`) are evaluated against
//!   the daemon's user. Every other criterion (`Address`, `Host`, `LocalAddress`, `LocalPort`,
//!   `RDomain`, `all`, ...) and every line fil cannot read **might apply**: its value is one of
//!   the outcomes, and a forward must be allowed by every outcome (the stricter wins).
//! - **Defaults** are OpenSSH's: forwarding and the agent allowed, `PermitOpen` and
//!   `PermitListen` `any`, `GatewayPorts no`.
//! - **A value fil does not know** is read as the strict one.
//! - **An included file that cannot be read,** or a directory an include glob must list and
//!   cannot, is skipped and reported ([`Settings::unreadable`]): what fil can read is still
//!   enforced. The main file unreadable is [`Sshd::Unknown`].
//!
//! Never logged: the config is the admin's, and what fil found in it is said only as the one
//! deciding setting of a refusal.

use std::path::{Path, PathBuf};

use fil_proto::mux::{Permit, TcpForwarding, Toggle};

use super::Account;
use super::pattern::{
    GlobComp, ListMatch, Unevaluable, glob_check, match_pattern, match_pattern_list,
};
use super::stamp::Stamp;

/// How deep `Include`s may nest (sshd's `SERVCONF_MAX_DEPTH`).
const MAX_DEPTH: usize = 16;

/// Most files one config may include in all: a bound on the work and on what is watched.
const MAX_FILES: usize = 256;

/// Where a value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Src {
    /// OpenSSH's built-in default.
    Default,
    /// A line; `maybe`: in a `Match` block that might apply.
    Line {
        file: PathBuf,
        line: u32,
        maybe: bool,
    },
}

/// `AllowTcpForwarding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tcp {
    All,
    No,
    Local,
    Remote,
    Unrecognized,
}

impl Tcp {
    pub fn allows(self, remote: bool) -> Result<(), TcpForwarding> {
        match (self, remote) {
            (Tcp::All, _) | (Tcp::Local, false) | (Tcp::Remote, true) => Ok(()),
            (Tcp::No, _) => Err(TcpForwarding::No),
            (Tcp::Local, true) => Err(TcpForwarding::Local),
            (Tcp::Remote, false) => Err(TcpForwarding::Remote),
            (Tcp::Unrecognized, _) => Err(TcpForwarding::Unrecognized),
        }
    }
}

/// A yes/no setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Yes,
    No,
    Unrecognized,
}

/// `GatewayPorts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gateway {
    No,
    Yes,
    ClientSpecified,
    Unrecognized,
}

impl Gateway {
    /// Whether a bind beyond loopback is honoured.
    pub fn allows_wide(self) -> Result<(), Toggle> {
        match self {
            Gateway::Yes | Gateway::ClientSpecified => Ok(()),
            Gateway::No => Err(Toggle::No),
            Gateway::Unrecognized => Err(Toggle::Unrecognized),
        }
    }
}

/// A host pattern and port of `PermitOpen` or `PermitListen`; `None` for `*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPort {
    pub host: String,
    pub port: Option<u16>,
}

/// `PermitOpen` or `PermitListen` (and the policy file's `permit_open`, `permit_listen`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermitList {
    Any,
    None,
    List(Vec<HostPort>),
    Unrecognized,
}

impl PermitList {
    /// `PermitOpen`'s rule (sshd's `open_match`): the port equal or `*`, the host equal as
    /// written (case-sensitive) or `*`.
    pub fn permits_open(&self, host: &str, port: u16) -> Result<(), Permit> {
        match self {
            PermitList::Any => Ok(()),
            PermitList::None => Err(Permit::None),
            PermitList::Unrecognized => Err(Permit::Unrecognized),
            PermitList::List(l) => l
                .iter()
                .any(|e| e.port.is_none_or(|p| p == port) && (e.host == "*" || e.host == host))
                .then_some(())
                .ok_or(Permit::NotListed),
        }
    }

    /// `PermitListen`'s rule (sshd's `remote_open_match`): the port equal or `*`, the listen host
    /// (lowercased) matching the entry's pattern.
    pub fn permits_listen(&self, host: &str, port: u16) -> Result<(), Permit> {
        match self {
            PermitList::Any => Ok(()),
            PermitList::None => Err(Permit::None),
            PermitList::Unrecognized => Err(Permit::Unrecognized),
            PermitList::List(l) => {
                let host = host.to_ascii_lowercase();
                l.iter()
                    .any(|e| e.port.is_none_or(|p| p == port) && match_pattern(&host, &e.host))
                    .then_some(())
                    .ok_or(Permit::NotListed)
            }
        }
    }

    /// The entries of one `PermitOpen` line (`listen`: `PermitListen`, where a bare port means
    /// any host). A list that has `any` or `none` with anything else, or an entry fil cannot
    /// read (port 0 among them, as sshd's `permitopen_port`), permits nothing. sshd compares
    /// `any` and `none` case-sensitively (`ANY` is a bad port to it, and the config is refused):
    /// `exact` does too; the policy file passes `false`.
    pub fn parse(args: &[String], listen: bool, exact: bool) -> PermitList {
        let is = |a: &str, w: &str| {
            if exact {
                a == w
            } else {
                a.eq_ignore_ascii_case(w)
            }
        };
        match args {
            [] => PermitList::Unrecognized,
            [one] if is(one, "any") => PermitList::Any,
            [one] if is(one, "none") => PermitList::None,
            _ => args
                .iter()
                .map(|a| parse_host_port(a, listen))
                .collect::<Option<Vec<_>>>()
                .map_or(PermitList::Unrecognized, PermitList::List),
        }
    }
}

/// `host:port`, `[v6]:port` or (`listen`) a bare `port`; the port may be `*`.
fn parse_host_port(s: &str, listen: bool) -> Option<HostPort> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        (h.to_string(), after.strip_prefix(':')?)
    } else if let Some((h, p)) = s.split_once(':') {
        (h.to_string(), p)
    } else if listen {
        ("*".to_string(), s)
    } else {
        return None;
    };
    if host.is_empty() || host.contains(':') && !s.starts_with('[') {
        return None;
    }
    let port = match port {
        "*" => None,
        p if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => match p.parse() {
            Ok(0) | Err(_) => return None,
            Ok(n) => Some(n),
        },
        _ => return None,
    };
    Some(HostPort { host, port })
}

/// Every value of a setting that might be in force for the daemon's user: a forward must be
/// allowed by all of them. Never empty once [`Settings::finish`] ran: the default is the last
/// resort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcomes<V> {
    pub values: Vec<(V, Src)>,
}

impl<V> Outcomes<V> {
    /// The first outcome `check` refuses, with its source.
    pub fn first_refusal<E>(&self, check: impl Fn(&V) -> Result<(), E>) -> Option<(E, &Src)> {
        self.values
            .iter()
            .find_map(|(v, src)| check(v).err().map(|e| (e, src)))
    }
}

/// One setting while the config is read: its first value outside `Match`, its first value in a
/// `Match` block that surely applies, and the values of blocks that might apply before that.
#[derive(Clone, Debug)]
struct Acc<V> {
    global: Option<(V, Src)>,
    definite: Option<(V, Src)>,
    maybes: Vec<(V, Src)>,
    /// The last block a might-apply value came from (a block's first value only).
    maybe_block: Option<usize>,
}

impl<V> Default for Acc<V> {
    fn default() -> Self {
        Self {
            global: None,
            definite: None,
            maybes: Vec::new(),
            maybe_block: None,
        }
    }
}

impl<V> Acc<V> {
    fn set(&mut self, ctx: Ctx, v: V, src: Src) {
        match ctx {
            Ctx::Global => {
                if self.global.is_none() {
                    self.global = Some((v, src));
                }
            }
            Ctx::Match(_, Applies::No) => {}
            Ctx::Match(_, Applies::Yes) => {
                if self.definite.is_none() {
                    self.definite = Some((v, src));
                }
            }
            // sshd applies the first value among the blocks that apply: one that might apply
            // counts only while no block that surely applies has set it, and only its first value.
            Ctx::Match(block, Applies::Maybe) => {
                if self.definite.is_none() && self.maybe_block != Some(block) {
                    self.maybe_block = Some(block);
                    self.maybes.push((v, src));
                }
            }
        }
    }

    fn finish(self, default: V) -> Outcomes<V> {
        let mut values = self.maybes;
        values.push(
            self.definite
                .or(self.global)
                .unwrap_or((default, Src::Default)),
        );
        Outcomes { values }
    }
}

/// Whether a `Match` block applies to the daemon's user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applies {
    Yes,
    No,
    Maybe,
}

impl Applies {
    /// Both must hold: the weaker of the two.
    fn and(self, other: Applies) -> Applies {
        match (self, other) {
            (Applies::No, _) | (_, Applies::No) => Applies::No,
            (Applies::Maybe, _) | (_, Applies::Maybe) => Applies::Maybe,
            (Applies::Yes, Applies::Yes) => Applies::Yes,
        }
    }
}

/// Where a line is: outside `Match`, or in block number `n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    Global,
    Match(usize, Applies),
}

impl Ctx {
    fn applies(self) -> Applies {
        match self {
            Ctx::Global => Applies::Yes,
            Ctx::Match(_, a) => a,
        }
    }
}

/// The forwarding settings of a config that could be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub disable: Outcomes<Flag>,
    pub tcp: Outcomes<Tcp>,
    pub agent: Outcomes<Flag>,
    pub permit_open: Outcomes<PermitList>,
    pub permit_listen: Outcomes<PermitList>,
    pub gateway: Outcomes<Gateway>,
    /// Included files, and directories an include glob had to list, that could not be read (in
    /// the order met): skipped.
    pub unreadable: Vec<PathBuf>,
    /// The first line (file, line number) fil could not evaluate as sshd would: an `Include`
    /// pattern whose meaning differs between `glob(3)` implementations, or that is `PATH_MAX`
    /// bytes or longer, or not valid UTF-8; an `Include` line OpenSSH releases split
    /// differently; or a file with a NUL byte (which sshd reads as gluing two lines together).
    /// What the config says is then not known, and unlike an unreadable include (skipped, the
    /// rest enforced) the mirror refuses everything unless fil's own policy file sets the key.
    pub unevaluable: Option<(PathBuf, u32)>,
}

impl Settings {
    /// OpenSSH's built-in defaults: what sshd runs with when its config does not say.
    pub fn defaults() -> Settings {
        let d = |v| Outcomes {
            values: vec![(v, Src::Default)],
        };
        Settings {
            disable: d(Flag::No),
            tcp: Outcomes {
                values: vec![(Tcp::All, Src::Default)],
            },
            agent: d(Flag::Yes),
            permit_open: Outcomes {
                values: vec![(PermitList::Any, Src::Default)],
            },
            permit_listen: Outcomes {
                values: vec![(PermitList::Any, Src::Default)],
            },
            gateway: Outcomes {
                values: vec![(Gateway::No, Src::Default)],
            },
            unreadable: Vec::new(),
            unevaluable: None,
        }
    }
}

/// What the main config gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sshd {
    /// There is no config: OpenSSH's defaults stand in for it.
    Missing,
    /// It is there but cannot be read (RHEL and Fedora keep it 0600): `Unknown`, and OpenSSH's
    /// defaults stand in for it.
    Unknown,
    Mirrored(Box<Settings>),
}

/// What a read looked at, for re-reading when any of it changes: every file read or tried and
/// every directory a glob listed, each stamped before it was read.
pub type Watched = Vec<Stamp>;

/// Reads `main` and what it includes, for `account`.
pub fn read(main: &Path, account: &Account) -> (Sshd, Watched) {
    let mut p = Parser {
        account,
        base: main.parent().unwrap_or(Path::new("/")).to_path_buf(),
        acc: Default::default(),
        blocks: 0,
        files: 0,
        unreadable: Vec::new(),
        unevaluable: None,
        watched: vec![Stamp::of(main)],
    };
    let bytes = match std::fs::read(main) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Sshd::Missing, p.watched),
        Err(_) => return (Sshd::Unknown, p.watched),
    };
    let mut ctx = Ctx::Global;
    p.file(main, &bytes, &mut ctx, Applies::Yes, 0);
    let a = p.acc;
    let settings = Settings {
        disable: a.disable.finish(Flag::No),
        tcp: a.tcp.finish(Tcp::All),
        agent: a.agent.finish(Flag::Yes),
        permit_open: a.permit_open.finish(PermitList::Any),
        permit_listen: a.permit_listen.finish(PermitList::Any),
        gateway: a.gateway.finish(Gateway::No),
        unreadable: p.unreadable,
        unevaluable: p.unevaluable,
    };
    (Sshd::Mirrored(Box::new(settings)), p.watched)
}

/// Reads config text as if it were `name` (tests).
#[cfg(test)]
pub fn parse_text(name: &Path, text: &str, account: &Account) -> Settings {
    let dir_tmp = tempfile::tempdir().unwrap();
    let dir = dir_tmp.path().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    let r = read(&path, account).0;
    match r {
        Sshd::Mirrored(s) => *s,
        other => panic!("{other:?}"),
    }
}

#[derive(Default)]
struct AllAcc {
    disable: Acc<Flag>,
    tcp: Acc<Tcp>,
    agent: Acc<Flag>,
    permit_open: Acc<PermitList>,
    permit_listen: Acc<PermitList>,
    gateway: Acc<Gateway>,
}

struct Parser<'a> {
    account: &'a Account,
    base: PathBuf,
    acc: AllAcc,
    blocks: usize,
    files: usize,
    unreadable: Vec<PathBuf>,
    unevaluable: Option<(PathBuf, u32)>,
    watched: Watched,
}

impl Parser<'_> {
    /// Notes a line fil cannot evaluate as sshd would (the first one is the one named).
    fn cannot_evaluate(&mut self, file: &Path, line: u32) {
        if self.unevaluable.is_none() {
            self.unevaluable = Some((file.to_path_buf(), line));
        }
    }

    /// One file's lines; `ctx` is the `Match` context, which an included file starts in and
    /// which goes back to the includer's when the file ends. `cap` is how far the block that
    /// included the file applies: a `Match` in the file applies at most that far. That is sshd's
    /// `SSHCFG_NEVERMATCH` for a file included from a block that does not apply, and the strict
    /// side for one that might.
    fn file(&mut self, path: &Path, bytes: &[u8], ctx: &mut Ctx, cap: Applies, depth: usize) {
        let text = String::from_utf8_lossy(bytes);
        // Which lines are not valid UTF-8 (the text above has U+FFFD for them, which is no byte
        // of the file), by the same line numbers as `lines()`; and the first with a NUL, which
        // sshd (it copies each line with `strlen`) reads as ending the line *and its newline*,
        // gluing the next line on: a file with one is not read as it is written.
        let invalid: Vec<bool> = bytes
            .split(|&b| b == b'\n')
            .map(|l| std::str::from_utf8(l).is_err())
            .collect();
        if let Some(n) = bytes.split(|&b| b == b'\n').position(|l| l.contains(&0)) {
            self.cannot_evaluate(path, u32::try_from(n + 1).unwrap_or(u32::MAX));
        }
        for (n, raw) in text.lines().enumerate() {
            let Some(l) = split_line(raw) else {
                continue;
            };
            let line = u32::try_from(n + 1).unwrap_or(u32::MAX);
            if !l.keyword_agreed {
                // sshd reads another keyword here than fil does, and one of the two is a
                // keyword fil mirrors (`"Match" User x` is a `Match` to sshd): what the line
                // does is not known.
                self.cannot_evaluate(path, line);
                continue;
            }
            let args = l.args();
            let not_utf8 = invalid.get(n).copied().unwrap_or(false);
            let src = |maybe: bool| Src::Line {
                file: path.to_path_buf(),
                line,
                maybe,
            };
            let maybe = matches!(ctx, Ctx::Match(_, Applies::Maybe));
            let c = *ctx;
            match l.keyword.to_ascii_lowercase().as_str() {
                "match" => {
                    self.blocks += 1;
                    // A criterion with a byte that is not UTF-8 is not the text fil compared.
                    let applies = if not_utf8 {
                        Applies::Maybe
                    } else {
                        self.match_line(&l)
                    };
                    *ctx = Ctx::Match(self.blocks, cap.and(applies));
                }
                // A pattern with a byte that is not UTF-8 is not the text fil would glob (the
                // replacement character matches nothing): the include is not known.
                "include" if not_utf8 => self.cannot_evaluate(path, line),
                "include" => {
                    let cap = cap.and(ctx.applies());
                    match l.agreed() {
                        Some(args) => self.include(args, path, line, ctx, cap, depth),
                        // sshd releases would include different files (8.7 and later split
                        // the line with `argv_split`, earlier ones with `strdelim`), or one
                        // cannot split it: which files sshd reads is not known, so neither is
                        // what the config says. Not skip-and-mark: that is for a file fil
                        // cannot read, and this one it read.
                        None => self.cannot_evaluate(path, line),
                    }
                }
                "disableforwarding" => self.acc.disable.set(c, flag(args), src(maybe)),
                "allowagentforwarding" => self.acc.agent.set(c, flag(args), src(maybe)),
                "allowtcpforwarding" => self.acc.tcp.set(c, tcp(args), src(maybe)),
                "gatewayports" => self.acc.gateway.set(c, gateway(args), src(maybe)),
                "permitopen" => {
                    self.acc
                        .permit_open
                        .set(c, permit_list(args, not_utf8, false), src(maybe))
                }
                "permitlisten" => {
                    self.acc
                        .permit_listen
                        .set(c, permit_list(args, not_utf8, true), src(maybe))
                }
                _ => {}
            }
        }
    }

    fn include(
        &mut self,
        args: &[String],
        from: &Path,
        line: u32,
        ctx: &mut Ctx,
        cap: Applies,
        depth: usize,
    ) {
        if depth + 1 > MAX_DEPTH {
            // sshd refuses to start on this; fil cannot know what was meant.
            self.unreadable.push(from.to_path_buf());
            return;
        }
        for pattern in args {
            // As sshd: a path that starts with neither `/` nor `~` is in SSHDIR (the main file's
            // directory); one with `~` is globbed as written, from sshd's working directory `/`.
            let pattern = if Path::new(pattern).is_absolute() {
                PathBuf::from(pattern)
            } else if pattern.starts_with('~') {
                Path::new("/").join(pattern)
            } else {
                self.base.join(pattern)
            };
            if self.files >= MAX_FILES {
                // The cap is reached: fil reads no more files, so it does not list the
                // directories for them either (every further `Include` would cost a glob and a
                // stamp for each directory, and a config can hold thousands of them). The
                // pattern is reported as not read, which marks the mirror incomplete.
                self.unreadable.push(pattern);
                continue;
            }
            let g = glob(&pattern);
            if g.unevaluable {
                // A pattern whose meaning differs between globs: what the config says is not
                // known, and the mirror refuses everything (unless the policy file sets the key).
                self.cannot_evaluate(from, line);
            }
            self.watched.extend(g.dirs);
            self.unreadable.extend(g.unlistable);
            if g.truncated {
                // More than fil reads: what it did not read is reported as not read.
                self.unreadable.push(pattern.clone());
            }
            for f in g.files {
                if self.files >= MAX_FILES {
                    self.unreadable.push(f);
                    continue;
                }
                self.files += 1;
                self.watched.push(Stamp::of(&f));
                match std::fs::read(&f) {
                    Ok(b) => {
                        let outer = *ctx;
                        self.file(&f, &b, ctx, cap, depth + 1);
                        *ctx = outer;
                    }
                    // Not there (or a file where a directory is wanted): sshd's glob finds
                    // nothing either.
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) => {}
                    Err(_) => self.unreadable.push(f),
                }
            }
        }
    }

    /// A `Match` line, for the daemon's user: its criteria as OpenSSH 9.9 and later split them
    /// (`argv_split`) and as earlier releases do (`strdelim`). Where the two disagree (a quote
    /// or an escape that one release reads and the other keeps), or either cannot split the
    /// line, the block might apply.
    fn match_line(&self, l: &Line) -> Applies {
        match (&l.argv, &l.legacy) {
            (Some(a), Some(b)) => {
                let (a, b) = (self.applies(a), self.applies(b));
                if a == b { a } else { Applies::Maybe }
            }
            _ => Applies::Maybe,
        }
    }

    /// `Match` criteria, for the daemon's user. All criteria must hold; one that surely fails
    /// makes the block not apply, one fil cannot evaluate makes it one that might.
    ///
    /// Tokens are paired as sshd's `match_cfg_line` does: `crit=arg` is one criterion, `all`
    /// and `invalid-user` take no argument, every other criterion takes the next token; a token
    /// starting with `#` ends the line. A criterion whose argument is missing, empty or starts
    /// with `#` is one sshd refuses ("Missing Match criteria"): it might apply.
    fn applies(&self, args: &[String]) -> Applies {
        let args = match args.iter().position(|a| a.starts_with('#')) {
            Some(n) => &args[..n],
            None => args,
        };
        if args.is_empty() {
            return Applies::Maybe;
        }
        let mut out = Applies::Yes;
        let mut it = args.iter();
        while let Some(tok) = it.next() {
            let (crit, arg) = match tok.split_once('=') {
                Some((c, a)) => (c.to_ascii_lowercase(), Some(a.to_string())),
                None => {
                    let c = tok.to_ascii_lowercase();
                    match c.as_str() {
                        "all" | "invalid-user" => (c, None),
                        _ => (c, it.next().cloned()),
                    }
                }
            };
            let r = match (crit.as_str(), arg) {
                // sshd always applies `all`; the owner reads it as a block that might (strict).
                ("all", None) => Applies::Maybe,
                // For logins of no account: never the daemon's own user.
                ("invalid-user", None) => Applies::No,
                // Missing, as sshd reads it: a config it refuses.
                (_, Some(a)) if a.is_empty() => Applies::Maybe,
                ("user", Some(a)) => match &self.account.user {
                    Some(u) => list_applies(match_pattern_list(u, &a)),
                    None => Applies::Maybe,
                },
                ("group", Some(a)) => match &self.account.groups {
                    Some(g) => group_match(g, &a),
                    None => Applies::Maybe,
                },
                // A criterion fil cannot evaluate, one without its argument, a line sshd would
                // refuse: might apply.
                _ => Applies::Maybe,
            };
            match r {
                Applies::No => return Applies::No,
                Applies::Maybe => out = Applies::Maybe,
                Applies::Yes => {}
            }
        }
        out
    }
}

/// A `PermitOpen` or `PermitListen` line of the sshd config (`listen`: `PermitListen`). An
/// entry with a byte that is not UTF-8 (`not_utf8`: the line has one, and the text fil reads
/// has the replacement character in its place, which is no byte of the file) is not the entry
/// sshd compares: the list is one fil cannot read, and permits nothing (strict). A byte that
/// is not UTF-8 elsewhere on the line (a comment) does not touch the entries.
fn permit_list(args: &[String], not_utf8: bool, listen: bool) -> PermitList {
    if not_utf8 && args.iter().any(|a| a.contains(char::REPLACEMENT_CHARACTER)) {
        return PermitList::Unrecognized;
    }
    PermitList::parse(args, listen, true)
}

fn yes_no(b: bool) -> Applies {
    if b { Applies::Yes } else { Applies::No }
}

fn flag(args: &[String]) -> Flag {
    match args {
        [v] if v.eq_ignore_ascii_case("yes") => Flag::Yes,
        [v] if v.eq_ignore_ascii_case("no") => Flag::No,
        _ => Flag::Unrecognized,
    }
}

fn tcp(args: &[String]) -> Tcp {
    match args {
        [v] => match v.to_ascii_lowercase().as_str() {
            "yes" | "all" => Tcp::All,
            "no" => Tcp::No,
            "local" => Tcp::Local,
            "remote" => Tcp::Remote,
            _ => Tcp::Unrecognized,
        },
        _ => Tcp::Unrecognized,
    }
}

fn gateway(args: &[String]) -> Gateway {
    match args {
        [v] => match v.to_ascii_lowercase().as_str() {
            "no" => Gateway::No,
            "yes" => Gateway::Yes,
            "clientspecified" => Gateway::ClientSpecified,
            _ => Gateway::Unrecognized,
        },
        _ => Gateway::Unrecognized,
    }
}

/// A config line's arguments, split as each OpenSSH release would split them. Since 8.7 the
/// values are split with `argv_split` (single and double quotes anywhere in a word, backslash
/// escapes), and since 9.9 the `Match` criteria are too; before that both went through
/// `strdelim` (a double quote only, and a single `=` also ends a word). fil cannot tell which
/// sshd the host runs, so it uses a line's arguments only where the two agree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    keyword: String,
    /// sshd's reading of the keyword (`strdelim`, [`sshd_keyword`]) is fil's, or neither is a
    /// keyword fil mirrors ([`mirrored`]).
    keyword_agreed: bool,
    /// `argv_split`'s words; `None` for an unclosed quote (sshd refuses the config).
    argv: Option<Vec<String>>,
    /// `strdelim`'s words; `None` for an unclosed double quote.
    legacy: Option<Vec<String>>,
}

impl Line {
    /// The arguments where every release reads them the same; `None` where they differ or a
    /// release cannot split them.
    fn agreed(&self) -> Option<&[String]> {
        match (&self.argv, &self.legacy) {
            (Some(a), Some(b)) if a == b => Some(a),
            _ => None,
        }
    }

    /// The arguments for a setting: those every release agrees on, or none, which every setting
    /// reads as a value it does not know (the strict side).
    fn args(&self) -> &[String] {
        self.agreed().unwrap_or(&[])
    }
}

/// sshd's `WHITESPACE` (`misc.h`).
const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

/// A config line as sshd splits it: the keyword (ended by whitespace or `=`), then the rest,
/// after one optional `=`, split both ways ([`Line`]). Trailing whitespace and form feeds are
/// dropped first, as sshd drops them. `None` for a blank or comment line, and for one without a
/// keyword that sshd does not read as a mirrored one either.
///
/// Every release reads the keyword with `strdelim`, which also ends a word at `"` and reads a
/// quoted word: `"Match" User x` is a `Match` line to sshd. fil does not follow that reading;
/// where it gives another keyword than fil's and either is one fil mirrors, the line is marked
/// ([`Line::keyword_agreed`]) and the config is unevaluable.
fn split_line(raw: &str) -> Option<Line> {
    let s = raw
        .trim_start_matches([' ', '\t', '\r', '\n', '\x0b', '\x0c'])
        .trim_end_matches([' ', '\t', '\r', '\n', '\x0c']);
    if s.is_empty() || s.starts_with('#') {
        return None;
    }
    let end = s
        .find(|c: char| c.is_ascii_whitespace() || c == '=')
        .unwrap_or(s.len());
    let keyword = s[..end].to_string();
    let keyword_agreed = keyword_agrees(&keyword, sshd_keyword(s).as_deref());
    if keyword.is_empty() && keyword_agreed {
        return None;
    }
    let mut rest = s[end..].trim_start_matches(WHITESPACE);
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start_matches(WHITESPACE);
    }
    Some(Line {
        keyword,
        keyword_agreed,
        argv: argv_split(rest),
        legacy: strdelim_split(rest),
    })
}

/// The keywords fil mirrors (sshd compares keywords case-insensitively).
fn mirrored(keyword: &str) -> bool {
    [
        "match",
        "include",
        "disableforwarding",
        "allowagentforwarding",
        "allowtcpforwarding",
        "gatewayports",
        "permitopen",
        "permitlisten",
    ]
    .iter()
    .any(|k| keyword.eq_ignore_ascii_case(k))
}

/// fil's keyword is sshd's, or neither is one fil mirrors (both skip the line, or sshd refuses
/// a keyword it does not know). `sshd`: `None` where sshd reads no keyword.
fn keyword_agrees(fil: &str, sshd: Option<&str>) -> bool {
    sshd == Some(fil) || (!mirrored(fil) && sshd.is_none_or(|k| !mirrored(k)))
}

/// The keyword as sshd reads it: the first word `strdelim` gives (which ends at whitespace, a
/// `"` or one `=`, and reads a `"` as opening a word that runs to the next `"`), or, when that
/// word is empty (a line that starts with `=`), the next one. `None` for an unclosed quote,
/// which reads no keyword.
fn sshd_keyword(s: &str) -> Option<String> {
    let ws = |c: char| WHITESPACE.contains(&c);
    let mut rest: Vec<char> = s.chars().collect();
    for _ in 0..2 {
        let (word, next) = match rest.iter().position(|&c| ws(c) || c == '"' || c == '=') {
            None => (rest.iter().collect::<String>(), Vec::new()),
            Some(q) if rest[q] == '"' => {
                rest.remove(q);
                let close = (q..rest.len()).find(|&i| rest[i] == '"')?;
                let after = rest[close + 1..].iter().skip_while(|c| ws(**c)).copied();
                (rest[..close].iter().collect(), after.collect())
            }
            Some(d) => {
                let mut after: Vec<char> = rest[d + 1..]
                    .iter()
                    .skip_while(|c| ws(**c))
                    .copied()
                    .collect();
                if rest[d] != '=' && after.first() == Some(&'=') {
                    after = after[1..].iter().skip_while(|c| ws(**c)).copied().collect();
                }
                (rest[..d].iter().collect(), after)
            }
        };
        if !word.is_empty() {
            return Some(word);
        }
        rest = next;
    }
    Some(String::new())
}

/// OpenSSH's `argv_split(s, .., terminate_on_comment = 1)` (`misc.c`, 8.7 and later): words
/// separated by spaces and tabs; a `'` or `"` outside quotes opens a quote that the same
/// character closes, anywhere in a word; a backslash before `'`, `"`, `\` (or, outside quotes,
/// a space) makes that character literal, before anything else it stays; an unquoted `#` at the
/// start of a word ends the line. `None` for an unclosed quote.
fn argv_split(s: &str) -> Option<Vec<String>> {
    let s: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] == ' ' || s[i] == '\t' {
            i += 1;
            continue;
        }
        if s[i] == '#' {
            break;
        }
        let mut quote = None;
        let mut word = String::new();
        while i < s.len() {
            let c = s[i];
            let next = s.get(i + 1).copied();
            if c == '\\' {
                if matches!(next, Some('\'' | '"' | '\\')) || (quote.is_none() && next == Some(' '))
                {
                    i += 1;
                    word.push(s[i]);
                } else {
                    word.push(c);
                }
            } else if quote.is_none() && (c == ' ' || c == '\t') {
                break;
            } else if quote.is_none() && (c == '"' || c == '\'') {
                quote = Some(c);
            } else if quote == Some(c) {
                quote = None;
            } else {
                word.push(c);
            }
            i += 1;
        }
        if quote.is_some() {
            return None;
        }
        out.push(word);
    }
    Some(out)
}

/// The words OpenSSH's `strdelim` gives, called until it gives none (`misc.c`, before 8.7 for
/// values and before 9.9 for `Match`): a word ends at whitespace, a `"` or one `=`; a `"`
/// is dropped and the word runs to the next `"`, which ends it. An empty word, or one starting
/// with `#`, ends the line (the callers stop there). `None` for an unclosed quote.
fn strdelim_split(s: &str) -> Option<Vec<String>> {
    let mut b: Vec<char> = s.chars().collect();
    let ws = |c: char| WHITESPACE.contains(&c);
    let skip_ws = |b: &[char], from: usize| {
        from + b[from.min(b.len())..]
            .iter()
            .take_while(|c| ws(**c))
            .count()
    };
    let mut out = Vec::new();
    let mut at = Some(0);
    while let Some(old) = at {
        let word: String;
        match (old..b.len()).find(|&i| ws(b[i]) || b[i] == '"' || b[i] == '=') {
            None => {
                word = b[old..].iter().collect();
                at = None;
            }
            Some(q) if b[q] == '"' => {
                b.remove(q);
                let close = (q..b.len()).find(|&i| b[i] == '"')?;
                word = b[old..close].iter().collect();
                at = Some(skip_ws(&b, close + 1));
            }
            Some(d) => {
                let equals = b[d] == '=';
                word = b[old..d].iter().collect();
                let mut next = skip_ws(&b, d + 1);
                if !equals && b.get(next) == Some(&'=') {
                    next = skip_ws(&b, next + 1);
                }
                at = Some(next);
            }
        }
        if word.is_empty() || word.starts_with('#') {
            break;
        }
        out.push(word);
    }
    Some(out)
}

/// What a `Match User` pattern list says about whether the block applies: a match without a
/// negated one does, any other answer sshd gives does not, and an answer fil cannot give as
/// sshd would (a subpattern of 1023 bytes or more, see [`match_pattern_list`]) *might*.
fn list_applies(m: ListMatch) -> Applies {
    match m {
        ListMatch::Positive => Applies::Yes,
        ListMatch::NoMatch | ListMatch::Negated => Applies::No,
        ListMatch::Unsure => Applies::Maybe,
    }
}

/// OpenSSH's `ga_match_pattern_list`: one of the user's groups matches, and none matches a
/// negated pattern. A negated match is sshd's answer at once, whatever a long subpattern after
/// it would have said; a group on which fil is [`ListMatch::Unsure`] makes the answer *might*
/// unless a negated one settles it.
fn group_match(groups: &[String], list: &str) -> Applies {
    let (mut found, mut unsure) = (false, false);
    for g in groups {
        match match_pattern_list(g, list) {
            ListMatch::Negated => return Applies::No,
            ListMatch::Positive => found = true,
            ListMatch::NoMatch => {}
            ListMatch::Unsure => unsure = true,
        }
    }
    if unsure {
        Applies::Maybe
    } else {
        yes_no(found)
    }
}

/// What a glob found.
struct Globbed {
    /// The files it names, sorted (as `glob(3)`).
    files: Vec<PathBuf>,
    /// The directories it listed (and the paths that were not directories where a directory was
    /// needed), stamped before they were looked at.
    dirs: Vec<Stamp>,
    /// Directories it had to list and could not (not a missing one): what they hold is unknown.
    unlistable: Vec<PathBuf>,
    /// It named more than [`MAX_FILES`]; the rest were dropped.
    truncated: bool,
    /// The pattern uses a construct fil cannot evaluate as sshd's `glob(3)` does (see
    /// `pattern::compile`), is [`PATH_MAX`] bytes or longer, or meets a name it cannot match
    /// identically by bytes and by characters: the config's settings are not known.
    unevaluable: bool,
}

/// The longest path `glob(3)` takes: a pattern this long or longer finds nothing in OpenBSD's
/// `glob()` (`GLOB_NOMATCH` for `strnlen(pattern, PATH_MAX) == PATH_MAX`), and fil does not
/// evaluate it ([`Globbed::unevaluable`]). It is the host's own limit (4096 on Linux, 1024 on
/// macOS and the BSDs): the daemon runs where sshd does.
const PATH_MAX: usize = nix::libc::PATH_MAX as usize;

/// The files a glob names, as sshd's `glob(3)` (called with no flags) does: `*`, `?`, `[...]` and
/// backslash escapes in any component; a leading `.` is matched only by a pattern that starts
/// with one (glob(7)); the result sorted by `strcmp` on the whole path. A path without a
/// wildcard names itself, whether or not it exists. A pattern whose meaning differs between
/// `glob(3)` implementations is not guessed at ([`Globbed::unevaluable`]).
///
/// The pattern is taken apart on its raw bytes, not as `Path` components: `Path` drops a
/// trailing `/` and every `.` component, and sshd's glob keeps both. `conf.d/a.conf/` and
/// `conf.d/*.conf/.` name nothing there (`lstat` of a file with a `/` after it is `ENOTDIR`),
/// where the normalized path would name `a.conf`.
fn glob(pattern: &Path) -> Globbed {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut g = Globbed {
        files: Vec::new(),
        dirs: Vec::new(),
        unlistable: Vec::new(),
        truncated: false,
        unevaluable: false,
    };
    let raw = pattern.as_os_str().as_bytes();
    if raw.len() >= PATH_MAX {
        // Globs disagree at this length: OpenBSD's finds nothing (`GLOB_NOMATCH`), one
        // without that check globs it anyway, and one that fails another way stops sshd from
        // starting. Not guessed at.
        g.unevaluable = true;
        return g;
    }
    let comps: Result<Vec<GlobComp>, Unevaluable> =
        glob_check(raw).and_then(|()| raw.split(|&b| b == b'/').map(GlobComp::new).collect());
    let Ok(comps) = comps else {
        g.unevaluable = true;
        return g;
    };
    let as_path = |b: &[u8]| PathBuf::from(OsString::from_vec(b.to_vec()));
    // The paths so far, as bytes: the empty first component of an absolute pattern makes the
    // root, and every other component (an empty one from `//` or a trailing `/`, `.`, `..`)
    // is added as it is written.
    let mut paths: Vec<Vec<u8>> = vec![Vec::new()];
    let mut listed = false;
    for (i, comp) in comps.iter().enumerate() {
        let join = |base: &[u8], name: &[u8]| {
            let mut p = base.to_vec();
            if i > 0 {
                p.push(b'/');
            }
            p.extend_from_slice(name);
            p
        };
        let mut next = Vec::new();
        if let Some(name) = comp.literal() {
            next.extend(paths.iter().map(|b| join(b, name)));
        } else {
            listed = true;
            for base in &paths {
                let dir = match (base.is_empty(), i) {
                    (false, _) => as_path(base),
                    (true, 0) => PathBuf::from("."),
                    (true, _) => PathBuf::from("/"),
                };
                g.dirs.push(Stamp::of(&dir));
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    // Nothing there: sshd's glob finds nothing either.
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        continue;
                    }
                    // There, but fil may not list it (sshd, as root, can): what it holds is not
                    // mirrored, and the mirror says so.
                    Err(_) => {
                        g.unlistable.push(dir);
                        continue;
                    }
                };
                // Names as bytes, not as text: a name that is not UTF-8 is still a name. sshd's
                // `readdir` also yields `.` and `..`, which only a component that starts with
                // a `.` can match (`.*` includes them, as a directory the pattern goes through).
                let mut names: Vec<Vec<u8>> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().as_bytes().to_vec())
                    .collect();
                if comp.starts_with_dot() {
                    names.push(b".".to_vec());
                    names.push(b"..".to_vec());
                }
                names.retain(|n| match comp.matches(n) {
                    Ok(m) => m,
                    // A name the readings of the pattern disagree on (bytes and characters):
                    // the pattern cannot be evaluated identically, and what the config says
                    // is not known.
                    Err(Unevaluable) => {
                        g.unevaluable = true;
                        false
                    }
                });
                names.sort();
                next.extend(names.iter().map(|n| join(base, n)));
            }
        }
        paths = next;
        if paths.len() > MAX_FILES {
            paths.truncate(MAX_FILES);
            g.truncated = true;
        }
    }
    if listed {
        // glob(3) sorts the whole result with `strcmp`, not directory by directory (`a-b/x`
        // comes before `a/x`).
        paths.sort();
    }
    // What sshd's glob keeps is what `lstat` finds; of that, only files are configs. A path
    // with a `/` after a file (a trailing `/`, a `.` component) is `ENOTDIR` there: nothing.
    // (A path that is not there is kept: it is read, found missing, and watched, so that the
    // file appearing is seen.)
    for p in paths {
        let p = as_path(&p);
        match std::fs::symlink_metadata(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
                g.dirs.push(Stamp::of(&p));
            }
            _ if p.is_dir() => {}
            _ => g.files.push(p),
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Account {
        Account {
            user: Some("dev".into()),
            groups: Some(vec!["dev".into(), "staff".into(), "docker".into()]),
        }
    }

    fn parse(text: &str) -> Settings {
        parse_text(Path::new("sshd_config"), text, &me())
    }

    fn line(s: &Src) -> (u32, bool) {
        match s {
            Src::Line { line, maybe, .. } => (*line, *maybe),
            Src::Default => (0, false),
        }
    }

    /// Where a value was set: (line, maybe); line 0 is the default.
    type Line = (u32, bool);

    /// An outcome as (value, line, maybe).
    type Outcome = (Tcp, u32, bool);

    /// The outcomes of `AllowTcpForwarding`.
    fn tcp_of(text: &str) -> Vec<Outcome> {
        parse(text)
            .tcp
            .values
            .iter()
            .map(|(v, s)| {
                let (l, m) = line(s);
                (*v, l, m)
            })
            .collect()
    }

    #[test]
    fn defaults_are_openssh_s() {
        let s = parse("# nothing\n\nPort 22\n");
        assert_eq!(s.tcp.values, vec![(Tcp::All, Src::Default)]);
        assert_eq!(s.agent.values, vec![(Flag::Yes, Src::Default)]);
        assert_eq!(s.disable.values, vec![(Flag::No, Src::Default)]);
        assert_eq!(s.gateway.values, vec![(Gateway::No, Src::Default)]);
        assert_eq!(s.permit_open.values, vec![(PermitList::Any, Src::Default)]);
        assert_eq!(
            s.permit_listen.values,
            vec![(PermitList::Any, Src::Default)]
        );
    }

    #[test]
    fn keywords_are_case_insensitive_and_the_first_value_wins() {
        assert_eq!(
            tcp_of("allowtcpforwarding Local\nAllowTcpForwarding yes\n"),
            vec![(Tcp::Local, 1, false)]
        );
        assert_eq!(tcp_of("ALLOWTCPFORWARDING=no\n"), vec![(Tcp::No, 1, false)]);
        assert_eq!(
            tcp_of("  AllowTcpForwarding = remote # comment\n"),
            vec![(Tcp::Remote, 1, false)]
        );
        assert_eq!(
            tcp_of("AllowTcpForwarding \"all\"\n"),
            vec![(Tcp::All, 1, false)]
        );
        // Unknown values, or more than one, are the strict side.
        assert_eq!(
            tcp_of("AllowTcpForwarding maybe\n"),
            vec![(Tcp::Unrecognized, 1, false)]
        );
        assert_eq!(
            tcp_of("AllowTcpForwarding\n"),
            vec![(Tcp::Unrecognized, 1, false)]
        );
        assert_eq!(
            tcp_of("AllowTcpForwarding \"no\n"),
            vec![(Tcp::Unrecognized, 1, false)]
        );
        assert_eq!(
            parse("GatewayPorts sometimes\n").gateway.values[0].0,
            Gateway::Unrecognized
        );
        assert_eq!(
            parse("AllowAgentForwarding nope\n").agent.values[0].0,
            Flag::Unrecognized
        );
    }

    #[test]
    fn match_user_and_group_with_negation_and_wildcards() {
        let cases: &[(&str, Vec<Outcome>)] = &[
            // Applies: the block's value wins over the global one.
            (
                "AllowTcpForwarding yes\nMatch User dev\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 3, false)],
            ),
            (
                "Match User d?v\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, false)],
            ),
            (
                "Match User alice,d*\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, false)],
            ),
            // Does not apply.
            (
                "Match User alice\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "Match User *,!dev\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "Match User !dev\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            // A negated pattern that does not match, alone, matches nothing (OpenSSH).
            (
                "Match User !alice\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "Match User !alice,*\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, false)],
            ),
            // Groups: any of the user's, none negated.
            (
                "Match Group staff\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, false)],
            ),
            (
                "Match Group wheel,dock*\n AllowTcpForwarding local\n",
                vec![(Tcp::Local, 2, false)],
            ),
            (
                "Match Group *,!docker\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "Match Group wheel\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            // Every criterion must hold.
            (
                "Match User dev Group wheel\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "match user dev group staff\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, false)],
            ),
            // The first block that applies wins.
            (
                "Match User dev\n AllowTcpForwarding local\nMatch Group staff\n AllowTcpForwarding no\n",
                vec![(Tcp::Local, 2, false)],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(&tcp_of(text), want, "{text:?}");
        }
    }

    #[test]
    fn criteria_fil_cannot_evaluate_might_apply_and_the_stricter_wins() {
        let cases: &[(&str, Vec<Outcome>)] = &[
            // Both the block's value and the outer one are outcomes.
            (
                "AllowTcpForwarding local\nMatch Address 10.0.0.0/8\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 3, true), (Tcp::Local, 1, false)],
            ),
            (
                "Match Host *.example\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match LocalPort 2222\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match LocalAddress 1.2.3.4\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match RDomain x\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match all\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match Bogus x\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            (
                "Match User\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            // A user criterion that surely fails beats an unknown one.
            (
                "Match User alice Address 10.0.0.1\n AllowTcpForwarding no\n",
                vec![(Tcp::All, 0, false)],
            ),
            (
                "Match User dev Address 10.0.0.1\n AllowTcpForwarding no\n",
                vec![(Tcp::No, 2, true), (Tcp::All, 0, false)],
            ),
            // A block that surely applies after one that might: both are outcomes.
            (
                "Match Address 10.0.0.1\n AllowTcpForwarding remote\nMatch User dev\n AllowTcpForwarding local\n",
                vec![(Tcp::Remote, 2, true), (Tcp::Local, 4, false)],
            ),
            // One that might apply after one that surely does cannot win.
            (
                "Match User dev\n AllowTcpForwarding local\nMatch Address 10.0.0.1\n AllowTcpForwarding no\n",
                vec![(Tcp::Local, 2, false)],
            ),
            // Only a block's first value counts.
            (
                "Match Address 10.0.0.1\n AllowTcpForwarding remote\n AllowTcpForwarding no\n",
                vec![(Tcp::Remote, 2, true), (Tcp::All, 0, false)],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(&tcp_of(text), want, "{text:?}");
        }
        // An account fil cannot look up: User and Group might apply.
        let s = parse_text(
            Path::new("sshd_config"),
            "Match Group wheel\n AllowTcpForwarding no\n",
            &Account::default(),
        );
        assert_eq!(s.tcp.values[0].0, Tcp::No);
        assert!(matches!(s.tcp.values[0].1, Src::Line { maybe: true, .. }));
    }

    #[test]
    fn permit_lists() {
        let open = |t: &str| parse(t).permit_open.values[0].0.clone();
        assert_eq!(open("PermitOpen any\n"), PermitList::Any);
        assert_eq!(open("PermitOpen none\n"), PermitList::None);
        let l = open("PermitOpen db:5432 [2001:db8::1]:80 *:443 localhost:*\n");
        assert_eq!(l.permits_open("db", 5432), Ok(()));
        assert_eq!(l.permits_open("2001:db8::1", 80), Ok(()));
        assert_eq!(l.permits_open("anything", 443), Ok(()));
        assert_eq!(l.permits_open("localhost", 9), Ok(()));
        assert_eq!(
            l.permits_open("DB", 5432),
            Err(Permit::NotListed),
            "case-sensitive"
        );
        assert_eq!(l.permits_open("db", 5433), Err(Permit::NotListed));
        assert_eq!(open("PermitOpen any db:1\n"), PermitList::Unrecognized);
        assert_eq!(open("PermitOpen db\n"), PermitList::Unrecognized);
        assert_eq!(open("PermitOpen db:x\n"), PermitList::Unrecognized);
        assert_eq!(open("PermitOpen\n"), PermitList::Unrecognized);
        assert_eq!(
            PermitList::Unrecognized.permits_open("db", 1),
            Err(Permit::Unrecognized)
        );
        assert_eq!(PermitList::None.permits_open("db", 1), Err(Permit::None));

        let listen = |t: &str| parse(t).permit_listen.values[0].0.clone();
        let l = listen("PermitListen 8080 localhost:9000 10.0.*:*\n");
        assert_eq!(l.permits_listen("localhost", 8080), Ok(()));
        assert_eq!(
            l.permits_listen("*", 8080),
            Ok(()),
            "a bare port is any host"
        );
        assert_eq!(l.permits_listen("LocalHost", 9000), Ok(()));
        assert_eq!(l.permits_listen("10.0.3.4", 1), Ok(()));
        assert_eq!(l.permits_listen("127.0.0.1", 9000), Err(Permit::NotListed));
        assert_eq!(listen("PermitListen none\n"), PermitList::None);
    }

    #[test]
    fn include_with_globs_relative_paths_and_match_context() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(dir.join("conf.d/20-b.conf"), "AllowTcpForwarding remote\n").unwrap();
        std::fs::write(
            dir.join("conf.d/10-a.conf"),
            "GatewayPorts clientspecified\nMatch User dev\n AllowAgentForwarding no\n",
        )
        .unwrap();
        std::fs::write(dir.join("conf.d/.hidden.conf"), "AllowTcpForwarding no\n").unwrap();
        std::fs::write(dir.join("conf.d/30-c.txt"), "AllowTcpForwarding no\n").unwrap();
        std::fs::write(dir.join("abs.conf"), "PermitOpen none\n").unwrap();
        let abs = dir.join("abs.conf");
        // Relative to the main config's directory; sorted; `.hidden` and `*.txt` left out; the
        // Match in 10-a.conf ends with the file, so the main file's next line is global again.
        let main = format!(
            "Include conf.d/*.conf {}\nAllowAgentForwarding yes\nAllowTcpForwarding no\nMatch Address 1.2.3.4\n Include conf.d/2?-b.conf\n",
            abs.display()
        );
        std::fs::write(dir.join("sshd_config"), main).unwrap();
        let (r, watched) = read(&dir.join("sshd_config"), &me());
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        let f = |p: &str| dir.join(p);
        assert_eq!(
            s.gateway.values,
            vec![(
                Gateway::ClientSpecified,
                Src::Line {
                    file: f("conf.d/10-a.conf"),
                    line: 1,
                    maybe: false
                }
            )]
        );
        assert_eq!(
            s.agent.values,
            vec![(
                Flag::No,
                Src::Line {
                    file: f("conf.d/10-a.conf"),
                    line: 3,
                    maybe: false
                }
            )]
        );
        // 20-b.conf's global line comes first; included again inside a Match that might apply.
        assert_eq!(
            s.tcp.values,
            vec![
                (
                    Tcp::Remote,
                    Src::Line {
                        file: f("conf.d/20-b.conf"),
                        line: 1,
                        maybe: true
                    }
                ),
                (
                    Tcp::Remote,
                    Src::Line {
                        file: f("conf.d/20-b.conf"),
                        line: 1,
                        maybe: false
                    }
                ),
            ]
        );
        assert_eq!(s.permit_open.values[0].0, PermitList::None);
        assert!(
            watched.iter().any(|w| w.path == f("conf.d")),
            "the glob's directory is watched"
        );
        assert!(watched.iter().any(|w| w.path == f("conf.d/20-b.conf")));

        // An included file that cannot be read is skipped and reported; the rest is enforced.
        // (Root reads everything: there the check is moot.)
        if rustix::process::geteuid().as_raw() != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                f("conf.d/20-b.conf"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            let (r, _) = read(&dir.join("sshd_config"), &me());
            let Sshd::Mirrored(s) = r else {
                panic!("{r:?}")
            };
            assert_eq!(
                s.unreadable,
                vec![f("conf.d/20-b.conf"), f("conf.d/20-b.conf")]
            );
            assert_eq!(
                s.tcp.values,
                vec![(
                    Tcp::No,
                    Src::Line {
                        file: f("sshd_config"),
                        line: 3,
                        maybe: false
                    }
                )]
            );
            // The RHEL case: the main file unreadable is no mirror at all.
            std::fs::set_permissions(f("sshd_config"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            assert_eq!(read(&f("sshd_config"), &me()).0, Sshd::Unknown);
            std::fs::set_permissions(f("sshd_config"), std::fs::Permissions::from_mode(0o644))
                .unwrap();
            std::fs::set_permissions(
                f("conf.d/20-b.conf"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        assert_eq!(read(&f("missing"), &me()).0, Sshd::Missing);

        // Includes that include themselves stop at the depth bound.
        std::fs::write(
            f("loop.conf"),
            format!("Include {}\n", f("loop.conf").display()),
        )
        .unwrap();
        let (r, _) = read(&f("loop.conf"), &me());
        let Sshd::Mirrored(s) = r else { panic!() };
        assert!(!s.unreadable.is_empty());
    }

    /// sshd's `SSHCFG_NEVERMATCH`: a file included from a `Match` block that does not apply
    /// never applies a `Match` of its own; from one that might apply, its blocks only might.
    #[test]
    fn an_include_inside_a_match_block_is_capped_by_it() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("inner.conf"),
            "Match User dev\n AllowTcpForwarding yes\n",
        )
        .unwrap();
        let tcp = |outer: &str| {
            std::fs::write(
                dir.join("sshd_config"),
                format!("AllowTcpForwarding no\n{outer}\n Include inner.conf\n"),
            )
            .unwrap();
            let Sshd::Mirrored(s) = read(&dir.join("sshd_config"), &me()).0 else {
                panic!()
            };
            s.tcp
                .values
                .iter()
                .map(|(v, s)| (*v, line(s)))
                .collect::<Vec<_>>()
        };
        // The outer block does not apply: neither does the inner one (sshd: `no`).
        assert_eq!(tcp("Match User alice"), [(Tcp::No, (1, false))]);
        // The outer block might apply: the inner one only might, and `no` stays an outcome.
        assert_eq!(
            tcp("Match Address 10.0.0.1"),
            [(Tcp::All, (2, true)), (Tcp::No, (1, false))]
        );
        // The outer block applies: so does the inner one.
        assert_eq!(tcp("Match User dev"), [(Tcp::All, (2, false))]);
    }

    /// `crit=arg` tokens (current OpenSSH) are one criterion each; `all` and `invalid-user` take
    /// no argument.
    #[test]
    fn match_criteria_pair_as_sshd_pairs_them() {
        let account = Account {
            user: Some("user".into()),
            groups: Some(vec!["user".into()]),
        };
        let tcp = |text: &str| {
            parse_text(Path::new("sshd_config"), text, &account)
                .tcp
                .values
                .iter()
                .map(|(v, s)| (*v, line(s)))
                .collect::<Vec<_>>()
        };
        let cases: &[(&str, Vec<(Tcp, Line)>)] = &[
            // The review's case: `User user` between two `crit=arg` tokens.
            (
                "AllowTcpForwarding yes\nMatch Address=10.0.0.1 User user LocalPort=22\n AllowTcpForwarding no\n",
                vec![(Tcp::No, (3, true)), (Tcp::All, (1, false))],
            ),
            (
                "Match User=user\n AllowTcpForwarding no\n",
                vec![(Tcp::No, (2, false))],
            ),
            (
                "Match user=alice\n AllowTcpForwarding no\n",
                vec![(Tcp::All, (0, false))],
            ),
            (
                "Match Group=user User user\n AllowTcpForwarding no\n",
                vec![(Tcp::No, (2, false))],
            ),
            // `invalid-user` is never the daemon's own account.
            (
                "Match invalid-user\n AllowTcpForwarding no\n",
                vec![(Tcp::All, (0, false))],
            ),
            // `all` takes no argument (and might apply: the owner's strict reading).
            (
                "Match all\n AllowTcpForwarding no\n",
                vec![(Tcp::No, (2, true)), (Tcp::All, (0, false))],
            ),
            // A criterion without its argument: might apply.
            (
                "Match User user Group\n AllowTcpForwarding no\n",
                vec![(Tcp::No, (2, true)), (Tcp::All, (0, false))],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(&tcp(text), want, "{text:?}");
        }
    }

    /// sshd reads `any` and `none` case-sensitively and refuses port 0: a list it would refuse
    /// permits nothing.
    #[test]
    fn permit_lists_sshd_would_refuse_permit_nothing() {
        let open = |t: &str| parse(t).permit_open.values[0].0.clone();
        let listen = |t: &str| parse(t).permit_listen.values[0].0.clone();
        for t in ["PermitOpen ANY\n", "PermitOpen None\n", "PermitOpen db:0\n"] {
            assert_eq!(open(t), PermitList::Unrecognized, "{t:?}");
        }
        for t in [
            "PermitListen Any\n",
            "PermitListen 0\n",
            "PermitListen localhost:0\n",
        ] {
            assert_eq!(listen(t), PermitList::Unrecognized, "{t:?}");
        }
        // The policy file is fil's own: there `ANY` is `any`.
        assert_eq!(
            PermitList::parse(&["ANY".to_string()], false, false),
            PermitList::Any
        );
    }

    fn words(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|w| w.to_string()).collect())
    }

    /// `argv_split` (OpenSSH 8.7 and later) and `strdelim` (before), word for word.
    #[test]
    fn lines_split_as_each_openssh_release_splits_them() {
        for (rest, argv, legacy) in [
            ("dev", words(&["dev"]), words(&["dev"])),
            ("'dev'", words(&["dev"]), words(&["'dev'"])),
            ("de\"v\"", words(&["dev"]), words(&["dev"])),
            ("\"a b\" c", words(&["a b", "c"]), words(&["a b", "c"])),
            ("a\"b c\"d", words(&["ab cd"]), words(&["ab c", "d"])),
            ("d\\'v", words(&["d'v"]), words(&["d\\'v"])),
            ("d\\ev", words(&["d\\ev"]), words(&["d\\ev"])),
            ("a\\ b", words(&["a b"]), words(&["a\\", "b"])),
            ("a # c", words(&["a"]), words(&["a"])),
            ("a=b", words(&["a=b"]), words(&["a", "b"])),
            ("''", words(&[""]), words(&["''"])),
            ("\"\" x", words(&["", "x"]), words(&[])),
            ("'dev", None, words(&["'dev"])),
            ("\"dev", None, None),
        ] {
            assert_eq!(argv_split(rest), argv, "argv {rest:?}");
            assert_eq!(strdelim_split(rest), legacy, "strdelim {rest:?}");
        }
    }

    /// Quoted `Match` arguments: what every OpenSSH release reads alike is evaluated; where
    /// releases differ (`'dev'` is `dev` to 9.9 and later, a literal `'dev'` before), or a line
    /// cannot be split, the block might apply. Never "does not apply" where sshd may apply it.
    #[test]
    fn quoted_match_arguments_are_read_as_sshd_reads_them() {
        let surely = vec![(Tcp::No, 2, false)];
        let maybe = vec![(Tcp::No, 2, true), (Tcp::All, 0, false)];
        let not = vec![(Tcp::All, 0, false)];
        let cases: &[(&str, &Vec<Outcome>)] = &[
            ("Match User 'dev'", &maybe),
            ("Match User de\"v\"", &surely),
            ("Match User \"dev\"", &surely),
            ("Match User \"d*\"", &surely),
            ("Match Group 'staff'", &maybe),
            ("Match Group st\"aff\"", &surely),
            ("Match User d\\ev", &not),
            ("Match User 'alice'", &not),
            ("Match User 'dev", &maybe),
            ("Match User \"dev", &maybe),
            ("Match 'User' dev", &maybe),
            ("Match User=", &maybe),
            ("Match User \"\"", &maybe),
            ("Match User '#dev'", &maybe),
            ("Match User dev # a comment", &surely),
        ];
        for (m, want) in cases {
            assert_eq!(
                &tcp_of(&format!("{m}\n AllowTcpForwarding no\n")),
                *want,
                "{m:?}"
            );
        }
    }

    /// A criterion with an empty argument (`User=`, `User ""`) is one sshd refuses ("Missing
    /// Match criteria"): it might apply, never "does not".
    #[test]
    fn an_empty_match_argument_might_apply() {
        let account = me();
        let p = Parser {
            account: &account,
            base: PathBuf::from("/"),
            acc: Default::default(),
            blocks: 0,
            files: 0,
            unreadable: Vec::new(),
            unevaluable: None,
            watched: Vec::new(),
        };
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for args in [
            w(&["User="]),
            w(&["User", ""]),
            w(&["Group="]),
            w(&["user=dev", "group="]),
        ] {
            assert_eq!(p.applies(&args), Applies::Maybe, "{args:?}");
        }
        assert_eq!(p.applies(&w(&["User=dev"])), Applies::Yes);
    }

    /// A value releases read differently is one fil does not know (strict); one they all read
    /// alike is read.
    #[test]
    fn quoted_values_are_read_only_where_every_release_agrees() {
        for (t, want) in [
            ("AllowTcpForwarding \"no\"\n", Tcp::No),
            ("AllowTcpForwarding n\"o\"\n", Tcp::No),
            ("AllowTcpForwarding 'no'\n", Tcp::Unrecognized),
            ("AllowTcpForwarding 'yes'\n", Tcp::Unrecognized),
            ("AllowTcpForwarding y\\es\n", Tcp::Unrecognized),
        ] {
            assert_eq!(tcp_of(t), vec![(want, 1, false)], "{t:?}");
        }
    }

    /// An `Include` line OpenSSH releases split differently (`argv_split` since 8.7, `strdelim`
    /// before), or that one of them cannot split, is a line fil cannot evaluate: which files
    /// sshd reads is not known. It includes nothing, and the mirror is unevaluable (naming the
    /// line), not merely incomplete: fil read the file, it cannot tell what the line means. The
    /// twins every release splits alike are read.
    #[test]
    fn an_include_releases_split_differently_makes_the_mirror_unevaluable() {
        let t = scratch(&[
            ("a b.conf", "AllowTcpForwarding no\n"),
            ("ab cd.conf", "AllowTcpForwarding no\n"),
            ("a.conf", "AllowTcpForwarding no\n"),
        ]);
        let d = t.path();
        for pattern in [
            // `a b.conf` to 8.7 and later, `'a` and `b.conf'` before.
            "'conf.d/a b.conf'",
            // A backslash before a space: one word to `argv_split`, two to `strdelim`.
            "conf.d/a\\ b.conf",
            // A quote inside a word: `ab cd.conf` to one, `ab c` and `d.conf` to the other.
            "conf.d/a\"b c\"d.conf",
            // A single quote kept by `strdelim` and dropped by `argv_split`.
            "conf.d/'a.conf'",
            // Unclosed: `argv_split` cannot split it (and sshd refuses the config).
            "'conf.d/a.conf",
            "\"conf.d/a.conf",
        ] {
            let s = include_settings(d, pattern);
            assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)), "{pattern}");
            assert!(s.unreadable.is_empty(), "{pattern}: {:?}", s.unreadable);
            // Nothing was included: the value is the main file's.
            assert_eq!(
                s.tcp.values.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
                [Tcp::Local],
                "{pattern}"
            );
        }
        // Double quotes around a whole word: every release reads them alike, and it is read.
        let s = include_settings(d, "\"conf.d/a b.conf\"");
        assert_eq!(s.unevaluable, None);
        assert!(s.unreadable.is_empty());
        assert_eq!(
            s.tcp.values.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            [Tcp::No]
        );
    }

    /// sshd reads the keyword with `strdelim`, which reads a quoted word: `"Match" User x` is a
    /// `Match` to sshd, where fil would skip a line whose keyword is `"Match"` (with the
    /// quotes). The probe: a `Match` for another user and then `GatewayPorts yes` read as a
    /// global `GatewayPorts yes` by fil, allowing a wide `-R` that sshd keeps on loopback.
    /// Where the two readings differ and either is a keyword fil mirrors, the config is
    /// unevaluable, naming the line.
    #[test]
    fn a_keyword_sshd_reads_differently_makes_the_mirror_unevaluable() {
        let t = scratch(&[("a.conf", "AllowTcpForwarding no\n")]);
        let d = t.path();
        // The review's probe.
        let s = write_main(d, b"\"Match\" User someoneelse\n    GatewayPorts yes\n");
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)));
        for text in [
            "\"Include\" conf.d/a.conf\n",
            "\"AllowTcpForwarding\" no\n",
            "\"GatewayPorts\" yes\n",
            "\"match\" all\n",
            "\"PERMITOPEN\" none\n",
            // A line that starts with `=`: fil reads no keyword, sshd the next word.
            "=Match User someoneelse\n",
            "= AllowAgentForwarding no\n",
        ] {
            let s = write_main(d, format!("# x\n{text}").as_bytes());
            assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 2)), "{text:?}");
        }
        // Where the readings agree, or neither is mirrored, the line is read as before.
        for (text, tcp) in [
            ("AllowTcpForwarding no\n", Tcp::No),
            ("AllowTcpForwarding=no\n", Tcp::No),
            ("AllowTcpForwarding = no\n", Tcp::No),
            ("allowtcpforwarding\tno\n", Tcp::No),
            ("\"Banner\" /etc/issue\nAllowTcpForwarding no\n", Tcp::No),
            ("\"Banner /etc/issue\nAllowTcpForwarding no\n", Tcp::No),
            ("=\nAllowTcpForwarding no\n", Tcp::No),
        ] {
            let s = write_main(d, text.as_bytes());
            assert_eq!(s.unevaluable, None, "{text:?}");
            assert_eq!(s.tcp.values[0].0, tcp, "{text:?}");
        }
    }

    /// `strdelim`'s first word, as sshd reads a keyword.
    #[test]
    fn the_keyword_is_read_as_strdelim_reads_it() {
        for (line, want) in [
            ("Match User x", Some("Match")),
            ("Match=User x", Some("Match")),
            ("Match\tUser", Some("Match")),
            ("\"Match\" User x", Some("Match")),
            ("Mat\"ch x\" y", Some("Match x")),
            ("=Match User x", Some("Match")),
            ("= Match", Some("Match")),
            ("=", Some("")),
            ("\"Match User x", None),
        ] {
            assert_eq!(sshd_keyword(line).as_deref(), want, "{line:?}");
        }
    }

    /// An include directory the daemon may not list (sshd, as root, can) is reported like an
    /// include file it cannot read: the mirror is incomplete. A missing one is not.
    #[test]
    fn an_include_directory_that_cannot_be_listed_marks_the_mirror_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().as_raw() == 0 {
            return; // root lists it anyway
        }
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(
            dir.join("conf.d/10-hardening.conf"),
            "AllowTcpForwarding no\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sshd_config"),
            "Include conf.d/*.conf\nInclude missing.d/*.conf\n",
        )
        .unwrap();
        std::fs::set_permissions(dir.join("conf.d"), std::fs::Permissions::from_mode(0o300))
            .unwrap();
        let (r, _) = read(&dir.join("sshd_config"), &me());
        std::fs::set_permissions(dir.join("conf.d"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        assert_eq!(s.unreadable, vec![dir.join("conf.d")]);
    }

    /// The fuzzer's finding on the file cap: past it, each `Include` line still globbed and
    /// stamped the directories, so a short config of `Include` lines cost seconds. Now the
    /// patterns past the cap are reported as unread and nothing is listed for them.
    #[test]
    fn includes_past_the_file_cap_cost_nothing_and_mark_the_mirror_incomplete() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        for i in 0..=MAX_FILES + 10 {
            std::fs::write(dir.join(format!("conf.d/{i:04}.conf")), "# nothing\n").unwrap();
        }
        let lines = 3000;
        let mut text = String::new();
        for _ in 0..lines {
            text.push_str("Include conf.d/*.conf\n");
        }
        text.push_str("AllowTcpForwarding no\n");
        std::fs::write(dir.join("sshd_config"), text).unwrap();
        let (r, watched) = read(&dir.join("sshd_config"), &me());
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        // The first glob truncates at the cap (reported), and the other lines are reported
        // without a listing: the directory is watched once and each file once.
        assert!(s.unreadable.len() >= lines, "{}", s.unreadable.len());
        assert!(watched.len() <= MAX_FILES + 8, "{} stamps", watched.len());
        // What fil could read is still enforced.
        assert!(s.tcp.values.iter().any(|(t, _)| *t == Tcp::No));
    }

    /// A `Match User` or `Match Group` list with a subpattern of 1023 bytes or more: sshd answers
    /// "no match" for the whole list (a 1024-byte buffer), the permissive reading of a `Match`
    /// (the block does not apply). fil reads it as a block that *might* apply, so the stricter
    /// value wins; a negated match met before it still settles the block as not applying.
    #[test]
    fn a_long_match_subpattern_might_apply() {
        let long = |n: usize| "x".repeat(n);
        let might = vec![(Tcp::No, 3, true), (Tcp::All, 1, false)];
        let not = vec![(Tcp::All, 1, false)];
        let cases: Vec<(String, Vec<Outcome>)> = vec![
            // 1022 bytes fits: read as a pattern, `dev` does not match it.
            (format!("User {}", long(1022)), not.clone()),
            // 1023 does not fit: sshd gives up on the whole list.
            (format!("User {}", long(1023)), might.clone()),
            (format!("User {}", long(2000)), might.clone()),
            // Even after a positive.
            (format!("User dev,{}", long(1023)), might.clone()),
            // A negated match before it is sshd's answer at once ...
            (format!("User !dev,{}", long(1023)), not.clone()),
            // ... one after it is never reached.
            (format!("User {},!dev", long(1023)), might.clone()),
            // The same for groups (the account is in `staff`).
            (format!("Group staff,{}", long(1023)), might.clone()),
            (format!("Group {}", long(1023)), might.clone()),
            (format!("Group {}", long(1022)), not.clone()),
            (format!("Group !docker,{}", long(1023)), not.clone()),
            // Every criterion must hold: a surely-failing one still settles the block.
            (format!("User {} Group wheel", long(1023)), not.clone()),
            (format!("User {} Group staff", long(1023)), might.clone()),
        ];
        for (crit, want) in cases {
            let text = format!("AllowTcpForwarding yes\nMatch {crit}\n AllowTcpForwarding no\n");
            let short = crit.chars().take(24).collect::<String>();
            assert_eq!(tcp_of(&text), want, "Match {short}...");
        }
    }

    /// A scratch config directory with `conf.d`.
    fn scratch(files: &[(&str, &str)]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("conf.d")).unwrap();
        for (name, text) in files {
            std::fs::write(t.path().join("conf.d").join(name), text).unwrap();
        }
        t
    }

    /// The mirror of `Include pattern` in `dir` (then `AllowTcpForwarding local`).
    fn include_settings(dir: &Path, pattern: &str) -> Settings {
        write_main(
            dir,
            format!("Include {pattern}\nAllowTcpForwarding local\n").as_bytes(),
        )
    }

    /// The mirror of the main config `bytes` in `dir`.
    fn write_main(dir: &Path, bytes: &[u8]) -> Settings {
        std::fs::write(dir.join("sshd_config"), bytes).unwrap();
        let (r, _) = read(&dir.join("sshd_config"), &me());
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        *s
    }

    /// What `Include pattern` does in `dir`: the first `AllowTcpForwarding` (file name, or
    /// `main` when it is the one after the include) and the unreadable list.
    fn include_in(dir: &Path, pattern: &str) -> (Vec<(Tcp, String)>, Vec<PathBuf>) {
        let s = include_settings(dir, pattern);
        let got = s
            .tcp
            .values
            .iter()
            .map(|(v, src)| match src {
                Src::Line { file, .. } => {
                    // (The path is as the glob wrote it, `./` and all; compared without.)
                    let rel = file.strip_prefix(dir).unwrap();
                    (
                        *v,
                        rel.components().collect::<PathBuf>().display().to_string(),
                    )
                }
                Src::Default => (*v, "default".into()),
            })
            .collect();
        (got, s.unreadable)
    }

    /// An `Include` whose glob means something different to another `glob(3)` than to
    /// OpenBSD's is a line fil cannot evaluate: the mirror names it (file and line) and refuses
    /// everything ([`Settings::unevaluable`]); its evaluable twin is read.
    #[test]
    fn an_include_glob_fil_cannot_evaluate_makes_the_mirror_unevaluable() {
        let t = scratch(&[("a.conf", "AllowTcpForwarding no\n"), ("5.conf", "")]);
        let d = t.path();
        // (pattern, evaluable?)
        for (pattern, ok) in [
            // `[^x]` negates in glibc and not in OpenBSD's glob.
            ("conf.d/[^x]*.conf", false),
            ("conf.d/[!x]*.conf", true),
            // POSIX classes.
            ("conf.d/[[:alpha:]]*.conf", false),
            ("conf.d/[a-z]*.conf", true),
            // A backslash inside a set.
            ("conf.d/[\\a]*.conf", false),
            ("conf.d/[a]\\*.conf", true),
            // A set that spans two path components.
            ("conf.d/[a/b]*.conf", false),
            ("conf.d/a\\/b.conf", false),
            // A set fil reads one way and another glob another.
            ("conf.d/[a-C]*.conf", false),
            ("conf.d/[aé]*.conf", false),
            // The common case.
            ("conf.d/*.conf", true),
            ("conf.d/a.conf", true),
            ("conf.d/?.conf", true),
        ] {
            let s = include_settings(d, pattern);
            if ok {
                assert_eq!(s.unevaluable, None, "{pattern}");
                assert!(s.unreadable.is_empty(), "{pattern}: {:?}", s.unreadable);
            } else {
                // The line is named, not skipped as an unreadable include.
                assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)), "{pattern}");
                assert!(s.unreadable.is_empty(), "{pattern}: {:?}", s.unreadable);
            }
        }
        // The evaluable twins are read.
        assert_eq!(
            include_in(d, "conf.d/[!x]*.conf").0,
            vec![(Tcp::No, "conf.d/a.conf".to_string())]
        );
        // The line number is the include's, in whichever file it is.
        let s = write_main(d, b"# one\nInclude conf.d/b.conf\n");
        assert_eq!(s.unevaluable, None);
        std::fs::write(d.join("conf.d/b.conf"), "# 1\n# 2\nInclude conf.d/[^x]\n").unwrap();
        let s = write_main(d, b"# one\nInclude conf.d/b.conf\n");
        assert_eq!(s.unevaluable, Some((d.join("conf.d/b.conf"), 3)));
        // The first is the one named.
        std::fs::write(
            d.join("conf.d/b.conf"),
            "Include conf.d/[^x]\nInclude conf.d/[^y]\n",
        )
        .unwrap();
        let s = write_main(d, b"# one\nInclude conf.d/b.conf\n");
        assert_eq!(s.unevaluable, Some((d.join("conf.d/b.conf"), 1)));
    }

    /// The review's probe (F1): `Path` drops a trailing `/` and `.` components, sshd's glob
    /// keeps them, and `lstat("a.conf/")` is `ENOTDIR`: those patterns include nothing, where
    /// the normalized path read `a.conf`.
    #[test]
    fn a_trailing_slash_or_dot_component_after_a_file_includes_nothing() {
        let t = scratch(&[("a.conf", "AllowTcpForwarding yes\n")]);
        std::fs::create_dir_all(t.path().join("conf.d/sub")).unwrap();
        let d = t.path();
        let read_a = (Tcp::All, "conf.d/a.conf".to_string());
        let nothing = (Tcp::Local, "sshd_config".to_string());
        let read_via_sub = (Tcp::All, "conf.d/sub/../a.conf".to_string());
        for (pattern, want) in [
            // The probe's three rows: nothing.
            ("conf.d/*/", &nothing),
            ("conf.d/a.conf/", &nothing),
            ("conf.d/*.conf/.", &nothing),
            // The same with the other ways to say it.
            ("conf.d/a.conf/.", &nothing),
            ("conf.d/a.conf/..", &nothing),
            ("conf.d/a.conf//", &nothing),
            ("conf.d/?.conf/", &nothing),
            ("conf.d/a.conf/./", &nothing),
            ("conf.d/*.conf/x", &nothing),
            // `.` and `/` between directories are fine: they agree with sshd.
            ("conf.d/./*.conf", &read_a),
            ("conf.d//*.conf", &read_a),
            ("conf.d/./a.conf", &read_a),
            ("conf.d/sub/../*.conf", &read_via_sub),
            ("conf.d/*.conf", &read_a),
            ("./conf.d/*.conf", &read_a),
        ] {
            let (got, unreadable) = include_in(d, pattern);
            let pattern = pattern.to_string();
            assert_eq!(got.first(), Some(want), "{pattern}: {got:?}");
            assert!(unreadable.is_empty(), "{pattern}: {unreadable:?}");
        }
        // A directory with a `/` after it is a directory (lstat finds it), and a directory is
        // not a config: nothing, and nothing unreadable either.
        let (got, unreadable) = include_in(d, "conf.d/sub/");
        assert_eq!(got.first(), Some(&nothing));
        assert!(unreadable.is_empty());
    }

    /// A pattern is the raw bytes: `glob` keeps what `Path::components` would drop.
    #[test]
    fn glob_keeps_the_trailing_slash_and_dot_components() {
        let t = scratch(&[("a.conf", "")]);
        let d = t.path();
        let names = |p: &str| -> Vec<String> {
            glob(&d.join(p))
                .files
                .iter()
                .map(|f| f.strip_prefix(d).unwrap().display().to_string())
                .collect()
        };
        assert_eq!(names("conf.d/*.conf"), ["conf.d/a.conf"]);
        assert_eq!(names("conf.d/./*.conf"), ["conf.d/./a.conf"]);
        assert_eq!(names("conf.d//*.conf"), ["conf.d//a.conf"]);
        assert!(names("conf.d/*.conf/").is_empty());
        assert!(names("conf.d/*.conf/.").is_empty());
        assert!(names("conf.d/a.conf/").is_empty());
        assert!(names("conf.d/*/").is_empty());
    }

    /// A pattern as long as the host's `PATH_MAX` or longer is one globs disagree on (OpenBSD's
    /// finds nothing, `GLOB_NOMATCH`; others glob it or fail): unevaluable, naming the line,
    /// and no directory is listed. One byte shorter, the glob is run.
    #[test]
    fn a_pattern_of_path_max_bytes_or_more_is_unevaluable() {
        let t = scratch(&[("a.conf", "AllowTcpForwarding no\n")]);
        let d = t.path();
        let prefix = d.join("conf.d/").display().to_string().len();
        for tail in ["*.conf", "a.conf"] {
            for len in [PATH_MAX, PATH_MAX + 50] {
                let pad = "x".repeat(len - prefix - tail.len());
                let pattern = format!("conf.d/{pad}{tail}");
                let g = glob(&d.join(&pattern));
                assert!(
                    g.files.is_empty() && g.dirs.is_empty() && g.unevaluable,
                    "{len}"
                );
                let s = include_settings(d, &pattern);
                assert!(s.unreadable.is_empty(), "{len}");
                assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)), "{len}");
            }
            // One byte under the limit: evaluated (and finds nothing here only because that
            // name does not exist).
            let pad = "x".repeat(PATH_MAX - 1 - prefix - tail.len());
            let pattern = format!("conf.d/{pad}{tail}");
            assert!(!glob(&d.join(&pattern)).unevaluable);
            assert_eq!(include_settings(d, &pattern).unevaluable, None);
        }
        // Just under the limit the glob is run (a wildcard lists the directory).
        let pad = "x".repeat(PATH_MAX - 1 - prefix - 1);
        let g = glob(&d.join(format!("conf.d/{pad}*")));
        assert!(!g.dirs.is_empty());
    }

    /// A NUL byte: sshd copies each line with `strlen`, so the rest of the line *and its
    /// newline* are gone and the next line is glued onto the start. A file with one is not read
    /// as it is written: unevaluable, naming the file and the line.
    #[test]
    fn a_nul_byte_in_a_file_makes_the_mirror_unevaluable() {
        let t = scratch(&[]);
        let d = t.path();
        let s = write_main(
            d,
            b"AllowTcpForwarding no\nAllowAgentForwarding\0 no\nGatewayPorts yes\n",
        );
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 2)));
        // In an included file, that file's line.
        std::fs::write(d.join("conf.d/z.conf"), b"# x\n# y\n\0\n").unwrap();
        let s = write_main(d, b"Include conf.d/z.conf\n");
        assert_eq!(s.unevaluable, Some((d.join("conf.d/z.conf"), 3)));
        // The first one is the one named.
        let s = write_main(d, b"\0\nInclude conf.d/z.conf\n");
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)));
        // None: a clean file.
        let s = write_main(d, b"AllowTcpForwarding no\n");
        assert_eq!(s.unevaluable, None);
    }

    /// A byte that is not UTF-8 in an `Include` line is a pattern fil would glob as text, with
    /// the replacement character in its place (matching nothing, silently, where sshd includes
    /// the file): unevaluable. In a `Match` line the block might apply. Elsewhere, nothing.
    #[test]
    fn a_byte_that_is_not_utf8_is_unevaluable_in_an_include_and_maybe_in_a_match() {
        let t = scratch(&[("a.conf", "AllowTcpForwarding no\n")]);
        let d = t.path();
        let s = write_main(
            d,
            b"# fine\nInclude conf.d/\xff.conf\nAllowTcpForwarding local\n",
        );
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 2)));
        let s = write_main(d, b"Include conf.d/a.conf conf.d/\xe9*\n");
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)));
        // A Match with such a byte might apply, whatever it says.
        let s = write_main(
            d,
            b"AllowTcpForwarding yes\nMatch User nobody\xff\n AllowTcpForwarding no\n",
        );
        assert_eq!(
            s.tcp.values.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            [Tcp::No, Tcp::All]
        );
        assert_eq!(s.unevaluable, None);
        // A comment, or a keyword fil does not mirror: not a line fil needs to read.
        let s = write_main(
            d,
            b"# caf\xe9\nBanner /etc/\xffissue\nAllowTcpForwarding no\n",
        );
        assert_eq!(s.unevaluable, None);
        // Valid UTF-8 outside ASCII is fine.
        let s = write_main(d, "Include conf.d/é*\n".as_bytes());
        assert_eq!(s.unevaluable, None);
    }

    /// A `PermitOpen` or `PermitListen` entry with a byte that is not UTF-8 is not the entry
    /// fil would compare (the text has the replacement character in its place): the list is one
    /// fil cannot read and permits nothing, not a list whose other entries still permit. A byte
    /// that is not UTF-8 in a comment on the line does not touch the entries.
    #[test]
    fn a_permit_entry_with_a_byte_that_is_not_utf8_is_unrecognized() {
        let t = scratch(&[]);
        let d = t.path();
        for text in [
            &b"PermitOpen db:5432 caf\xe9:80\n"[..],
            b"PermitOpen \xff:5432\n",
            b"PermitOpen db:5432 [fe80::1%\xff]:22\n",
        ] {
            let s = write_main(d, text);
            assert_eq!(
                s.permit_open.values[0].0,
                PermitList::Unrecognized,
                "{:?}",
                String::from_utf8_lossy(text)
            );
            assert_eq!(
                s.permit_open.values[0].0.permits_open("db", 5432),
                Err(Permit::Unrecognized)
            );
        }
        for text in [
            &b"PermitListen localhost:9000 caf\xe9:80\n"[..],
            b"PermitListen *\xff*:9000\n",
        ] {
            let s = write_main(d, text);
            assert_eq!(
                s.permit_listen.values[0].0,
                PermitList::Unrecognized,
                "{:?}",
                String::from_utf8_lossy(text)
            );
            assert_eq!(
                s.permit_listen.values[0]
                    .0
                    .permits_listen("localhost", 9000),
                Err(Permit::Unrecognized)
            );
        }
        // In a comment: the entries are read.
        let s = write_main(d, b"PermitOpen db:5432 # caf\xe9\n");
        assert_eq!(s.permit_open.values[0].0.permits_open("db", 5432), Ok(()));
        // Valid UTF-8 outside ASCII is an entry like any other.
        let s = write_main(d, "PermitOpen db:5432 café:80\n".as_bytes());
        assert_eq!(s.permit_open.values[0].0.permits_open("db", 5432), Ok(()));
        // Nothing of this is unevaluable: the setting is one fil cannot read, on its own.
        let s = write_main(d, b"PermitOpen \xff:5432\n");
        assert_eq!(s.unevaluable, None);
    }

    /// The review's probe (F2) end to end: a run of `[` in an `Include` line is read in time
    /// linear in its length.
    #[test]
    fn a_run_of_open_brackets_in_an_include_is_read_quickly() {
        let t = scratch(&[]);
        let d = t.path();
        let started = std::time::Instant::now();
        for n in [900, 3_000] {
            let pattern = format!("conf.d/{}", "[".repeat(n));
            let s = include_settings(d, &pattern);
            // (Past the host's `PATH_MAX` it is unevaluable for its length alone.)
            let long = d.join(&pattern).as_os_str().len() >= PATH_MAX;
            assert_eq!(s.unevaluable.is_some(), long, "{n}");
            let s = include_settings(d, &format!("conf.d/{}]", "[".repeat(n)));
            let _ = s;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A name `?` or a set cannot be matched identically on by bytes and by characters makes
    /// the include a line fil cannot evaluate (the name is left out, and the settings are not
    /// known); a pattern that reads the same either way is no problem.
    #[test]
    fn a_multibyte_name_a_wildcard_reads_two_ways_is_unevaluable() {
        let t = scratch(&[
            ("a.conf", "AllowTcpForwarding no\n"),
            ("é.conf", "AllowTcpForwarding yes\n"),
        ]);
        let d = t.path();
        // `?` is one byte or one character: `é.conf` is in doubt, `a.conf` is not.
        let s = include_settings(d, "conf.d/?.conf");
        assert_eq!(s.unevaluable, Some((d.join("sshd_config"), 1)));
        // `*` is the same either way: both are read, sorted by bytes (`a` before `é`).
        let s = include_settings(d, "conf.d/*.conf");
        assert_eq!(s.unevaluable, None);
        assert!(s.unreadable.is_empty());
        let (got, _) = include_in(d, "conf.d/*.conf");
        assert_eq!(got, vec![(Tcp::No, "conf.d/a.conf".to_string())]);
        // A multibyte pattern against ASCII names, and `?` against ASCII names: no doubt.
        assert_eq!(include_settings(d, "conf.d/é*").unevaluable, None);
    }

    /// glob(3) sorts the whole result with `strcmp`: `a-b/x.conf` comes before `a/x.conf` (`-` is
    /// before `/`), where sorting directory by directory would put `a` first. The first value
    /// wins, so the order decides.
    #[test]
    fn include_results_are_sorted_as_whole_paths() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        for (dir, v) in [("a", "yes"), ("a-b", "no")] {
            std::fs::create_dir_all(d.join("conf.d").join(dir)).unwrap();
            std::fs::write(
                d.join("conf.d").join(dir).join("x.conf"),
                format!("AllowTcpForwarding {v}\n"),
            )
            .unwrap();
        }
        let (got, _) = include_in(d, "conf.d/*/x.conf");
        assert_eq!(got[0], (Tcp::No, "conf.d/a-b/x.conf".to_string()));
        let g = glob(&d.join("conf.d/*/x.conf"));
        let names: Vec<_> = g
            .files
            .iter()
            .map(|f| f.strip_prefix(d).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["conf.d/a-b/x.conf", "conf.d/a/x.conf"]);
    }

    /// sshd's `readdir` yields `.` and `..`, which a component that starts with a dot matches:
    /// `.*` goes through them.
    #[test]
    fn a_dot_star_component_goes_through_dot_and_dot_dot() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir_all(d.join("sub/.hidden")).unwrap();
        for f in ["x.conf", "sub/x.conf", "sub/.hidden/x.conf"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        let g = glob(&d.join("sub/.*/x.conf"));
        let names: Vec<_> = g
            .files
            .iter()
            .map(|f| f.strip_prefix(d).unwrap().display().to_string())
            .collect();
        // Sorted by bytes: `..` before `.` before `.hidden`.
        assert_eq!(
            names,
            ["sub/../x.conf", "sub/./x.conf", "sub/.hidden/x.conf"]
        );
        // And a pattern without a leading dot matches none of them.
        let g = glob(&d.join("sub/*/x.conf"));
        assert!(g.files.iter().all(|f| !f.exists()));
    }

    /// A name that is not UTF-8 is still a name (a lossy conversion would name a file that does
    /// not exist, and the include would be skipped without a word). Skipped where the file
    /// system refuses such names (APFS).
    #[test]
    fn a_file_name_that_is_not_utf8_is_included() {
        use std::os::unix::ffi::OsStringExt;
        let t = scratch(&[]);
        let d = t.path();
        let name = std::ffi::OsString::from_vec(b"\xff1.conf".to_vec());
        if std::fs::write(d.join("conf.d").join(&name), "AllowTcpForwarding no\n").is_err() {
            return;
        }
        let g = glob(&d.join("conf.d/*.conf"));
        assert_eq!(g.files, vec![d.join("conf.d").join(&name)]);
        let (got, unreadable) = include_in(d, "conf.d/*.conf");
        assert_eq!(got[0].0, Tcp::No);
        assert!(unreadable.is_empty());
    }

    #[test]
    fn many_stars_through_a_match_line_cost_no_more_than_the_length_of_the_input() {
        // The fuzzer's finding: a recursive match of this many stars against a name that does
        // not match never finished (it is exponential in the stars); the matchers themselves
        // are tested in `pattern`.
        let pattern = format!("{}b", "a*".repeat(40));
        let s = parse(&format!("Match User {pattern}\nDisableForwarding yes\n"));
        assert!(s.disable.values.iter().all(|(f, _)| *f == Flag::No));
    }
}
