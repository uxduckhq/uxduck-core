//! Opus from an RTSP camera (S149): packets kept whole, their lengths read
//! from their table-of-contents byte (RFC 6716 section 3.1), and the
//! `dOps` an MP4's `Opus` sample entry needs. Never decoded.

/// Opus always counts at 48 kHz, whatever it was made from.
pub const RATE: u32 = 48_000;

/// What an Opus stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusConfig {
    /// 1 or 2: the SDP's `sprop-stereo` says which the camera sends.
    pub channels: u8,
}

impl OpusConfig {
    /// "Opus, 48 kHz, stereo".
    #[must_use]
    pub fn describe(&self) -> String {
        let channels = if self.channels == 1 { "mono" } else { "stereo" };
        format!("Opus, 48 kHz, {channels}")
    }

    /// The `dOps` box's body (the Opus-in-ISOBMFF mapping, section 4.3.2):
    /// version 0, the channels, no pre-skip, since a recording begins in
    /// the middle of the stream, the rate, no gain, and mapping family 0
    /// (mono or stereo).
    #[must_use]
    pub fn dops(&self) -> Vec<u8> {
        let mut b = vec![0, self.channels];
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&RATE.to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
        b.push(0);
        b
    }
}

/// How long a packet plays, in samples at 48 kHz: its frames' size from its
/// configuration, times how many frames it holds. `None` for an empty
/// packet or one whose count is missing; at most 120 ms, as Opus allows.
#[must_use]
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // 2.5, 5, 10, 20, 40 and 60 ms at 48 kHz.
    let frame = match config {
        // SILK: 10, 20, 40, 60 ms.
        0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
        // Hybrid: 10, 20 ms.
        12..=15 => [480, 960][usize::from(config % 2)],
        // CELT: 2.5, 5, 10, 20 ms.
        _ => [120, 240, 480, 960][usize::from(config % 4)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3f),
    };
    let samples = frame * frames;
    (frames > 0 && samples <= 5760).then_some(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packets_length_comes_from_its_toc() {
        // CELT fullband 20 ms, one frame: what most encoders send.
        assert_eq!(packet_samples(&[0xFC, 1, 2]), Some(960));
        // SILK wideband 60 ms.
        assert_eq!(packet_samples(&[(11 << 3), 1]), Some(2880));
        // Hybrid 10 ms, two frames of equal size.
        assert_eq!(packet_samples(&[(12 << 3) | 1, 1]), Some(960));
        // CELT 2.5 ms, code 3 with six frames.
        assert_eq!(packet_samples(&[(16 << 3) | 3, 6]), Some(720));
        // Code 3 with its count missing, or more than 120 ms.
        assert_eq!(packet_samples(&[(16 << 3) | 3]), None);
        assert_eq!(packet_samples(&[(3 << 3) | 3, 3]), None);
        assert_eq!(packet_samples(&[]), None);
    }

    #[test]
    fn dops_says_the_channels_and_rate() {
        let stereo = OpusConfig { channels: 2 };
        assert_eq!(stereo.dops(), [0, 2, 0, 0, 0x00, 0x00, 0xBB, 0x80, 0, 0, 0]);
        assert_eq!(stereo.describe(), "Opus, 48 kHz, stereo");
        assert_eq!(OpusConfig { channels: 1 }.describe(), "Opus, 48 kHz, mono");
    }
}
