//! How late a camera's pictures are (ADR-0017 V26, S262): when each one
//! arrived, on the monotonic clock and on the wall clock; RFC 3550's
//! interarrival jitter; the delay from capture by the camera's own clock,
//! from its RTCP sender reports or its KLV Precision Time Stamp; and a
//! window of recent samples summarised by their median. The camera's clock
//! is matched to this computer's by the first way that has measured it
//! (S355): an RTSP server's clock, timed by its sender reports and
//! `GET_PARAMETER` round trips, else the two clocks assumed in step. Nothing
//! here allocates per picture.

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

/// A way of matching the camera's clock to this computer's (S355). They
/// are tried in this order, and the first with a measurement is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockWay {
    /// The clock of the companion computer serving the camera, measured by
    /// MAVLink TIMESYNC (S357): its RTSP server's sender reports are stamped
    /// by it.
    Companion,
    /// The RTSP server's clock, measured as `ServerClock` says. Only an
    /// RTCP sender report's time is the server's by construction.
    Rtsp,
    /// The two clocks assumed in step, as S262 did.
    Assumed,
}

/// Why a way is not used, when that is not because a way before it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockGap {
    /// The camera is not reached this way, such as a camera not over RTSP.
    NotServed,
    /// No sender report in the last `OFFSET_SPAN`.
    NoReports,
    /// No answered request in the last `OFFSET_SPAN`.
    NoAnswers,
    /// Read only one way, from a computer's status messages: off by however
    /// long they took, which nobody knows, so it dates no picture (S357).
    OneWay,
    /// The RTSP server's sender reports say when each picture was taken, but
    /// not when they were sent, so measured by them its clock reads behind
    /// by at least a picture's time from the sensor to the server: 38 ms on
    /// Buggy's `MediaMTX`. It dates no picture.
    DatedByPictures,
    /// Not the clock the camera dates its pictures by: a KLV time stamp
    /// may come from GPS on another machine, not the RTSP server's clock.
    OtherClock,
}

/// One way of matching the camera's clock, and what it found (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockMatch {
    pub way: ClockWay,
    /// The onboard computer id of the companion computer, for `Companion`
    /// (S357).
    pub computer_id: Option<u8>,
    /// The camera's clock minus this computer's, once measured: 0 for
    /// `Assumed`.
    pub offset_us: Option<i64>,
    /// Give or take, once measured: none for `Assumed`, which nobody can say.
    pub within_us: Option<u64>,
    /// Whether this way dates the pictures.
    pub used: bool,
    pub gap: Option<ClockGap>,
}

/// What the camera's clock says about how late its pictures arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraDelay {
    pub clock: CameraClock,
    /// From capture to arrival, by the camera's clock matched as `matches`
    /// says: the median over the last second, when it is plausible. A
    /// measured clock allows its uncertainty below 0, which reads as 0.
    pub delay_us: Option<u64>,
    /// The same median before that check (S355): negative when the
    /// camera's clock dates pictures after they arrive, which no capture
    /// time can.
    pub signed_us: Option<i64>,
    /// The ways of matching the camera's clock, in the order tried (S355).
    pub matches: [ClockMatch; 3],
}

/// The companion computer serving a camera, and its clock as MAVLink
/// measured it in the last minute (S357).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Companion {
    /// Its onboard computer id, 191 to 194.
    pub computer_id: u8,
    /// None when it has not been heard for a minute.
    pub clock: Option<CompanionClock>,
}

/// How a companion computer's clock was measured (S357).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionClock {
    /// By `TIMESYNC` round trips.
    Timesync(Offset),
    /// One way, from the time its status messages say they were sent: its
    /// clock minus this computer's, less however long they took.
    OneWay { offset_us: i64 },
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

    /// The clock the camera states its capture time by, matched to this
    /// computer's by the first way that has measured it, and the delay when
    /// it is plausible. The ways are tried in the order of `ClockWay`:
    /// `companion`, the companion computer serving the camera (S357), then
    /// `server`, the RTSP server's clock for a camera reached over RTSP
    /// (S355), then the two clocks assumed in step.
    #[must_use]
    pub fn summary(
        &self,
        now: Instant,
        companion: Option<&Companion>,
        server: Option<&ServerClock>,
    ) -> Option<CameraDelay> {
        let clock = self.clock?;
        let unmeasured = |way, gap| ClockMatch {
            way,
            computer_id: None,
            offset_us: None,
            within_us: None,
            used: false,
            gap: Some(gap),
        };
        let measured = |way, offset: Offset| ClockMatch {
            way,
            computer_id: None,
            offset_us: Some(offset.offset_us),
            within_us: Some(offset.within_us),
            used: false,
            gap: None,
        };
        let companion =
            companion.map_or(unmeasured(ClockWay::Companion, ClockGap::NotServed), |c| {
                ClockMatch {
                    computer_id: Some(c.computer_id),
                    ..match c.clock {
                        Some(CompanionClock::Timesync(offset)) => {
                            measured(ClockWay::Companion, offset)
                        }
                        Some(CompanionClock::OneWay { offset_us }) => ClockMatch {
                            offset_us: Some(offset_us),
                            ..unmeasured(ClockWay::Companion, ClockGap::OneWay)
                        },
                        None => unmeasured(ClockWay::Companion, ClockGap::NoAnswers),
                    }
                }
            });
        let server = match server.map(|s| (s.offset(now), s.dates_by_pictures)) {
            None => unmeasured(ClockWay::Rtsp, ClockGap::NotServed),
            Some((Ok(offset), true)) => ClockMatch {
                gap: Some(ClockGap::DatedByPictures),
                ..measured(ClockWay::Rtsp, offset)
            },
            Some((Err(_), true)) => unmeasured(ClockWay::Rtsp, ClockGap::DatedByPictures),
            Some((Ok(offset), false)) => measured(ClockWay::Rtsp, offset),
            Some((Err(Missing::Reports), false)) => unmeasured(ClockWay::Rtsp, ClockGap::NoReports),
            Some((Err(Missing::Answers), false)) => unmeasured(ClockWay::Rtsp, ClockGap::NoAnswers),
        };
        let mut matches = [
            companion,
            server,
            ClockMatch {
                way: ClockWay::Assumed,
                computer_id: None,
                offset_us: Some(0),
                within_us: None,
                used: false,
                gap: None,
            },
        ];
        // A KLV time stamp may be GPS's on another machine: a computer's
        // clock is only its sender reports'.
        if clock == CameraClock::Klv {
            for way in &mut matches[..2] {
                if way.gap != Some(ClockGap::NotServed) {
                    way.gap = Some(ClockGap::OtherClock);
                }
            }
        }
        let first = matches
            .iter()
            .position(|m| m.gap.is_none())
            .unwrap_or(matches.len() - 1);
        matches[first].used = true;
        let used = matches[first];
        let signed_us = self
            .window
            .median(now)
            .map(|d| d.saturating_add(used.offset_us.unwrap_or(0)));
        let limit = i64::try_from(PLAUSIBLE.as_micros()).unwrap_or(i64::MAX);
        let below = i64::try_from(used.within_us.unwrap_or(0)).unwrap_or(i64::MAX);
        let delay_us = signed_us
            .filter(|d| (-below..=limit).contains(d))
            .map(|d| d.max(0).unsigned_abs());
        Some(CameraDelay {
            clock,
            delay_us,
            signed_us,
            matches,
        })
    }
}

/// How long a clock's measurements count (S355): a minute, six sender
/// reports from a server that sends one every 10 s.
pub const OFFSET_SPAN: Duration = Duration::from_secs(60);

/// Measurements kept of each kind: a minute of one a second.
const MEASUREMENTS: usize = 64;

/// The newest measurements, each with when it was taken.
#[derive(Clone, Debug)]
struct Recent<T: Copy> {
    samples: [Option<(Instant, T)>; MEASUREMENTS],
    next: usize,
}

impl<T: Copy> Default for Recent<T> {
    fn default() -> Self {
        Self {
            samples: [None; MEASUREMENTS],
            next: 0,
        }
    }
}

impl<T: Copy> Recent<T> {
    fn add(&mut self, at: Instant, value: T) {
        self.samples[self.next] = Some((at, value));
        self.next = (self.next + 1) % MEASUREMENTS;
    }

    /// Those from the last `OFFSET_SPAN` before `now`.
    fn since(&self, now: Instant) -> impl Iterator<Item = T> + '_ {
        self.samples
            .iter()
            .flatten()
            .filter(move |(at, _)| now.saturating_duration_since(*at) <= OFFSET_SPAN)
            .map(|(_, value)| *value)
    }
}

/// A clock's offset from this computer's, as a way measured it (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offset {
    /// That clock minus this computer's.
    pub offset_us: i64,
    /// Give or take.
    pub within_us: u64,
}

/// What a measurement of a clock still waits for (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// A sender report in the last `OFFSET_SPAN`.
    Reports,
    /// An answered request in the last `OFFSET_SPAN`.
    Answers,
}

/// An RTSP server's clock against this computer's (S355), measured as NTP
/// measures a clock. Each video sender report states the server's time as
/// it was sent, so its time less its arrival by this computer's clock is
/// the offset less how long it took. The largest of the last minute's took
/// the least, and `GET_PARAMETER` round trips say how long that least is: at
/// most the fastest round trip, so half of it give or take half of it.
/// That assumes the fastest report took no longer than the fastest round
/// trip, true on a link whose reports do not wait behind the video.
#[derive(Clone, Debug, Default)]
pub struct ServerClock {
    /// Each report's time less its arrival.
    reports: Recent<i64>,
    /// Each round trip.
    round_trips: Recent<u64>,
    /// Whether the server's sender reports say when each picture was taken
    /// but not when they were sent. gortsplib, `MediaMTX`'s RTSP library,
    /// states each one as its last picture's time plus the time since it
    /// handled that picture's packet (`pkg/rtpsender/sender.go`, read
    /// 2026-11-13), and `MediaMTX`'s Raspberry Pi camera times a picture by
    /// when the sensor captured it (`mediamtx-rpicamera`'s `camera.cpp`, read
    /// 2026-11-14). live555, `GStreamer` and `FFmpeg` state when they send, as
    /// RFC 3550 says.
    dates_by_pictures: bool,
}

impl ServerClock {
    /// For the RTSP server whose `Server` header says `server`.
    #[must_use]
    pub fn new(server: Option<&str>) -> Self {
        Self {
            dates_by_pictures: server.is_some_and(|s| s.to_ascii_lowercase().contains("gortsplib")),
            ..Self::default()
        }
    }

    /// A sender report, its NTP time, and when it arrived.
    pub fn report(&mut self, ntp: u64, arrival: Arrival) {
        self.reports
            .add(arrival.at, ntp_minus_unix_us(ntp, arrival.wall_us));
    }

    /// A request sent then, answered now, whatever its answer.
    pub fn round_trip(&mut self, sent: Instant, answered: Instant) {
        let took = answered.saturating_duration_since(sent).as_micros();
        self.round_trips
            .add(answered, u64::try_from(took).unwrap_or(u64::MAX));
    }

    /// The server's clock minus this computer's, from the last
    /// `OFFSET_SPAN`'s reports and round trips.
    ///
    /// # Errors
    ///
    /// What the last `OFFSET_SPAN` had none of, the reports first.
    pub fn offset(&self, now: Instant) -> Result<Offset, Missing> {
        let ahead = self.reports.since(now).max().ok_or(Missing::Reports)?;
        let round_trip = self.round_trips.since(now).min().ok_or(Missing::Answers)?;
        let within_us = round_trip / 2;
        Ok(Offset {
            offset_us: ahead.saturating_add_unsigned(within_us),
            within_us,
        })
    }
}

/// An NTP time less a time in microseconds since 1970, in microseconds to
/// the nearest. NTP's seconds wrap every 136 years, the next time in 2036,
/// so the two are compared as NTP compares times (RFC 5905): right while
/// they are less than 68 years apart.
#[must_use]
pub fn ntp_minus_unix_us(ntp: u64, unix_us: u64) -> i64 {
    let seconds = unix_us / 1_000_000 + NTP_TO_UNIX_S;
    let fraction = ((unix_us % 1_000_000) << 32) / 1_000_000;
    let unix = (seconds << 32) | fraction;
    #[allow(clippy::cast_possible_wrap, reason = "NTP times compare wrapped")]
    let apart = i128::from(ntp.wrapping_sub(unix) as i64);
    i64::try_from((apart * 1_000_000 + (1 << 31)) >> 32).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// No companion computer serves the camera (S357).
    const COMPANION_NOT_SERVED: ClockMatch = ClockMatch {
        way: ClockWay::Companion,
        computer_id: None,
        offset_us: None,
        within_us: None,
        used: false,
        gap: Some(ClockGap::NotServed),
    };

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

    /// A camera not reached over RTSP: its clock assumed in step.
    fn not_over_rtsp(clock: CameraClock, delay_us: Option<u64>, signed_us: i64) -> CameraDelay {
        CameraDelay {
            clock,
            delay_us,
            signed_us: Some(signed_us),
            matches: [
                COMPANION_NOT_SERVED,
                ClockMatch {
                    way: ClockWay::Rtsp,
                    computer_id: None,
                    offset_us: None,
                    within_us: None,
                    used: false,
                    gap: Some(ClockGap::NotServed),
                },
                ClockMatch {
                    way: ClockWay::Assumed,
                    computer_id: None,
                    offset_us: Some(0),
                    within_us: None,
                    used: true,
                    gap: None,
                },
            ],
        }
    }

    #[test]
    fn the_cameras_delay_is_withheld_when_its_clock_is_not_in_step() {
        let start = Instant::now();
        let mut delay = CaptureDelay::default();
        assert_eq!(delay.summary(start, None, None), None);
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
            delay.summary(now, None, None),
            Some(not_over_rtsp(
                CameraClock::SenderReports,
                Some(120_000),
                120_000
            ))
        );
        // A camera whose clock is ahead: pictures from the future.
        let mut ahead = CaptureDelay::default();
        ahead.push(CameraClock::Klv, wall + 2_000_000, arrival(start, wall));
        assert_eq!(
            ahead.summary(start, None, None),
            Some(not_over_rtsp(CameraClock::Klv, None, -2_000_000))
        );
        // Or a minute behind.
        let mut behind = CaptureDelay::default();
        behind.push(CameraClock::Klv, wall - 60_000_000, arrival(start, wall));
        assert_eq!(behind.summary(start, None, None).unwrap().delay_us, None);
        // Five seconds is still a delay.
        let mut slow = CaptureDelay::default();
        slow.push(CameraClock::Klv, wall - 5_000_000, arrival(start, wall));
        assert_eq!(
            slow.summary(start, None, None).unwrap().delay_us,
            Some(5_000_000)
        );
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
            delay.summary(start, None, None),
            Some(not_over_rtsp(CameraClock::Klv, Some(80_000), 80_000))
        );
    }

    /// NTP's 64 bits for a time in microseconds since 1970.
    fn ntp(unix_us: u64) -> u64 {
        let seconds = unix_us / 1_000_000 + NTP_TO_UNIX_S;
        (seconds << 32) | (((unix_us % 1_000_000) << 32) / 1_000_000)
    }

    #[test]
    fn ntp_times_are_compared_with_their_sign_and_across_the_wrap() {
        let wall = 1_793_491_200_000_000u64;
        assert_eq!(ntp_minus_unix_us(ntp(wall + 2_000_000), wall), 2_000_000);
        assert_eq!(ntp_minus_unix_us(ntp(wall - 1_500), wall + 7), -1_507);
        // 8 days 6 h behind, as the maintainer's camera was.
        assert_eq!(
            ntp_minus_unix_us(ntp(wall - 712_505_489_123), wall),
            -712_505_489_123
        );
        // NTP's seconds wrap on 2036-02-07 06:28:16 UTC: a camera 2 s
        // ahead of a computer 1 s before then reads 1 s past NTP's zero.
        let before_wrap = ((1u64 << 32) - NTP_TO_UNIX_S - 1) * 1_000_000;
        assert_eq!(ntp_minus_unix_us(1 << 32, before_wrap), 2_000_000);
        // And one a second behind a computer just past it.
        assert_eq!(
            ntp_minus_unix_us(u64::MAX << 32, before_wrap + 2_000_000),
            -2_000_000
        );
    }

    /// A server clock `offset_us` from this computer's, its reports taking
    /// `transits` and its answers `round_trips`, all in microseconds.
    fn server(
        start: Instant,
        offset_us: i64,
        transits: &[u64],
        round_trips: &[u64],
    ) -> ServerClock {
        named(None, start, offset_us, transits, round_trips)
    }

    /// The same, from a server whose `Server` header is `name`.
    fn named(
        name: Option<&str>,
        start: Instant,
        offset_us: i64,
        transits: &[u64],
        round_trips: &[u64],
    ) -> ServerClock {
        let wall = 1_793_491_200_000_000u64;
        let mut clock = ServerClock::new(name);
        for (i, transit) in (0u64..).zip(transits) {
            let sent = wall + i * 10_000_000;
            let at = start + Duration::from_secs(i * 10) + Duration::from_micros(*transit);
            let stated = ntp(sent.checked_add_signed(offset_us).unwrap());
            clock.report(stated, arrival(at, sent + transit));
        }
        for (i, took) in (0u64..).zip(round_trips) {
            let sent = start + Duration::from_secs(i);
            clock.round_trip(sent, sent + Duration::from_micros(*took));
        }
        clock
    }

    #[test]
    fn a_servers_clock_is_its_fastest_report_and_half_the_fastest_round_trip() {
        let start = Instant::now();
        let now = start + Duration::from_secs(50);
        // The maintainer's camera: 8 days 6 h behind, its reports taking
        // about half a millisecond and its round trips 1.22 ms at best.
        let behind = server(
            start,
            -712_505_489_000,
            &[640, 520, 900, 700],
            &[1_590, 1_220, 12_950, 1_400],
        );
        assert_eq!(
            behind.offset(now),
            Ok(Offset {
                // The fastest report, 520 µs, less 610 µs, so 90 µs ahead
                // of the truth: within the 610 µs it claims.
                offset_us: -712_505_489_000 + 90,
                within_us: 610,
            })
        );
        // A clock 3.2 s ahead is positive.
        let ahead = server(start, 3_200_000, &[1_000], &[2_000]);
        assert_eq!(
            ahead.offset(now),
            Ok(Offset {
                offset_us: 3_200_000,
                within_us: 1_000,
            })
        );
    }

    #[test]
    fn a_server_clock_measures_only_from_the_last_minute() {
        let start = Instant::now();
        let mut clock = server(start, -5_000_000, &[1_000], &[2_000]);
        assert_eq!(
            clock.offset(start + Duration::from_secs(30)),
            Ok(Offset {
                offset_us: -5_000_000,
                within_us: 1_000,
            })
        );
        // A minute later, neither counts: reports are missed first.
        let later = start + Duration::from_secs(62);
        assert_eq!(clock.offset(later), Err(Missing::Reports));
        let wall = 1_793_491_200_000_000u64 + 62_000_000;
        clock.report(ntp(wall - 5_000_000), arrival(later, wall + 400));
        assert_eq!(clock.offset(later), Err(Missing::Answers));
        clock.round_trip(start + Duration::from_millis(61_997), later);
        assert_eq!(
            clock.offset(later),
            Ok(Offset {
                offset_us: -5_000_400 + 1_500,
                within_us: 1_500,
            })
        );
    }

    #[test]
    fn a_server_that_answers_no_request_is_not_measured() {
        let start = Instant::now();
        let silent = server(start, -712_505_489_000, &[500, 600], &[]);
        assert_eq!(
            silent.offset(start + Duration::from_secs(20)),
            Err(Missing::Answers)
        );
        // Its pictures are dated assuming the clocks are in step, so not.
        let wall = 1_793_491_200_000_000u64;
        let mut delay = CaptureDelay::default();
        delay.push(
            CameraClock::SenderReports,
            wall - 712_505_489_000 - 40_000,
            arrival(start, wall),
        );
        let summary = delay.summary(start, None, Some(&silent)).unwrap();
        assert_eq!(summary.delay_us, None);
        assert_eq!(summary.signed_us, Some(712_505_529_000));
        assert_eq!(
            summary.matches,
            [
                COMPANION_NOT_SERVED,
                ClockMatch {
                    way: ClockWay::Rtsp,
                    computer_id: None,
                    offset_us: None,
                    within_us: None,
                    used: false,
                    gap: Some(ClockGap::NoAnswers),
                },
                ClockMatch {
                    way: ClockWay::Assumed,
                    computer_id: None,
                    offset_us: Some(0),
                    within_us: None,
                    used: true,
                    gap: None,
                },
            ]
        );
    }

    #[test]
    fn the_servers_clock_dates_its_reports_pictures() {
        let start = Instant::now();
        let wall = 1_793_491_200_000_000u64;
        let behind = -712_505_489_000i64;
        let clock = server(start, behind, &[500], &[1_200]);
        let measured = ClockMatch {
            way: ClockWay::Rtsp,
            computer_id: None,
            offset_us: Some(behind + 100),
            within_us: Some(600),
            used: true,
            gap: None,
        };
        let not_needed = ClockMatch {
            way: ClockWay::Assumed,
            computer_id: None,
            offset_us: Some(0),
            within_us: None,
            used: false,
            gap: None,
        };
        // Pictures captured `late` before they arrive, by the camera's clock.
        let summary = |late: i64| {
            let mut delay = CaptureDelay::default();
            let captured = wall.checked_add_signed(behind - late).unwrap();
            delay.push(CameraClock::SenderReports, captured, arrival(start, wall));
            delay.summary(start, None, Some(&clock)).unwrap()
        };
        assert_eq!(
            summary(45_000),
            CameraDelay {
                clock: CameraClock::SenderReports,
                delay_us: Some(45_100),
                signed_us: Some(45_100),
                matches: [COMPANION_NOT_SERVED, measured, not_needed],
            }
        );
        // Dated slightly after they arrive, within the clock's uncertainty: 0.
        assert_eq!(summary(-500).delay_us, Some(0));
        assert_eq!(summary(-500).signed_us, Some(-400));
        // By its own reports' clock, the maintainer's camera dates them
        // 14.65 ms after they arrive.
        let after = summary(-14_750);
        assert_eq!(after.delay_us, None);
        assert_eq!(after.signed_us, Some(-14_650));
        assert_eq!(after.matches, [COMPANION_NOT_SERVED, measured, not_needed]);
        // Over five seconds is no live delay either.
        assert_eq!(summary(6_000_000).delay_us, None);

        // A KLV time stamp is not the server's clock: it stays assumed.
        let mut klv = CaptureDelay::default();
        klv.push(CameraClock::Klv, wall - 80_000, arrival(start, wall));
        let klv = klv.summary(start, None, Some(&clock)).unwrap();
        assert_eq!(klv.delay_us, Some(80_000));
        assert_eq!(
            klv.matches,
            [
                COMPANION_NOT_SERVED,
                ClockMatch {
                    used: false,
                    gap: Some(ClockGap::OtherClock),
                    ..measured
                },
                ClockMatch {
                    used: true,
                    ..not_needed
                },
            ]
        );
    }

    /// 8 days 6 h behind, as Buggy's camera was.
    const BEHIND: i64 = -712_505_489_000;

    /// A picture captured `late` before it arrives by a camera's `clock`,
    /// `BEHIND` this computer's, dated with `companion` first and an RTSP
    /// server that reads 100 µs off ±600 µs (S357).
    fn dated_with(clock: CameraClock, late: i64, companion: &Companion) -> CameraDelay {
        let start = Instant::now();
        let wall = 1_793_491_200_000_000u64;
        let server = server(start, BEHIND, &[500], &[1_200]);
        let mut delay = CaptureDelay::default();
        let captured = wall.checked_add_signed(BEHIND - late).unwrap();
        delay.push(clock, captured, arrival(start, wall));
        delay
            .summary(start, Some(companion), Some(&server))
            .unwrap()
    }

    const RTSP_MEASURED: ClockMatch = ClockMatch {
        way: ClockWay::Rtsp,
        computer_id: None,
        offset_us: Some(BEHIND + 100),
        within_us: Some(600),
        used: false,
        gap: None,
    };

    const NOT_NEEDED: ClockMatch = ClockMatch {
        way: ClockWay::Assumed,
        computer_id: None,
        offset_us: Some(0),
        within_us: None,
        used: false,
        gap: None,
    };

    #[test]
    fn a_companion_computers_timesync_clock_dates_pictures_before_the_servers() {
        // TIMESYNC reads 50 µs off ±200 µs.
        let timesync = Companion {
            computer_id: 192,
            clock: Some(CompanionClock::Timesync(Offset {
                offset_us: BEHIND + 50,
                within_us: 200,
            })),
        };
        let measured = dated_with(CameraClock::SenderReports, 45_000, &timesync);
        assert_eq!(measured.delay_us, Some(45_050));
        assert_eq!(
            measured.matches,
            [
                ClockMatch {
                    way: ClockWay::Companion,
                    computer_id: Some(192),
                    offset_us: Some(BEHIND + 50),
                    within_us: Some(200),
                    used: true,
                    gap: None,
                },
                RTSP_MEASURED,
                NOT_NEEDED,
            ]
        );
        // Its ± is the one that allows a delay below 0.
        let late = |late| dated_with(CameraClock::SenderReports, late, &timesync);
        assert_eq!(late(-150).delay_us, Some(0));
        assert_eq!(
            (late(-400).delay_us, late(-400).signed_us),
            (None, Some(-350))
        );

        // A KLV time stamp is neither computer's clock: it stays assumed,
        // with what TIMESYNC measured shown beside it.
        let klv = dated_with(CameraClock::Klv, 80_000, &timesync);
        assert_eq!(
            klv.matches.map(|m| (m.gap, m.used)),
            [
                (Some(ClockGap::OtherClock), false),
                (Some(ClockGap::OtherClock), false),
                (None, true),
            ]
        );
        assert_eq!(klv.matches[0].offset_us, Some(BEHIND + 50));
    }

    #[test]
    fn a_quiet_or_one_way_companion_leaves_the_pictures_to_the_servers_clock() {
        // Quiet for a minute: the server's clock dates them.
        let quiet = Companion {
            computer_id: 192,
            clock: None,
        };
        let fallback = dated_with(CameraClock::SenderReports, 45_000, &quiet);
        assert_eq!(fallback.delay_us, Some(45_100));
        assert_eq!(
            fallback.matches,
            [
                ClockMatch {
                    way: ClockWay::Companion,
                    computer_id: Some(192),
                    offset_us: None,
                    within_us: None,
                    used: false,
                    gap: Some(ClockGap::NoAnswers),
                },
                ClockMatch {
                    used: true,
                    ..RTSP_MEASURED
                },
                NOT_NEEDED,
            ]
        );

        // Read one way only: listed with its reading, never used.
        let one_way = Companion {
            computer_id: 192,
            clock: Some(CompanionClock::OneWay {
                offset_us: BEHIND - 3_000,
            }),
        };
        let listed = dated_with(CameraClock::SenderReports, 45_000, &one_way);
        assert_eq!(listed.delay_us, Some(45_100));
        assert_eq!(
            listed.matches[0],
            ClockMatch {
                way: ClockWay::Companion,
                computer_id: Some(192),
                offset_us: Some(BEHIND - 3_000),
                within_us: None,
                used: false,
                gap: Some(ClockGap::OneWay),
            }
        );
        assert!(listed.matches[1].used);
    }

    #[test]
    fn a_server_that_dates_its_reports_by_its_pictures_dates_none() {
        let start = Instant::now();
        let wall = 1_793_491_200_000_000u64;
        // Buggy's MediaMTX: its reports read the clock 38 ms further behind.
        let mediamtx = named(Some("gortsplib"), start, BEHIND - 38_000, &[500], &[1_200]);
        let dated = |companion: Option<&Companion>, late: i64| {
            let mut delay = CaptureDelay::default();
            let captured = wall.checked_add_signed(BEHIND - late).unwrap();
            delay.push(CameraClock::SenderReports, captured, arrival(start, wall));
            delay.summary(start, companion, Some(&mediamtx)).unwrap()
        };
        let listed = ClockMatch {
            gap: Some(ClockGap::DatedByPictures),
            offset_us: Some(BEHIND - 38_000 + 100),
            ..RTSP_MEASURED
        };
        // No companion: the clocks are assumed in step, and the measurement
        // is listed with why it is not used.
        let alone = dated(None, 24_000);
        assert_eq!(
            alone.matches,
            [
                COMPANION_NOT_SERVED,
                listed,
                ClockMatch {
                    used: true,
                    ..NOT_NEEDED
                },
            ]
        );
        assert_eq!(alone.delay_us, None, "8 days apart, assumed in step");
        // A companion's clock dates them, 24 ms.
        let timesync = Companion {
            computer_id: 191,
            clock: Some(CompanionClock::Timesync(Offset {
                offset_us: BEHIND,
                within_us: 900,
            })),
        };
        let served = dated(Some(&timesync), 24_000);
        assert_eq!(served.delay_us, Some(24_000));
        assert_eq!(served.matches[1], listed);
        assert!(served.matches[0].used);
        // Before a measurement it says the same.
        let unmeasured = ServerClock::new(Some("gortsplib"));
        let mut delay = CaptureDelay::default();
        delay.push(
            CameraClock::SenderReports,
            wall - 24_000,
            arrival(start, wall),
        );
        let waiting = delay.summary(start, None, Some(&unmeasured)).unwrap();
        assert_eq!(waiting.matches[1].gap, Some(ClockGap::DatedByPictures));
        assert_eq!(waiting.matches[1].offset_us, None);
        assert_eq!(waiting.delay_us, Some(24_000), "in step, assumed");
        // Other servers are measured as before, whatever their case.
        for name in [
            "GStreamer RTSP server",
            "LIVE555 Streaming Media v2024.11.28",
        ] {
            assert!(!ServerClock::new(Some(name)).dates_by_pictures, "{name}");
        }
        assert!(ServerClock::new(Some("GortSPLib")).dates_by_pictures);
    }
}
