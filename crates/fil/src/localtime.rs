//! This machine's offset from UTC, for the times of day in `fil ports`.
//!
//! The standard library has no time zone and the workspace has no date crate, so this asks the C
//! library (`localtime_r`, which reads `TZ` and the system zone database as every other program on
//! the machine does). It is the one `unsafe` call in the `fil` crate: one function, a pointer to a
//! local `time_t` and one to a local `tm`. Anything the C library cannot answer is UTC.
//!
//! **Soundness condition: nothing in this process modifies the environment.** `localtime_r` is
//! thread-safe except against a concurrent `setenv`/`putenv`, which `std::env::set_var` and
//! `remove_var` would be (they are `unsafe` in edition 2024, and the workspace denies
//! `unsafe_code`, so adding one takes its own reviewed `allow`). There is none in this tree; keep
//! it so, or call this before any thread that could change the environment exists.

/// The most an offset can be: no zone is further than UTC+14 or UTC-12, and a nonsense value
/// from a broken zone database must not skew every time on the screen.
const MAX_OFFSET_SECS: i64 = 15 * 3600;

/// Seconds east of UTC that this machine's zone is at `unix_secs` (daylight saving included), or
/// 0 when the zone cannot be read or the instant is not representable.
#[allow(unsafe_code)]
pub fn utc_offset_secs(unix_secs: i64) -> i32 {
    // `time_t` is 64 bits on every platform fil runs on (Linux, macOS); on one where it is not,
    // an instant that does not fit is UTC rather than a different instant.
    #[allow(clippy::useless_conversion)]
    let Some(t) = libc::time_t::try_from(unix_secs).ok() else {
        return 0;
    };
    // Zeroed, and read only when `localtime_r` says it filled it in: POSIX does not say what it
    // leaves in `tm` when it fails.
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `t` and `tm` are live locals, and `localtime_r` is the re-entrant form (no static
    // buffer); see the module docs for the environment condition.
    let filled = unsafe { !libc::localtime_r(&t, tm.as_mut_ptr()).is_null() };
    if !filled {
        return 0;
    }
    // SAFETY: an all-zero `tm` is a valid value (its `tm_zone` is a null raw pointer), and
    // `localtime_r` succeeded, so this is what it wrote.
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
