//! A recording as bytes to append (S40): the init segment once the first
//! keyframe brings its parameter sets, then a fragment at every keyframe,
//! or every two seconds of a long group of pictures. Times come from the
//! pictures' own, each step held between 1 ms and 1 s, so a camera whose
//! clock jumps or starts again cannot break the file. The caller writes
//! and syncs; nothing here touches a disk.

use std::sync::Arc;

use crate::au::AccessUnit;
use crate::mp4::{ParameterSets, Sample, TIMESCALE, fragment, init_segment};
use crate::nal::Codec;

const MAX_FRAGMENT_US: u64 = 2_000_000;
const MIN_STEP_US: u64 = 1_000;
const MAX_STEP_US: u64 = 1_000_000;
/// For the last picture, whose next never came.
const DEFAULT_STEP_US: u64 = 20_000;

#[derive(Debug, Default)]
pub struct Recorder {
    codec: Option<Codec>,
    /// Pictures of the fragment in progress.
    pending: Vec<Arc<AccessUnit>>,
    sequence: u32,
    /// Where the next fragment starts, in [`TIMESCALE`] units.
    decode_time: u64,
    last_step_us: u64,
    /// What the recording holds so far, in microseconds.
    pub duration_us: u64,
    pub pictures: u64,
}

fn codec_of(unit: &AccessUnit) -> Option<Codec> {
    let codec = &unit.sps.codec;
    if codec.starts_with("avc") {
        Some(Codec::H264)
    } else if codec.starts_with("hvc") || codec.starts_with("hev") {
        Some(Codec::H265)
    } else {
        None
    }
}

fn step(from_us: u64, to_us: u64) -> u64 {
    to_us
        .saturating_sub(from_us)
        .clamp(MIN_STEP_US, MAX_STEP_US)
}

fn ticks(us: u64) -> u64 {
    us * u64::from(TIMESCALE) / 1_000_000
}

impl Recorder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sequence: 1,
            last_step_us: DEFAULT_STEP_US,
            ..Self::default()
        }
    }

    /// Whether the init segment has been written.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.codec.is_some()
    }

    /// The bytes to append for a picture: nothing until the first keyframe
    /// with its parameter sets, then the init segment, then each fragment as
    /// it ends.
    pub fn push(&mut self, unit: Arc<AccessUnit>) -> Vec<u8> {
        let Some(codec) = self.codec else {
            let Some(codec) = codec_of(&unit) else {
                return Vec::new();
            };
            let sets = ParameterSets::of(codec, &unit.data);
            if !unit.key || !sets.complete(codec) {
                return Vec::new();
            }
            let size = |v: Option<u32>| v.and_then(|v| u16::try_from(v).ok()).unwrap_or(0);
            let init = init_segment(
                codec,
                &sets,
                size(unit.sps.display_width.or(unit.sps.coded_width)),
                size(unit.sps.display_height.or(unit.sps.coded_height)),
            );
            self.codec = Some(codec);
            self.pending.push(unit);
            return init;
        };
        if codec_of(&unit) != Some(codec) {
            // Another codec needs another recording.
            return Vec::new();
        }
        let span = self.pending.first().map_or(0, |first| {
            unit.timestamp_us.saturating_sub(first.timestamp_us)
        });
        let out = if !self.pending.is_empty() && (unit.key || span >= MAX_FRAGMENT_US) {
            self.flush(Some(unit.timestamp_us))
        } else {
            Vec::new()
        };
        self.pending.push(unit);
        out
    }

    /// The fragment in progress, at the end of the recording.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.flush(None)
    }

    fn flush(&mut self, next_us: Option<u64>) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        let mut durations = Vec::with_capacity(pending.len());
        for (i, unit) in pending.iter().enumerate() {
            let next = pending.get(i + 1).map(|n| n.timestamp_us).or(next_us);
            let step_us = next.map_or(self.last_step_us, |n| step(unit.timestamp_us, n));
            self.last_step_us = step_us;
            durations.push(step_us);
        }
        let samples: Vec<Sample<'_>> = pending
            .iter()
            .zip(&durations)
            .map(|(unit, us)| Sample {
                annex_b: &unit.data,
                #[allow(clippy::cast_possible_truncation, reason = "a second at most")]
                duration: ticks(*us) as u32,
                key: unit.key,
            })
            .collect();
        let out = fragment(self.sequence, self.decode_time, &samples);
        self.sequence += 1;
        let total: u64 = durations.iter().sum();
        self.decode_time += ticks(total);
        self.duration_us += total;
        self.pictures += pending.len() as u64;
        out
    }
}

/// One KLV packet for the file beside a recording: its time since the
/// recording began, its length, and its bytes.
#[must_use]
pub fn klv_record(since_start_us: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&since_start_us.to_be_bytes());
    #[allow(clippy::cast_possible_truncation, reason = "a KLV packet is small")]
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::complete_length;
    use crate::nal::SpsInfo;

    fn unit(key: bool, timestamp_us: u64, data: Vec<u8>) -> Arc<AccessUnit> {
        Arc::new(AccessUnit {
            data,
            key,
            timestamp_us,
            sps: SpsInfo {
                id: 0,
                codec: "avc1.64001f".into(),
                coded_width: Some(1280),
                coded_height: Some(720),
                display_width: Some(1280),
                display_height: Some(720),
            },
        })
    }

    fn keyframe(timestamp_us: u64) -> Arc<AccessUnit> {
        let sps = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10,
        ];
        let data = [
            &[0, 0, 0, 1][..],
            &sps,
            &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
            &[0, 0, 0, 1, 0x65, 1, 2],
        ]
        .concat();
        unit(true, timestamp_us, data)
    }

    #[test]
    fn it_starts_at_a_keyframe_and_writes_a_fragment_per_group() {
        let mut r = Recorder::new();
        assert!(r.push(unit(false, 0, vec![0, 0, 0, 1, 0x41, 1])).is_empty());
        let mut file = r.push(keyframe(20_000));
        assert!(r.started());
        for i in 2..5 {
            file.extend(r.push(unit(false, i * 20_000, vec![0, 0, 0, 1, 0x41, 1])));
        }
        // The next keyframe ends the first fragment.
        let first_end = file.len();
        file.extend(r.push(keyframe(100_000)));
        assert!(file.len() > first_end);
        assert_eq!(r.pictures, 4);
        assert_eq!(r.duration_us, 80_000);
        file.extend(r.finish());
        assert_eq!(complete_length(&file), file.len());
        assert_eq!(r.pictures, 5);
    }

    #[test]
    fn a_clock_that_jumps_back_keeps_the_file_moving_forward() {
        let mut r = Recorder::new();
        let _ = r.push(keyframe(5_000_000));
        let _ = r.push(unit(false, 1_000, vec![0, 0, 0, 1, 0x41, 1]));
        let _ = r.push(keyframe(1_020_000));
        assert_eq!(
            r.duration_us,
            1_000 + 1_000_000,
            "steps held to 1 ms and 1 s"
        );
    }
}
