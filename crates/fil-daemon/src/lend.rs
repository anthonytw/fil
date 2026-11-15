//! Sharing the wire lock between the daemons of one wire (DESIGN.md section 5.3; the FROZEN
//! `docs/m4/release.md` section 6.4.4).
//!
//! The install's garbage collection keeps every version directory of a wire while
//! `fil-<major>-<rev>.lock` is held, and it reads "held" from a shared, non-blocking `flock`
//! failing: the lock must be held **exclusively**, by every daemon of the wire, for its whole
//! life. Daemons of several builds of one wire run side by side, and two processes cannot each
//! hold an exclusive `flock` on their own descriptors of one file. They can share **one open file
//! description**, whose lock is theirs together: `flock` locks belong to the open file
//! description, not the process or the descriptor, and the description lives (locked) until the
//! last descriptor that refers to it is closed, in whichever process.
//!
//! So the first daemon of a wire opens and locks the file, and every later one is handed a
//! descriptor of that very description, by `SCM_RIGHTS` over a Unix socket, the **lending
//! socket** (`fil-<major>-<rev>-v<ver>.lend`, one per running daemon, mode 0600, in the socket
//! directory). Every daemon of the wire then literally holds the lock, and nobody ever takes it
//! again: the one lock is held from the first daemon's start to the last one's exit, with no
//! instant between in which a probe finds it free.
//!
//! - **Never released by hand.** No daemon unlocks the file (`LOCK_UN` would release it for all
//!   of them); each only closes its descriptor when it exits. Every descriptor is close-on-exec
//!   (received with `MSG_CMSG_CLOEXEC` on Linux; on macOS, which has no such flag, set at once
//!   after the receive, and the session's child closes every stray descriptor before its `exec`
//!   anyway: a short-lived helper forked in that instant would only hold the lock a little longer).
//! - **A descriptor in flight holds the lock too.** Once the lender's `sendmsg` returns, the
//!   kernel holds a reference to the description until the borrower receives it (or the socket is
//!   closed unread), so a lender that exits right after sending leaves nothing unlocked.
//! - **Checked on arrival.** Both ends check that the peer is the same user (the kernel's peer
//!   credentials, as for the daemon's socket, DESIGN.md section 11), and the borrower keeps what
//!   it got only if it is a regular file of ours, is the file at the wire lock's path now, and a
//!   non-blocking exclusive `flock` on it succeeds: on a description that holds the lock that is a
//!   no-op, and on any other it either fails (somebody else holds the lock: not kept) or takes the
//!   lock, which also leaves this daemon holding the lock on the file at the path.
//! - **Only a holder lends.** A daemon lends only while its own descriptor is the file at the
//!   path; otherwise (the file was removed: its repair is under way) it hangs up, and the borrower
//!   asks another, or takes the new file's lock itself.

use std::fs::File;
use std::io;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use rustix::fs::{FileType, FlockOperation, flock, fstat};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::io::Interest;
use tokio::net::UnixStream;

use crate::paths::{Paths, current_uid};
use crate::start::{LockStep, is_current, lock_step, open_lock_file};

/// The one byte that carries the descriptor (a message with ancillary data needs a byte of data).
const LEND_BYTE: u8 = b'L';
/// How long one borrow (connect, peer check, receive) may take before the next lender is asked.
const BORROW_WAIT: Duration = Duration::from_secs(1);
/// How long a lender waits for its borrower to take the byte (it is one byte on a fresh
/// connection, so the buffer has room: this only bounds a peer that is not a daemon).
const LEND_WAIT: Duration = Duration::from_secs(1);

/// How a daemon came to hold the wire lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Got {
    /// It took the lock on the file itself: no daemon of the wire was running.
    Took,
    /// Another daemon of the wire lent it its descriptor.
    Borrowed,
}

/// Why the wire lock could not be had.
#[derive(Debug, thiserror::Error)]
pub enum WireLockError {
    /// The lock is held and no daemon lent it within the wait: a daemon from before daemons were
    /// keyed by build holds it alone (it has no lending socket), or one of the wire is wedged.
    #[error("the wire lock is held and no daemon of the wire lent it")]
    NotLent,
    #[error("the wire lock: {0}")]
    Io(#[from] io::Error),
}

/// Takes the wire lock ([`Paths::lock`]), or borrows it from a running daemon of another build of
/// the wire ([`crate::paths::lenders`]), trying both until `wait` runs out: a daemon that is
/// starting may not lend yet, and a lender that is exiting may release the lock (then it is
/// taken). The file is followed if it is replaced meanwhile ([`lock_step`]). Returns the
/// descriptor this daemon then keeps for its whole life (never unlocked, only closed).
pub async fn acquire(paths: &Paths, wait: Duration) -> Result<(File, Got), WireLockError> {
    let path = paths.lock();
    let mut file = open_lock_file(&path)?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match lock_step(&mut file, &path, || {})? {
            LockStep::Locked => return Ok((file, Got::Took)),
            LockStep::Replaced => continue,
            LockStep::Busy => {}
        }
        for lender in crate::paths::lenders(paths) {
            match borrow(&lender, &path).await {
                Ok(f) => return Ok((f, Got::Borrowed)),
                Err(e) => tracing::debug!(error = %e, "a lender did not lend"),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(WireLockError::NotLent);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Asks the daemon on the lending socket `sock` for its descriptor of the wire lock at `lock`,
/// and keeps it only if it is the locked file at that path (the checks in the
/// [module docs](self)).
pub async fn borrow(sock: &Path, lock: &Path) -> io::Result<File> {
    let file = tokio::time::timeout(BORROW_WAIT, receive(sock))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    verify(&file, lock)?;
    Ok(file)
}

/// Whether `file` (a descriptor that arrived) is the wire lock: a regular file of ours, the file
/// at `lock` now, and locked by this daemon once an exclusive try on it succeeds.
pub fn verify(file: &File, lock: &Path) -> io::Result<()> {
    let st = fstat(file)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile || st.st_uid != current_uid() {
        return Err(io::Error::other(
            "a lent descriptor that is not a lock file of ours",
        ));
    }
    if !is_current(file, lock) {
        return Err(io::Error::other(
            "a lent descriptor that is not the wire lock now",
        ));
    }
    flock(file, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| io::Error::other("a lent descriptor that does not hold the wire lock"))?;
    Ok(())
}

/// Connects to `sock`, checks the lender is the same user, and receives the descriptor.
async fn receive(sock: &Path) -> io::Result<File> {
    let stream = UnixStream::connect(sock).await?;
    check_peer(&stream)?;
    loop {
        stream.readable().await?;
        match stream.try_io(Interest::READABLE, || receive_now(&stream)) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            r => return r,
        }
    }
}

fn receive_now(stream: &UnixStream) -> io::Result<File> {
    let mut byte = [0u8; 1];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = RecvFlags::empty();
    let got = recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut byte)],
        &mut control,
        flags,
    )?;
    let mut fds = Vec::new();
    for msg in control.drain() {
        if let RecvAncillaryMessage::ScmRights(r) = msg {
            fds.extend(r);
        }
    }
    // Close-on-exec first (macOS: it could not be asked for at the receive), then the checks.
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    if got.bytes == 0 && fds.is_empty() {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    if byte[0] != LEND_BYTE || fds.len() != 1 {
        return Err(io::Error::other("an unexpected answer on a lending socket"));
    }
    Ok(File::from(fds.pop().expect("one descriptor")))
}

/// The peer of `stream` is this user (the kernel says so: `SO_PEERCRED` on Linux,
/// `LOCAL_PEERCRED` on macOS).
fn check_peer(stream: &UnixStream) -> io::Result<()> {
    match stream.peer_cred() {
        Ok(c) if c.uid() == current_uid() => Ok(()),
        Ok(_) => Err(io::Error::other("a lending socket peer of another user")),
        Err(e) => Err(e),
    }
}

/// Lends `lock` (this daemon's descriptor of the wire lock) to the daemon connected on `stream`,
/// after checking it is the same user (`allowed_uid`). The caller has checked that `lock` is the
/// file at the path.
pub async fn lend(stream: UnixStream, lock: &File, allowed_uid: u32) -> io::Result<()> {
    match stream.peer_cred() {
        Ok(c) if crate::daemon::peer_allowed(c.uid(), allowed_uid) => {}
        Ok(c) => {
            tracing::warn!(
                peer_uid = c.uid(),
                "refused a lending connection from another user"
            );
            return Err(io::Error::other("another user"));
        }
        Err(e) => return Err(e),
    }
    let send = async {
        loop {
            stream.writable().await?;
            match stream.try_io(Interest::WRITABLE, || send_now(&stream, lock)) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                r => return r,
            }
        }
    };
    tokio::time::timeout(LEND_WAIT, send)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

fn send_now(stream: &UnixStream, lock: &File) -> io::Result<()> {
    let fds = [lock.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("no room for the descriptor"));
    }
    let n = sendmsg(
        stream,
        &[IoSlice::new(&[LEND_BYTE])],
        &mut control,
        SendFlags::empty(),
    )?;
    if n != 1 {
        return Err(io::Error::from(io::ErrorKind::WriteZero));
    }
    Ok(())
}
