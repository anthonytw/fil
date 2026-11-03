//! Shared helpers for the real-network harness binaries (`tests/realnet`, `docs/m1/realnet.md`).
//!
//! Seven programs live here, all test-only and never shipped. The two drivers for the real `fil`
//! binary (`ptydrive`, a terminal; `sockprobe`, a stranger at a Unix socket) have their own docs;
//! the rest stand in for one end of a link:
//!
//! - `pipe-echo`: stands in for `fil server --stdio` (server mode, the default) and for the
//!   client side of the pipe (`pipe-echo client`). Real preamble and frame code from `fil-proto`.
//! - `quic-echo`: stands in for the daemon's QUIC endpoint and for the client, using the real
//!   `fil_transport::auth` configs. Also `udp-send`/`udp-recv`, a probe that measures reordering.
//! - `quiclink-probe`: the same two roles, built on the real `QuicLink` and `QuicListener`.
//! - `pipe-probe`: the same for the real `PipeLink`, on a pipe command.
//! - `pipe-detach`: the daemon side and a slow ssh stand-in for the detach-delivery test.
//!
//! Shared by the programs: [`keys`] (the key bootstrap of the two QUIC programs) and [`pty`] (a
//! program on a pseudo-terminal, shared by `ptydrive` and the Pi examples).
//!
//! Output is line-oriented text meant for `grep` in the bash harness: `KEY k=v k=v ...`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod keys;
pub mod pty;

/// Milliseconds since the Unix epoch. The harness subtracts these from its own `date +%s%3N`,
/// which works because all processes share the runner's clock.
pub fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Fill `buf` with deterministic pseudo-random data for absolute stream offset `offset`. Random
/// access lets a receiver check any chunk without keeping the whole stream.
pub fn fill_pattern(seed: u64, offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        let pos = offset + i as u64;
        let block = splitmix(seed ^ (pos / 8));
        *b = block.to_le_bytes()[(pos % 8) as usize];
    }
}

pub fn pattern(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0; len];
    fill_pattern(seed, offset, &mut v);
    v
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// Replace control characters so untrusted text is safe to print (the real client must do the
/// same with the preamble's skipped bytes, `fil-proto/src/preamble.rs`).
pub fn sanitize(bytes: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(max)])
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// `--flag value` and bare `--flag` options, plus the positional arguments after `--`.
#[derive(Debug, Default)]
pub struct Args {
    pub opts: HashMap<String, String>,
    pub rest: Vec<String>,
}

impl Args {
    /// Flags listed in `bare` take no value.
    pub fn parse(args: impl IntoIterator<Item = String>, bare: &[&str]) -> Result<Self, String> {
        let mut out = Args::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if a == "--" {
                out.rest.extend(it.by_ref());
                break;
            }
            let Some(name) = a.strip_prefix("--") else {
                return Err(format!("unexpected argument {a:?}"));
            };
            if bare.contains(&name) {
                out.opts.insert(name.to_owned(), String::new());
            } else {
                let v = it.next().ok_or(format!("--{name} needs a value"))?;
                out.opts.insert(name.to_owned(), v);
            }
        }
        Ok(out)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.opts.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.opts.get(name).map(String::as_str)
    }

    pub fn require(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or(format!("--{name} is required"))
    }

    pub fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| format!("--{name}: bad number {v:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_random_access() {
        let whole = pattern(7, 0, 100);
        assert_eq!(pattern(7, 13, 50), whole[13..63]);
        assert_ne!(pattern(8, 0, 100), whole);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(
            hex_decode(&hex_encode(&[0, 1, 254, 255])),
            Some(vec![0, 1, 254, 255])
        );
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn args_parse() {
        let a = Args::parse(
            ["--mode", "bulk", "--verbose", "--", "ssh", "-T"].map(String::from),
            &["verbose"],
        )
        .unwrap();
        assert_eq!(a.get("mode"), Some("bulk"));
        assert!(a.flag("verbose"));
        assert_eq!(a.rest, ["ssh", "-T"]);
        assert_eq!(a.num("n", 5u32).unwrap(), 5);
        assert!(Args::parse(["--mode".to_owned()], &[]).is_err());
    }

    #[test]
    fn sanitize_strips_controls() {
        assert_eq!(sanitize(b"a\x1b[31mb\n", 100), "a?[31mb?");
    }
}
