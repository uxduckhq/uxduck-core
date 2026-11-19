//! A vehicle's cameras (ADR-0063 Y5, S404): what the MAVLink session
//! learns of each camera, kept so every window, the agent and the CLI see
//! the same ones. A camera is a MAVLink component that says what it is, or
//! one its autopilot speaks for. One that is gone is kept and marked so.
//! Each of its video streams is joined to the video source it arrives as.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use serde_json::Value;
use uxduck_contracts::dto::camera::{
    Camera as CameraDto, CameraCommandParams, CameraImagesParams, CameraImagesResult,
    CameraListResult,
};
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

/// How many photos one answer of `camera/images` carries when no number
/// is asked for, and the most it carries. They size an answer and cut
/// nothing: each answer says how many photos come before its first, and
/// the next call reads on from there. What bounds the list itself is
/// [`Camera::PHOTOS_KEPT`].
const PHOTOS_PAGE: usize = 50;
const PHOTOS_PAGE_MOST: usize = 500;

/// One camera's photos by their index in the camera's own list: the newest
/// [`Camera::PHOTOS_KEPT`] it described. They are a map, so that keeping
/// one costs the same wherever its index falls. A camera sets the order it
/// describes its photos in, and one that sends its indexes downward has
/// each placed before all the others: no kept photo is moved to make room
/// for it, on the engine's thread.
#[derive(Debug, Default)]
pub(crate) struct Photos {
    kept: BTreeMap<u32, CameraPhoto>,
    /// The lowest index dropped so far, to count each dropped photo once.
    dropped_from: Option<u32>,
}

impl Photos {
    /// Keeps a photo by its index, in place of one already kept with it,
    /// and drops the photo with the lowest index when that makes more
    /// than [`Camera::PHOTOS_KEPT`]. Whether one was dropped that was not
    /// counted as dropped before.
    fn place(&mut self, photo: CameraPhoto) -> bool {
        let index = photo.index;
        self.kept.insert(index, photo);
        if self.kept.len() <= Camera::PHOTOS_KEPT {
            return false;
        }
        let Some((gone, _)) = self.kept.pop_first() else {
            return false;
        };
        // Nothing of a dropped photo is kept to know it by. One described
        // below the whole list, at or above the lowest index dropped so
        // far, is taken as a dropped photo told again and is not counted
        // twice. One below that index was not dropped before.
        let again = gone == index && self.dropped_from.is_some_and(|from| gone >= from);
        self.dropped_from = Some(self.dropped_from.map_or(gone, |from| from.min(gone)));
        !again
    }

    /// The newest [`Camera::PHOTOS_SENT`] of them, oldest first. Taken
    /// from the end and turned round: an iterator told to give the last
    /// few in order walks every photo before them to find where they
    /// start, at each photo a camera describes.
    fn newest(&self) -> Vec<CameraPhoto> {
        let mut newest: Vec<CameraPhoto> = self
            .kept
            .values()
            .rev()
            .take(Camera::PHOTOS_SENT)
            .cloned()
            .collect();
        newest.reverse();
        newest
    }
}

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
            photos_listed: before.map_or(0, |b| b.photos_listed),
            photos_dropped: before.map_or(0, |b| b.photos_dropped),
            sources: Vec::new(),
            recording_not_asked: before.and_then(|b| b.recording_not_asked.clone()),
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
    /// with nothing said yet of its settings, its card or its photos. A
    /// daemon that starts has asked no camera to record yet.
    pub(crate) fn cameras_unheard(&mut self) {
        self.camera_photos.clear();
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
                    || camera.photos_listed > 0
                    || camera.photos_dropped > 0
                    || camera.recording_not_asked.is_some()
            })
            .cloned()
            .collect();
        for mut camera in heard {
            camera.present = false;
            camera.recording_not_asked = None;
            camera.settings = None;
            camera.capture = None;
            camera.storage = None;
            camera.photos.clear();
            camera.photos_listed = 0;
            camera.photos_dropped = 0;
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
            photos_not_asked: capture.images_not_asked,
            photos_awaited: capture.images_awaited,
            available_mib: capture.available_mib,
        });
        let shorter = matches!(
            (before.capture, camera.capture),
            (Some(was), Some(now)) if now.photo_count < was.photo_count
        );
        if shorter {
            camera.photos.clear();
            camera.photos_listed = 0;
            camera.photos_dropped = 0;
        }
        if camera == *before {
            return;
        }
        if shorter {
            self.camera_photos.remove(&camera.id);
        }
        camera.version += 1;
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
    }

    /// A photo a camera said it took (S405), kept with where it was taken,
    /// in the order of the camera's own list. The newest
    /// [`Camera::PHOTOS_KEPT`] are kept, for as long as this run lasts or
    /// until a format empties the list, and the camera says how many
    /// older ones are not. The newest [`Camera::PHOTOS_SENT`] go with the
    /// camera, with how many there are, and `camera/images` reads the
    /// rest.
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
        let all = self.camera_photos.entry(camera.id.clone()).or_default();
        if all.place(photo) {
            camera.photos_dropped = camera.photos_dropped.saturating_add(1);
        }
        camera.photos = all.newest();
        camera.photos_listed = u32::try_from(all.kept.len()).unwrap_or(u32::MAX);
        if self.state.cameras.get(&camera.id) == Some(&camera) {
            return;
        }
        camera.version += 1;
        let heard = camera.clone();
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
        // A controller's photo is said where its pilot is (S416).
        self.controller_photo_heard(&heard, image.index, image.ok);
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

    /// A camera toggle as what the camera needs, from what it last said
    /// (S416), as `ArmToggle` goes as Arm or Disarm: so it is sent and
    /// receipted as that. Cameras named by no component are all those of
    /// the vehicle that can do it, and must agree, except that a recording
    /// one stops them all. Refused while the camera has not said.
    pub(crate) fn camera_toggled(
        &self,
        vehicle_id: &str,
        command: Command,
    ) -> Result<Command, ApiError> {
        let Command::Camera {
            component_id,
            device_id,
            action,
        } = command
        else {
            return Ok(command);
        };
        if !action.toggles() {
            return Ok(Command::Camera {
                component_id,
                device_id,
                action,
            });
        }
        let name = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |vehicle| vehicle.name.clone());
        let refused = |why: &str| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!("{name}'s camera {why}, so nothing was sent."),
            )
        };
        let mut said = self
            .state
            .cameras
            .values()
            .filter(|camera| camera.vehicle_id == vehicle_id && camera.present)
            .filter(|camera| {
                component_id.is_none_or(|id| camera.component_id == id)
                    && (component_id.is_none() || camera.device_id == device_id)
            })
            .filter(|camera| camera.allows(&action))
            .map(|camera| camera.toggled(action.clone()));
        let Some(first) = said.next() else {
            let label = action.label();
            let mut asked = label.chars();
            let asked: String = asked
                .next()
                .map(|first| first.to_lowercase().chain(asked).collect())
                .unwrap_or_default();
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("None of {name}'s cameras can {asked}, so nothing was sent."),
            ));
        };
        let mut resolved = first.map_err(refused)?;
        for other in said {
            let other = other.map_err(refused)?;
            if other == resolved {
                continue;
            }
            if action == CameraAction::ToggleRecording {
                resolved = CameraAction::StopRecording;
                continue;
            }
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "{name}'s cameras are not set alike, so nothing was sent. Swap them one at a time in the Camera tab."
                ),
            ));
        }
        Ok(Command::Camera {
            component_id,
            device_id,
            action: resolved,
        })
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
        if let Change::CameraRemoved { id } = change {
            self.camera_photos.remove(id);
        }
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

/// `camera/images`, for anyone who may read: the page of a camera's photos
/// before the place named, or the newest.
pub(crate) fn photos(
    engine: &Engine,
    params: &CameraImagesParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let Some(camera) = engine.state.cameras.get(&params.camera_id) else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no camera {}", params.camera_id),
        ));
    };
    let kept = engine
        .camera_photos
        .get(&camera.id)
        .map(|photos| &photos.kept);
    let limit = params.limit.map_or(PHOTOS_PAGE, |limit| {
        usize::try_from(limit).map_or(PHOTOS_PAGE_MOST, |limit| limit.min(PHOTOS_PAGE_MOST))
    });
    let mut page: Vec<&CameraPhoto> = kept.map_or_else(Vec::new, |kept| {
        let before = params.before.map_or(Bound::Unbounded, Bound::Excluded);
        kept.range((Bound::Unbounded, before))
            .rev()
            .take(limit)
            .map(|(_, photo)| photo)
            .collect()
    });
    page.reverse();
    // How many come before the page, counted by walking them: at most
    // the photos kept of one camera, for a read someone asked for.
    let earlier = match (
        kept,
        page.first().map(|photo| photo.index).or(params.before),
    ) {
        (Some(kept), Some(from)) => kept.range(..from).count(),
        (Some(kept), None) => kept.len(),
        (None, _) => 0,
    };
    to_value(&CameraImagesResult {
        camera_id: camera.id.clone(),
        photos: page.into_iter().map(Into::into).collect(),
        total: u32::try_from(kept.map_or(0, BTreeMap::len)).unwrap_or(u32::MAX),
        earlier: u32::try_from(earlier).unwrap_or(u32::MAX),
        dropped: camera.photos_dropped,
        not_listed: camera.photos_not_listed(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Where each kept photo is in memory, by its index.
    fn places(photos: &Photos) -> BTreeMap<u32, usize> {
        photos
            .kept
            .iter()
            .map(|(index, photo)| (*index, std::ptr::from_ref(photo).addr()))
            .collect()
    }

    // A camera that sends its indexes downward has each photo placed
    // before all the others. Keeping one must not move the rest to make
    // room, or each photo costs more the longer the list is, on the
    // engine's thread. Held by how many kept photos are somewhere else in
    // memory after one more is kept, and not by a clock.
    #[test]
    fn a_photo_placed_before_all_the_others_moves_no_more_of_them_as_the_list_grows() {
        let mut photos = Photos::default();
        for index in (0..20_000_u32).rev() {
            let before = index.is_multiple_of(500).then(|| places(&photos));
            photos.place(CameraPhoto {
                index,
                taken_at_us: 0,
                place: None,
                ok: true,
                file: String::new(),
            });
            let Some(before) = before else {
                continue;
            };
            let after = places(&photos);
            let moved = before
                .iter()
                .filter(|(index, at)| after[*index] != **at)
                .count();
            // A map by index moves the photos of the nodes it touches: a
            // dozen at most in each of a few levels.
            assert!(
                moved <= 100,
                "{moved} of {} kept photos moved for one more",
                before.len()
            );
        }
        assert_eq!(places(&photos).len(), 20_000);
        let newest: Vec<u32> = photos.newest().iter().map(|photo| photo.index).collect();
        assert_eq!(newest, (19_980..20_000).collect::<Vec<_>>());
    }
}
