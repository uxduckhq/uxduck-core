//! What a video file added to a flight is (ADR-0039 R8, S208): how it
//! plays in a browser, when it began and how long it is, read from its own
//! bytes without decoding. An MP4, MOV or M4V gives its `mvhd` creation
//! time and duration and its picture's codec; an MPEG-TS (188-byte packets,
//! or the 192 of `.m2ts`) its codec from the PMT, its length from its first
//! and last video time stamps, and with MISB ST 0601 KLV its exact start
//! from the Precision Time Stamp against the picture's clock. Anything else
//! is said plainly, not guessed at.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use uxduck_media::ts::{StreamKind, kind_of_stream_type, read_timestamp};

/// Seconds from 1904-01-01, the MP4 epoch, to the Unix epoch.
const MP4_EPOCH_S: u64 = 2_082_844_800;

/// How a browser gets to play it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Play {
    /// As it is: MP4 or MOV with H.264, HEVC, AV1 or VP9.
    #[default]
    Direct,
    /// Its pictures and sound put into MP4, nothing decoded: H.264 or HEVC
    /// in MPEG-TS.
    Remux,
    /// As `Remux`, from 192-byte `.m2ts` packets.
    Remux192,
    /// Decoded and encoded again as H.264 by the media helper, without its
    /// sound: MPEG-2 or MPEG-1 video, Motion JPEG, DV.
    Transcode,
}

/// What a file says of itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probed {
    /// From the file's own metadata, in Unix microseconds; else none.
    pub created_us: Option<u64>,
    /// From the file's own metadata or time stamps; else none.
    pub duration_us: Option<u64>,
    /// When its first picture was taken, by its KLV time stamps.
    pub klv_start_us: Option<u64>,
    /// "H.264", "HEVC", "MPEG-2"…
    pub codec: Option<String>,
    pub play: Play,
}

fn read_box_header(file: &mut File) -> std::io::Result<Option<(u64, [u8; 4], u64)>> {
    let start = file.stream_position()?;
    let mut header = [0u8; 8];
    if file.read(&mut header)? < 8 {
        return Ok(None);
    }
    let mut size = u64::from(u32::from_be_bytes([
        header[0], header[1], header[2], header[3],
    ]));
    let kind = [header[4], header[5], header[6], header[7]];
    let mut header_len = 8;
    if size == 1 {
        let mut large = [0u8; 8];
        file.read_exact(&mut large)?;
        size = u64::from_be_bytes(large);
        header_len = 16;
    } else if size == 0 {
        size = file.metadata()?.len().saturating_sub(start);
    }
    Ok(Some((start, kind, size.max(header_len))))
}

/// The `mvhd` box's creation time and duration, from a file's top-level
/// `moov`, wherever it is in the file.
fn mvhd(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut at = 0u64;
    while at + 8 <= length {
        file.seek(SeekFrom::Start(at))?;
        let Some((start, kind, size)) = read_box_header(&mut file)? else {
            break;
        };
        if &kind == b"moov" {
            let end = start + size;
            let mut inner = file.stream_position()?;
            while inner + 8 <= end {
                file.seek(SeekFrom::Start(inner))?;
                let Some((child, kind, size)) = read_box_header(&mut file)? else {
                    break;
                };
                if &kind == b"mvhd" {
                    let mut version = [0u8; 4];
                    file.read_exact(&mut version)?;
                    return Ok(Some(if version[0] == 1 {
                        let mut v = [0u8; 28];
                        file.read_exact(&mut v)?;
                        let created = u64::from_be_bytes(v[0..8].try_into().unwrap_or_default());
                        let scale =
                            u64::from(u32::from_be_bytes(v[16..20].try_into().unwrap_or_default()));
                        let duration = u64::from_be_bytes(v[20..28].try_into().unwrap_or_default());
                        (created, duration * 1_000_000 / scale.max(1))
                    } else {
                        let mut v = [0u8; 16];
                        file.read_exact(&mut v)?;
                        let created =
                            u64::from(u32::from_be_bytes(v[0..4].try_into().unwrap_or_default()));
                        let scale =
                            u64::from(u32::from_be_bytes(v[8..12].try_into().unwrap_or_default()));
                        let duration =
                            u64::from(u32::from_be_bytes(v[12..16].try_into().unwrap_or_default()));
                        (created, duration * 1_000_000 / scale.max(1))
                    }));
                }
                inner = child + size;
            }
            return Ok(None);
        }
        at = start + size;
    }
    Ok(None)
}

/// How much of each end of an MPEG-TS is read for its time stamps, its
/// tables and its first KLV.
const TS_END: u64 = 8 << 20;
const TS_PACKET: usize = 188;
const PTS_WRAP: u64 = 1 << 33;

/// The 188-byte packets in `bytes`, from the first that starts a run of
/// sync bytes `size` apart; a 192-byte packet's 4-byte time code dropped.
fn packets(bytes: &[u8], size: usize) -> impl Iterator<Item = &[u8]> {
    let skip = size - TS_PACKET;
    let start = (0..size.min(bytes.len())).find(|&i| {
        (0..3).all(|n| bytes.get(i + skip + n * size).is_none_or(|b| *b == 0x47))
            && bytes.get(i + skip) == Some(&0x47)
    });
    bytes[start.unwrap_or(bytes.len())..]
        .chunks_exact(size)
        .map(move |p| &p[skip..])
        .filter(|p| p[0] == 0x47)
}

/// A packet's PID, whether it starts a unit, and its payload.
fn split(packet: &[u8]) -> Option<(u16, bool, &[u8])> {
    let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
    let start = packet[1] & 0x40 != 0;
    let at = match (packet[3] >> 4) & 0x03 {
        1 => 4,
        3 => 5 + usize::from(packet[4]),
        _ => return None,
    };
    Some((pid, start, packet.get(at..)?))
}

/// The PES time stamp and payload of a whole PES.
fn pes(bytes: &[u8]) -> Option<(Option<u64>, &[u8])> {
    if !bytes.starts_with(&[0, 0, 1]) {
        return None;
    }
    let flags = *bytes.get(7)?;
    let header = usize::from(*bytes.get(8)?);
    let pts = (flags & 0x80 != 0)
        .then(|| read_timestamp(bytes, 9))
        .flatten();
    Some((pts, bytes.get(9 + header..)?))
}

/// The first PES time stamp of each unit started in `bytes` for video
/// stream ids, found without the PMT, so either end of a file can be read.
fn video_pts(bytes: &[u8], size: usize) -> Vec<u64> {
    packets(bytes, size)
        .filter_map(split)
        .filter(|(_, start, payload)| {
            *start
                && payload.starts_with(&[0, 0, 1])
                && payload.get(3).is_some_and(|id| (0xE0..=0xEF).contains(id))
        })
        .filter_map(|(_, _, payload)| pes(payload).and_then(|(pts, _)| pts))
        .collect()
}

/// A program's streams from the head of an MPEG-TS: each PID's kind and
/// stream type.
fn streams(head: &[u8], size: usize) -> Vec<(u16, u8, StreamKind)> {
    let section = |payload: &[u8]| -> Option<Vec<u8>> {
        let pointer = usize::from(*payload.first()?);
        let table = payload.get(1 + pointer..)?;
        let length = usize::from(u16::from_be_bytes([*table.get(1)?, *table.get(2)?]) & 0x0FFF);
        Some(table.get(..3 + length)?.to_vec())
    };
    let mut pmt_pid = None;
    for (pid, start, payload) in packets(head, size).filter_map(split) {
        if !start {
            continue;
        }
        if pid == 0
            && pmt_pid.is_none()
            && let Some(pat) = section(payload)
        {
            // The first program's PMT: entries of 4 bytes after 8, less the CRC.
            pmt_pid = pat.get(8..pat.len().saturating_sub(4)).and_then(|entries| {
                entries
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .find(|e| e[0] != 0 || e[1] != 0)
                    .map(|e| (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]))
            });
        } else if Some(pid) == pmt_pid
            && let Some(pmt) = section(payload)
        {
            let info = usize::from(u16::from_be_bytes([pmt[10], pmt[11]]) & 0x0FFF);
            let end = pmt.len().saturating_sub(4);
            let mut at = 12 + info;
            let mut out = Vec::new();
            while at + 5 <= end {
                let stream_type = pmt[at];
                let es_pid = (u16::from(pmt[at + 1] & 0x1F) << 8) | u16::from(pmt[at + 2]);
                let length = usize::from(u16::from_be_bytes([pmt[at + 3], pmt[at + 4]]) & 0x0FFF);
                let descriptors = pmt.get(at + 5..(at + 5 + length).min(end)).unwrap_or(&[]);
                out.push((
                    es_pid,
                    stream_type,
                    kind_of_stream_type(stream_type, descriptors),
                ));
                at += 5 + length;
            }
            return out;
        }
    }
    Vec::new()
}

/// The first KLV Precision Time Stamp and the PES time stamp it came
/// with, from the KLV PIDs in `head`.
fn first_klv(head: &[u8], size: usize, klv: &[u16]) -> Option<(u64, Option<u64>)> {
    let mut units: std::collections::HashMap<u16, Vec<u8>> = std::collections::HashMap::new();
    let read = |unit: &[u8]| -> Option<(u64, Option<u64>)> {
        let (pts, payload) = pes(unit)?;
        let values = uxduck_media::klv::parse(payload)?;
        Some((values.values.timestamp_us?, pts))
    };
    for (pid, start, payload) in packets(head, size).filter_map(split) {
        if !klv.contains(&pid) {
            continue;
        }
        if start {
            if let Some(unit) = units.remove(&pid)
                && let Some(found) = read(&unit)
            {
                return Some(found);
            }
            units.insert(pid, payload.to_vec());
        } else if let Some(unit) = units.get_mut(&pid) {
            unit.extend_from_slice(payload);
        }
    }
    units.values().find_map(|unit| read(unit))
}

/// An MPEG-TS, with packets of `size` bytes.
fn probe_ts(file: &mut File, length: u64, size: usize) -> Result<Probed, String> {
    let read_at = |file: &mut File, at: u64, n: u64| -> std::io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(at))?;
        let mut bytes = Vec::new();
        file.take(n).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let head = read_at(file, 0, TS_END).map_err(|e| e.to_string())?;
    let tail = read_at(file, length.saturating_sub(TS_END), TS_END).map_err(|e| e.to_string())?;
    let streams = streams(&head, size);
    let Some(&(_, stream_type, kind)) = streams.iter().find(|(_, t, k)| {
        matches!(k, StreamKind::H264 | StreamKind::H265)
            || matches!(t, 0x01 | 0x02 | 0x10 | 0x42 | 0xD1 | 0xEA)
    }) else {
        return Err(if streams.is_empty() {
            "This MPEG-TS file has no program table UXDuck can read.".into()
        } else {
            "This MPEG-TS file has no video.".into()
        });
    };
    let (codec, play) = match (kind, stream_type) {
        (StreamKind::H264, _) => (
            "H.264",
            if size == TS_PACKET {
                Play::Remux
            } else {
                Play::Remux192
            },
        ),
        (StreamKind::H265, _) => (
            "HEVC",
            if size == TS_PACKET {
                Play::Remux
            } else {
                Play::Remux192
            },
        ),
        (_, 0x02) => ("MPEG-2", Play::Transcode),
        (_, 0x01) => ("MPEG-1", Play::Transcode),
        (_, 0x10) => {
            return Err(
                "MPEG-4 Part 2 video can't be played yet. Use H.264, HEVC or MPEG-2.".into(),
            );
        }
        _ => {
            return Err(
                "This MPEG-TS file's video can't be played yet. Use H.264, HEVC or MPEG-2.".into(),
            );
        }
    };
    let first = video_pts(&head, size).first().copied();
    let last = video_pts(&tail, size).last().copied();
    let duration_us = first.zip(last).and_then(|(first, last)| {
        // The 33-bit clock wraps every 26.5 hours.
        let ticks = last.wrapping_sub(first) & (PTS_WRAP - 1);
        (ticks > 0).then(|| ticks * 100 / 9)
    });
    let klv_pids: Vec<u16> = streams
        .iter()
        .filter(|(_, _, k)| *k == StreamKind::Klv)
        .map(|(pid, ..)| *pid)
        .collect();
    let klv_start_us = first_klv(&head, size, &klv_pids).map(|(unix_us, pts)| {
        // Where the first picture is against the KLV's own time: the KLV's
        // PES time stamp less the first picture's, as a signed distance.
        let ahead = match (pts, first) {
            (Some(pts), Some(first)) => {
                let ticks = pts.wrapping_sub(first) & (PTS_WRAP - 1);
                let signed = i64::try_from(ticks).unwrap_or(0)
                    - if ticks >= PTS_WRAP / 2 {
                        i64::try_from(PTS_WRAP).unwrap_or(0)
                    } else {
                        0
                    };
                signed * 100 / 9
            }
            // Asynchronous KLV has no time stamp of its own: it is taken as
            // at the first picture.
            _ => 0,
        };
        unix_us.saturating_add_signed(-ahead)
    });
    Ok(Probed {
        created_us: None,
        duration_us,
        klv_start_us,
        codec: Some(codec.into()),
        play,
    })
}

/// The first video track's sample entry in an MP4 or MOV: its four-letter
/// code, from `moov/trak/mdia/minf/stbl/stsd`.
fn mp4_video_codec(file: &mut File, length: u64) -> std::io::Result<Option<[u8; 4]>> {
    // The children of the box at [from, to), by kind.
    fn children(file: &mut File, from: u64, to: u64) -> std::io::Result<Vec<(u64, [u8; 4], u64)>> {
        let mut out = Vec::new();
        let mut at = from;
        while at + 8 <= to {
            file.seek(SeekFrom::Start(at))?;
            let Some((start, kind, size)) = read_box_header(file)? else {
                break;
            };
            out.push((start, kind, size));
            at = start + size;
        }
        Ok(out)
    }
    fn body(file: &mut File, start: u64) -> std::io::Result<u64> {
        file.seek(SeekFrom::Start(start))?;
        read_box_header(file)?;
        file.stream_position()
    }
    let top = children(file, 0, length)?;
    let Some(&(moov, _, moov_size)) = top.iter().find(|(_, k, _)| k == b"moov") else {
        return Ok(None);
    };
    let moov_body = body(file, moov)?;
    for (trak, kind, size) in children(file, moov_body, moov + moov_size)? {
        if &kind != b"trak" {
            continue;
        }
        let trak_body = body(file, trak)?;
        let Some(&(mdia, _, mdia_size)) = children(file, trak_body, trak + size)?
            .iter()
            .find(|(_, k, _)| k == b"mdia")
        else {
            continue;
        };
        let mdia_body = body(file, mdia)?;
        let inside = children(file, mdia_body, mdia + mdia_size)?;
        // `hdlr`: version and flags, pre-defined, then the handler type.
        let video = match inside.iter().find(|(_, k, _)| k == b"hdlr") {
            Some(&(hdlr, _, _)) => {
                let at = body(file, hdlr)? + 8;
                file.seek(SeekFrom::Start(at))?;
                let mut handler = [0u8; 4];
                file.read_exact(&mut handler)?;
                &handler == b"vide"
            }
            None => false,
        };
        if !video {
            continue;
        }
        let mut at = (mdia, mdia_size);
        for name in [b"minf", b"stbl", b"stsd"] {
            let from = body(file, at.0)?;
            let Some(&(child, _, child_size)) = children(file, from, at.0 + at.1)?
                .iter()
                .find(|(_, k, _)| k == name)
            else {
                return Ok(None);
            };
            at = (child, child_size);
        }
        // `stsd`: version and flags, the count, then the first entry's
        // size and its code.
        let at = body(file, at.0)? + 8 + 4;
        file.seek(SeekFrom::Start(at))?;
        let mut code = [0u8; 4];
        file.read_exact(&mut code)?;
        return Ok(Some(code));
    }
    Ok(None)
}

/// An MP4, MOV or M4V.
fn probe_mp4(file: &mut File, path: &Path, length: u64) -> Result<Probed, String> {
    let code = mp4_video_codec(file, length).map_err(|e| e.to_string())?;
    let (codec, play) = match code.as_ref().map(|c| &c[..]) {
        Some(b"avc1" | b"avc3") => ("H.264", Play::Direct),
        Some(b"hvc1" | b"hev1") => ("HEVC", Play::Direct),
        Some(b"av01") => ("AV1", Play::Direct),
        Some(b"vp09") => ("VP9", Play::Direct),
        Some(b"jpeg" | b"mjpa" | b"mjpb") => ("Motion JPEG", Play::Transcode),
        Some(b"dvc " | b"dvcp" | b"dvpp" | b"dv5n" | b"dv5p" | b"dvh5" | b"dvh6") => {
            ("DV", Play::Transcode)
        }
        Some(b"mx3p" | b"mx4p" | b"mx5p" | b"mx3n" | b"mx4n" | b"mx5n" | b"m2v1") => {
            ("MPEG-2", Play::Transcode)
        }
        Some(c) if c.starts_with(b"ap") => {
            return Err("ProRes video can't be played yet. Export it as H.264 or HEVC.".into());
        }
        Some(b"mp4v") => {
            return Err("MPEG-4 Part 2 video can't be played yet. Use H.264 or HEVC.".into());
        }
        Some(c) => {
            return Err(format!(
                "This video's codec ({}) can't be played yet. Use H.264 or HEVC.",
                String::from_utf8_lossy(c).trim()
            ));
        }
        None => return Err("This file has no video track UXDuck can read.".into()),
    };
    let mut probed = Probed {
        codec: Some(codec.into()),
        play,
        ..Probed::default()
    };
    if let Some((created_s, duration_us)) = mvhd(path).map_err(|e| e.to_string())? {
        // Zero, or before 2000: no clock set.
        if created_s > MP4_EPOCH_S + 946_684_800 {
            probed.created_us = Some((created_s - MP4_EPOCH_S) * 1_000_000);
        }
        probed.duration_us = (duration_us > 0).then_some(duration_us);
    }
    Ok(probed)
}

/// What a video file is and how it plays.
///
/// # Errors
/// What a person reads: the file cannot be read, or is not a video UXDuck
/// can play, naming what it is.
pub fn probe(path: &Path) -> Result<Probed, String> {
    let mut file = File::open(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let mut magic = [0u8; 200];
    let n = file.read(&mut magic).map_err(|e| e.to_string())?;
    let magic = &magic[..n];
    let at = |i: usize| magic.get(i).copied();
    if at(0) == Some(0x47) && (n < 189 || at(188) == Some(0x47)) {
        return probe_ts(&mut file, length, TS_PACKET);
    }
    if at(4) == Some(0x47) && (n < 197 || at(196) == Some(0x47)) {
        return probe_ts(&mut file, length, 192);
    }
    if let Some(kind) = magic.get(4..8)
        && matches!(
            kind,
            b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip" | b"pnot"
        )
    {
        return probe_mp4(&mut file, path, length);
    }
    let what = if magic.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "Matroska and WebM files"
    } else if magic.starts_with(b"RIFF") {
        "AVI files"
    } else if magic.starts_with(&[0, 0, 1, 0xBA]) {
        "MPEG program streams"
    } else {
        return Err("This isn't a video file UXDuck can read. Use MP4, MOV or MPEG-TS.".into());
    };
    Err(format!(
        "{what} can't be added yet. Use MP4, MOV or MPEG-TS."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_media::nal::Codec;
    use uxduck_media::tsmux::TsWriter;

    fn boxed(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(body);
        out
    }

    /// A video track whose first sample entry is `code`.
    fn video_track(code: [u8; 4]) -> Vec<u8> {
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend(boxed(code, &[0u8; 78]));
        let stbl = boxed(*b"stbl", &boxed(*b"stsd", &stsd));
        let minf = boxed(*b"minf", &stbl);
        let mut mdia = boxed(*b"hdlr", &hdlr);
        mdia.extend(minf);
        boxed(*b"trak", &boxed(*b"mdia", &mdia))
    }

    fn movie(code: [u8; 4], created_unix: u64) -> Vec<u8> {
        // 90 s at a timescale of 1000.
        let mut mvhd = vec![0u8; 4];
        mvhd.extend_from_slice(
            &u32::try_from(created_unix + MP4_EPOCH_S)
                .unwrap()
                .to_be_bytes(),
        );
        mvhd.extend_from_slice(&0u32.to_be_bytes());
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&90_000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 80]);
        let mut moov = boxed(*b"mvhd", &mvhd);
        // An audio track first, as phones often write.
        let mut sound = video_track(*b"mp4a");
        let at = sound.windows(4).position(|w| w == b"vide").unwrap();
        sound[at..at + 4].copy_from_slice(b"soun");
        moov.extend(sound);
        moov.extend(video_track(code));
        // The movie box after the media data, as cameras write it.
        let mut file = boxed(*b"ftyp", b"isom0000");
        file.extend(boxed(*b"mdat", &[0u8; 64]));
        file.extend(boxed(*b"moov", &moov));
        file
    }

    fn written(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn an_mp4s_start_length_and_codec_come_from_its_boxes() {
        let dir = tempfile::tempdir().unwrap();
        // 2026-10-28 14:02:17 UTC.
        let unix = 1_793_196_137u64;
        let probed = probe(&written(&dir, "phone.mp4", &movie(*b"avc1", unix))).unwrap();
        assert_eq!(probed.created_us, Some(unix * 1_000_000));
        assert_eq!(probed.duration_us, Some(90_000_000));
        assert_eq!(probed.codec.as_deref(), Some("H.264"));
        assert_eq!(probed.play, Play::Direct);
        let hevc = probe(&written(&dir, "iphone.mov", &movie(*b"hvc1", unix))).unwrap();
        assert_eq!(
            (hevc.codec.as_deref(), hevc.play),
            (Some("HEVC"), Play::Direct)
        );
        let mjpeg = probe(&written(&dir, "old.mov", &movie(*b"jpeg", unix))).unwrap();
        assert_eq!(mjpeg.play, Play::Transcode);
        let prores = probe(&written(&dir, "edit.mov", &movie(*b"apcn", unix)));
        assert!(prores.unwrap_err().starts_with("ProRes"));
        // A clock never set: no start.
        let unset = probe(&written(&dir, "unset.mp4", &movie(*b"avc1", 0))).unwrap();
        assert_eq!(unset.created_us, None);
    }

    #[test]
    fn other_containers_are_named_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let mkv = written(&dir, "screen.mkv", &[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0]);
        assert!(probe(&mkv).unwrap_err().starts_with("Matroska and WebM"));
        let avi = written(&dir, "old.avi", b"RIFF\0\0\0\0AVI LIST");
        assert!(probe(&avi).unwrap_err().starts_with("AVI"));
        let text = written(&dir, "notes.mp4", b"not a video at all");
        assert!(
            probe(&text)
                .unwrap_err()
                .starts_with("This isn't a video file")
        );
        assert!(
            probe(&dir.path().join("gone.mp4"))
                .unwrap_err()
                .contains("gone.mp4")
        );
    }

    /// Three seconds at 25 frames, with a big middle so the ends are read
    /// apart, its clock starting near the 33-bit wrap; KLV each second
    /// when `klv` gives the first picture's time.
    fn transport(klv: Option<u64>) -> Vec<u8> {
        let mut writer = TsWriter::new(Codec::H264, klv.is_some());
        let start = PTS_WRAP - 90_000;
        let mut ts = Vec::new();
        for frame in 0..75u64 {
            let picture = vec![0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
            let size = if frame == 40 { 9 << 20 } else { picture.len() };
            let mut body = picture;
            body.resize(size, 0x55);
            let pts = (start + frame * 3_600) & (PTS_WRAP - 1);
            ts.extend(writer.picture(&body, pts, frame % 25 == 0));
            if let Some(first_us) = klv
                && frame % 25 == 5
            {
                let at = first_us + frame * 40_000;
                let set = uxduck_media::klv::encode(&[(2, at.to_be_bytes().to_vec())]);
                ts.extend(writer.klv(&set, pts));
            }
        }
        ts
    }

    #[test]
    fn an_mpeg_ts_length_comes_from_its_video_time_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let probed = probe(&written(&dir, "screen.ts", &transport(None))).unwrap();
        assert_eq!(probed.created_us, None);
        assert_eq!(probed.klv_start_us, None);
        assert_eq!(probed.codec.as_deref(), Some("H.264"));
        assert_eq!(probed.play, Play::Remux);
        // The last frame starts 40 ms before the end.
        assert_eq!(probed.duration_us, Some(2_960_000));
        // The same in 192-byte packets, as `.m2ts`, under any name.
        let m2ts: Vec<u8> = transport(None)
            .as_chunks::<TS_PACKET>()
            .0
            .iter()
            .flat_map(|p| [0u8, 0, 0, 0].into_iter().chain(p.iter().copied()))
            .collect();
        let probed = probe(&written(&dir, "camera.mpg", &m2ts)).unwrap();
        assert_eq!(probed.play, Play::Remux192);
        assert_eq!(probed.duration_us, Some(2_960_000));
    }

    #[test]
    fn klv_time_stamps_give_the_first_pictures_moment() {
        let dir = tempfile::tempdir().unwrap();
        // 2026-10-28 14:02:17.250 UTC at the first picture.
        let first_us = 1_793_196_137_250_000;
        let probed = probe(&written(&dir, "isr.ts", &transport(Some(first_us)))).unwrap();
        assert_eq!(probed.klv_start_us, Some(first_us));
    }
}
