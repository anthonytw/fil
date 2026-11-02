//! Records real programs on a PTY into fixtures (`just record`; dev tool).
//!
//! Each scenario runs in a scrubbed environment: an empty `PATH`-only env, a throwaway
//! `HOME`, a neutral `PS1`, a working directory with invented files. The raw output goes to
//! `tests/fixtures/<name>.raw` with a `.meta` file. Inspect the result for personal data
//! before committing: this tool does not guarantee anything about what a program prints.
//!
//! Usage: `record [scenario ...]` (default: all). Scenarios: shell_ls, vim_edit, less_view,
//! top_view, progress_real.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

struct Scenario {
    name: &'static str,
    description: &'static str,
    cols: u16,
    rows: u16,
    argv: Vec<&'static str>,
    env: Vec<(&'static str, &'static str)>,
    /// (delay before sending, bytes)
    input: Vec<(u64, &'static [u8])>,
    /// Upper bound on how long to wait for the program to exit after the last input.
    grace_ms: u64,
}

const PY: &str = "\
import sys


def fizzbuzz(n):
    \"\"\"Return the FizzBuzz string for n.\"\"\"
    if n % 15 == 0:
        return 'FizzBuzz'
    elif n % 3 == 0:
        return 'Fizz'
    elif n % 5 == 0:
        return 'Buzz'
    return str(n)


class Counter:
    def __init__(self, limit=20):
        self.limit = limit  # TODO: make configurable

    def run(self):
        for i in range(1, self.limit + 1):
            print(fizzbuzz(i))


if __name__ == '__main__':
    Counter(int(sys.argv[1]) if len(sys.argv) > 1 else 20).run()
";

const VIMRC: &str = "set nocompatible\nsyntax on\nset number\nset background=dark\nset t_Co=256\n\
set noswapfile\nset viminfo=\nset shortmess+=I\nset ruler\nset laststatus=2\n";

/// Invented working directory contents.
fn populate(proj: &Path) {
    let w = |p: &str, c: &str| {
        let f = proj.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
    };
    w("README.md", "# demo\n\nA small demo project.\n");
    w("notes.txt", &long_text());
    w("fizz.py", PY);
    w("Makefile", "all:\n\t@echo build\n");
    w("src/main.rs", "fn main() {}\n");
    w("src/lib.rs", "pub fn f() {}\n");
    w("docs/guide.md", "guide\n");
    w("data.tar.gz", "not really an archive\n");
    w("photo.png", "png\n");
    w("run.sh", "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = proj.join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::os::unix::fs::symlink("README.md", proj.join("latest"));
    }
}

fn long_text() -> String {
    let mut s = String::new();
    for i in 1..=80 {
        s.push_str(&format!(
            "{i:02}  Line {i} of the notes file: the pattern marker appears on every seventh line.{}\n",
            if i % 7 == 0 { " MARKER" } else { "" }
        ));
    }
    s
}

fn scenarios() -> Vec<Scenario> {
    let sh_env = vec![
        ("TERM", "xterm-256color"),
        ("CLICOLOR_FORCE", "1"),
        ("BASH_SILENCE_DEPRECATION_WARNING", "1"),
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "en_US.UTF-8"),
    ];
    vec![
        Scenario {
            name: "shell_ls",
            description: "bash with a neutral prompt: colored ls (dirs, executable, symlink), a prompt, SGR from printf",
            cols: 80,
            rows: 24,
            argv: vec!["/bin/bash", "--noprofile", "--norc", "-i"],
            env: sh_env.clone(),
            input: vec![
                (600, b"PS1='$ '\r"),
                (300, b"ls -G -F\r"),
                (300, b"printf '\\033[1;31mred\\033[0m \\033[4mul\\033[0m \\033[38;5;208morange\\033[0m\\n'\r"),
                (300, b"ls -G -F src docs README.md\r"),
                (300, b"echo done\r"),
                (300, b"exit\r"),
            ],
            grace_ms: 3000,
        },
        Scenario {
            name: "vim_edit",
            description: "vim with syntax highlighting: open a python file, insert a line, search, :set cursorline, quit",
            cols: 80,
            rows: 24,
            argv: vec!["/usr/bin/vim", "-u", "HOME/vimrc", "-n", "fizz.py"],
            env: sh_env.clone(),
            input: vec![
                (900, b"/Buzz\r"),
                (300, b"n"),
                (300, b"Goprint('end of file')\x1b"),
                (300, b":set cursorline\r"),
                (300, b"gg3jdd"),
                (300, b"u"),
                (300, b":q!\r"),
            ],
            grace_ms: 3000,
        },
        Scenario {
            name: "less_view",
            description: "less -N on an 80-line file: page down, search, jump to end, quit",
            cols: 80,
            rows: 24,
            argv: vec!["/usr/bin/less", "-N", "-R", "notes.txt"],
            env: {
                let mut e = sh_env.clone();
                e.push(("LESSHISTFILE", "-"));
                e
            },
            input: vec![
                (700, b" "),
                (300, b"/MARKER\r"),
                (300, b"n"),
                (300, b"G"),
                (300, b"g"),
                (300, b"q"),
            ],
            grace_ms: 3000,
        },
        Scenario {
            name: "top_view",
            description: "macOS top (alternate screen, partial scroll regions, reverse index): process names and the clock replaced by placeholders",
            cols: 100,
            rows: 30,
            argv: vec!["/usr/bin/top", "-o", "cpu", "-n", "12", "-s", "1", "-stats", "pid,command,cpu,mem,threads"],
            env: sh_env.clone(),
            input: vec![(3500, b"q")],
            grace_ms: 3000,
        },
        Scenario {
            name: "progress_real",
            description: "a bash loop redrawing a progress bar with carriage returns, then a spinner, in a real shell",
            cols: 80,
            rows: 12,
            argv: vec![
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-c",
                "echo fetching; for i in $(seq 0 5 100); do n=$((i/5)); printf '\\r[%-20s] %3d%%' \"$(printf '#%.0s' $(seq 1 $n))\" $i; sleep 0.02; done; echo; echo done",
            ],
            env: sh_env,
            input: vec![],
            grace_ms: 5000,
        },
    ]
}

fn record(sc: &Scenario, home: &Path) -> Vec<u8> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: sc.rows,
            cols: sc.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let proj = home.join("proj");
    let mut cmd = CommandBuilder::new(sc.argv[0]);
    for a in &sc.argv[1..] {
        cmd.arg(a.replace("HOME", &home.to_string_lossy()));
    }
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("HOME", home);
    cmd.env("PS1", "$ ");
    for (k, v) in &sc.env {
        cmd.env(k, v);
    }
    cmd.cwd(&proj);
    let mut child = pty.slave.spawn_command(cmd).expect("spawn");
    drop(pty.slave);

    let out = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pty.master.try_clone_reader().unwrap();
    let sink = Arc::clone(&out);
    let t = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let mut writer = pty.master.take_writer().unwrap();
    for (delay, bytes) in &sc.input {
        thread::sleep(Duration::from_millis(*delay));
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }
    let deadline = Instant::now() + Duration::from_millis(sc.grace_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                break;
            }
        }
    }
    thread::sleep(Duration::from_millis(100));
    drop(writer);
    drop(pty.master);
    let _ = t.join();
    out.lock().unwrap().clone()
}

/// `top` lists whatever is running on the recording machine. Replace the text of the COMMAND
/// column (it follows a cursor move to column 19 or 20) and the clock with same-length
/// placeholders; the escape sequences, and so the layout and the scrolling, are untouched.
fn scrub_top(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut n = 0u32;
    while i < b.len() {
        out.push(b[i]);
        let mut j = i + 1;
        if b[i] == b'H' {
            // A CUP into the COMMAND column of a process row: `ESC [ row ; col H`.
            let cup = out.iter().rposition(|&c| c == 0x1b).and_then(|e| {
                let p = std::str::from_utf8(&out[e + 2..out.len() - 1]).ok()?;
                let (r, c) = p.split_once(';')?;
                Some((r.parse::<u16>().ok()?, c.parse::<u16>().ok()?))
            });
            let in_column = cup.is_some_and(|(r, c)| r >= 11 && (17..=46).contains(&c));
            if in_column && !b[j..].starts_with(b"COMMAND") {
                let start = j;
                while j < b.len() && b[j] >= 0x20 && b[j] != 0x1b {
                    j += 1;
                }
                n += 1;
                let name = format!("proc{n:04}");
                for k in 0..j - start {
                    out.push(name.as_bytes()[k % name.len()]);
                }
            }
        }
        i = j;
    }
    // The clock: HH:MM:SS.
    let mut k = 0;
    while k + 8 <= out.len() {
        let w = &out[k..k + 8];
        if w[2] == b':'
            && w[5] == b':'
            && w.iter()
                .enumerate()
                .all(|(x, c)| x == 2 || x == 5 || c.is_ascii_digit())
        {
            out[k..k + 8].copy_from_slice(b"12:00:00");
            k += 8;
        } else {
            k += 1;
        }
    }
    out
}

fn main() {
    let want: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let home = std::env::temp_dir().join(format!("fil-record-{}", std::process::id()));
    std::fs::create_dir_all(home.join("proj")).unwrap();
    std::fs::write(home.join("vimrc"), VIMRC).unwrap();
    populate(&home.join("proj"));
    for sc in scenarios() {
        if !want.is_empty() && !want.iter().any(|w| w == sc.name) {
            continue;
        }
        let mut bytes = record(&sc, &home);
        if sc.name == "top_view" {
            bytes = scrub_top(&bytes);
        }
        let text = String::from_utf8_lossy(&bytes);
        let leaked = text.contains(&*home.to_string_lossy());
        std::fs::write(dir.join(format!("{}.raw", sc.name)), &bytes).unwrap();
        std::fs::write(
            dir.join(format!("{}.meta", sc.name)),
            format!(
                "cols = {}\nrows = {}\nkind = real\ndescription = {}\n",
                sc.cols, sc.rows, sc.description
            ),
        )
        .unwrap();
        println!(
            "{:<14} {:>7} bytes{}",
            sc.name,
            bytes.len(),
            if leaked {
                "  WARNING: contains the temp path"
            } else {
                ""
            }
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}
