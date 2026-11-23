//! `/etc/uxduck/companion.toml` (ADR-0061 B4, ADR-0066): the features a camera
//! computer runs, in one shape that the app's vehicle page also reads and
//! writes. `[video]` turns on the video sender, `[mavlink]` names the vehicle's
//! router, and each `[cameras.<path>]` table is a camera by its `MediaMTX`
//! path: whether it follows the connection, its floor, and where and when it
//! records on the vehicle.
//!
//! ```toml
//! [video]
//! interfaces = ["eth0", "tailscale0"]
//!
//! [mavlink]
//! router = "udp:127.0.0.1:15010"
//! system_id = 1
//!
//! [cameras.front]
//! follow = true
//! floor_bitrate = 150000
//! record_path = "front_record"
//! record = "armed"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::onboard_recording::{self, Mode, PathConfig};
use super::sender::{self, CameraConfig};

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub video: Option<sender::Config>,
    #[serde(default)]
    pub mavlink: Mavlink,
    #[serde(default)]
    pub cameras: BTreeMap<String, Camera>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mavlink {
    /// The vehicle's MAVLink router, `udp:HOST:PORT`.
    #[serde(default = "default_router")]
    pub router: String,
    /// The vehicle's system id (`SYSID_THISMAV` on `ArduPilot`).
    #[serde(default = "default_system_id")]
    pub system_id: u8,
}

impl Default for Mavlink {
    fn default() -> Self {
        Self {
            router: default_router(),
            system_id: default_system_id(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    /// Whether the sender follows the connection. A Raspberry Pi camera
    /// follows unless this is false.
    pub follow: Option<bool>,
    /// The lowest bitrate it follows down to, bit/s.
    #[serde(default = "sender::default_floor")]
    pub floor_bitrate: u64,
    /// The `MediaMTX` path that records it on the vehicle. None: it does not.
    pub record_path: Option<String>,
    /// When it records: `armed` when unset.
    pub record: Option<Mode>,
}

fn default_router() -> String {
    "udp:127.0.0.1:15010".into()
}
fn default_system_id() -> u8 {
    1
}

impl File {
    /// Reads and checks the file at `path`.
    ///
    /// # Errors
    /// A file that cannot be read, does not parse or cannot work, in words a
    /// person can act on.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// # Errors
    /// Text that does not parse or cannot work.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self = toml::from_str(text).map_err(|e| format!("does not read: {e}"))?;
        file.check()?;
        Ok(file)
    }

    /// # Errors
    /// What is wrong with it.
    pub fn check(&self) -> Result<(), String> {
        for (name, camera) in &self.cameras {
            if camera.record.is_some() && camera.record_path.is_none() {
                return Err(format!(
                    "the camera {name} has record but no record_path, the MediaMTX path that records it"
                ));
            }
        }
        let recorder = self.recorder();
        if self.video.is_none() && recorder.is_none() {
            return Err("turns on no feature: add a [video] section, or a record_path to a camera under [cameras.<path>]".into());
        }
        if let Some(video) = &self.video {
            video.check(&self.sender_cameras())?;
        }
        if let Some(recorder) = recorder {
            recorder.check()?;
        }
        Ok(())
    }

    /// The cameras the video sender answers for: every camera.
    #[must_use]
    pub fn sender_cameras(&self) -> Vec<CameraConfig> {
        self.cameras
            .iter()
            .map(|(path, camera)| CameraConfig {
                path: path.clone(),
                floor_bitrate: camera.floor_bitrate,
                follow: camera.follow.unwrap_or(true),
            })
            .collect()
    }

    /// The recorder, when a camera records on the vehicle.
    #[must_use]
    pub fn recorder(&self) -> Option<onboard_recording::Config> {
        let paths: Vec<PathConfig> = self
            .cameras
            .iter()
            .filter_map(|(name, camera)| {
                Some(PathConfig {
                    path: camera.record_path.clone()?,
                    camera: name.clone(),
                    mode: camera.record.unwrap_or_default(),
                })
            })
            .collect();
        (!paths.is_empty()).then(|| onboard_recording::Config {
            mavlink: self.mavlink.router.clone(),
            system_id: self.mavlink.system_id,
            mediamtx_api: self
                .video
                .as_ref()
                .map_or_else(sender::default_api, |v| v.mediamtx_api.clone()),
            paths,
        })
    }

    /// Whether `newer` changes more than the recording modes, which a reload
    /// applies. The rest needs a restart.
    #[must_use]
    pub fn beyond_modes(&self, newer: &Self) -> bool {
        let without_modes = |file: &Self| {
            let mut file = file.clone();
            for camera in file.cameras.values_mut() {
                camera.record = None;
            }
            file
        };
        without_modes(self) != without_modes(newer)
    }
}

#[cfg(test)]
mod tests;
