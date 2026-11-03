//! `sockprobe PATH`: what happens to a stranger who connects to a Unix socket.
//!
//! Connects to PATH, then waits up to 3 s for the other side to speak or hang up. Prints one
//! line: `REFUSED errno=... msg=...` (the connect failed), `CLOSED` (the peer hung up at once,
//! which is how the fil daemon treats a connection from another user), `DATA bytes=N` or
//! `WAITING` (the peer accepted us and is waiting for us to say something: the daemon's answer
//! to its own user). The realnet suite runs it as a second Unix user.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: sockprobe PATH");
        return ExitCode::from(2);
    };
    let mut s = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "REFUSED errno={} msg={}",
                e.raw_os_error().unwrap_or(0),
                e.to_string().replace(' ', "_")
            );
            return ExitCode::SUCCESS;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 256];
    match s.read(&mut buf) {
        Ok(0) => println!("CLOSED"),
        Ok(n) => println!("DATA bytes={n}"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            println!("WAITING");
        }
        Err(e) => println!("CLOSED reset={e}"),
    }
    ExitCode::SUCCESS
}
