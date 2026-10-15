#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "a test"
)]

mod fixtures;

use fixtures::{concat, hex, packetize, pat, pes, pmt};
use uxduck_media::ts::{Demuxer, Pes, StreamKind};

fn es_a() -> Vec<u8> {
    hex(&format!(
        "000000014601500000000126 01af{}",
        "ab".repeat(400)
    ))
}
fn es_b() -> Vec<u8> {
    hex(&format!("00000001460150000000010201d0{}", "cd".repeat(300)))
}

fn stream(understate: usize) -> Vec<u8> {
    let mut parts = vec![pat(0x20), pmt(0x20)];
    parts.extend(packetize(0x21, &pes(0xe0, 1000, &es_a(), understate), 0));
    parts.extend(packetize(0x21, &pes(0xe0, 2800, &es_b(), understate), 3));
    parts.concat()
}

fn demux(data: &[u8], trust_pes_length: bool) -> (Vec<Pes>, Demuxer) {
    let mut demuxer = Demuxer::new(trust_pes_length);
    let mut out = demuxer.push(data);
    out.extend(demuxer.flush());
    (out, demuxer)
}

#[test]
fn a_pes_ends_at_the_next_unit_start_not_at_an_understated_length() {
    let (out, demuxer) = demux(&stream(8), false);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].data, es_a());
    assert_eq!(out[0].pts, Some(1000));
    assert_eq!(out[0].kind, StreamKind::H265);
    assert!(!out[0].damaged);
    assert_eq!(demuxer.stats.pes_length_mismatches, 2);
    assert_eq!(demuxer.stats.pes_length_max_excess, 8);
}

#[test]
fn a_strict_demuxer_loses_the_last_bytes_of_every_picture() {
    let (out, _) = demux(&stream(8), true);
    assert_eq!(out[0].data.len(), es_a().len() - 8);
}

#[test]
fn correct_lengths_demux_the_same_either_way() {
    assert_eq!(demux(&stream(0), true).0[1].data, es_b());
    assert_eq!(demux(&stream(0), false).1.stats.pes_length_mismatches, 0);
}

#[test]
fn a_continuity_gap_marks_the_pes_damaged() {
    let mut packets = packetize(0x21, &pes(0xe0, 1000, &es_a(), 0), 0);
    packets.remove(1);
    let mut parts = vec![pat(0x20), pmt(0x20)];
    parts.extend(packets);
    parts.extend(packetize(0x21, &pes(0xe0, 2800, &es_b(), 0), 3));
    let (out, demuxer) = demux(&parts.concat(), false);
    assert_eq!(demuxer.stats.continuity_errors, 1);
    assert!(out[0].damaged);
    assert!(!out[1].damaged);
}

#[test]
fn resynchronises_after_stray_bytes_and_across_chunk_boundaries() {
    let data = concat(&[&hex("0102"), &stream(0)]);
    let mut demuxer = Demuxer::new(false);
    let mut out = Vec::new();
    for chunk in data.chunks(100) {
        out.extend(demuxer.push(chunk));
    }
    out.extend(demuxer.flush());
    assert_eq!(demuxer.stats.resyncs, 1);
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].data, es_b());
}
