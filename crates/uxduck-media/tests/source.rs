#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

mod fixtures;

use fixtures::{annex_b, hex, packetize, pat, pes, pmt, sample_pps, sample_sps, sample_vps};
use uxduck_media::source::{Output, Source};
use uxduck_media::udp::Container;

fn idr() -> Vec<u8> {
    hex("2601af26f9e2ef2c707d22af80710254")
}

/// RTP version 2, payload type 33.
fn rtp(sequence: u16, ts: &[u8]) -> Vec<u8> {
    let [hi, lo] = sequence.to_be_bytes();
    [&[0x80, 33, hi, lo, 0, 0, 0, 0, 0, 0, 0, 1][..], ts].concat()
}

#[test]
fn rtp_datagrams_become_pictures_with_their_codec_and_counters() {
    let key = annex_b(&[&sample_vps(), &sample_sps(), &sample_pps(), &idr()]);
    let mut packets = vec![pat(0x20), pmt(0x20)];
    packets.extend(packetize(0x21, &pes(0xe0, 1000, &key, 8), 0));
    packets.extend(packetize(0x21, &pes(0xe0, 2800, &key, 8), 1));
    let mut source = Source::new();
    let mut units = Vec::new();
    // Seven packets to a datagram, as cameras send them.
    for (i, chunk) in packets.chunks(7).enumerate() {
        units.extend(source.push(&rtp(u16::try_from(i).unwrap(), &chunk.concat())));
    }
    // The first picture ends when the second starts.
    assert_eq!(units.len(), 1);
    let Output::Unit(unit) = &units[0] else {
        panic!("not a picture")
    };
    assert!(unit.key);
    let status = source.status();
    assert_eq!(status.container, Some(Container::RtpTs));
    assert_eq!(status.codec.as_deref(), Some("hvc1.1.2.L120"));
    assert_eq!((status.width, status.height), (Some(1280), Some(720)));
    assert_eq!(status.unsupported, None);
    assert_eq!(status.counters.pictures, 1);
    assert_eq!(status.counters.pes_length_mismatches, 1);
    assert_eq!(status.counters.pes_length_max_excess, 8);

    // A silence and a fresh start keep the totals.
    source.resume();
    assert_eq!(source.counters().pictures, 1);
}

#[test]
fn a_stream_without_h264_or_h265_says_so() {
    // A PMT naming only MPEG-2 video (stream type 0x02) on 0x21.
    let pmt = fixtures::packet(
        0x20,
        0,
        true,
        &hex("00 02b012 0001 c1 00 00 e021 f000 02e021f000 00000000"),
    );
    let mut source = Source::new();
    source.push(&[pat(0x20), pmt].concat());
    assert!(source.status().unsupported.unwrap().contains("H.264"));
    let mut other = Source::new();
    other.push(b"hello");
    assert!(other.status().unsupported.is_some());
}

/// A 3 s test pattern made with ffmpeg (`testsrc2`, 320×180, 25 fps, H.264
/// baseline, a keyframe a second): nothing personal, so it is committed.
#[test]
fn the_h264_fixture_plays_through_a_source() {
    let file = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/testsrc-h264.mpegts"
    ))
    .expect("the fixture");
    let mut source = Source::new();
    let mut pictures = 0;
    for datagram in file.chunks(7 * 188) {
        pictures += source
            .push(datagram)
            .iter()
            .filter(|o| matches!(o, Output::Unit(_)))
            .count();
    }
    let status = source.status();
    assert!(status.codec.as_deref().unwrap().starts_with("avc1.42"));
    assert_eq!((status.width, status.height), (Some(320), Some(180)));
    // 75 pictures; the last waits for one that never comes.
    assert_eq!(pictures, 74);
    assert_eq!(status.counters.keyframes, 3);
    assert_eq!(status.counters.lost, 0);
}

/// The same pattern as RTP carrying H.264 straight, with payload type 96
/// and no SDP: the source learns the codec from the stream.
#[test]
fn rtp_h264_with_a_dynamic_payload_type_is_recognised() {
    let file = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/testsrc-h264.mpegts"
    ))
    .expect("the fixture");
    let datagrams = uxduck_media::replay::rtp_video_datagrams(&file, 7);
    let mut source = Source::new();
    let mut pictures = 0;
    for datagram in &datagrams {
        pictures += source
            .push(&datagram.bytes)
            .iter()
            .filter(|o| matches!(o, Output::Unit(_)))
            .count();
    }
    let status = source.status();
    assert_eq!(status.container, Some(Container::RtpH264));
    assert!(status.codec.as_deref().unwrap().starts_with("avc1.42"));
    assert_eq!((status.width, status.height), (Some(320), Some(180)));
    // Every picture, the last too: the marker ends each one.
    assert_eq!(pictures, 75);
    assert_eq!(status.counters.lost, 0);
}
