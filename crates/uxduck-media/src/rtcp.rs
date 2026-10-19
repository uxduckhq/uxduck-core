//! Lining an RTSP camera's sound up with its pictures (ADR-0017 V22,
//! S149). The two tracks' RTP clocks start at random values and run at
//! their own rates, so a sound frame's time on the pictures' clock comes
//! from a moment known on both: the `RTP-Info` PLAY answered with (each
//! track's timestamp at the start), then each track's RTCP sender reports
//! (its timestamp at a wall-clock time, RFC 3550 section 6.4.1), which
//! keep the two in step for as long as the camera plays. A camera that
//! sends neither has its first sound placed at the latest picture.

/// A sender report: its stream, and its RTP timestamp at an NTP time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport {
    pub ssrc: u32,
    /// NTP time: seconds since 1900 in the high 32 bits, a fraction in the low.
    pub ntp: u64,
    pub rtp: u32,
}

/// The sender report in a compound RTCP packet, if it has one.
#[must_use]
pub fn sender_report(mut packet: &[u8]) -> Option<SenderReport> {
    while packet.len() >= 4 {
        if packet[0] >> 6 != 2 {
            return None;
        }
        let length = (usize::from(u16::from_be_bytes([packet[2], packet[3]])) + 1) * 4;
        let this = packet.get(..length)?;
        if this[1] == 200 && this.len() >= 20 {
            let word = |at: usize| {
                u32::from_be_bytes([this[at], this[at + 1], this[at + 2], this[at + 3]])
            };
            return Some(SenderReport {
                ssrc: word(4),
                ntp: (u64::from(word(8)) << 32) | u64::from(word(12)),
                rtp: word(16),
            });
        }
        packet = &packet[length..];
    }
    None
}

/// How the sound was lined up with the pictures, best last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Alignment {
    /// The first sound at the latest picture: the camera said nothing.
    Arrival,
    /// From the `RTP-Info` PLAY answered with.
    RtpInfo,
    /// From both tracks' sender reports.
    SenderReports,
}

/// A video and a sound RTP timestamp of the same moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pair {
    video: u32,
    audio: u32,
}

/// The video's RTP clock: 90 kHz.
const VIDEO_RATE: i64 = 90_000;

/// Maps a sound track's RTP timestamps onto the video's.
#[derive(Debug)]
pub struct AudioSync {
    rate: i64,
    pair: Option<(Pair, Alignment)>,
    video_report: Option<SenderReport>,
    audio_report: Option<SenderReport>,
}

impl AudioSync {
    /// For a sound track whose RTP clock runs at `rate`.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            rate: i64::from(rate.max(1)),
            pair: None,
            video_report: None,
            audio_report: None,
        }
    }

    /// How the sound is lined up now, if it is.
    #[must_use]
    pub fn alignment(&self) -> Option<Alignment> {
        self.pair.map(|(_, how)| how)
    }

    fn set(&mut self, pair: Pair, how: Alignment) {
        if self.pair.is_none_or(|(_, now)| how >= now) {
            self.pair = Some((pair, how));
        }
    }

    /// Both tracks' timestamps at the start, from PLAY's `RTP-Info`.
    pub fn rtp_info(&mut self, video: u32, audio: u32) {
        self.set(Pair { video, audio }, Alignment::RtpInfo);
    }

    /// The video track's sender report.
    pub fn video_report(&mut self, report: SenderReport) {
        self.video_report = Some(report);
        self.pair_reports();
    }

    /// The sound track's sender report.
    pub fn audio_report(&mut self, report: SenderReport) {
        self.audio_report = Some(report);
        self.pair_reports();
    }

    /// The sound's timestamp at the video report's moment, from the
    /// latest report of each.
    fn pair_reports(&mut self) {
        let (Some(video), Some(audio)) = (self.video_report, self.audio_report) else {
            return;
        };
        #[allow(clippy::cast_possible_wrap, reason = "NTP times a few seconds apart")]
        let apart = video.ntp.wrapping_sub(audio.ntp) as i64;
        let ticks = i128::from(apart) * i128::from(self.rate) / (1i128 << 32);
        #[allow(clippy::cast_possible_truncation, reason = "RTP time wraps")]
        let audio_rtp = audio.rtp.wrapping_add_signed(ticks as i32);
        self.set(
            Pair {
                video: video.rtp,
                audio: audio_rtp,
            },
            Alignment::SenderReports,
        );
    }

    /// With nothing from the camera: this sound timestamp is at this
    /// video timestamp, the latest picture's.
    pub fn arrival(&mut self, video: u32, audio: u32) {
        if self.pair.is_none() {
            self.pair = Some((Pair { video, audio }, Alignment::Arrival));
        }
    }

    /// A sound RTP timestamp on the video's RTP clock, once lined up.
    #[must_use]
    pub fn video_timestamp(&self, audio: u32) -> Option<u32> {
        let (pair, _) = self.pair?;
        #[allow(clippy::cast_possible_wrap, reason = "the distance from the pair")]
        let since = i64::from(audio.wrapping_sub(pair.audio) as i32);
        let video_ticks = since * VIDEO_RATE / self.rate;
        #[allow(clippy::cast_possible_truncation, reason = "RTP time wraps")]
        Some(pair.video.wrapping_add_signed(video_ticks as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second as NTP's fraction.
    const SECOND: u64 = 1 << 32;

    fn report(ntp: u64, rtp: u32) -> SenderReport {
        SenderReport { ssrc: 1, ntp, rtp }
    }

    #[test]
    fn ffmpegs_sender_report_is_read() {
        // A sender report from ffmpeg's RTP muxer, captured on its RTCP port.
        let packet = [
            0x80, 0xc8, 0x00, 0x06, 0x1b, 0xc4, 0xfa, 0x7f, 0xee, 0x7f, 0xda, 0x34, 0x5f, 0xbe,
            0x76, 0xc8, 0x8e, 0x00, 0xcc, 0x36, 0x00, 0x00, 0x01, 0x13, 0x00, 0x00, 0xd8, 0x55,
        ];
        let sr = sender_report(&packet).unwrap();
        assert_eq!(sr.ssrc, 0x1bc4_fa7f);
        assert_eq!(sr.ntp >> 32, 0xee7f_da34);
        assert_eq!(sr.rtp, 0x8e00_cc36);
        // A receiver report then a sender report in one compound packet.
        let rr = [0x80, 201, 0, 1, 0, 0, 0, 9];
        assert_eq!(sender_report(&[&rr[..], &packet].concat()), Some(sr));
        assert_eq!(sender_report(&rr), None);
        assert_eq!(sender_report(&packet[..10]), None);
    }

    #[test]
    fn sender_reports_line_the_sound_up_across_the_wrap() {
        let mut sync = AudioSync::new(48_000);
        assert_eq!(sync.video_timestamp(0), None);
        // The video's clock at 1000 s of wall time; the sound's report is
        // half a second later, its clock about to wrap.
        sync.video_report(report(1000 * SECOND, 4_000_000));
        assert_eq!(sync.alignment(), None);
        sync.audio_report(report(1000 * SECOND + SECOND / 2, u32::MAX - 23_999));
        assert_eq!(sync.alignment(), Some(Alignment::SenderReports));
        // At the video's report the sound read u32::MAX - 47_999; a second
        // of sound later, past the wrap, is 90_000 video ticks later.
        assert_eq!(sync.video_timestamp(u32::MAX - 47_999), Some(4_000_000));
        assert_eq!(sync.video_timestamp(0), Some(4_090_000));
        assert_eq!(sync.video_timestamp(u32::MAX - 71_999), Some(3_955_000));
    }

    #[test]
    fn rtp_info_lines_up_at_once_and_reports_take_over() {
        let mut sync = AudioSync::new(8_000);
        sync.arrival(500, 7);
        assert_eq!(sync.alignment(), Some(Alignment::Arrival));
        sync.rtp_info(90_000, 16_000);
        assert_eq!(sync.alignment(), Some(Alignment::RtpInfo));
        assert_eq!(sync.video_timestamp(24_000), Some(180_000));
        // The camera's reports say the sound is 100 ms later than RTP-Info had it.
        sync.video_report(report(SECOND, 90_000));
        sync.audio_report(report(SECOND, 16_800));
        assert_eq!(sync.video_timestamp(24_000), Some(171_000));
        // Arrival never overrides either.
        sync.arrival(0, 0);
        assert_eq!(sync.alignment(), Some(Alignment::SenderReports));
        sync.rtp_info(0, 0);
        assert_eq!(sync.video_timestamp(24_000), Some(171_000));
    }
}
