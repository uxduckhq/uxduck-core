//! The simulated camera's own settings (ADR-0063 Y5, S411): the camera
//! definition file it names in `CAMERA_INFORMATION` and serves over
//! MAVLink FTP, and the settings that file names, read and written as
//! extended parameters (`PARAM_EXT_*`).
//!
//! The settings do what a camera's do, as far as the simulation shows it:
//! a photo's format and a video's size and rate change how fast the card
//! fills, automatic exposure picks the ISO and the shutter speed itself,
//! the video's size takes a moment to change, and it cannot change while
//! the camera records.

use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::dialects::ardupilotmega::enums::ParamAck;
use mavio::dialects::ardupilotmega::messages::{ParamExtAck, ParamExtValue};

use super::Camera;
use crate::camera_params::Value;
use crate::fake_files::FakeFiles;
use crate::payload::wire;

/// The definition file the camera keeps, where it is on the camera, and
/// the address and version it gives for it.
pub const DEFINITION: &str = include_str!("definition.xml");
pub const DEFINITION_PATH: &str = "/camera.xml";
pub const DEFINITION_URI: &str = "mftp:///camera.xml";
pub const DEFINITION_VERSION: u16 = 1;

/// The settings by name, in the order of its list.
pub const NAMES: [&str; 9] = [
    "CAM_MODE",
    "CAM_EXPMODE",
    "CAM_EV",
    "CAM_ISO",
    "CAM_SHUTTERSPD",
    "CAM_VIDRES",
    "CAM_VIDFPS",
    "CAM_PHOTOFMT",
    "CAM_SHUTTERCNT",
];

const ISOS: [u32; 6] = [100, 200, 400, 800, 1600, 3200];
const SHUTTERS_S: [f32; 7] = [0.0005, 0.001, 0.002, 0.004, 0.008, 0.016_666, 0.033_333];
const RATES: [u32; 3] = [24, 30, 60];
/// What automatic exposure picks.
const AUTO_ISO: u32 = 400;
const AUTO_SHUTTER_S: f32 = 0.002;
/// 3840 x 2160, the largest video, which it cannot record 60 times a
/// second.
const LARGEST: u32 = 0;
/// How long the video's size takes to change, in milliseconds.
const RESIZE_MS: u64 = 1000;
/// How many settings of a list it sends at once, and again at each step,
/// as a camera paces a long list. One sent whole is lost in part on its
/// way: a UDP socket holds a few hundred frames and drops the rest.
const LIST_AT_ONCE: usize = 32;

/// What the camera keeps of its own settings. Its mode is the camera's.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Settings {
    manual_exposure: bool,
    ev: f32,
    iso: u32,
    shutter_s: f32,
    /// 0 is 3840 x 2160, 1 is 1920 x 1080 and 2 is 1280 x 720.
    video_size: u32,
    video_rate: u32,
    /// 0 is JPEG, 1 is RAW and 2 is both.
    photo_format: u32,
    /// The size its video is changing to, and when it has changed.
    resizing: Option<(u32, u64)>,
    /// The next setting of a list it is part of the way through sending.
    listing: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            manual_exposure: false,
            ev: 0.0,
            iso: AUTO_ISO,
            shutter_s: AUTO_SHUTTER_S,
            video_size: 1,
            video_rate: 30,
            photo_format: 0,
            resizing: None,
            listing: None,
        }
    }
}

impl Settings {
    /// What an image takes of the card, in MiB.
    pub(super) fn image_mib(&self) -> f32 {
        match self.photo_format {
            0 => 8.0,
            1 => 24.0,
            _ => 32.0,
        }
    }

    /// What a second of video takes of the card, in MiB.
    #[allow(clippy::cast_precision_loss, reason = "a rate of at most 60")]
    pub(super) fn video_mib_per_s(&self) -> f32 {
        let size = match self.video_size {
            0 => 6.0,
            1 => 1.5,
            _ => 0.75,
        };
        size * self.video_rate as f32 / 30.0
    }
}

impl Camera {
    /// Whether it has settings of its own: it names a definition file,
    /// and is not one that only sends video.
    fn has_settings(&self) -> bool {
        !self.video_only && self.definition_uri.is_some()
    }

    /// The files it serves over MAVLink FTP: its definition file alone.
    pub(super) fn own_files(definition: &[u8]) -> FakeFiles {
        let mut files = FakeFiles::default();
        files.files.clear();
        files.dirs = std::iter::once("/".to_owned()).collect();
        files
            .files
            .insert(DEFINITION_PATH.to_owned(), definition.to_vec());
        files
    }

    /// Serves this file as its definition, where its own would be.
    pub fn serve_definition(&mut self, bytes: &[u8]) {
        self.files = Self::own_files(bytes);
    }

    /// A message about its files or its settings, from `from`: a MAVLink
    /// FTP request, or one of the extended parameter protocol's.
    pub fn settings_message(&mut self, from: (u8, u8), message: &Ardupilotmega) -> Vec<Vec<u8>> {
        match message {
            Ardupilotmega::FileTransferProtocol(m) => {
                let request = crate::transfers::ftp::read(&m.payload);
                let answered = self.files.answer(&request, Vec::new);
                answered
                    .payloads
                    .into_iter()
                    .map(|payload| {
                        self.sent = self.sent.wrapping_add(1);
                        crate::sim::ftp_frame(
                            (self.system, self.component),
                            from,
                            self.sent,
                            payload,
                        )
                    })
                    .collect()
            }
            _ if !self.has_settings() => Vec::new(),
            Ardupilotmega::ParamExtRequestList(_) => {
                self.settings.listing = Some(0);
                self.list_on()
            }
            Ardupilotmega::ParamExtRequestRead(m) => {
                let index = usize::try_from(m.param_index).ok().or_else(|| {
                    let name = crate::decode::param_name(&m.param_id);
                    NAMES.iter().position(|known| *known == name)
                });
                index
                    .and_then(|index| self.setting_said(index))
                    .into_iter()
                    .collect()
            }
            Ardupilotmega::ParamExtSet(m) => {
                let name = crate::decode::param_name(&m.param_id);
                let Some(value) = Value::read(m.param_type as u8, &m.param_value) else {
                    return Vec::new();
                };
                let (result, now) = self.set_setting(&name, &value);
                let mut frames = vec![self.setting_acked(&name, &now, result)];
                // Its mode is told as the protocol's own setting too.
                if name == NAMES[0] && result == ParamAck::Accepted {
                    frames.push(self.report(super::Report::Settings));
                }
                frames
            }
            _ => Vec::new(),
        }
    }

    /// What is due of its settings by now: a video size that has changed
    /// is acknowledged again, as accepted.
    pub(super) fn settings_step(&mut self) -> Vec<Vec<u8>> {
        let mut frames = self.list_on();
        let Some((size, at_ms)) = self.settings.resizing else {
            return frames;
        };
        if self.now_ms < at_ms {
            return frames;
        }
        self.settings.resizing = None;
        self.settings.video_size = size;
        if size == LARGEST && self.settings.video_rate > 30 {
            self.settings.video_rate = 30;
        }
        frames.push(self.setting_acked("CAM_VIDRES", &Value::Uint32(size), ParamAck::Accepted));
        frames
    }

    /// The next [`LIST_AT_ONCE`] settings of a list it is sending.
    fn list_on(&mut self) -> Vec<Vec<u8>> {
        let Some(from) = self.settings.listing else {
            return Vec::new();
        };
        let to = (from + LIST_AT_ONCE).min(self.listed());
        self.settings.listing = (to < self.listed()).then_some(to);
        (from..to)
            .filter_map(|index| self.setting_said(index))
            .collect()
    }

    /// One setting's value now, by its place in the list.
    fn setting(&self, index: usize) -> Option<Value> {
        let settings = &self.settings;
        Some(match *NAMES.get(index)? {
            "CAM_MODE" => Value::Uint32(u32::from(self.video_mode)),
            "CAM_EXPMODE" => Value::Uint32(u32::from(settings.manual_exposure)),
            "CAM_EV" => Value::Real32(settings.ev),
            "CAM_ISO" => Value::Uint32(settings.iso),
            "CAM_SHUTTERSPD" => Value::Real32(settings.shutter_s),
            "CAM_VIDRES" => Value::Uint32(settings.video_size),
            "CAM_VIDFPS" => Value::Uint32(settings.video_rate),
            "CAM_PHOTOFMT" => Value::Uint32(settings.photo_format),
            _ => Value::Uint32(self.shots),
        })
    }

    /// Writes a setting: how the camera answers, and the value it has
    /// after, or the one sent for a setting it does not have.
    #[allow(clippy::float_cmp, reason = "one of the values it lists")]
    fn set_setting(&mut self, name: &str, value: &Value) -> (ParamAck, Value) {
        let Some(index) = NAMES.iter().position(|known| *known == name) else {
            return (ParamAck::ValueUnsupported, value.clone());
        };
        let held = self.setting(index).unwrap_or_else(|| value.clone());
        if held.kind() != value.kind() {
            return (ParamAck::ValueUnsupported, held);
        }
        let (video, recording) = (self.video_mode, self.recording.is_some());
        let settings = &mut self.settings;
        let result = match (name, value) {
            // One that captures only in the mode for it keeps its mode
            // while it captures (ADR-0063 Y16), by this way as by its
            // command.
            ("CAM_MODE", Value::Uint32(mode @ (0 | 1)))
                if self.mode_bound
                    && (recording || self.interval.is_some())
                    && (*mode == 1) != video =>
            {
                ParamAck::Failed
            }
            ("CAM_MODE", Value::Uint32(mode @ (0 | 1))) => {
                self.video_mode = *mode == 1;
                ParamAck::Accepted
            }
            ("CAM_EXPMODE", Value::Uint32(mode @ (0 | 1))) => {
                settings.manual_exposure = *mode == 1;
                // Automatic exposure picks these itself.
                if !settings.manual_exposure {
                    (settings.iso, settings.shutter_s) = (AUTO_ISO, AUTO_SHUTTER_S);
                }
                ParamAck::Accepted
            }
            // What its present mode rules out, and a video's size and rate
            // while it records.
            ("CAM_EV", _) if settings.manual_exposure => ParamAck::Failed,
            ("CAM_ISO" | "CAM_SHUTTERSPD", _) if !settings.manual_exposure => ParamAck::Failed,
            ("CAM_VIDRES" | "CAM_VIDFPS", _) if !video || recording => ParamAck::Failed,
            ("CAM_PHOTOFMT", _) if video => ParamAck::Failed,
            ("CAM_EV", Value::Real32(ev))
                if (-3.0..=3.0).contains(ev) && (ev * 2.0).fract() == 0.0 =>
            {
                settings.ev = *ev;
                ParamAck::Accepted
            }
            ("CAM_ISO", Value::Uint32(iso)) if ISOS.contains(iso) => {
                settings.iso = *iso;
                ParamAck::Accepted
            }
            ("CAM_SHUTTERSPD", Value::Real32(shutter)) if SHUTTERS_S.contains(shutter) => {
                settings.shutter_s = *shutter;
                ParamAck::Accepted
            }
            ("CAM_VIDRES", Value::Uint32(size @ 0..=2)) => match settings.resizing {
                // The same write again, as its sender does when the
                // answer was lost.
                Some((to, _)) if to == *size => ParamAck::InProgress,
                Some(_) => ParamAck::Failed,
                None if settings.video_size == *size => ParamAck::Accepted,
                None => {
                    settings.resizing = Some((*size, self.now_ms + RESIZE_MS));
                    ParamAck::InProgress
                }
            },
            ("CAM_VIDFPS", Value::Uint32(rate))
                if RATES.contains(rate) && (*rate <= 30 || settings.video_size != LARGEST) =>
            {
                settings.video_rate = *rate;
                ParamAck::Accepted
            }
            ("CAM_PHOTOFMT", Value::Uint32(format @ 0..=2)) => {
                settings.photo_format = *format;
                ParamAck::Accepted
            }
            // It counts its photos itself.
            ("CAM_SHUTTERCNT", _) => ParamAck::Failed,
            _ => ParamAck::ValueUnsupported,
        };
        (result, self.setting(index).unwrap_or(held))
    }

    /// How many settings its list has: its own, and any it was given
    /// beyond them ([`Camera::more_settings`]).
    fn listed(&self) -> usize {
        NAMES.len() + usize::from(self.more_settings)
    }

    fn setting_said(&mut self, index: usize) -> Option<Vec<u8>> {
        // One beyond its own is a number that is always 0.
        let (name, value) = match NAMES.get(index) {
            Some(name) => ((*name).to_owned(), self.setting(index)?),
            None if index < self.listed() => (format!("X{index}"), Value::Uint8(0)),
            None => return None,
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "nine settings, and at most 65,535 more"
        )]
        let said = ParamExtValue {
            param_count: self.listed().min(usize::from(u16::MAX)) as u16,
            param_index: index as u16,
            param_id: wire::text(&name),
            param_value: value.bytes(),
            param_type: crate::sim::ext_type(value.kind()),
        };
        Some(self.send(&said))
    }

    fn setting_acked(&mut self, name: &str, value: &Value, result: ParamAck) -> Vec<u8> {
        self.send(&ParamExtAck {
            param_id: wire::text(name),
            param_value: value.bytes(),
            param_type: crate::sim::ext_type(value.kind()),
            param_result: result,
        })
    }
}
