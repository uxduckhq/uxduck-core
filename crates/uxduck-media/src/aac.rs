//! A camera's AAC sound in MPEG-TS (ADR-0017 V22, S147): ADTS frames split
//! out of a PES, each 1024 samples, kept as the raw frames an MP4 holds,
//! with the `AudioSpecificConfig` its `esds` needs. Never decoded.

use crate::bits::{BitReader, OutOfBits};

/// Sampling rates by ADTS's frequency index.
const RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Samples in one AAC frame.
pub const SAMPLES_PER_FRAME: u32 = 1024;

/// What an AAC stream is: its object type, rate and channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// MPEG-4 audio object type: 2 for AAC LC.
    pub object_type: u8,
    pub frequency_index: u8,
    pub sample_rate: u32,
    /// ADTS's channel configuration: 1 mono, 2 stereo, up to 7 (7.1).
    pub channels: u8,
}

impl AacConfig {
    /// The two-byte `AudioSpecificConfig`: object type, frequency index and
    /// channel configuration.
    #[must_use]
    pub fn audio_specific_config(&self) -> [u8; 2] {
        [
            (self.object_type << 3) | (self.frequency_index >> 1),
            ((self.frequency_index & 1) << 7) | (self.channels << 3),
        ]
    }

    /// The ADTS header for a raw frame of `length` bytes, without a CRC:
    /// how AAC goes into MPEG-TS (stream type 0x0f, S149).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, reason = "the length's bits, masked")]
    pub fn adts_header(&self, length: usize) -> [u8; 7] {
        let total = length + 7;
        [
            0xFF,
            0xF1,
            ((self.object_type - 1) << 6) | (self.frequency_index << 2) | (self.channels >> 2),
            ((self.channels & 3) << 6) | ((total >> 11) as u8 & 0x03),
            (total >> 3) as u8,
            ((total as u8 & 0x07) << 5) | 0x1F,
            0xFC,
        ]
    }

    /// "AAC, 48 kHz, stereo".
    #[must_use]
    pub fn describe(&self) -> String {
        let rate = if self.sample_rate.is_multiple_of(1000) {
            format!("{} kHz", self.sample_rate / 1000)
        } else {
            format!("{:.1} kHz", f64::from(self.sample_rate) / 1000.0)
        };
        let channels = match self.channels {
            1 => "mono".to_owned(),
            2 => "stereo".to_owned(),
            n => format!("{n} channels"),
        };
        format!("AAC, {rate}, {channels}")
    }

    /// How long a frame lasts, in microseconds.
    #[must_use]
    pub fn frame_us(&self) -> f64 {
        f64::from(SAMPLES_PER_FRAME) * 1_000_000.0 / f64::from(self.sample_rate)
    }
}

pub use crate::audio::AudioUnit;

/// An escaped audio object type: 5 bits, or 6 more past 31.
fn object_type(r: &mut BitReader<'_>) -> Result<u32, OutOfBits> {
    let t = r.u(5)?;
    Ok(if t == 31 { 32 + r.u(6)? } else { t })
}

/// An `AudioSpecificConfig` as RTSP cameras give it (S149), read from `r`
/// up to where a LATM `StreamMuxConfig` goes on: AAC whose rate has an
/// index, with a channel configuration and 1024-sample frames. HE-AAC
/// (object type 5 or 29) is kept as AAC LC at its core rate, which players
/// widen again by finding the SBR in the frames. Anything else is `None`.
pub(crate) fn read_audio_specific_config(r: &mut BitReader<'_>) -> Option<AacConfig> {
    let mut object = object_type(r).ok()?;
    let frequency_index = u8::try_from(r.u(4).ok()?).ok()?;
    if frequency_index == 15 {
        return None;
    }
    let channels = u8::try_from(r.u(4).ok()?).ok()?;
    if object == 5 || object == 29 {
        if r.u(4).ok()? == 15 {
            r.skip(24);
        }
        object = object_type(r).ok()?;
    }
    // AAC main, LC, SSR and LTP, whose GASpecificConfig follows.
    if !(1..=4).contains(&object) || channels == 0 || channels > 7 {
        return None;
    }
    let frame_length_flag = r.u(1).ok()?;
    if r.u(1).ok()? == 1 {
        // dependsOnCoreCoder, then its delay.
        r.skip(14);
    }
    let _extension_flag = r.u(1).ok()?;
    // 960-sample frames would need their own lengths.
    if frame_length_flag == 1 {
        return None;
    }
    Some(AacConfig {
        object_type: u8::try_from(object).ok()?,
        frequency_index,
        sample_rate: *RATES.get(usize::from(frequency_index))?,
        channels,
    })
}

impl AacConfig {
    /// From an `AudioSpecificConfig`'s bytes, such as an SDP's `config=`
    /// (S149); anything after the fields UXDuck keeps is ignored.
    #[must_use]
    pub fn from_audio_specific_config(bytes: &[u8]) -> Option<Self> {
        read_audio_specific_config(&mut BitReader::new(bytes, 0))
    }
}

/// An ADTS header at the start of `b`: the stream's config, the header's
/// length and the whole frame's.
fn header(b: &[u8]) -> Option<(AacConfig, usize, usize)> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = b[1] & 1 == 1;
    let profile = b[2] >> 6;
    let frequency_index = (b[2] >> 2) & 0x0F;
    let channels = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame_length =
        (usize::from(b[3] & 0x03) << 11) | (usize::from(b[4]) << 3) | usize::from(b[5] >> 5);
    let raw_blocks = b[6] & 0x03;
    let header_length = if protection_absent { 7 } else { 9 };
    let sample_rate = *RATES.get(usize::from(frequency_index))?;
    // More than one raw block a frame, or a channel layout given in-band,
    // is not what cameras send; such a frame is skipped.
    if raw_blocks != 0 || channels == 0 || frame_length <= header_length {
        return None;
    }
    Some((
        AacConfig {
            object_type: profile + 1,
            frequency_index,
            sample_rate,
            channels,
        },
        header_length,
        frame_length,
    ))
}

/// The ADTS frames in a PES payload, in order, each with its config, and
/// how many bytes could not be read as one.
#[must_use]
pub fn adts_frames(data: &[u8]) -> (Vec<(AacConfig, &[u8])>, usize) {
    let mut frames = Vec::new();
    let mut skipped = 0;
    let mut at = 0;
    while at < data.len() {
        match header(&data[at..]) {
            Some((config, header_length, frame_length)) if at + frame_length <= data.len() => {
                frames.push((config, &data[at + header_length..at + frame_length]));
                at += frame_length;
            }
            _ => {
                skipped += 1;
                at += 1;
            }
        }
    }
    (frames, skipped)
}

/// What an MPEG-TS stream type that is sound but not ADTS AAC is called,
/// for "UXDuck doesn't record this sound".
#[must_use]
pub fn other_audio_name(stream_type: u8) -> Option<&'static str> {
    match stream_type {
        0x03 | 0x04 => Some("MPEG audio"),
        0x11 => Some("AAC in LATM"),
        0x81 => Some("AC-3"),
        0x87 => Some("E-AC-3"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ADTS frame of AAC LC, 48 kHz stereo, around `payload`.
    #[allow(clippy::cast_possible_truncation, reason = "the length's bits, masked")]
    fn adts(payload: &[u8]) -> Vec<u8> {
        let length = payload.len() + 7;
        let mut out = vec![
            0xFF,
            0xF1,
            (1 << 6) | (3 << 2),
            (2 << 6) | ((length >> 11) as u8 & 0x03),
            (length >> 3) as u8,
            ((length as u8 & 0x07) << 5) | 0x1F,
            0xFC,
        ];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn frames_are_split_from_a_pes_with_their_config() {
        let pes = [adts(&[1, 2, 3]), adts(&[4, 5])].concat();
        let (frames, skipped) = adts_frames(&pes);
        assert_eq!(skipped, 0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1, &[1, 2, 3]);
        assert_eq!(frames[1].1, &[4, 5]);
        let config = frames[0].0;
        assert_eq!(
            config,
            AacConfig {
                object_type: 2,
                frequency_index: 3,
                sample_rate: 48_000,
                channels: 2
            }
        );
        // AAC LC (2), 48 kHz (3), stereo (2): 0x11 0x90.
        assert_eq!(config.audio_specific_config(), [0x11, 0x90]);
        assert_eq!(config.describe(), "AAC, 48 kHz, stereo");
        assert!((config.frame_us() - 21_333.33).abs() < 0.01);
    }

    #[test]
    fn junk_and_a_frame_cut_short_are_skipped() {
        let mut pes = vec![0x00, 0x12];
        pes.extend(adts(&[9, 9, 9]));
        let mut cut = adts(&[7; 20]);
        cut.truncate(10);
        pes.extend(cut);
        let (frames, skipped) = adts_frames(&pes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, &[9, 9, 9]);
        assert_eq!(skipped, 12);
    }

    #[test]
    fn other_sound_is_named() {
        assert_eq!(other_audio_name(0x03), Some("MPEG audio"));
        assert_eq!(other_audio_name(0x1b), None);
    }
}
