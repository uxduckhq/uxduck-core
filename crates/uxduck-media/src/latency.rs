//! How late a camera's pictures are (ADR-0017 V26, S262): when each one
//! arrived, on the monotonic clock and on the wall clock; RFC 3550's
//! interarrival jitter; the delay from capture by the camera's own clock,
//! from its RTCP sender reports or its KLV Precision Time Stamp; and a
//! window of recent samples summarised by their median. Nothing here
//! allocates per picture.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rtcp::SenderReport;

/// When a datagram arrived: the monotonic clock for what UXDuck holds, the
/// wall clock for what the camera's clock says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    pub at: Instant,
    /// Microseconds since 1970.
    pub wall_us: u64,
}

impl Arrival {
    #[must_use]
    pub fn now() -> Self {
        Self {
            at: Instant::now(),
            wall_us: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX)),
        }
    }
}

/// How far back a summary looks.
pub const SPAN: Duration = Duration::from_secs(1);

/// Samples kept: a second of pictures at 120 fps.
const SAMPLES: usize = 128;

/// Recent samples, newest over oldest once full.
#[derive(Clone, Debug)]
pub struct Window {
    samples: [Option<(Instant, i64)>; SAMPLES],
    next: usize,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            samples: [None; SAMPLES],
            next: 0,
        }
    }
}

impl Window {
    pub fn add(&mut self, at: Instant, value: i64) {
        self.samples[self.next] = Some((at, value));
        self.next = (self.next + 1) % SAMPLES;
    }

    /// The median of the samples from the last `SPAN` before `now`, the
    /// lower of the middle two when there is an even number.
    #[must_use]
    pub fn median(&self, now: Instant) -> Option<i64> {
        let mut recent = [0i64; SAMPLES];
        let mut n = 0;
        for (at, value) in self.samples.iter().flatten() {
            if now.saturating_duration_since(*at) <= SPAN {
                recent[n] = *value;
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        let middle = (n - 1) / 2;
        Some(*recent[..n].select_nth_unstable(middle).1)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// RFC 3550's interarrival jitter (section 6.4.1): how unevenly pictures
/// arrive against the times they are presented at. Each picture is
/// compared with the one before it, by its first datagram's arrival and its
/// decoding time; a jump in the stream's timestamps (a file looping, a
/// camera restarting) starts the comparison again.
#[derive(Clone, Debug, Default)]
pub struct Jitter {
    last: Option<(Instant, u64)>,
    estimate_us: f64,
    samples: u64,
}

/// Timestamps further apart than this are a jump, not the next picture.
const JUMP_US: u64 = 5_000_000;

impl Jitter {
    /// A picture that arrived `at`, decoded at `media_us` on its own clock.
    #[allow(
        clippy::cast_precision_loss,
        reason = "microseconds between two pictures"
    )]
    pub fn push(&mut self, at: Instant, media_us: u64) {
        if let Some((last_at, last_us)) = self.last
            && media_us > last_us
            && media_us - last_us <= JUMP_US
        {
            let arrived = at.saturating_duration_since(last_at).as_secs_f64() * 1e6;
            let presented = (media_us - last_us) as f64;
            let d = (arrived - presented).abs();
            self.estimate_us += (d - self.estimate_us) / 16.0;
            self.samples += 1;
        }
        self.last = Some((at, media_us));
    }

    /// The jitter in microseconds, once two pictures have been compared.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive number of microseconds"
    )]
    pub fn us(&self) -> Option<u64> {
        (self.samples > 0).then(|| self.estimate_us.round() as u64)
    }
}

/// Where the camera says when it captured a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CameraClock {
    /// RTCP sender reports: the RTP clock at a wall-clock time.
    SenderReports,
    /// MISB ST 0601's Precision Time Stamp (tag 2) in its KLV.
    Klv,
}

/// Beyond this, a delay says the two clocks are not in step rather than
/// how late the picture is.
pub const PLAUSIBLE: Duration = Duration::from_secs(5);

/// What the camera's clock says about how late its pictures arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraDelay {
    pub clock: CameraClock,
    /// From capture to arrival, when the clocks seem in step: the median
    /// over the last second, never negative nor over `PLAUSIBLE`.
    pub delay_us: Option<u64>,
}

/// The delay from capture by the camera's clock to arrival, per picture.
#[derive(Clone, Debug, Default)]
pub struct CaptureDelay {
    /// The clock used, KLV over sender reports once a KLV time is seen:
    /// MISB defines its time stamp as the picture's capture.
    clock: Option<CameraClock>,
    window: Window,
}

/// The video's RTP clock: 90 kHz.
const RTP_RATE: i64 = 90_000;
/// Seconds from NTP's epoch, 1900, to Unix's, 1970.
const NTP_TO_UNIX_S: u64 = 2_208_988_800;

/// A sender report's NTP time as microseconds since 1970.
#[must_use]
pub fn ntp_unix_us(ntp: u64) -> Option<u64> {
    let seconds = (ntp >> 32).checked_sub(NTP_TO_UNIX_S)?;
    let fraction = ((ntp & 0xffff_ffff) * 1_000_000) >> 32;
    Some(seconds * 1_000_000 + fraction)
}

/// When a picture with this RTP timestamp was captured, by the report's
/// wall clock: microseconds since 1970.
#[must_use]
pub fn rtp_capture_us(report: &SenderReport, timestamp: u32) -> Option<u64> {
    let at = ntp_unix_us(report.ntp)?;
    #[allow(clippy::cast_possible_wrap, reason = "RTP time wraps")]
    let ticks = i64::from(timestamp.wrapping_sub(report.rtp) as i32);
    at.checked_add_signed(ticks * 1_000_000 / RTP_RATE)
}

impl CaptureDelay {
    /// A picture or KLV packet captured at `captured_us` (since 1970) by
    /// `clock`, which arrived as `arrival` says.
    pub fn push(&mut self, clock: CameraClock, captured_us: u64, arrival: Arrival) {
        if self.clock.is_some_and(|c| c > clock) {
            return;
        }
        if self.clock != Some(clock) {
            self.window.clear();
            self.clock = Some(clock);
        }
        #[allow(
            clippy::cast_possible_wrap,
            reason = "two times since 1970, well within range"
        )]
        let delay = arrival.wall_us as i64 - captured_us as i64;
        self.window.add(arrival.at, delay);
    }

    /// The clock the camera states its capture time by, and the delay when
    /// it is plausible.
    #[must_use]
    pub fn summary(&self, now: Instant) -> Option<CameraDelay> {
        let clock = self.clock?;
        let limit = i64::try_from(PLAUSIBLE.as_micros()).unwrap_or(i64::MAX);
        let delay_us = self
            .window
            .median(now)
            .filter(|d| (0..=limit).contains(d))
            .and_then(|d| u64::try_from(d).ok());
        Some(CameraDelay { clock, delay_us })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn arrival(at: Instant, wall_us: u64) -> Arrival {
        Arrival { at, wall_us }
    }

    #[test]
    fn the_median_is_of_the_last_second_only() {
        let start = Instant::now();
        let mut window = Window::default();
        assert_eq!(window.median(start), None);
        // Old samples that a second later no longer count.
        for i in 0..10 {
            window.add(start + i * MS, 1_000);
        }
        for (i, v) in [5, 1, 9, 3, 7].into_iter().enumerate() {
            window.add(start + 1500 * MS + u32::try_from(i).unwrap() * MS, v);
        }
        assert_eq!(window.median(start + 1600 * MS), Some(5));
        // An even count takes the lower middle.
        window.add(start + 1601 * MS, 4);
        assert_eq!(window.median(start + 1600 * MS), Some(4));
        assert_eq!(window.median(start + 5 * Duration::from_secs(1)), None);
    }

    #[test]
    fn a_full_window_keeps_the_newest() {
        let start = Instant::now();
        let mut window = Window::default();
        for i in 0..300u32 {
            window.add(start + i * MS, i64::from(i));
        }
        // The newest 128: 172 to 299.
        assert_eq!(window.median(start + 300 * MS), Some(235));
    }

    #[test]
    fn evenly_arriving_pictures_have_no_jitter() {
        let start = Instant::now();
        let mut jitter = Jitter::default();
        assert_eq!(jitter.us(), None);
        for i in 0..50u32 {
            jitter.push(start + i * 40 * MS, u64::from(i) * 40_000);
        }
        assert_eq!(jitter.us(), Some(0));
    }

    #[test]
    fn jitter_follows_rfc_3550() {
        let start = Instant::now();
        let mut jitter = Jitter::default();
        // Every other picture 10 ms late: |D| alternates 10 ms and 10 ms.
        let mut expected = 0.0f64;
        for i in 0..200u32 {
            let late = if i % 2 == 1 { 10 * MS } else { Duration::ZERO };
            jitter.push(start + i * 40 * MS + late, u64::from(i) * 40_000);
            if i > 0 {
                expected += (10_000.0 - expected) / 16.0;
            }
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected = expected.round() as u64;
        assert_eq!(jitter.us(), Some(expected));
        assert!((9_990..=10_000).contains(&expected), "{expected}");
    }

    #[test]
    fn a_jump_in_timestamps_starts_jitter_again() {
        let start = Instant::now();
        let mut jitter = Jitter::default();
        jitter.push(start, 10_000_000);
        jitter.push(start + 40 * MS, 10_040_000);
        // The file loops back to its start: not a picture 10 s early.
        jitter.push(start + 80 * MS, 0);
        jitter.push(start + 120 * MS, 40_000);
        assert_eq!(jitter.us(), Some(0));
        // Nor a camera that restarts minutes ahead.
        jitter.push(start + 160 * MS, 600_000_000);
        assert_eq!(jitter.us(), Some(0));
    }

    #[test]
    fn a_sender_report_dates_a_picture_by_its_rtp_time() {
        // 2026-11-01 00:00:00 UTC is 1_793_491_200 s since 1970.
        let unix_s = 1_793_491_200u64;
        let report = SenderReport {
            ssrc: 1,
            ntp: ((unix_s + NTP_TO_UNIX_S) << 32) | (1 << 31),
            rtp: u32::MAX - 44_999,
        };
        assert_eq!(ntp_unix_us(report.ntp), Some(unix_s * 1_000_000 + 500_000));
        // One second of RTP time later, across the wrap.
        assert_eq!(
            rtp_capture_us(&report, 45_000),
            Some(unix_s * 1_000_000 + 1_500_000)
        );
        // And a picture from before the report.
        assert_eq!(
            rtp_capture_us(&report, u32::MAX - 89_999),
            Some(unix_s * 1_000_000)
        );
        // An NTP time before 1970 dates nothing.
        assert_eq!(ntp_unix_us(5 << 32), None);
    }

    #[test]
    fn the_cameras_delay_is_withheld_when_its_clock_is_not_in_step() {
        let start = Instant::now();
        let mut delay = CaptureDelay::default();
        assert_eq!(delay.summary(start), None);
        let wall = 1_793_491_200_000_000u64;
        for i in 0..10u32 {
            let at = start + i * 40 * MS;
            let wall = wall + u64::from(i) * 40_000;
            delay.push(
                CameraClock::SenderReports,
                wall - 120_000,
                arrival(at, wall),
            );
        }
        let now = start + 400 * MS;
        assert_eq!(
            delay.summary(now),
            Some(CameraDelay {
                clock: CameraClock::SenderReports,
                delay_us: Some(120_000),
            })
        );
        // A camera whose clock is ahead: pictures from the future.
        let mut ahead = CaptureDelay::default();
        ahead.push(CameraClock::Klv, wall + 2_000_000, arrival(start, wall));
        assert_eq!(
            ahead.summary(start),
            Some(CameraDelay {
                clock: CameraClock::Klv,
                delay_us: None,
            })
        );
        // Or a minute behind.
        let mut behind = CaptureDelay::default();
        behind.push(CameraClock::Klv, wall - 60_000_000, arrival(start, wall));
        assert_eq!(behind.summary(start).unwrap().delay_us, None);
        // Five seconds is still a delay.
        let mut slow = CaptureDelay::default();
        slow.push(CameraClock::Klv, wall - 5_000_000, arrival(start, wall));
        assert_eq!(slow.summary(start).unwrap().delay_us, Some(5_000_000));
    }

    #[test]
    fn klv_time_is_preferred_to_sender_reports() {
        let start = Instant::now();
        let wall = 1_793_491_200_000_000u64;
        let mut delay = CaptureDelay::default();
        delay.push(
            CameraClock::SenderReports,
            wall - 300_000,
            arrival(start, wall),
        );
        delay.push(CameraClock::Klv, wall - 80_000, arrival(start, wall));
        delay.push(
            CameraClock::SenderReports,
            wall - 300_000,
            arrival(start, wall),
        );
        assert_eq!(
            delay.summary(start),
            Some(CameraDelay {
                clock: CameraClock::Klv,
                delay_us: Some(80_000),
            })
        );
    }
}
