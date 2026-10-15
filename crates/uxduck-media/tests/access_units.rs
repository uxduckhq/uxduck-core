#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

mod fixtures;

use fixtures::{annex_b, concat, hex, sample_aud, sample_pps, sample_sps, sample_vps};
use uxduck_media::au::{Filter, OnDamage};
use uxduck_media::nal::{Codec, parse_sps, split_annex_b};

fn idr() -> Vec<u8> {
    hex("2601af26f9e2ef2c707d22af80710254")
}
fn trail() -> Vec<u8> {
    hex("0201d0d724c0aabbccdd")
}
fn kinds(data: &[u8]) -> Vec<u8> {
    split_annex_b(data)
        .iter()
        .map(|n| (n[0] >> 1) & 0x3f)
        .collect()
}
fn key_with_sets() -> Vec<u8> {
    annex_b(&[&sample_vps(), &sample_sps(), &sample_pps(), &idr()])
}

#[test]
fn splits_three_and_four_byte_start_codes_and_trims_trailing_zeros() {
    let data = hex("00000001 4001 aa 000001 4201 bb 0000 00000001 4401 cc");
    let nals = split_annex_b(&data);
    assert_eq!(
        nals.iter().map(|n| n[0]).collect::<Vec<_>>(),
        [0x40, 0x42, 0x44]
    );
    assert_eq!(nals[1], &hex("4201bb")[..]);
}

#[test]
fn the_sps_gives_the_codec_string_ffmpeg_writes_into_hvcc() {
    let sps = parse_sps(Codec::H265, &sample_sps()).unwrap();
    assert_eq!(sps.codec, "hvc1.1.2.L120");
    assert_eq!(sps.coded_width, Some(1280));
    assert_eq!(sps.coded_height, Some(736));
    assert_eq!(sps.display_height, Some(720));
}

#[test]
fn an_h264_sps_gives_avc1_ppccll() {
    let sps = parse_sps(
        Codec::H264,
        &hex("6764001facd9405005bb011000000300100000030320f1831960"),
    )
    .unwrap();
    assert_eq!(sps.codec, "avc1.64001f");
}

#[test]
fn drops_the_cameras_delimiter_and_the_nal_unit_it_fakes() {
    let mut filter = Filter::new(Codec::H265);
    let key_es = annex_b(&[
        &sample_aud(),
        &sample_vps(),
        &sample_sps(),
        &sample_pps(),
        &idr(),
    ]);
    let key = filter.push(&key_es, Some(900), false).unwrap();
    assert!(key.key);
    assert_eq!(kinds(&key.data), [32, 33, 34, 19]);

    // The delimiter as it arrives at PES 511: its private bytes contain
    // 00 00 01 and fake a NAL unit of type 11, layer 5.
    let aud_with_fake_start = concat(&[&sample_aud()[..6], &hex("000001 16295d")]);
    let delta_es = concat(&[
        &hex("00000001"),
        &aud_with_fake_start,
        &hex("00000001"),
        &trail(),
    ]);
    let delta = filter.push(&delta_es, Some(2700), false).unwrap();
    assert!(!delta.key);
    assert_eq!(split_annex_b(&delta.data), [&trail()[..]]);
    assert_eq!(filter.stats.nals_dropped.get("AUD"), Some(&2));
    assert_eq!(filter.stats.nals_dropped.get("layer id"), Some(&1));
    assert_eq!(delta.timestamp_us, 30_000);
}

#[test]
fn waits_for_a_keyframe_at_the_start_and_after_damage_and_brings_parameter_sets_to_keyframes() {
    let mut filter = Filter::new(Codec::H265);
    assert!(filter.push(&annex_b(&[&trail()]), Some(0), false).is_none());
    assert_eq!(filter.stats.skipped_waiting_for_key, 1);

    assert!(filter.push(&key_with_sets(), Some(1), false).unwrap().key);
    assert!(filter.push(&annex_b(&[&trail()]), Some(2), true).is_none());
    assert!(filter.push(&annex_b(&[&trail()]), Some(3), false).is_none());

    // A keyframe without parameter sets gets the remembered ones in front.
    let key = filter.push(&annex_b(&[&idr()]), Some(4), false).unwrap();
    assert_eq!(kinds(&key.data), [32, 33, 34, 19]);
}

#[test]
fn passing_through_hands_the_decoder_the_pes_untouched() {
    let mut filter = Filter::new(Codec::H265).passing_through();
    let es = annex_b(&[
        &sample_aud(),
        &sample_vps(),
        &sample_sps(),
        &sample_pps(),
        &idr(),
    ]);
    assert_eq!(filter.push(&es, Some(0), false).unwrap().data, es);
}

#[test]
fn on_damage_submit_hands_a_damaged_picture_to_the_decoder_instead_of_freezing() {
    let mut filter = Filter::new(Codec::H265).on_damage(OnDamage::Submit);
    filter.push(&key_with_sets(), Some(0), false);
    assert!(filter.push(&annex_b(&[&trail()]), Some(1), true).is_some());
    assert!(!filter.waiting_for_key());
    assert_eq!(filter.stats.damaged, 1);
}

#[test]
fn the_timestamp_keeps_increasing_across_the_33_bit_wrap() {
    let mut filter = Filter::new(Codec::H265);
    let before = filter
        .push(&key_with_sets(), Some((1 << 33) - 900), false)
        .unwrap();
    let after = filter
        .push(&annex_b(&[&trail()]), Some(900), false)
        .unwrap();
    assert_eq!(after.timestamp_us - before.timestamp_us, 20_000);
}
