//! The client's half of the bootstrap's exchange (`docs/m4/plan.md` §3 U1): the mode line, the
//! reading of what the script prints, and the upload in lockstep. Over any byte stream, so the
//! tests drive it against the real script in a scratch `HOME` and against fakes.
//!
//! **Gated on M4 OQ-1**: compiled only with the `install-bootstrap` feature.
//!
//! The rules it keeps:
//!
//! - **The mode line first, and nothing else** until the script has answered: the binary is
//!   never written anywhere but to the script, which has greeted first.
//! - **Noise before the answer is skipped** (an rc file's output), within the preamble's bound.
//! - **A preamble with no `FIL-BOOT1 exec` before it** means a forced command ran `fil server
//!   --stdio` in place of the bootstrap: the exchange ends at once, sending nothing more.
//! - **Lockstep:** after the bytes, nothing is sent until the result line has been read, so the
//!   script's `head -c` cannot read past them (macOS's reads through stdio).
//! - Everything the remote says is untrusted text, shown only sanitized.

use std::time::Duration;

use fil_proto::limits::{MAX_PREAMBLE_SKIP, PROTOCOL_MAJOR, PROTOCOL_REVISION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::gc::Report;
use super::upload::Upload;

/// The line a marker starts with.
const MARK: &str = "FIL-BOOT1 ";
/// The start of fil's pipe preamble (`fil_proto::preamble`).
const PREAMBLE: &[u8] = b"\x1bFIL-CONNECT ";
/// The longest line read; longer ones are noise and are cut.
const MAX_LINE: usize = 4096;
/// How long `fil install --check` may take, its login included (a wrapper on `PATH` may hang).
pub const CHECK_LIMIT: Duration = Duration::from_secs(120);

/// The first line the client writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Attach,
    Check,
    Remove,
}

impl Mode {
    fn line(self) -> &'static [u8] {
        match self {
            Mode::Attach => b"attach\n",
            Mode::Check => b"check\n",
            Mode::Remove => b"remove\n",
        }
    }
}

/// What went wrong in an exchange.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BootError {
    #[error(
        "this host ran `fil server --stdio` in place of fil's install command (a forced command allows only the plain one): install fil there by hand, or use remote = \"plain\""
    )]
    Forced,
    #[error(
        "the host has neither sha256sum nor shasum, which the install needs: install fil there by hand"
    )]
    NoHashTool,
    #[error(
        "the install directory or one of its parents on the host is not safe to use (owned by someone else, writable by others, a symbolic link, or with an ACL): nothing was installed"
    )]
    BadDirectory,
    #[error("the upload arrived damaged (its SHA-256 did not match): nothing was installed")]
    BadHash,
    #[error("the installed fil refused its arguments: nothing was installed")]
    BadArgument,
    #[error("the installed fil speaks wire {0}, not {PROTOCOL_MAJOR}.{PROTOCOL_REVISION}")]
    WrongWire(String),
    #[error(
        "fil {} is not installed on the host: run --remove with that version of fil",
        super::layout::this_version_dir()
    )]
    NotInstalled,
    #[error(
        "the host ended the install without a result{0}: its home may be read-only or mounted noexec, or the login ended (ssh's message, if any, is above)"
    )]
    Ended(String),
    #[error("the host printed too much before fil's install command answered")]
    TooMuchNoise,
    #[error("timed out waiting for the host")]
    Timeout,
    #[error("the pipe failed: {0}")]
    Io(String),
}

fn io(e: std::io::Error) -> BootError {
    BootError::Io(e.to_string())
}

/// Lines from the script, with what came after the last one kept for whoever reads on (the
/// pipe's preamble scanner, after `FIL-BOOT1 exec`).
pub struct Lines<R> {
    inner: R,
    buf: Vec<u8>,
    /// Bytes skipped as noise so far.
    noise: usize,
}

/// One thing the script said.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Said {
    /// A `FIL-BOOT1 ...` line, without the prefix.
    Mark(String),
    /// A `FIL-GC ...` or `FIL-HAVE ...` (or other `FIL-`) line, whole.
    Fil(String),
    /// fil's pipe preamble began.
    Preamble,
    /// Anything else (rc-file noise).
    Noise,
    /// The stream ended.
    End,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    pub fn new(inner: R) -> Self {
        Lines {
            inner,
            buf: Vec::new(),
            noise: 0,
        }
    }

    /// What is buffered beyond the lines read, and the reader.
    pub fn into_parts(self) -> (Vec<u8>, R) {
        (self.buf, self.inner)
    }

    async fn next(&mut self) -> Result<Said, BootError> {
        loop {
            if let Some(at) = find(&self.buf, PREAMBLE) {
                let before_nl = self.buf[..at].iter().rposition(|&b| b == b'\n');
                // A complete line before the preamble is read first; the preamble itself is not
                // consumed (it belongs to the pipe).
                if before_nl.is_none() {
                    return Ok(Said::Preamble);
                }
            }
            if let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=nl).collect();
                let text = String::from_utf8_lossy(&line[..nl]).into_owned();
                let text = text.strip_suffix('\r').unwrap_or(&text).to_string();
                if let Some(rest) = text.strip_prefix(MARK) {
                    return Ok(Said::Mark(rest.to_string()));
                }
                if text.starts_with("FIL-") {
                    return Ok(Said::Fil(text));
                }
                self.noise += line.len();
                if self.noise > MAX_PREAMBLE_SKIP {
                    return Err(BootError::TooMuchNoise);
                }
                return Ok(Said::Noise);
            }
            if self.buf.len() > MAX_LINE {
                // A line too long to be ours: noise; keep its tail in case a marker follows.
                let cut = self.buf.len() - PREAMBLE.len();
                self.noise += cut;
                self.buf.drain(..cut);
                if self.noise > MAX_PREAMBLE_SKIP {
                    return Err(BootError::TooMuchNoise);
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk).await.map_err(io)?;
            if n == 0 {
                return Ok(Said::End);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What the script answered to `attach`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Greeted {
    /// `FIL-BOOT1 exec`: an installed copy of this wire is starting; the pipe continues.
    Exec,
    /// `FIL-BOOT1 attach <uname -s> <uname -m>`: no usable copy; an upload may follow.
    Greeting { sys: String, machine: String },
}

/// Sends the mode line.
pub async fn send_mode<W: AsyncWrite + Unpin>(w: &mut W, mode: Mode) -> Result<(), BootError> {
    w.write_all(mode.line()).await.map_err(io)?;
    w.flush().await.map_err(io)
}

/// Reads the answer to `attach`.
pub async fn read_greeting<R: AsyncRead + Unpin>(r: &mut Lines<R>) -> Result<Greeted, BootError> {
    loop {
        match r.next().await? {
            Said::Mark(m) if m == "exec" => return Ok(Greeted::Exec),
            Said::Mark(m) if m.starts_with("attach ") => {
                let mut parts = m.split(' ').skip(1);
                let sys = parts.next().unwrap_or_default();
                let machine = parts.next().unwrap_or_default();
                return Ok(Greeted::Greeting {
                    sys: fil_proto::text::sanitize_inline(sys),
                    machine: fil_proto::text::sanitize_inline(machine),
                });
            }
            Said::Preamble => return Err(BootError::Forced),
            Said::End => return Err(BootError::Ended(String::new())),
            Said::Mark(_) | Said::Fil(_) | Said::Noise => {}
        }
    }
}

/// What to send after a successful install: `go` (start the installed `fil server --stdio` on
/// this login) or `done` (`fil install`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    Go,
    Done,
}

/// Sends `put <size> <sha256>` and the bytes, then reads the result in lockstep and answers it.
/// Returns the collection's report from the host, if it sent one.
pub async fn upload<R, W>(
    r: &mut Lines<R>,
    w: &mut W,
    up: &Upload,
    then: Then,
) -> Result<Option<Report>, BootError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let header = format!("put {} {}\n", up.bytes.len(), up.sha256);
    // A script that refuses early (no hash tool, a bad directory) stops reading: the write fails,
    // and its answer is still there to read.
    let sent = async {
        w.write_all(header.as_bytes()).await?;
        w.write_all(&up.bytes).await?;
        w.flush().await
    }
    .await;
    read_result(r, w, then, sent.err()).await
}

/// The lockstep half of [`upload`]: reads the result line of an upload whose bytes have been
/// sent, and only then answers it. `send_error` is why sending stopped early, if it did.
pub async fn read_result<R, W>(
    r: &mut Lines<R>,
    w: &mut W,
    then: Then,
    send_error: Option<std::io::Error>,
) -> Result<Option<Report>, BootError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut report = None;
    loop {
        match r.next().await? {
            Said::Fil(l) if l.starts_with("FIL-GC ") => report = parse_report(&l),
            Said::Mark(m) => {
                let refusal = match m.as_str() {
                    "nohash" => BootError::NoHashTool,
                    "baddir" => BootError::BadDirectory,
                    "badhash" => BootError::BadHash,
                    "badarg" => BootError::BadArgument,
                    _ => {
                        if let Some(wire) = m.strip_prefix("ok ") {
                            let ours = format!("{PROTOCOL_MAJOR}.{PROTOCOL_REVISION}");
                            if wire != ours {
                                let _ = w.write_all(b"done\n").await;
                                return Err(BootError::WrongWire(
                                    fil_proto::text::sanitize_inline(wire),
                                ));
                            }
                            let answer: &[u8] = match then {
                                Then::Go => b"go\n",
                                Then::Done => b"done\n",
                            };
                            w.write_all(answer).await.map_err(io)?;
                            w.flush().await.map_err(io)?;
                            return Ok(report);
                        }
                        continue;
                    }
                };
                return Err(refusal);
            }
            Said::Preamble => return Err(BootError::Forced),
            Said::End => {
                let why = match &send_error {
                    Some(e) => format!(" (sending failed: {e})"),
                    None => String::new(),
                };
                return Err(BootError::Ended(why));
            }
            Said::Fil(_) | Said::Noise => {}
        }
    }
}

/// Parses `FIL-GC removed=N failed=N recent=N far-future=N in-use=N`.
pub fn parse_report(line: &str) -> Option<Report> {
    let mut r = Report::default();
    for field in line.strip_prefix("FIL-GC ")?.split(' ') {
        let (k, v) = field.split_once('=')?;
        let v: usize = v.parse().ok()?;
        match k {
            "removed" => r.removed = v,
            "failed" => r.failed = v,
            "recent" => r.recent = v,
            "far-future" => r.far_future = v,
            "in-use" => r.in_use = v,
            _ => {}
        }
    }
    Some(r)
}

/// What `check` found on the host. Sanitized, one line each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// The fil-installed copies that pass the checks (their directory names).
    pub installed: Vec<String>,
    /// `fil --version` of the fil on `PATH`.
    pub on_path: Option<String>,
    /// `--version` of `~/.local/bin/fil`, if it passes the checks.
    pub local_bin: Option<String>,
}

/// The most entries `check` reports.
const MAX_CHECK_LINES: usize = 64;
/// The longest text kept of one entry.
const MAX_CHECK_TEXT: usize = 200;

fn clean(text: &str) -> String {
    fil_proto::text::sanitize_inline(text)
        .chars()
        .take(MAX_CHECK_TEXT)
        .collect()
}

/// Reads `check`'s answer to the end.
pub async fn read_check<R: AsyncRead + Unpin>(r: &mut Lines<R>) -> Result<CheckReport, BootError> {
    let mut report = CheckReport::default();
    let mut lines = 0;
    loop {
        match r.next().await? {
            Said::End => return Ok(report),
            Said::Preamble => return Err(BootError::Forced),
            Said::Fil(l) => {
                lines += 1;
                if lines > MAX_CHECK_LINES {
                    continue;
                }
                if let Some(v) = l.strip_prefix("FIL-HAVE ") {
                    report.installed.push(clean(v));
                } else if let Some(v) = l.strip_prefix("FIL-PATH ") {
                    report.on_path = Some(clean(v));
                } else if let Some(v) = l.strip_prefix("FIL-LOCAL ") {
                    report.local_bin = Some(clean(v));
                }
            }
            Said::Mark(_) | Said::Noise => {}
        }
    }
}

/// Reads `remove`'s answer: the collection's report, or that this version is not installed.
pub async fn read_remove<R: AsyncRead + Unpin>(r: &mut Lines<R>) -> Result<Report, BootError> {
    let mut handed_over = false;
    loop {
        match r.next().await? {
            Said::Mark(m) if m == "gc" => handed_over = true,
            Said::Mark(m) if m == "none" => return Err(BootError::NotInstalled),
            Said::Mark(m) if m == "baddir" => return Err(BootError::BadDirectory),
            Said::Fil(l) if handed_over && l.starts_with("FIL-GC ") => {
                return parse_report(&l).ok_or(BootError::Ended(" (an unreadable report)".into()));
            }
            Said::Preamble => return Err(BootError::Forced),
            Said::End => return Err(BootError::Ended(String::new())),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[u8]) -> Lines<&[u8]> {
        Lines::new(text)
    }

    #[tokio::test]
    async fn the_greeting_and_the_marker_are_found_past_rc_file_noise() {
        let mut r = lines(b"welcome back\r\n\x1b[1mbold\x1b[0m\nFIL-BOOT1 attach Linux x86_64\n");
        assert_eq!(
            read_greeting(&mut r).await,
            Ok(Greeted::Greeting {
                sys: "Linux".into(),
                machine: "x86_64".into()
            })
        );
        let mut r = lines(b"motd\nFIL-BOOT1 exec\n\x1bFIL-CONNECT 1\nframes");
        assert_eq!(read_greeting(&mut r).await, Ok(Greeted::Exec));
        let (rest, _) = r.into_parts();
        assert_eq!(
            rest, b"\x1bFIL-CONNECT 1\nframes",
            "the preamble is left for the pipe"
        );
    }

    #[tokio::test]
    async fn a_preamble_with_no_marker_is_a_forced_command() {
        for text in [
            &b"\x1bFIL-CONNECT 1\n"[..],
            b"noise\n\x1bFIL-CONNECT 1\n",
            b"noise without a newline\x1bFIL-CONNECT 1\n",
        ] {
            assert_eq!(
                read_greeting(&mut lines(text)).await,
                Err(BootError::Forced)
            );
        }
    }

    #[tokio::test]
    async fn the_greeting_is_sanitized_and_endless_noise_is_refused() {
        let mut r = lines(b"FIL-BOOT1 attach Lin\x1b]0;x\x07ux x86_64\n");
        let Ok(Greeted::Greeting { sys, .. }) = read_greeting(&mut r).await else {
            panic!()
        };
        assert!(!sys.contains('\x1b') && !sys.contains('\x07'), "{sys:?}");
        let noise = vec![b'x'; MAX_PREAMBLE_SKIP + 10_000];
        assert_eq!(
            read_greeting(&mut lines(&noise)).await,
            Err(BootError::TooMuchNoise)
        );
        assert_eq!(
            read_greeting(&mut lines(b"no answer\n")).await,
            Err(BootError::Ended(String::new()))
        );
    }

    fn upload_of(bytes: &[u8]) -> Upload {
        Upload {
            bytes: bytes.to_vec(),
            sha256: super::super::remote::hex(
                ring::digest::digest(&ring::digest::SHA256, bytes).as_ref(),
            ),
            source: super::super::upload::Source::Itself,
        }
    }

    #[tokio::test]
    async fn the_upload_is_the_header_then_the_bytes_then_the_answer_after_the_result() {
        let ok = format!(
            "FIL-GC removed=2 failed=0 recent=1 far-future=0 in-use=0\nFIL-BOOT1 ok {PROTOCOL_MAJOR}.{PROTOCOL_REVISION}\n"
        );
        let mut r = lines(ok.as_bytes());
        let mut sent = Vec::new();
        let up = upload_of(b"BINARY");
        let report = upload(&mut r, &mut sent, &up, Then::Done).await.unwrap();
        assert_eq!(report.unwrap().removed, 2);
        assert_eq!(
            sent,
            format!("put 6 {}\nBINARYdone\n", up.sha256).into_bytes()
        );

        let mut sent = Vec::new();
        upload(&mut lines(ok.as_bytes()), &mut sent, &up, Then::Go)
            .await
            .unwrap();
        assert!(sent.ends_with(b"BINARYgo\n"));
    }

    #[tokio::test]
    async fn refusals_and_a_wrong_wire_are_errors() {
        let up = upload_of(b"x");
        for (said, want) in [
            ("FIL-BOOT1 nohash\n", BootError::NoHashTool),
            ("FIL-BOOT1 baddir\n", BootError::BadDirectory),
            ("FIL-BOOT1 badhash\n", BootError::BadHash),
            ("", BootError::Ended(String::new())),
        ] {
            let mut sent = Vec::new();
            assert_eq!(
                upload(&mut lines(said.as_bytes()), &mut sent, &up, Then::Go).await,
                Err(want)
            );
            assert!(!sent.ends_with(b"go\n"), "nothing is answered to a refusal");
        }
        let mut sent = Vec::new();
        let r = upload(&mut lines(b"FIL-BOOT1 ok 9.99\n"), &mut sent, &up, Then::Go).await;
        assert_eq!(r, Err(BootError::WrongWire("9.99".into())));
        assert!(
            sent.ends_with(b"done\n"),
            "a copy of another wire is never started"
        );
    }

    #[tokio::test]
    async fn check_and_remove_answers() {
        let mut r = lines(
            b"rc noise\nFIL-HAVE 0.1.0-w1.12\nFIL-HAVE x\x1b[2Jy\nFIL-PATH fil 0.0.9 (wire 1.3)\n",
        );
        let c = read_check(&mut r).await.unwrap();
        assert_eq!(c.installed, ["0.1.0-w1.12", "x[2Jy"]);
        assert_eq!(c.on_path.as_deref(), Some("fil 0.0.9 (wire 1.3)"));
        assert_eq!(c.local_bin, None);

        let mut r =
            lines(b"FIL-BOOT1 gc\nFIL-GC removed=1 failed=0 recent=0 far-future=0 in-use=1\n");
        let rep = read_remove(&mut r).await.unwrap();
        assert_eq!((rep.removed, rep.in_use), (1, 1));
        assert_eq!(
            read_remove(&mut lines(b"FIL-BOOT1 none\n")).await,
            Err(BootError::NotInstalled)
        );
        // A report before the hand-over is not the installed copy's.
        assert_eq!(
            read_remove(&mut lines(
                b"FIL-GC removed=9 failed=0 recent=0 far-future=0 in-use=0\n"
            ))
            .await,
            Err(BootError::Ended(String::new()))
        );
    }
}
