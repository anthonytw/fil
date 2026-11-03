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
//!   hand off from the main thread). The writes at the end of a run are the exception: they go
//!   through [`TermOutput::write_all_within`] ([`write_within`]): after the termios is back, in whole
//!   escape sequences, polled with a deadline on the calling thread, so a terminal that stopped
//!   reading cannot hold the client in raw mode and nothing is left running behind the call.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
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
                // One plain write, not `write_fd`'s retry loop. The fd is blocking, so a terminal
                // that has stopped reading (XOFF, a stuck emulator) with a full queue can still
                // make this one wait, but the sequence is short (about a hundred bytes), so only a
                // terminal whose queue is already full does.
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
            // A window that cannot be read right now is not a new size: no event, and the client
            // goes on with the size it has (the next good read, or the next event, corrects it).
            let Ok((cols, rows)) = winsize(&size_fd) else {
                continue;
            };
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

/// The window size, or an error when it cannot be read or reads as empty (no terminal size on
/// the descriptor, a transient failure, a size of 0x0). There is no default here on purpose: a
/// made-up 80x24 would look like the user resizing the window.
fn winsize<Fd: AsFd>(fd: Fd) -> io::Result<(u16, u16)> {
    match tcgetwinsize(fd) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => Ok((w.ws_col, w.ws_row)),
        Ok(_) => Err(io::Error::other(
            "the terminal reports an empty window size",
        )),
        Err(e) => Err(errno(e)),
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

/// Writes `bytes` to the terminal or pipe on `fd`, giving up with [`io::ErrorKind::TimedOut`]
/// after `within`. For the end of a process, when a stuck write must not keep the user's shell
/// from getting the terminal back; nothing is left running after it returns.
///
/// - **No thread, no shared flag.** A tty is reopened by name as a fresh open file description,
///   with `O_NONBLOCK`, so the blocking mode of the descriptor the shell shares is not touched
///   (any other `fd`, or a tty that cannot be reopened, gets `O_NONBLOCK` for the length of the
///   call and its old flags back after). The deadline is kept by polling on the calling thread.
/// - **Whole sequences.** `bytes` is written in pieces that each start at an escape sequence,
///   and a piece is begun only once the terminal has room, so a terminal that stopped reading is
///   usually noticed between two sequences and not in the middle of one. A synchronized update
///   (`CSI ? 2026 h` ... `CSI ? 2026 l`) is one piece. A terminal that takes part of a piece and
///   then stops can still be left with a cut sequence: giving up on a stuck terminal cannot be
///   made free of it.
pub fn write_within<Fd: AsFd>(fd: Fd, bytes: &[u8], within: Duration) -> io::Result<()> {
    let deadline = Instant::now() + within;
    let fd = fd.as_fd();
    let (w, restore) = match reopen_tty(fd) {
        Some(w) => (w, None),
        None => {
            let w = rustix::io::dup(fd).map_err(errno)?;
            let old = rustix::fs::fcntl_getfl(&w).map_err(errno)?;
            if old.contains(rustix::fs::OFlags::NONBLOCK) {
                (w, None)
            } else {
                rustix::fs::fcntl_setfl(&w, old | rustix::fs::OFlags::NONBLOCK).map_err(errno)?;
                (w, Some(old))
            }
        }
    };
    let result = write_pieces(&w, bytes, deadline);
    if let Some(old) = restore {
        let _ = rustix::fs::fcntl_setfl(&w, old);
    }
    result
}

/// Writes `text` to this process's standard error, within `within` (see [`write_within`]): for
/// what is printed after the terminal was handed back, such as the error that ended a run.
/// Failures are ignored; there is nowhere else to say them.
pub fn write_stderr_within(text: &str, within: Duration) {
    let _ = write_within(rustix::stdio::stderr(), text.as_bytes(), within);
}

/// A new `O_NONBLOCK` open file description of the terminal behind `fd`, if it is one that can be
/// opened again by name and is the same device.
fn reopen_tty(fd: BorrowedFd<'_>) -> Option<OwnedFd> {
    use rustix::fs::{Mode, OFlags, fstat, open};
    if !rustix::termios::isatty(fd) {
        return None;
    }
    let name = rustix::termios::ttyname(fd, Vec::new()).ok()?;
    let w = open(
        name.as_c_str(),
        OFlags::WRONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    // The name could have been reused since: it must be the terminal we were given.
    let (a, b) = (fstat(fd).ok()?, fstat(&w).ok()?);
    let same = a.st_rdev == b.st_rdev && a.st_dev == b.st_dev && a.st_ino == b.st_ino;
    (same && rustix::termios::isatty(&w)).then_some(w)
}

/// Splits `bytes` before each escape sequence, except that a synchronized update stays whole and
/// the `ESC \\` that ends a string is not a start.
fn pieces(bytes: &[u8]) -> Vec<&[u8]> {
    const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
    const SYNC_END: &[u8] = b"\x1b[?2026l";
    let mut out = Vec::new();
    let (mut start, mut hold_until) = (0, 0);
    for i in 0..bytes.len() {
        if bytes[i] != 0x1b || i < hold_until || bytes.get(i + 1) == Some(&b'\\') {
            continue;
        }
        if i > start {
            out.push(&bytes[start..i]);
            start = i;
        }
        if bytes[i..].starts_with(SYNC_BEGIN)
            && let Some(end) = bytes[i..]
                .windows(SYNC_END.len())
                .position(|w| w == SYNC_END)
        {
            hold_until = i + end + SYNC_END.len();
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

fn write_pieces(w: &OwnedFd, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    for piece in pieces(bytes) {
        // Room first: a terminal that is full stops the write here, between two sequences.
        wait_writable(w, deadline)?;
        let mut rest = piece;
        while !rest.is_empty() {
            match rustix::io::write(w, rest) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => rest = &rest[n..],
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => wait_writable(w, deadline)?,
                Err(e) => return Err(errno(e)),
            }
        }
    }
    Ok(())
}

fn wait_writable(w: &OwnedFd, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let t = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(w, PollFlags::OUT)];
        match poll(&mut fds, Some(&t)) {
            Ok(n) if n > 0 => return Ok(()),
            Ok(_) | Err(Errno::INTR) => {}
            Err(e) => return Err(errno(e)),
        }
    }
}

impl TermOutput for TtyOutput {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        winsize(&self.fd)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_fd(&self.fd, bytes)
    }

    fn write_all_within(&mut self, bytes: &[u8], within: Duration) -> io::Result<()> {
        write_within(&self.fd, bytes, within)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{pieces, write_within};

    #[test]
    fn a_full_pipe_times_out_and_keeps_its_blocking_mode() {
        let (rx, tx) = rustix::pipe::pipe().unwrap();
        let before = rustix::fs::fcntl_getfl(&tx).unwrap();
        // Fill it through a non-blocking duplicate of the writer.
        let nb = rustix::io::dup(&tx).unwrap();
        rustix::fs::fcntl_setfl(&nb, before | rustix::fs::OFlags::NONBLOCK).unwrap();
        while rustix::io::write(&nb, &[0u8; 4096]).is_ok() {}
        // (That flag is shared: put the writer back the way a caller has it.)
        rustix::fs::fcntl_setfl(&tx, before).unwrap();
        let started = std::time::Instant::now();
        let err = write_within(&tx, b"fil: boom\n", Duration::from_millis(150)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            !rustix::fs::fcntl_getfl(&tx)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK),
            "the writer was left non-blocking"
        );
        // With room it writes whole.
        rustix::fs::fcntl_setfl(&rx, rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut sink = [0u8; 65536];
        while rustix::io::read(&rx, &mut sink).is_ok_and(|n| n == sink.len()) {}
        write_within(&tx, b"fil: boom\n", Duration::from_secs(2)).unwrap();
    }

    fn joined(p: &[&[u8]]) -> Vec<u8> {
        p.concat()
    }

    #[test]
    fn exit_output_is_cut_only_between_sequences() {
        let b = b"\x18\x1b[?1l\x1b>\x1b[0m\x1b]8;;\x1b\\\r\nbye\r\n";
        let p = pieces(b);
        assert_eq!(joined(&p), b);
        assert_eq!(
            p,
            vec![
                &b"\x18"[..],
                b"\x1b[?1l",
                b"\x1b>",
                b"\x1b[0m",
                b"\x1b]8;;\x1b\\\r\nbye\r\n"
            ]
        );
    }

    #[test]
    fn a_synchronized_update_is_one_piece() {
        let b = b"\x1b[?2026h\x1b[3;4H\x1b[K\x1b[?2026l\x1b[?25h";
        let p = pieces(b);
        assert_eq!(joined(&p), b);
        assert_eq!(
            p,
            vec![&b"\x1b[?2026h\x1b[3;4H\x1b[K\x1b[?2026l"[..], b"\x1b[?25h"]
        );
        // One that is never closed is cut like any other text.
        assert_eq!(pieces(b"\x1b[?2026h\x1b[K").len(), 2);
        assert!(pieces(b"").is_empty());
    }
}
