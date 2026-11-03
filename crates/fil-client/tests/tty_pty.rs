//! The real terminal (`fil_client::tty`) on a pseudo-terminal: raw mode in and out, input, output,
//! window size changes, signals, hangup and the panic hook. On demand: `just client-pty`.
//!
//! These raise SIGWINCH and SIGTERM at the test process itself (the handlers are installed by
//! the terminal under test), so they run one at a time in their own test binary and are not part
//! of `just ci`.

use std::os::fd::OwnedFd;
use std::time::Duration;

use fil_client::tty::from_fds;
use fil_client::{TermEvent, TermInput, TermOutput};
use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Termios, Winsize, tcgetattr, tcsetwinsize};

struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn pty() -> Pty {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave = open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap();
    Pty { master, slave }
}

fn dup(fd: &OwnedFd) -> OwnedFd {
    rustix::io::dup(fd).unwrap()
}

fn lflags(fd: &OwnedFd) -> LocalModes {
    tcgetattr(fd).unwrap().local_modes - LocalModes::PENDIN
}

fn is_cooked(t: &Termios) -> bool {
    t.local_modes
        .contains(LocalModes::ICANON | LocalModes::ECHO)
}

/// Plays the terminal emulator: reads everything the slave writes (echo included). A pty's
/// output must be consumed, or `tcsetattr(TCSADRAIN)` waits for it forever.
fn drain(master: &OwnedFd) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    let master = dup(master);
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = sink.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match rustix::io::read(&master, &mut buf[..]) {
                Ok(n) if n > 0 => out.lock().unwrap().extend_from_slice(&buf[..n]),
                _ => return,
            }
        }
    });
    sink
}

#[allow(dead_code)]
fn read_master(master: &OwnedFd, want: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while out.len() < want && std::time::Instant::now() < deadline {
        let mut fds = [rustix::event::PollFd::new(
            master,
            rustix::event::PollFlags::IN,
        )];
        let t = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        if rustix::event::poll(&mut fds, Some(&t)).unwrap() == 0 {
            continue;
        }
        let mut buf = [0u8; 256];
        match rustix::io::read(master, &mut buf[..]) {
            Ok(n) if n > 0 => out.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    out
}

async fn next(input: &mut impl TermInput) -> TermEvent {
    tokio::time::timeout(Duration::from_secs(5), input.next())
        .await
        .expect("an event")
}

#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn raw_mode_is_entered_and_restored() {
    let p = pty();
    let before = tcgetattr(&p.slave).unwrap();
    assert!(is_cooked(&before));
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), Some("xterm".into())).unwrap();
    assert_eq!(term.local_term.as_deref(), Some("xterm"));

    term.output.set_raw(true).unwrap();
    let raw = tcgetattr(&p.slave).unwrap();
    assert!(
        !raw.local_modes
            .intersects(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
    );

    term.output.set_raw(false).unwrap();
    assert_eq!(lflags(&p.slave), before.local_modes);

    // Dropping the output restores it too.
    term.output.set_raw(true).unwrap();
    drop(term.output);
    assert_eq!(lflags(&p.slave), before.local_modes);
}

#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn input_is_read_only_in_raw_mode_and_output_is_written_exactly() {
    let p = pty();
    let seen = drain(&p.master);
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    term.output.set_raw(true).unwrap();
    rustix::io::write(&p.master, b"hello").unwrap();
    assert_eq!(
        next(&mut term.input).await,
        TermEvent::Input(b"hello".to_vec())
    );
    // Raw: Ctrl-C is a byte, not a signal, and nothing is echoed back.
    rustix::io::write(&p.master, b"\x03").unwrap();
    assert_eq!(next(&mut term.input).await, TermEvent::Input(vec![3]));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen.lock().unwrap().as_slice(), b"", "no echo in raw mode");

    // With raw mode off, input is left for the shell: fil's reader does not read it.
    term.output.set_raw(false).unwrap();
    rustix::io::write(&p.master, b"later\n").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), term.input.next())
            .await
            .is_err(),
        "the reader must not eat the shell's input"
    );
    // Back in raw mode the queued line is delivered: not lost, not read early.
    term.output.set_raw(true).unwrap();
    assert_eq!(
        next(&mut term.input).await,
        TermEvent::Input(b"later\n".to_vec())
    );

    // Output: bytes written exactly, including escapes and large writes.
    seen.lock().unwrap().clear();
    let big: Vec<u8> = (0..4000u32).map(|i| b'a' + (i % 26) as u8).collect();
    term.output.write_all(b"\x1b[31mred\x1b[0m").unwrap();
    term.output.write_all(&big).unwrap();
    let mut want = b"\x1b[31mred\x1b[0m".to_vec();
    want.extend_from_slice(&big);
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= want.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The tty's output processing is off in raw mode, so nothing is translated.
    assert_eq!(*seen.lock().unwrap(), want);
}

#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn window_size_changes_and_signals_become_events() {
    let p = pty();
    let _seen = drain(&p.master);
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    tcsetwinsize(
        &p.master,
        Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    assert_eq!(term.output.size().unwrap(), (100, 30));
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::WINCH)
        .unwrap();
    assert_eq!(
        next(&mut term.input).await,
        TermEvent::Resize {
            cols: 100,
            rows: 30
        }
    );
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
    assert_eq!(next(&mut term.input).await, TermEvent::Signal(15));
}

#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn hangup_is_end_of_input() {
    let p = pty();
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    term.output.set_raw(true).unwrap();
    let Pty { master, slave } = p;
    drop(master);
    assert_eq!(next(&mut term.input).await, TermEvent::Eof);
    drop(slave);
}

#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn a_panic_puts_the_terminal_back_before_unwinding_reaches_anyone() {
    let p = pty();
    let before = tcgetattr(&p.slave).unwrap();
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    term.output.set_raw(true).unwrap();
    assert!(!is_cooked(&tcgetattr(&p.slave).unwrap()));
    // Leak the output so only the panic hook, not a destructor, can restore the terminal.
    std::mem::forget(term.output);
    let r = std::panic::catch_unwind(|| panic!("boom"));
    assert!(r.is_err());
    assert_eq!(lflags(&p.slave), before.local_modes);
    // ...and it also switched off what a program may have left on, in a form that is invisible
    // when it is already off. (Not the alternate screen or the kitty stack: see the hook.)
    let got = read_master(&p.master, 100);
    let text = String::from_utf8_lossy(&got).into_owned();
    for want in [
        "\x18", "?1000l", "?1006l", "?1004l", "?2004l", "?1l", "\x1b>", "?25h",
    ] {
        assert!(text.contains(want), "{want:?} missing from {text:?}");
    }
    assert!(!text.contains("?1049"), "{text:?}");
}

/// A terminal that has stopped reading (its queue is full and nobody drains the master) cannot
/// hold the end of a run: the bounded write gives up at its deadline and the termios is restored
/// anyway. The plain `write_all` on the same descriptor blocks for good.
#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn a_terminal_that_stopped_reading_cannot_hold_the_exit_writes() {
    let p = pty();
    let before = tcgetattr(&p.slave).unwrap();
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    term.output.set_raw(true).unwrap();
    assert!(!is_cooked(&tcgetattr(&p.slave).unwrap()));

    // Fill the terminal's output queue through a second, non-blocking open of the same slave.
    let name = ptsname(&p.master, Vec::new()).unwrap();
    let filler = open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .unwrap();
    let chunk = [b'.'; 1024];
    let mut filled = 0usize;
    loop {
        match rustix::io::write(&filler, &chunk) {
            Ok(n) => filled += n,
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("{e}"),
        }
        assert!(filled < 64 << 20, "the queue never filled");
    }

    let started = std::time::Instant::now();
    let err = term
        .output
        .write_all_within(b"\x1b[?25h\r\ndetached\r\n", Duration::from_millis(300))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    // The termios does not wait for the queue, so it is back at once.
    term.output.set_raw(false).unwrap();
    assert_eq!(lflags(&p.slave), before.local_modes);

    // When the terminal reads again, a bounded write goes through whole.
    let sink = drain(&p.master);
    term.output
        .write_all_within(b"after\r\n", Duration::from_secs(5))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        String::from_utf8_lossy(&sink.lock().unwrap()).contains("after"),
        "a bounded write on a terminal that reads was lost"
    );
}

/// No window size on the terminal (a fresh pty reports 0x0), or one that cannot be read, is an
/// error from `size()` and no `Resize` event: the client keeps the size it has instead of
/// treating a made-up 80x24 as the user resizing the window.
#[tokio::test]
#[ignore = "on demand: just client-pty"]
async fn an_unreadable_window_size_is_an_error_and_no_resize_event() {
    let p = pty();
    let _seen = drain(&p.master);
    let mut term = from_fds(dup(&p.slave), dup(&p.slave), None).unwrap();
    assert!(
        term.output.size().is_err(),
        "an empty size was reported as {:?}",
        term.output.size()
    );
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::WINCH)
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), term.input.next())
            .await
            .is_err(),
        "a SIGWINCH with no readable size became an event"
    );
    // A real size is reported again, by `size()` and by the event.
    tcsetwinsize(
        &p.master,
        Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    assert_eq!(term.output.size().unwrap(), (100, 30));
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::WINCH)
        .unwrap();
    assert_eq!(
        next(&mut term.input).await,
        TermEvent::Resize {
            cols: 100,
            rows: 30
        }
    );
}
