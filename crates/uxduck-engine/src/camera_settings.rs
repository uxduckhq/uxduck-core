//! A camera's own settings (ADR-0063 Y5, S411): the ones its definition
//! file names. The file is at the address the camera gives in what it says
//! of itself: on the vehicle, read over MAVLink FTP without asking, or at an
//! `http` or `https` address, fetched only once a person agrees. A file
//! with a version is kept by this computer and not fetched again. What the
//! file names is read from the camera as extended parameters, and written
//! as a camera command, with a command's checks, approval and receipt.
//!
//! None of this is in the camera's own record, which every window loads at
//! startup: `camera/settings` answers it, and the camera's `version` moves
//! when it changes. It lasts a daemon run, as who has a gimbal does.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::camera::{
    Camera as CameraDto, CameraDefinitionFetchParams, CameraDefinitionStatus, CameraSetting,
    CameraSettingOption, CameraSettingsParams, CameraSettingsResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::camera::{Camera, CameraAction, CameraMode, CameraSettingKind};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::events::Change;
use uxduck_mavlink::camera_definition::{self, Definition};
use uxduck_mavlink::camera_file;
use uxduck_mavlink::camera_params::Value;

use crate::Engine;
use crate::rpc::handlers::to_value;
use crate::runtime::TransferOrder;
use crate::vehicles::data_plane;

/// The setting a camera's mode is, by the protocol's own name for it.
const MODE: &str = "CAM_MODE";

/// Where UXDuck is with each camera's definition file and its values, by
/// camera id.
#[derive(Default)]
pub(crate) struct Book(BTreeMap<String, Entry>);

struct Entry {
    /// The address and version this is for, as the camera gave them.
    uri: String,
    version: u16,
    /// The camera's maker and model with the address: a camera that says
    /// it is another has another file.
    key: String,
    file: File,
    /// The file is this computer's own copy.
    kept: bool,
    /// The camera was heard when this was last looked at.
    heard: bool,
    /// The mode it last said it is in.
    mode: Option<CameraMode>,
    values: BTreeMap<String, Value>,
    reading: bool,
    read_failure: Option<String>,
}

enum File {
    /// At an address a person has not agreed to fetch from.
    Ask,
    Fetching,
    Failed(String),
    Refused(String),
    Loaded(Definition),
}

impl Entry {
    /// A setting's value as a number, as the camera is set now. Its mode
    /// is what it last said its mode is, when it has.
    fn value(&self, name: &str) -> Option<f64> {
        if name == MODE
            && let Some(mode) = self.mode
        {
            return Some(f64::from(mode.id()));
        }
        self.values.get(name).and_then(Value::number)
    }
}

/// The name this computer keeps a camera's definition file under: its
/// maker, its model and the address, so two cameras that name the same
/// path do not share a file.
fn kept_as(camera: &Camera, uri: &str) -> String {
    let said = camera.information.as_ref();
    let mut hash = Sha256::new();
    for part in [
        said.map_or("", |said| said.vendor.as_str()),
        said.map_or("", |said| said.model.as_str()),
        uri,
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", uxduck_core::encoding::Hex(&hash.finalize()[..16]))
}

fn http(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

impl Engine {
    /// What follows a change, for cameras' own settings: a camera that
    /// names a definition file has it fetched, one that is back is asked
    /// for its values again, and one that is removed is forgotten.
    pub(crate) fn camera_settings_after(&mut self, change: &Change) {
        match change {
            Change::CameraRemoved { id } => {
                self.camera_settings.0.remove(id);
            }
            Change::CameraUpserted { camera } => self.camera_definition_wanted(camera),
            _ => {}
        }
    }

    /// Nothing here moves the camera's version: this runs inside the
    /// change that just moved it.
    fn camera_definition_wanted(&mut self, camera: &Camera) {
        // A camera its autopilot speaks for has no component of its own
        // to ask, so it is as one that names no file.
        let named = camera
            .information
            .as_ref()
            .map(|said| (said.definition_uri.trim(), said.definition_version))
            .filter(|(uri, _)| !uri.is_empty() && camera.device_id.is_none());
        let Some((uri, version)) = named else {
            self.camera_settings.0.remove(&camera.id);
            return;
        };
        let mode = camera.settings.and_then(|settings| settings.mode);
        let key = kept_as(camera, uri);
        let known = self
            .camera_settings
            .0
            .get_mut(&camera.id)
            .filter(|entry| entry.key == key && entry.version == version);
        let Some(entry) = known else {
            self.camera_settings.0.remove(&camera.id);
            if camera.present {
                self.camera_definition_start(camera, uri, version);
            }
            return;
        };
        let back = camera.present && !entry.heard;
        let mode_changed = entry.mode.is_some() && mode.is_some() && entry.mode != mode;
        entry.heard = camera.present;
        if mode.is_some() {
            entry.mode = mode;
        }
        if !camera.present {
            entry.reading = false;
            return;
        }
        match &entry.file {
            File::Loaded(_) if back => self.camera_settings_read(&camera.id, None),
            // What its mode decides may have changed with it.
            File::Loaded(definition) if mode_changed => {
                let mut again = vec![MODE.to_owned()];
                again.extend(
                    definition
                        .setting(MODE)
                        .map(|setting| setting.updates.clone())
                        .unwrap_or_default(),
                );
                self.camera_settings_read(&camera.id, Some(again));
            }
            // A read that the camera's going away cut short.
            File::Fetching | File::Failed(_) if back && camera_file::is_ftp(uri) => {
                let file = self.camera_definition_fetch(camera, uri, false);
                if let Some(entry) = self.camera_settings.0.get_mut(&camera.id) {
                    entry.file = file;
                }
            }
            _ => {}
        }
    }

    fn camera_definition_start(&mut self, camera: &Camera, uri: &str, version: u16) {
        let mut entry = Entry {
            uri: uri.to_owned(),
            version,
            key: kept_as(camera, uri),
            file: File::Fetching,
            kept: false,
            heard: true,
            mode: camera.settings.and_then(|settings| settings.mode),
            values: BTreeMap::new(),
            reading: false,
            read_failure: None,
        };
        // This computer's own copy, when it kept one of this version. A
        // file whose version is 0 is never kept.
        let kept = (version != 0)
            .then(|| self.system.camera_definition_kept(&entry.key, version))
            .flatten()
            .and_then(|bytes| camera_definition::parse(&bytes).ok());
        if let Some(definition) = kept {
            entry.file = File::Loaded(definition);
            entry.kept = true;
            self.camera_settings.0.insert(camera.id.clone(), entry);
            self.camera_settings_read(&camera.id, None);
            return;
        }
        entry.file = self.camera_definition_fetch(camera, uri, false);
        self.camera_settings.0.insert(camera.id.clone(), entry);
    }

    /// Starts fetching a camera's definition file, and says where that
    /// leaves it. A file on the vehicle is read without asking. One at an
    /// address is fetched only when a person `agreed`.
    fn camera_definition_fetch(&mut self, camera: &Camera, uri: &str, agreed: bool) -> File {
        if camera_file::is_ftp(uri) {
            let Some((from, path)) = camera_file::ftp_address(uri) else {
                return File::Failed(
                    "The camera names a MAVLink FTP address that UXDuck cannot read.".into(),
                );
            };
            let blocked = self
                .vehicle(&camera.vehicle_id)
                .and_then(|vehicle| self.not_writable(&vehicle.link_id));
            if let Some(why) = blocked {
                return File::Failed(format!("The file was not read: {why}."));
            }
            self.links.transfer(TransferOrder::CameraDefinition {
                vehicle_id: camera.vehicle_id.clone(),
                component_id: camera.component_id,
                from,
                path,
            });
            return File::Fetching;
        }
        if !http(uri) {
            return File::Failed(
                "UXDuck reads a camera's definition file from the vehicle over MAVLink FTP, or from an http or https address. This camera names another kind of address.".into(),
            );
        }
        if !agreed {
            return File::Ask;
        }
        match self.system.camera_definition_fetch(&camera.id, uri) {
            Ok(()) => File::Fetching,
            Err(why) => File::Failed(why),
        }
    }

    /// A definition file's read over MAVLink FTP ended: its bytes, or why
    /// there are none.
    pub fn camera_definition_read(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        result: Result<Vec<u8>, String>,
    ) {
        let Some(id) = self
            .camera_at(vehicle_id, component_id, None)
            .map(|camera| camera.id.clone())
        else {
            return;
        };
        self.camera_definition_arrived(&id, None, result);
    }

    /// A definition file's fetch from an address ended: its bytes, or why
    /// there are none, as a sentence.
    pub fn camera_definition_fetched(
        &mut self,
        camera_id: &str,
        uri: &str,
        result: Result<Vec<u8>, String>,
    ) {
        self.camera_definition_arrived(camera_id, Some(uri), result);
    }

    fn camera_definition_arrived(
        &mut self,
        camera_id: &str,
        fetched_from: Option<&str>,
        result: Result<Vec<u8>, String>,
    ) {
        let Some(entry) = self.camera_settings.0.get_mut(camera_id) else {
            return;
        };
        // An answer nobody waits for, or one for an address the camera no
        // longer names.
        let awaited = matches!(entry.file, File::Fetching)
            && match fetched_from {
                Some(uri) => uri == entry.uri,
                None => camera_file::is_ftp(&entry.uri),
            };
        if !awaited {
            return;
        }
        let mut loaded = false;
        entry.file = match result {
            Err(why) => File::Failed(why),
            Ok(bytes) => match camera_definition::parse(&bytes) {
                Err(why) => File::Refused(why),
                Ok(definition) => {
                    loaded = true;
                    entry.kept = false;
                    entry.values.clear();
                    if entry.version != 0 {
                        self.system
                            .camera_definition_keep(&entry.key, entry.version, &bytes);
                    }
                    File::Loaded(definition)
                }
            },
        };
        if loaded {
            self.camera_settings_read(camera_id, None);
        }
        self.camera_settings_moved(camera_id);
    }

    /// Asks a camera for the values of its settings: all of them, or
    /// those named.
    fn camera_settings_read(&mut self, camera_id: &str, names: Option<Vec<String>>) {
        let Some(camera) = self.state.cameras.get(camera_id) else {
            return;
        };
        let (vehicle_id, component_id) = (camera.vehicle_id.clone(), camera.component_id);
        let blocked = self
            .vehicle(&vehicle_id)
            .and_then(|vehicle| self.not_writable(&vehicle.link_id));
        let Some(entry) = self.camera_settings.0.get_mut(camera_id) else {
            return;
        };
        if let Some(why) = blocked {
            entry.reading = false;
            entry.read_failure = Some(format!("Its values were not read: {why}."));
            return;
        }
        entry.reading = true;
        entry.read_failure = None;
        self.links.transfer(TransferOrder::CameraSettings {
            vehicle_id,
            component_id,
            names,
        });
    }

    /// A camera gave values of its settings, and with `read` a read of
    /// them ended. Only the settings its definition file names are kept.
    pub fn camera_settings_said(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        values: Vec<(String, Value)>,
        read: Option<Result<(), String>>,
    ) {
        let Some(id) = self
            .camera_at(vehicle_id, component_id, None)
            .map(|camera| camera.id.clone())
        else {
            return;
        };
        let Some(entry) = self.camera_settings.0.get_mut(&id) else {
            return;
        };
        let File::Loaded(definition) = &entry.file else {
            return;
        };
        let mut changed = false;
        for (name, value) in values {
            if definition.setting(&name).is_some() && entry.values.get(&name) != Some(&value) {
                entry.values.insert(name, value);
                changed = true;
            }
        }
        let ended = read.is_some();
        if let Some(read) = read {
            entry.reading = false;
            entry.read_failure = read.err();
        }
        // The values of a read in progress are told once, at its end.
        if ended || (changed && !entry.reading) {
            self.camera_settings_moved(&id);
        }
    }

    /// The camera's settings changed for everyone looking: its version
    /// moves, and they read `camera/settings` again.
    fn camera_settings_moved(&mut self, camera_id: &str) {
        let Some(mut camera) = self.state.cameras.get(camera_id).cloned() else {
            return;
        };
        camera.version += 1;
        self.commit_as(data_plane(), Change::CameraUpserted { camera });
    }

    /// A command that writes one of a camera's own settings, with the
    /// setting's type from the camera's definition file.
    ///
    /// # Errors
    /// Why it is not sent: no one camera is named, its file is not read,
    /// the file names no such setting, the camera sets it itself, or its
    /// type cannot hold the value.
    pub(crate) fn camera_setting_typed(
        &self,
        vehicle_id: &str,
        command: Command,
    ) -> Result<Command, ApiError> {
        let Command::Camera {
            component_id,
            device_id,
            action: CameraAction::SetSetting { name, value, .. },
        } = &command
        else {
            return Ok(command);
        };
        let refuse = |why: String| Err(ApiError::new(ErrorKind::InvalidValue, why));
        let camera = component_id.and_then(|id| self.camera_at(vehicle_id, id, *device_id));
        let Some(camera) = camera else {
            return refuse(
                "name the camera whose setting this is: the vehicle has none, or more than one"
                    .into(),
            );
        };
        let Some(File::Loaded(definition)) =
            self.camera_settings.0.get(&camera.id).map(|e| &e.file)
        else {
            return refuse(
                "UXDuck has not read this camera's definition file, which names its settings"
                    .into(),
            );
        };
        let Some(setting) = definition.setting(name) else {
            return refuse(format!(
                "this camera's definition file names no setting {name}"
            ));
        };
        if setting.read_only {
            return refuse(format!("the camera sets {name} itself: it is read only"));
        }
        if let Some(why) = setting.kind.refuses(*value) {
            return refuse(why.into());
        }
        Ok(Command::Camera {
            component_id: *component_id,
            device_id: *device_id,
            action: CameraAction::SetSetting {
                name: name.clone(),
                value: *value,
                kind: Some(setting.kind),
            },
        })
    }

    /// A command ended: what that means for a gimbal (S406) and for a
    /// camera's own settings.
    pub(crate) fn payload_command_done(&mut self, receipt: &Receipt) {
        self.gimbal_command_done(receipt);
        self.camera_settings_after_command(receipt);
    }

    /// What a camera command that went through means for the camera's
    /// own settings: the one written is read back with those it says to
    /// update, and after a reset all of them are.
    fn camera_settings_after_command(&mut self, receipt: &Receipt) {
        if receipt.status != ReceiptStatus::Accepted {
            return;
        }
        let Command::Camera {
            component_id: Some(component_id),
            device_id: None,
            action,
        } = &receipt.command
        else {
            return;
        };
        let Some(id) = self
            .camera_at(&receipt.vehicle_id, *component_id, None)
            .map(|camera| camera.id.clone())
        else {
            return;
        };
        let Some(File::Loaded(definition)) = self.camera_settings.0.get(&id).map(|e| &e.file)
        else {
            return;
        };
        let names = match action {
            CameraAction::SetSetting { name, .. } => {
                let mut names = vec![name.clone()];
                names.extend(
                    definition
                        .setting(name)
                        .map(|setting| setting.updates.clone())
                        .unwrap_or_default(),
                );
                Some(names)
            }
            CameraAction::Reset => None,
            _ => return,
        };
        self.camera_settings_read(&id, names);
    }

    /// `camera/settings`'s answer for a camera.
    fn camera_settings_result(&self, camera: &Camera) -> CameraSettingsResult {
        let uri = CameraDto::from(camera).definition.map(|d| d.uri);
        let mut result = CameraSettingsResult {
            camera_id: camera.id.clone(),
            status: CameraDefinitionStatus::None,
            uri,
            reason: None,
            kept: false,
            reading: false,
            read_failure: None,
            settings: Vec::new(),
        };
        let Some(entry) = self.camera_settings.0.get(&camera.id) else {
            result.reason = match (&result.uri, camera.device_id) {
                (None, _) => None,
                (Some(_), Some(_)) => Some(
                    "The autopilot speaks for this camera. UXDuck reads a camera's own settings from a camera that is a MAVLink component of its own.".into(),
                ),
                (Some(_), None) => Some(
                    "The camera is not heard. UXDuck reads its definition file when it is heard again.".into(),
                ),
            };
            return result;
        };
        result.kept = entry.kept;
        result.reading = entry.reading;
        result.read_failure.clone_from(&entry.read_failure);
        let definition = match &entry.file {
            File::Ask => {
                result.status = CameraDefinitionStatus::Ask;
                return result;
            }
            File::Fetching => {
                result.status = CameraDefinitionStatus::Fetching;
                return result;
            }
            File::Failed(why) => {
                result.status = CameraDefinitionStatus::Failed;
                result.reason = Some(why.clone());
                return result;
            }
            File::Refused(why) => {
                result.status = CameraDefinitionStatus::Refused;
                result.reason = Some(why.clone());
                return result;
            }
            File::Loaded(definition) => definition,
        };
        result.status = CameraDefinitionStatus::Loaded;
        let value = |name: &str| entry.value(name);
        result.settings = definition
            .settings
            .iter()
            .map(|setting| CameraSetting {
                name: setting.name.clone(),
                description: setting.description.clone(),
                kind: setting.kind.into(),
                value: value(&setting.name),
                unit: setting.unit.clone(),
                options: definition
                    .choices(setting, &value)
                    .into_iter()
                    .map(|(name, value)| CameraSettingOption { name, value })
                    .collect(),
                min: setting.min,
                max: setting.max,
                step: setting.step,
                read_only: setting.read_only || setting.kind == CameraSettingKind::Custom,
                hidden: !setting.control,
                excluded_by: definition
                    .excluded_by(&setting.name, &value)
                    .map(str::to_owned),
            })
            .collect();
        result
    }
}

fn camera(engine: &Engine, id: &str) -> Result<Camera, ApiError> {
    engine
        .state
        .cameras
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no camera {id}")))
}

/// `camera/settings`, for anyone who may read. With `refresh` the
/// camera is asked for every value again, as a parameter refresh asks the
/// vehicle.
pub(crate) fn settings(
    engine: &mut Engine,
    params: &CameraSettingsParams,
    granted: &[Capability],
) -> Result<Json, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let camera = camera(engine, &params.camera_id)?;
    let loaded = matches!(
        engine.camera_settings.0.get(&camera.id).map(|e| &e.file),
        Some(File::Loaded(_))
    );
    if params.refresh && loaded {
        engine.writable_vehicle(&camera.vehicle_id)?;
        engine.camera_settings_read(&camera.id, None);
    }
    to_value(&engine.camera_settings_result(&camera))
}

/// `camera/definition/fetch`: a person agrees to fetch the file from the
/// address they were shown, or asks for a fetch that failed or was
/// refused to be tried again.
pub(crate) fn fetch(
    engine: &mut Engine,
    params: &CameraDefinitionFetchParams,
) -> Result<Json, ApiError> {
    let camera = camera(engine, &params.camera_id)?;
    let shown = CameraDto::from(&camera).definition.map(|d| d.uri);
    let Some(entry) = engine.camera_settings.0.get(&camera.id) else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "UXDuck has no definition file to fetch for this camera now",
        ));
    };
    // What was agreed to is the address that was read, not one the camera
    // has named since.
    if shown.as_deref() != Some(params.uri.as_str()) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the camera now names another address. Read it, then agree again",
        ));
    }
    if matches!(entry.file, File::Ask | File::Failed(_) | File::Refused(_)) {
        let uri = entry.uri.clone();
        let file = engine.camera_definition_fetch(&camera, &uri, true);
        if let Some(entry) = engine.camera_settings.0.get_mut(&camera.id) {
            entry.file = file;
        }
        engine.camera_settings_moved(&camera.id);
    }
    to_value(&engine.camera_settings_result(&camera))
}
