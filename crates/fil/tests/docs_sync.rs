//! The user guide (`docs/user/`), the man page and the completions against the code
//! (`docs/m4/plan.md`, DOC): the committed `fil.1` is what the generator writes, every
//! subcommand and flag of `fil --help` is in the guide, every TOML example parses with the real
//! parser, the daemon and policy pages name every key their parsers accept (and the keys they
//! name are accepted), and no host or address but a documentation example appears.
//!
//! `config.toml`'s page (`docs/user/config.md`, with `docs/user/examples/`) belongs to M4's CFG:
//! its keys are checked against `config::keys` by a unit test in `config.rs`, and this file only
//! parses its TOML examples and applies the link and host rules to it.

use std::path::{Path, PathBuf};

use clap::CommandFactory;
use fil_session::policy::file::{self, Keys};

#[path = "../examples/support/docgen.rs"]
mod docgen;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file under `dir` with the extension, sorted.
fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path, ext));
        } else if path.extension().is_some_and(|e| e == ext) {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn pages() -> Vec<(PathBuf, String)> {
    let found: Vec<_> = files(&root().join("docs/user"), "md")
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect();
    assert!(!found.is_empty(), "docs/user has no pages");
    found
}

fn page(name: &str) -> String {
    std::fs::read_to_string(root().join("docs/user").join(name)).unwrap()
}

fn guide() -> String {
    pages()
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n")
}

fn name_of(path: &Path) -> String {
    let relative = path.strip_prefix(root().join("docs/user")).unwrap_or(path);
    relative.display().to_string()
}

// ---- the man page and the completions

#[test]
fn the_committed_man_page_is_what_the_generator_writes() {
    let committed = std::fs::read(root().join("docs/man/fil.1")).expect("docs/man/fil.1");
    assert!(
        committed == docgen::man_page(),
        "docs/man/fil.1 is out of date with the command line: run `just docs-man` and commit it"
    );
}

#[test]
fn every_shell_gets_a_completion_script() {
    for (shell, path) in docgen::COMPLETIONS {
        let script = String::from_utf8(docgen::completion(shell)).unwrap();
        assert!(script.contains("fil"), "{shell}: a script for fil");
        assert!(
            script.contains("transport"),
            "{shell}: the script knows the flags"
        );
        assert!(path.contains("fil"), "{shell}: {path}");
        for hidden in ["--install-finish", "--install-gc", "--daemon"] {
            assert!(
                !script.contains(hidden),
                "{shell}: offers the hidden {hidden}"
            );
        }
    }
}

// ---- the command line in the guide

/// Whether `token` is in `text` as a word of its own: not part of a longer flag or name.
fn mentions(text: &str, token: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    text.match_indices(token).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + token.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// Every subcommand (as `fil SUB`) and every flag that `fil --help` and `fil SUB --help` list.
fn command_line() -> Vec<String> {
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
        for arg in cmd.get_arguments().filter(|a| !a.is_hide_set()) {
            if let Some(short) = arg.get_short() {
                out.push(format!("-{short}"));
            }
            if let Some(long) = arg.get_long() {
                out.push(format!("--{long}"));
            }
        }
        for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
            let path = format!("{path} {}", sub.get_name());
            out.push(path.clone());
            walk(sub, &path, out);
        }
    }
    let mut cmd = fil::cli::Cli::command();
    cmd.build();
    let mut out = Vec::new();
    walk(&cmd, "fil", &mut out);
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_subcommand_and_flag_is_in_the_guide() {
    let guide = guide();
    let missing: Vec<_> = command_line()
        .into_iter()
        .filter(|t| !mentions(&guide, t))
        .collect();
    assert!(
        missing.is_empty(),
        "in `fil --help` but not in docs/user: {missing:?}"
    );
}

#[test]
fn the_command_line_has_what_the_test_looks_for() {
    // A guard on the walk above: it must see the commands and flags the guide is about.
    let all = command_line();
    for t in [
        "fil ls",
        "fil install",
        "-L",
        "--transport",
        "--rm",
        "--check",
        "--config",
    ] {
        assert!(all.iter().any(|a| a == t), "{t} in {all:?}");
    }
}

#[test]
fn there_is_no_prediction_in_0_1_0() {
    // N9: 0.1.0 has no `predict` key and no `--predict` flag, and the guide has no page on it.
    for (path, text) in pages() {
        // config.md says, in a section of its own, that the key is not there in 0.1.0.
        let says_so = name_of(&path) == "config.md";
        assert!(
            !text.contains("--predict") && (says_so || !text.contains("`predict`")),
            "{}: names a predict key or flag",
            name_of(&path)
        );
        for block in toml_blocks(&text) {
            assert!(
                !block.contains("predict"),
                "{}: predict in TOML",
                name_of(&path)
            );
        }
    }
    assert!(!root().join("docs/user/prediction.md").exists());
}

// ---- the TOML examples

/// The text of every fenced block marked `toml`.
fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```toml") => current = Some(String::new()),
            (Some(_), "```") => blocks.extend(current.take()),
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    assert!(current.is_none(), "an unclosed ```toml block");
    blocks
}

#[test]
fn every_toml_example_parses_with_the_real_parser() {
    let mut checked = std::collections::BTreeMap::<String, usize>::new();
    for (path, text) in pages() {
        let name = name_of(&path);
        for (i, block) in toml_blocks(&text).iter().enumerate() {
            let at = format!("{name}, TOML block {}", i + 1);
            match name.as_str() {
                "config.md" => {
                    fil::config::Config::parse(block, &at).unwrap_or_else(|e| panic!("{at}: {e}"));
                }
                "daemon.md" => {
                    fil_daemon::DaemonConfig::parse(block).unwrap_or_else(|e| panic!("{at}: {e}"));
                }
                "policy.md" => {
                    assert!(
                        file::parse(block).is_some(),
                        "{at}: policy.toml does not parse"
                    );
                }
                other => panic!("{at}: no parser for the TOML of {other}; use ```text"),
            }
            *checked.entry(name.clone()).or_default() += 1;
        }
    }
    for needed in ["daemon.md", "policy.md"] {
        assert!(checked.contains_key(needed), "{needed} has no TOML example");
    }
}

// ---- the keys of daemon.toml and policy.toml

/// The keys serde lists in the error for an unknown key in `table` of `daemon.toml`.
fn daemon_keys(table: Option<&str>) -> Vec<String> {
    let text = match table {
        None => "zz_not_a_key = 1".to_string(),
        Some(t) => format!("[{t}]\nzz_not_a_key = 1"),
    };
    let err = fil_daemon::DaemonConfig::parse(&text).unwrap_err();
    let list = err
        .split("expected one of ")
        .nth(1)
        .or_else(|| err.split("expected ").nth(1))
        .unwrap_or_else(|| panic!("no list of keys in: {err}"));
    list.lines()
        .next()
        .unwrap()
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The top-level keys that are tables: those that accept `[key]` with keys of their own.
fn daemon_tables() -> Vec<String> {
    daemon_keys(None)
        .into_iter()
        .filter(|k| {
            fil_daemon::DaemonConfig::parse(&format!("[{k}]\nzz_not_a_key = 1"))
                .is_err_and(|e| e.contains("unknown field `zz_not_a_key`"))
        })
        .collect()
}

#[test]
fn daemon_md_names_every_key_the_daemon_accepts() {
    let page = page("daemon.md");
    let tables = daemon_tables();
    assert!(tables.len() >= 2, "found the tables: {tables:?}");
    let mut keys = daemon_keys(None);
    for t in &tables {
        keys.retain(|k| k != t);
        keys.extend(daemon_keys(Some(t)));
    }
    assert!(keys.len() >= 13, "found the keys: {keys:?}");
    for key in &keys {
        assert!(
            page.contains(&format!("`{key}`")),
            "daemon.md does not name `{key}`, which daemon.toml accepts"
        );
    }
    for t in &tables {
        assert!(
            page.contains(&format!("[{t}]")),
            "daemon.md: the [{t}] table"
        );
    }
}

#[test]
fn daemon_md_names_only_keys_the_daemon_accepts() {
    // The key column of every table in the page: `name` in the first cell.
    let accepted: Vec<String> = {
        let mut v = daemon_keys(None);
        for t in daemon_tables() {
            v.extend(daemon_keys(Some(&t)));
        }
        v
    };
    for line in page("daemon.md").lines() {
        let Some(cell) = line.strip_prefix("| `") else {
            continue;
        };
        let key = cell.split('`').next().unwrap();
        assert!(
            accepted.iter().any(|k| k == key),
            "daemon.md documents `{key}`, which daemon.toml does not accept"
        );
    }
}

#[test]
fn policy_md_names_every_key_the_policy_file_accepts() {
    // The fields of `Keys`, from its Debug form: a key added to the parser has no sample below
    // until someone writes one, and this test fails until then.
    let debug = format!("{:?}", Keys::default());
    let fields: std::collections::BTreeSet<&str> = debug
        .split(['{', '}', ','])
        .skip(1)
        .filter_map(|f| f.split(':').next())
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    let samples = [
        ("local", "local = false"),
        ("remote", "remote = false"),
        ("agent", "agent = false"),
        ("direct", "direct = false"),
        ("permit_open", "permit_open = [\"devbox:22\"]"),
        ("permit_listen", "permit_listen = [\"8080\"]"),
        ("gateway_ports", "gateway_ports = true"),
    ];
    let sampled: std::collections::BTreeSet<&str> = samples.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        sampled, fields,
        "a sample for every key of policy.toml, and no other"
    );
    let page = page("policy.md");
    for (key, sample) in samples {
        assert!(
            file::parse(sample).is_some(),
            "policy.toml accepts {sample}"
        );
        assert!(
            page.contains(&format!("| `{key}` |")),
            "policy.md has no table row for `{key}`"
        );
    }
    assert!(
        file::parse("zz_not_a_key = 1").is_none(),
        "unknown keys are refused"
    );
    // The keys' table is in the second layer's section; the first layer's table names sshd's.
    let layer2 = page
        .split_once("## Layer 2")
        .expect("the Layer 2 section")
        .1;
    for line in layer2.lines() {
        if let Some(cell) = line.strip_prefix("| `") {
            let key = cell.split('`').next().unwrap();
            assert!(
                samples.iter().any(|(k, _)| *k == key),
                "policy.md documents `{key}`, which policy.toml does not accept"
            );
        }
    }
}

// ---- links

#[test]
fn relative_links_in_the_guide_resolve() {
    for (path, text) in pages() {
        for (_, rest) in text.match_indices("](").map(|(i, _)| ((), &text[i + 2..])) {
            let target = rest.split(')').next().unwrap();
            if target.starts_with("http") || target.starts_with('#') || target.is_empty() {
                continue;
            }
            let file = target.split('#').next().unwrap();
            let resolved = path.parent().unwrap().join(file);
            assert!(
                resolved.exists(),
                "{}: the link ({target}) leads nowhere",
                name_of(&path)
            );
        }
    }
}

// ---- hosts and addresses

const DOC_TLDS: [&str; 3] = ["invalid", "test", "example"];
const DOC_DOMAINS: [&str; 3] = ["example.net", "example.com", "example.org"];
/// Words after an `@` that are placeholders or the one example host.
const DOC_HOSTS: [&str; 4] = ["devbox", "name", "host", "HOST"];
/// Names that look like hosts and are not: the project's own public sites (registry, forge, the
/// two specifications the changelog cites).
const NOT_HOSTS: [&str; 4] = [
    "crates.io",
    "github.com",
    "keepachangelog.com",
    "semver.org",
];
/// Last labels that make a dotted word a domain name, not a file name.
const TLDS: &[&str] = &[
    "com", "net", "org", "io", "dev", "lan", "lab", "home", "local", "internal", "edu", "gov",
    "me", "us", "uk", "de", "co", "app", "xyz", "info", "cloud", "corp", "test", "invalid", "ai",
    "ca", "au", "fr", "nl", "jp", "cn", "ru", "br", "eu", "tv", "cc", "ch", "se", "es", "pl",
    "biz", "pro", "tech", "site", "online", "mil", "int", "arpa", "ws", "cz", "be", "dk", "fi",
    "kr", "mx", "nz", "za", "sg", "hk", "tw",
];

fn ipv4_is_documentation(parts: [u8; 4]) -> bool {
    matches!(
        parts,
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] | [127, ..] | [0, 0, 0, 0]
    )
}

/// Words after a command in code (`ssh devbox`, `fil ls devbox`) that stand for a host.
const PLACEHOLDERS: [&str; 6] = ["devbox", "HOST", "FILE", "SESSION", "NAME", "COMMAND"];
const SUBCOMMANDS: [&str; 9] = [
    "ls", "kill", "detach", "forward", "ports", "server", "install", "config", "show",
];

/// The commands in a Markdown text: shell blocks line by line, and inline `spans`. Blocks of
/// output (`text`) and of TOML are not commands.
fn code_segments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut shell = false;
    for line in text.lines() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            fenced = !fenced;
            shell = fenced && matches!(info.trim(), "sh" | "bash" | "");
        } else if fenced {
            if shell {
                out.push(line.to_string());
            }
        } else {
            out.extend(line.split('`').skip(1).step_by(2).map(str::to_string));
        }
    }
    out
}

/// Host names without a dot, which the scan of dotted names cannot see: the first word after
/// `ssh` or `fil` in code that is not a flag, a subcommand, a number or a placeholder.
fn single_label_hosts(text: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for segment in code_segments(text) {
        // A comment (`# ...`) is prose.
        let code = segment.split(" #").next().unwrap_or("");
        if code.trim_start().starts_with('#') {
            continue;
        }
        let words: Vec<&str> = code.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            let starts = i == 0 || matches!(words[i - 1], "$" | "|" | "&&" | "sudo");
            if !starts || !matches!(*word, "ssh" | "fil") {
                continue;
            }
            for next in &words[i + 1..] {
                let next = next.trim_end_matches([',', '.', ';', ')', '\'', '"']);
                if next.starts_with('-') || !next.starts_with(|c: char| c.is_ascii_alphabetic()) {
                    continue;
                }
                if SUBCOMMANDS.contains(&next) {
                    continue;
                }
                if !PLACEHOLDERS.contains(&next) && !next.contains(['@', '/', '=']) {
                    bad.push(format!("{word} {next}"));
                }
                break;
            }
        }
    }
    bad
}

/// What `text` names that is not a documentation example: host names, IPv4 and IPv6 addresses.
fn foreign_hosts(text: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let tokens =
        text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@')));
    for raw in tokens {
        let token = raw.trim_matches(|c| c == '.' || c == '-' || c == '_');
        if let Some((_, host)) = token.rsplit_once('@') {
            let host = host.trim_end_matches('.');
            let ok = DOC_HOSTS.contains(&host)
                || DOC_DOMAINS
                    .iter()
                    .any(|d| host == *d || host.ends_with(&format!(".{d}")));
            if !host.is_empty() && !ok {
                bad.push(token.to_string());
            }
            continue;
        }
        if !token.contains('.') || NOT_HOSTS.contains(&token) {
            continue;
        }
        let labels: Vec<&str> = token.split('.').collect();
        if labels.len() == 4
            && labels
                .iter()
                .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
        {
            let parsed: Option<Vec<u8>> = labels.iter().map(|l| l.parse().ok()).collect();
            match parsed {
                Some(p) if ipv4_is_documentation([p[0], p[1], p[2], p[3]]) => {}
                _ => bad.push(token.to_string()),
            }
            continue;
        }
        let last = labels.last().unwrap().to_ascii_lowercase();
        if !TLDS.contains(&last.as_str()) {
            continue;
        }
        let documentation = DOC_DOMAINS
            .iter()
            .any(|d| token == *d || token.ends_with(&format!(".{d}")))
            || DOC_TLDS.contains(&last.as_str());
        if !documentation {
            bad.push(token.to_string());
        }
    }
    // IPv6: runs of hex digits and colons.
    for run in text.split(|c: char| !(c.is_ascii_hexdigit() || c == ':')) {
        // `::` somewhere, or too many groups to be a port spec like `:8080:`.
        let colons = run.matches(':').count();
        if !(run.contains("::") || colons >= 4) || !run.bytes().any(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let run = run.to_ascii_lowercase();
        if !(run.starts_with("2001:db8") || run == "::1") {
            bad.push(run);
        }
    }
    bad
}

#[test]
fn the_host_scan_knows_a_documentation_host_from_a_real_one() {
    for ok in [
        "ssh devbox",
        "me@devbox.example.net",
        "192.0.2.7 and 198.51.100.1 and 203.0.113.9",
        "[2001:db8::1]:443 and [::1] and ::",
        "127.0.0.1:9000, 0.0.0.0, config.toml, daemon.log, fil.1, crates.io, 1.12, 0.1.0",
        "fil.invalid, since 09:12",
    ] {
        assert_eq!(foreign_hosts(ok), Vec::<String>::new(), "{ok}");
    }
    for bad in [
        "ssh me@work.corp",
        "pi.local",
        "web.example.net.evil.com",
        "10.0.0.5",
        "192.168.1.1",
        "[2606:4700::1111]",
        "git@github.com",
        "www.google.com",
    ] {
        assert!(!foreign_hosts(bad).is_empty(), "{bad} should be flagged");
    }
}

#[test]
fn the_scan_sees_a_host_without_a_dot() {
    assert!(single_label_hosts("run `ssh myserver` now").contains(&"ssh myserver".to_string()));
    assert!(
        !single_label_hosts("```sh\nfil ls devbox\nfil -- ls\nfil forward -L 5173\n```")
            .iter()
            .any(|_| true)
    );
    assert!(single_label_hosts("`fil build-box -L 80`").len() == 1);
}

#[test]
fn only_documentation_hosts_appear_in_the_guide() {
    // The guide, and the two files that ship beside it in the archive.
    let mut texts = pages();
    for name in ["README.md", "CHANGELOG.md"] {
        let path = root().join(name);
        texts.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
    }
    for (path, text) in texts {
        let mut bad = foreign_hosts(&text);
        bad.extend(single_label_hosts(&text));
        assert!(
            bad.is_empty(),
            "{}: names hosts or addresses that are not documentation examples: {bad:?}",
            name_of(&path)
        );
    }
    for example in files(&root().join("docs/user/examples"), "toml") {
        let bad = foreign_hosts(&std::fs::read_to_string(&example).unwrap());
        assert!(bad.is_empty(), "{}: {bad:?}", example.display());
    }
}
