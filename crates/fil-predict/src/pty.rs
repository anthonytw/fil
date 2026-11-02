//! Real programs on a PTY behind a `fil-netsim` link (feature `harness`).
//!
//! The server side of the spike: a program on a PTY, a pump that frames its output for the
//! client over a simulated pipe (with latency in both directions), and a termios sampler
//! that stands in for the daemon's "canonical mode / echo" report (DESIGN.md §9; in M1 the
//! daemon reports it). Frames, server to client:
//!
//! | tag | payload                      |
//! |-----|------------------------------|
//! | 0   | PTY output bytes             |
//! | 1   | one byte: bit 0 ICANON, bit 1 ECHO |
//! | 2   | end of stream                |
//!
//! Client to server is the raw keystrokes. The harness and the demo are the two clients.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fil_netsim::{StreamConfig, StreamControl, StreamEnd, stream_pair};
use fil_term::Size;
use nix::sys::termios::LocalFlags;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::overlay::PtyMode;
use crate::sim::Rng;

/// What to run.
#[derive(Clone, Debug)]
pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A scratch directory removed on drop. Programs get it as `HOME` and working directory, so
/// nothing real is touched and no real path appears in a recording.
pub struct Scratch {
    pub dir: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    pub fn new() -> std::io::Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fil-predict-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin"))?;
        Ok(Scratch { dir })
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub enum Frame {
    Output(Vec<u8>),
    Mode(PtyMode),
    Eof,
}

/// The client's end of the link and the handles to stop the program.
pub struct Remote {
    pub client: StreamEnd,
    pub ctl: StreamControl,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    buf: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LinkConfig {
    pub one_way: Duration,
    pub jitter: Duration,
    pub seed: u64,
    /// Cut the program's output into pieces of 1 to this many bytes with probability 1/3 per
    /// read, to stress sequences split across frames. 0 turns it off.
    pub split: usize,
}

fn mode_of(master: &(dyn portable_pty::MasterPty + Send)) -> Option<PtyMode> {
    let t = master.get_termios()?;
    Some(PtyMode {
        canonical: t.local_flags.contains(LocalFlags::ICANON),
        echo: t.local_flags.contains(LocalFlags::ECHO),
    })
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 5);
    v.push(tag);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Spawns `spec` on a PTY of `size` and starts pumping. Must be called inside a tokio
/// runtime.
pub fn spawn(spec: &Spec, size: Size, link: &LinkConfig) -> Result<Remote, String> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;
    let mut cmd = CommandBuilder::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.cwd(&spec.cwd);
    let child = pty.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pty.master.take_writer().map_err(|e| e.to_string())?;
    let master = std::sync::Arc::new(std::sync::Mutex::new(pty.master));

    let (client, server, ctl) = stream_pair(
        link.seed,
        StreamConfig {
            latency: link.one_way,
            jitter: link.jitter,
            ..StreamConfig::default()
        },
    );

    // Blocking PTY I/O lives on threads.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = out_tx.send(None);
                    return;
                }
                Ok(n) => {
                    if out_tx.send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(b) = in_rx.recv() {
            if writer.write_all(&b).is_err() || writer.flush().is_err() {
                return;
            }
        }
    });

    let split = link.split;
    let mut rng = Rng::new(link.seed ^ 0x51_17);
    tokio::spawn(async move {
        let (mut rd, mut wr) = tokio::io::split(server);
        let mut last: Option<PtyMode> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        let mut inbuf = [0u8; 1024];
        loop {
            // The report goes out before any output read after the change.
            let sample = |last: &mut Option<PtyMode>| -> Option<Vec<u8>> {
                let m = mode_of(master.lock().ok()?.as_ref())?;
                if *last == Some(m) {
                    return None;
                }
                *last = Some(m);
                Some(frame(1, &[u8::from(m.canonical) | (u8::from(m.echo) << 1)]))
            };
            tokio::select! {
                got = out_rx.recv() => match got {
                    Some(Some(bytes)) => {
                        if let Some(f) = sample(&mut last)
                            && wr.write_all(&f).await.is_err()
                        {
                            return;
                        }
                        let mut rest = &bytes[..];
                        while !rest.is_empty() {
                            let n = if split > 0 && rng.chance(33) {
                                (1 + rng.below(split as u64) as usize).min(rest.len())
                            } else {
                                rest.len()
                            };
                            if wr.write_all(&frame(0, &rest[..n])).await.is_err() {
                                return;
                            }
                            rest = &rest[n..];
                        }
                    }
                    _ => {
                        let _ = wr.write_all(&frame(2, &[])).await;
                        let _ = wr.shutdown().await;
                        return;
                    }
                },
                n = rd.read(&mut inbuf) => match n {
                    Ok(n) if n > 0 => {
                        if in_tx.send(inbuf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    _ => return,
                },
                _ = tick.tick() => {
                    if let Some(f) = sample(&mut last)
                        && wr.write_all(&f).await.is_err()
                    {
                        return;
                    }
                }
            }
        }
    });

    Ok(Remote {
        client,
        ctl,
        child,
        buf: Vec::new(),
    })
}

impl Remote {
    /// The next frame from the program, or `None` once the link is closed.
    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() >= 5 {
                let n = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]])
                    as usize;
                if self.buf.len() >= 5 + n {
                    let tag = self.buf[0];
                    let payload = self.buf[5..5 + n].to_vec();
                    self.buf.drain(..5 + n);
                    return Some(match tag {
                        0 => Frame::Output(payload),
                        1 => Frame::Mode(PtyMode {
                            canonical: payload[0] & 1 != 0,
                            echo: payload[0] & 2 != 0,
                        }),
                        _ => Frame::Eof,
                    });
                }
            }
            let mut tmp = [0u8; 4096];
            match self.client.read(&mut tmp).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    /// Sends keystrokes to the program (they cross the link).
    pub async fn send(&mut self, bytes: &[u8]) -> bool {
        self.client.write_all(bytes).await.is_ok()
    }

    /// Stops the program (only the process this spawned).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---- the programs ---------------------------------------------------------------------

/// `(milliseconds to wait, bytes to type)` steps.
pub type Script = Vec<(u64, Vec<u8>)>;

pub struct Program {
    pub name: &'static str,
    /// Characters typed into a hidden prompt that must never reach the screen.
    pub secret: &'static str,
    spec: fn(&Scratch, u64) -> Spec,
    keys: fn(&mut Rng, usize) -> Script,
}

impl Program {
    pub fn spec(&self, scratch: &Scratch, seed: u64) -> Spec {
        (self.spec)(scratch, seed)
    }

    /// `(milliseconds to wait, bytes to type)`.
    pub fn keys(&self, rng: &mut Rng, n: usize) -> Script {
        (self.keys)(rng, n)
    }
}

fn base_env(s: &Scratch) -> Vec<(String, String)> {
    let home = s.dir.to_string_lossy().into_owned();
    vec![
        ("HOME".into(), home.clone()),
        ("TERM".into(), "xterm-256color".into()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("LC_ALL".into(), "en_US.UTF-8".into()),
        ("PATH".into(), format!("{home}/bin")),
        ("BASH_SILENCE_DEPRECATION_WARNING".into(), "1".into()),
        ("PS1".into(), "$ ".into()),
        ("LESS".into(), "-R".into()),
        ("LESSHISTFILE".into(), "-".into()),
    ]
}

/// Where a tool lives on this machine (the paths differ between macOS and Linux).
fn find(name: &str) -> String {
    for dir in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = Path::new(dir).join(name);
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    name.to_string()
}

/// A PATH of exactly the commands the programs may run.
fn tools(s: &Scratch) {
    for name in [
        "ls", "cat", "date", "sleep", "seq", "head", "vim", "less", "stty",
    ] {
        let _ = std::os::unix::fs::symlink(find(name), s.path("bin").join(name));
    }
}

fn spec_of(s: &Scratch, argv: &[&str]) -> Spec {
    tools(s);
    Spec {
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        env: base_env(s),
        cwd: s.dir.clone(),
    }
}

fn typing(rng: &mut Rng, n: usize, words: &[&str]) -> Script {
    let mut v = Vec::new();
    while v.len() < n {
        let gap = if rng.chance(15) {
            rng.range(150, 400)
        } else {
            rng.range(8, 90)
        };
        let k: Vec<u8> = match rng.below(100) {
            0..=69 => vec![*rng.pick(b"abcdefghijklmnopqrstuvwxyz0123456789 ,._-")],
            70..=79 => vec![0x7f],
            80..=87 => rng.pick(words).as_bytes().to_vec(),
            88..=92 => vec![b'\r'],
            93..=94 => b"\x1b[D".to_vec(),
            95 => b"\x1b[C".to_vec(),
            96 => vec![0x15],
            97 => "é".as_bytes().to_vec(),
            98 => "日本".as_bytes().to_vec(),
            _ => vec![0x01],
        };
        v.push((gap, k));
    }
    v
}

fn shell_spec(s: &Scratch, _seed: u64) -> Spec {
    spec_of(s, &[find("bash").as_str(), "--noprofile", "--norc", "-i"])
}

fn shell_keys(rng: &mut Rng, n: usize) -> Script {
    let mut v = typing(
        rng,
        n,
        &[
            "echo hello",
            "ls",
            "date",
            "seq 1 30",
            "echo 'a b c'",
            "head -c 300 /dev/null",
            "printf 'x\\ty\\n'",
            "cat",
        ],
    );
    v.push((300, b"\x03".to_vec()));
    v
}

fn cat_spec(s: &Scratch, _seed: u64) -> Spec {
    spec_of(s, &[find("cat").as_str()])
}

fn cat_keys(rng: &mut Rng, n: usize) -> Script {
    let mut v = typing(rng, n, &["hello world", "the quick brown fox"]);
    v.push((300, vec![4]));
    v
}

fn vim_spec(s: &Scratch, _seed: u64) -> Spec {
    let _ = std::fs::write(s.path("note.txt"), "first line\nsecond line\nthird line\n");
    spec_of(
        s,
        &[
            find("vim").as_str(),
            "-u",
            "NONE",
            "-i",
            "NONE",
            "-n",
            "note.txt",
        ],
    )
}

fn vim_keys(rng: &mut Rng, n: usize) -> Script {
    let mut v = vec![(600, b"i".to_vec())];
    for (g, k) in typing(rng, n, &["word", "hello"]) {
        // No Enter-driven surprises beyond new lines; keep keys inside insert mode.
        v.push((g, k));
    }
    v.push((100, vec![0x1b]));
    for _ in 0..8 {
        v.push((rng.range(30, 120), vec![*rng.pick(b"hjklxwb0$u")]));
    }
    v.push((200, b":q!\r".to_vec()));
    v
}

fn less_spec(s: &Scratch, _seed: u64) -> Spec {
    let text: String = (1..=80)
        .map(|i| format!("line {i} of the file\n"))
        .collect();
    let _ = std::fs::write(s.path("long.txt"), text);
    spec_of(s, &[find("less").as_str(), "long.txt"])
}

fn less_keys(rng: &mut Rng, n: usize) -> Script {
    let mut v = Vec::new();
    for _ in 0..n {
        let k: &[u8] = rng.pick(&[
            &b" "[..],
            b"b",
            b"j",
            b"k",
            b"d",
            b"u",
            b"G",
            b"g",
            b"/line 4\r",
            b"n",
            b"\x1b[B",
            b"\x1b[A",
        ]);
        v.push((rng.range(20, 200), k.to_vec()));
    }
    v.push((200, b"q".to_vec()));
    v
}

fn password_spec(s: &Scratch, _seed: u64) -> Spec {
    spec_of(
        s,
        &[
            find("bash").as_str(),
            "--noprofile",
            "--norc",
            "-c",
            "for i in 1 2 3 4 5 6 7 8 9 10; do read -s -p 'Password: ' x; echo; echo \"got ${#x} chars\"; done",
        ],
    )
}

fn password_keys(rng: &mut Rng, n: usize) -> Script {
    let mut v = Vec::new();
    while v.len() < n {
        // Typed ahead of the prompt, the way a fast typist does.
        for _ in 0..rng.range(2, 8) {
            v.push((rng.range(8, 70), vec![*rng.pick(b"QXZ")]));
        }
        if rng.chance(30) {
            v.push((rng.range(8, 70), vec![0x7f]));
        }
        v.push((rng.range(8, 120), b"\r".to_vec()));
    }
    v
}

/// Escape sequences, as `printf` format strings with octal escapes, for the adversary.
fn adversary_pool() -> Vec<String> {
    let esc = |b: &[u8]| -> String {
        b.iter()
            .map(|c| {
                if c.is_ascii_alphanumeric() || *c == b' ' {
                    (*c as char).to_string()
                } else {
                    format!("\\{c:03o}")
                }
            })
            .collect()
    };
    let items: Vec<&[u8]> = vec![
        b"\x1b[4h",
        b"\x1b[4l",
        b"\x1b(0",
        b"\x1b(B",
        b"\x0e",
        b"\x0f",
        b"\x1b[?6h\x1b[2;8r",
        b"\x1b[?6l\x1b[r",
        b"\x1b[1;31;4:3;58;5;9m",
        b"\x1b[0m",
        b"\x1b]8;;http://example.test/x\x07",
        b"\x1b]8;;\x07",
        b"\x1b7",
        b"\x1b8",
        b"\x1b[2J\x1b[H",
        b"\x1b[?1049h\x1b[2J",
        b"\x1b[?1049l",
        b"\x1b[1;1Hclock",
        b"\r\nbackground job done\r\n",
        b"\x1b[?25l",
        b"\x1b[?25h",
        b"\x1b[?7l",
        b"\x1b[?7h",
        "日本語".as_bytes(),
        b"\x1b[40C",
        b"\x1b[?2026h",
        b"\x1b[?2026l",
        b"\x1b[!p",
        b"\x1bc",
    ];
    items.into_iter().map(esc).collect()
}

fn adversary_spec(s: &Scratch, seed: u64) -> Spec {
    let pool = adversary_pool()
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "RANDOM={seed}\n\
         stty -icanon -echo min 1 time 0\n\
         d=({pool})\n\
         printf '$ '\n\
         while IFS= read -rsn1 c; do\n\
         \tif [ -z \"$c\" ]; then printf '\\r\\n$ '\n\
         \telif [ \"$c\" = \"$(printf '\\177')\" ]; then printf '\\b \\b'\n\
         \telse printf '%s' \"$c\"; fi\n\
         \tif [ $((RANDOM % 5)) -eq 0 ]; then printf \"${{d[RANDOM % ${{#d[@]}}]}}\"; fi\n\
         done\n"
    );
    let _ = std::fs::write(s.path("adversary.sh"), script);
    spec_of(
        s,
        &[
            find("bash").as_str(),
            "--noprofile",
            "--norc",
            "adversary.sh",
        ],
    )
}

fn background_spec(s: &Scratch, _seed: u64) -> Spec {
    spec_of(
        s,
        &[
            find("bash").as_str(),
            "--noprofile",
            "--norc",
            "-c",
            "(while sleep 0.4; do printf '\\r\\n[bg] tick\\r\\n'; done) & exec cat",
        ],
    )
}

pub fn programs() -> Vec<Program> {
    vec![
        Program {
            name: "shell",
            secret: "",
            spec: shell_spec,
            keys: shell_keys,
        },
        Program {
            name: "cat",
            secret: "",
            spec: cat_spec,
            keys: cat_keys,
        },
        Program {
            name: "vim",
            secret: "",
            spec: vim_spec,
            keys: vim_keys,
        },
        Program {
            name: "less",
            secret: "",
            spec: less_spec,
            keys: less_keys,
        },
        Program {
            name: "read-s",
            secret: "QXZ",
            spec: password_spec,
            keys: password_keys,
        },
        Program {
            name: "adversary",
            secret: "",
            spec: adversary_spec,
            keys: |rng, n| typing(rng, n, &["hello", "world"]),
        },
        Program {
            name: "background",
            secret: "",
            spec: background_spec,
            keys: cat_keys,
        },
    ]
}

/// The directory a spec runs in, for messages.
pub fn dir_of(spec: &Spec) -> &Path {
    &spec.cwd
}
