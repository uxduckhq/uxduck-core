//! A camera's KLV as its vehicle's telemetry (ADR-0017 V17, S41): where the
//! camera is, where it looks, and the corners of what it sees, as `camera.*`
//! items; and the platform it names as evidence for which vehicle a camera
//! belongs to (V5).

use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::video::CameraMetadata;
use uxduck_core::events::{Actor, Change};

use crate::Engine;

/// The camera component, as the items' component.
const CAMERA_COMPONENT: u8 = 100;

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
    }
}

type Number = (
    &'static str,
    &'static str,
    Unit,
    fn(&CameraMetadata) -> Option<f64>,
);

/// The numbers a packet may carry, as items.
const NUMBERS: [Number; 11] = [
    (
        "camera.latitude_deg",
        "Camera latitude",
        Unit::Degree,
        |m| m.sensor_latitude_deg,
    ),
    (
        "camera.longitude_deg",
        "Camera longitude",
        Unit::Degree,
        |m| m.sensor_longitude_deg,
    ),
    (
        "camera.altitude_msl_m",
        "Camera altitude",
        Unit::Meter,
        |m| m.sensor_altitude_m,
    ),
    ("camera.azimuth_deg", "Camera azimuth", Unit::Degree, |m| {
        m.sensor_azimuth_deg
    }),
    (
        "camera.elevation_deg",
        "Camera elevation",
        Unit::Degree,
        |m| m.sensor_elevation_deg,
    ),
    ("camera.hfov_deg", "Field of view", Unit::Degree, |m| {
        m.horizontal_fov_deg
    }),
    (
        "camera.vfov_deg",
        "Vertical field of view",
        Unit::Degree,
        |m| m.vertical_fov_deg,
    ),
    ("camera.slant_range_m", "Slant range", Unit::Meter, |m| {
        m.slant_range_m
    }),
    (
        "camera.frame_latitude_deg",
        "Looking at, latitude",
        Unit::Degree,
        |m| m.frame_latitude_deg,
    ),
    (
        "camera.frame_longitude_deg",
        "Looking at, longitude",
        Unit::Degree,
        |m| m.frame_longitude_deg,
    ),
    (
        "camera.frame_elevation_msl_m",
        "Looking at, elevation",
        Unit::Meter,
        |m| m.frame_elevation_m,
    ),
];

/// The items a packet gives: the numbers it has, and its texts.
#[must_use]
pub fn items(m: &CameraMetadata, time_us: u64) -> Vec<TelemetryItem> {
    let item =
        |key: String, label: String, unit: Unit, value: Option<f64>, text: Option<String>| {
            TelemetryItem {
                key,
                label,
                unit,
                value,
                text,
                source: "klv".into(),
                component_id: CAMERA_COMPONENT,
                time_us,
            }
        };
    let mut out = Vec::new();
    for (key, label, text) in [
        (
            "camera.platform",
            "Camera's platform",
            &m.platform_designation,
        ),
        ("camera.sensor", "Camera", &m.image_source),
    ] {
        if let Some(text) = text {
            out.push(item(
                key.into(),
                label.into(),
                Unit::None,
                None,
                Some(text.clone()),
            ));
        }
    }
    for (key, label, unit, get) in NUMBERS {
        if let Some(value) = get(m) {
            out.push(item(key.into(), label.into(), unit, Some(value), None));
        }
    }
    for (i, (lat, lon)) in m.corners.iter().flatten().enumerate() {
        let n = i + 1;
        for (axis, value) in [("latitude", lat), ("longitude", lon)] {
            out.push(item(
                format!("camera.corner{n}_{axis}_deg"),
                format!("Corner {n} {axis}"),
                Unit::Degree,
                Some(*value),
                None,
            ));
        }
    }
    out
}

impl Engine {
    /// A camera's KLV, a few times a second.
    pub fn video_klv(&mut self, source_id: &str, metadata: &CameraMetadata) {
        let Some(source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        let mut vehicle_id = source.vehicle_id.clone();
        // The platform it names, if that is a vehicle's name, says whose it
        // is when nothing better has.
        if vehicle_id.is_none()
            && !source.assigned_by_hand
            && let Some(platform) = metadata.platform_designation.as_deref()
            && let Some(vehicle) = self
                .state
                .vehicles
                .values()
                .find(|v| !v.dismissed && v.name.trim().eq_ignore_ascii_case(platform.trim()))
        {
            vehicle_id = Some(vehicle.id.clone());
            let mut claimed = source;
            claimed.vehicle_id.clone_from(&vehicle_id);
            claimed.version += 1;
            self.commit_as(actor(), Change::VideoSourceUpserted { source: claimed });
        }
        let Some(vehicle_id) = vehicle_id else {
            return;
        };
        let now = self.clock.now_us();
        self.telemetry_observed(VehicleTelemetry {
            vehicle_id,
            positions: Vec::new(),
            items: items(metadata, now),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_camera_item_says_its_unit_in_its_key() {
        let m = CameraMetadata {
            platform_designation: Some("p".into()),
            image_source: Some("s".into()),
            sensor_latitude_deg: Some(1.0),
            sensor_longitude_deg: Some(1.0),
            sensor_altitude_m: Some(1.0),
            platform_heading_deg: Some(1.0),
            horizontal_fov_deg: Some(1.0),
            vertical_fov_deg: Some(1.0),
            sensor_azimuth_deg: Some(1.0),
            sensor_elevation_deg: Some(1.0),
            slant_range_m: Some(1.0),
            frame_latitude_deg: Some(1.0),
            frame_longitude_deg: Some(1.0),
            frame_elevation_m: Some(1.0),
            corners: Some([(1.0, 1.0); 4]),
        };
        let all = items(&m, 0);
        assert_eq!(all.len(), 2 + 11 + 8);
        for item in all {
            assert!(
                uxduck_core::domain::telemetry::key_says_unit(&item.key, item.unit),
                "{}",
                item.key
            );
        }
    }
}
