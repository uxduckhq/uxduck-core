//! Cameras on a companion computer (S359, S360, ADR-0060): what `MediaMTX`
//! on it serves, as its local API said when UXDuck last read it over SSH,
//! the picture settings UXDuck writes for a `rpiCamera` path, the presets
//! that name them, and the camera watchdog. The computer is the truth (C1):
//! this is the last read, kept on its check beside its networks.

use serde::{Deserialize, Serialize};

/// The picture of a `rpiCamera` path: what UXDuck sets through `MediaMTX`'s
/// environment (C2, C3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureSettings {
    pub width: u32,
    pub height: u32,
    /// Pictures a second.
    pub fps: u32,
    /// The sensor mode as `MediaMTX` takes it, `width:height:bit depth:packing`
    /// such as `2304:1296:10:P`. Empty lets the camera choose.
    #[serde(default)]
    pub mode: String,
    /// Pictures from one keyframe to the next.
    pub idr_period: u32,
    /// Bits a second.
    pub bitrate: u32,
}

/// UXDuck's presets (C4). The vehicle keeps numbers, and the page names the
/// preset they match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PicturePreset {
    LowLatency,
    Detail,
    Fastest,
    DataSaver,
}

impl PicturePreset {
    pub const ALL: [Self; 4] = [
        Self::LowLatency,
        Self::Detail,
        Self::Fastest,
        Self::DataSaver,
    ];

    /// Its numbers, each with a keyframe once a second.
    #[must_use]
    pub fn settings(self) -> PictureSettings {
        let (width, height, fps, mode, bitrate) = match self {
            Self::LowLatency => (1536, 864, 50, "2304:1296:10:P", 2_000_000),
            Self::Detail => (1920, 1080, 30, "", 2_000_000),
            Self::Fastest => (1280, 720, 60, "1536:864:10:P", 2_000_000),
            Self::DataSaver => (1280, 720, 30, "2304:1296:10:P", 1_000_000),
        };
        PictureSettings {
            width,
            height,
            fps,
            mode: mode.to_owned(),
            idr_period: fps,
            bitrate,
        }
    }

    /// The preset `settings` are, if any.
    #[must_use]
    pub fn matching(settings: &PictureSettings) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.settings() == *settings)
    }

    /// The presets a camera with `sensor` can take. One that names a sensor
    /// mode is offered only for a sensor whose modes UXDuck knows.
    #[must_use]
    pub fn offered(sensor: Option<&str>) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|preset| {
                let settings = preset.settings();
                (settings.mode.is_empty() || sensor.and_then(sensor_modes).is_some())
                    && check_picture(&settings, sensor).is_ok()
            })
            .collect()
    }
}

/// One mode of a sensor: its size and the most pictures a second it gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SensorMode {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
}

/// Camera Module 3 (IMX708), every variant: its 10-bit modes as libcamera
/// lists them. 1536×864 is the middle of the sensor (a cropped view), the
/// other two see all of it.
const IMX708: &[SensorMode] = &[
    SensorMode {
        width: 1536,
        height: 864,
        max_fps: 120,
    },
    SensorMode {
        width: 2304,
        height: 1296,
        max_fps: 56,
    },
    SensorMode {
        width: 4608,
        height: 2592,
        max_fps: 14,
    },
];

/// The modes of a sensor UXDuck knows, by the name the computer gives it
/// (`imx708`, `imx708_wide`, `imx708_noir`).
#[must_use]
pub fn sensor_modes(sensor: &str) -> Option<&'static [SensorMode]> {
    let base = sensor
        .trim()
        .to_ascii_lowercase()
        .replace("_wide", "")
        .replace("_noir", "");
    (base == "imx708").then_some(IMX708)
}

/// Whether UXDuck can set a path's picture from the environment (C2). Its
/// name becomes part of an environment variable's name: `MediaMTX` reads
/// that name in lower case and ends it at the first `_`, and systemd keeps
/// only letters, digits and `_` in one.
///
/// # Errors
/// Why not, for people.
pub fn check_path_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("A camera path needs a name.".to_owned());
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(format!(
            "MediaMTX reads path names from the environment in lower case, so UXDuck cannot set {name}. Rename it in lower case to adjust it here."
        ));
    }
    if name.contains('_') {
        return Err(format!(
            "MediaMTX ends a path's name at the first _ in the environment, so UXDuck cannot set {name}. Rename it without _ to adjust it here."
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(format!(
            "systemd allows only letters, digits and _ in an environment variable's name, so UXDuck cannot set {name}. Rename it with letters and digits only to adjust it here."
        ));
    }
    Ok(())
}

/// Bounds UXDuck keeps to for a sensor it does not know. `MediaMTX` itself
/// only refuses a width or height of zero.
const ANY_SIZE: std::ops::RangeInclusive<u32> = 64..=8192;
const ANY_FPS: std::ops::RangeInclusive<u32> = 1..=120;
const IDR_PERIOD: std::ops::RangeInclusive<u32> = 1..=600;
const BITRATE: std::ops::RangeInclusive<u32> = 100_000..=25_000_000;

/// A sensor mode string's size and bit depth: `width:height`, then
/// optionally `:bit depth` and then `:P` or `:U` for its packing.
fn parse_mode(mode: &str) -> Option<(u32, u32, Option<u32>)> {
    let parts: Vec<_> = mode.split(':').collect();
    let number = |i: usize| parts.get(i)?.parse::<u32>().ok();
    match parts.len() {
        2 => Some((number(0)?, number(1)?, None)),
        3 => Some((number(0)?, number(1)?, Some(number(2)?))),
        4 if matches!(parts[3], "P" | "U") => Some((number(0)?, number(1)?, Some(number(2)?))),
        _ => None,
    }
}

/// Whether a camera with `sensor` can take `settings`: its modes where
/// UXDuck knows the sensor, and UXDuck's bounds always (C3).
///
/// # Errors
/// What is wrong, for people.
pub fn check_picture(settings: &PictureSettings, sensor: Option<&str>) -> Result<(), String> {
    let PictureSettings {
        width,
        height,
        fps,
        mode,
        idr_period,
        bitrate,
    } = settings;
    if !ANY_SIZE.contains(width) || !ANY_SIZE.contains(height) {
        return Err("Use a width and height from 64 to 8192.".to_owned());
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err("Use an even width and height.".to_owned());
    }
    if !ANY_FPS.contains(fps) {
        return Err("Use 1 to 120 pictures a second.".to_owned());
    }
    if !IDR_PERIOD.contains(idr_period) {
        return Err("Use a keyframe every 1 to 600 pictures.".to_owned());
    }
    if !BITRATE.contains(bitrate) {
        return Err("Use a bitrate from 0.1 to 25 Mbit/s.".to_owned());
    }
    let chosen = if mode.is_empty() {
        None
    } else {
        Some(parse_mode(mode).ok_or_else(|| {
            format!("{mode} is not a sensor mode. Write it as width:height:bit depth:packing, such as 2304:1296:10:P.")
        })?)
    };
    let (Some(sensor), Some(modes)) = (sensor, sensor.and_then(sensor_modes)) else {
        return Ok(());
    };
    if let Some((mode_width, mode_height, depth)) = chosen {
        let Some(found) = modes
            .iter()
            .find(|m| m.width == mode_width && m.height == mode_height)
        else {
            return Err(format!(
                "The {sensor} sensor has no {mode_width}×{mode_height} mode. Its modes are {}.",
                modes
                    .iter()
                    .map(|m| format!("{}×{}", m.width, m.height))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        if depth.is_some_and(|d| d != 10) {
            return Err(format!("The {sensor} sensor's modes are 10-bit."));
        }
        if *width > found.width || *height > found.height {
            return Err(format!(
                "{width}×{height} is larger than the {mode_width}×{mode_height} sensor mode."
            ));
        }
        if *fps > found.max_fps {
            return Err(format!(
                "The {mode_width}×{mode_height} sensor mode gives at most {} pictures a second.",
                found.max_fps
            ));
        }
        return Ok(());
    }
    if modes
        .iter()
        .any(|m| m.width >= *width && m.height >= *height && m.max_fps >= *fps)
    {
        Ok(())
    } else {
        Err(format!(
            "The {sensor} sensor cannot give {width}×{height} at {fps} pictures a second."
        ))
    }
}

/// What `MediaMTX` on a computer serves, as its local API said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCameras {
    /// The systemd unit running a `mediamtx` binary; absent when none runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Its configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// Its API, such as `127.0.0.1:9997`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// UXDuck's drop-in for the unit, whether or not it exists yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_in: Option<String>,
    /// The unit's user, when it is a login user (C7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger: Option<CameraLinger>,
    #[serde(default)]
    pub paths: Vec<CompanionCameraPath>,
    #[serde(default)]
    pub watchdog: CameraWatchdog,
    /// What the picture change that made this read did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<PictureApplied>,
    /// Why the cameras could not be read, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The unit's login user and whether its sessions ending can stop the
/// cameras (C7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraLinger {
    pub user: String,
    /// `loginctl enable-linger` is on for it.
    pub lingering: bool,
    /// systemd-logind removes its shared memory when its last session ends.
    pub remove_ipc: bool,
}

impl CameraLinger {
    /// Signing in over SSH as this user can stop the cameras.
    #[must_use]
    pub fn at_risk(&self) -> bool {
        self.remove_ipc && !self.lingering
    }
}

/// One path in `MediaMTX`'s configuration, with its live state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCameraPath {
    pub name: String,
    /// `rpiCamera`, `publisher`, or the scheme of the address it pulls from
    /// (never the address, which can hold a password).
    pub source: String,
    /// `rpiCameraCamID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_id: Option<u32>,
    /// The sensor, such as `imx708`, when the computer says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<String>,
    /// Its effective picture, for a `rpiCamera` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PictureSettings>,
    /// UXDuck's drop-in sets its picture.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub set_by_uxduck: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_demand: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready: bool,
    #[serde(default)]
    pub readers: u32,
    /// Bytes it has received since `MediaMTX` started.
    #[serde(default)]
    pub bytes: u64,
}

impl CompanionCameraPath {
    /// Whether UXDuck can set its picture here, or why not (C2, C3).
    ///
    /// # Errors
    /// Why not, for people.
    pub fn adjustable(&self) -> Result<(), String> {
        if self.source != "rpiCamera" {
            return Err(
                "Only a Raspberry Pi camera (source rpiCamera) can be adjusted here.".to_owned(),
            );
        }
        check_path_name(&self.name)
    }
}

/// The camera watchdog (C9, C10).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraWatchdog {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub installed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    /// It stopped restarting after three restarts in ten minutes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gave_up: bool,
    /// Its recent restarts, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restarts: Vec<WatchdogRestart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogRestart {
    /// When, by the computer's clock.
    pub at_us: u64,
    /// The camera that had stopped.
    pub path: String,
}

/// How a picture change ended (C5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PictureOutcome {
    Applied,
    /// A camera did not come back, and the computer put the old settings back.
    RolledBack,
    /// The old settings did not bring every camera back either.
    RollbackFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureApplied {
    /// The path whose picture was changed.
    pub path: String,
    pub outcome: PictureOutcome,
    /// The camera that did not come back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
    /// `MediaMTX`'s last lines for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, fps: u32, mode: &str) -> PictureSettings {
        PictureSettings {
            width,
            height,
            fps,
            mode: mode.to_owned(),
            idr_period: fps,
            bitrate: 2_000_000,
        }
    }

    #[test]
    fn the_ugv_settings_are_low_latency() {
        let now = picture(1536, 864, 50, "2304:1296:10:P");
        assert_eq!(
            PicturePreset::matching(&now),
            Some(PicturePreset::LowLatency)
        );
        assert_eq!(
            PicturePreset::matching(&picture(1920, 1080, 30, "")),
            Some(PicturePreset::Detail)
        );
        assert_eq!(PicturePreset::matching(&picture(1920, 1080, 25, "")), None);
    }

    #[test]
    fn presets_needing_a_mode_wait_for_a_known_sensor() {
        assert_eq!(PicturePreset::offered(Some("imx708")), PicturePreset::ALL);
        assert_eq!(
            PicturePreset::offered(Some("imx708_wide_noir")),
            PicturePreset::ALL
        );
        assert_eq!(
            PicturePreset::offered(None),
            [PicturePreset::Detail],
            "an unknown sensor's modes are unknown"
        );
        assert_eq!(
            PicturePreset::offered(Some("imx477")),
            [PicturePreset::Detail]
        );
        for preset in PicturePreset::ALL {
            assert_eq!(check_picture(&preset.settings(), Some("imx708")), Ok(()));
        }
    }

    #[test]
    fn values_are_checked_against_the_sensors_modes() {
        let imx708 = Some("imx708");
        assert!(
            check_picture(&picture(1536, 864, 60, "2304:1296:10:P"), imx708)
                .unwrap_err()
                .contains("at most 56")
        );
        assert!(check_picture(&picture(1920, 1080, 60, "1536:864:10:P"), imx708).is_err());
        assert!(check_picture(&picture(1280, 720, 30, "1920:1080:10:P"), imx708).is_err());
        assert!(check_picture(&picture(1280, 720, 30, "2304:1296:12:P"), imx708).is_err());
        assert!(check_picture(&picture(1920, 1080, 100, ""), imx708).is_err());
        assert!(check_picture(&picture(1280, 720, 100, ""), imx708).is_ok());
        assert!(check_picture(&picture(1280, 720, 30, "2304:1296"), imx708).is_ok());
        assert!(check_picture(&picture(1280, 720, 30, "2304:1296:10:X"), None).is_err());
        assert!(check_picture(&picture(1280, 720, 30, "2304:1296:10:P:x"), None).is_err());
        assert!(check_picture(&picture(1280, 720, 30, "2304:1296:10:P"), None).is_ok());
        assert!(check_picture(&picture(1281, 720, 30, ""), None).is_err());
        assert!(check_picture(&picture(1280, 720, 0, ""), None).is_err());
        assert!(check_picture(&picture(8, 8, 30, ""), None).is_err());
        let mut fast = picture(1280, 720, 30, "");
        fast.bitrate = 50_000_000;
        assert!(check_picture(&fast, None).is_err());
        fast.bitrate = 2_000_000;
        fast.idr_period = 0;
        assert!(check_picture(&fast, None).is_err());
    }

    #[test]
    fn only_names_the_environment_can_carry_are_adjustable() {
        assert!(check_path_name("front").is_ok());
        assert!(check_path_name("cam2").is_ok());
        assert!(check_path_name("Front").unwrap_err().contains("lower case"));
        assert!(check_path_name("cam_1").unwrap_err().contains("first _"));
        assert!(check_path_name("cam-1").unwrap_err().contains("systemd"));
        assert!(check_path_name("cams/front").is_err());
        assert!(check_path_name("~^cam$").is_err());
        assert!(check_path_name("").is_err());
        let proxy = CompanionCameraPath {
            name: "front".into(),
            source: "rtsp".into(),
            camera_id: None,
            sensor: None,
            settings: None,
            set_by_uxduck: false,
            on_demand: false,
            ready: true,
            readers: 0,
            bytes: 0,
        };
        assert!(proxy.adjustable().unwrap_err().contains("rpiCamera"));
    }

    #[test]
    fn lingering_matters_only_where_ipc_is_removed() {
        let mut linger = CameraLinger {
            user: "uxduck".into(),
            lingering: false,
            remove_ipc: true,
        };
        assert!(linger.at_risk());
        linger.lingering = true;
        assert!(!linger.at_risk());
        linger.lingering = false;
        linger.remove_ipc = false;
        assert!(!linger.at_risk());
    }
}
