//! The sshd mirror (DESIGN.md section 11.2): the forwarding settings of the host's
//! `sshd_config`, as sshd would apply them to the daemon's user, as far as fil can tell.
//!
//! - **Settings:** `DisableForwarding`, `AllowTcpForwarding`, `AllowAgentForwarding`,
//!   `PermitOpen`, `PermitListen`, `GatewayPorts`. Every other keyword is skipped.
//! - **Parsing as sshd does:** keywords are case-insensitive, `keyword value` or
//!   `keyword=value`, double quotes group, a word starting with `#` ends the line; the first value
//!   of a setting wins; `Include` takes globs, relative to the main config's directory
//!   (`/etc/ssh`), in sorted order, at most 16 deep, and a file included from a `Match` block
//!   is inside it.
//! - **`Match`:** `User` and `Group` (pattern lists with `*`, `?` and `!`) are evaluated against
//!   the daemon's user. Every other criterion (`Address`, `Host`, `LocalAddress`, `LocalPort`,
//!   `RDomain`, `all`, ...) and every line fil cannot read **might apply**: its value is one of
//!   the outcomes, and a forward must be allowed by every outcome (the stricter wins).
//! - **Defaults** are OpenSSH's: forwarding and the agent allowed, `PermitOpen` and
//!   `PermitListen` `any`, `GatewayPorts no`.
//! - **A value fil does not know** is read as the strict one.
//! - **An included file that cannot be read** is skipped and reported ([`Settings::unreadable`]):
//!   what fil can read is still enforced. The main file unreadable is [`Sshd::Unknown`].
//!
//! Never logged: the config is the admin's, and what fil found in it is said only as the one
//! deciding setting of a refusal.

use std::path::{Path, PathBuf};

use fil_proto::mux::{Permit, TcpForwarding, Toggle};

use super::Account;
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
    /// Included files that could not be read (in the order met): skipped.
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
    let dir = std::env::temp_dir().join(format!(
        "fil-sshd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    let r = read(&path, account).0;
    let _ = std::fs::remove_dir_all(&dir);
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
            let Some((keyword, args)) = split_line(raw) else {
                continue;
            };
            let line = u32::try_from(n + 1).unwrap_or(u32::MAX);
            let src = |maybe: bool| Src::Line {
                file: path.to_path_buf(),
                line,
                maybe,
            };
            let maybe = matches!(ctx, Ctx::Match(_, Applies::Maybe));
            let c = *ctx;
            match keyword.to_ascii_lowercase().as_str() {
                "match" => {
                    self.blocks += 1;
                    *ctx = Ctx::Match(self.blocks, cap.and(self.applies(&args)));
                }
                "include" => {
                    let cap = cap.and(ctx.applies());
                    self.include(&args, path, ctx, cap, depth)
                }
                "disableforwarding" => self.acc.disable.set(c, flag(&args), src(maybe)),
                "allowagentforwarding" => self.acc.agent.set(c, flag(&args), src(maybe)),
                "allowtcpforwarding" => self.acc.tcp.set(c, tcp(&args), src(maybe)),
                "gatewayports" => self.acc.gateway.set(c, gateway(&args), src(maybe)),
                "permitopen" => {
                    self.acc
                        .permit_open
                        .set(c, PermitList::parse(&args, false, true), src(maybe))
                }
                "permitlisten" => {
                    self.acc
                        .permit_listen
                        .set(c, PermitList::parse(&args, true, true), src(maybe))
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
            let g = glob(&pattern);
            self.watched.extend(g.dirs);
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

    /// A `Match` line, for the daemon's user. All criteria must hold; one that surely fails
    /// makes the block not apply, one fil cannot evaluate makes it one that might.
    ///
    /// Tokens are paired as sshd's `match_cfg_line` does: `crit=arg` is one criterion, `all`
    /// and `invalid-user` take no argument, every other criterion takes the next token.
    fn applies(&self, args: &[String]) -> Applies {
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
                ("user", Some(a)) => match &self.account.user {
                    Some(u) => yes_no(match_pattern_list(u, &a) == Some(true)),
                    None => Applies::Maybe,
                },
                ("group", Some(a)) => match &self.account.groups {
                    Some(g) => yes_no(group_match(g, &a)),
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

/// A config line as sshd splits it: the keyword (ended by whitespace or `=`), then the
/// arguments, with an optional `=` before them, double quotes grouping, and a word starting
/// with `#` ending the line. `None` for a blank or comment line. A line fil cannot split
/// (an unclosed quote) gives its keyword with no arguments, which every setting reads as a value
/// it does not know.
fn split_line(raw: &str) -> Option<(String, Vec<String>)> {
    let s = raw.trim_start_matches([' ', '\t', '\r', '\n', '\x0b', '\x0c']);
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
    let mut rest = s[end..].trim_start();
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start();
    }
    let mut args = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        if c == '#' {
            break;
        }
        let mut word = String::new();
        if c == '"' {
            chars.next();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                word.push(c);
            }
            if !closed {
                return Some((keyword, Vec::new()));
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_ascii_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
        }
        args.push(word);
    }
    Some((keyword, args))
}

/// OpenSSH's `match_pattern`: `*` any run, `?` any one character, the rest literally.
pub fn match_pattern(s: &str, pattern: &str) -> bool {
    fn go(s: &[u8], p: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| go(&s[i..], rest)),
            Some((b'?', rest)) => !s.is_empty() && go(&s[1..], rest),
            Some((c, rest)) => s.first() == Some(c) && go(&s[1..], rest),
        }
    }
    go(s.as_bytes(), pattern.as_bytes())
}

/// OpenSSH's `match_pattern_list` (case-sensitive): `Some(true)` if a pattern matches and no
/// negated one does, `Some(false)` if none does, `None` if a negated one does (which beats
/// everything).
pub fn match_pattern_list(s: &str, list: &str) -> Option<bool> {
    let mut positive = false;
    for p in list.split(',') {
        let (neg, p) = match p.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, p),
        };
        if match_pattern(s, p) {
            if neg {
                return None;
            }
            positive = true;
        }
    }
    Some(positive)
}

/// OpenSSH's `ga_match_pattern_list`: one of the user's groups matches, and none matches a
/// negated pattern.
fn group_match(groups: &[String], list: &str) -> bool {
    let mut found = false;
    for g in groups {
        match match_pattern_list(g, list) {
            None => return false,
            Some(true) => found = true,
            Some(false) => {}
        }
    }
    found
}

/// What a glob found.
struct Globbed {
    /// The files it names, sorted (as `glob(3)`).
    files: Vec<PathBuf>,
    /// The directories it listed, stamped before they were listed.
    dirs: Vec<Stamp>,
    /// It named more than [`MAX_FILES`]; the rest were dropped.
    truncated: bool,
}

/// The files a glob names. `*`, `?`, `[...]` and backslash escapes in any component; a leading
/// `.` is matched only by a pattern that starts with one. A path without a wildcard names
/// itself, whether or not it exists.
fn glob(pattern: &Path) -> Globbed {
    let mut dirs = Vec::new();
    let mut truncated = false;
    let mut paths = vec![PathBuf::from("/")];
    let comps: Vec<_> = pattern
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => Some("..".to_string()),
            _ => None,
        })
        .collect();
    for comp in &comps {
        let wild = comp.contains(['*', '?', '[', '\\']);
        let mut next = Vec::new();
        for base in &paths {
            if !wild {
                next.push(base.join(comp));
                continue;
            }
            dirs.push(Stamp::of(base));
            let Ok(rd) = std::fs::read_dir(base) else {
                continue;
            };
            let mut names: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| (!n.starts_with('.') || comp.starts_with('.')) && glob_match(n, comp))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|n| base.join(n)));
        }
        paths = next;
        if paths.len() > MAX_FILES {
            paths.truncate(MAX_FILES);
            truncated = true;
        }
    }
    // Only files: a directory a glob names is not a config.
    let files = paths
        .into_iter()
        .filter(|p| !p.is_dir())
        .collect::<Vec<_>>();
    Globbed {
        files,
        dirs,
        truncated,
    }
}

/// One glob component: `*`, `?` and bracket classes (`[abc]`, `[a-z]`, `[!x]`).
fn glob_match(s: &str, p: &str) -> bool {
    fn class(c: char, p: &[char]) -> Option<(bool, usize)> {
        // p starts after '['; returns (matched, length consumed including ']')
        let mut i = 0;
        let neg = matches!(p.first(), Some('!') | Some('^'));
        if neg {
            i += 1;
        }
        let mut hit = false;
        let mut first = true;
        while i < p.len() {
            if p[i] == ']' && !first {
                return Some((hit != neg, i + 1));
            }
            first = false;
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                if p[i] <= c && c <= p[i + 2] {
                    hit = true;
                }
                i += 3;
            } else {
                if p[i] == c {
                    hit = true;
                }
                i += 1;
            }
        }
        None
    }
    fn go(s: &[char], p: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| go(&s[i..], &p[1..])),
            Some('?') => !s.is_empty() && go(&s[1..], &p[1..]),
            Some('[') => match (
                s.first(),
                class(s.first().copied().unwrap_or('\0'), &p[1..]),
            ) {
                (Some(_), Some((true, n))) => go(&s[1..], &p[1 + n..]),
                (Some(_), Some((false, _))) | (None, _) => false,
                // An unclosed bracket is a literal `[`.
                (Some(c), None) => *c == '[' && go(&s[1..], &p[1..]),
            },
            // `glob(3)` without `GLOB_NOESCAPE`: a backslash makes the next character literal.
            Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && go(&s[1..], &p[2..]),
            Some(c) => s.first() == Some(c) && go(&s[1..], &p[1..]),
        }
    }
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = p.chars().collect();
    go(&s, &p)
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
        let dir = std::env::temp_dir().join(format!("fil-inc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sshd's `SSHCFG_NEVERMATCH`: a file included from a `Match` block that does not apply
    /// never applies a `Match` of its own; from one that might apply, its blocks only might.
    #[test]
    fn an_include_inside_a_match_block_is_capped_by_it() {
        let dir = std::env::temp_dir().join(format!("fil-inc-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn glob_components() {
        for (s, p, want) in [
            ("10-a.conf", "*.conf", true),
            ("10-a.conf", "1?-a.conf", true),
            ("10-a.conf", "[0-9]*", true),
            ("x.conf", "[!x]*", false),
            ("a]", "[]a]]", true),
            ("[x", "[x", true),
            ("a.conf", "*.txt", false),
            ("a*b", "a\\*b", true),
            ("axb", "a\\*b", false),
            ("[x]", "\\[x\\]", true),
        ] {
            assert_eq!(glob_match(s, p), want, "{s} {p}");
        }
    }
}
