//! The forwards' tunables as a config file writes them (DESIGN.md section 14): the client's
//! `[defaults.forward]` and the daemon's `daemon.toml` `[forward]` share these keys, their forms and
//! their bounds.
//!
//! ```toml
//! window = "1MiB"     # 16KiB to 4MiB: the receive window this side advertises per channel
//! buffer = "1MiB"     # 16KiB to 16MiB: what a channel's sender keeps until the peer consumed it
//! open_wait = "30s"   # 0 to 1h: how long a new connection waits for a link
//! linger = "10m"      # 0 to 24h: how long open connections outlive a disconnect, and how long
//!                     # the daemon keeps a withheld entry after its run (at least 30 s)
//! ```
//!
//! A size is a number of bytes or text (`"64KiB"`, `"1 MiB"`, `"8k"`; powers of 1024); a duration
//! is a number of seconds or text (`"30s"`, `"10m"`, `"1500ms"`, `"2h"`).

use std::time::Duration;

use fil_proto::limits::{DEFAULT_FORWARD_BUFFER, FORWARD_BUDGET, LINGER, MIN_WINDOW, OPEN_WAIT};
use fil_proto::mux::{MuxConfig, Window};
use serde::Deserialize;

/// The longest `open_wait`.
pub const MAX_OPEN_WAIT: Duration = Duration::from_secs(3600);
/// The longest `linger`.
pub const MAX_LINGER: Duration = Duration::from_secs(24 * 3600);

/// One tunable's value as written: a number, or text with a unit.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Int(u64),
    Text(String),
}

/// The tunables, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTunables {
    pub window: Window,
    pub buffer: usize,
    pub open_wait: Duration,
    pub linger: Duration,
}

impl Default for ForwardTunables {
    fn default() -> Self {
        Self {
            window: Window::DEFAULT,
            buffer: DEFAULT_FORWARD_BUFFER,
            open_wait: OPEN_WAIT,
            linger: LINGER,
        }
    }
}

impl ForwardTunables {
    /// The MUX configuration with these tunables over the defaults.
    pub fn mux_config(&self) -> MuxConfig {
        MuxConfig {
            window: self.window,
            buffer: self.buffer,
            open_wait: self.open_wait,
            linger: self.linger,
            ..MuxConfig::default()
        }
    }

    /// Sets the keys that are given, each checked against its bounds. An error names the key
    /// (`forward.window`, ...) and what it accepts.
    pub fn apply(
        &mut self,
        window: Option<&Value>,
        buffer: Option<&Value>,
        open_wait: Option<&Value>,
        linger: Option<&Value>,
    ) -> Result<(), String> {
        if let Some(v) = window {
            let n = size("window", v)?;
            self.window = u32::try_from(n)
                .ok()
                .and_then(|n| Window::new(n).ok())
                .ok_or_else(|| format!("forward.window = {n}: expected 16KiB to 4MiB"))?;
        }
        if let Some(v) = buffer {
            let n = size("buffer", v)?;
            let (lo, hi) = (MIN_WINDOW as usize, FORWARD_BUDGET / 4);
            if !(lo..=hi).contains(&n) {
                return Err(format!("forward.buffer = {n}: expected 16KiB to 16MiB"));
            }
            self.buffer = n;
        }
        if let Some(v) = open_wait {
            let d = duration(v).map_err(|e| format!("forward.open_wait: {e}"))?;
            if d > MAX_OPEN_WAIT {
                return Err("forward.open_wait: expected 0 to 1h".to_string());
            }
            self.open_wait = d;
        }
        if let Some(v) = linger {
            let d = duration(v).map_err(|e| format!("forward.linger: {e}"))?;
            if d > MAX_LINGER {
                return Err("forward.linger: expected 0 to 24h".to_string());
            }
            self.linger = d;
        }
        Ok(())
    }
}

fn size(key: &str, v: &Value) -> Result<usize, String> {
    match v {
        Value::Int(n) => usize::try_from(*n).map_err(|_| format!("forward.{key} = {n}: too large")),
        Value::Text(t) => parse_byte_size(t)
            .map_err(|e| format!("forward.{key}: {e} (examples: \"1MiB\", \"64KiB\")")),
    }
}

fn duration(v: &Value) -> Result<Duration, String> {
    match v {
        Value::Int(n) => Ok(Duration::from_secs(*n)),
        Value::Text(t) => parse_duration(t),
    }
}

/// A size such as `"4KiB"`, `"0"`, `"512"`, `"2 MiB"`, `"8k"` (powers of 1024).
pub fn parse_byte_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: usize = num.parse().map_err(|_| format!("not a size: {s:?}"))?;
    let mult: usize = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        _ => return Err(format!("unknown size unit in {s:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("size too large: {s:?}"))
}

/// A duration such as `"30s"`, `"10m"`, `"1500ms"`, `"2h"` (whole numbers; no unit is seconds).
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let n: u64 = num.parse().map_err(|_| {
        format!("not a duration: {text:?} (whole numbers; examples: \"30s\", \"10m\")")
    })?;
    let secs = |mult: u64| {
        n.checked_mul(mult)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("duration too large: {text:?}"))
    };
    match unit.trim() {
        "ms" => Ok(Duration::from_millis(n)),
        "s" | "" => secs(1),
        "m" => secs(60),
        "h" => secs(3600),
        _ => Err(format!(
            "unknown duration unit in {text:?} (use ms, s, m or h)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn every_key_and_its_bounds() {
        let mut t = ForwardTunables::default();
        t.apply(
            Some(&text("256KiB")),
            Some(&Value::Int(2 * 1024 * 1024)),
            Some(&text("5s")),
            Some(&Value::Int(120)),
        )
        .unwrap();
        assert_eq!(t.window.get(), 256 * 1024);
        assert_eq!(t.buffer, 2 * 1024 * 1024);
        assert_eq!(t.open_wait, Duration::from_secs(5));
        assert_eq!(t.linger, Duration::from_secs(120));
        let m = t.mux_config();
        assert_eq!(
            (m.window, m.buffer, m.linger),
            (t.window, t.buffer, t.linger)
        );

        for (w, b, o, l, want) in [
            (Some(text("1KiB")), None, None, None, "forward.window"),
            (Some(text("8MiB")), None, None, None, "forward.window"),
            (None, Some(Value::Int(1)), None, None, "forward.buffer"),
            (None, Some(text("1 parsec")), None, None, "forward.buffer"),
            (None, None, Some(text("2d")), None, "forward.open_wait"),
            (None, None, Some(text("2h")), None, "forward.open_wait"),
            (None, None, None, Some(text("48h")), "forward.linger"),
        ] {
            let e = ForwardTunables::default()
                .apply(w.as_ref(), b.as_ref(), o.as_ref(), l.as_ref())
                .unwrap_err();
            assert!(e.starts_with(want), "{e}");
        }
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_byte_size("4KiB").unwrap(), 4096);
        assert_eq!(parse_byte_size("0").unwrap(), 0);
        assert_eq!(parse_byte_size(" 2 MiB ").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_byte_size("512b").unwrap(), 512);
        assert!(parse_byte_size("4 parsecs").is_err());
        assert!(parse_byte_size("KiB").is_err());
        assert_eq!(
            parse_duration("1500ms").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
        assert!(parse_duration("ten").is_err());
        assert!(parse_duration("1d").is_err());
    }
}
