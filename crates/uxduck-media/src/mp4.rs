//! Fragmented MP4 for recordings (ADR-0017 V16, ADR-0003 C11, S40): the
//! camera's own pictures, never re-encoded, written as an init segment and
//! then one `moof` and `mdat` per fragment, each starting on a keyframe. A
//! power cut loses at most the fragment being written: [`complete_length`]
//! finds where the last whole fragment ends.
//!
//! The parameter sets go in `avcC` or `hvcC` from the first keyframe, and
//! stay in the pictures too, so a camera that changes them mid-recording
//! still plays in Chromium.

use crate::bits::unescape_rbsp;
use crate::nal::{Codec, h264, h265, parse_header, split_annex_b};

/// 90 kHz, as MPEG-TS and RTP count.
pub const TIMESCALE: u32 = 90_000;

/// A box: its size, type and body.
fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    #[allow(
        clippy::cast_possible_truncation,
        reason = "boxes here are far below 4 GB"
    )]
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    out
}

/// A full box: a box with a version and flags.
fn full_box(kind: [u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version];
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    mp4_box(kind, &b)
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// The parameter sets of a keyframe, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    /// The parameter sets in a picture, in Annex B.
    #[must_use]
    pub fn of(codec: Codec, annex_b: &[u8]) -> Self {
        let mut sets = Self::default();
        for nal in split_annex_b(annex_b) {
            match (codec, parse_header(codec, nal).kind) {
                (Codec::H264, Some(h264::SPS)) | (Codec::H265, Some(h265::SPS)) => {
                    sets.sps.push(nal.to_vec());
                }
                (Codec::H264, Some(h264::PPS)) | (Codec::H265, Some(h265::PPS)) => {
                    sets.pps.push(nal.to_vec());
                }
                (Codec::H265, Some(h265::VPS)) => sets.vps.push(nal.to_vec()),
                _ => {}
            }
        }
        sets
    }

    /// Enough to describe the stream: an SPS and a PPS, and a VPS for H.265.
    #[must_use]
    pub fn complete(&self, codec: Codec) -> bool {
        !self.sps.is_empty()
            && !self.pps.is_empty()
            && (codec == Codec::H264 || !self.vps.is_empty())
    }
}

#[allow(clippy::cast_possible_truncation, reason = "parameter sets are short")]
fn length_prefixed_u16(nal: &[u8]) -> Vec<u8> {
    [&(nal.len() as u16).to_be_bytes()[..], nal].concat()
}

fn avcc(sets: &ParameterSets) -> Vec<u8> {
    let sps = &sets.sps[0];
    let mut b = vec![1, sps[1], sps[2], sps[3], 0xff];
    #[allow(clippy::cast_possible_truncation, reason = "at most 31 by the format")]
    b.push(0xe0 | (sets.sps.len().min(31) as u8));
    for s in sets.sps.iter().take(31) {
        b.extend(length_prefixed_u16(s));
    }
    #[allow(clippy::cast_possible_truncation, reason = "at most 255 by the format")]
    b.push(sets.pps.len().min(255) as u8);
    for p in sets.pps.iter().take(255) {
        b.extend(length_prefixed_u16(p));
    }
    mp4_box(*b"avcC", &b)
}

fn hvcc(sets: &ParameterSets) -> Vec<u8> {
    let sps = unescape_rbsp(&sets.sps[0]);
    // The SPS's profile_tier_level: profile space, tier and profile, the
    // compatibility flags, the constraint flags and the level, as hvcC has
    // them, after the two-byte header and the byte of ids.
    let mut ptl = [0u8; 12];
    if let Some(bytes) = sps.get(3..15) {
        ptl.copy_from_slice(bytes);
    }
    let mut b = vec![1];
    b.extend_from_slice(&ptl);
    // No spatial segmentation or parallelism; 4:2:0, 8 bits.
    b.extend_from_slice(&[0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0]);
    // One temporal layer, nested, four-byte lengths.
    b.push(0x0f);
    let arrays = [
        (h265::VPS, &sets.vps),
        (h265::SPS, &sets.sps),
        (h265::PPS, &sets.pps),
    ];
    b.push(3);
    for (kind, nals) in arrays {
        b.push(0x80 | kind);
        #[allow(clippy::cast_possible_truncation, reason = "a handful of sets")]
        b.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for nal in nals {
            b.extend(length_prefixed_u16(nal));
        }
    }
    mp4_box(*b"hvcC", &b)
}

fn sample_entry(codec: Codec, sets: &ParameterSets, width: u16, height: u16) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[0u8; 16]);
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    b.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[0u8; 32]);
    b.extend_from_slice(&0x0018u16.to_be_bytes());
    b.extend_from_slice(&(-1i16).to_be_bytes());
    match codec {
        Codec::H264 => {
            b.extend(avcc(sets));
            mp4_box(*b"avc1", &b)
        }
        Codec::H265 => {
            b.extend(hvcc(sets));
            mp4_box(*b"hvc1", &b)
        }
    }
}

/// `ftyp` and `moov` for one video track.
#[must_use]
#[allow(clippy::similar_names, reason = "the names are the boxes' own")]
pub fn init_segment(codec: Codec, sets: &ParameterSets, width: u16, height: u16) -> Vec<u8> {
    let ftyp = mp4_box(*b"ftyp", b"iso6\0\0\x02\0iso6mp41");
    let mut mvhd = vec![0u8; 8];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 10]);
    mvhd.extend(matrix());
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    let mvhd = full_box(*b"mvhd", 0, 0, &mvhd);

    let mut tkhd = vec![0u8; 8];
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 4]);
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend(matrix());
    tkhd.extend_from_slice(&(u32::from(width) << 16).to_be_bytes());
    tkhd.extend_from_slice(&(u32::from(height) << 16).to_be_bytes());
    let tkhd = full_box(*b"tkhd", 0, 3, &tkhd);

    let mut mdhd = vec![0u8; 8];
    mdhd.extend_from_slice(&TIMESCALE.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes());
    mdhd.extend_from_slice(&0u16.to_be_bytes());
    let mdhd = full_box(*b"mdhd", 0, 0, &mdhd);
    let hdlr = full_box(
        *b"hdlr",
        0,
        0,
        b"\0\0\0\0vide\0\0\0\0\0\0\0\0\0\0\0\0UXDuck video\0",
    );
    let vmhd = full_box(*b"vmhd", 0, 1, &[0u8; 8]);
    let dref = full_box(
        *b"dref",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &full_box(*b"url ", 0, 1, &[])].concat(),
    );
    let dinf = mp4_box(*b"dinf", &dref);
    let stsd = full_box(
        *b"stsd",
        0,
        0,
        &[
            &1u32.to_be_bytes()[..],
            &sample_entry(codec, sets, width, height),
        ]
        .concat(),
    );
    let empty = |kind: [u8; 4]| full_box(kind, 0, 0, &0u32.to_be_bytes());
    let stsz = full_box(*b"stsz", 0, 0, &[0u8; 8]);
    let stbl = mp4_box(
        *b"stbl",
        &[
            stsd,
            empty(*b"stts"),
            empty(*b"stsc"),
            stsz,
            empty(*b"stco"),
        ]
        .concat(),
    );
    let minf = mp4_box(*b"minf", &[vmhd, dinf, stbl].concat());
    let mdia = mp4_box(*b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = mp4_box(*b"trak", &[tkhd, mdia].concat());
    let mut trex = 1u32.to_be_bytes().to_vec();
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&[0u8; 12]);
    let mvex = mp4_box(*b"mvex", &full_box(*b"trex", 0, 0, &trex));
    let moov = mp4_box(*b"moov", &[mvhd, trak, mvex].concat());
    [ftyp, moov].concat()
}

/// One picture for a fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample<'a> {
    /// Annex B, as access units are.
    pub annex_b: &'a [u8],
    /// In [`TIMESCALE`] units.
    pub duration: u32,
    pub key: bool,
}

/// `moof` and `mdat` for pictures starting at `decode_time` (in
/// [`TIMESCALE`] units since the recording began).
#[must_use]
pub fn fragment(sequence: u32, decode_time: u64, samples: &[Sample<'_>]) -> Vec<u8> {
    let mut mdat_body = Vec::new();
    let mut entries = Vec::new();
    for sample in samples {
        let start = mdat_body.len();
        for nal in split_annex_b(sample.annex_b) {
            #[allow(clippy::cast_possible_truncation, reason = "a NAL unit is below 4 GB")]
            mdat_body.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            mdat_body.extend_from_slice(nal);
        }
        #[allow(clippy::cast_possible_truncation, reason = "a picture is below 4 GB")]
        let size = (mdat_body.len() - start) as u32;
        // Keyframes depend on nothing; the rest are not sync samples.
        let flags: u32 = if sample.key { 0x0200_0000 } else { 0x0101_0000 };
        entries.push((sample.duration, size, flags));
    }
    let mfhd = full_box(*b"mfhd", 0, 0, &sequence.to_be_bytes());
    // Default base is the moof.
    let tfhd = full_box(*b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes());
    let tfdt = full_box(*b"tfdt", 1, 0, &decode_time.to_be_bytes());
    let trun_for = |offset: u32| {
        #[allow(clippy::cast_possible_truncation, reason = "a fragment's pictures")]
        let mut b = (entries.len() as u32).to_be_bytes().to_vec();
        b.extend_from_slice(&offset.to_be_bytes());
        for (duration, size, flags) in &entries {
            b.extend_from_slice(&duration.to_be_bytes());
            b.extend_from_slice(&size.to_be_bytes());
            b.extend_from_slice(&flags.to_be_bytes());
        }
        // Data offset, and each sample's duration, size and flags.
        full_box(*b"trun", 0, 0x0701, &b)
    };
    let moof_len = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len() + trun_for(0).len();
    #[allow(clippy::cast_possible_truncation, reason = "a moof is small")]
    let traf = mp4_box(
        *b"traf",
        &[tfhd, tfdt, trun_for((moof_len + 8) as u32)].concat(),
    );
    let moof = mp4_box(*b"moof", &[mfhd, traf].concat());
    [moof, mp4_box(*b"mdat", &mdat_body)].concat()
}

/// How much of a recording is whole: the init segment and every fragment
/// whose `moof` and `mdat` both arrived. 0 when not even the init segment
/// is there.
#[must_use]
pub fn complete_length(file: &[u8]) -> usize {
    let mut at = 0;
    let mut whole = 0;
    let mut pending_moof = false;
    while let Some(header) = file.get(at..at + 8) {
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = &header[4..8];
        if size < 8 || at + size > file.len() {
            break;
        }
        at += size;
        match kind {
            b"moov" => whole = at,
            b"moof" => pending_moof = true,
            b"mdat" if pending_moof => {
                pending_moof = false;
                whole = at;
            }
            _ => {}
        }
    }
    whole
}

/// The boxes inside a box's body, by type and body.
fn children(mut body: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    while body.len() >= 8 {
        let size = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        if size < 8 || size > body.len() {
            break;
        }
        out.push((&body[4..8], &body[8..size]));
        body = &body[size..];
    }
    out
}

/// How long the whole fragments of a recording last, in microseconds:
/// every sample duration their `trun`s carry.
#[must_use]
pub fn duration_us(file: &[u8]) -> u64 {
    let mut ticks = 0u64;
    for (kind, moof) in children(&file[..complete_length(file)]) {
        if kind != b"moof" {
            continue;
        }
        for (kind, traf) in children(moof) {
            if kind != b"traf" {
                continue;
            }
            for (kind, trun) in children(traf) {
                if kind == b"trun" {
                    ticks += trun_ticks(trun);
                }
            }
        }
    }
    ticks * 1_000_000 / u64::from(TIMESCALE)
}

/// One whole fragment of a recording: where its `moof` and `mdat` lie in
/// the file, and when it plays, in microseconds since the recording began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub offset: u64,
    pub len: u64,
    pub start_us: u64,
    pub duration_us: u64,
}

/// A recording's layout (S83): where its init segment ends, and its whole
/// fragments in order. Segments for the account are cut along these, so
/// each starts on a keyframe and plays on its own after the init segment.
#[must_use]
pub fn layout(file: &[u8]) -> (u64, Vec<Fragment>) {
    layout_from(&mut std::io::Cursor::new(file), 0, file.len() as u64).unwrap_or_default()
}

/// The same from `from` (the start of the file or of a fragment) to `end`,
/// reading only box headers and each `moof`, so a recording of hours is
/// never in memory. A box that runs past `end` is not whole yet.
///
/// # Errors
/// The file cannot be read.
pub fn layout_from<R: std::io::Read + std::io::Seek>(
    file: &mut R,
    from: u64,
    end: u64,
) -> std::io::Result<(u64, Vec<Fragment>)> {
    use std::io::SeekFrom;
    let mut init_end = 0;
    let mut fragments = Vec::new();
    let mut at = from;
    let mut moof: Option<(u64, Vec<u8>)> = None;
    let mut header = [0u8; 8];
    while at + 8 <= end {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut header)?;
        let size = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        if size < 8 || at + size > end {
            break;
        }
        let start = at;
        at += size;
        match &header[4..8] {
            b"moov" => init_end = at,
            b"moof" => {
                let mut body = vec![0; usize::try_from(size - 8).unwrap_or(0)];
                file.read_exact(&mut body)?;
                moof = Some((start, body));
            }
            b"mdat" => {
                let Some((offset, moof)) = moof.take() else {
                    continue;
                };
                let (mut base, mut ticks) = (0u64, 0u64);
                for (kind, traf) in children(&moof) {
                    if kind != b"traf" {
                        continue;
                    }
                    for (kind, body) in children(traf) {
                        match kind {
                            b"tfdt" => base = tfdt_ticks(body),
                            b"trun" => ticks += trun_ticks(body),
                            _ => {}
                        }
                    }
                }
                let us = |t: u64| t * 1_000_000 / u64::from(TIMESCALE);
                fragments.push(Fragment {
                    offset,
                    len: at - offset,
                    start_us: us(base),
                    duration_us: us(ticks),
                });
            }
            _ => {}
        }
    }
    Ok((init_end, fragments))
}

fn tfdt_ticks(tfdt: &[u8]) -> u64 {
    match tfdt.first() {
        Some(1) => tfdt
            .get(4..12)
            .map_or(0, |b| u64::from_be_bytes(b.try_into().unwrap_or_default())),
        _ => tfdt.get(4..8).map_or(0, |b| {
            u64::from(u32::from_be_bytes(b.try_into().unwrap_or_default()))
        }),
    }
}

fn trun_ticks(trun: &[u8]) -> u64 {
    let read = |at: usize| {
        trun.get(at..at + 4)
            .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let flags = read(0) & 0x00ff_ffff;
    let count = read(4) as usize;
    let mut at = 8;
    if flags & 0x1 != 0 {
        at += 4;
    }
    if flags & 0x4 != 0 {
        at += 4;
    }
    let fields = [0x100, 0x200, 0x400, 0x800]
        .iter()
        .filter(|f| flags & **f != 0)
        .count();
    let mut ticks = 0u64;
    for _ in 0..count {
        if flags & 0x100 != 0 {
            ticks += u64::from(read(at));
        }
        at += 4 * fields;
    }
    ticks
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, reason = "test sizes")]
mod tests {
    use super::*;

    fn boxes(file: &[u8]) -> Vec<String> {
        let mut at = 0;
        let mut out = Vec::new();
        while let Some(h) = file.get(at..at + 8) {
            let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
            out.push(String::from_utf8_lossy(&h[4..8]).into_owned());
            if size < 8 {
                break;
            }
            at += size;
        }
        out
    }

    const SPS: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x19, 0x60,
    ];

    fn key() -> Vec<u8> {
        [
            &[0, 0, 0, 1][..],
            &SPS,
            &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
            &[0, 0, 0, 1, 0x65, 1, 2, 3],
        ]
        .concat()
    }

    #[test]
    fn a_recording_is_an_init_segment_then_fragments_and_a_cut_loses_only_the_last() {
        let sets = ParameterSets::of(Codec::H264, &key());
        assert!(sets.complete(Codec::H264));
        let init = init_segment(Codec::H264, &sets, 1280, 720);
        assert_eq!(boxes(&init), ["ftyp", "moov"]);
        assert!(init.windows(4).any(|w| w == b"avcC"));
        let k = key();
        let delta = [0, 0, 0, 1, 0x41, 9, 9];
        let first = fragment(
            1,
            0,
            &[
                Sample {
                    annex_b: &k,
                    duration: 1800,
                    key: true,
                },
                Sample {
                    annex_b: &delta,
                    duration: 1800,
                    key: false,
                },
            ],
        );
        assert_eq!(boxes(&first), ["moof", "mdat"]);
        // The data offset points just past the mdat's header.
        let moof_len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        let mdat_start = moof_len + 8;
        assert_eq!(
            &first[mdat_start..mdat_start + 4],
            &(SPS.len() as u32).to_be_bytes()
        );
        let second = fragment(
            2,
            3600,
            &[Sample {
                annex_b: &k,
                duration: 1800,
                key: true,
            }],
        );
        let file = [init.clone(), first.clone(), second.clone()].concat();
        assert_eq!(complete_length(&file), file.len());
        let cut = &file[..file.len() - 5];
        assert_eq!(complete_length(cut), init.len() + first.len());
        assert_eq!(complete_length(&init[..10]), 0);
        // 1800 + 1800 + 1800 ticks of 90 kHz; the cut one is gone.
        assert_eq!(duration_us(&file), 60_000);
        assert_eq!(duration_us(cut), 40_000);
        // The layout segments are cut along: each fragment, when it plays.
        let (init_end, fragments) = layout(&file);
        assert_eq!(init_end, init.len() as u64);
        assert_eq!(
            fragments,
            [
                Fragment {
                    offset: init.len() as u64,
                    len: first.len() as u64,
                    start_us: 0,
                    duration_us: 40_000
                },
                Fragment {
                    offset: (init.len() + first.len()) as u64,
                    len: second.len() as u64,
                    start_us: 40_000,
                    duration_us: 20_000
                },
            ]
        );
        assert_eq!(layout(cut).1.len(), 1);
    }

    #[test]
    fn hevc_takes_its_profile_from_the_sps() {
        let sps = [
            0x42, 0x01, 0x01, 0x01, 0x40, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
            0x00, 0x00, 0x03, 0x00, 0x78, 0xa0,
        ];
        let sets = ParameterSets {
            vps: vec![vec![0x40, 0x01, 0x0c]],
            sps: vec![sps.to_vec()],
            pps: vec![vec![0x44, 0x01, 0xc0]],
        };
        let init = init_segment(Codec::H265, &sets, 1280, 720);
        let at = init.windows(4).position(|w| w == b"hvcC").unwrap() + 4;
        // Version 1, then general profile space, tier and profile 1 (Main).
        assert_eq!(&init[at..at + 2], &[1, 0x01]);
        // Level 120, after the escaped compatibility and constraint bytes.
        assert_eq!(init[at + 12], 0x78);
    }
}
