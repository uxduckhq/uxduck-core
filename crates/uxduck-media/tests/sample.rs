//! The whole capture through demuxer, filter and KLV. Skipped when the file
//! is not on this machine (`UXDUCK_TS_SAMPLE`, or the maintainer's
//! Downloads folder).

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use std::path::PathBuf;

use uxduck_media::au::Filter;
use uxduck_media::nal::Codec;
use uxduck_media::ts::{Demuxer, StreamKind};
use uxduck_media::{au, klv};

fn sample() -> Option<Vec<u8>> {
    let path = std::env::var_os("UXDUCK_TS_SAMPLE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join("Downloads/reproducible-mpeg2ts-klv.ts"))
        })?;
    std::fs::read(path).ok()
}

struct Run {
    demuxer: Demuxer,
    filter: au::Filter,
    klv_ok: u32,
    klv_bad: u32,
    units: u32,
    first_key: Option<u32>,
    video_pes: u32,
}

fn run(data: &[u8], trust_pes_length: bool) -> Run {
    let mut demuxer = Demuxer::new(trust_pes_length);
    let mut all = demuxer.push(data);
    all.extend(demuxer.flush());
    let mut r = Run {
        demuxer,
        filter: Filter::new(Codec::H265),
        klv_ok: 0,
        klv_bad: 0,
        units: 0,
        first_key: None,
        video_pes: 0,
    };
    for pes in all {
        match pes.kind {
            StreamKind::H265 => {
                if let Some(unit) = r.filter.push(&pes.data, pes.pts, pes.damaged) {
                    if unit.key && r.first_key.is_none() {
                        r.first_key = Some(r.video_pes);
                    }
                    r.units += 1;
                }
                r.video_pes += 1;
            }
            StreamKind::Klv => {
                if klv::parse(&pes.data).is_some_and(|k| k.checksum_ok) {
                    r.klv_ok += 1;
                } else {
                    r.klv_bad += 1;
                }
            }
            _ => {}
        }
    }
    r
}

#[test]
fn the_capture_understates_every_length_and_tolerant_demuxing_recovers_it_all() {
    let Some(data) = sample() else {
        eprintln!("no sample capture; skipped");
        return;
    };
    let r = run(&data, false);
    // PAT and PMT come only with each IDR, so the 85 pictures before the
    // first have no stream map.
    assert_eq!(r.video_pes, 960);
    assert_eq!(r.demuxer.stats.pes_length_mismatches, 1920);
    assert_eq!(r.demuxer.stats.pes_length_max_excess, 8);
    assert_eq!(r.demuxer.stats.continuity_errors, 0);
    assert_eq!(r.first_key, Some(0));
    assert_eq!(r.units, 960);
    assert_eq!(r.filter.stats.keyframes, 10);
    assert_eq!(r.filter.stats.nals_dropped.get("AUD"), Some(&960));
    let fake = r
        .filter
        .stats
        .nals_dropped
        .get("forbidden bit")
        .unwrap_or(&0)
        + r.filter.stats.nals_dropped.get("layer id").unwrap_or(&0);
    assert!(fake >= 4);
    assert_eq!((r.klv_ok, r.klv_bad), (960, 0));
}

#[test]
fn the_capture_through_a_source_in_camera_sized_datagrams() {
    let Some(data) = sample() else {
        return;
    };
    let mut source = uxduck_media::source::Source::new();
    let mut pictures = 0;
    let mut klv = 0;
    for datagram in data.chunks(7 * 188) {
        for out in source.push(datagram) {
            match out {
                uxduck_media::source::Output::Unit(_) => pictures += 1,
                uxduck_media::source::Output::Klv { .. } => klv += 1,
            }
        }
    }
    let status = source.status();
    assert_eq!(status.codec.as_deref(), Some("hvc1.1.2.L120"));
    assert!(status.klv);
    // The last picture waits for one that never comes.
    assert_eq!(pictures, 959);
    assert!(klv >= 959);
    assert_eq!(status.counters.lost, 0);
}

#[test]
fn the_capture_as_rtp_h265_is_recognised() {
    let Some(data) = sample() else {
        return;
    };
    let mut source = uxduck_media::source::Source::new();
    let mut pictures = 0;
    for datagram in uxduck_media::replay::rtp_video_datagrams(&data, 7) {
        pictures += source
            .push(&datagram.bytes)
            .iter()
            .filter(|o| matches!(o, uxduck_media::source::Output::Unit(_)))
            .count();
    }
    let status = source.status();
    assert_eq!(
        status.container,
        Some(uxduck_media::udp::Container::RtpH265)
    );
    assert_eq!(status.codec.as_deref(), Some("hvc1.1.2.L120"));
    assert!(pictures >= 950, "{pictures}");
}

#[test]
fn the_capture_demuxed_strictly_fails_every_klv_checksum() {
    let Some(data) = sample() else {
        return;
    };
    let r = run(&data, true);
    assert_eq!((r.klv_ok, r.klv_bad), (0, 960));
}
