//! A vehicle's cameras (ADR-0063 Y5, S404): what the MAVLink session
//! learns of each camera, kept so every window, the agent and the CLI see
//! the same ones. A camera is a MAVLink component that says what it is, or
//! one its autopilot speaks for. One that is gone is kept and marked so.
//! Each of its video streams is joined to the video source it arrives as.

use std::collections::BTreeSet;

use serde_json::Value;
use uxduck_contracts::dto::camera::{Camera as CameraDto, CameraCommandParams, CameraListResult};
use uxduck_contracts::dto::command::{Command as CommandDto, CommandSendParams};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::camera::{
    Camera, CameraAction, CameraCapture, CameraInformation, CameraMode, CameraPhoto,
    CameraPhotoPlace, CameraSettings, CameraStorage, CameraStorageStatus, CameraStream,
};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::Change;
use uxduck_mavlink::camera as seen;
use uxduck_mavlink::camera_control as control;

use crate::rpc::handlers::to_value;
use crate::vehicles::data_plane;
use crate::{Engine, upsert};

fn information(said: seen::Information) -> CameraInformation {
    CameraInformation {
        vendor: said.vendor,
        model: said.model,
        firmware_version: said.firmware_version,
        focal_length_mm: said.focal_length_mm,
        sensor_size_mm: said.sensor_size_mm,
        resolution_px: said.resolution_px,
        lens_id: said.lens_id,
        flags: said.flags,
        definition_version: said.definition_version,
        definition_uri: said.definition_uri,
        gimbal_device_id: said.gimbal_device_id,
    }
}

fn stream(said: seen::Stream) -> CameraStream {
    CameraStream {
        stream_id: said.stream_id,
        name: said.name,
        uri: said.uri,
        kind: said.kind,
        encoding: said.encoding,
        flags: said.flags,
        frame_rate_hz: said.frame_rate_hz,
        width: said.width,
        height: said.height,
        bitrate: said.bitrate,
        rotation: said.rotation,
        hfov: said.hfov,
        source_id: None,
    }
}

impl Engine {
    /// What the data plane now knows of a camera: heard, identified, with
    /// other streams, gone or back. A camera of a vehicle that is not
    /// known, such as one on a link just removed, is nobody's.
    pub fn camera_observed(&mut self, observed: seen::Observation) {
        let Some(vehicle_id) = self.vehicle(&observed.vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let id = Camera::id_of(&vehicle_id, observed.component_id, observed.device_id);
        let before = self.state.cameras.get(&id);
        let mut camera = Camera {
            id,
            vehicle_id,
            component_id: observed.component_id,
            device_id: observed.device_id,
            present: observed.present,
            information: observed.information.map(information),
            streams: observed.streams.into_iter().map(stream).collect(),
            // What it said of itself stands until it says otherwise (S405).
            settings: before.and_then(|b| b.settings),
            capture: before.and_then(|b| b.capture),
            storage: before.and_then(|b| b.storage.clone()),
            photos: before.map(|b| b.photos.clone()).unwrap_or_default(),
            sources: Vec::new(),
            version: before.map_or(0, |b| b.version),
        };
        // The sensors it has said it has, with those it said before
        // (ADR-0063 Y5).
        camera.sources = camera.sources_said(before);
        self.join_streams(&mut camera);
        if before == Some(&camera) {
            return;
        }
        camera.version += 1;
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
    }

    /// A camera that was listed is no camera of its own: its autopilot was
    /// passing on a camera component's answer.
    pub fn camera_removed(&mut self, vehicle_id: &str, component_id: u8, device_id: Option<u8>) {
        let vehicle_id = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |v| v.id.as_str());
        let id = Camera::id_of(vehicle_id, component_id, device_id);
        if self.state.cameras.contains_key(&id) {
            self.commit_as(data_plane(), Change::CameraRemoved { id });
        }
    }

    /// Nothing is heard until the data plane reports again: every camera
    /// starts a daemon run gone, as every vehicle starts it offline, and
    /// with nothing said yet of its settings, its card or its photos.
    pub(crate) fn cameras_unheard(&mut self) {
        let heard: Vec<Camera> = self
            .state
            .cameras
            .values()
            .filter(|camera| {
                camera.present
                    || camera.settings.is_some()
                    || camera.capture.is_some()
                    || camera.storage.is_some()
                    || !camera.photos.is_empty()
            })
            .cloned()
            .collect();
        for mut camera in heard {
            camera.present = false;
            camera.settings = None;
            camera.capture = None;
            camera.storage = None;
            camera.photos.clear();
            camera.version += 1;
            self.commit_as(data_plane(), Change::CameraUpserted { camera });
        }
    }

    /// A vehicle's camera by its address, under any id the vehicle has.
    pub(crate) fn camera_at(
        &self,
        vehicle_id: &str,
        component_id: u8,
        device_id: Option<u8>,
    ) -> Option<&Camera> {
        let vehicle_id = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |vehicle| vehicle.id.as_str());
        self.state
            .cameras
            .get(&Camera::id_of(vehicle_id, component_id, device_id))
    }

    /// What a camera last said of its settings, its card and what it is
    /// capturing (S405). A list of photos that got shorter was emptied, as
    /// a format does, so the photos kept from it go too.
    pub fn camera_state(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        device_id: Option<u8>,
        said: &control::State,
    ) {
        let Some(before) = self.camera_at(vehicle_id, component_id, device_id) else {
            return;
        };
        let mut camera = before.clone();
        camera.settings = said.settings.map(|settings| CameraSettings {
            mode: CameraMode::of(settings.mode),
            zoom_pct: settings.zoom_pct,
            focus_pct: settings.focus_pct,
        });
        camera.storage = said.storage.as_ref().map(|storage| CameraStorage {
            storage_id: storage.storage_id,
            count: storage.storage_count,
            status: CameraStorageStatus::of(storage.status),
            total_mib: storage.total_mib,
            used_mib: storage.used_mib,
            available_mib: storage.available_mib,
            name: storage.name.clone(),
        });
        camera.capture = said.capture.map(|capture| CameraCapture {
            interval_s: capture.interval_s,
            recording: capture.recording,
            recording_since_us: capture.recording_since_us,
            photo_count: capture.image_count,
            photos_lost: capture.images_lost,
            available_mib: capture.available_mib,
        });
        let shorter = matches!(
            (before.capture, camera.capture),
            (Some(was), Some(now)) if now.photo_count < was.photo_count
        );
        if shorter {
            camera.photos.clear();
        }
        if camera == *before {
            return;
        }
        camera.version += 1;
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
    }

    /// A photo a camera said it took (S405), kept with where it was taken:
    /// the latest [`Camera::PHOTOS_KEPT`] of them, in the order of the
    /// camera's own list.
    pub fn camera_image(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        device_id: Option<u8>,
        image: control::Image,
    ) {
        let Some(before) = self.camera_at(vehicle_id, component_id, device_id) else {
            return;
        };
        let mut camera = before.clone();
        let photo = CameraPhoto {
            index: image.index,
            // A clock that was never set counts from when the camera
            // started, so when UXDuck heard of the photo is nearer.
            taken_at_us: if image.time_utc_us >= CLOCK_SET_US {
                image.time_utc_us
            } else {
                image.heard_us
            },
            place: image.place.map(
                |(latitude_deg, longitude_deg, altitude_amsl_m, relative_altitude_m)| {
                    CameraPhotoPlace {
                        latitude_deg,
                        longitude_deg,
                        altitude_amsl_m,
                        relative_altitude_m,
                    }
                },
            ),
            ok: image.ok,
            file: image.file_url,
        };
        match camera
            .photos
            .binary_search_by_key(&photo.index, |kept| kept.index)
        {
            Ok(at) => camera.photos[at] = photo,
            Err(at) => camera.photos.insert(at, photo),
        }
        let over = camera.photos.len().saturating_sub(Camera::PHOTOS_KEPT);
        camera.photos.drain(..over);
        if camera == *before {
            return;
        }
        camera.version += 1;
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
    }

    /// A camera command that names no camera, for the vehicle's only
    /// camera that says it can do it. With none or several it stays for
    /// every camera, through the autopilot, as V20's commands always went.
    pub(crate) fn camera_addressed(&self, vehicle_id: &str, command: Command) -> Command {
        let Command::Camera {
            component_id: None,
            device_id: None,
            action,
        } = command
        else {
            return command;
        };
        let mut able = self.state.cameras.values().filter(|camera| {
            camera.vehicle_id == vehicle_id
                && camera.present
                && camera.information.is_some()
                && camera.allows(&action)
        });
        let (component_id, device_id) = match (able.next(), able.next()) {
            (Some(only), None) => (Some(only.component_id), only.device_id),
            _ => (None, None),
        };
        Command::Camera {
            component_id,
            device_id,
            action,
        }
    }

    /// The camera a video source's picture comes from, when it has said
    /// what it is and that it can do this: where the source's own photo
    /// goes (V20).
    pub(crate) fn camera_of_source(
        &self,
        source_id: &str,
        action: &CameraAction,
    ) -> Option<(u8, Option<u8>)> {
        self.state
            .cameras
            .values()
            .filter(|camera| camera.present && camera.information.is_some())
            .filter(|camera| camera.allows(action))
            .find(|camera| {
                camera
                    .streams
                    .iter()
                    .any(|stream| stream.source_id.as_deref() == Some(source_id))
            })
            .map(|camera| (camera.component_id, camera.device_id))
    }

    /// A camera's refusal in words that say why, where UXDuck knows it
    /// from what the camera last said of itself (S405): a full card, or a
    /// format asked of a camera that is capturing.
    pub(crate) fn camera_refusal(&self, receipt: &Receipt, why: &str) -> Option<String> {
        let Command::Camera {
            component_id,
            device_id,
            action,
        } = &receipt.command
        else {
            return None;
        };
        let cameras: Vec<&Camera> = self
            .state
            .cameras
            .values()
            .filter(|camera| camera.vehicle_id == receipt.vehicle_id)
            .filter(|camera| {
                component_id.is_none_or(|id| camera.component_id == id)
                    && (device_id.is_none() || camera.device_id == *device_id)
            })
            .collect();
        let why = why.trim_end_matches('.');
        if action.captures()
            && let Some(full) = cameras.iter().find(|camera| camera.card_full())
        {
            // In megabytes, as the Camera tab counts a card.
            let left = (full.available_mib().unwrap_or_default().max(0.0) * 1.048_576).round();
            return Some(format!(
                "The camera's card is full, with {left} MB left. Format the card or put in another, then try again. The camera said: {why}."
            ));
        }
        let capturing = cameras.iter().any(|camera| {
            camera
                .capture
                .is_some_and(|capture| capture.recording || capture.interval_s.is_some())
        });
        (action.erases() && capturing).then(|| {
            format!(
                "The camera is recording or taking photos at an interval. Stop that, then format its card. The camera said: {why}."
            )
        })
    }

    /// The video source each of a camera's streams arrives as: the one
    /// fetched from the stream's address, or the one arriving on the UDP
    /// port it names, of the same vehicle.
    fn join_streams(&self, camera: &mut Camera) {
        for stream in &mut camera.streams {
            let port = stream.udp_port();
            let address = stream.uri.trim();
            stream.source_id = self
                .state
                .video_sources
                .values()
                .filter(|source| {
                    source
                        .vehicle_id
                        .as_deref()
                        .and_then(|id| self.vehicle(id))
                        .is_some_and(|vehicle| vehicle.id == camera.vehicle_id)
                })
                .find(|source| match port {
                    Some(port) => {
                        source.port == port
                            && source.url.is_none()
                            && source.capture.is_none()
                            && source.file.is_none()
                    }
                    None => source.url.as_deref() == Some(address),
                })
                .map(|source| source.id.clone());
        }
    }

    /// Whether anything on a vehicle is known to take images or record
    /// video. Until one of its cameras says what it is, it may: the
    /// vehicle is told to shoot as before (V20).
    pub(crate) fn vehicle_captures(&self, vehicle_id: &str) -> bool {
        let vehicle_id = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |vehicle| vehicle.id.as_str());
        let mut identified = self
            .state
            .cameras
            .values()
            .filter(|camera| camera.vehicle_id == vehicle_id)
            .filter_map(|camera| camera.information.as_ref())
            .peekable();
        identified.peek().is_none() || identified.any(CameraInformation::captures)
    }

    /// What follows a change, for the cameras: they go with their vehicle
    /// and are gone while it is offline, their streams are joined to the
    /// video sources as those come and go, and a stream of a vehicle with
    /// nothing that takes images or records is not told to (V20), until
    /// someone says so by hand.
    pub(crate) fn cameras_after(&mut self, change: &Change) {
        if self.state.cameras.is_empty() {
            return;
        }
        match change {
            Change::VehicleRemoved { id } => {
                let gone: Vec<String> = self
                    .state
                    .cameras
                    .values()
                    .filter(|camera| camera.vehicle_id == *id)
                    .map(|camera| camera.id.clone())
                    .collect();
                for id in gone {
                    self.commit_as(data_plane(), Change::CameraRemoved { id });
                }
            }
            Change::VehicleUpserted { vehicle } if vehicle.presence == Presence::Offline => {
                let heard: Vec<Camera> = self
                    .state
                    .cameras
                    .values()
                    .filter(|camera| camera.vehicle_id == vehicle.id && camera.present)
                    .cloned()
                    .collect();
                for mut camera in heard {
                    camera.present = false;
                    camera.version += 1;
                    self.commit_as(data_plane(), Change::CameraUpserted { camera });
                }
            }
            Change::VideoSourceUpserted { .. } | Change::VideoSourceRemoved { .. } => {
                let cameras: Vec<Camera> = self.state.cameras.values().cloned().collect();
                for before in cameras {
                    let mut camera = before.clone();
                    self.join_streams(&mut camera);
                    if camera != before {
                        camera.version += 1;
                        self.commit_as(data_plane(), Change::CameraUpserted { camera });
                    }
                }
            }
            // What one camera says changes what the vehicle can do, for the
            // video of every camera on it.
            Change::CameraUpserted { camera } => {
                let captures = self.vehicle_captures(&camera.vehicle_id);
                let joined: BTreeSet<&String> = self
                    .state
                    .cameras
                    .values()
                    .filter(|other| other.vehicle_id == camera.vehicle_id)
                    .flat_map(|other| &other.streams)
                    .filter_map(|stream| stream.source_id.as_ref())
                    .collect();
                let sources: Vec<_> = joined
                    .into_iter()
                    .filter_map(|id| self.state.video_sources.get(id))
                    .filter(|source| {
                        !source.vehicle_camera_by_hand && source.vehicle_camera != captures
                    })
                    .cloned()
                    .collect();
                for mut source in sources {
                    source.vehicle_camera = captures;
                    source.version += 1;
                    self.commit_as(data_plane(), Change::VideoSourceUpserted { source });
                }
            }
            _ => {}
        }
    }
}

/// A camera's clock reads later than this once it was set: 2017.
const CLOCK_SET_US: u64 = 1_500_000_000_000_000;

/// `camera/command` as the `command/send` it is (S405): a camera command
/// for the camera's vehicle, so its checks, its approval and its receipt
/// are a command's.
pub(crate) fn as_command(
    engine: &Engine,
    params: &CameraCommandParams,
) -> Result<ClientRequest, ApiError> {
    let camera = engine.state.cameras.get(&params.camera_id).ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no camera {}", params.camera_id),
        )
    })?;
    Ok(ClientRequest::CommandSend(CommandSendParams {
        vehicle_id: camera.vehicle_id.clone(),
        command: CommandDto::Camera {
            component_id: Some(camera.component_id),
            device_id: camera.device_id,
            action: params.action.clone(),
        },
        confirm: params.confirm,
        force: false,
        override_preflight: Vec::new(),
    }))
}

/// Every camera of every vehicle, by id.
pub(crate) fn listed(engine: &Engine) -> Vec<CameraDto> {
    engine.state.cameras.values().map(Into::into).collect()
}

/// `camera/list`, for anyone who may read.
pub(crate) fn list(engine: &Engine, granted: &[Capability]) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    to_value(&CameraListResult {
        cameras: listed(engine),
    })
}

/// The wire form of a camera change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::CameraUpserted { camera } => vec![EntityChange {
            camera: Some(camera.into()),
            ..upsert(EntityType::Camera, &camera.id)
        }],
        Change::CameraRemoved { id } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Camera, id)
        }],
        _ => Vec::new(),
    }
}
