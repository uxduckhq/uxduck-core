//! A recording as bytes to append (S40): the init segment once the first
//! keyframe brings its parameter sets, then a fragment at every keyframe,
//! or every two seconds of a long group of pictures. Times come from the
//! pictures' own, each step held between 1 ms and 1 s, so a camera whose
//! clock jumps or starts again cannot break the file. The caller writes
//! and syncs; nothing here touches a disk.
//!
//! A camera's sound, AAC or Opus, is a second track (S147, S149). The tracks are fixed by
//! the init segment, so a recording of a stream that says it has sound
//! ([`Recorder::with_sound`]) waits up to [`SOUND_WAIT_US`] of pictures
//! after its first keyframe for the first sound; sound that starts later is
//! in the next recording. Each fragment takes the frames that play before the
//! next one starts, placed by their own time since the first picture.

use std::sync::Arc;

use crate::au::AccessUnit;
use crate::audio::{AudioConfig, AudioUnit};
use crate::mp4::{
    ParameterSets, Sample, SoundRun, TIMESCALE, fragment_with_sound, init_segment_with_sound,
};
use crate::nal::Codec;

/// How long after its first keyframe a recording waits for the camera's
/// first sound before it goes on without (S147).
pub const SOUND_WAIT_US: u64 = 1_500_000;
const MAX_FRAGMENT_US: u64 = 2_000_000;
const MIN_STEP_US: u64 = 1_000;
const MAX_STEP_US: u64 = 1_000_000;
/// For the last picture, whose next never came.
const DEFAULT_STEP_US: u64 = 20_000;

/// The first keyframe's description, while the recording waits for sound.
#[derive(Debug)]
struct Start {
    codec: Codec,
    sets: ParameterSets,
    width: u16,
    height: u16,
}

#[derive(Debug, Default)]
pub struct Recorder {
    codec: Option<Codec>,
    /// The stream says it has sound: wait for it before the init segment.
    expect_sound: bool,
    /// Waiting for the first sound before the init segment.
    starting: Option<Start>,
    /// How long it has waited, in pictures' steps.
    waited_us: u64,
    /// When the first picture plays, on the stream's clock.
    start_us: u64,
    /// The camera's sound, when the recording has it.
    sound: Option<AudioConfig>,
    /// Frames of sound not yet in a fragment.
    pending_sound: Vec<Arc<AudioUnit>>,
    /// Where the sound's next frame goes, at its sampling rate.
    sound_time: u64,
    /// Pictures of the fragment in progress.
    pending: Vec<Arc<AccessUnit>>,
    sequence: u32,
    /// Where the next fragment starts, in [`TIMESCALE`] units.
    decode_time: u64,
    last_step_us: u64,
    /// What the recording holds so far, in microseconds.
    pub duration_us: u64,
    pub pictures: u64,
    /// Frames of sound written (S147).
    pub sound_frames: u64,
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

    /// A recording of a stream that says it carries sound it records: it waits for
    /// the first sound before writing anything (S147).
    #[must_use]
    pub fn with_sound() -> Self {
        Self {
            expect_sound: true,
            ..Self::new()
        }
    }

    /// Whether the first keyframe has come: the recording has begun, its
    /// init segment written or waiting for sound.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.codec.is_some() || self.starting.is_some()
    }

    /// Whether the recording has the camera's sound (S147).
    #[must_use]
    pub const fn has_sound(&self) -> bool {
        self.codec.is_some() && self.sound.is_some()
    }

    /// The init segment for what started, with the sound if it came.
    fn begin(&mut self) -> Vec<u8> {
        let Some(start) = self.starting.take() else {
            return Vec::new();
        };
        self.codec = Some(start.codec);
        init_segment_with_sound(
            start.codec,
            &start.sets,
            start.width,
            start.height,
            self.sound.as_ref(),
        )
    }

    /// The bytes to append for a picture: nothing until the first keyframe
    /// with its parameter sets, then (once the first sound came, or did
    /// not within [`SOUND_WAIT_US`]) the init segment, then each fragment
    /// as it ends.
    pub fn push(&mut self, unit: Arc<AccessUnit>) -> Vec<u8> {
        if let Some(start) = &self.starting {
            if codec_of(&unit) != Some(start.codec) {
                return Vec::new();
            }
            if let Some(last) = self.pending.last() {
                self.waited_us += step(last.timestamp_us, unit.timestamp_us);
            }
            self.pending.push(unit);
            return if self.waited_us >= SOUND_WAIT_US {
                self.begin()
            } else {
                Vec::new()
            };
        }
        let Some(codec) = self.codec else {
            let Some(codec) = codec_of(&unit) else {
                return Vec::new();
            };
            let sets = ParameterSets::of(codec, &unit.data);
            if !unit.key || !sets.complete(codec) {
                return Vec::new();
            }
            let size = |v: Option<u32>| v.and_then(|v| u16::try_from(v).ok()).unwrap_or(0);
            self.starting = Some(Start {
                codec,
                sets,
                width: size(unit.sps.display_width.or(unit.sps.coded_width)),
                height: size(unit.sps.display_height.or(unit.sps.coded_height)),
            });
            self.start_us = unit.timestamp_us;
            self.pending.push(unit);
            return if self.expect_sound {
                Vec::new()
            } else {
                self.begin()
            };
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

    /// The bytes to append for a frame of sound: the init segment, when
    /// the recording was waiting for its first sound; otherwise nothing, the
    /// frame kept for its fragment. Sound before the first picture, of
    /// another kind, or for a recording that began without sound, is left
    /// out (S147).
    pub fn push_audio(&mut self, unit: Arc<AudioUnit>) -> Vec<u8> {
        if unit.timestamp_us < self.start_us || !self.started() {
            return Vec::new();
        }
        if self.starting.is_some() && self.sound.is_none() {
            self.sound = Some(unit.config);
            self.pending_sound.push(unit);
            return self.begin();
        }
        if self.sound == Some(unit.config) {
            self.pending_sound.push(unit);
        }
        Vec::new()
    }

    /// The fragment in progress, at the end of the recording.
    pub fn finish(&mut self) -> Vec<u8> {
        let init = self.begin();
        if self.pending.is_empty() {
            return init;
        }
        [init, self.flush(None)].concat()
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
        // The sound that plays before the next fragment starts.
        let take = next_us.map_or(self.pending_sound.len(), |next| {
            self.pending_sound
                .partition_point(|u| u.timestamp_us < next)
        });
        let sound: Vec<Arc<AudioUnit>> = self.pending_sound.drain(..take).collect();
        let frames: Vec<&[u8]> = sound.iter().map(|u| u.data.as_slice()).collect();
        let sound_durations: Vec<u32> = sound.iter().map(|u| u.samples).collect();
        let run = self.sound.zip(sound.first()).map(|(config, first)| {
            // Placed by its own time, never over the frames before it.
            let rate = u64::from(config.sample_rate());
            let at = (first.timestamp_us - self.start_us) * rate / 1_000_000;
            let decode_time = at.max(self.sound_time);
            self.sound_time =
                decode_time + sound_durations.iter().map(|d| u64::from(*d)).sum::<u64>();
            decode_time
        });
        self.sound_frames += frames.len() as u64;
        let out = fragment_with_sound(
            self.sequence,
            self.decode_time,
            &samples,
            run.map(|decode_time| SoundRun {
                decode_time,
                frames: &frames,
                durations: &sound_durations,
            }),
        );
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

#[cfg(test)]
mod sound_tests {
    use super::*;
    use crate::mp4::{complete_length, duration_us};
    use crate::nal::SpsInfo;

    fn unit(key: bool, timestamp_us: u64) -> Arc<AccessUnit> {
        let sps = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10,
        ];
        let data = if key {
            [
                &[0, 0, 0, 1][..],
                &sps,
                &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
                &[0, 0, 0, 1, 0x65, 1, 2],
            ]
            .concat()
        } else {
            vec![0, 0, 0, 1, 0x41, 1]
        };
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

    const STEREO_48K: crate::aac::AacConfig = crate::aac::AacConfig {
        object_type: 2,
        frequency_index: 3,
        sample_rate: 48_000,
        channels: 2,
    };

    fn frame(timestamp_us: u64) -> Arc<AudioUnit> {
        Arc::new(AudioUnit::aac(
            vec![0x21, 0x10, 0x04],
            timestamp_us,
            STEREO_48K,
        ))
    }

    fn count(file: &[u8], kind: [u8; 4]) -> usize {
        file.windows(4).filter(|w| *w == kind).count()
    }

    #[test]
    fn a_stream_with_sound_waits_for_it_then_keeps_it_in_each_fragment() {
        let mut r = Recorder::with_sound();
        // Sound before the first picture is left out.
        assert!(r.push_audio(frame(900_000)).is_empty());
        assert!(
            r.push(unit(true, 1_000_000)).is_empty(),
            "waits for the sound"
        );
        assert!(r.push(unit(false, 1_040_000)).is_empty());
        let mut file = r.push_audio(frame(1_010_000));
        assert!(count(&file, *b"mp4a") == 1 && count(&file, *b"esds") == 1);
        assert!(r.has_sound());
        for i in 1..20 {
            file.extend(r.push_audio(frame(1_010_000 + i * 21_333)));
        }
        file.extend(r.push(unit(false, 1_080_000)));
        // The next keyframe ends the first fragment, with its sound.
        file.extend(r.push(unit(true, 1_120_000)));
        file.extend(r.finish());
        assert_eq!(
            count(&file, *b"traf"),
            4,
            "video and sound in both fragments"
        );
        assert_eq!(complete_length(&file), file.len());
        // The length is the pictures'; the sound's ticks are at 48 kHz.
        assert_eq!(duration_us(&file), r.duration_us);
        assert_eq!(r.sound_frames, 20);
        // A cut keeps the whole fragments, pictures and sound together.
        let cut = &file[..file.len() - 3];
        assert!(complete_length(cut) < file.len());
        assert_eq!(count(&cut[..complete_length(cut)], *b"traf"), 2);
    }

    #[test]
    fn opus_is_kept_with_its_dops_and_each_packets_own_length() {
        let opus = crate::audio::AudioConfig::Opus(crate::opus::OpusConfig { channels: 2 });
        let packet = |timestamp_us: u64, samples: u32| {
            Arc::new(AudioUnit {
                data: vec![0xFC, 1, 2],
                timestamp_us,
                config: opus,
                samples,
            })
        };
        let mut r = Recorder::with_sound();
        let mut file = r.push(unit(true, 0));
        file.extend(r.push_audio(packet(10_000, 960)));
        assert!(count(&file, *b"Opus") == 1 && count(&file, *b"dOps") == 1);
        assert_eq!(count(&file, *b"mp4a"), 0);
        // A 40 ms packet among 20 ms ones: the next goes 2880 samples on.
        file.extend(r.push_audio(packet(30_000, 1920)));
        file.extend(r.push_audio(packet(70_000, 960)));
        file.extend(r.push(unit(false, 40_000)));
        file.extend(r.finish());
        assert_eq!(complete_length(&file), file.len());
        assert_eq!(r.sound_frames, 3);
        // The sound's trun: three packets, their lengths as they came.
        let trun = file.windows(4).rposition(|w| w == b"trun").unwrap();
        let entries = &file[trun + 16..trun + 16 + 24];
        let lengths: Vec<u32> = entries
            .chunks(8)
            .map(|e| u32::from_be_bytes([e[0], e[1], e[2], e[3]]))
            .collect();
        assert_eq!(lengths, [960, 1920, 960]);
        // It starts 10 ms in, at 48 kHz.
        let tfdt = file.windows(4).rposition(|w| w == b"tfdt").unwrap();
        let at = u64::from_be_bytes(file[tfdt + 8..tfdt + 16].try_into().unwrap());
        assert_eq!(at, 480);
    }

    #[test]
    fn without_sound_in_time_it_goes_on_with_the_pictures_alone() {
        let mut r = Recorder::with_sound();
        let mut file = r.push(unit(true, 0));
        for i in 1..=40 {
            file.extend(r.push(unit(i % 25 == 0, i * 40_000)));
        }
        assert!(count(&file, *b"moov") == 1 && count(&file, *b"mp4a") == 0);
        // Sound that starts late joins the next recording, not this one.
        assert!(r.push_audio(frame(1_700_000)).is_empty());
        file.extend(r.finish());
        assert!(!r.has_sound());
        assert_eq!(count(&file, *b"traf"), count(&file, *b"moof"));
    }

    #[test]
    fn a_clock_that_jumps_back_while_waiting_still_starts() {
        let mut r = Recorder::with_sound();
        let mut file = r.push(unit(true, 5_000_000));
        for i in 0..2 {
            file.extend(r.push(unit(false, 1_000 + i)));
        }
        // Each step counts at least 1 ms; two pictures a second apart pass the wait.
        file.extend(r.push(unit(false, 1_001_000)));
        file.extend(r.push(unit(false, 2_001_000)));
        assert_eq!(count(&file, *b"moov"), 1);
    }
}
