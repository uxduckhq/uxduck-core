//! An RTSP camera's sound track in RTP (ADR-0017 V22, S149): AAC as
//! `mpeg4-generic` (RFC 3640, AU headers, an AU split over packets) or
//! `MP4A-LATM` (RFC 6416, its `StreamMuxConfig` in the SDP or in the
//! stream), and Opus (RFC 7587, one packet each). Frames come out with
//! their RTP timestamps at the track's clock; lining them up with the
//! pictures is `rtcp.rs`'s. Sound in another codec is named, not kept.

use crate::aac::{AacConfig, SAMPLES_PER_FRAME, read_audio_specific_config};
use crate::audio::AudioConfig;
use crate::bits::BitReader;
use crate::opus::{OpusConfig, packet_samples};
use crate::rtp::Header;

/// How a track's RTP carries its sound, from its SDP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// RFC 3640's AU headers, sized as the `fmtp` says.
    Mpeg4Generic {
        config: AacConfig,
        size_length: u32,
        index_length: u32,
        index_delta_length: u32,
        cts_delta_length: u32,
        dts_delta_length: u32,
    },
    /// LATM, with its `StreamMuxConfig` from the SDP (`cpresent=0`) or in
    /// the stream.
    Latm {
        config: Option<LatmConfig>,
    },
    Opus(OpusConfig),
}

/// What a LATM `StreamMuxConfig` says, of what cameras send: one program,
/// one layer, frames of a length given in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatmConfig {
    pub config: AacConfig,
    /// Frames in each `AudioMuxElement`.
    sub_frames: u32,
    /// Bits of other data after the frames.
    other_data_bits: u32,
}

fn latm_value(r: &mut BitReader<'_>) -> Option<u32> {
    let bytes = r.u(2).ok()?;
    let mut value = 0u32;
    for _ in 0..=bytes {
        value = (value << 8) | r.u(8).ok()?;
    }
    Some(value)
}

/// A `StreamMuxConfig` (ISO/IEC 14496-3 1.7.3), versions 0 and 1.
fn stream_mux_config(r: &mut BitReader<'_>) -> Option<LatmConfig> {
    let version = r.u(1).ok()?;
    if version == 1 {
        if r.u(1).ok()? == 1 {
            return None;
        }
        latm_value(r)?;
    }
    let same_time_framing = r.u(1).ok()?;
    let sub_frames = r.u(6).ok()? + 1;
    let programs = r.u(4).ok()?;
    let layers = r.u(3).ok()?;
    if same_time_framing == 0 || programs != 0 || layers != 0 {
        return None;
    }
    let config = if version == 0 {
        read_audio_specific_config(r)?
    } else {
        let length = usize::try_from(latm_value(r)?).ok()?;
        let start = r.position();
        let config = read_audio_specific_config(r)?;
        r.skip((start + length).saturating_sub(r.position()));
        config
    };
    // Frames whose lengths are given in bytes, then the buffer's fullness.
    if r.u(3).ok()? != 0 {
        return None;
    }
    r.skip(8);
    let mut other_data_bits = 0;
    if r.u(1).ok()? == 1 {
        if version == 1 {
            other_data_bits = latm_value(r)?;
        } else {
            loop {
                let more = r.u(1).ok()?;
                other_data_bits = (other_data_bits << 8) | r.u(8).ok()?;
                if more == 0 {
                    break;
                }
            }
        }
    }
    if r.u(1).ok()? == 1 {
        r.skip(8);
    }
    Some(LatmConfig {
        config,
        sub_frames,
        other_data_bits,
    })
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What sound an SDP's `a=rtpmap` encoding is when UXDuck does not record
/// it, for "UXDuck doesn't record this sound".
fn other_name(encoding: &str) -> String {
    let upper = encoding.to_ascii_uppercase();
    match upper.as_str() {
        "PCMU" => "G.711 µ-law".into(),
        "PCMA" => "G.711 A-law".into(),
        "G722" => "G.722".into(),
        "L8" | "L16" | "L24" => "PCM".into(),
        "MPA" => "MPEG audio".into(),
        "AC3" => "AC-3".into(),
        "EAC3" => "E-AC-3".into(),
        _ if upper.starts_with("G726") || upper.starts_with("AAL2-G726") => "G.726".into(),
        _ => encoding.to_owned(),
    }
}

/// A sound track's format from its SDP: the `a=rtpmap` encoding name, its
/// `a=fmtp` parameters as written, and how many channels the `a=rtpmap`
/// gives. A codec UXDuck does not record, or AAC described in a way it
/// cannot read, is `Err` with what to call it.
///
/// # Errors
/// The sound's name when it is not recorded.
pub fn format(encoding: &str, fmtp: &str) -> Result<Format, String> {
    let params: Vec<(String, &str)> = fmtp
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
        .collect();
    let param = |key: &str| params.iter().find_map(|(k, v)| (k == key).then_some(*v));
    let number = |key: &str| param(key).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    match encoding.to_ascii_uppercase().as_str() {
        "MPEG4-GENERIC" => {
            let mode = param("mode").unwrap_or("").to_ascii_lowercase();
            let config = param("config")
                .and_then(hex)
                .and_then(|c| AacConfig::from_audio_specific_config(&c));
            match config {
                Some(config) if mode.starts_with("aac") && number("sizelength") > 0 => {
                    Ok(Format::Mpeg4Generic {
                        config,
                        size_length: number("sizelength"),
                        index_length: number("indexlength"),
                        index_delta_length: number("indexdeltalength"),
                        cts_delta_length: number("ctsdeltalength"),
                        dts_delta_length: number("dtsdeltalength"),
                    })
                }
                _ => Err("AAC".into()),
            }
        }
        "MP4A-LATM" => {
            // `cpresent` is 1 unless the SDP says 0.
            if param("cpresent") == Some("0") {
                let config = param("config")
                    .and_then(hex)
                    .and_then(|c| stream_mux_config(&mut BitReader::new(&c, 0)));
                match config {
                    Some(config) => Ok(Format::Latm {
                        config: Some(config),
                    }),
                    None => Err("AAC in LATM".into()),
                }
            } else {
                Ok(Format::Latm { config: None })
            }
        }
        "OPUS" => Ok(Format::Opus(OpusConfig {
            channels: if param("sprop-stereo") == Some("1") {
                2
            } else {
                1
            },
        })),
        _ => Err(other_name(encoding)),
    }
}

/// One frame of sound and its RTP timestamp, at the track's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub timestamp: u32,
    pub data: Vec<u8>,
    /// How long it plays at the sound's own rate.
    pub samples: u32,
}

/// An AU of `mpeg4-generic` arriving over several packets.
#[derive(Debug)]
struct Fragment {
    timestamp: u32,
    size: usize,
    data: Vec<u8>,
}

/// Puts one sound track's frames back together.
#[derive(Debug)]
pub struct AudioDepacketizer {
    format: Format,
    /// The track's RTP clock, from its `a=rtpmap`.
    clock_rate: u32,
    next_sequence: Option<u16>,
    fragment: Option<Fragment>,
    /// LATM's packets of one element, until the marker.
    gathered: Vec<u8>,
    gathered_timestamp: Option<u32>,
    gathered_broken: bool,
    /// LATM whose configuration comes in the stream (`cpresent=1`).
    latm_in_band: bool,
    /// Packets missing by sequence number.
    pub lost: u64,
}

impl AudioDepacketizer {
    #[must_use]
    pub fn new(format: Format, clock_rate: u32) -> Self {
        Self {
            format,
            clock_rate: clock_rate.max(1),
            next_sequence: None,
            fragment: None,
            gathered: Vec::new(),
            gathered_timestamp: None,
            gathered_broken: false,
            latm_in_band: matches!(format, Format::Latm { config: None }),
            lost: 0,
        }
    }

    /// What the sound is, once known: from the SDP, or for LATM with its
    /// configuration in the stream, from the first element.
    #[must_use]
    pub const fn config(&self) -> Option<AudioConfig> {
        match self.format {
            Format::Mpeg4Generic { config, .. } => Some(AudioConfig::Aac(config)),
            Format::Latm { config: Some(c) } => Some(AudioConfig::Aac(c.config)),
            Format::Latm { config: None } => None,
            Format::Opus(config) => Some(AudioConfig::Opus(config)),
        }
    }

    /// A packet; the frames it completes.
    pub fn push(&mut self, header: &Header, payload: &[u8]) -> Vec<Frame> {
        let lost = self
            .next_sequence
            .map_or(0, |next| header.sequence.wrapping_sub(next));
        // A packet from the past, repeated or reordered, is left out.
        if lost >= 0x8000 {
            return Vec::new();
        }
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        self.lost += u64::from(lost);
        match self.format {
            Format::Mpeg4Generic { .. } => self.generic(header, payload, lost > 0),
            Format::Latm { .. } => self.latm(header, payload, lost > 0),
            Format::Opus(_) => packet_samples(payload)
                .map(|samples| Frame {
                    timestamp: header.timestamp,
                    data: payload.to_vec(),
                    samples,
                })
                .into_iter()
                .collect(),
        }
    }

    /// The RTP timestamp of the `n`th AAC frame after one at `timestamp`.
    fn aac_timestamp(&self, timestamp: u32, n: usize, config: AacConfig) -> u32 {
        let ticks = n as u64 * u64::from(SAMPLES_PER_FRAME) * u64::from(self.clock_rate)
            / u64::from(config.sample_rate.max(1));
        #[allow(clippy::cast_possible_truncation, reason = "RTP time wraps")]
        timestamp.wrapping_add(ticks as u32)
    }

    fn generic(&mut self, header: &Header, payload: &[u8], lost: bool) -> Vec<Frame> {
        let Format::Mpeg4Generic {
            config,
            size_length,
            index_length,
            index_delta_length,
            cts_delta_length,
            dts_delta_length,
        } = self.format
        else {
            return Vec::new();
        };
        let (Some(&high), Some(&low)) = (payload.first(), payload.get(1)) else {
            return Vec::new();
        };
        let header_bits = usize::from(u16::from_be_bytes([high, low]));
        let Some(headers) = payload.get(2..2 + header_bits.div_ceil(8)) else {
            return Vec::new();
        };
        let data = &payload[2 + headers.len()..];
        let mut r = BitReader::new(headers, 0);
        let mut sizes = Vec::new();
        while r.position() < header_bits {
            let Ok(size) = r.u(size_length) else { break };
            let index_bits = if sizes.is_empty() {
                index_length
            } else {
                index_delta_length
            };
            r.skip(index_bits as usize);
            for delta in [cts_delta_length, dts_delta_length] {
                if delta > 0 && r.u(1) == Ok(1) {
                    r.skip(delta as usize);
                }
            }
            sizes.push(size as usize);
        }
        // The rest of an AU split over packets: same timestamp, nothing lost.
        if let Some(mut fragment) = self.fragment.take()
            && !lost
            && fragment.timestamp == header.timestamp
            && sizes.len() == 1
        {
            fragment.data.extend_from_slice(data);
            if fragment.data.len() < fragment.size && !header.marker {
                self.fragment = Some(fragment);
                return Vec::new();
            }
            if fragment.data.len() >= fragment.size {
                fragment.data.truncate(fragment.size);
                return vec![Frame {
                    timestamp: fragment.timestamp,
                    data: fragment.data,
                    samples: SAMPLES_PER_FRAME,
                }];
            }
            return Vec::new();
        }
        if let [size] = sizes[..]
            && size > data.len()
        {
            self.fragment = Some(Fragment {
                timestamp: header.timestamp,
                size,
                data: data.to_vec(),
            });
            return Vec::new();
        }
        let mut frames = Vec::with_capacity(sizes.len());
        let mut at = 0;
        for (n, size) in sizes.into_iter().enumerate() {
            let Some(au) = data.get(at..at + size) else {
                break;
            };
            at += size;
            frames.push(Frame {
                timestamp: self.aac_timestamp(header.timestamp, n, config),
                data: au.to_vec(),
                samples: SAMPLES_PER_FRAME,
            });
        }
        frames
    }

    fn latm(&mut self, header: &Header, payload: &[u8], lost: bool) -> Vec<Frame> {
        // A new timestamp starts a new element: the last never got its marker.
        if self
            .gathered_timestamp
            .is_some_and(|t| t != header.timestamp)
        {
            self.gathered.clear();
            self.gathered_broken = false;
        }
        self.gathered_broken |= lost;
        self.gathered_timestamp = Some(header.timestamp);
        self.gathered.extend_from_slice(payload);
        if !header.marker {
            return Vec::new();
        }
        let element = std::mem::take(&mut self.gathered);
        let broken = std::mem::take(&mut self.gathered_broken);
        self.gathered_timestamp = None;
        if broken {
            return Vec::new();
        }
        self.elements(&element, header.timestamp)
    }

    /// `AudioMuxElement`s, one after another, each byte-aligned: with the
    /// configuration in the stream, each may bring a new one first.
    fn elements(&mut self, data: &[u8], timestamp: u32) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut r = BitReader::new(data, 0);
        while r.left() >= 8 {
            if self.latm_in_band && r.u(1) == Ok(0) {
                // useSameStreamMux is 0: a configuration comes first.
                let config = stream_mux_config(&mut r);
                self.format = Format::Latm { config };
                if config.is_none() {
                    return frames;
                }
            }
            let Format::Latm { config: Some(latm) } = self.format else {
                return frames;
            };
            for _ in 0..latm.sub_frames {
                let mut length = 0usize;
                loop {
                    let Ok(byte) = r.u(8) else { return frames };
                    length += byte as usize;
                    if byte != 255 {
                        break;
                    }
                }
                if r.left() < length * 8 {
                    return frames;
                }
                let frame = if r.position().is_multiple_of(8) {
                    let at = r.position() / 8;
                    r.skip(length * 8);
                    data[at..at + length].to_vec()
                } else {
                    (0..length)
                        .map(|_| u8::try_from(r.u(8).unwrap_or(0)).unwrap_or(0))
                        .collect()
                };
                frames.push(Frame {
                    timestamp: self.aac_timestamp(timestamp, frames.len(), latm.config),
                    data: frame,
                    samples: SAMPLES_PER_FRAME,
                });
            }
            r.skip(latm.other_data_bits as usize);
            r.align();
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: AacConfig = AacConfig {
        object_type: 2,
        frequency_index: 3,
        sample_rate: 48_000,
        channels: 2,
    };

    fn format_generic() -> Format {
        format(
            "MPEG4-GENERIC",
            "mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190",
        )
        .unwrap()
    }

    fn header(sequence: u16, timestamp: u32, marker: bool) -> Header {
        Header {
            payload_type: 97,
            marker,
            sequence,
            timestamp,
            ssrc: 1,
        }
    }

    /// One AU header of AAC-hbr (13 bits of size, 3 of index) per size.
    fn au_headers(sizes: &[usize]) -> Vec<u8> {
        let bits = sizes.len() * 16;
        let mut out = u16::try_from(bits).unwrap().to_be_bytes().to_vec();
        for size in sizes {
            out.extend_from_slice(&(u16::try_from(*size).unwrap() << 3).to_be_bytes());
        }
        out
    }

    #[test]
    fn an_au_split_over_packets_is_joined_and_one_missing_a_packet_is_dropped() {
        let au: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let mut d = AudioDepacketizer::new(format_generic(), 48_000);
        let part =
            |range: std::ops::Range<usize>| [au_headers(&[3000]), au[range].to_vec()].concat();
        assert!(d.push(&header(1, 100, false), &part(0..1200)).is_empty());
        assert!(d.push(&header(2, 100, false), &part(1200..2400)).is_empty());
        let joined = d.push(&header(3, 100, true), &part(2400..3000));
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].data, au);
        assert_eq!(joined[0].timestamp, 100);
        // The middle packet lost: nothing, and the next whole AU comes through.
        assert!(d.push(&header(4, 1124, false), &part(0..1200)).is_empty());
        assert!(d.push(&header(6, 1124, true), &part(2400..3000)).is_empty());
        assert_eq!(d.lost, 1);
        let next = d.push(
            &header(7, 2148, true),
            &[au_headers(&[2, 1]), vec![9, 9, 8]].concat(),
        );
        assert_eq!(
            next.iter().map(|f| f.data.clone()).collect::<Vec<_>>(),
            [vec![9, 9], vec![8]]
        );
        assert_eq!(next[1].timestamp, 2148 + 1024);
        // A late, repeated packet is left out.
        assert!(d.push(&header(5, 1124, true), &part(0..10)).is_empty());
    }

    /// Writes bits most significant first.
    #[derive(Default)]
    struct Bits {
        bytes: Vec<u8>,
        used: usize,
    }

    impl Bits {
        fn put(&mut self, value: u32, bits: u32) {
            for i in (0..bits).rev() {
                if self.used.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = u8::try_from((value >> i) & 1).unwrap();
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.used % 8);
                self.used += 1;
            }
        }
    }

    #[test]
    fn latm_with_its_configuration_in_the_stream_learns_it_from_the_first_element() {
        let mut d = AudioDepacketizer::new(format("MP4A-LATM", "cpresent=1").unwrap(), 48_000);
        assert_eq!(d.config(), None);
        // useSameStreamMux 0, then the StreamMuxConfig of the capture's SDP
        // (44 bits), a frame's length and the frame, unaligned.
        let element = |with_config: bool, frame: &[u8]| {
            let mut b = Bits::default();
            b.put(u32::from(!with_config), 1);
            if with_config {
                for (value, bits) in [
                    (0, 1),
                    (1, 1),
                    (0, 6),
                    (0, 4),
                    (0, 3),
                    (2, 5),
                    (3, 4),
                    (2, 4),
                ] {
                    b.put(value, bits);
                }
                for (value, bits) in [(0, 3), (0, 3), (0xff, 8), (0, 1), (0, 1)] {
                    b.put(value, bits);
                }
            }
            let mut length = frame.len();
            while length >= 255 {
                b.put(255, 8);
                length -= 255;
            }
            b.put(u32::try_from(length).unwrap(), 8);
            for byte in frame {
                b.put(u32::from(*byte), 8);
            }
            b.bytes
        };
        // Before any configuration, an element that says "the same" is left out.
        assert!(
            d.push(&header(1, 0, true), &element(false, &[1]))
                .is_empty()
        );
        let first = d.push(&header(2, 1024, true), &element(true, &[0x21, 0x10, 0x05]));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].data, [0x21, 0x10, 0x05]);
        assert_eq!(d.config(), Some(AudioConfig::Aac(STEREO_48K)));
        let next = d.push(&header(3, 2048, true), &element(false, &[7; 300]));
        assert_eq!(next[0].data, vec![7; 300]);
    }

    #[test]
    fn sound_uxduck_does_not_record_is_named() {
        assert_eq!(format("PCMU", ""), Err("G.711 µ-law".into()));
        assert_eq!(format("PCMA", ""), Err("G.711 A-law".into()));
        assert_eq!(format("G726-32", ""), Err("G.726".into()));
        assert_eq!(format("L16", ""), Err("PCM".into()));
        assert_eq!(format("speex", ""), Err("speex".into()));
        // AAC the SDP does not describe well enough.
        assert_eq!(format("MPEG4-GENERIC", "mode=AAC-hbr"), Err("AAC".into()));
        assert_eq!(format("MP4A-LATM", "cpresent=0"), Err("AAC in LATM".into()));
        // HE-AAC with its SBR signalled: AAC LC at the core rate.
        let he = format(
            "MPEG4-GENERIC",
            "mode=AAC-hbr;sizelength=13;config=2B118800",
        )
        .unwrap();
        assert!(matches!(
            he,
            Format::Mpeg4Generic { config, .. } if config.object_type == 2 && config.sample_rate == 24_000
        ));
    }
}
