//! Byte builders for synthetic transport streams, and bytes from the
//! sample capture (the experiment's `test/fixtures.ts`).

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    reason = "each test file uses some; bytes built from small numbers"
)]

pub fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// From the capture, the IDR at PES 85.
pub fn sample_vps() -> Vec<u8> {
    hex("40010c01ffff01400000030000030000030000030078ac09")
}
pub fn sample_sps() -> Vec<u8> {
    hex("42010101400000030000030000030000030078a00280802e1f1396b4a421192e08")
}
pub fn sample_pps() -> Vec<u8> {
    hex("4401c0f3c0cc90")
}
/// The camera's access unit delimiter: private bytes after the header.
pub fn sample_aud() -> Vec<u8> {
    hex("4601010000ff63510000455a002a")
}
/// A metadata AU cell header of 0xFF, then MISB ST 0601 (the capture's
/// first KLV PES).
pub fn sample_klv() -> Vec<u8> {
    hex(
        "ffffffffff060e2b34020b01010e0103010100000081d10208000563d8fc18962205023c8a060214370702f7f80b0a436f6c6962726920454f0c0557475338340d04000000000e04000000000f020b931002582d110231991204003900001304fff9000114040000000015040000000017040000000018040000000019028c381a0280001b0280001c0280001d0280001e0280001f02800020028000210280004f020000500200006501006601006701006801086901026a01006b01006c04a6cb48406d0454e8aa426e01006f04000000007004000000007104000000007204ceea000001025345",
    )
}

pub fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// One 188-byte packet; short payloads are padded with adaptation field
/// stuffing.
pub fn packet(pid: u16, cc: u8, unit_start: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0xffu8; 188];
    p[0] = 0x47;
    p[1] = (if unit_start { 0x40 } else { 0 }) | (pid >> 8) as u8;
    p[2] = (pid & 0xff) as u8;
    let room = 184;
    if payload.len() >= room {
        p[3] = 0x10 | (cc & 0x0f);
        p[4..].copy_from_slice(&payload[..room]);
        return p;
    }
    p[3] = 0x30 | (cc & 0x0f);
    let stuffing = room - payload.len() - 1;
    p[4] = stuffing as u8;
    if stuffing > 0 {
        p[5] = 0;
    }
    p[5 + stuffing..].copy_from_slice(payload);
    p
}

pub fn pat(pmt_pid: u16) -> Vec<u8> {
    packet(
        0,
        0,
        true,
        &hex(&format!(
            "00 00b00d 0001 c1 00 00 0001 {:04x} 00000000",
            0xe000 | pmt_pid
        )),
    )
}

/// A PMT with HEVC on 0x21 and synchronous KLV on 0x28.
pub fn pmt(pmt_pid: u16) -> Vec<u8> {
    packet(
        pmt_pid,
        0,
        true,
        &hex("00 02b017 0001 c1 00 00 e021 f000 24e021f000 15e028f000 00000000"),
    )
}

/// A PES with a PTS whose length field is off by `understate` bytes.
pub fn pes(stream_id: u8, pts: u64, es: &[u8], understate: usize) -> Vec<u8> {
    let length = 8 + es.len() - understate;
    let header = [
        0,
        0,
        1,
        stream_id,
        (length >> 8) as u8,
        (length & 0xff) as u8,
        0x84,
        0x80,
        5,
        0x21 | (((pts >> 30) as u8) << 1),
        ((pts >> 22) & 0xff) as u8,
        (((pts >> 14) & 0xfe) | 1) as u8,
        ((pts >> 7) & 0xff) as u8,
        (((pts << 1) & 0xfe) | 1) as u8,
    ];
    concat(&[&header, es])
}

/// A PES split into packets on one PID, from continuity counter `cc`.
pub fn packetize(pid: u16, data: &[u8], mut cc: u8) -> Vec<Vec<u8>> {
    data.chunks(184)
        .enumerate()
        .map(|(i, chunk)| {
            let p = packet(pid, cc, i == 0, chunk);
            cc = cc.wrapping_add(1);
            p
        })
        .collect()
}

pub fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
        .collect()
}
