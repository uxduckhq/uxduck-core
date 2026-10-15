#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

mod fixtures;

use fixtures::sample_klv;
use uxduck_media::klv::parse;

#[test]
fn finds_the_local_set_behind_a_0xff_au_cell_header_and_checks_the_checksum() {
    let klv = parse(&sample_klv()).unwrap();
    assert!(klv.checksum_ok);
    assert_eq!(klv.values.image_source.as_deref(), Some("Colibri EO"));
    // 2018 or later, in microseconds.
    assert!(klv.values.timestamp_us.unwrap() > 1_500_000_000_000_000);
}

#[test]
fn the_three_bytes_a_strict_demuxer_drops_take_the_checksum_with_them() {
    let data = sample_klv();
    let klv = parse(&data[..data.len() - 3]).unwrap();
    assert!(!klv.checksum_ok);
}

#[test]
fn bytes_without_a_local_set_are_not_klv() {
    assert!(parse(&[0xff; 80]).is_none());
}

use uxduck_media::klv::{encode, to_signed};

#[test]
fn a_frame_centre_and_its_corner_offsets_make_a_footprint() {
    let packet = encode(&[
        (10, b"Copter 1".to_vec()),
        (13, to_signed(-35.363_261, 90.0, 4)),
        (14, to_signed(149.165_23, 180.0, 4)),
        (23, to_signed(-35.36, 90.0, 4)),
        (24, to_signed(149.17, 180.0, 4)),
        (26, to_signed(0.001, 0.075, 2)),
        (27, to_signed(-0.002, 0.075, 2)),
        (28, to_signed(0.001, 0.075, 2)),
        (29, to_signed(0.002, 0.075, 2)),
        (30, to_signed(-0.001, 0.075, 2)),
        (31, to_signed(0.002, 0.075, 2)),
        (32, to_signed(-0.001, 0.075, 2)),
        (33, to_signed(-0.002, 0.075, 2)),
    ]);
    let klv = parse(&packet).unwrap();
    assert!(klv.checksum_ok);
    let v = klv.values;
    assert_eq!(v.platform_designation.as_deref(), Some("Copter 1"));
    assert!((v.sensor_latitude_deg.unwrap() + 35.363_261).abs() < 1e-6);
    let corners = v.corners.expect("corners");
    assert!((corners[0].0 - (-35.359)).abs() < 1e-5, "{corners:?}");
    assert!((corners[0].1 - 149.168).abs() < 1e-5, "{corners:?}");
    assert!((corners[3].1 - 149.168).abs() < 1e-5, "{corners:?}");
}

#[test]
fn whole_corners_win_and_an_error_value_is_no_value() {
    let mut tags = vec![
        (23, to_signed(10.0, 90.0, 4)),
        (24, to_signed(20.0, 180.0, 4)),
        // MISB's error value for the sensor's latitude.
        (13, vec![0x80, 0, 0, 0]),
    ];
    for (i, (lat, lon)) in [(10.1, 20.1), (10.1, 19.9), (9.9, 19.9), (9.9, 20.1)]
        .into_iter()
        .enumerate()
    {
        let tag = u8::try_from(82 + 2 * i).unwrap();
        tags.push((tag, to_signed(lat, 90.0, 4)));
        tags.push((tag + 1, to_signed(lon, 180.0, 4)));
    }
    let v = parse(&encode(&tags)).unwrap().values;
    assert_eq!(v.sensor_latitude_deg, None);
    let corners = v.corners.unwrap();
    assert!((corners[1].1 - 19.9).abs() < 1e-6);
    // Not every corner: no footprint.
    let partial = parse(&encode(&tags[..5])).unwrap().values;
    assert_eq!(partial.corners, None);
}
