//! A real terminal: raw mode through termios, size changes, input and signals (no `unsafe`).
//!
//! - **Raw mode** is entered on [`TermOutput::set_raw`] and undone four ways, so the user's shell
//!   is not left without echo: by `set_raw(false)`, when the [`TtyOutput`] is dropped, by a
//!   panic hook that restores the saved settings before the panic message prints, and by the
//!   [`TermEvent::Signal`] the loop turns into a normal return. `kill -9` and `process::exit`
//!   skip all of them; a caller that exits the process must drop the terminal first.
//! - **Input** is read on a thread that only reads while raw mode is on, so keystrokes typed while
//!   fil is connecting stay in the kernel's queue instead of being eaten early. (kqueue cannot
//!   watch a tty on macOS, so tokio's async fd is not an option.) The thread sleeps in `poll`
//!   with no timeout, on the terminal (while raw mode is on) and on a self-pipe that raw-mode
//!   changes and shutdown write to: an idle client makes no wakeups for it.
//! - **Output** is written with blocking writes, in a loop that handles partial writes and
//!   `EINTR`/`EAGAIN`. The client loop runs on the main thread (`block_on`), so a terminal that
//!   cannot keep up applies backpressure to that loop, by design: the loop stops reading the
//!   link, stops acknowledging output the terminal has not taken, and the daemon's catch-up rule
//!   (DESIGN.md section 6.2) skips a slow client ahead with a snapshot instead of buffering
//!   without bound. Liveness does not wait on the loop: the links' heartbeats and watchdogs run
//!   on the runtime's worker threads. What does wait is the user's own input, resizes and
//!   signals, which are read by the same loop; against a terminal that has stopped for good (XOFF,
//!   a hung emulator) the way out is the terminal's, not ours. If that ever matters, the fix is a
//!   dedicated writer thread with a bounded queue, not `block_in_place` (which has no worker to
//!   hand off from the main thread).

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

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

/// Written by the panic hook after the termios is back: cancel a half-written sequence, end a
/// synchronized update, mouse reporting (every mode and encoding), focus events, bracketed paste,
/// application cursor keys and keypad, show the cursor, reset the pen. Each is a no-op on a
/// terminal where the mode is already off.
const PANIC_RESET: &[u8] = b"\x18\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\
\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[?25h\x1b[0m";

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
                // The program's modes cannot be read here (the shadow screen is not reachable
                // from a hook), so write the part of the exit restore that does nothing visible
                // when the modes are already off. It does not leave the alternate screen or pop
                // the kitty keyboard stack: both need the shadow's state (`?1049l` on the primary
                // screen makes some terminals jump the cursor), so a panic in a full-screen
                // program still leaves those; `reset` clears them.
                // One write, not `write_fd`: a hook must not wait on a terminal that is stuck.
                let _ = rustix::io::write(fd, PANIC_RESET);
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
        // Drain, not flush: typeahead stays queued for the session. (Entering only: output
        // still queued must reach the terminal as it was written, in cooked mode.)
        tcsetattr(&fd, OptionalActions::Drain, &raw).map_err(errno)?;
        Ok(RawGuard { fd, saved })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // `Now`, not `Drain`: draining waits for the terminal to take all queued output, and a
        // terminal that has stopped reading (XOFF, a stuck emulator) would hang the exit forever.
        let _ = tcsetattr(&self.fd, OptionalActions::Now, &self.saved);
        if let Ok(mut slot) = PANIC_RESTORE.lock() {
            *slot = None;
        }
    }
}

struct Shared {
    /// Raw mode is on: the reader may read.
    enabled: AtomicBool,
    stop: AtomicBool,
    /// The write end of the reader thread's self-pipe.
    wake_tx: OwnedFd,
}

impl Shared {
    /// Wakes the reader thread out of `poll` to look at `enabled` and `stop` again.
    fn wake(&self) {
        // Non-blocking: a full pipe already holds a wakeup.
        let _ = rustix::io::write(&self.wake_tx, &[1]);
    }
}

/// A pipe whose two ends never block and do not leak into children.
///
/// Where the OS has `pipe2` (Linux, the BSDs) both flags are set atomically, so a child spawned
/// by another thread at the same moment cannot inherit it. macOS has no `pipe2`: there the flags
/// are set right after `pipe()`, and a `fork`+`exec` on another thread between the two calls
/// could inherit the two ends. That child would only hold a pipe open (it never writes to
/// it, and the reader thread never sees EOF on it), so the window is harmless; it is closed as
/// far as the platform allows by setting the flags before anything else is done with the fds.
fn self_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(not(any(target_vendor = "apple", target_os = "haiku", target_os = "aix")))]
    {
        use rustix::pipe::{PipeFlags, pipe_with};
        pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).map_err(errno)
    }
    #[cfg(any(target_vendor = "apple", target_os = "haiku", target_os = "aix"))]
    {
        let (rx, tx) = rustix::pipe::pipe().map_err(errno)?;
        for fd in [&rx, &tx] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(errno)?;
            rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::NONBLOCK).map_err(errno)?;
        }
        Ok((rx, tx))
    }
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
        self.shared.wake();
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
    let (wake_rx, wake_tx) = self_pipe()?;
    let shared = Arc::new(Shared {
        enabled: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        wake_tx,
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
        .spawn(move || read_loop(input, wake_rx, tx, reader_shared))?;

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

fn read_loop(fd: OwnedFd, wake_rx: OwnedFd, tx: mpsc::Sender<TermEvent>, shared: Arc<Shared>) {
    let mut buf = [0u8; 4096];
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        // Sleep until the terminal has something (only while raw mode is on) or someone wakes
        // us: no timeout, so an idle client costs nothing here.
        let enabled = shared.enabled.load(Ordering::SeqCst);
        let mut fds = [
            PollFd::new(&wake_rx, PollFlags::IN),
            PollFd::new(&fd, PollFlags::IN),
        ];
        let watched = if enabled { 2 } else { 1 };
        match poll(&mut fds[..watched], None) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => {
                let _ = tx.blocking_send(TermEvent::Eof);
                return;
            }
        }
        if !fds[0].revents().is_empty() {
            // Raw mode changed, or we are to stop: drain the pipe and look again.
            let mut sink = [0u8; 64];
            while matches!(rustix::io::read(&wake_rx, &mut sink[..]), Ok(n) if n > 0) {}
            continue;
        }
        // Raw mode may have been left while we waited: do not eat the shell's input.
        if !enabled || !shared.enabled.load(Ordering::SeqCst) {
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

/// Writes all of `bytes`, handling partial writes, `EINTR` and `EAGAIN`.
fn write_fd(fd: &OwnedFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                // The terminal is full: wait for room (backpressure, see the module docs).
                let mut fds = [PollFd::new(fd, PollFlags::OUT)];
                let _ = poll(&mut fds, None);
            }
            Err(e) => return Err(errno(e)),
        }
    }
    Ok(())
}

impl TermOutput for TtyOutput {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok(winsize(&self.fd))
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_fd(&self.fd, bytes)
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
        self.shared.wake();
        Ok(())
    }
}

impl Drop for TtyOutput {
    fn drop(&mut self) {
        self.shared.enabled.store(false, Ordering::SeqCst);
        self.shared.wake();
        // Dropping the guard restores the saved settings.
        self.raw = None;
    }
}
