//! A small MPEG-TS demuxer for one program, written to survive a camera
//! whose every PES understates `PES_packet_length` (video by 8 bytes, KLV by
//! 3): a PES ends where the next one starts on its PID, not where its
//! length says (ADR-0017 V2). `trust_pes_length` restores the strict
//! behaviour, so the difference can be measured. A continuity gap marks the
//! PES it lands in as damaged rather than gluing the pieces together.
//!
//! Out of scope: PSI sections spanning several packets, more than one
//! program, and scrambled streams.

use std::collections::BTreeMap;

pub const PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    H264,
    H265,
    Klv,
    Other,
}

/// One PES as received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pes {
    pub pid: u16,
    pub kind: StreamKind,
    pub stream_id: u8,
    /// 90 kHz, 33 bits, not unwrapped.
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// The elementary stream bytes after the PES header.
    pub data: Vec<u8>,
    /// `PES_packet_length` as written; 0 means unbounded.
    pub declared_length: usize,
    /// The bytes that followed the length field before the next PES began.
    pub received_length: usize,
    /// A continuity gap or transport error hit it: its bytes are incomplete.
    pub damaged: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemuxStats {
    pub packets: u64,
    pub bytes: u64,
    pub resyncs: u64,
    pub transport_errors: u64,
    pub continuity_errors: u64,
    pub pes: u64,
    /// PES whose length field disagreed with the bytes received.
    pub pes_length_mismatches: u64,
    /// The largest (received - declared); positive means the field understates.
    pub pes_length_max_excess: i64,
}

#[derive(Debug)]
struct Stream {
    kind: StreamKind,
    bytes: Vec<u8>,
    started: bool,
    damaged: bool,
    last_cc: Option<u8>,
}

#[must_use]
pub fn kind_of_stream_type(stream_type: u8, descriptors: &[u8]) -> StreamKind {
    match stream_type {
        0x1b => StreamKind::H264,
        0x24 => StreamKind::H265,
        // 0x15 is metadata carried in PES (synchronous KLV); 0x06 private data
        // is asynchronous KLV when a registration descriptor says "KLVA".
        0x15 => StreamKind::Klv,
        0x06 if has_klva_registration(descriptors) => StreamKind::Klv,
        _ => StreamKind::Other,
    }
}

fn has_klva_registration(descriptors: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < descriptors.len() {
        let length = usize::from(descriptors[i + 1]);
        let body = descriptors.get(i + 2..(i + 2 + length).min(descriptors.len()));
        if descriptors[i] == 0x05 && body.is_some_and(|b| b.starts_with(b"KLVA")) {
            return true;
        }
        i += 2 + length;
    }
    false
}

/// A 33-bit PTS or DTS at `at`.
#[must_use]
pub fn read_timestamp(b: &[u8], at: usize) -> Option<u64> {
    let t = b.get(at..at + 5)?;
    Some(
        (u64::from((t[0] >> 1) & 0x07) << 30)
            | (u64::from(t[1]) << 22)
            | (u64::from(t[2] >> 1) << 15)
            | (u64::from(t[3]) << 7)
            | u64::from(t[4] >> 1),
    )
}

#[derive(Debug, Default)]
pub struct Demuxer {
    pub stats: DemuxStats,
    trust_pes_length: bool,
    rest: Vec<u8>,
    pmt_pid: Option<u16>,
    streams: BTreeMap<u16, Stream>,
}

impl Demuxer {
    #[must_use]
    pub fn new(trust_pes_length: bool) -> Self {
        Self {
            trust_pes_length,
            ..Self::default()
        }
    }

    /// The PIDs and kinds the PMT named so far.
    #[must_use]
    pub fn streams(&self) -> BTreeMap<u16, StreamKind> {
        self.streams.iter().map(|(pid, s)| (*pid, s.kind)).collect()
    }

    /// Bytes as they arrive, in any chunks; the PES completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Pes> {
        self.stats.bytes += chunk.len() as u64;
        let mut data = std::mem::take(&mut self.rest);
        data.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = 0;
        while i + PACKET_SIZE <= data.len() {
            let next_ok = i + PACKET_SIZE >= data.len() || data[i + PACKET_SIZE] == SYNC_BYTE;
            if data[i] != SYNC_BYTE || !next_ok {
                self.stats.resyncs += 1;
                i = find_sync(&data, i + 1);
                continue;
            }
            self.packet(&data[i..i + PACKET_SIZE], &mut out);
            i += PACKET_SIZE;
        }
        self.rest = data.split_off(i);
        out
    }

    /// The PES in progress on every PID; call at the end of a stream.
    pub fn flush(&mut self) -> Vec<Pes> {
        let pids: Vec<u16> = self.streams.keys().copied().collect();
        let mut out = Vec::new();
        for pid in pids {
            self.finish(pid, &mut out);
        }
        out
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<Pes>) {
        self.stats.packets += 1;
        let transport_error = p[1] & 0x80 != 0;
        let unit_start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
        let adaptation = (p[3] >> 4) & 0x03;
        let cc = p[3] & 0x0f;
        if transport_error {
            self.stats.transport_errors += 1;
            if let Some(stream) = self.streams.get_mut(&pid) {
                stream.damaged = true;
            }
            return;
        }
        let mut offset = 4;
        let mut discontinuity = false;
        if adaptation & 0x02 != 0 {
            let length = usize::from(p[4]);
            discontinuity = length > 0 && p[5] & 0x80 != 0;
            offset += 1 + length;
        }
        if adaptation & 0x01 == 0 || offset >= PACKET_SIZE {
            return;
        }
        let payload = &p[offset..];
        if pid == 0 && unit_start {
            self.pat(payload);
            return;
        }
        if Some(pid) == self.pmt_pid && unit_start {
            self.pmt(payload);
            return;
        }
        let Some(stream) = self.streams.get_mut(&pid) else {
            return;
        };
        if let Some(last) = stream.last_cc
            && !discontinuity
        {
            if cc == last {
                return; // a permitted duplicate packet
            }
            if cc != (last + 1) & 0x0f {
                self.stats.continuity_errors += 1;
                stream.damaged = true;
            }
        }
        stream.last_cc = Some(cc);
        if unit_start {
            self.finish(pid, out);
            let Some(stream) = self.streams.get_mut(&pid) else {
                return;
            };
            stream.damaged = false;
            stream.started = true;
            stream.bytes.extend_from_slice(payload);
        } else if stream.started {
            stream.bytes.extend_from_slice(payload);
        }
    }

    fn pat(&mut self, payload: &[u8]) {
        let Some(s) = section(payload) else {
            return;
        };
        let end = section_end(s);
        let mut i = 8;
        while i + 4 <= end {
            let program = (u16::from(s[i]) << 8) | u16::from(s[i + 1]);
            if program != 0 {
                self.pmt_pid = Some((u16::from(s[i + 2] & 0x1f) << 8) | u16::from(s[i + 3]));
                return;
            }
            i += 4;
        }
    }

    fn pmt(&mut self, payload: &[u8]) {
        let Some(s) = section(payload) else {
            return;
        };
        if s.len() < 12 {
            return;
        }
        let end = section_end(s);
        let program_info_length = (usize::from(s[10] & 0x0f) << 8) | usize::from(s[11]);
        let mut i = 12 + program_info_length;
        while i + 5 <= end {
            let stream_type = s[i];
            let pid = (u16::from(s[i + 1] & 0x1f) << 8) | u16::from(s[i + 2]);
            let info_length = (usize::from(s[i + 3] & 0x0f) << 8) | usize::from(s[i + 4]);
            let descriptors = s
                .get(i + 5..(i + 5 + info_length).min(s.len()))
                .unwrap_or(&[]);
            let kind = kind_of_stream_type(stream_type, descriptors);
            if self
                .streams
                .get(&pid)
                .is_none_or(|known| known.kind != kind)
            {
                self.streams.insert(
                    pid,
                    Stream {
                        kind,
                        bytes: Vec::new(),
                        started: false,
                        damaged: false,
                        last_cc: None,
                    },
                );
            }
            i += 5 + info_length;
        }
    }

    fn finish(&mut self, pid: u16, out: &mut Vec<Pes>) {
        let Some(stream) = self.streams.get_mut(&pid) else {
            return;
        };
        if !stream.started {
            return;
        }
        let bytes = std::mem::take(&mut stream.bytes);
        stream.started = false;
        let (kind, damaged) = (stream.kind, stream.damaged);
        if let Some(pes) = self.parse_pes(pid, kind, &bytes, damaged) {
            self.stats.pes += 1;
            out.push(pes);
        }
    }

    fn parse_pes(&mut self, pid: u16, kind: StreamKind, b: &[u8], damaged: bool) -> Option<Pes> {
        if b.len() < 9 || b[0] != 0 || b[1] != 0 || b[2] != 1 {
            return None;
        }
        let stream_id = b[3];
        let declared_length = (usize::from(b[4]) << 8) | usize::from(b[5]);
        let received_length = b.len() - 6;
        if declared_length != 0 && declared_length != received_length {
            self.stats.pes_length_mismatches += 1;
            let excess = i64::try_from(received_length).unwrap_or(i64::MAX)
                - i64::try_from(declared_length).unwrap_or(i64::MAX);
            self.stats.pes_length_max_excess = self.stats.pes_length_max_excess.max(excess);
        }
        // Fewer bytes than declared means packets went missing.
        let damaged = damaged || (declared_length != 0 && received_length < declared_length);
        let end = if self.trust_pes_length && declared_length != 0 {
            b.len().min(6 + declared_length)
        } else {
            b.len()
        };
        let flags = b[7];
        let start = 9 + usize::from(b[8]);
        if start > end {
            return None;
        }
        let pts = if flags & 0x80 != 0 {
            read_timestamp(b, 9)
        } else {
            None
        };
        let dts = if flags & 0xc0 == 0xc0 {
            read_timestamp(b, 14)
        } else {
            pts
        };
        Some(Pes {
            pid,
            kind,
            stream_id,
            pts,
            dts,
            data: b[start..end].to_vec(),
            declared_length,
            received_length,
            damaged,
        })
    }
}

/// A PSI section after its pointer field.
fn section(payload: &[u8]) -> Option<&[u8]> {
    payload.get(1 + usize::from(*payload.first()?)..)
}

/// Where a section's entries end: before its CRC.
fn section_end(s: &[u8]) -> usize {
    if s.len() < 3 {
        return 0;
    }
    let length = (usize::from(s[1] & 0x0f) << 8) | usize::from(s[2]);
    (3 + length).saturating_sub(4).min(s.len())
}

fn find_sync(data: &[u8], from: usize) -> usize {
    (from..data.len())
        .find(|&j| {
            data[j] == SYNC_BYTE
                && (j + PACKET_SIZE >= data.len() || data[j + PACKET_SIZE] == SYNC_BYTE)
        })
        .unwrap_or(data.len())
}
