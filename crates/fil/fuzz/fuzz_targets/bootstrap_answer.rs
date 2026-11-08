//! The answer of the remote host's install bootstrap (`fil::install::boot`), which comes from a
//! host that fil has not authenticated: arbitrary bytes, delivered in pieces of any size, through
//! the line reader and each of the readers built on it (the greeting, the upload result, `check`'s
//! report and `remove`'s). None may panic, and what `check` reports stays within its bounds and
//! free of control characters. The first byte picks the piece size, the second the reader.

#![no_main]

use std::pin::Pin;
use std::task::{Context, Poll};

use fil::install::boot::{self, Lines, Then};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncRead, ReadBuf};

/// A reader that hands out at most `step` bytes per read.
struct Pieces<'a> {
    data: &'a [u8],
    step: usize,
}

impl AsyncRead for Pieces<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let n = self.data.len().min(self.step).min(buf.remaining());
        buf.put_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Poll::Ready(Ok(()))
    }
}

// `MAX_CHECK_LINES` and `MAX_CHECK_TEXT` in boot.rs (private).
const MAX_CHECK_LINES: usize = 64;
const MAX_CHECK_TEXT: usize = 200;

fn bounded(text: &str) {
    assert!(text.chars().count() <= MAX_CHECK_TEXT);
    assert!(!text.chars().any(char::is_control));
}

fuzz_target!(|data: &[u8]| {
    let [step, which, input @ ..] = data else {
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut lines = Lines::new(Pieces {
        data: input,
        step: usize::from(*step).max(1),
    });
    rt.block_on(async {
        let mut sink = tokio::io::sink();
        match which % 6 {
            0 => {
                if let Ok(boot::Greeted::Greeting { sys, machine }) =
                    boot::read_greeting(&mut lines).await
                {
                    assert!(!sys.chars().any(char::is_control));
                    assert!(!machine.chars().any(char::is_control));
                }
            }
            1 => {
                let _ = boot::read_result(&mut lines, &mut sink, Then::Go, None).await;
            }
            2 => {
                let _ = boot::read_result(&mut lines, &mut sink, Then::Done, None).await;
            }
            3 => {
                if let Ok(report) = boot::read_check(&mut lines).await {
                    assert!(report.installed.len() <= MAX_CHECK_LINES);
                    report.installed.iter().for_each(|t| bounded(t));
                    report.on_path.iter().for_each(|t| bounded(t));
                    report.local_bin.iter().for_each(|t| bounded(t));
                }
            }
            4 => {
                let _ = boot::read_remove(&mut lines).await;
            }
            _ => {
                if let Ok(text) = std::str::from_utf8(input) {
                    let _ = boot::parse_report(text);
                }
            }
        }
    });
});
