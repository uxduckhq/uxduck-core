//! Writing MPEG-TS (ADR-0017 V23, S151): an RTSP camera's pictures,
//! unchanged, and its KLV, in the format a camera that sends MPEG-TS is
//! recorded in. One program: the video on [`VIDEO_PID`], which carries the
//! PCR, and asynchronous KLV on [`KLV_PID`] (stream type 0x06 with a
//! `KLVA` registration, MISB ST 1402). PAT and PMT go before every
//! keyframe, so a file cut at one plays on its own. Nothing is re-encoded.
//!
//! An RTSP camera's sound (S149) joins on [`AUDIO_PID`] with its first
//! frame, the PMT sent again with it: AAC in ADTS (stream type 0x0f), or
//! Opus as the Opus-in-MPEG-TS mapping has it (private data registered as
//! `Opus`, each packet after its control header).
//!
//! Also the keyframe index written beside a recording: where each
//! keyframe's packets begin, and when it plays.

use crate::audio::{AudioConfig, AudioUnit};
use crate::nal::Codec;
use crate::ts::PACKET_SIZE;

pub const PMT_PID: u16 = 0x1000;
pub const VIDEO_PID: u16 = 0x100;
pub const KLV_PID: u16 = 0x101;
pub const AUDIO_PID: u16 = 0x102;

/// How far ahead of a picture its PCR runs: 100 ms of decoder buffer.
const PCR_LEAD_90K: u64 = 9_000;

/// MPEG-2's CRC-32 (polynomial 0x04C11DB7, no reflection), as PSI uses.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04C1_1DB7
            };
        }
    }
    crc
}

/// A PTS as PES writes it: the 4-bit prefix, then 33 bits with markers.
#[allow(clippy::cast_possible_truncation, reason = "the bits are masked")]
fn timestamp(prefix: u8, ts: u64) -> [u8; 5] {
    let ts = ts & ((1 << 33) - 1);
    [
        (prefix << 4) | (((ts >> 30) as u8 & 0x07) << 1) | 1,
        (ts >> 22) as u8,
        (((ts >> 15) as u8) << 1) | 1,
        (ts >> 7) as u8,
        ((ts as u8) << 1) | 1,
    ]
}

/// Writes one program's packets, keeping each PID's continuity counter.
#[derive(Debug)]
pub struct TsWriter {
    codec: Codec,
    klv: bool,
    /// The camera's sound, once its first frame came (S149).
    sound: Option<AudioConfig>,
    counters: [u8; 5],
}

impl TsWriter {
    /// A program of `codec` video, with a KLV stream when `klv`.
    #[must_use]
    pub fn new(codec: Codec, klv: bool) -> Self {
        Self {
            codec,
            klv,
            sound: None,
            counters: [0; 5],
        }
    }

    fn counter(&mut self, pid: u16) -> u8 {
        let slot = match pid {
            0 => 0,
            PMT_PID => 1,
            VIDEO_PID => 2,
            AUDIO_PID => 4,
            _ => 3,
        };
        let now = self.counters[slot];
        self.counters[slot] = (now + 1) & 0x0F;
        now
    }

    /// A section in one packet, `pointer_field` 0, padded with 0xFF.
    fn section(&mut self, pid: u16, section: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(PACKET_SIZE);
        #[allow(clippy::cast_possible_truncation, reason = "a 13-bit PID")]
        packet.extend_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8]);
        packet.push(0x10 | self.counter(pid));
        packet.push(0);
        packet.extend_from_slice(section);
        let crc = crc32(section);
        packet.extend_from_slice(&crc.to_be_bytes());
        packet.resize(PACKET_SIZE, 0xFF);
        packet
    }

    /// The PAT and the PMT.
    #[must_use]
    pub fn tables(&mut self) -> Vec<u8> {
        // PAT: transport stream 1, program 1 at the PMT's PID.
        #[allow(clippy::cast_possible_truncation, reason = "a 13-bit PID")]
        let pat = [
            0x00,
            0xB0,
            13,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0x00,
            0x01,
            0xE0 | (PMT_PID >> 8) as u8,
            PMT_PID as u8,
        ];
        let mut streams = Vec::new();
        let video_type = match self.codec {
            Codec::H264 => 0x1B,
            Codec::H265 => 0x24,
        };
        #[allow(clippy::cast_possible_truncation, reason = "13-bit PIDs")]
        streams.extend_from_slice(&[
            video_type,
            0xE0 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0xF0,
            0,
        ]);
        if self.klv {
            // Asynchronous KLV: private data registered as "KLVA".
            #[allow(clippy::cast_possible_truncation, reason = "13-bit PIDs")]
            streams.extend_from_slice(&[
                0x06,
                0xE0 | (KLV_PID >> 8) as u8,
                KLV_PID as u8,
                0xF0,
                6,
                0x05,
                4,
                b'K',
                b'L',
                b'V',
                b'A',
            ]);
        }
        match self.sound {
            Some(AudioConfig::Aac(_)) => {
                #[allow(clippy::cast_possible_truncation, reason = "13-bit PIDs")]
                streams.extend_from_slice(&[
                    0x0F,
                    0xE0 | (AUDIO_PID >> 8) as u8,
                    AUDIO_PID as u8,
                    0xF0,
                    0,
                ]);
            }
            Some(AudioConfig::Opus(opus)) => {
                // Registered as "Opus", with its channel configuration in
                // an extension descriptor (tag 0x7F, extension 0x80).
                #[allow(clippy::cast_possible_truncation, reason = "13-bit PIDs")]
                streams.extend_from_slice(&[
                    0x06,
                    0xE0 | (AUDIO_PID >> 8) as u8,
                    AUDIO_PID as u8,
                    0xF0,
                    10,
                    0x05,
                    4,
                    b'O',
                    b'p',
                    b'u',
                    b's',
                    0x7F,
                    2,
                    0x80,
                    opus.channels,
                ]);
            }
            None => {}
        }
        // The PMT's version goes up when the sound joins.
        let version = u8::from(self.sound.is_some());
        // From program_number to the CRC: 9 bytes, the streams, 4 of CRC.
        let length = 9 + streams.len() + 4;
        #[allow(clippy::cast_possible_truncation, reason = "13-bit PID, short section")]
        let mut pmt = vec![
            0x02,
            0xB0 | ((length >> 8) as u8 & 0x0F),
            length as u8,
            0x00,
            0x01,
            0xC1 | (version << 1),
            0x00,
            0x00,
            0xE0 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0xF0,
            0x00,
        ];
        pmt.extend(streams);
        let mut out = self.section(0, &pat);
        out.extend(self.section(PMT_PID, &pmt));
        out
    }

    /// A PES in packets on `pid`: the first starts the unit (with the PCR
    /// and random access on a keyframe), the last is padded by its
    /// adaptation field.
    fn packets(&mut self, pid: u16, pes: &[u8], pcr: Option<u64>, key: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(pes.len() + pes.len() / 180 * 8 + PACKET_SIZE);
        let mut at = 0;
        let mut first = true;
        while at < pes.len() || first {
            // The adaptation field's bytes after its length: flags, PCR, stuffing.
            let mut adaptation: Option<Vec<u8>> = None;
            if first && (pcr.is_some() || key) {
                let mut field = vec![if key { 0x40 } else { 0 }];
                if let Some(base) = pcr {
                    field[0] |= 0x10;
                    #[allow(clippy::cast_possible_truncation, reason = "the bits are masked")]
                    field.extend_from_slice(&[
                        (base >> 25) as u8,
                        (base >> 17) as u8,
                        (base >> 9) as u8,
                        (base >> 1) as u8,
                        ((base as u8 & 1) << 7) | 0x7E,
                        0,
                    ]);
                }
                adaptation = Some(field);
            }
            let room =
                |a: &Option<Vec<u8>>| PACKET_SIZE - 4 - a.as_ref().map_or(0, |f| 1 + f.len());
            let left = pes.len() - at;
            if left < room(&adaptation) {
                // Stuffing in the adaptation field, so the payload ends the packet.
                let field = adaptation.get_or_insert_with(Vec::new);
                if !field.is_empty() || PACKET_SIZE - 4 - left > 1 {
                    if field.is_empty() {
                        field.push(0);
                    }
                    while PACKET_SIZE - 4 - 1 - field.len() > left {
                        field.push(0xFF);
                    }
                }
            }
            let take = left.min(room(&adaptation));
            #[allow(clippy::cast_possible_truncation, reason = "a 13-bit PID")]
            out.extend_from_slice(&[
                0x47,
                (if first { 0x40 } else { 0 }) | (pid >> 8) as u8,
                pid as u8,
                (if adaptation.is_some() { 0x30 } else { 0x10 }) | self.counter(pid),
            ]);
            if let Some(field) = &adaptation {
                #[allow(clippy::cast_possible_truncation, reason = "under 184")]
                out.push(field.len() as u8);
                out.extend_from_slice(field);
            }
            out.extend_from_slice(&pes[at..at + take]);
            at += take;
            first = false;
        }
        out
    }

    /// A picture as it goes in the file: the tables first on a keyframe,
    /// then its PES with the PTS (`pts` at 90 kHz) and, on a keyframe, the
    /// PCR. Annex B as access units are; nothing in it is changed.
    #[must_use]
    pub fn picture(&mut self, annex_b: &[u8], pts: u64, key: bool) -> Vec<u8> {
        let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x84, 0x80, 5];
        pes.extend_from_slice(&timestamp(0b0010, pts));
        // An access unit delimiter first, as MPEG-TS asks of H.264 and H.265.
        match self.codec {
            Codec::H264 => pes.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]),
            Codec::H265 => pes.extend_from_slice(&[0, 0, 0, 1, 0x46, 0x01, 0x50]),
        }
        pes.extend_from_slice(annex_b);
        let mut out = if key { self.tables() } else { Vec::new() };
        let pcr = key.then(|| pts.saturating_sub(PCR_LEAD_90K));
        out.extend(self.packets(VIDEO_PID, &pes, pcr, key));
        out
    }

    /// A KLV packet with its PTS (90 kHz), as asynchronous private data.
    #[must_use]
    pub fn klv(&mut self, data: &[u8], pts: u64) -> Vec<u8> {
        let length = 3 + 5 + data.len();
        #[allow(clippy::cast_possible_truncation, reason = "a KLV packet is short")]
        let mut pes = vec![
            0,
            0,
            1,
            0xBD,
            (length >> 8) as u8,
            length as u8,
            0x80,
            0x80,
            5,
        ];
        pes.extend_from_slice(&timestamp(0b0010, pts));
        pes.extend_from_slice(data);
        self.packets(KLV_PID, &pes, None, false)
    }
}

impl TsWriter {
    /// A frame of sound with its PTS (90 kHz), one frame to a PES: AAC
    /// behind its ADTS header, Opus behind its control header. The first
    /// frame brings the sound into the PMT, sent again before it; a frame
    /// of another kind than the first is left out.
    #[must_use]
    pub fn audio(&mut self, unit: &AudioUnit, pts: u64) -> Vec<u8> {
        let mut out = Vec::new();
        match self.sound {
            None => {
                self.sound = Some(unit.config);
                out = self.tables();
            }
            Some(config) if config != unit.config => return out,
            Some(_) => {}
        }
        let (stream_id, frame) = match unit.config {
            AudioConfig::Aac(aac) => (
                0xC0,
                [&aac.adts_header(unit.data.len())[..], &unit.data].concat(),
            ),
            AudioConfig::Opus(_) => {
                // The control header: its prefix, no trims, and the size.
                let mut header = vec![0x7F, 0xE0];
                let mut size = unit.data.len();
                while size >= 255 {
                    header.push(0xFF);
                    size -= 255;
                }
                #[allow(clippy::cast_possible_truncation, reason = "under 255")]
                header.push(size as u8);
                (0xBD, [&header[..], &unit.data].concat())
            }
        };
        let length = 3 + 5 + frame.len();
        #[allow(clippy::cast_possible_truncation, reason = "a frame of sound is short")]
        let mut pes = vec![
            0,
            0,
            1,
            stream_id,
            (length >> 8) as u8,
            length as u8,
            0x80,
            0x80,
            5,
        ];
        pes.extend_from_slice(&timestamp(0b0010, pts));
        pes.extend_from_slice(&frame);
        out.extend(self.packets(AUDIO_PID, &pes, None, false));
        out
    }
}

/// One entry of the keyframe index: the keyframe's time since the first
/// picture, in microseconds, and where its packets begin in the file, in
/// bytes; 16 bytes, big-endian.
#[must_use]
pub fn index_entry(since_us: u64, offset: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&since_us.to_be_bytes());
    out[8..].copy_from_slice(&offset.to_be_bytes());
    out
}

/// The keyframe index's entries; a torn last entry is left out.
#[must_use]
pub fn read_index(bytes: &[u8]) -> Vec<(u64, u64)> {
    let (entries, _torn) = bytes.as_chunks::<16>();
    entries
        .iter()
        .map(|e| {
            let (time, offset) = e.split_at(8);
            (
                u64::from_be_bytes(time.try_into().unwrap_or_default()),
                u64::from_be_bytes(offset.try_into().unwrap_or_default()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::au::Filter;
    use crate::ts::{Demuxer, StreamKind};

    const SPS: [u8; 12] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10,
    ];

    fn key() -> Vec<u8> {
        [
            &[0, 0, 0, 1][..],
            &SPS,
            &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
            &[0, 0, 0, 1, 0x65],
            &[7u8; 900],
        ]
        .concat()
    }

    #[test]
    fn the_crc_is_mpeg2s() {
        // The PAT of a common single-program stream, and its CRC.
        let pat = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        assert_eq!(crc32(&pat), 0x2AB1_04B2);
    }

    #[test]
    fn pictures_and_klv_come_back_through_the_demuxer_unchanged() {
        let mut writer = TsWriter::new(Codec::H264, true);
        let delta = [&[0, 0, 0, 1, 0x41][..], &[3u8; 400]].concat();
        let mut file = writer.picture(&key(), 90_000, true);
        file.extend(writer.klv(b"\x06\x0e\x2b\x34klv", 90_000));
        file.extend(writer.picture(&delta, 93_600, false));
        file.extend(writer.picture(&key(), 97_200, true));
        assert_eq!(file.len() % PACKET_SIZE, 0);
        assert!(file.chunks(PACKET_SIZE).all(|p| p[0] == 0x47));

        let mut demuxer = Demuxer::new(false);
        let mut all = demuxer.push(&file);
        all.extend(demuxer.flush());
        assert_eq!(demuxer.stats.continuity_errors, 0);
        let mut filter = Filter::new(Codec::H264);
        let mut units = Vec::new();
        let mut klv = Vec::new();
        for pes in all {
            match pes.kind {
                StreamKind::H264 => units.extend(filter.push(&pes.data, pes.pts, pes.damaged)),
                StreamKind::Klv => klv.push((pes.pts, pes.data)),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(units.len(), 3);
        assert!(units[0].key && !units[1].key && units[2].key);
        assert!(units[0].data.windows(4).any(|w| w == [0x65, 7, 7, 7]));
        assert_eq!(units[1].timestamp_us - units[0].timestamp_us, 40_000);
        assert_eq!(klv, [(Some(90_000), b"\x06\x0e\x2b\x34klv".to_vec())]);
    }

    #[test]
    fn the_index_reads_back_and_leaves_out_a_torn_entry() {
        let mut bytes = [index_entry(0, 0), index_entry(1_000_000, 188 * 40)].concat();
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(read_index(&bytes), [(0, 0), (1_000_000, 188 * 40)]);
    }
}
