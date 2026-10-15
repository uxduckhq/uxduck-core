//! One sender's stream, datagram by datagram (ADR-0017 V2, V3, V4): what
//! container it is, the demuxer or RTP depacketizer and NAL filter for its
//! video, its KLV, and the counters its source reports. The listener keeps
//! one per sender and does the I/O.

use std::sync::Arc;

use crate::au::{AccessUnit, Filter};
use crate::nal::{Codec, SpsInfo};
use crate::rtp::{Depacketizer, Header};
use crate::ts::{Demuxer, StreamKind};
use crate::udp::{Container, Packet, classify};

/// RTP packets after which a stream whose codec is still unknown is not one
/// UXDuck can read: about two seconds of a camera's video.
const RTP_GIVE_UP_AFTER: u64 = 2000;

/// What a datagram produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// A picture ready for a decoder; shared by every watcher.
    Unit(Arc<AccessUnit>),
    /// A KLV PES payload and its timestamp, with its values when its
    /// checksum holds (S41).
    Klv {
        timestamp_us: u64,
        data: Vec<u8>,
        values: Option<Box<crate::klv::Values>>,
    },
}

/// Counted since the source started, except where it says otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub bytes: u64,
    pub datagrams: u64,
    pub pictures: u64,
    pub keyframes: u64,
    pub lost: u64,
    pub damaged: u64,
    pub pes_length_mismatches: u64,
    pub pes_length_max_excess: i64,
    pub nals_dropped: u64,
    /// KLV local sets read, and those whose checksum failed (S41).
    pub klv: u64,
    pub klv_bad: u64,
}

impl Counters {
    fn add(&mut self, o: &Self) {
        self.bytes += o.bytes;
        self.datagrams += o.datagrams;
        self.pictures += o.pictures;
        self.keyframes += o.keyframes;
        self.lost += o.lost;
        self.damaged += o.damaged;
        self.pes_length_mismatches += o.pes_length_mismatches;
        self.pes_length_max_excess = self.pes_length_max_excess.max(o.pes_length_max_excess);
        self.nals_dropped += o.nals_dropped;
        self.klv += o.klv;
        self.klv_bad += o.klv_bad;
    }
}

/// What a source looks like now.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub container: Option<Container>,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Why nothing can be relayed, when nothing can.
    pub unsupported: Option<String>,
    pub klv: bool,
    pub counters: Counters,
}

/// RTP's 32-bit timestamp, kept increasing and given to the filter as the
/// 33-bit PTS it unwraps.
#[derive(Debug, Default)]
struct RtpClock {
    last: Option<u32>,
    extended: u64,
}

impl RtpClock {
    fn pts(&mut self, timestamp: u32) -> u64 {
        if let Some(last) = self.last {
            #[allow(clippy::cast_possible_wrap, reason = "the step between two packets")]
            let step = i64::from(timestamp.wrapping_sub(last) as i32);
            self.extended = self.extended.saturating_add_signed(step);
        } else {
            self.extended = u64::from(timestamp);
        }
        self.last = Some(timestamp);
        self.extended & ((1 << 33) - 1)
    }
}

/// Whether an SPS read under a codec looks like a real one: a stream read
/// as the wrong codec rarely gives a sane picture size.
fn plausible(sps: Option<&SpsInfo>) -> bool {
    let sane = |v: Option<u32>| v.is_some_and(|v| (16..=8192).contains(&v));
    sps.is_some_and(|s| sane(s.coded_width) && sane(s.coded_height))
}

#[derive(Debug)]
pub struct Source {
    container: Option<Container>,
    demuxer: Demuxer,
    filter: Option<Filter>,
    /// RTP video once its codec is known; the filter is `filter`.
    depacketizer: Option<Depacketizer>,
    /// RTP video while its codec is not known: one try per codec.
    candidates: Vec<(Depacketizer, Filter)>,
    rtp_clock: RtpClock,
    rtp_packets: u64,
    klv_count: u64,
    klv_bad: u64,
    klv: bool,
    unsupported: Option<String>,
    /// Totals from before the last resume.
    base: Counters,
    bytes: u64,
    datagrams: u64,
}

impl Default for Source {
    fn default() -> Self {
        Self::new()
    }
}

impl Source {
    #[must_use]
    pub fn new() -> Self {
        Self {
            container: None,
            demuxer: Demuxer::new(false),
            filter: None,
            depacketizer: None,
            candidates: Vec::new(),
            rtp_clock: RtpClock::default(),
            rtp_packets: 0,
            klv_count: 0,
            klv_bad: 0,
            klv: false,
            unsupported: None,
            base: Counters::default(),
            bytes: 0,
            datagrams: 0,
        }
    }

    /// RTP video whose codec the SDP said, with the parameter sets it gave
    /// (`sprop-…`), for cameras that send them only there (RTSP, S39).
    #[must_use]
    pub fn rtp(codec: Codec, parameter_sets: &[Vec<u8>]) -> Self {
        let mut filter = Filter::new(codec);
        if !parameter_sets.is_empty() {
            let mut sets = Vec::new();
            for set in parameter_sets {
                sets.extend_from_slice(&[0, 0, 0, 1]);
                sets.extend_from_slice(set);
            }
            // No picture, so nothing comes out; the sets are remembered.
            let _ = filter.push(&sets, None, false);
        }
        Self {
            container: Some(match codec {
                Codec::H264 => Container::RtpH264,
                Codec::H265 => Container::RtpH265,
            }),
            depacketizer: Some(Depacketizer::new(codec)),
            filter: Some(filter),
            ..Self::new()
        }
    }

    /// After a silence: fresh parsers, which wait for a keyframe again, and
    /// the counters kept.
    pub fn resume(&mut self) {
        self.base = self.counters();
        self.demuxer = Demuxer::new(false);
        // RTP video keeps the codec it was found to be.
        let codec = self.depacketizer.as_ref().map(|d| d.codec);
        self.depacketizer = codec.map(Depacketizer::new);
        self.filter = codec.map(Filter::new);
        self.candidates.clear();
        self.rtp_clock = RtpClock::default();
        self.rtp_packets = 0;
        self.klv_count = 0;
        self.klv_bad = 0;
        self.bytes = 0;
        self.datagrams = 0;
    }

    /// Another stream in this one's place, such as a camera restarted with
    /// other settings: as `resume`, and the codec is learned again too.
    pub fn restart(&mut self) {
        self.resume();
        self.container = None;
        self.depacketizer = None;
        self.filter = None;
        self.unsupported = None;
    }

    /// Asks for nothing but a keyframe next, such as after a decoder error.
    pub fn wait_for_keyframe(&mut self, reason: &'static str) {
        if let Some(filter) = &mut self.filter {
            filter.wait_for_keyframe(reason);
        }
    }

    pub fn push(&mut self, datagram: &[u8]) -> Vec<Output> {
        self.datagrams += 1;
        self.bytes += datagram.len() as u64;
        let ts = match classify(datagram) {
            Ok(Packet::Ts { container, ts }) => {
                self.container = Some(container);
                ts
            }
            Ok(Packet::Rtp { header, payload }) => return self.push_rtp(&header, payload),
            Err(why) => {
                // One stray datagram among good ones is not the stream.
                if self.container.is_none() {
                    self.unsupported = Some(format!("UXDuck can't play this stream: {why}"));
                }
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for pes in self.demuxer.push(ts) {
            match pes.kind {
                StreamKind::H264 | StreamKind::H265 => {
                    let codec = if pes.kind == StreamKind::H264 {
                        Codec::H264
                    } else {
                        Codec::H265
                    };
                    let filter = match &mut self.filter {
                        Some(f) if f.codec == codec => f,
                        _ => self.filter.insert(Filter::new(codec)),
                    };
                    if let Some(unit) = filter.push(&pes.data, pes.pts, pes.damaged) {
                        out.push(Output::Unit(Arc::new(unit)));
                    }
                }
                StreamKind::Klv => {
                    let timestamp_us = pes.pts.map_or(0, |p| (p * 100 + 4) / 9);
                    out.push(self.klv_unit(timestamp_us, pes.data));
                }
                StreamKind::Other => {}
            }
        }
        self.unsupported = self.why_unsupported();
        out
    }

    /// A KLV unit, from MPEG-TS or an RTSP camera's KLV track (S91): read,
    /// counted, and passed on with its values when its checksum holds.
    pub fn klv_unit(&mut self, timestamp_us: u64, data: Vec<u8>) -> Output {
        self.klv = true;
        // A bad checksum is counted and its values ignored.
        let values = crate::klv::parse(&data).and_then(|k| {
            self.klv_count += 1;
            if k.checksum_ok {
                Some(Box::new(k.values))
            } else {
                self.klv_bad += 1;
                None
            }
        });
        Output::Klv {
            timestamp_us,
            data,
            values,
        }
    }

    fn push_rtp(&mut self, header: &Header, payload: &[u8]) -> Vec<Output> {
        self.rtp_packets += 1;
        let mut out = Vec::new();
        if let (Some(depacketizer), Some(filter)) = (&mut self.depacketizer, &mut self.filter) {
            for picture in depacketizer.push(header, payload) {
                let pts = self.rtp_clock.pts(picture.timestamp);
                if let Some(unit) = filter.push(&picture.annex_b, Some(pts), picture.damaged) {
                    out.push(Output::Unit(Arc::new(unit)));
                }
            }
            return out;
        }
        if self.candidates.is_empty() {
            self.container = Some(Container::RtpVideo);
            self.candidates = [Codec::H264, Codec::H265]
                .into_iter()
                .map(|c| (Depacketizer::new(c), Filter::new(c)))
                .collect();
        }
        let mut chosen = None;
        for (i, (depacketizer, filter)) in self.candidates.iter_mut().enumerate() {
            for picture in depacketizer.push(header, payload) {
                let pts = self.rtp_clock.pts(picture.timestamp);
                let unit = filter.push(&picture.annex_b, Some(pts), picture.damaged);
                if plausible(filter.sps()) {
                    chosen = Some(i);
                    out.extend(unit.map(|u| Output::Unit(Arc::new(u))));
                }
            }
            if chosen.is_some() {
                break;
            }
        }
        if let Some(i) = chosen {
            let (depacketizer, filter) = self.candidates.swap_remove(i);
            self.candidates.clear();
            self.container = Some(match depacketizer.codec {
                Codec::H264 => Container::RtpH264,
                Codec::H265 => Container::RtpH265,
            });
            self.depacketizer = Some(depacketizer);
            self.filter = Some(filter);
            self.unsupported = None;
        } else if self.rtp_packets >= RTP_GIVE_UP_AFTER {
            self.unsupported = Some(
                "RTP that is not H.264, H.265 or MPEG-TS, or has no keyframe with its parameter sets"
                    .to_owned(),
            );
        }
        out
    }

    fn why_unsupported(&self) -> Option<String> {
        let streams = self.demuxer.streams();
        let video = streams
            .values()
            .any(|k| matches!(k, StreamKind::H264 | StreamKind::H265));
        (!streams.is_empty() && !video).then(|| {
            "the stream has no H.264 or H.265 video, which is what UXDuck plays".to_owned()
        })
    }

    #[must_use]
    pub fn counters(&self) -> Counters {
        let d = &self.demuxer.stats;
        let mut now = Counters {
            bytes: self.bytes,
            datagrams: self.datagrams,
            lost: d.continuity_errors
                + d.transport_errors
                + self.depacketizer.as_ref().map_or(0, |r| r.lost),
            pes_length_mismatches: d.pes_length_mismatches,
            pes_length_max_excess: d.pes_length_max_excess,
            klv: self.klv_count,
            klv_bad: self.klv_bad,
            ..Counters::default()
        };
        if let Some(f) = &self.filter {
            now.pictures = f.stats.units;
            now.keyframes = f.stats.keyframes;
            now.damaged = f.stats.damaged;
            now.nals_dropped = f.stats.nals_dropped.values().sum();
        }
        let mut total = self.base.clone();
        total.add(&now);
        total
    }

    #[must_use]
    pub fn status(&self) -> Status {
        let sps = self.filter.as_ref().and_then(|f| f.sps().cloned());
        Status {
            container: self.container,
            codec: sps.as_ref().map(|s| s.codec.clone()),
            width: sps.as_ref().and_then(|s| s.display_width.or(s.coded_width)),
            height: sps
                .as_ref()
                .and_then(|s| s.display_height.or(s.coded_height)),
            unsupported: self.unsupported.clone(),
            klv: self.klv,
            counters: self.counters(),
        }
    }
}
