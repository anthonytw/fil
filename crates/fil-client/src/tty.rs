//! A real terminal: raw mode through termios, size changes, input and signals (no `unsafe`).
//!
//! - **Raw mode** is entered on [`TermOutput::set_raw`] and undone four ways, so the user's shell
//!   is not left without echo: by `set_raw(false)`, when the [`TtyOutput`] is dropped, by a
//!   panic hook that restores the saved settings before the panic message prints, and by the
//!   [`TermEvent::Signal`] the loop turns into a normal return. `kill -9` and `process::exit`
//!   skip all of them; a caller that exits the process must drop the terminal first.
//! - **Input** is read on a thread that polls the terminal and only reads while raw mode is on, so
//!   keystrokes typed while fil is connecting stay in the kernel's queue instead of being eaten
//!   early. (kqueue cannot watch a tty on macOS, so tokio's async fd is not an option.)
//! - **Output** is written with blocking writes, in a loop that handles partial writes and
//!   `EINTR`/`EAGAIN`.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::terminal::{TermEvent, TermInput, TermOutput, Terminal};

fn errno(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

/// What a panic needs to put the terminal back.
static PANIC_RESTORE: Mutex<Option<(OwnedFd, Termios)>> = Mutex::new(None);
static HOOK: Once = Once::new();

fn install_panic_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // `try_lock`: a panic while the lock is held must not deadlock the hook.
            if let Ok(guard) = PANIC_RESTORE.try_lock()
                && let Some((fd, saved)) = guard.as_ref()
            {
                let _ = tcsetattr(fd, OptionalActions::Now, saved);
            }
            previous(info);
        }));
    });
}

struct RawGuard {
    fd: OwnedFd,
    saved: Termios,
}

impl RawGuard {
    fn enter(tty: &OwnedFd) -> io::Result<RawGuard> {
        let fd = rustix::io::dup(tty).map_err(errno)?;
        let saved = tcgetattr(&fd).map_err(errno)?;
        let mut raw = saved.clone();
        raw.make_raw();
        install_panic_hook();
        let for_panic = rustix::io::dup(&fd).map_err(errno)?;
        if let Ok(mut slot) = PANIC_RESTORE.lock() {
            *slot = Some((for_panic, saved.clone()));
        }
        // Drain, not flush: typeahead stays queued for the session.
        tcsetattr(&fd, OptionalActions::Drain, &raw).map_err(errno)?;
        Ok(RawGuard { fd, saved })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.fd, OptionalActions::Drain, &self.saved);
        if let Ok(mut slot) = PANIC_RESTORE.lock() {
            *slot = None;
        }
    }
}

struct Shared {
    /// Raw mode is on: the reader may read.
    enabled: AtomicBool,
    stop: AtomicBool,
}

/// The output half of a real terminal.
pub struct TtyOutput {
    fd: OwnedFd,
    raw: Option<RawGuard>,
    shared: Arc<Shared>,
}

/// The input half: keystrokes, size changes and signals.
pub struct TtyInput {
    rx: mpsc::Receiver<TermEvent>,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for TtyInput {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// The terminal on this process's stdin and stdout. Must be called inside a tokio runtime
/// (it starts the signal tasks). Fails if stdin or stdout is not a terminal.
pub fn stdio() -> io::Result<Terminal<TtyInput, TtyOutput>> {
    let input = rustix::io::dup(rustix::stdio::stdin()).map_err(errno)?;
    let output = rustix::io::dup(rustix::stdio::stdout()).map_err(errno)?;
    from_fds(input, output, std::env::var("TERM").ok())
}

/// A terminal on these descriptors (both must be terminals; they may be the same device).
/// Must be called inside a tokio runtime.
pub fn from_fds(
    input: OwnedFd,
    output: OwnedFd,
    local_term: Option<String>,
) -> io::Result<Terminal<TtyInput, TtyOutput>> {
    if !rustix::termios::isatty(&input) || !rustix::termios::isatty(&output) {
        return Err(io::Error::other("fil needs a terminal on stdin and stdout"));
    }
    let shared = Arc::new(Shared {
        enabled: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    // Bounded: when the client stops reading (a big paste on a slow link) the reader thread
    // blocks, and the kernel's queue pushes back on the terminal.
    let (tx, rx) = mpsc::channel(64);
    let mut tasks = Vec::new();

    // Signals first, so a failure here leaves no thread behind.
    let size_fd = rustix::io::dup(&output).map_err(errno)?;
    let mut winch = signal(SignalKind::window_change())?;
    let txw = tx.clone();
    tasks.push(tokio::spawn(async move {
        while winch.recv().await.is_some() {
            let (cols, rows) = winsize(&size_fd);
            if txw.send(TermEvent::Resize { cols, rows }).await.is_err() {
                break;
            }
        }
    }));
    for (kind, num) in [
        (SignalKind::terminate(), 15),
        (SignalKind::hangup(), 1),
        (SignalKind::interrupt(), 2),
        (SignalKind::quit(), 3),
    ] {
        let mut s = signal(kind)?;
        let txs = tx.clone();
        tasks.push(tokio::spawn(async move {
            if s.recv().await.is_some() {
                let _ = txs.send(TermEvent::Signal(num)).await;
            }
        }));
    }

    let reader_shared = shared.clone();
    std::thread::Builder::new()
        .name("fil-tty-input".into())
        .spawn(move || read_loop(input, tx, reader_shared))?;

    Ok(Terminal {
        input: TtyInput {
            rx,
            shared: shared.clone(),
            tasks,
        },
        output: TtyOutput {
            fd: output,
            raw: None,
            shared,
        },
        local_term,
    })
}

fn winsize<Fd: AsFd>(fd: Fd) -> (u16, u16) {
    match tcgetwinsize(fd) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => (w.ws_col, w.ws_row),
        _ => (80, 24),
    }
}

fn read_loop(fd: OwnedFd, tx: mpsc::Sender<TermEvent>, shared: Arc<Shared>) {
    let mut buf = [0u8; 4096];
    let tick = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if !shared.enabled.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&tick)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => {
                let _ = tx.blocking_send(TermEvent::Eof);
                return;
            }
        }
        // Raw mode may have been left while we waited: do not eat the shell's input.
        if !shared.enabled.load(Ordering::SeqCst) {
            continue;
        }
        match rustix::io::read(&fd, &mut buf[..]) {
            Ok(0) => {
                let _ = tx.blocking_send(TermEvent::Eof);
                return;
            }
            Ok(n) => {
                if tx
                    .blocking_send(TermEvent::Input(buf[..n].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(Errno::INTR | Errno::AGAIN) => continue,
            Err(_) => {
                let _ = tx.blocking_send(TermEvent::Eof);
                return;
            }
        }
    }
}

impl TermInput for TtyInput {
    async fn next(&mut self) -> TermEvent {
        // `recv` is cancel-safe. A closed channel means the reader is gone.
        self.rx.recv().await.unwrap_or(TermEvent::Eof)
    }
}

impl TermOutput for TtyOutput {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok(winsize(&self.fd))
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match rustix::io::write(&self.fd, bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = &bytes[n..],
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    let mut fds = [PollFd::new(&self.fd, PollFlags::OUT)];
                    let _ = poll(&mut fds, None);
                }
                Err(e) => return Err(errno(e)),
            }
        }
        Ok(())
    }

    fn set_raw(&mut self, raw: bool) -> io::Result<()> {
        if raw {
            if self.raw.is_none() {
                self.raw = Some(RawGuard::enter(&self.fd)?);
            }
            self.shared.enabled.store(true, Ordering::SeqCst);
        } else {
            self.shared.enabled.store(false, Ordering::SeqCst);
            self.raw = None;
        }
        Ok(())
    }
}

impl Drop for TtyOutput {
    fn drop(&mut self) {
        self.shared.enabled.store(false, Ordering::SeqCst);
        // Dropping the guard restores the saved settings.
        self.raw = None;
    }
}
