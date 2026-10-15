//! A recording made from the fixtures is an MP4 others can read: written
//! where `UXDUCK_RECORD_OUT` says, so `ffprobe` can check it by hand.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use uxduck_media::mp4::complete_length;
use uxduck_media::record::Recorder;
use uxduck_media::source::{Output, Source};

fn record(file: &[u8]) -> (Vec<u8>, u64) {
    let mut source = Source::new();
    let mut recorder = Recorder::new();
    let mut out = Vec::new();
    for datagram in file.chunks(7 * 188) {
        for output in source.push(datagram) {
            if let Output::Unit(unit) = output {
                out.extend(recorder.push(unit));
            }
        }
    }
    out.extend(recorder.finish());
    (out, recorder.pictures)
}

#[test]
fn the_h264_pattern_records_whole() {
    let file = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let (mp4, pictures) = record(&file);
    assert_eq!(pictures, 74);
    assert_eq!(complete_length(&mp4), mp4.len());
    if let Some(dir) = std::env::var_os("UXDUCK_RECORD_OUT") {
        std::fs::write(std::path::Path::new(&dir).join("h264.mp4"), &mp4).unwrap();
    }
}

#[test]
fn the_camera_capture_records_whole_when_it_is_here() {
    let Some(path) = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Downloads/reproducible-mpeg2ts-klv.ts"))
    else {
        return;
    };
    let Ok(file) = std::fs::read(path) else {
        return;
    };
    let (mp4, pictures) = record(&file);
    assert!(pictures >= 950);
    assert_eq!(complete_length(&mp4), mp4.len());
    if let Some(dir) = std::env::var_os("UXDUCK_RECORD_OUT") {
        std::fs::write(std::path::Path::new(&dir).join("h265.mp4"), &mp4).unwrap();
    }
}
