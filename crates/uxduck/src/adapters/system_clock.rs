//! This computer's wall clock for an onboard computer that sets it from the
//! ground station's (S373): whether anything keeps it, and stepping it.

/// Whether the kernel says something keeps the clock: an NTP client such as
/// systemd-timesyncd or chrony that reached a time server clears its
/// `STA_UNSYNC`. Elsewhere than Linux UXDuck never sets the clock, so it
/// counts as kept.
#[cfg(target_os = "linux")]
#[allow(
    unsafe_code,
    reason = "adjtimex with no modes only reads the kernel's clock state"
)]
#[must_use]
pub fn kept() -> bool {
    // SAFETY: a zeroed timex is valid, and with `modes` 0 adjtimex only
    // fills it in.
    let mut timex: libc::timex = unsafe { std::mem::zeroed() };
    // SAFETY: as above, with a pointer to that timex.
    let state = unsafe { libc::adjtimex(&raw mut timex) };
    state != -1 && state != libc::TIME_ERROR && timex.status & libc::STA_UNSYNC == 0
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn kept() -> bool {
    true
}

/// How fast the kernel slews the clock for `adjtime`: 500 µs a second
/// (Linux's `MAX_TICKADJ`).
const SLEW_NS_PER_S: u128 = 500_000;

/// How [`set`] moved the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    /// Run a little faster or slower until it had moved, so it never jumped
    /// or ran backwards.
    Slewed,
    /// Set at once.
    Stepped,
}

/// Whether the kernel can slew the clock `by_ns` nanoseconds within
/// `within`.
#[must_use]
pub fn slews(by_ns: i64, within: std::time::Duration) -> bool {
    u128::from(by_ns.unsigned_abs()) <= SLEW_NS_PER_S * within.as_nanos() / 1_000_000_000
}

/// Moves the wall clock by `by_ns` nanoseconds: slewed when the kernel can
/// do it within `within`, so a small error never makes the clock jump or
/// run backwards, and stepped when it is further off. It needs
/// `CAP_SYS_TIME`.
///
/// # Errors
/// The kernel's refusal, usually for want of `CAP_SYS_TIME`.
pub fn set(by_ns: i64, within: std::time::Duration) -> std::io::Result<Moved> {
    if slews(by_ns, within) {
        slew(by_ns)?;
        Ok(Moved::Slewed)
    } else {
        step(by_ns)?;
        Ok(Moved::Stepped)
    }
}

#[cfg(target_os = "linux")]
#[allow(
    unsafe_code,
    reason = "adjtime with a delta and no old delta only asks the kernel to slew the clock"
)]
#[allow(
    clippy::useless_conversion,
    reason = "time_t and suseconds_t are i64 here and i32 on 32-bit Linux"
)]
fn slew(by_ns: i64) -> std::io::Result<()> {
    let micros = by_ns / 1_000;
    let delta = libc::timeval {
        tv_sec: (micros / 1_000_000)
            .try_into()
            .map_err(std::io::Error::other)?,
        tv_usec: (micros % 1_000_000)
            .try_into()
            .map_err(std::io::Error::other)?,
    };
    // SAFETY: a valid timeval, and no old delta, which adjtime allows.
    if unsafe { libc::adjtime(&raw const delta, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn step(by_ns: i64) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?;
    let then = i128::try_from(now.as_nanos())
        .map_err(std::io::Error::other)?
        .saturating_add(i128::from(by_ns));
    let second = 1_000_000_000_i128;
    let timespec = rustix::time::Timespec {
        tv_sec: then
            .div_euclid(second)
            .try_into()
            .map_err(std::io::Error::other)?,
        tv_nsec: then
            .rem_euclid(second)
            .try_into()
            .map_err(std::io::Error::other)?,
    };
    rustix::time::clock_settime(rustix::time::ClockId::Realtime, timespec)
        .map_err(std::io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn slew(_by_ns: i64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UXDuck sets the clock only on Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
fn step(_by_ns: i64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UXDuck sets the clock only on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::slews;

    #[test]
    fn an_error_the_kernel_slews_away_before_the_next_answer_is_slewed() {
        let minute = Duration::from_secs(60);
        assert!(
            slews(-1_000_000, minute),
            "Buggy's 1 ms a minute after it was set"
        );
        assert!(slews(30_000_000, minute), "500 µs a second for a minute");
        assert!(slews(-30_000_000, minute));
        assert!(!slews(30_000_001, minute));
        assert!(
            !slews(883_493_650_000_000, minute),
            "10 days 5 h is stepped"
        );
    }
}
