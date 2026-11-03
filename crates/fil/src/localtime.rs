//! This machine's offset from UTC, for the times of day in `fil ports`.
//!
//! The standard library has no time zone and the workspace has no date crate, so this asks the C
//! library (`localtime_r`, which reads `TZ` and the system zone database as every other program on
//! the machine does). It is the one `unsafe` call in the `fil` crate: one function, a pointer to a
//! local `time_t` and one to a local `tm`. Anything the C library cannot answer is UTC.

/// The most an offset can be: no zone is further than UTC+14 or UTC-12, and a nonsense value
/// from a broken zone database must not skew every time on the screen.
const MAX_OFFSET_SECS: i64 = 15 * 3600;

/// Seconds east of UTC that this machine's zone is at `unix_secs` (daylight saving included), or
/// 0 when the zone cannot be read.
#[allow(unsafe_code)]
pub fn utc_offset_secs(unix_secs: i64) -> i32 {
    // `time_t` is 64 bits on every platform fil runs on (Linux, macOS).
    #[allow(clippy::unnecessary_cast)]
    let t = unix_secs as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `t` and `tm` are live locals; `localtime_r` writes only to `tm` (it is the
    // re-entrant form, with no static buffer) and returns null, leaving `tm` zeroed, on failure.
    let filled = unsafe { !libc::localtime_r(&t, tm.as_mut_ptr()).is_null() };
    if !filled {
        return 0;
    }
    // SAFETY: zero-initialized above, and `localtime_r` succeeded.
    let tm = unsafe { tm.assume_init() };
    // `c_long`: 64 bits wherever `time_t` is.
    let off = tm.tm_gmtoff;
    off.clamp(-MAX_OFFSET_SECS, MAX_OFFSET_SECS) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_is_a_plausible_zone() {
        // Whatever zone this machine is in, it is a real one.
        let off = utc_offset_secs(1_700_000_000);
        assert!((-12 * 3600..=14 * 3600).contains(&off), "{off}");
        // And a time the platform cannot represent is UTC, not a panic.
        assert_eq!(utc_offset_secs(i64::MAX), 0);
    }
}
