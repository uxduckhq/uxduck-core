//! RTP carrying H.264 (RFC 6184) or H.265 (RFC 7798) straight, as
//! QGroundControl's default 5600 stream does (ADR-0017 V3, S39): a header,
//! pictures put back together from single NAL units, aggregates and
//! fragments, and the reverse for `uxduck dev video-replay` and tests.
//! Without SDP the payload type says nothing, so which codec it is gets
//! learned from the stream (`source.rs`).

use crate::nal::Codec;

/// RTP's fixed header, what UXDuck reads of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub payload_type: u8,
    /// The last packet of a picture.
    pub marker: bool,
    pub sequence: u16,
    /// 90 kHz for video.
    pub timestamp: u32,
    pub ssrc: u32,
}

/// The header and payload of an RTP packet, past any CSRCs, extension and
/// padding.
#[must_use]
pub fn parse(datagram: &[u8]) -> Option<(Header, &[u8])> {
    if datagram.len() < 12 || datagram[0] >> 6 != 2 {
        return None;
    }
    let csrc = usize::from(datagram[0] & 0x0f);
    let mut start = 12 + 4 * csrc;
    if datagram[0] & 0x10 != 0 {
        let length = datagram.get(start + 2..start + 4)?;
        start += 4 + 4 * usize::from(u16::from_be_bytes([length[0], length[1]]));
    }
    let mut end = datagram.len();
    if datagram[0] & 0x20 != 0 {
        end = end.checked_sub(usize::from(*datagram.last()?))?;
    }
    let payload = datagram.get(start..end)?;
    Some((
        Header {
            payload_type: datagram[1] & 0x7f,
            marker: datagram[1] & 0x80 != 0,
            sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
            timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
            ssrc: u32::from_be_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]),
        },
        payload,
    ))
}

/// One picture's NAL units in Annex B, and whether packets went missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub annex_b: Vec<u8>,
    /// 90 kHz, as the packets had it.
    pub timestamp: u32,
    pub damaged: bool,
}

const H264_STAP_A: u8 = 24;
const H264_FU_A: u8 = 28;
const H265_AP: u8 = 48;
const H265_FU: u8 = 49;

/// Puts one codec's pictures back together.
#[derive(Debug)]
pub struct Depacketizer {
    pub codec: Codec,
    nals: Vec<Vec<u8>>,
    fragment: Option<Vec<u8>>,
    timestamp: Option<u32>,
    last_sequence: Option<u16>,
    damaged: bool,
    /// Packets missing by sequence number.
    pub lost: u64,
}

impl Depacketizer {
    #[must_use]
    pub const fn new(codec: Codec) -> Self {
        Self {
            codec,
            nals: Vec::new(),
            fragment: None,
            timestamp: None,
            last_sequence: None,
            damaged: false,
            lost: 0,
        }
    }

    pub fn push(&mut self, header: &Header, payload: &[u8]) -> Vec<Picture> {
        let mut out = Vec::new();
        if let Some(last) = self.last_sequence {
            let gap = header.sequence.wrapping_sub(last).wrapping_sub(1);
            let back = last.wrapping_sub(header.sequence);
            if gap != 0 && gap < 0x8000 {
                // A gap forward: packets lost.
                self.lost += u64::from(gap);
                self.damaged = true;
                self.fragment = None;
            } else if gap >= 0x8000 && back < 100 {
                // A little behind: a late or repeated packet.
                return out;
            } else if gap >= 0x8000 {
                // Far behind: the sender started again.
                self.damaged = true;
                self.fragment = None;
            }
        }
        self.last_sequence = Some(header.sequence);
        // A new timestamp without the last picture's marker: that picture
        // is over.
        if self.timestamp.is_some_and(|t| t != header.timestamp) {
            out.extend(self.finish());
        }
        self.timestamp = Some(header.timestamp);
        match self.codec {
            Codec::H264 => self.h264(payload),
            Codec::H265 => self.h265(payload),
        }
        if header.marker {
            out.extend(self.finish());
        }
        out
    }

    fn finish(&mut self) -> Option<Picture> {
        let timestamp = self.timestamp.take()?;
        let damaged = std::mem::take(&mut self.damaged) || self.fragment.take().is_some();
        if self.nals.is_empty() {
            return None;
        }
        let mut annex_b = Vec::new();
        for nal in self.nals.drain(..) {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&nal);
        }
        Some(Picture {
            annex_b,
            timestamp,
            damaged,
        })
    }

    fn aggregate(&mut self, mut units: &[u8]) {
        while units.len() >= 2 {
            let size = usize::from(u16::from_be_bytes([units[0], units[1]]));
            let Some(nal) = units.get(2..2 + size) else {
                self.damaged = true;
                return;
            };
            if !nal.is_empty() {
                self.nals.push(nal.to_vec());
            }
            units = &units[2 + size..];
        }
    }

    fn fragment(&mut self, start: bool, end: bool, nal_header: &[u8], data: &[u8]) {
        if start {
            if self.fragment.is_some() {
                self.damaged = true;
            }
            self.fragment = Some([nal_header, data].concat());
        } else if let Some(fragment) = &mut self.fragment {
            fragment.extend_from_slice(data);
        } else {
            // The start went missing.
            self.damaged = true;
            return;
        }
        if end && let Some(nal) = self.fragment.take() {
            self.nals.push(nal);
        }
    }

    fn h264(&mut self, p: &[u8]) {
        let Some(&first) = p.first() else {
            return;
        };
        match first & 0x1f {
            1..=23 => self.nals.push(p.to_vec()),
            H264_STAP_A => self.aggregate(&p[1..]),
            H264_FU_A if p.len() > 2 => {
                let fu = p[1];
                let nal_header = [(first & 0xe0) | (fu & 0x1f)];
                self.fragment(fu & 0x80 != 0, fu & 0x40 != 0, &nal_header, &p[2..]);
            }
            _ => self.damaged = true,
        }
    }

    fn h265(&mut self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        match (p[0] >> 1) & 0x3f {
            0..=47 => self.nals.push(p.to_vec()),
            H265_AP => self.aggregate(&p[2..]),
            H265_FU => {
                let fu = p[2];
                let nal_header = [(p[0] & 0x81) | ((fu & 0x3f) << 1), p[1]];
                self.fragment(fu & 0x80 != 0, fu & 0x40 != 0, &nal_header, &p[3..]);
            }
            _ => self.damaged = true,
        }
    }
}

/// Packs pictures into RTP: a NAL unit that fits goes alone, a bigger one
/// in fragments; the last packet of a picture has the marker.
#[derive(Debug)]
pub struct Packetizer {
    pub codec: Codec,
    pub payload_type: u8,
    pub ssrc: u32,
    pub sequence: u16,
    /// The largest payload, below a typical MTU.
    pub max_payload: usize,
}

impl Packetizer {
    #[must_use]
    pub const fn new(codec: Codec, payload_type: u8, ssrc: u32) -> Self {
        Self {
            codec,
            payload_type,
            ssrc,
            sequence: 0,
            max_payload: 1200,
        }
    }

    fn packet(&mut self, marker: bool, timestamp: u32, payload: &[u8]) -> Vec<u8> {
        let mut d = Vec::with_capacity(12 + payload.len());
        d.push(0x80);
        d.push(self.payload_type | if marker { 0x80 } else { 0 });
        d.extend_from_slice(&self.sequence.to_be_bytes());
        d.extend_from_slice(&timestamp.to_be_bytes());
        d.extend_from_slice(&self.ssrc.to_be_bytes());
        d.extend_from_slice(payload);
        self.sequence = self.sequence.wrapping_add(1);
        d
    }

    /// One picture's NAL units (without start codes) as packets.
    pub fn picture(&mut self, nals: &[&[u8]], timestamp: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let header_len = match self.codec {
            Codec::H264 => 1,
            Codec::H265 => 2,
        };
        for (i, nal) in nals.iter().enumerate() {
            let last_nal = i + 1 == nals.len();
            if nal.len() <= self.max_payload {
                out.push(self.packet(last_nal, timestamp, nal));
                continue;
            }
            let (head, body) = nal.split_at(header_len);
            let chunks: Vec<&[u8]> = body.chunks(self.max_payload - header_len - 1).collect();
            for (j, chunk) in chunks.iter().enumerate() {
                let (start, end) = (j == 0, j + 1 == chunks.len());
                let flags = (u8::from(start) << 7) | (u8::from(end) << 6);
                let payload = match self.codec {
                    Codec::H264 => {
                        let indicator = (head[0] & 0xe0) | H264_FU_A;
                        [&[indicator, flags | (head[0] & 0x1f)][..], chunk].concat()
                    }
                    Codec::H265 => {
                        let kind = (head[0] >> 1) & 0x3f;
                        let indicator = (head[0] & 0x81) | (H265_FU << 1);
                        [&[indicator, head[1], flags | kind][..], chunk].concat()
                    }
                };
                out.push(self.packet(last_nal && end, timestamp, &payload));
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test bytes"
)]
mod tests {
    use super::*;

    fn depacketize(codec: Codec, packets: &[Vec<u8>]) -> (Vec<Picture>, u64) {
        let mut d = Depacketizer::new(codec);
        let mut out = Vec::new();
        for packet in packets {
            let (header, payload) = parse(packet).unwrap();
            out.extend(d.push(&header, payload));
        }
        (out, d.lost)
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
            .collect()
    }

    #[test]
    fn pictures_survive_packing_and_unpacking_in_both_codecs() {
        for (codec, sps, idr) in [
            (Codec::H264, vec![0x67, 1, 2, 3], {
                let mut v = vec![0x65];
                v.extend((0..3000).map(|i| (i % 251) as u8));
                v
            }),
            (Codec::H265, vec![0x42, 0x01, 9, 9], {
                let mut v = vec![0x26, 0x01];
                v.extend((0..3000).map(|i| (i % 251) as u8));
                v
            }),
        ] {
            let mut p = Packetizer::new(codec, 96, 7);
            let mut packets = p.picture(&[&sps, &idr], 900);
            assert!(packets.len() > 3, "the keyframe is fragmented");
            assert!(parse(packets.last().unwrap()).unwrap().0.marker);
            packets.extend(p.picture(&[&sps], 2700));
            let (pictures, lost) = depacketize(codec, &packets);
            assert_eq!(lost, 0);
            assert_eq!(pictures.len(), 2);
            assert_eq!(pictures[0].annex_b, annex_b(&[&sps, &idr]));
            assert_eq!(pictures[0].timestamp, 900);
            assert!(!pictures[0].damaged);
        }
    }

    #[test]
    fn a_lost_fragment_damages_its_picture_only() {
        let mut p = Packetizer::new(Codec::H264, 96, 7);
        let big: Vec<u8> = std::iter::once(0x65)
            .chain((0..3000).map(|i| i as u8))
            .collect();
        let mut packets = p.picture(&[&big], 900);
        packets.remove(1);
        packets.extend(p.picture(&[&[0x41, 1, 2]], 2700));
        let (pictures, lost) = depacketize(Codec::H264, &packets);
        assert_eq!(lost, 1);
        assert!(
            pictures
                .iter()
                .all(|pic| pic.timestamp != 900 || pic.damaged)
        );
        let after = pictures.last().unwrap();
        assert_eq!(after.timestamp, 2700);
        assert!(!after.damaged);
    }

    #[test]
    fn aggregates_and_a_missing_marker_still_make_pictures() {
        // STAP-A with an SPS and a PPS, then an IDR without the marker.
        let stap = [
            vec![0x80, 96, 0, 1, 0, 0, 3, 0x84, 0, 0, 0, 7],
            vec![0x78, 0, 3, 0x67, 1, 2, 0, 2, 0x68, 3],
        ]
        .concat();
        let idr = [
            vec![0x80, 96, 0, 2, 0, 0, 3, 0x84, 0, 0, 0, 7],
            vec![0x65, 9],
        ]
        .concat();
        let next = [
            vec![0x80, 0xe0, 0, 3, 0, 0, 10, 0x8c, 0, 0, 0, 7],
            vec![0x41, 1],
        ]
        .concat();
        let (pictures, _) = depacketize(Codec::H264, &[stap, idr, next]);
        assert_eq!(pictures.len(), 2);
        assert_eq!(
            pictures[0].annex_b,
            annex_b(&[&[0x67, 1, 2], &[0x68, 3], &[0x65, 9]])
        );
    }
}
