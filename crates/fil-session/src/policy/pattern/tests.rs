//! Tests of the two pattern languages (`super`). Every row names the rule it checks; the
//! divergences from OpenSSH that review found each have a table, a "mutation" table (a one
//! character change that must flip the outcome, so a rule that is dropped or loosened is caught),
//! and an exhaustive comparison against a literal port of the C code on small alphabets.

use super::*;

// ---------------------------------------------------------------------------------------
// match_pattern: ssh_config(5) PATTERNS, OpenSSH's match.c.
// ---------------------------------------------------------------------------------------

#[test]
fn match_pattern_rows() {
    let rows: &[(&str, &str, bool)] = &[
        // Literal text matches itself, exactly and case-sensitively.
        ("abc", "abc", true),
        ("abc", "abd", false),
        ("abc", "ABC", false),
        ("abc", "ab", false),
        ("ab", "abc", false),
        // `*` is any run, including none, anywhere.
        ("", "*", true),
        ("abc", "*", true),
        ("abc", "a*", true),
        ("abc", "*c", true),
        ("abc", "a*c", true),
        ("ac", "a*c", true),
        ("abc", "a*b*c", true),
        ("abc", "**", true),
        ("abc", "*b", false),
        // `?` is exactly one.
        ("abc", "a?c", true),
        ("ac", "a?c", false),
        ("", "?", false),
        ("abc", "???", true),
        ("abc", "??", false),
        // No brackets: `[` and `]` are themselves (that is the glob language, not this one).
        ("abc", "a[b]c", false),
        ("a[b]c", "a[b]c", true),
        ("b", "[a-c]", false),
        ("[a-c]", "[a-c]", true),
        ("a", "[!b]", false),
        // No backslash escape: a backslash is a character.
        ("a\\b", "a\\b", true),
        ("ab", "a\\b", false),
        ("a*", "a\\*", false),
        ("a\\x", "a\\*", true),
        // No braces: `{a,b}` is literal text in a single pattern.
        ("{a,b}", "{a,b}", true),
        ("a", "{a,b}", false),
        // No `!` in a single pattern (it negates only in a list): it is a character.
        ("!x", "!x", true),
        ("x", "!x", false),
        // `?` is one BYTE: sshd's C code walks bytes. "é" is two bytes, "日" three.
        ("é", "?", false),
        ("é", "??", true),
        ("é", "???", false),
        ("é", "*", true),
        ("é", "é", true),
        ("é", "\u{e8}", false),
        ("日", "?", false),
        ("日", "???", true),
        ("日本", "??????", true),
        ("日本", "????", false),
        ("日本", "??", false),
        ("aé", "a?", false),
        ("aé", "a??", true),
        ("éa", "??a", true),
        ("éa", "?a", false),
        ("éa", "?*a", true),
        // Mutation of the byte rule: one more `?` flips it.
        ("é", "?", false),
        ("é", "??", true),
    ];
    for &(s, p, want) in rows {
        assert_eq!(match_pattern(s, p), want, "match_pattern({s:?}, {p:?})");
    }
}

fn long(n: usize) -> String {
    "x".repeat(n)
}

#[test]
fn match_pattern_list_rows() {
    use ListMatch::*;
    let rows: Vec<(String, String, ListMatch)> = vec![
        // One pattern, one hit, one miss.
        ("dev".into(), "dev".into(), Positive),
        ("dev".into(), "staff".into(), NoMatch),
        // Any of several.
        ("dev".into(), "x,dev".into(), Positive),
        ("dev".into(), "dev,x".into(), Positive),
        ("dev".into(), "x,y".into(), NoMatch),
        // Wildcards inside a list.
        ("dev".into(), "d*,x".into(), Positive),
        ("dev".into(), "x,d?v".into(), Positive),
        // A negated match beats everything, whichever side of the positive it is on.
        ("dev".into(), "d*,!dev".into(), Negated),
        ("dev".into(), "!dev,d*".into(), Negated),
        ("dev".into(), "dev,!d*".into(), Negated),
        // A negated pattern that does not match changes nothing; alone it never makes a match.
        ("dev".into(), "!x,d*".into(), Positive),
        ("dev".into(), "d*,!x".into(), Positive),
        ("dev".into(), "!x".into(), NoMatch),
        ("dev".into(), "!d*".into(), Negated),
        // Only one `!` is the negation; the next is a character.
        ("dev".into(), "!!dev".into(), NoMatch),
        ("!dev".into(), "!!dev".into(), Negated),
        // Empty lists and empty items, as the C walk reads them: an empty item matches only the
        // empty string; a trailing comma ends the list.
        ("dev".into(), "".into(), NoMatch),
        ("".into(), "".into(), NoMatch),
        ("dev".into(), "dev,".into(), Positive),
        ("dev".into(), ",dev".into(), Positive),
        ("dev".into(), ",".into(), NoMatch),
        ("".into(), ",".into(), Positive),
        ("".into(), "a,,b".into(), Positive),
        ("".into(), "dev,".into(), NoMatch),
        ("dev".into(), "!".into(), NoMatch),
        ("".into(), "!".into(), Negated),
        // Case-sensitive (`Match User`, `Match Group` call it with no lowercasing).
        ("dev".into(), "DEV".into(), NoMatch),
        // A list is split on bytes; `?` in it is one byte.
        ("é".into(), "??".into(), Positive),
        ("é".into(), "?".into(), NoMatch),
        ("é".into(), "x,??".into(), Positive),
        ("é".into(), "!??,x".into(), Negated),
        // A NUL ends a C string: the list is not read as written.
        ("dev".into(), "dev\0x".into(), Unsure),
        // THE CAP: a subpattern of 1023 bytes or more is "no match" for the whole list in sshd
        // (`char sub[1024]`); fil says Unsure so a Match block might apply. 1022 still fits.
        ("dev".into(), long(1022), NoMatch),
        ("dev".into(), long(1023), Unsure),
        ("dev".into(), long(1024), Unsure),
        ("dev".into(), long(5000), Unsure),
        // Even after an earlier positive.
        ("dev".into(), format!("dev,{}", long(1022)), Positive),
        ("dev".into(), format!("dev,{}", long(1023)), Unsure),
        ("dev".into(), format!("d*,{}", long(1023)), Unsure),
        // The `!` is not counted against the subpattern.
        ("dev".into(), format!("!{}", long(1022)), NoMatch),
        ("dev".into(), format!("!{}", long(1023)), Unsure),
        // A negated match met before the long one is sshd's answer at once.
        ("dev".into(), format!("!dev,{}", long(1023)), Negated),
        // After it, the long one is reached first.
        ("dev".into(), format!("{},!dev", long(1023)), Unsure),
        // The cap is in bytes, not characters: 511 "é" are 1022 bytes, 512 are 1024.
        ("dev".into(), "é".repeat(511), NoMatch),
        ("dev".into(), "é".repeat(512), Unsure),
        // A long list of short items is no problem.
        ("dev".into(), format!("{}dev", "a,".repeat(2500)), Positive),
        // A long `*` pattern is a long subpattern.
        ("dev".into(), "*".repeat(1023), Unsure),
        ("dev".into(), "*".repeat(1022), Positive),
    ];
    for (s, list, want) in &rows {
        let shown = if list.len() > 40 {
            format!("<{} bytes>", list.len())
        } else {
            format!("{list:?}")
        };
        assert_eq!(
            match_pattern_list(s, list),
            *want,
            "match_pattern_list({s:?}, {shown})"
        );
    }
}

/// OpenSSH's `match_pattern`, ported literally (recursive, on bytes).
fn c_match_pattern(s: &[u8], p: &[u8]) -> bool {
    let (mut s, mut p) = (s, p);
    loop {
        let Some(&pc) = p.first() else {
            return s.is_empty();
        };
        if pc == b'*' {
            while p.first() == Some(&b'*') {
                p = &p[1..];
            }
            let Some(&next) = p.first() else {
                return true;
            };
            if next != b'?' && next != b'*' {
                let mut i = 0;
                while i < s.len() {
                    if s[i] == next && c_match_pattern(&s[i + 1..], &p[1..]) {
                        return true;
                    }
                    i += 1;
                }
                return false;
            }
            let mut i = 0;
            while i < s.len() {
                if c_match_pattern(&s[i..], p) {
                    return true;
                }
                i += 1;
            }
            return false;
        }
        let Some(&sc) = s.first() else {
            return false;
        };
        if pc != b'?' && pc != sc {
            return false;
        }
        s = &s[1..];
        p = &p[1..];
    }
}

/// OpenSSH's `match_pattern_list`, ported literally, including `char sub[1024]`.
fn c_match_pattern_list(s: &[u8], pattern: &[u8]) -> i32 {
    let len = pattern.len();
    let mut got_positive = false;
    let mut i = 0;
    while i < len {
        let negated = pattern[i] == b'!';
        if negated {
            i += 1;
        }
        let mut sub = Vec::new();
        while i < len && sub.len() < 1024 - 1 && pattern[i] != b',' {
            sub.push(pattern[i]);
            i += 1;
        }
        if sub.len() >= 1024 - 1 {
            return 0;
        }
        if i < len && pattern[i] == b',' {
            i += 1;
        }
        if c_match_pattern(s, &sub) {
            if negated {
                return -1;
            }
            got_positive = true;
        }
    }
    i32::from(got_positive)
}

/// Every string of up to `max` items from `alphabet`.
fn strings(alphabet: &[&str], max: usize) -> Vec<String> {
    let mut all = vec![String::new()];
    let mut layer = all.clone();
    for _ in 0..max {
        layer = layer
            .iter()
            .flat_map(|s| alphabet.iter().map(move |c| format!("{s}{c}")))
            .collect();
        all.extend(layer.iter().cloned());
    }
    all
}

#[test]
fn match_pattern_agrees_with_the_c_walk_on_every_small_input() {
    // Rule: the linear matcher is the C function (on bytes, so "é" is two `?`), for every pattern
    // of up to 4 items over the metacharacters, a letter, a multibyte character and the
    // characters this language does NOT treat specially, against every name of up to 3 items.
    let pats = strings(&["*", "?", "a", "é", "[", "\\", "!", ","], 4);
    let names = strings(&["a", "é", "*", "["], 3);
    for p in &pats {
        for s in &names {
            assert_eq!(
                match_pattern(s, p),
                c_match_pattern(s.as_bytes(), p.as_bytes()),
                "match_pattern({s:?}, {p:?})"
            );
        }
    }
}

#[test]
fn match_pattern_list_agrees_with_the_c_walk_on_every_small_list() {
    // Rule: the list walk is the C function: negation, empty items, trailing commas, in any
    // order. (The cap is in the rows above, and in the next test.)
    let lists = strings(&["a", "é", "!", ",", "*", "?"], 5);
    let names = strings(&["a", "é"], 2);
    for l in &lists {
        for s in &names {
            let want = match c_match_pattern_list(s.as_bytes(), l.as_bytes()) {
                1 => ListMatch::Positive,
                0 => ListMatch::NoMatch,
                _ => ListMatch::Negated,
            };
            assert_eq!(
                match_pattern_list(s, l),
                want,
                "match_pattern_list({s:?}, {l:?})"
            );
        }
    }
}

#[test]
fn the_cap_agrees_with_the_c_walk_at_every_length_around_it() {
    // Rule: sshd answers 0 for the whole list once a subpattern is 1023 bytes or more, wherever
    // it is in the list and whatever came before; fil answers Unsure exactly then (and
    // otherwise as the C walk does).
    for n in 1020..=1026 {
        for (head, tail) in [
            ("", ""),
            ("dev,", ""),
            ("!x,", ""),
            ("", ",dev"),
            ("", ",!dev"),
            ("!dev,", ""),
            ("dev,", ",!dev"),
        ] {
            for bang in ["", "!"] {
                let list = format!("{head}{bang}{}{tail}", long(n));
                let c = c_match_pattern_list(b"dev", list.as_bytes());
                let got = match_pattern_list("dev", &list);
                let capped = n >= 1023 && !(head.contains("!dev"));
                if capped {
                    assert_eq!(got, ListMatch::Unsure, "n={n} head={head:?} tail={tail:?}");
                    // The C answer there is the permissive 0 (no match).
                    assert_eq!(c, 0, "n={n} head={head:?} tail={tail:?}");
                } else {
                    let want = match c {
                        1 => ListMatch::Positive,
                        0 => ListMatch::NoMatch,
                        _ => ListMatch::Negated,
                    };
                    assert_eq!(got, want, "n={n} head={head:?} bang={bang:?} tail={tail:?}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// glob(3): the Include language.
// ---------------------------------------------------------------------------------------

/// What a glob row expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V {
    /// Matches.
    Y,
    /// Does not.
    N,
    /// Unevaluable: the include is one fil could not read.
    U,
}
use V::*;

fn verdict(pattern: &str, name: &[u8]) -> V {
    match GlobComp::new(pattern.as_bytes()).and_then(|g| g.matches(name)) {
        Ok(true) => Y,
        Ok(false) => N,
        Err(Unevaluable) => U,
    }
}

fn run(rows: &[(&str, &str, V)]) {
    for &(p, name, want) in rows {
        assert_eq!(verdict(p, name.as_bytes()), want, "glob({p:?}) on {name:?}");
    }
}

#[test]
fn glob_plain_constructs_where_every_glob_agrees() {
    run(&[
        // `*`, `?` and literals.
        ("*.conf", "10-a.conf", Y),
        ("*.conf", "a.txt", N),
        ("*.conf", ".conf", N), // a leading dot is matched only by a literal dot
        ("1?-a.conf", "10-a.conf", Y),
        ("a*b*c", "aXbYc", Y),
        ("a*b*c", "aXbY", N),
        ("**", "x", Y),
        // A set, a range, a negated set.
        ("[0-9]*", "10-a.conf", Y),
        ("[0-9]*", "a", N),
        ("[abc]", "b", Y),
        ("[abc]", "d", N),
        ("[a-c]", "b", Y),
        ("[a-c]", "d", N),
        ("[a-cx-z]", "y", Y),
        ("[a-bd-e]", "e", Y),
        ("[a-bd-e]", "c", N),
        ("[a-a]", "a", Y),
        ("[1-3]", "2", Y),
        ("[!x]*", "x.conf", N),
        ("[!x]*", "y.conf", Y),
        ("[!a-c]", "d", Y),
        ("[!a-c]", "b", N),
        // The first member may be `]` (glob(7)).
        ("[]a]", "]", Y),
        ("[]a]", "a", Y),
        ("[]a]", "b", N),
        ("[!]a]", "b", Y),
        ("[!]a]", "]", N),
        // A `-` first or last is a literal.
        ("[a-]", "-", Y),
        ("[a-]", "a", Y),
        ("[a-]", "b", N),
        ("[-a]", "-", Y),
        ("[-]", "-", Y),
        ("[!-]", "-", N),
        ("[!-]", "a", Y),
        ("[!-a]", "-", N),
        ("[a-c-]", "-", Y),
        // A `[` with no `]` after it is a literal `[` (glob(7)); so is the rest.
        ("[x", "[x", Y),
        ("[x", "x", N),
        ("[", "[", Y),
        ("[]", "[]", Y),
        ("[!", "[!", Y),
        ("[!]", "[!]", Y),
        ("a[b", "a[b", Y),
        ("]", "]", Y),
        // A `[` as a set member, a `[` that opens nothing.
        ("[[]", "[", Y),
        ("[[]", "a", N),
        ("[[a]", "[", Y),
        ("[a[]", "[", Y),
        // A backslash outside a set makes the next character literal, a trailing one is itself.
        ("a\\*b", "a*b", Y),
        ("a\\*b", "axb", N),
        ("\\[x\\]", "[x]", Y),
        ("\\[x\\]", "x", N),
        ("a\\", "a\\", Y),
        ("a\\\\b", "a\\b", Y),
        ("a\\?", "a?", Y),
        ("a\\?", "ab", N),
        ("\\*", "*", Y),
        ("\\*", "x", N),
        ("\\a", "a", Y),
        // GLOB_BRACE is not passed: braces and commas are characters.
        ("{a,b}", "{a,b}", Y),
        ("{a,b}", "a", N),
        ("a,b", "a,b", Y),
        // No list negation either: a `!` is a character outside a set.
        ("!a", "!a", Y),
        ("!a", "b", N),
        // Case-sensitive.
        ("*.CONF", "a.conf", N),
        // A literal dot first matches a dotfile.
        (".*", ".x", Y),
        (".*", ".", Y),
        (".*", "..", Y),
        (".conf", ".conf", Y),
        // Any other first token does not, even one that could (glob(7)).
        ("*", ".x", N),
        ("*", ".", N),
        ("?x", ".x", N),
        ("[.]x", ".x", N),
        ("[!a]x", ".x", N),
    ]);
}

#[test]
fn glob_caret_first_in_a_set_is_unevaluable() {
    // glob(7): `!` negates. OpenBSD's glob keeps `^` as a member; glibc and musl also negate with
    // it, so `[^x]` is opposite sets. Refused, with `!` as the evaluable twin.
    run(&[
        ("[^x]", "a", U),
        ("[^x]", "x", U),
        ("[^x]*", "x", U),
        ("[^0-9]*.conf", "a.conf", U),
        ("[!x]", "a", Y),
        ("[!x]", "x", N),
        // `^` anywhere else is the same member in every glob.
        ("[a^]", "^", Y),
        ("[a^]", "b", N),
        ("[!^]", "^", N),
        ("[!^]", "a", Y),
        ("[x^y]", "^", Y),
        // Not in a set, a caret is a character.
        ("^", "^", Y),
        ("a^b", "a^b", Y),
        ("\\^", "^", Y),
    ]);
}

#[test]
fn glob_posix_classes_are_unevaluable() {
    // glob(7) "Character classes". fil does not implement them: OpenBSD's glob has the ctype
    // classes (and finds nothing for an unknown name), and other globs differ on the names and on
    // `[.x.]` and `[=x=]`. Refused, in every spelling.
    run(&[
        ("[[:digit:]]", "5", U),
        ("[[:digit:]]", "d]", U),
        ("[[:alpha:]]x", "ax", U),
        ("[![:digit:]]", "a", U),
        ("[a[:digit:]]", "5", U),
        ("[[:]", "x", U),
        ("[[.a.]]", "a", U),
        ("[[=a=]]", "a", U),
        ("[[.-.]]", "-", U),
        // Mutations: a `[` in a set not followed by `:`, `.`, `=` is a plain member.
        ("[[]", "[", Y),
        ("[[a]", "[", Y),
        ("[[x:]", ":", Y),
        ("[a[]", "[", Y),
        ("[[!]", "!", Y),
    ]);
}

#[test]
fn glob_backslash_inside_a_set_is_unevaluable() {
    // OpenBSD's glob makes the next character literal even in a set (`[\]]` is the set `]`);
    // glob(7) leaves it undefined and other globs keep the backslash as a member. Refused.
    run(&[
        ("[\\]]", "]", U),
        ("[\\]]", "\\", U),
        ("[a\\-z]", "-", U),
        ("[a\\-z]", "b", U),
        ("[\\!x]", "!", U),
        ("[\\a]", "a", U),
        ("[a\\b]", "b", U),
        ("[!\\a]", "b", U),
        ("[a-\\z]", "b", U),
        // An unclosed `[` whose `]` is escaped: OpenBSD's reading and glibc's differ in what
        // closes it.
        ("[a\\]", "[a]", U),
        ("[\\]", "[]", U),
        // Mutations: the backslash outside the set, or escaping a character that is not special
        // inside it, is the evaluable case.
        ("\\[a]", "[a]", Y),
        ("[a]\\]", "a]", Y),
        ("x\\]", "x]", Y),
        ("[ab]\\a", "ba", Y),
    ]);
}

#[test]
fn glob_ranges_and_dashes_that_are_not_clean_are_unevaluable() {
    run(&[
        // A second `-` in the middle (`a-c` then `-e`): undefined in POSIX.
        ("[a-c-e]", "d", U),
        ("[a-c-e]", "-", U),
        ("[a-c-]", "-", Y),
        // A range that starts or ends at `-`.
        ("[--a]", "b", U),
        ("[+--]", "b", U),
        ("[a--]", "b", U),
        ("[!--a]", "b", U),
        // A reversed range.
        ("[c-a]", "b", U),
        ("[9-0]", "5", U),
        ("[b-a]", "a", U),
        ("[a-a]", "a", Y),
        // Ends of different kinds: collation decides in some locales (`[A-z]` spans `[\]^_`).
        ("[a-Z]", "b", U),
        ("[A-z]", "b", U),
        ("[0-z]", "b", U),
        ("[0-a]", "b", U),
        ("[+-/]", "-", U),
        ("[a-C]", "b", U),
        ("[a-z]", "b", Y),
        ("[A-Z]", "B", Y),
        ("[0-9]", "5", Y),
        // A `-` in the middle that is not a range: `[a-]` first/last, `[a-b]`; `[a-b-]` is
        // a range then a last `-`.
        ("[x-]", "-", Y),
        ("[a-b-]", "-", Y),
    ]);
}

#[test]
fn glob_non_ascii_set_members_are_unevaluable() {
    // A byte in OpenBSD's glob, a character in one that follows the locale.
    run(&[
        ("[é]", "é", U),
        ("[é]", "a", U),
        ("[!é]", "a", U),
        ("[a-é]", "b", U),
        ("[aé]", "a", U),
        ("[日本]", "x", U),
        // Mutation: the non-ASCII character outside the set is a literal, which is the same
        // sequence of bytes in either reading.
        ("é[ab]", "éa", Y),
        ("[ab]é", "bé", Y),
        ("é*", "éa", Y),
    ]);
}

#[test]
fn glob_a_slash_in_a_set_or_after_a_backslash_is_unevaluable() {
    // Whole-pattern check: a set cannot span path components.
    for (p, want) in [
        ("/etc/ssh/[ab/c]d.conf", false),
        ("/etc/[ab/c]d.conf", false),
        ("/etc/ssh/[a/]x", false),
        ("/etc/a\\/b", false),
        ("/etc/ssh/a\\/b.conf", false),
        // Mutations: closed within the component, no `]` after, a backslash escaping a backslash.
        ("/etc/ssh/[ab]/c]d.conf", true),
        ("/etc/ssh/[ab", true),
        ("/etc/ssh/[ab/c", true),
        ("/etc/ssh/a\\\\/b.conf", true),
        ("/etc/ssh/*.conf", true),
        ("/etc/ssh/sshd_config.d/*.conf", true),
    ] {
        assert_eq!(glob_check(p.as_bytes()).is_ok(), want, "glob_check({p:?})");
    }
    // And one component alone.
    run(&[("[a/b]", "a", U), ("a\\/b", "a/b", U)]);
}

#[test]
fn glob_a_leading_dot_rule() {
    run(&[
        // glob(7): a leading period is matched only by a literal period.
        ("*", ".hidden", N),
        ("?hidden", ".hidden", N),
        ("[.]hidden", ".hidden", N),
        (".h*", ".hidden", Y),
        (".*", ".hidden", Y),
        ("*", "hidden", Y),
        // A dot that is not first is an ordinary character.
        ("a*", "a.b", Y),
        ("*.b", "a.b", Y),
        // An escaped dot first: OpenBSD's glob strips the escape and matches it; others differ.
        ("\\.*", ".x", U),
        ("\\.*", "x", U),
        ("\\.?", ".x", U),
        ("\\.[ab]", ".a", U),
        // With no wildcard it is just a name (no directory listing decides it).
        ("\\.conf", ".conf", Y),
    ]);
    let g = GlobComp::new(b"\\.conf").unwrap();
    assert_eq!(g.literal(), Some(&b".conf"[..]));
    assert!(GlobComp::new(b"*.conf").unwrap().literal().is_none());
    assert_eq!(
        GlobComp::new(b"a\\*b").unwrap().literal(),
        Some(&b"a*b"[..])
    );
    assert_eq!(GlobComp::new(b"[x").unwrap().literal(), Some(&b"[x"[..]));
    assert!(GlobComp::new(b".*").unwrap().starts_with_dot());
    assert!(!GlobComp::new(b"*").unwrap().starts_with_dot());
}

#[test]
fn glob_question_mark_and_sets_on_multibyte_names_are_unevaluable_when_the_readings_differ() {
    // sshd's glob walks bytes; one that follows the locale walks characters. Where the two give
    // different answers for a name, the name is Unevaluable; where they agree it is not.
    run(&[
        // `?` is a byte in one reading, a character in the other: they differ.
        ("?.conf", "é.conf", U),
        ("??.conf", "é.conf", U),
        ("?", "日", U),
        ("???", "日", U),
        ("?b", "日b", U),
        ("é?", "éé", U),
        ("[!x].conf", "é.conf", U),
        ("[!a]", "é", U),
        ("[!a][!a]", "é", U),
        // They agree: no `?` or set consumes part of a character.
        ("*.conf", "é.conf", Y),
        ("é*", "éa", Y),
        ("é.conf", "é.conf", Y),
        ("*?", "日", Y), // at least one unit either way
        ("?*", "日", Y),
        ("*é*", "xéx", Y),
        // They agree because both fail or both succeed.
        ("é?", "éa", Y),
        ("?a", "éa", U), // bytes: `?` takes 0xC3, then 0xA9 is not `a`; chars: matches
        ("[a-z]*", "éa", N),
        ("[!a]*", "éa", Y),
        ("[a-z]?", "éa", N),
        ("a?", "éa", N),
        // Mutation: the same pattern on an ASCII name needs no second reading.
        ("?.conf", "x.conf", Y),
        ("??.conf", "xy.conf", Y),
        ("?", "a", Y),
        ("???", "abc", Y),
    ]);
    // Names that are not UTF-8 have a reading of their own: each bad byte is a unit.
    assert_eq!(verdict("?", b"\xff"), Y);
    assert_eq!(verdict("??", b"\xff\xfe"), Y);
    assert_eq!(verdict("?", b"\xff\xfe"), N);
    assert_eq!(verdict("*", b"\xff\xfe"), Y);
    assert_eq!(verdict("?\u{e9}", b"\xffb"), N);
    assert_eq!(verdict("?", b"\xe6\x97"), N);
    assert_eq!(verdict("??", b"\xe6\x97"), Y);
    // A name that is valid UTF-8 for `?` but not otherwise: bytes say 3, chars say 1 + invalid.
    assert_eq!(verdict("*", b"\xc3"), Y);
}

#[test]
fn match_and_glob_are_different_languages() {
    // Rule: the same text means different things to `Match` criteria and to `Include`. A table of
    // text, name, what match_pattern says, what the glob says.
    for (text, name, as_match, as_glob) in [
        // Brackets: literal for Match, a set for the glob.
        ("[ab]", "a", false, Y),
        ("[ab]", "[ab]", true, N),
        // Backslash: literal for Match, an escape for the glob.
        ("a\\*", "a\\x", true, N),
        ("a\\*", "a*", false, Y),
        // `?` on a multibyte name: a byte for Match; for the glob the readings disagree.
        ("?", "é", false, U),
        // A leading dot is nothing special to Match (it has no filenames).
        ("*", ".x", true, N),
        // Both: `*` and `?` and literals.
        ("a*", "abc", true, Y),
        ("a?c", "abc", true, Y),
    ] {
        assert_eq!(
            match_pattern(name, text),
            as_match,
            "match {text:?} {name:?}"
        );
        assert_eq!(
            verdict(text, name.as_bytes()),
            as_glob,
            "glob {text:?} {name:?}"
        );
    }
    // `!` negation and commas exist only in a Match list: in a glob they are characters.
    assert_eq!(match_pattern_list("a", "!b,a"), ListMatch::Positive);
    assert_eq!(verdict("!b,a", b"a"), N);
    assert_eq!(verdict("!b,a", b"!b,a"), Y);
}

#[test]
fn glob_mutations_flip_the_outcome() {
    // Each row: a pattern that is evaluable and one character changed so that it is not (or
    // that matches differently), with the name checked on both. A rule that is dropped or
    // loosened fails its row.
    for (rule, good, bad, name, good_v, bad_v) in [
        ("`!` negates, `^` is refused", "[!x]", "[^x]", "a", Y, U),
        ("a class is refused", "[xd:]", "[[:d]", "d", Y, U),
        ("collating symbol refused", "[x.]", "[[.]", "x", Y, U),
        ("equivalence class refused", "[x=]", "[[=]", "=", Y, U),
        ("backslash in a set refused", "[ab]", "[a\\b]", "b", Y, U),
        (
            "backslash outside is an escape",
            "\\[a]",
            "[\\a]",
            "[a]",
            Y,
            U,
        ),
        ("non-ASCII member refused", "[ab]", "[aé]", "a", Y, U),
        ("reversed range refused", "[a-c]", "[c-a]", "b", Y, U),
        ("mixed range refused", "[a-z]", "[a-Z]", "b", Y, U),
        ("middle dash refused", "[a-c]", "[a-c-e]", "b", Y, U),
        ("range ending at dash refused", "[a-]", "[a--]", "a", Y, U),
        ("escaped leading dot refused", ".*", "\\.*", ".x", Y, U),
        ("`?` on a multibyte name", "x*", "x?", "xé", Y, U),
        ("range upper bound counts", "[0-9]", "[0-8]", "9", Y, N),
        ("range lower bound counts", "[1-9]", "[2-9]", "1", Y, N),
        ("negation flips", "[!a]", "[a]", "a", N, Y),
        ("first `]` is a member", "[]a]", "[a]", "]", Y, N),
        ("unclosed `[` is literal", "[a", "[a]", "[a", Y, N),
        ("dotfiles need a literal dot", ".*", "*", ".x", Y, N),
        ("trailing escape is itself", "a\\", "a", "a\\", Y, N),
    ] {
        assert_eq!(
            verdict(good, name.as_bytes()),
            good_v,
            "{rule}: {good:?} on {name:?}"
        );
        assert_eq!(
            verdict(bad, name.as_bytes()),
            bad_v,
            "{rule}: {bad:?} on {name:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The glob, against a literal port of OpenBSD's glob0() and match().
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tk {
    All,
    One,
    Set,
    Not,
    Rng,
    End,
    Ch(u32),
}

/// OpenBSD `glob()`'s protect pass and `glob0()`, ported literally from the C.
fn c_glob0(p: &[u32]) -> Vec<Tk> {
    let b = u32::from(b'\\');
    let mut q: Vec<(u32, bool)> = Vec::new();
    let mut i = 0;
    while i < p.len() {
        if p[i] == b {
            i += 1;
            match p.get(i) {
                Some(&c) => {
                    q.push((c, true));
                    i += 1;
                }
                None => q.push((b, true)),
            }
        } else {
            q.push((p[i], false));
            i += 1;
        }
    }
    let un = |k: usize, ch: char| q.get(k).is_some_and(|&(c, pr)| !pr && c == u32::from(ch));
    let mut out = Vec::new();
    let mut i = 0;
    while i < q.len() {
        let (c, pr) = q[i];
        i += 1;
        if !pr && c == u32::from('[') {
            let not = un(i, '!');
            let mut j = i + usize::from(not);
            if j >= q.len() || !q[j + 1..].iter().any(|&(c, pr)| !pr && c == u32::from(']')) {
                out.push(Tk::Ch(c));
                continue;
            }
            out.push(Tk::Set);
            if not {
                out.push(Tk::Not);
            }
            let mut c = q[j].0;
            j += 1;
            loop {
                out.push(Tk::Ch(c));
                if un(j, '-') && q.get(j + 1).is_some_and(|_| !un(j + 1, ']')) {
                    out.push(Tk::Rng);
                    out.push(Tk::Ch(q[j + 1].0));
                    j += 2;
                }
                let (nc, npr) = q[j];
                j += 1;
                if !npr && nc == u32::from(']') {
                    break;
                }
                c = nc;
            }
            out.push(Tk::End);
            i = j;
        } else if !pr && c == u32::from('?') {
            out.push(Tk::One);
        } else if !pr && c == u32::from('*') {
            if out.last() != Some(&Tk::All) {
                out.push(Tk::All);
            }
        } else {
            out.push(Tk::Ch(c));
        }
    }
    out
}

/// OpenBSD `match()`, ported literally (recursive).
fn c_glob_match(name: &[u32], pat: &[Tk]) -> bool {
    let (mut ni, mut pi) = (0, 0);
    while pi < pat.len() {
        let t = pat[pi];
        pi += 1;
        match t {
            Tk::All => {
                if pi == pat.len() {
                    return true;
                }
                loop {
                    if c_glob_match(&name[ni..], &pat[pi..]) {
                        return true;
                    }
                    if ni >= name.len() {
                        return false;
                    }
                    ni += 1;
                }
            }
            Tk::One => {
                if ni >= name.len() {
                    return false;
                }
                ni += 1;
            }
            Tk::Set => {
                if ni >= name.len() {
                    return false;
                }
                let k = name[ni];
                ni += 1;
                let negate = pat[pi] == Tk::Not;
                if negate {
                    pi += 1;
                }
                let mut ok = false;
                loop {
                    let c = pat[pi];
                    pi += 1;
                    if c == Tk::End {
                        break;
                    }
                    let Tk::Ch(c) = c else { unreachable!() };
                    if pat[pi] == Tk::Rng {
                        let Tk::Ch(hi) = pat[pi + 1] else {
                            unreachable!()
                        };
                        if c <= k && k <= hi {
                            ok = true;
                        }
                        pi += 2;
                    } else if c == k {
                        ok = true;
                    }
                }
                if ok == negate {
                    return false;
                }
            }
            Tk::Ch(c) => {
                if ni >= name.len() || name[ni] != c {
                    return false;
                }
                ni += 1;
            }
            Tk::Not | Tk::Rng | Tk::End => unreachable!(),
        }
    }
    ni == name.len()
}

fn units(s: &str, chars: bool) -> Vec<u32> {
    if chars {
        s.chars().map(u32::from).collect()
    } else {
        s.bytes().map(u32::from).collect()
    }
}

#[test]
fn glob_agrees_with_openbsd_glob_on_every_small_pattern() {
    // Rule: for every pattern fil accepts, over the metacharacters (a bracket's `!`, `-`, `]`,
    // a backslash, a caret), a letter and a digit, the answer is OpenBSD's `glob0()` + `match()`
    // on bytes, for every name of up to 3 items over letters, digits and the same punctuation.
    // The patterns fil refuses are counted so a rule that refuses everything is caught.
    let pats = strings(&["*", "?", "a", "1", "[", "]", "!", "-", "\\", "^"], 5);
    let names = strings(&["a", "b", "1", "-", "!", "]", "["], 3);
    let (mut accepted, mut refused) = (0usize, 0usize);
    for p in &pats {
        let Ok(g) = GlobComp::new(p.as_bytes()) else {
            refused += 1;
            continue;
        };
        accepted += 1;
        let oracle = c_glob0(&units(p, false));
        for s in &names {
            assert_eq!(
                g.matches(s.as_bytes()),
                Ok(c_glob_match(&units(s, false), &oracle)),
                "glob({p:?}) on {s:?}"
            );
        }
    }
    assert!(accepted > 20_000, "accepted {accepted}");
    assert!(refused > 1_000, "refused {refused}");
}

#[test]
fn glob_on_multibyte_names_is_unevaluable_exactly_when_bytes_and_characters_differ() {
    // Rule: with multibyte characters in both pattern and name, a pattern fil accepts answers
    // Ok(v) when OpenBSD's walk gives v on bytes AND on characters, and Unevaluable when they
    // differ; never a guess.
    let pats = strings(&["*", "?", "a", "é", "[", "]", "!", "日"], 4);
    let names = strings(&["a", "é", "日", "!"], 3);
    let (mut disagree, mut agree) = (0usize, 0usize);
    for p in &pats {
        let Ok(g) = GlobComp::new(p.as_bytes()) else {
            continue;
        };
        let by_bytes = c_glob0(&units(p, false));
        let by_chars = c_glob0(&units(p, true));
        for s in &names {
            let b = c_glob_match(&units(s, false), &by_bytes);
            let c = c_glob_match(&units(s, true), &by_chars);
            let want = if b == c { Ok(b) } else { Err(Unevaluable) };
            assert_eq!(g.matches(s.as_bytes()), want, "glob({p:?}) on {s:?}");
            if b == c {
                agree += 1;
            } else {
                disagree += 1;
            }
        }
    }
    assert!(disagree > 1_000, "disagree {disagree}");
    assert!(agree > 10_000, "agree {agree}");
}

#[test]
fn many_stars_cost_no_more_than_the_length_of_the_input() {
    // The fuzzer's finding: a recursive match of this many stars against a name that does not
    // match never finished (it is exponential in the stars).
    let pattern = format!("{}b", "a*".repeat(40));
    let name = "a".repeat(200);
    assert!(!match_pattern(&name, &pattern));
    assert_eq!(verdict(&pattern, name.as_bytes()), N);
    assert!(match_pattern(&format!("{name}b"), &pattern));
    assert_eq!(verdict(&pattern, format!("{name}b").as_bytes()), Y);
    let stars = "*".repeat(100);
    assert!(!match_pattern("conf", &format!("{stars}x")));
    assert_eq!(verdict(&format!("{stars}x"), b"conf"), N);
    assert!(match_pattern("", &stars));
    // With `?` and sets mixed in, on a name of several bytes of a character.
    let mixed = format!("{}b", "?*[a]*".repeat(30));
    assert_eq!(verdict(&mixed, "é".repeat(100).as_bytes()), N);
}

#[test]
fn a_run_of_open_brackets_costs_time_linear_in_its_length() {
    // The review's probe: each `[` used to look to the end of the pattern for a `]` (and again
    // for an escaped one), so a run of them was quadratic (5,000 took 5.7 s in a debug build,
    // 20,000 over a minute). Now one pass finds where the closing brackets are.
    use std::time::{Duration, Instant};
    let t = Instant::now();
    for n in [5_000, 20_000] {
        // No `]` at all: every `[` is a literal.
        let p = "[".repeat(n);
        assert!(glob_check(p.as_bytes()).is_ok());
        let g = GlobComp::new(p.as_bytes()).unwrap();
        assert_eq!(g.literal(), Some(p.as_bytes()));
        // A `]` only at the end: the first `[` opens a set that holds the rest.
        let p = format!("{}]", "[".repeat(n));
        assert!(glob_check(p.as_bytes()).is_ok());
        let _ = GlobComp::new(p.as_bytes());
        // An escaped `]` at the end: refused, once.
        let p = format!("{}\\]", "[".repeat(n));
        assert_eq!(glob_check(p.as_bytes()), Err(Unevaluable));
        // Every `[` followed by `!`, and by a character that makes no range.
        let p = "[!a".repeat(n / 3);
        assert!(GlobComp::new(p.as_bytes()).is_ok());
        let p = "[a-".repeat(n / 3);
        assert!(GlobComp::new(p.as_bytes()).is_ok());
    }
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t.elapsed()
    );
}
