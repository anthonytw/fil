//! A program on a pseudo-terminal, with a headless screen, for the two drivers of the real `fil`
//! binary: `ptydrive` (a server that bash scenarios talk to) and the Pi examples' harness
//! (`crates/fil/examples/support/harness.rs`). What they share is here: opening the PTY,
//! spawning, the reader thread that keeps the raw bytes and a `vt100` screen current, the thread
//! that reaps the child and records how it ended, and sending, resizing and killing. How they
//! wait differs on purpose (a pattern with a settle time on the server; a closure with a short
//! look after exit in the harness), so each keeps its own waits and reads [`PtyState`] directly.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

pub use portable_pty::CommandBuilder;
use portable_pty::{ChildKiller, MasterPty, PtySize, native_pty_system};

/// How a program ended.
pub struct Exit {
    pub code: u32,
    /// The signal's name, when it was killed by one.
    pub signal: Option<String>,
}

/// Something that sees every byte the program prints, with the state locked (so it is exactly in
/// step with `raw` and the screen): the harness keeps a copy with the escape sequences removed.
pub trait Tap: Send + 'static {
    fn feed(&mut self, bytes: &[u8]);
}

impl Tap for () {
    fn feed(&mut self, _: &[u8]) {}
}

pub struct PtyState<T> {
    /// Every byte received from the program so far.
    pub raw: Vec<u8>,
    /// What a terminal would show now.
    pub parser: vt100::Parser,
    pub tap: T,
    pub exit: Option<Exit>,
    /// When the program was found to have ended.
    pub exit_at: Option<Instant>,
    /// The program's output has ended (the reader saw end of file).
    pub eof: bool,
}

pub struct PtyProc<T: Tap = ()> {
    state: Arc<Mutex<PtyState<T>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
}

fn nz(n: u16) -> Result<std::num::NonZeroU16, String> {
    std::num::NonZeroU16::new(n).ok_or_else(|| "rows and cols must be at least 1".to_string())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<T: Tap> PtyProc<T> {
    /// Starts `cmd` on a new PTY of `rows` x `cols`. `tap` sees the output.
    pub fn spawn(cmd: CommandBuilder, rows: u16, cols: u16, tap: T) -> Result<Self, String> {
        // Before anything is started: a zero size must not leave a child behind.
        let (nz_rows, nz_cols) = (nz(rows)?, nz(cols)?);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("cannot start the program: {e}"))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("writer: {e}"))?;
        let state = Arc::new(Mutex::new(PtyState {
            raw: Vec::new(),
            parser: vt100::Parser::new(nz_rows, nz_cols, 0),
            tap,
            exit: None,
            exit_at: None,
            eof: false,
        }));
        let st = state.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut g = lock(&st);
                let g = &mut *g;
                g.raw.extend_from_slice(&buf[..n]);
                g.tap.feed(&buf[..n]);
                g.parser.process(&buf[..n]);
            }
            lock(&st).eof = true;
        });
        let st = state.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            let mut g = lock(&st);
            g.exit_at = Some(Instant::now());
            g.exit = Some(match status {
                Ok(s) => Exit {
                    code: s.exit_code(),
                    signal: s.signal().map(str::to_string),
                },
                Err(_) => Exit {
                    code: 255,
                    signal: None,
                },
            });
        });
        Ok(PtyProc {
            state,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            pid,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The output, the screen and the exit status, locked. Keep it short.
    pub fn state(&self) -> MutexGuard<'_, PtyState<T>> {
        lock(&self.state)
    }

    /// Types `bytes` into the program's terminal.
    pub fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = lock(&self.writer);
        w.write_all(bytes).and_then(|()| w.flush())
    }

    /// Resizes the terminal (the program gets SIGWINCH) and the screen.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        let (r, c) = (nz(rows)?, nz(cols)?);
        lock(&self.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        lock(&self.state).parser.set_size(r, c);
        Ok(())
    }

    /// The terminal's master side (termios queries).
    pub fn master(&self) -> MutexGuard<'_, Box<dyn MasterPty + Send>> {
        lock(&self.master)
    }

    /// Hangs up the program (SIGHUP, portable-pty's kill).
    pub fn hangup(&self) {
        let _ = lock(&self.killer).kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sh(script: &str) -> CommandBuilder {
        let mut c = CommandBuilder::new("/bin/sh");
        c.arg("-c");
        c.arg(script);
        c
    }

    fn wait(p: &PtyProc, what: &str, f: impl Fn(&PtyState<()>) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if f(&p.state()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn output_reaches_the_raw_bytes_and_the_screen_and_the_exit_is_recorded() {
        let p = PtyProc::spawn(sh("echo hello; exit 3"), 24, 80, ()).unwrap();
        wait(&p, "the exit", |s| s.exit.is_some() && s.eof);
        let s = p.state();
        assert!(String::from_utf8_lossy(&s.raw).contains("hello"));
        assert!(s.parser.screen().contents().contains("hello"));
        assert_eq!(s.exit.as_ref().unwrap().code, 3);
    }

    #[test]
    fn input_is_typed_into_the_program_and_a_resize_reaches_it() {
        let p =
            PtyProc::spawn(sh("stty size; read x; stty size; echo got-$x"), 24, 80, ()).unwrap();
        wait(&p, "the first size", |s| {
            s.parser.screen().contents().contains("24 80")
        });
        p.resize(30, 100).unwrap();
        p.send(b"go\r").unwrap();
        wait(&p, "the second size and the input", |s| {
            let t = s.parser.screen().contents();
            t.contains("30 100") && t.contains("got-go")
        });
        assert!(p.resize(0, 10).is_err());
    }

    #[test]
    fn a_hangup_ends_a_program_that_waits() {
        let p = PtyProc::spawn(sh("sleep 60"), 24, 80, ()).unwrap();
        p.hangup();
        wait(&p, "the exit", |s| s.exit.is_some());
    }

    #[test]
    fn a_tap_sees_the_same_bytes() {
        struct Count(Arc<Mutex<usize>>);
        impl Tap for Count {
            fn feed(&mut self, b: &[u8]) {
                *self.0.lock().unwrap() += b.len();
            }
        }
        let n = Arc::new(Mutex::new(0));
        let p = PtyProc::spawn(sh("printf abcdef"), 24, 80, Count(n.clone())).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && !p.state().eof {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*n.lock().unwrap(), p.state().raw.len());
        assert!(*n.lock().unwrap() >= 6);
    }
}
