//! The sshd mirror (DESIGN.md section 11.2): the forwarding settings of the host's
//! `sshd_config`, as sshd would apply them to the daemon's user, as far as fil can tell.
//!
//! - **Settings:** `DisableForwarding`, `AllowTcpForwarding`, `AllowAgentForwarding`,
//!   `PermitOpen`, `PermitListen`, `GatewayPorts`. Every other keyword is skipped.
//! - **Parsing as sshd does:** keywords are case-insensitive, `keyword value` or
//!   `keyword=value`, a word starting with `#` ends the line; quotes and escapes are read as
//!   every OpenSSH release reads them, and where releases differ (single quotes, a quote inside
//!   a word, backslashes) a value is one fil does not know and a `Match` might apply; the first value
//!   of a setting wins; `Include` takes globs, relative to the main config's directory
//!   (`/etc/ssh`), in sorted order, at most 16 deep, and a file included from a `Match` block
//!   is inside it.
//! - **Two pattern languages** ([`super::pattern`]): `Match` criteria use OpenSSH's `match_pattern`
//!   (`*`, `?` one byte, `!` in comma lists; no brackets), `Include` uses `glob(3)` (brackets, escapes).
//!   An `Include` glob whose meaning differs between `glob(3)` implementations is skipped and the
//!   mirror marked incomplete; a `Match` list fil cannot read as sshd does might apply.
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
    /// the order met), and files whose `Include` lines fil could not split: skipped.
    pub unreadable: Vec<PathBuf>,
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
        watched: vec![Stamp::of(main)],
    };
    let text = match std::fs::read(main) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Sshd::Missing, p.watched),
        Err(_) => return (Sshd::Unknown, p.watched),
    };
    let mut ctx = Ctx::Global;
    p.file(main, &text, &mut ctx, Applies::Yes, 0);
    let a = p.acc;
    let settings = Settings {
        disable: a.disable.finish(Flag::No),
        tcp: a.tcp.finish(Tcp::All),
        agent: a.agent.finish(Flag::Yes),
        permit_open: a.permit_open.finish(PermitList::Any),
        permit_listen: a.permit_listen.finish(PermitList::Any),
        gateway: a.gateway.finish(Gateway::No),
        unreadable: p.unreadable,
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
    watched: Watched,
}

impl Parser<'_> {
    /// One file's lines; `ctx` is the `Match` context, which an included file starts in and
    /// which goes back to the includer's when the file ends. `cap` is how far the block that
    /// included the file applies: a `Match` in the file applies at most that far. That is sshd's
    /// `SSHCFG_NEVERMATCH` for a file included from a block that does not apply, and the strict
    /// side for one that might.
    fn file(&mut self, path: &Path, text: &str, ctx: &mut Ctx, cap: Applies, depth: usize) {
        for (n, raw) in text.lines().enumerate() {
            let Some(l) = split_line(raw) else {
                continue;
            };
            let args = l.args();
            let line = u32::try_from(n + 1).unwrap_or(u32::MAX);
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
                    *ctx = Ctx::Match(self.blocks, cap.and(self.match_line(&l)));
                }
                "include" => {
                    let cap = cap.and(ctx.applies());
                    match l.agreed() {
                        Some(args) => self.include(args, path, ctx, cap, depth),
                        // sshd releases would include different files: fil reads none of
                        // them, and says the mirror is incomplete.
                        None => self.unreadable.push(path.to_path_buf()),
                    }
                }
                "disableforwarding" => self.acc.disable.set(c, flag(args), src(maybe)),
                "allowagentforwarding" => self.acc.agent.set(c, flag(args), src(maybe)),
                "allowtcpforwarding" => self.acc.tcp.set(c, tcp(args), src(maybe)),
                "gatewayports" => self.acc.gateway.set(c, gateway(args), src(maybe)),
                "permitopen" => {
                    self.acc
                        .permit_open
                        .set(c, PermitList::parse(args, false, true), src(maybe))
                }
                "permitlisten" => {
                    self.acc
                        .permit_listen
                        .set(c, PermitList::parse(args, true, true), src(maybe))
                }
                _ => {}
            }
        }
    }

    fn include(&mut self, args: &[String], from: &Path, ctx: &mut Ctx, cap: Applies, depth: usize) {
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
                // A pattern whose meaning differs between globs: fil includes none of it and
                // says the mirror is incomplete (the strict side is not a guess).
                self.unreadable.push(pattern.clone());
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
                        let text = String::from_utf8_lossy(&b).into_owned();
                        let outer = *ctx;
                        self.file(&f, &text, ctx, cap, depth + 1);
                        *ctx = outer;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
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

/// A config line as sshd splits it: the keyword (ended by whitespace or `=`, as `strdelim`
/// ends it), then the rest, after one optional `=`, split both ways ([`Line`]). Trailing
/// whitespace and form feeds are dropped first, as sshd drops them. `None` for a blank or
/// comment line.
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
    if keyword.is_empty() {
        return None;
    }
    let mut rest = s[end..].trim_start_matches(WHITESPACE);
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start_matches(WHITESPACE);
    }
    Some(Line {
        keyword,
        argv: argv_split(rest),
        legacy: strdelim_split(rest),
    })
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
    /// The directories it listed, stamped before they were listed.
    dirs: Vec<Stamp>,
    /// Directories it had to list and could not (not a missing one), or whose entries it could
    /// not match as sshd's glob would ([`Unevaluable`]): what they hold is unknown.
    unlistable: Vec<PathBuf>,
    /// It named more than [`MAX_FILES`]; the rest were dropped.
    truncated: bool,
    /// The pattern uses a construct fil cannot evaluate as sshd's `glob(3)` does (see
    /// `pattern::compile`): it names nothing, and the include is one fil could not read.
    unevaluable: bool,
}

/// The files a glob names, as sshd's `glob(3)` (called with no flags) does: `*`, `?`, `[...]` and
/// backslash escapes in any component; a leading `.` is matched only by a pattern that starts
/// with one (glob(7)); the result sorted by `strcmp` on the whole path. A path without a
/// wildcard names itself, whether or not it exists. A pattern whose meaning differs between
/// `glob(3)` implementations is not guessed at ([`Globbed::unevaluable`]).
fn glob(pattern: &Path) -> Globbed {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut g = Globbed {
        files: Vec::new(),
        dirs: Vec::new(),
        unlistable: Vec::new(),
        truncated: false,
        unevaluable: false,
    };
    let comps: Result<Vec<GlobComp>, Unevaluable> = glob_check(pattern.as_os_str().as_bytes())
        .and_then(|()| {
            pattern
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.as_bytes()),
                    std::path::Component::ParentDir => Some(&b".."[..]),
                    _ => None,
                })
                .map(GlobComp::new)
                .collect()
        });
    let Ok(comps) = comps else {
        g.unevaluable = true;
        return g;
    };
    let mut paths = vec![PathBuf::from("/")];
    let mut listed = false;
    for comp in &comps {
        let mut next = Vec::new();
        if let Some(name) = comp.literal() {
            next.extend(paths.iter().map(|b| b.join(OsStr::from_bytes(name))));
        } else {
            listed = true;
            for base in &paths {
                g.dirs.push(Stamp::of(base));
                let rd = match std::fs::read_dir(base) {
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
                        g.unlistable.push(base.clone());
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
                let mut unsure = false;
                names.retain(|n| match comp.matches(n) {
                    Ok(m) => m,
                    // A name the readings of the pattern disagree on: not included, and the
                    // directory is reported as one fil could not read all of.
                    Err(Unevaluable) => {
                        unsure = true;
                        false
                    }
                });
                if unsure {
                    g.unlistable.push(base.clone());
                }
                names.sort();
                next.extend(names.into_iter().map(|n| base.join(OsString::from_vec(n))));
            }
        }
        paths = next;
        if paths.len() > MAX_FILES {
            paths.truncate(MAX_FILES);
            g.truncated = true;
        }
    }
    if listed {
        // glob(3) sorts the whole result with `strcmp`, not directory by directory, and a path
        // compares as a string (`a-b/x` comes before `a/z`; `Path`'s ordering would not).
        paths.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    }
    // Only files: a directory a glob names is not a config.
    g.files = paths.into_iter().filter(|p| !p.is_dir()).collect();
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

    /// An `Include` line releases split differently includes nothing, and the mirror says it
    /// is incomplete.
    #[test]
    fn an_include_releases_split_differently_marks_the_mirror_incomplete() {
        let dir_tmp = tempfile::tempdir().unwrap();
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.conf"), "AllowTcpForwarding yes\n").unwrap();
        std::fs::write(dir.join("sshd_config"), "Include 'a b.conf'\n").unwrap();
        let (r, _) = read(&dir.join("sshd_config"), &me());
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        assert_eq!(s.unreadable, vec![dir.join("sshd_config")]);
        assert_eq!(s.tcp.values, vec![(Tcp::All, Src::Default)]);
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

    /// What `Include pattern` does in `dir`: the first `AllowTcpForwarding` (file name, or
    /// `main` when it is the one after the include) and the unreadable list.
    fn include_in(dir: &Path, pattern: &str) -> (Vec<(Tcp, String)>, Vec<PathBuf>) {
        std::fs::write(
            dir.join("sshd_config"),
            format!("Include {pattern}\nAllowTcpForwarding local\n"),
        )
        .unwrap();
        let (r, _) = read(&dir.join("sshd_config"), &me());
        let Sshd::Mirrored(s) = r else {
            panic!("{r:?}")
        };
        let got = s
            .tcp
            .values
            .iter()
            .map(|(v, src)| match src {
                Src::Line { file, .. } => {
                    (*v, file.strip_prefix(dir).unwrap().display().to_string())
                }
                Src::Default => (*v, "default".into()),
            })
            .collect();
        (got, s.unreadable)
    }

    /// An `Include` whose glob means something different to another `glob(3)` than to
    /// OpenBSD's is skipped and the mirror says it is incomplete; its evaluable twin is read.
    #[test]
    fn an_include_glob_fil_cannot_evaluate_is_skipped_and_marks_the_mirror_incomplete() {
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
            let (got, unreadable) = include_in(d, pattern);
            if ok {
                assert!(unreadable.is_empty(), "{pattern}: {unreadable:?}");
            } else {
                // Skipped: nothing of the include is in force, and the pattern is named.
                assert_eq!(unreadable, vec![d.join(pattern)], "{pattern}");
                assert_eq!(
                    got,
                    vec![(Tcp::Local, "sshd_config".to_string())],
                    "{pattern}"
                );
            }
        }
        // The evaluable twins are read.
        assert_eq!(
            include_in(d, "conf.d/[!x]*.conf").0,
            vec![(Tcp::No, "conf.d/a.conf".to_string())]
        );
    }

    /// A name `?` or a set cannot be matched identically on by bytes and by characters leaves
    /// the directory marked unreadable and the name out; the rest of the directory is read.
    #[test]
    fn a_multibyte_name_a_wildcard_reads_two_ways_marks_the_directory() {
        let t = scratch(&[
            ("a.conf", "AllowTcpForwarding no\n"),
            ("é.conf", "AllowTcpForwarding yes\n"),
        ]);
        let d = t.path();
        // `?` is one byte or one character: `é.conf` is in doubt, `a.conf` is not.
        let (got, unreadable) = include_in(d, "conf.d/?.conf");
        assert_eq!(got, vec![(Tcp::No, "conf.d/a.conf".to_string())]);
        assert_eq!(unreadable, vec![d.join("conf.d")]);
        // `*` is the same either way: both are read, sorted by bytes (`a` before `é`).
        let (got, unreadable) = include_in(d, "conf.d/*.conf");
        assert_eq!(got, vec![(Tcp::No, "conf.d/a.conf".to_string())]);
        assert!(unreadable.is_empty());
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
