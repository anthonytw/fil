//! The two pattern languages of `sshd_config`, kept apart (DESIGN.md section 11.2).
//!
//! sshd reads two different ones, and they must not be mixed:
//!
//! - **`match_pattern` / `match_pattern_list`** (OpenSSH's `match.c`), for `Match User`,
//!   `Match Group` and `PermitListen`: `*` any run, `?` any one **byte** (sshd's C code is
//!   byte-wise, so a multibyte character is several `?`), everything else literally (no
//!   brackets, no backslash, no braces); in a *list*, commas separate patterns and a leading `!`
//!   negates one. ssh_config(5) PATTERNS describes it, and sshd_config(5) refers to it for
//!   `Match` ("Patterns ... are as described in the PATTERNS section of ssh_config(5)").
//! - **`GlobComp`** (`glob(3)`, called with no flags), for `Include` file names: `*`, `?`,
//!   `[...]` sets, backslash escapes; no commas, no list negation, no braces. OpenSSH-portable
//!   builds against its own copy of OpenBSD's `glob.c` where the system `glob(3)` lacks
//!   `gl_matchc` or `gl_statv` (glibc, musl and macOS do not have them all), but a distribution
//!   may build against the system one: the two agree on the plain constructs and differ on
//!   others. fil evaluates what both agree on and refuses to guess at the rest: such a pattern
//!   is **[`Unevaluable`]**, and the caller treats the config as one whose settings are not
//!   known (everything is refused unless fil's own policy file says otherwise).
//!
//! Both matchers are linear in the product of their lengths however many `*` there are: a
//! recursive match is exponential in them, and the fuzzer found config lines that never finished.

use std::fmt;

// ---------------------------------------------------------------------------------------
// The shared walk.
// ---------------------------------------------------------------------------------------

/// Matches `s` against a pattern of `*` (`None`) and items that each consume exactly one input
/// element (`Some`, which `one` checks). The walk keeps the one place it can back up to, the last
/// `*`, so it is linear in the product of the lengths however many `*` there are. A recursive
/// match is exponential in them: the fuzzer found `Match User` and `Include` lines that never
/// finished, and both are read again whenever the config changes.
fn wild<T: Copy, K>(s: &[T], toks: &[Option<K>], one: impl Fn(&K, T) -> bool) -> bool {
    let (mut i, mut j) = (0, 0);
    // After the last `*`: the token just past it, and the input index its run has reached.
    let mut back: Option<(usize, usize)> = None;
    while i < s.len() {
        match toks.get(j) {
            Some(None) => {
                back = Some((j + 1, i));
                j += 1;
            }
            Some(Some(k)) if one(k, s[i]) => {
                i += 1;
                j += 1;
            }
            _ => match back {
                Some((tj, si)) => {
                    // The star takes one more element.
                    back = Some((tj, si + 1));
                    i = si + 1;
                    j = tj;
                }
                None => return false,
            },
        }
    }
    toks[j..].iter().all(Option::is_none)
}

// ---------------------------------------------------------------------------------------
// Match patterns: OpenSSH's match.c.
// ---------------------------------------------------------------------------------------

/// OpenSSH's `match_pattern` (ssh_config(5) PATTERNS): `*` any run, `?` any one **byte**, the
/// rest literally. Its C code walks bytes, so `?` does not consume a whole multibyte character:
/// `é` (two bytes) is `??`, not `?`.
pub fn match_pattern(s: &str, pattern: &str) -> bool {
    match_bytes(s.as_bytes(), pattern.as_bytes())
}

fn match_bytes(s: &[u8], pattern: &[u8]) -> bool {
    // `None` is a `*`; `Some(None)` is a `?`; `Some(Some(b))` is the byte `b`.
    let toks: Vec<Option<Option<u8>>> = pattern
        .iter()
        .map(|&b| match b {
            b'*' => None,
            b'?' => Some(None),
            b => Some(Some(b)),
        })
        .collect();
    wild(s, &toks, |k, c| k.is_none_or(|b| b == c))
}

/// What `match_pattern_list` said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMatch {
    /// A pattern matched and no negated one did (sshd's 1).
    Positive,
    /// None matched (sshd's 0).
    NoMatch,
    /// A negated pattern matched, which beats everything (sshd's -1).
    Negated,
    /// fil cannot say what sshd answers: a subpattern reached the length at which sshd gives up,
    /// or the list holds a NUL. See [`match_pattern_list`].
    Unsure,
}

/// The longest subpattern `match_pattern_list` takes: its buffer is `char sub[1024]`, and it
/// answers "no match" once a subpattern (without its `!`) fills it, which is 1023 bytes.
const SUBPATTERN_CAP: usize = 1023;

/// OpenSSH's `match_pattern_list` (ssh_config(5) PATTERNS; case-sensitive, as `Match User` and
/// `Match Group` call it): a comma-separated list, each pattern with an optional leading `!`.
///
/// **The cap.** sshd copies each subpattern into a 1024-byte buffer and, as soon as one does not
/// fit (1023 bytes or more), returns "no match" for the *whole list*, even after an earlier
/// positive. That is [`ListMatch::NoMatch`] to sshd; fil answers [`ListMatch::Unsure`]
/// instead. "No match" is the permissive reading of a `Match` (the block does not apply, so a
/// stricter outer value would not be combined with it), and the limit is an implementation
/// detail another release may change: the caller must read `Unsure` as *might apply*, the strict
/// side. A negated match met before the long subpattern is still [`ListMatch::Negated`], as sshd
/// returns it at once.
pub fn match_pattern_list(s: &str, list: &str) -> ListMatch {
    let (s, b) = (s.as_bytes(), list.as_bytes());
    // C strings end at a NUL; a list with one is not read the way it is written.
    if b.contains(&0) {
        return ListMatch::Unsure;
    }
    let mut positive = false;
    let mut i = 0;
    while i < b.len() {
        let negated = b[i] == b'!';
        if negated {
            i += 1;
        }
        let start = i;
        while i < b.len() && i - start < SUBPATTERN_CAP && b[i] != b',' {
            i += 1;
        }
        if i - start >= SUBPATTERN_CAP {
            return ListMatch::Unsure;
        }
        let sub = &b[start..i];
        if i < b.len() && b[i] == b',' {
            i += 1;
        }
        if match_bytes(s, sub) {
            if negated {
                return ListMatch::Negated;
            }
            positive = true;
        }
    }
    if positive {
        ListMatch::Positive
    } else {
        ListMatch::NoMatch
    }
}

// ---------------------------------------------------------------------------------------
// Include patterns: glob(3), as OpenBSD's glob.c reads them, where every glob agrees.
// ---------------------------------------------------------------------------------------

/// A glob pattern fil cannot evaluate the way sshd's `glob(3)` would, or a name it cannot say
/// matches identically under every reading. The caller treats it as unreadable, never as a
/// match and never as a non-match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unevaluable;

impl fmt::Display for Unevaluable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a glob fil cannot evaluate as sshd's would")
    }
}

/// One item of a compiled glob component that consumes one input unit. (A `*` is the `None`
/// that [`wild`] takes.)
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// `?`.
    One,
    Lit(u32),
    /// A bracket set: whether it is negated (`[!...]`) and its members as inclusive ranges (a
    /// single member is `(c, c)`).
    Set {
        neg: bool,
        ranges: Vec<(u32, u32)>,
    },
}

impl Item {
    fn accepts(&self, c: u32) -> bool {
        match self {
            Item::One => true,
            Item::Lit(l) => *l == c,
            Item::Set { neg, ranges } => {
                // OpenBSD `match()`: it fails when "found" equals "negated".
                ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *neg
            }
        }
    }
}

/// Input units above every Unicode scalar: a byte that is not valid UTF-8, in the character
/// reading of a name.
const INVALID: u32 = 0x11_0000;

fn byte_units(b: &[u8]) -> Vec<u32> {
    b.iter().map(|&x| u32::from(x)).collect()
}

/// A name as characters; each byte that is not valid UTF-8 is a unit of its own.
fn char_units(b: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(b.len());
    let mut rest = b;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.extend(s.chars().map(u32::from));
                break;
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                if let Ok(s) = std::str::from_utf8(good) {
                    out.extend(s.chars().map(u32::from));
                }
                let n = e.error_len().unwrap_or(bad.len()).max(1);
                out.extend(bad[..n].iter().map(|&x| INVALID + u32::from(x)));
                rest = &bad[n..];
            }
        }
    }
    out
}

const BACKSLASH: u32 = b'\\' as u32;
const SLASH: u32 = b'/' as u32;

/// Reads a glob pattern as OpenBSD's `glob()` and `glob0()` do, and says [`Unevaluable`] for
/// every construct on which another `glob(3)` (glibc, musl, the BSD libcs) may differ. The
/// pattern is `units` (bytes, or characters), and may hold `/`.
///
/// glob(3) and glob(7) (and sshd's call, which passes no flags, so no `GLOB_NOESCAPE`,
/// `GLOB_BRACE` or `GLOB_TILDE`):
///
/// - A backslash makes the next character literal, one that ends the pattern is a literal
///   backslash (glob(3) "GLOB_NOESCAPE": without it, "a backslash quotes the next character").
/// - `*` and `?` are any run and any one character; `[` opens a set only if a `]` follows it,
///   else it is a literal `[` (glob(7): "a `[` that is not part of a valid bracket expression
///   matches itself"); the first member of a set may be `]`; `!` first negates; `a-z` is a range
///   and a `-` first or last is a literal.
///
/// Where globs differ, so the pattern is refused:
///
/// - **`^` first in a set.** glob(7) names `!` as the negation; OpenBSD's glob has no other and
///   keeps `^` as a member, while other implementations (glibc, musl) also accept `^` as
///   negation. `[^x]` is the opposite set in the two.
/// - **`[:class:]`, `[.coll.]`, `[=equiv=]`** (glob(7) "Character classes"). fil does not
///   implement them. OpenBSD's glob supports the ctype classes (`g_charclass`; portable ships
///   `charclass.h`) and finds nothing for an unknown class name, and the other globs differ
///   again on the names and on collating symbols and equivalence classes, so a set that opens
///   with `[:`, `[.` or `[=` is refused rather than guessed at.
/// - **A backslash inside a set.** OpenBSD's glob quotes the next character (so `[\]]` is the
///   set `]`); glob(7) leaves it undefined, and some implementations keep the backslash as a
///   member.
/// - **A `-` that is not a clean range or a first or last member** (`[a-c-e]`, `[--a]`), and
///   **a range** that is reversed, or whose ends are not both digits, both lower case or both
///   upper case: POSIX leaves them undefined and collation-dependent (`[A-z]` spans the
///   punctuation in between in some locales and not in others).
/// - **A non-ASCII member** of a set: a byte in one reading, a character in the other.
/// - **A set that would contain `/`** (`[a/b]`, or a `[` whose closing `]` is in a later path
///   component), and **a backslash before `/`**: glob(7) says a slash can be matched only by a
///   slash, and implementations disagree on what a pattern like this means.
fn compile(units: &[u32]) -> Result<Vec<Option<Item>>, Unevaluable> {
    // glob(): a backslash protects the next character; a trailing one is itself.
    let mut v: Vec<(u32, bool)> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        i += 1;
        if c == BACKSLASH {
            match units.get(i) {
                Some(&n) => {
                    v.push((n, true));
                    i += 1;
                }
                None => v.push((BACKSLASH, true)),
            }
        } else {
            v.push((c, false));
        }
    }
    let n = v.len();
    // An unprotected `ch` at `k`.
    let is = |k: usize, ch: char| v.get(k).is_some_and(|&(c, p)| !p && c == u32::from(ch));
    // Whether an unprotected (`close`) or an escaped (`escaped`) `]` is at or after `k`, for
    // every `k`, in one pass: a run of `[` would otherwise look to the end of the pattern once
    // for each (quadratic: the fuzz-timeout class of bug).
    let close_at = u32::from(']');
    let mut close = vec![false; n + 1];
    let mut escaped = vec![false; n + 1];
    for k in (0..n).rev() {
        let (c, p) = v[k];
        close[k] = close[k + 1] || (!p && c == close_at);
        escaped[k] = escaped[k + 1] || (p && c == close_at);
    }
    let mut out: Vec<Option<Item>> = Vec::new();
    let mut i = 0;
    while i < n {
        let (c, protected) = v[i];
        i += 1;
        if protected {
            if c == SLASH {
                return Err(Unevaluable);
            }
            out.push(Some(Item::Lit(c)));
            continue;
        }
        match char::from_u32(c) {
            // glob0() collapses adjacent stars.
            Some('*') => {
                if out.last() != Some(&None) {
                    out.push(None);
                }
            }
            Some('?') => out.push(Some(Item::One)),
            Some('[') => {
                let neg = is(i, '!');
                let first = i + usize::from(neg);
                // glob0(): no `]` after the first member, so a literal `[` (and the `!` is read
                // again as itself).
                if first >= n || !close[first + 1] {
                    // ... unless an escaped `]` follows: OpenBSD's glob reads it as a plain `]`,
                    // and a glob that lets a backslash escape inside a set would read the `[` as
                    // opening a set that the escaped `]` does not close, or as one it does.
                    if escaped[first.min(n)] {
                        return Err(Unevaluable);
                    }
                    out.push(Some(Item::Lit(c)));
                    continue;
                }
                let (item, next) = set(&v, first, neg)?;
                out.push(Some(item));
                i = next;
            }
            _ => out.push(Some(Item::Lit(c))),
        }
    }
    Ok(out)
}

/// A bracket set whose body starts at `first` of `v`; returns it and the index past its `]`.
fn set(v: &[(u32, bool)], first: usize, neg: bool) -> Result<(Item, usize), Unevaluable> {
    let is = |k: usize, ch: char| v.get(k).is_some_and(|&(c, p)| !p && c == u32::from(ch));
    let dash = u32::from('-');
    // `^` first, not negated: the opposite set in glibc and musl.
    if !neg && is(first, '^') {
        return Err(Unevaluable);
    }
    let mut ranges = Vec::new();
    let mut k = first;
    loop {
        let Some(&(lo, lo_protected)) = v.get(k) else {
            return Err(Unevaluable);
        };
        let at = k;
        k += 1;
        // A backslash in the set (its character is protected), a non-ASCII member, a slash.
        if lo_protected || lo >= 0x80 || lo == SLASH {
            return Err(Unevaluable);
        }
        // `[:`, `[.`, `[=`: a class, a collating symbol, an equivalence class.
        if lo == u32::from('[') && (is(k, ':') || is(k, '.') || is(k, '=')) {
            return Err(Unevaluable);
        }
        // OpenBSD: a `-` next, and past it anything but the closing `]`, makes a range.
        let range_end =
            if is(k, '-') && v.get(k + 1).is_some_and(|&(c, p)| p || c != u32::from(']')) {
                let (hi, hi_protected) = v[k + 1];
                if hi_protected || hi >= 0x80 || hi == SLASH || hi == dash || lo == dash {
                    return Err(Unevaluable);
                }
                k += 2;
                Some(hi)
            } else {
                None
            };
        match range_end {
            Some(hi) => {
                let same_kind = |a: u32, b: u32| {
                    let kind = |c: u32| match char::from_u32(c) {
                        Some(c) if c.is_ascii_digit() => 1,
                        Some(c) if c.is_ascii_lowercase() => 2,
                        Some(c) if c.is_ascii_uppercase() => 3,
                        _ => 0,
                    };
                    kind(a) != 0 && kind(a) == kind(b)
                };
                if lo > hi || !same_kind(lo, hi) {
                    return Err(Unevaluable);
                }
                ranges.push((lo, hi));
            }
            None => {
                // A `-` member is a literal only first or last.
                if lo == dash && at != first && !is(k, ']') {
                    return Err(Unevaluable);
                }
                ranges.push((lo, lo));
            }
        }
        // The loop of glob0(): the next character, until an unprotected `]`.
        match v.get(k) {
            Some(&(c, p)) => {
                if !p && c == u32::from(']') {
                    return Ok((Item::Set { neg, ranges }, k + 1));
                }
            }
            None => return Err(Unevaluable),
        }
    }
}

/// Whether a whole `Include` pattern is one fil can evaluate: [`compile`]'s rules over all of
/// it, `/` included, which is where a set or a backslash that spans two path components shows.
pub fn glob_check(pattern: &[u8]) -> Result<(), Unevaluable> {
    compile(&byte_units(pattern)).map(|_| ())
}

/// One path component of an `Include` pattern.
#[derive(Debug)]
pub struct GlobComp {
    /// The items, with bytes as units (sshd's glob walks bytes).
    bytes: Vec<Option<Item>>,
    /// The items, with characters as units: the reading of a glob that follows the locale.
    chars: Vec<Option<Item>>,
    /// The component starts with a literal `.`, the only way to match a name that does.
    dot: bool,
    /// Whether the two readings can differ at all: only `?` and sets consume a unit that is
    /// part of a character.
    split: bool,
    /// The name this component is, when it has no wildcard at all.
    literal: Option<Vec<u8>>,
}

impl GlobComp {
    /// Compiles one component, or says it is [`Unevaluable`].
    pub fn new(comp: &[u8]) -> Result<GlobComp, Unevaluable> {
        let bytes = compile(&byte_units(comp))?;
        let chars = compile(&char_units(comp))?;
        let literal = bytes
            .iter()
            .map(|t| match t {
                Some(Item::Lit(c)) => u8::try_from(*c).ok(),
                _ => None,
            })
            .collect::<Option<Vec<u8>>>();
        // glob0() strips a backslash's protection, so `\.x` with a wildcard after it starts with
        // a plain `.` (and matches a dotfile) there; glob(7) says a leading period must be
        // matched by a literal period, and whether an escaped one counts is not settled.
        if literal.is_none() && comp.starts_with(b"\\.") {
            return Err(Unevaluable);
        }
        Ok(GlobComp {
            split: bytes
                .iter()
                .any(|t| matches!(t, Some(Item::One | Item::Set { .. }))),
            dot: comp.first() == Some(&b'.'),
            bytes,
            chars,
            literal,
        })
    }

    /// The name the component is, if it has no wildcard (after backslash escapes): it names
    /// itself, whether or not it exists, and the directory need not be listed.
    pub fn literal(&self) -> Option<&[u8]> {
        self.literal.as_deref()
    }

    /// Whether the component starts with a literal `.`: it can match `.` and `..` and names that
    /// begin with one. Any other component matches no name that does (glob(7): "a leading
    /// period ... must be matched explicitly").
    pub fn starts_with_dot(&self) -> bool {
        self.dot
    }

    /// Whether the name matches. sshd's glob walks bytes, and a glob that follows the locale
    /// walks characters; they answer differently for a `?` or a set against a character of
    /// several bytes. A name they disagree on is [`Unevaluable`].
    pub fn matches(&self, name: &[u8]) -> Result<bool, Unevaluable> {
        // No wildcard: the component is a name, whatever its first byte.
        if let Some(lit) = &self.literal {
            return Ok(lit == name);
        }
        if name.first() == Some(&b'.') && !self.dot {
            return Ok(false);
        }
        let by_bytes = wild(name, &self.bytes, |it, c| it.accepts(u32::from(c)));
        if !self.split || name.is_ascii() {
            return Ok(by_bytes);
        }
        let by_chars = wild(&char_units(name), &self.chars, |it, c| it.accepts(c));
        if by_bytes == by_chars {
            Ok(by_bytes)
        } else {
            Err(Unevaluable)
        }
    }
}

#[cfg(test)]
mod tests;
