//! Firmware to install (S189, ADR-0037 V10): a person's `.apj` or `.px4`
//! file, or a release from `ArduPilot`'s manifest
//! (`firmware.ardupilot.org/manifest.json.gz`), which UXDuck fetches at run
//! time and never bundles. Both file kinds are JSON with the board's id and
//! the image, compressed with zlib and in base64; the manifest lists every
//! build with its board id, vehicle type, release type and address.

use std::io::Read as _;

use base64::Engine as _;
use serde::Deserialize;

/// The most a firmware file may be: the largest boards have 2 MB of flash,
/// and base64 makes it a third larger.
pub const FILE_MAX: usize = 8 * 1024 * 1024;
/// The most an image may unpack to.
const IMAGE_MAX: u64 = 16 * 1024 * 1024;

/// A firmware file, read and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareFile {
    /// The board it is built for.
    pub board_id: u32,
    pub board_revision: Option<u32>,
    /// The image to write, unpacked.
    pub image: Vec<u8>,
    /// `ArduPilot`'s `.apj` or PX4's `.px4`, by its magic.
    pub px4: bool,
    /// `arducopter`, `ArduPlane`…
    pub summary: Option<String>,
    pub git_identity: Option<String>,
    /// Its own version, where it says one (PX4's files do; `ArduPilot`'s
    /// say "0.1").
    pub version: Option<String>,
}

/// Why a file cannot be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    TooLarge,
    /// Not a firmware file's JSON.
    NotFirmware,
    Missing(&'static str),
    /// The image is not base64 zlib.
    BadImage,
    /// The image unpacked to another size than the file says.
    SizeMismatch {
        said: u64,
        got: u64,
    },
    /// It also writes the board's external flash, which UXDuck does not
    /// yet.
    ExternalFlash,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "it is larger than any firmware file"),
            Self::NotFirmware => write!(f, "it is not an .apj or .px4 firmware file"),
            Self::Missing(field) => write!(f, "it has no {field}"),
            Self::BadImage => write!(f, "its image cannot be read"),
            Self::SizeMismatch { said, got } => {
                write!(f, "its image is {got} bytes, and the file says {said}")
            }
            Self::ExternalFlash => write!(
                f,
                "it also writes the board's external flash, which UXDuck does not install yet"
            ),
        }
    }
}

impl std::error::Error for FileError {}

#[derive(Deserialize)]
struct RawFile {
    magic: Option<String>,
    board_id: Option<serde_json::Value>,
    board_revision: Option<serde_json::Value>,
    image: Option<String>,
    image_size: Option<u64>,
    summary: Option<String>,
    git_identity: Option<String>,
    version: Option<String>,
    extf_image: Option<String>,
    extf_image_size: Option<u64>,
}

/// A JSON number, or a number in a string as some files have.
fn number(value: Option<&serde_json::Value>) -> Option<u32> {
    match value? {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Reads an `.apj` or `.px4` file: its board id and image, checked against
/// the size it says.
///
/// # Errors
/// Why it is not one UXDuck installs.
pub fn read_file(bytes: &[u8]) -> Result<FirmwareFile, FileError> {
    if bytes.len() > FILE_MAX {
        return Err(FileError::TooLarge);
    }
    let raw: RawFile = serde_json::from_slice(bytes).map_err(|_| FileError::NotFirmware)?;
    let board_id = number(raw.board_id.as_ref()).ok_or(FileError::Missing("board id"))?;
    let packed = raw.image.ok_or(FileError::Missing("image"))?;
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(packed.trim())
        .map_err(|_| FileError::BadImage)?;
    let mut image = Vec::new();
    flate2::read::ZlibDecoder::new(compressed.as_slice())
        .take(IMAGE_MAX)
        .read_to_end(&mut image)
        .map_err(|_| FileError::BadImage)?;
    if let Some(said) = raw.image_size
        && said != image.len() as u64
    {
        return Err(FileError::SizeMismatch {
            said,
            got: image.len() as u64,
        });
    }
    if raw.extf_image.is_some_and(|i| !i.trim().is_empty())
        && raw.extf_image_size.is_none_or(|n| n > 0)
    {
        return Err(FileError::ExternalFlash);
    }
    Ok(FirmwareFile {
        board_id,
        board_revision: number(raw.board_revision.as_ref()),
        image,
        px4: raw.magic.as_deref() == Some("PX4FWv1"),
        summary: raw.summary,
        git_identity: raw.git_identity,
        version: raw.version.filter(|v| v != "0.1"),
    })
}

/// A firmware file around an image, as `ArduPilot`'s build writes one: for
/// tests and the fake board.
#[must_use]
pub fn write_file(board_id: u32, image: &[u8], summary: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(image);
    let compressed = encoder.finish().unwrap_or_default();
    serde_json::json!({
        "magic": "APJFWv1",
        "board_id": board_id,
        "board_revision": 0,
        "summary": summary,
        "version": "0.1",
        "image_size": image.len(),
        "image": base64::engine::general_purpose::STANDARD.encode(compressed),
    })
    .to_string()
    .into_bytes()
}

/// A kind of vehicle `ArduPilot` builds firmware for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VehicleType {
    Copter,
    Heli,
    Plane,
    Rover,
    Sub,
    Blimp,
    Tracker,
}

impl VehicleType {
    pub const ALL: [Self; 7] = [
        Self::Copter,
        Self::Heli,
        Self::Plane,
        Self::Rover,
        Self::Sub,
        Self::Blimp,
        Self::Tracker,
    ];

    /// On the wire and in the CLI: "copter".
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Copter => "copter",
            Self::Heli => "heli",
            Self::Plane => "plane",
            Self::Rover => "rover",
            Self::Sub => "sub",
            Self::Blimp => "blimp",
            Self::Tracker => "tracker",
        }
    }

    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.slug().eq_ignore_ascii_case(slug.trim()))
    }

    /// For people: "Copter", "Helicopter".
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Copter => "Copter",
            Self::Heli => "Helicopter",
            Self::Plane => "Plane",
            Self::Rover => "Rover",
            Self::Sub => "Sub",
            Self::Blimp => "Blimp",
            Self::Tracker => "Antenna tracker",
        }
    }

    /// The manifest's `vehicletype` and `mav-type` for it.
    const fn manifest(self) -> (&'static str, &'static str) {
        match self {
            Self::Copter => ("Copter", "Copter"),
            Self::Heli => ("Copter", "HELICOPTER"),
            Self::Plane => ("Plane", "FIXED_WING"),
            Self::Rover => ("Rover", "GROUND_ROVER"),
            Self::Sub => ("Sub", "SUBMARINE"),
            Self::Blimp => ("Blimp", "Blimp"),
            Self::Tracker => ("AntennaTracker", "ANTENNA_TRACKER"),
        }
    }

    /// The firmware a vehicle of this `MAV_TYPE` (as UXDuck names it,
    /// "Quadrotor") runs, where it is one `ArduPilot` builds.
    #[must_use]
    pub fn of_vehicle(vehicle_type: &str) -> Option<Self> {
        let lower = vehicle_type.to_ascii_lowercase();
        Some(match lower.as_str() {
            "helicopter" => Self::Heli,
            "fixedwing" => Self::Plane,
            l if l.starts_with("vtol") => Self::Plane,
            "groundrover" | "surfaceboat" => Self::Rover,
            "submarine" => Self::Sub,
            "airship" => Self::Blimp,
            "antennatracker" => Self::Tracker,
            "quadrotor" | "hexarotor" | "octorotor" | "tricopter" | "coaxial" | "dodecarotor"
            | "decarotor" => Self::Copter,
            _ => return None,
        })
    }
}

/// Which line of releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    /// The current stable release (`OFFICIAL`).
    Stable,
    Beta,
    /// Built from the latest code (`DEV`).
    Dev,
    /// An earlier stable release (`STABLE-4.6.2`).
    Older,
}

impl Channel {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Dev => "dev",
            Self::Older => "older",
        }
    }

    fn of(release_type: &str) -> Option<Self> {
        Some(match release_type {
            "OFFICIAL" => Self::Stable,
            "BETA" => Self::Beta,
            "DEV" => Self::Dev,
            t if t.starts_with("STABLE-") => Self::Older,
            _ => return None,
        })
    }
}

/// One build in the manifest that UXDuck can install: an `.apj`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ManifestEntry {
    pub board_id: u32,
    pub platform: String,
    #[serde(rename = "vehicletype")]
    pub vehicle_type: String,
    #[serde(rename = "mav-type")]
    pub mav_type: String,
    #[serde(rename = "mav-firmware-version-type")]
    pub release_type: String,
    /// "V4.7.2", "V4.8.0-dev".
    #[serde(rename = "mav-firmware-version-str", default)]
    pub version_text: String,
    /// "4.7.2".
    #[serde(rename = "mav-firmware-version", default)]
    pub version: String,
    pub url: String,
    #[serde(rename = "USBID", default)]
    pub usb_ids: Vec<String>,
    #[serde(default)]
    pub brand_name: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub image_size: Option<u64>,
    #[serde(rename = "git-sha", default)]
    pub git_sha: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    format: Option<String>,
    board_id: Option<u32>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawManifest {
    firmware: Vec<RawEntry>,
}

/// Why the manifest cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestError(pub String);

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the firmware list cannot be read: {}", self.0)
    }
}

impl std::error::Error for ManifestError {}

/// The manifest's `.apj` builds, from its JSON.
///
/// # Errors
/// When it is not the manifest.
pub fn read_manifest(json: &[u8]) -> Result<Vec<ManifestEntry>, ManifestError> {
    let raw: RawManifest =
        serde_json::from_slice(json).map_err(|e| ManifestError(e.to_string()))?;
    Ok(raw
        .firmware
        .into_iter()
        .filter(|e| e.format.as_deref() == Some("apj") && e.board_id.is_some())
        .filter_map(|e| {
            let mut fields = e.rest;
            fields.insert("board_id".into(), e.board_id.into());
            serde_json::from_value(serde_json::Value::Object(fields)).ok()
        })
        .collect())
}

/// The same from the gzipped file the manifest is served as.
///
/// # Errors
/// When it is not gzip, or not the manifest.
pub fn read_manifest_gz(bytes: &[u8]) -> Result<Vec<ManifestEntry>, ManifestError> {
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(512 * 1024 * 1024)
        .read_to_end(&mut json)
        .map_err(|e| ManifestError(e.to_string()))?;
    read_manifest(&json)
}

impl ManifestEntry {
    fn is(&self, vehicle_type: VehicleType) -> bool {
        let (kind, mav_type) = vehicle_type.manifest();
        self.vehicle_type == kind && self.mav_type.eq_ignore_ascii_case(mav_type)
    }

    #[must_use]
    pub fn channel(&self) -> Option<Channel> {
        Channel::of(&self.release_type)
    }

    /// "4.7.2", or "4.8.0-dev" where the text says more than the number.
    #[must_use]
    pub fn version_words(&self) -> String {
        let text = self.version_text.trim_start_matches(['V', 'v']);
        if text.is_empty() {
            self.version.clone()
        } else {
            text.to_owned()
        }
    }

    /// Lists this USB id ("0x2DAE/0x1016").
    fn has_usb(&self, vendor_id: u16, product_id: u16) -> bool {
        self.usb_ids.iter().any(|id| {
            let mut parts = id.split('/').map(|p| {
                u16::from_str_radix(
                    p.trim().trim_start_matches("0x").trim_start_matches("0X"),
                    16,
                )
                .ok()
            });
            parts.next().flatten() == Some(vendor_id) && parts.next().flatten() == Some(product_id)
        })
    }
}

/// What is known of a board before its bootloader says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardHint {
    /// From its bootloader, or the last install.
    pub board_id: Option<u32>,
    /// The USB id its firmware's serial port has.
    pub usb: Option<(u16, u16)>,
    /// A platform asked for by name ("CubeOrange-bdshot").
    pub platform: Option<String>,
}

/// One release a board can take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub channel: Channel,
    pub version: String,
    pub url: String,
    pub board_id: u32,
    pub platform: String,
    pub git_sha: Option<String>,
    pub image_size: Option<u64>,
}

/// The releases for a board and vehicle type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Releases {
    /// The platform chosen, and the others its board id or USB id also
    /// matches (`CubeOrange`, then `CubeOrange-bdshot`…).
    pub platform: Option<String>,
    pub platforms: Vec<String>,
    /// Its board id, when one platform is chosen.
    pub board_id: Option<u32>,
    /// What people call it ("Cube Orange"), when the manifest says.
    pub brand: Option<String>,
    /// Stable, beta and dev, then earlier stables, newest first.
    pub releases: Vec<Release>,
}

/// A platform's rank as the one to offer first: the board's own name rather
/// than a variant of it (`CubeOrange` before `CubeOrange-bdshot`).
fn plain_first(a: &str, b: &str) -> std::cmp::Ordering {
    a.contains('-')
        .cmp(&b.contains('-'))
        .then(a.len().cmp(&b.len()))
        .then(a.cmp(b))
}

/// Compares "4.6.2" and "4.10.0" as versions.
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u32> {
        v.split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

/// The releases of `vehicle_type` for the board the hint names: by board
/// id where known, else by USB id. With no hint that narrows it, none.
#[must_use]
pub fn releases(
    entries: &[ManifestEntry],
    hint: &BoardHint,
    vehicle_type: VehicleType,
) -> Releases {
    let matching: Vec<&ManifestEntry> = entries
        .iter()
        .filter(|e| e.is(vehicle_type) && e.channel().is_some())
        .filter(|e| match (hint.board_id, hint.usb) {
            (Some(id), _) => e.board_id == id,
            (None, Some((vid, pid))) => e.has_usb(vid, pid),
            (None, None) => hint.platform.as_deref() == Some(e.platform.as_str()),
        })
        .collect();
    let mut platforms: Vec<String> = matching.iter().map(|e| e.platform.clone()).collect();
    platforms.sort_by(|a, b| plain_first(a, b));
    platforms.dedup();
    let Some(platform) = hint
        .platform
        .clone()
        .filter(|p| platforms.contains(p))
        .or_else(|| platforms.first().cloned())
    else {
        return Releases::default();
    };
    let of_platform: Vec<&ManifestEntry> = matching
        .into_iter()
        .filter(|e| e.platform == platform)
        .collect();
    let mut releases: Vec<Release> = of_platform
        .iter()
        .filter_map(|e| {
            Some(Release {
                channel: e.channel()?,
                version: e.version_words(),
                url: e.url.clone(),
                board_id: e.board_id,
                platform: e.platform.clone(),
                git_sha: e.git_sha.clone(),
                image_size: e.image_size,
            })
        })
        .collect();
    releases.sort_by(|a, b| {
        a.channel
            .cmp(&b.channel)
            .then_with(|| version_order(&b.version, &a.version))
    });
    // An earlier stable is listed once, and not when it is the stable one.
    let stable = releases
        .iter()
        .find(|r| r.channel == Channel::Stable)
        .map(|r| r.version.clone());
    let mut seen = std::collections::BTreeSet::new();
    releases.retain(|r| {
        r.channel != Channel::Older
            || (Some(&r.version) != stable.as_ref() && seen.insert(r.version.clone()))
    });
    Releases {
        board_id: of_platform.first().map(|e| e.board_id),
        brand: of_platform.iter().find_map(|e| e.brand_name.clone()),
        platform: Some(platform),
        platforms,
        releases,
    }
}

/// The build to install on a board the bootloader identified: `wanted` is
/// "stable", "beta", "dev" or an earlier stable's version ("4.6.2").
#[must_use]
pub fn resolve(
    entries: &[ManifestEntry],
    board_id: u32,
    platform: Option<&str>,
    vehicle_type: VehicleType,
    wanted: &str,
) -> Option<Release> {
    let hint = BoardHint {
        board_id: Some(board_id),
        usb: None,
        platform: platform.map(str::to_owned),
    };
    let list = releases(entries, &hint, vehicle_type);
    let wanted = wanted.trim().trim_start_matches(['V', 'v']);
    let by_channel = match wanted.to_ascii_lowercase().as_str() {
        "stable" | "official" => Some(Channel::Stable),
        "beta" => Some(Channel::Beta),
        "dev" | "latest" => Some(Channel::Dev),
        _ => None,
    };
    list.releases.into_iter().find(|r| match by_channel {
        Some(channel) => r.channel == channel,
        None => r.version == wanted,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "deterministic tests")]

    use super::*;

    #[test]
    fn a_written_file_reads_back() {
        let image: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let file = read_file(&write_file(140, &image, "arducopter")).unwrap();
        assert_eq!(file.board_id, 140);
        assert_eq!(file.image, image);
        assert!(!file.px4);
        assert_eq!(file.version, None, "ArduPilot's 0.1 says nothing");
    }

    #[test]
    fn bad_files_say_why() {
        assert_eq!(read_file(b"hello"), Err(FileError::NotFirmware));
        assert_eq!(
            read_file(br#"{"image": "eJwDAAAAAAE="}"#),
            Err(FileError::Missing("board id"))
        );
        assert_eq!(
            read_file(br#"{"board_id": 9, "image": "not base64!"}"#),
            Err(FileError::BadImage)
        );
        let mut json: serde_json::Value =
            serde_json::from_slice(&write_file(9, &[1, 2, 3, 4], "x")).unwrap();
        json["image_size"] = 5.into();
        assert_eq!(
            read_file(json.to_string().as_bytes()),
            Err(FileError::SizeMismatch { said: 5, got: 4 })
        );
        json["image_size"] = 4.into();
        json["extf_image"] = "eJwDAAAAAAE=".into();
        assert_eq!(
            read_file(json.to_string().as_bytes()),
            Err(FileError::ExternalFlash)
        );
        // A board id in a string, as some files have.
        json["extf_image"] = serde_json::Value::Null;
        json["board_id"] = "1063".into();
        assert_eq!(
            read_file(json.to_string().as_bytes()).unwrap().board_id,
            1063
        );
    }

    #[test]
    fn vehicles_map_to_the_firmware_they_run() {
        assert_eq!(
            VehicleType::of_vehicle("Quadrotor"),
            Some(VehicleType::Copter)
        );
        assert_eq!(
            VehicleType::of_vehicle("VtolTiltrotor"),
            Some(VehicleType::Plane)
        );
        assert_eq!(
            VehicleType::of_vehicle("SurfaceBoat"),
            Some(VehicleType::Rover)
        );
        assert_eq!(
            VehicleType::of_vehicle("Helicopter"),
            Some(VehicleType::Heli)
        );
        assert_eq!(VehicleType::of_vehicle("Gcs"), None);
        assert_eq!(VehicleType::from_slug("Copter"), Some(VehicleType::Copter));
    }
}
