//! A capture as a camera sent it (S38's demo, the experiment's `send`):
//! seven packets to a datagram, optionally in RTP, each due when its
//! packets' PCR says. Pure: the caller sleeps and sends.

use crate::au::Filter;
use crate::nal::{Codec, split_annex_b};
use crate::rtp::Packetizer;
use crate::ts::{Demuxer, PACKET_SIZE, StreamKind};

/// Packets per datagram, as cameras send them (1316 bytes).
const PER_DATAGRAM: usize = 7;

/// The 90 kHz PCR base of a packet, if it carries one.
#[must_use]
pub fn read_pcr(p: &[u8]) -> Option<u64> {
    if p.len() < 11 || p[3] & 0x20 == 0 || p[4] == 0 || p[5] & 0x10 == 0 {
        return None;
    }
    Some(
        (u64::from(p[6]) << 25)
            | (u64::from(p[7]) << 17)
            | (u64::from(p[8]) << 9)
            | (u64::from(p[9]) << 1)
            | (u64::from(p[10]) >> 7),
    )
}

/// Where a loop starts again: the first PAT, since pictures before it refer
/// to ones a camera sent earlier.
#[must_use]
pub fn first_pat(file: &[u8]) -> usize {
    (0..file.len() / PACKET_SIZE)
        .map(|i| i * PACKET_SIZE)
        .find(|&at| ((u16::from(file[at + 1]) & 0x1f) << 8 | u16::from(file[at + 2])) == 0)
        .unwrap_or(0)
}

/// One datagram and when to send it, in microseconds from the start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Datagram {
    pub at_us: u64,
    pub bytes: Vec<u8>,
}

/// The capture from `start` as datagrams. A PCR that goes back or jumps by
/// more than a second restarts the clock, as at a discontinuity.
#[must_use]
pub fn datagrams(file: &[u8], start: usize, rtp: Option<(u32, u16)>) -> Vec<Datagram> {
    let usable = &file[start.min(file.len())..file.len() - file.len() % PACKET_SIZE];
    let mut out = Vec::new();
    let mut base: Option<(u64, u64)> = None;
    let mut last_pcr: Option<u64> = None;
    let mut now_us = 0u64;
    let (ssrc, mut sequence) = rtp.unwrap_or((0, 0));
    let mut clock = 0u32;
    for chunk in usable.chunks(PER_DATAGRAM * PACKET_SIZE) {
        if let Some(pcr) = chunk.chunks(PACKET_SIZE).find_map(read_pcr) {
            let jumped = last_pcr.is_none_or(|last| pcr < last || pcr - last > 90_000);
            if jumped {
                base = Some((pcr, now_us));
            }
            if let Some((pcr0, at0)) = base {
                now_us = at0 + (pcr - pcr0) * 100 / 9;
            }
            last_pcr = Some(pcr);
            #[allow(clippy::cast_possible_truncation, reason = "RTP time wraps at 32 bits")]
            {
                clock = pcr as u32;
            }
        }
        let bytes = if rtp.is_some() {
            // RTP payload type 33 (MP2T), 90 kHz clock (RFC 2250).
            let mut d = vec![0x80, 33];
            d.extend_from_slice(&sequence.to_be_bytes());
            d.extend_from_slice(&clock.to_be_bytes());
            d.extend_from_slice(&ssrc.to_be_bytes());
            d.extend_from_slice(chunk);
            sequence = sequence.wrapping_add(1);
            d
        } else {
            chunk.to_vec()
        };
        out.push(Datagram {
            at_us: now_us,
            bytes,
        });
    }
    out
}

/// The capture's pictures as RTP carrying H.264 or H.265 straight, each
/// picture due at its own time, as QGroundControl's default stream is.
/// Payload type 96, the usual dynamic one.
#[must_use]
pub fn rtp_video_datagrams(file: &[u8], ssrc: u32) -> Vec<Datagram> {
    let mut demuxer = Demuxer::new(false);
    let mut all = demuxer.push(file);
    all.extend(demuxer.flush());
    let mut out = Vec::new();
    let mut packing: Option<(Filter, Packetizer)> = None;
    let mut first_us = None;
    for pes in all {
        let codec = match pes.kind {
            StreamKind::H264 => Codec::H264,
            StreamKind::H265 => Codec::H265,
            _ => continue,
        };
        let (filter, packetizer) =
            packing.get_or_insert_with(|| (Filter::new(codec), Packetizer::new(codec, 96, ssrc)));
        let Some(unit) = filter.push(&pes.data, pes.pts, pes.damaged) else {
            continue;
        };
        let at_us = unit.timestamp_us - *first_us.get_or_insert(unit.timestamp_us);
        #[allow(clippy::cast_possible_truncation, reason = "RTP time wraps at 32 bits")]
        let timestamp = (unit.timestamp_us * 9 / 100) as u32;
        let nals = split_annex_b(&unit.data);
        for bytes in packetizer.picture(&nals, timestamp) {
            out.push(Datagram { at_us, bytes });
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, reason = "test bytes")]
mod tests {
    use super::*;

    fn packet(pid: u16, pcr: Option<u64>) -> Vec<u8> {
        let mut p = vec![0xffu8; PACKET_SIZE];
        p[0] = 0x47;
        p[1] = (pid >> 8) as u8;
        p[2] = (pid & 0xff) as u8;
        p[3] = 0x10;
        if let Some(pcr) = pcr {
            p[3] = 0x30;
            p[4] = 7;
            p[5] = 0x10;
            p[6] = (pcr >> 25) as u8;
            p[7] = (pcr >> 17) as u8;
            p[8] = (pcr >> 9) as u8;
            p[9] = (pcr >> 1) as u8;
            p[10] = ((pcr & 1) << 7) as u8;
        }
        p
    }

    #[test]
    fn datagrams_are_paced_by_pcr_and_wrapped_in_rtp() {
        let mut file = Vec::new();
        file.extend(packet(0x21, None));
        file.extend(packet(0, None));
        for i in 0..14u64 {
            file.extend(packet(0x21, (i % 7 == 0).then_some(90_000 + i / 7 * 1_800)));
        }
        assert_eq!(read_pcr(&packet(0x21, Some(123_456))), Some(123_456));
        let start = first_pat(&file);
        assert_eq!(start, PACKET_SIZE);
        let plain = datagrams(&file, start, None);
        assert_eq!(plain.len(), 3);
        assert_eq!(plain[0].bytes.len(), 7 * PACKET_SIZE);
        let rtp = datagrams(&file, 0, Some((7, 65_535)));
        assert_eq!(rtp[0].bytes[..4], [0x80, 33, 0xff, 0xff]);
        assert_eq!(rtp[1].bytes[2..4], [0, 0], "the sequence wraps");
        assert_eq!(rtp[1].at_us - rtp[0].at_us, 20_000, "1800 ticks of 90 kHz");
        assert_eq!(rtp[2].at_us, rtp[1].at_us, "no PCR: sent straight after");
    }
}
