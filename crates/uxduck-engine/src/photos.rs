//! Photos from the video (ADR-0017 V19, S144): the picture a Camera tab
//! showed, made a JPEG by the browser, checked here and given EXIF with
//! where the camera's vehicle was, then written by the daemon into the
//! platform's pictures folder.

use base64::Engine as _;
use serde_json::Value;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{
    PHOTO_MAX, VideoPhoto as VideoPhotoDto, VideoPhotoParams, VideoPhotoShowParams,
    VideoPlaybackResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::domain::video::VideoPhoto;
use uxduck_core::events::{Actor, Change};
use uxduck_media::photo::{Civil, Tags, jpeg_size, with_exif};

use crate::rpc::handlers::to_value;
use crate::{Engine, upsert};

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
    }
}

/// A name the platform's file managers take: no slashes, colons or the
/// characters Windows refuses, and not empty.
pub(crate) fn file_safe(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean: String = clean.chars().take(60).collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if clean.is_empty() {
        "Camera".into()
    } else {
        clean.to_owned()
    }
}

/// Who a photo or recording is of, for its file name: the vehicle and the
/// camera, "Alpha - Front", so a vehicle's cameras are told apart
/// (maintainer, 2026-10-19); the camera alone when it is nobody's (S150).
pub(crate) fn file_who(vehicle: Option<&str>, camera: &str) -> String {
    match vehicle {
        Some(vehicle) => format!("{} - {}", file_safe(vehicle), file_safe(camera)),
        None => file_safe(camera),
    }
}

impl Engine {
    /// Where a vehicle is now, for a photo's EXIF: the autopilot's own
    /// estimate first, as the map draws it, and its height above sea level.
    fn where_vehicle_is(&self, vehicle_id: Option<&str>) -> Tags {
        let position = vehicle_id.and_then(|id| {
            let telemetry = self.telemetry.get(id)?;
            telemetry
                .positions
                .iter()
                .find(|p| p.source == "fused")
                .or_else(|| telemetry.positions.first())
        });
        Tags {
            latitude_deg: position.map(|p| p.latitude_deg),
            longitude_deg: position.map(|p| p.longitude_deg),
            altitude_m: position.and_then(|p| {
                p.altitudes
                    .iter()
                    .find(|a| a.frame == AltitudeFrame::Amsl)
                    .map(|a| a.meters)
            }),
            heading_deg: position.and_then(|p| p.heading_deg),
            ..Tags::default()
        }
    }

    fn video_photo(&mut self, params: &VideoPhotoParams) -> Result<VideoPhoto, ApiError> {
        let source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no video source {}", params.source_id),
                )
            })?;
        if self.video.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "Photos need UXDuck Desktop.",
            ));
        }
        // Base64 is four characters for three bytes.
        let too_big = || {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!("A photo can be at most {} MB.", PHOTO_MAX / (1024 * 1024)),
            )
        };
        if params.jpeg_base64.len() as u64 > PHOTO_MAX.div_ceil(3) * 4 {
            return Err(too_big());
        }
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(params.jpeg_base64.trim())
            .map_err(|_| ApiError::new(ErrorKind::InvalidValue, "The photo isn't base64."))?;
        if jpeg.len() as u64 > PHOTO_MAX {
            return Err(too_big());
        }
        let (width, height) = jpeg_size(&jpeg)
            .ok_or_else(|| ApiError::new(ErrorKind::InvalidValue, "The photo isn't a JPEG."))?;
        // An offset beyond ±14 hours is no time zone.
        let offset = params.utc_offset_minutes.clamp(-14 * 60, 14 * 60);

        let vehicle = source
            .vehicle_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id));
        let taken_at_us = self.clock.now_us();
        let tags = Tags {
            taken_at_us,
            utc_offset_minutes: offset,
            camera: source.name.clone(),
            width,
            height,
            ..self.where_vehicle_is(source.vehicle_id.as_deref())
        };
        let bytes = with_exif(&jpeg, &tags);
        let vehicle_name = vehicle.map(|v| v.name.clone());
        let stem = format!(
            "{} {}",
            Civil::at(taken_at_us, offset).file_stem(),
            file_who(vehicle_name.as_deref(), &source.name)
        );
        let port = self.video.port.as_mut().ok_or_else(|| {
            ApiError::new(ErrorKind::InvalidRequest, "Photos need UXDuck Desktop.")
        })?;
        let file_name = port
            .save_photo(&stem, &bytes)
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        // The vehicle's own camera takes its full-size photo too (V20).
        let camera_receipt_id = source
            .vehicle_id
            .as_deref()
            .filter(|_| source.vehicle_camera)
            .and_then(|vehicle_id| {
                self.tell_vehicle_camera(
                    vehicle_id,
                    crate::recordings::IMAGE_START_CAPTURE,
                    vec![0.0, 0.0, 1.0, 0.0],
                )
            });
        let photo = VideoPhoto {
            id: format!("vp{}", &self.random.token()[..16]),
            source_id: source.id.clone(),
            vehicle_id: source.vehicle_id.clone(),
            taken_at_us,
            file_name,
            bytes: bytes.len() as u64,
            width,
            height,
            latitude_deg: tags.latitude_deg,
            longitude_deg: tags.longitude_deg,
            altitude_m: tags.altitude_m,
            heading_deg: tags.heading_deg,
            camera_receipt_id,
            version: 1,
        };
        self.commit_as(
            actor(),
            Change::VideoPhotoUpserted {
                photo: photo.clone(),
            },
        );
        Ok(photo)
    }

    fn video_photo_show(
        &mut self,
        params: &VideoPhotoShowParams,
    ) -> Result<VideoPlaybackResult, ApiError> {
        let photo = self
            .state
            .video_photos
            .get(&params.photo_id)
            .ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no photo {}", params.photo_id))
            })?;
        let token = format!("vs{}", &self.random.token()[..32]);
        let Some(port) = self.video.port.as_mut() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "Photos need UXDuck Desktop.",
            ));
        };
        port.allow_photo(&token, &photo.id, &photo.file_name);
        Ok(VideoPlaybackResult {
            path: format!("/photos/{}.jpg?token={token}", photo.id),
            original_path: None,
        })
    }

    /// Shows a photo in the file manager: the pictures folder.
    pub(crate) fn reveal_photo(&mut self, photo_id: &str) -> Result<String, ApiError> {
        let photo =
            self.state.video_photos.get(photo_id).ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no photo {photo_id}"))
            })?;
        let file = photo.file_name.clone();
        let port = self.video.port.as_mut().ok_or_else(|| {
            ApiError::new(ErrorKind::InvalidRequest, "Photos need UXDuck Desktop.")
        })?;
        port.reveal_photo(&file)
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))
    }
}

pub(crate) fn photo(engine: &mut Engine, params: &VideoPhotoParams) -> Result<Value, ApiError> {
    to_value(&VideoPhotoDto::from(&engine.video_photo(params)?))
}

pub(crate) fn show(engine: &mut Engine, params: &VideoPhotoShowParams) -> Result<Value, ApiError> {
    to_value(&engine.video_photo_show(params)?)
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::VideoPhotoUpserted { photo } => vec![EntityChange {
            video_photo: Some(photo.into()),
            ..upsert(EntityType::VideoPhoto, &photo.id)
        }],
        Change::VideoPhotoRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::VideoPhoto, id)
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::file_safe;

    #[test]
    fn names_become_file_names() {
        assert_eq!(file_safe("Alpha"), "Alpha");
        assert_eq!(file_safe("udp://127.0.0.1:5600"), "udp 127.0.0.1 5600");
        assert_eq!(file_safe("  ..  "), "Camera");
        assert_eq!(file_safe("a\u{0}b\nc"), "a b c");
        // A vehicle's cameras told apart; a camera of nobody's by itself.
        assert_eq!(super::file_who(Some("Alpha"), "Front"), "Alpha - Front");
        assert_eq!(
            super::file_who(None, "MacBook Pro Camera"),
            "MacBook Pro Camera"
        );
    }
}
