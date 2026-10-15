//! What a video datagram carries (ADR-0017 V3): raw MPEG-TS, RTP carrying
//! MPEG-TS (payload type 33, MP2T), or RTP carrying H.264 or H.265 straight
//! (S39), whose codec the source learns from the stream.

use crate::rtp::{self, Header};
use crate::ts::PACKET_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// MPEG-TS packets, whole, as many as fit.
    RawTs,
    /// RTP carrying MPEG-TS.
    RtpTs,
    /// RTP carrying video whose codec is not known yet.
    RtpVideo,
    RtpH264,
    RtpH265,
}

/// A datagram, read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Ts { container: Container, ts: &'a [u8] },
    Rtp { header: Header, payload: &'a [u8] },
}

/// # Errors
/// A datagram that is neither MPEG-TS nor RTP.
pub fn classify(datagram: &[u8]) -> Result<Packet<'_>, &'static str> {
    if datagram.first() == Some(&0x47) && datagram.len().is_multiple_of(PACKET_SIZE) {
        return Ok(Packet::Ts {
            container: Container::RawTs,
            ts: datagram,
        });
    }
    let (header, payload) = rtp::parse(datagram).ok_or("neither MPEG-TS nor RTP")?;
    let carries_ts = header.payload_type == 33
        || (payload.first() == Some(&0x47) && payload.len().is_multiple_of(PACKET_SIZE));
    Ok(if carries_ts {
        Packet::Ts {
            container: Container::RtpTs,
            ts: payload,
        }
    } else {
        Packet::Rtp { header, payload }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ts_rtp_ts_and_rtp_video_are_told_apart() {
        let ts = {
            let mut p = vec![0xffu8; 188 * 2];
            p[0] = 0x47;
            p[188] = 0x47;
            p
        };
        assert!(matches!(
            classify(&ts),
            Ok(Packet::Ts {
                container: Container::RawTs,
                ..
            })
        ));
        let mut rtp = vec![0x80, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        rtp.extend_from_slice(&ts);
        let Ok(Packet::Ts {
            container,
            ts: payload,
        }) = classify(&rtp)
        else {
            panic!("not RTP carrying MPEG-TS")
        };
        assert_eq!(container, Container::RtpTs);
        assert_eq!(payload, &ts[..]);
        assert!(classify(b"hello").is_err());
        let h264 = [0x80, 0xe0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 1, 0x65, 1, 2];
        let Ok(Packet::Rtp { header, payload }) = classify(&h264) else {
            panic!("not RTP video")
        };
        assert_eq!((header.payload_type, header.marker), (96, true));
        assert_eq!(payload, &[0x65, 1, 2]);
    }
}
