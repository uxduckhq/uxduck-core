//! A recording as MPEG-TS (ADR-0017 V23, S151): what to append to the file
//! for each thing the hub brings, and the keyframe index beside it. The
//! caller writes and syncs; nothing here touches a disk.
//!
//! A camera that sends MPEG-TS is kept **as received**: its bytes from the
//! datagram in which the first keyframe's PES begins, with the latest PAT
//! and PMT it sent written first so the file plays from its start. A
//! camera that sends H.264 or H.265 over RTP has no MPEG-TS of its own: its
//! pictures, unchanged, and its KLV are **muxed** by [`TsWriter`]. Which it
//! is shows in the first thing that comes: MPEG-TS bytes, or a picture.

use std::sync::Arc;

use crate::au::AccessUnit;
use crate::audio::AudioUnit;
use crate::nal::Codec;
use crate::ts::PACKET_SIZE;
use crate::tsmux::{TsWriter, index_entry};

const MIN_STEP_US: u64 = 1_000;
const MAX_STEP_US: u64 = 1_000_000;
/// The most kept while waiting for the first keyframe, in bytes.
const MAX_WAITING: usize = 8 * 1024 * 1024;

/// What to append: to the recording, and to its keyframe index.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub bytes: Vec<u8>,
    pub index: Vec<u8>,
}

#[derive(Debug)]
enum Mode {
    Undecided,
    /// The camera's MPEG-TS as received.
    Received,
    /// Pictures and KLV written into MPEG-TS here.
    Muxed(TsWriter),
}

#[derive(Debug)]
pub struct TsRecorder {
    mode: Mode,
    started: bool,
    /// Before the first keyframe: the datagrams since the last picture
    /// came out, the first of them holding the next picture's start.
    waiting: Vec<Arc<[u8]>>,
    pat: Option<Vec<u8>>,
    pmt: Option<Vec<u8>>,
    pmt_pid: Option<u16>,
    /// Bytes written so far.
    offset: u64,
    /// Where the latest datagram begins in the file.
    latest_start: u64,
    /// Where the datagram that brought the last picture begins: the next
    /// picture's PES begins in it.
    last_picture_start: u64,
    last_us: Option<u64>,
    /// What the recording holds so far, in microseconds, by the pictures'
    /// steps held between 1 ms and 1 s.
    pub duration_us: u64,
    pub pictures: u64,
    /// The camera's sound is in the file.
    pub sound: bool,
}

impl Default for TsRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// A packet's PID.
fn pid(packet: &[u8]) -> u16 {
    (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2])
}

/// The first program's PMT PID in a PAT packet.
fn pmt_pid(packet: &[u8]) -> Option<u16> {
    let pointer = usize::from(*packet.get(4)?);
    let section = packet.get(5 + pointer..)?;
    let length = (usize::from(section.get(1)? & 0x0F) << 8) | usize::from(*section.get(2)?);
    // Programs from byte 8 to the CRC.
    let programs = section.get(8..(3 + length).saturating_sub(4))?;
    programs.as_chunks::<4>().0.iter().find_map(|p| {
        let number = u16::from_be_bytes([p[0], p[1]]);
        (number != 0).then(|| (u16::from(p[2] & 0x1F) << 8) | u16::from(p[3]))
    })
}

impl TsRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: Mode::Undecided,
            started: false,
            waiting: Vec::new(),
            pat: None,
            pmt: None,
            pmt_pid: None,
            offset: 0,
            latest_start: 0,
            last_picture_start: 0,
            last_us: None,
            duration_us: 0,
            pictures: 0,
            sound: false,
        }
    }

    /// Whether the first keyframe has come and the file begun.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// The bytes written, so a recording can say its size.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.offset
    }

    fn append(&mut self, out: &mut Written, bytes: &[u8]) {
        out.bytes.extend_from_slice(bytes);
        self.offset += bytes.len() as u64;
    }

    /// A camera's MPEG-TS bytes as received, ahead of what they bring.
    pub fn push_ts(&mut self, chunk: Arc<[u8]>) -> Written {
        let mut out = Written::default();
        if matches!(self.mode, Mode::Undecided) {
            self.mode = Mode::Received;
        }
        if !matches!(self.mode, Mode::Received) {
            return out;
        }
        for packet in chunk.as_chunks::<PACKET_SIZE>().0 {
            let starts = packet[1] & 0x40 != 0;
            if packet[0] != 0x47 || !starts {
                continue;
            }
            match pid(packet) {
                0 => {
                    self.pmt_pid = pmt_pid(packet).or(self.pmt_pid);
                    self.pat = Some(packet.to_vec());
                }
                p if Some(p) == self.pmt_pid => self.pmt = Some(packet.to_vec()),
                _ => {}
            }
        }
        if self.started {
            self.latest_start = self.offset;
            self.append(&mut out, &chunk);
        } else {
            self.waiting.push(chunk);
            let mut held: usize = self.waiting.iter().map(|c| c.len()).sum();
            while held > MAX_WAITING && self.waiting.len() > 1 {
                held -= self.waiting.remove(0).len();
            }
        }
        out
    }

    /// A picture: the start of the file at the first keyframe, the time it
    /// adds, and the index's entry for a keyframe; for a muxed recording,
    /// the picture itself.
    pub fn push_unit(&mut self, unit: &AccessUnit) -> Written {
        let mut out = Written::default();
        if matches!(self.mode, Mode::Undecided) {
            let codec = if unit.sps.codec.starts_with("hvc") || unit.sps.codec.starts_with("hev") {
                Codec::H265
            } else {
                Codec::H264
            };
            self.mode = Mode::Muxed(TsWriter::new(codec, true));
        }
        let pts = unit.timestamp_us * 9 / 100;
        if !self.started {
            if !unit.key {
                // The datagram that brought this picture holds the next one's start.
                let keep = self.waiting.len().saturating_sub(1);
                self.waiting.drain(..keep);
                return out;
            }
            self.started = true;
            self.pictures = 1;
            self.last_us = Some(unit.timestamp_us);
            match &mut self.mode {
                Mode::Received => {
                    let tables: Vec<u8> = self
                        .pat
                        .take()
                        .into_iter()
                        .chain(self.pmt.take())
                        .flatten()
                        .collect();
                    self.append(&mut out, &tables);
                    out.index.extend(index_entry(0, self.offset));
                    for chunk in std::mem::take(&mut self.waiting) {
                        self.latest_start = self.offset;
                        self.append(&mut out, &chunk);
                    }
                    self.last_picture_start = self.latest_start;
                }
                Mode::Muxed(writer) => {
                    let bytes = writer.picture(&unit.data, pts, true);
                    out.index.extend(index_entry(0, self.offset));
                    self.append(&mut out, &bytes);
                }
                Mode::Undecided => {}
            }
            return out;
        }
        if let Some(last) = self.last_us {
            self.duration_us += unit
                .timestamp_us
                .saturating_sub(last)
                .clamp(MIN_STEP_US, MAX_STEP_US);
        }
        self.last_us = Some(unit.timestamp_us);
        self.pictures += 1;
        match &mut self.mode {
            Mode::Received => {
                if unit.key {
                    out.index
                        .extend(index_entry(self.duration_us, self.last_picture_start));
                }
                self.last_picture_start = self.latest_start;
            }
            Mode::Muxed(writer) => {
                let bytes = writer.picture(&unit.data, pts, unit.key);
                if unit.key {
                    out.index.extend(index_entry(self.duration_us, self.offset));
                }
                self.append(&mut out, &bytes);
            }
            Mode::Undecided => {}
        }
        out
    }

    /// A KLV packet: already in a received recording; written into a
    /// muxed one once it has begun.
    pub fn push_klv(&mut self, timestamp_us: u64, data: &[u8]) -> Written {
        let mut out = Written::default();
        if self.started
            && let Mode::Muxed(writer) = &mut self.mode
        {
            let bytes = writer.klv(data, timestamp_us * 9 / 100);
            self.append(&mut out, &bytes);
        }
        out
    }

    /// A frame of sound: in a received recording already, so it says the
    /// file has sound; written into a muxed one once it has begun, an RTSP
    /// camera's sound track (S149).
    pub fn push_audio(&mut self, unit: &AudioUnit) -> Written {
        let mut out = Written::default();
        if !self.started {
            return out;
        }
        let bytes = match &mut self.mode {
            Mode::Received => {
                self.sound = true;
                return out;
            }
            Mode::Muxed(writer) => writer.audio(unit, unit.timestamp_us * 9 / 100),
            Mode::Undecided => return out,
        };
        self.sound |= !bytes.is_empty();
        self.append(&mut out, &bytes);
        out
    }
}

/// A recording cut short, as far as its whole packets go.
#[must_use]
pub const fn whole_length(len: u64) -> u64 {
    len - len % PACKET_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::SpsInfo;
    use crate::source::{Output, Source};
    use crate::tsmux::read_index;

    fn picture(key: bool, timestamp_us: u64) -> AccessUnit {
        let data = if key {
            [
                &[0, 0, 0, 1][..],
                &[
                    0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10,
                ],
                &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
                &[0, 0, 0, 1, 0x65, 9, 9],
            ]
            .concat()
        } else {
            vec![0, 0, 0, 1, 0x41, 9]
        };
        AccessUnit {
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
        }
    }

    #[test]
    fn an_rtp_camera_is_muxed_from_its_first_keyframe_with_its_klv() {
        let mut recorder = TsRecorder::new();
        assert!(recorder.push_unit(&picture(false, 0)).bytes.is_empty());
        assert!(recorder.push_klv(10_000, b"before").bytes.is_empty());
        let mut file = recorder.push_unit(&picture(true, 40_000)).bytes;
        file.extend(recorder.push_klv(40_000, b"klv").bytes);
        file.extend(recorder.push_unit(&picture(false, 80_000)).bytes);
        let w = recorder.push_unit(&picture(true, 120_000));
        assert_eq!(read_index(&w.index), [(80_000, file.len() as u64)]);
        file.extend(w.bytes);
        // A PES ends where the next on its PID begins: a second brings the first out.
        file.extend(recorder.push_klv(120_000, b"next").bytes);
        assert_eq!(file.len() % PACKET_SIZE, 0);
        // Read back: three pictures and the KLV, keyframe first.
        let mut source = Source::new();
        let (mut keys, mut klv) = (Vec::new(), Vec::new());
        for chunk in file.chunks(PACKET_SIZE * 7) {
            for output in source.push(chunk) {
                match output {
                    Output::Unit(u) => keys.push(u.key),
                    Output::Klv { data, .. } => klv.push(data),
                    Output::Audio(_) => {}
                }
            }
        }
        assert_eq!(klv, [b"klv".to_vec()]);
        assert_eq!(keys.first(), Some(&true));
    }

    /// An RTSP camera's sound (S149) muxed with its pictures, read back
    /// through the demuxer, and made into the playback MP4: the frames, their
    /// times and the kind of sound come back as they went in.
    fn sound_round_trip(config: crate::audio::AudioConfig, frames: &[(Vec<u8>, u32)]) {
        let rate = u64::from(config.sample_rate());
        let mut recorder = TsRecorder::new();
        let mut file = Vec::new();
        let mut sent = Vec::new();
        let mut at_samples = 0u64;
        // Pictures every 40 ms from 1 s, a keyframe a second; sound from 1.1 s.
        for i in 0..75u64 {
            let picture_us = 1_000_000 + i * 40_000;
            file.extend(recorder.push_unit(&picture(i % 25 == 0, picture_us)).bytes);
            while 1_100_000 + at_samples * 1_000_000 / rate < picture_us + 40_000 {
                let (data, samples) = &frames[sent.len() % frames.len()];
                let unit = AudioUnit {
                    data: data.clone(),
                    timestamp_us: 1_100_000 + at_samples * 1_000_000 / rate,
                    config,
                    samples: *samples,
                };
                file.extend(recorder.push_audio(&unit).bytes);
                sent.push(unit);
                at_samples += u64::from(*samples);
            }
        }
        assert!(recorder.sound);
        assert_eq!(file.len() % PACKET_SIZE, 0);
        let mut source = Source::new();
        let mut mp4 = crate::record::Recorder::with_sound();
        let mut playback = Vec::new();
        let mut back = Vec::new();
        for chunk in file.chunks(PACKET_SIZE * 7) {
            for output in source.push(chunk) {
                match output {
                    Output::Unit(u) => playback.extend(mp4.push(u)),
                    Output::Audio(a) => {
                        back.push(a.clone());
                        playback.extend(mp4.push_audio(a));
                    }
                    Output::Klv { .. } => {}
                }
            }
        }
        playback.extend(mp4.finish());
        // All but the last, whose PES ends only when another begins.
        assert_eq!(back.len(), sent.len() - 1);
        for (got, want) in back.iter().zip(&sent) {
            assert_eq!(
                (&got.data, got.config, got.samples),
                (&want.data, want.config, want.samples)
            );
            assert!(
                got.timestamp_us.abs_diff(want.timestamp_us) <= 12,
                "{} {}",
                got.timestamp_us,
                want.timestamp_us
            );
        }
        assert_eq!(
            source.status().audio.map(|a| a.name),
            Some(config.describe())
        );
        assert!(mp4.has_sound());
        // The sound starts 100 ms after the first picture, at its own rate.
        let tfdt = playback
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"tfdt")
            .nth(1)
            .unwrap()
            .0;
        let at = u64::from_be_bytes(playback[tfdt + 8..tfdt + 16].try_into().unwrap());
        assert!(at.abs_diff(rate / 10) <= 1, "{at}");
    }

    #[test]
    fn an_rtsp_cameras_aac_is_muxed_as_adts_and_comes_back_for_playback() {
        let config = crate::aac::AacConfig::from_audio_specific_config(&[0x11, 0x90]).unwrap();
        let frames: Vec<(Vec<u8>, u32)> = (0..9u8)
            .map(|i| (vec![0x21, i, 0x40, 7, 7, 7], 1024))
            .collect();
        sound_round_trip(crate::audio::AudioConfig::Aac(config), &frames);
    }

    #[test]
    fn an_rtsp_cameras_opus_is_muxed_with_its_control_headers_and_comes_back_for_playback() {
        let config = crate::audio::AudioConfig::Opus(crate::opus::OpusConfig { channels: 2 });
        // 20 ms packets, one of 300 bytes whose size needs two bytes, and a 10 ms one.
        let frames = vec![
            (vec![0xFC, 1, 2, 3], 960),
            ([&[0xFC][..], &[5; 299]].concat(), 960),
            (vec![0xF4, 9], 480),
        ];
        sound_round_trip(config, &frames);
    }

    #[test]
    fn a_cut_recording_keeps_its_whole_packets() {
        assert_eq!(whole_length(188 * 3 + 50), 188 * 3);
        assert_eq!(whole_length(0), 0);
    }
}
