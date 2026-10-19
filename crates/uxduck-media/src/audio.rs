//! A camera's sound, whatever carried it (ADR-0017 V22): AAC from MPEG-TS
//! (S147) or from an RTSP camera's own track, and Opus from one (S149).
//! Frames are kept as they came, never decoded, each with how long it plays
//! at its track's rate.

use crate::aac::AacConfig;
use crate::opus::OpusConfig;

/// What a sound track is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioConfig {
    Aac(AacConfig),
    Opus(OpusConfig),
}

impl AudioConfig {
    /// The track's rate, what its frames' lengths count: an AAC stream's
    /// sampling rate, and 48 kHz for Opus always.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        match self {
            Self::Aac(c) => c.sample_rate,
            Self::Opus(_) => crate::opus::RATE,
        }
    }

    #[must_use]
    pub const fn channels(&self) -> u8 {
        match self {
            Self::Aac(c) => c.channels,
            Self::Opus(c) => c.channels,
        }
    }

    /// "AAC, 48 kHz, stereo" or "Opus, stereo".
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Aac(c) => c.describe(),
            Self::Opus(c) => c.describe(),
        }
    }
}

impl From<AacConfig> for AudioConfig {
    fn from(config: AacConfig) -> Self {
        Self::Aac(config)
    }
}

/// One frame of sound and when it plays, on the pictures' clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioUnit {
    /// The raw frame: AAC without its ADTS header, or one Opus packet.
    pub data: Vec<u8>,
    pub timestamp_us: u64,
    pub config: AudioConfig,
    /// How long it plays, at [`AudioConfig::sample_rate`].
    pub samples: u32,
}

impl AudioUnit {
    /// An AAC frame, 1024 samples long.
    #[must_use]
    pub const fn aac(data: Vec<u8>, timestamp_us: u64, config: AacConfig) -> Self {
        Self {
            data,
            timestamp_us,
            config: AudioConfig::Aac(config),
            samples: crate::aac::SAMPLES_PER_FRAME,
        }
    }
}
