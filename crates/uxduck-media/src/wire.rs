//! The `/video` socket's binary messages (ADR-0017 V6), version 1. One
//! socket carries one source, so the source is not repeated. Big-endian:
//!
//! ```text
//! 0   u8   version, 1
//! 1   u8   kind: 1 access unit, 2 KLV
//! 2   u8   flags: 1 keyframe, 2 discontinuity (pictures were dropped before it)
//! 3   u8   codec string length, 0 when unchanged since the last message
//! 4   u32  sequence, per socket, from 0
//! 8   u64  timestamp, microseconds
//! 16  ..   codec string (ASCII), then the payload: Annex B, or a KLV PES
//! ```
//!
//! `packages/client/src/video/wire.ts` reads the same bytes; both test the
//! same message.

pub const VERSION: u8 = 1;
pub const HEADER: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    AccessUnit = 1,
    Klv = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    pub kind: Kind,
    pub key: bool,
    pub discontinuity: bool,
    pub sequence: u32,
    pub timestamp_us: u64,
    /// Only when it changed.
    pub codec: Option<&'a str>,
    pub payload: &'a [u8],
}

#[must_use]
pub fn encode(m: &Message<'_>) -> Vec<u8> {
    let codec = m.codec.unwrap_or("").as_bytes();
    let codec = &codec[..codec.len().min(255)];
    let mut out = Vec::with_capacity(HEADER + codec.len() + m.payload.len());
    out.push(VERSION);
    out.push(m.kind as u8);
    out.push(u8::from(m.key) | (u8::from(m.discontinuity) << 1));
    #[allow(clippy::cast_possible_truncation, reason = "clamped to 255 above")]
    out.push(codec.len() as u8);
    out.extend_from_slice(&m.sequence.to_be_bytes());
    out.extend_from_slice(&m.timestamp_us.to_be_bytes());
    out.extend_from_slice(codec);
    out.extend_from_slice(m.payload);
    out
}

/// # Errors
/// A message too short, of another version or of an unknown kind.
pub fn decode(b: &[u8]) -> Result<Message<'_>, &'static str> {
    if b.len() < HEADER {
        return Err("shorter than a header");
    }
    if b[0] != VERSION {
        return Err("another version");
    }
    let kind = match b[1] {
        1 => Kind::AccessUnit,
        2 => Kind::Klv,
        _ => return Err("an unknown kind"),
    };
    let codec_len = usize::from(b[3]);
    let codec = b
        .get(HEADER..HEADER + codec_len)
        .ok_or("shorter than its codec string")?;
    let codec = std::str::from_utf8(codec).map_err(|_| "a codec string that is not text")?;
    Ok(Message {
        kind,
        key: b[2] & 1 != 0,
        discontinuity: b[2] & 2 != 0,
        sequence: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        timestamp_us: u64::from_be_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]),
        codec: (codec_len > 0).then_some(codec),
        payload: &b[HEADER + codec_len..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message `wire.test.ts` decodes too.
    pub const SAMPLE: &str = "0101030400000007000000000000753061766331000000016742";

    #[test]
    fn a_keyframe_with_its_codec_round_trips_and_matches_the_client() {
        let m = Message {
            kind: Kind::AccessUnit,
            key: true,
            discontinuity: true,
            sequence: 7,
            timestamp_us: 30_000,
            codec: Some("avc1"),
            payload: &[0, 0, 0, 1, 0x67, 0x42],
        };
        let bytes = encode(&m);
        let sample: Vec<u8> = (0..SAMPLE.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&SAMPLE[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes, sample);
        assert_eq!(decode(&bytes), Ok(m));
        let klv = Message {
            kind: Kind::Klv,
            key: false,
            discontinuity: false,
            sequence: 8,
            timestamp_us: 1,
            codec: None,
            payload: b"x",
        };
        assert_eq!(decode(&encode(&klv)), Ok(klv));
        assert!(decode(&[1; 3]).is_err());
    }
}
