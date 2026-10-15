//! KLV in RTP (RFC 6597, S91): an RTSP camera's `smpte336m` track beside
//! its video. A KLV unit is the payloads of its packets in order, the last
//! one marked; a unit missing a packet is dropped whole, as a checksum
//! could not save it. The reverse is here for tests.

use crate::rtp::Header;

/// Puts KLV units back together from one track's packets.
#[derive(Debug, Default)]
pub struct KlvDepacketizer {
    unit: Vec<u8>,
    timestamp: Option<u32>,
    next_sequence: Option<u16>,
    /// A packet of the unit being gathered went missing.
    broken: bool,
    /// Units dropped because a packet went missing.
    pub dropped: u64,
}

/// One KLV unit and its RTP timestamp, at the track's clock rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KlvUnit {
    pub timestamp: u32,
    pub data: Vec<u8>,
}

impl KlvDepacketizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A packet; the unit it ends, if it ends one whole.
    pub fn push(&mut self, header: &Header, payload: &[u8]) -> Option<KlvUnit> {
        let lost = self
            .next_sequence
            .is_some_and(|next| next != header.sequence);
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        // A new timestamp starts a new unit: the last never got its marker.
        if self.timestamp.is_some_and(|t| t != header.timestamp) {
            self.drop_unit();
        }
        if lost {
            self.broken = true;
        }
        self.timestamp = Some(header.timestamp);
        self.unit.extend_from_slice(payload);
        if !header.marker {
            return None;
        }
        let broken = self.broken;
        let unit = KlvUnit {
            timestamp: header.timestamp,
            data: std::mem::take(&mut self.unit),
        };
        self.timestamp = None;
        self.broken = false;
        if broken || unit.data.is_empty() {
            self.dropped += 1;
            return None;
        }
        Some(unit)
    }

    fn drop_unit(&mut self) {
        if !self.unit.is_empty() {
            self.dropped += 1;
        }
        self.unit.clear();
        self.timestamp = None;
        self.broken = false;
    }
}

/// A KLV unit as RTP packets of at most `mtu` payload bytes, the last
/// marked: what a camera sends, for tests.
#[must_use]
pub fn packetize(
    data: &[u8],
    payload_type: u8,
    timestamp: u32,
    ssrc: u32,
    sequence: &mut u16,
    mtu: usize,
) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = data.chunks(mtu.max(1)).collect();
    let last = chunks.len().saturating_sub(1);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut packet = vec![0x80, payload_type & 0x7f | if i == last { 0x80 } else { 0 }];
            packet.extend_from_slice(&sequence.to_be_bytes());
            packet.extend_from_slice(&timestamp.to_be_bytes());
            packet.extend_from_slice(&ssrc.to_be_bytes());
            packet.extend_from_slice(chunk);
            *sequence = sequence.wrapping_add(1);
            packet
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::parse;

    fn feed(d: &mut KlvDepacketizer, packets: &[Vec<u8>]) -> Vec<KlvUnit> {
        packets
            .iter()
            .filter_map(|p| {
                let (header, payload) = parse(p).unwrap();
                d.push(&header, payload)
            })
            .collect()
    }

    #[test]
    fn a_unit_over_several_packets_comes_back_whole_at_its_marker() {
        let data: Vec<u8> = (0..=250).collect();
        let mut sequence = 65_534;
        let packets = packetize(&data, 97, 1000, 7, &mut sequence, 100);
        assert_eq!(packets.len(), 3);
        let mut d = KlvDepacketizer::new();
        let units = feed(&mut d, &packets);
        assert_eq!(
            units,
            vec![KlvUnit {
                timestamp: 1000,
                data
            }]
        );
        // One packet each, across the sequence number's wrap.
        let more = [
            packetize(b"ab", 97, 1100, 7, &mut sequence, 100),
            packetize(b"cd", 97, 1200, 7, &mut sequence, 100),
        ]
        .concat();
        assert_eq!(feed(&mut d, &more).len(), 2);
        assert_eq!(d.dropped, 0);
    }

    #[test]
    fn a_unit_missing_a_packet_is_dropped_and_the_next_one_kept() {
        let mut sequence = 10;
        let mut first = packetize(&[1; 250], 97, 1000, 7, &mut sequence, 100);
        first.remove(1);
        let second = packetize(&[2; 50], 97, 2000, 7, &mut sequence, 100);
        let mut d = KlvDepacketizer::new();
        let units = feed(&mut d, &[first, second].concat());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, vec![2; 50]);
        assert_eq!(d.dropped, 1);
    }

    #[test]
    fn a_unit_whose_marker_never_came_is_dropped_when_the_next_starts() {
        let mut sequence = 0;
        let mut first = packetize(&[1; 150], 97, 1000, 7, &mut sequence, 100);
        first.pop();
        sequence = sequence.wrapping_sub(1);
        let second = packetize(&[2; 50], 97, 2000, 7, &mut sequence, 100);
        let mut d = KlvDepacketizer::new();
        let units = feed(&mut d, &[first, second].concat());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 2000);
        assert_eq!(d.dropped, 1);
    }
}
