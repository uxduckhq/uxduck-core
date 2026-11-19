//! A controller's camera (ADR-0005 A3, S416): zoom and focus while a
//! button is held or an axis is pushed, and each photo a controller asked
//! for said where the pilot is once the camera says it took it.
//!
//! A held zoom or focus goes as the camera protocol's continuous zoom or
//! focus, through `command/send` like every camera command, and stops
//! however the hold ends: let go, the controller unplugged, its route
//! ended or the daemon stopping. A stop that could not be sent is owed,
//! and goes again once a second while the vehicle is heard. A camera
//! that refuses to zoom or focus on at a rate is zoomed or focused by
//! steps instead, one each time the controller says the input is still
//! held (`CAMERA_REPEAT_MS`).
#![allow(
    clippy::float_cmp,
    reason = "rates are quarter steps and zeros set exactly, never computed"
)]

use std::collections::BTreeMap;

use uxduck_contracts::dto::command::CommandSendParams;
use uxduck_core::domain::camera::{Camera, CameraAction};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::controller::ControllerPhoto;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::Change;

use crate::commands::Raise;
use crate::{ConnectionId, Engine};

/// How long after a controller's Take photo the camera's word of a photo
/// is taken as its answer: a camera says of each photo within a second or
/// two, and one that never says must not have a later photo, taken from
/// the Camera tab or a mission, said as the controller's.
const PHOTO_ANSWER_US: u64 = 10_000_000;

/// What the engine keeps of controllers' cameras. Never journaled.
#[derive(Debug, Default)]
pub(crate) struct Drives {
    /// The zoom and focus a controller is driving, by controller id.
    active: BTreeMap<String, Drive>,
    /// Cameras that refused to zoom (index 0) or focus (index 1) on at a
    /// rate, by camera id: they are driven by steps while UXDuck runs.
    by_steps: BTreeMap<String, [bool; 2]>,
    /// Photos a controller asked for whose camera has not said yet.
    photos: Vec<PhotoAsked>,
    /// The daemon is stopping: nothing more starts.
    ended: bool,
}

#[derive(Debug)]
struct Drive {
    vehicle_id: String,
    camera_id: String,
    component_id: u8,
    device_id: Option<u8>,
    connection: ConnectionId,
    /// What the controller asks for now, zoom then focus.
    wanted: [f32; 2],
    /// The rates the camera was last told to go on at, which a stop
    /// must end.
    running: [f32; 2],
    /// The command on its way: its receipt, and zoom or focus.
    waiting: Option<(String, usize)>,
    /// Refused while held, by steps too: nothing more is asked until it
    /// is let go.
    refused: [bool; 2],
    /// A stop that could not be sent, tried again once a second.
    owed: bool,
}

#[derive(Debug)]
struct PhotoAsked {
    receipt_id: String,
    controller_id: String,
    vehicle_id: String,
    component_id: Option<u8>,
    device_id: Option<u8>,
    asked_us: u64,
    /// How many photos each of the vehicle's cameras had counted when this
    /// was sent, by camera id. A camera that had said no count is not here.
    counted: BTreeMap<String, u32>,
}

impl Engine {
    /// What a routed controller's camera inputs ask for now (S416), as the
    /// data plane says it (`Report::CameraDrive`): a change, or the same
    /// again while held.
    pub fn controller_camera_drive(&mut self, id: &str, drive: [f32; 2]) {
        if self.camera_drives.ended {
            return;
        }
        if !self.camera_drives.active.contains_key(id) {
            if drive == [0.0; 2] {
                return;
            }
            let Some(started) = self.drive_started(id, drive) else {
                return;
            };
            self.camera_drives.active.insert(id.to_owned(), started);
        }
        let Some(drive_now) = self.camera_drives.active.get_mut(id) else {
            return;
        };
        for (refused, wanted) in drive_now.refused.iter_mut().zip(drive) {
            if wanted == 0.0 {
                *refused = false;
            }
        }
        drive_now.wanted = drive;
        self.drive_on(id, true);
    }

    /// A new zoom or focus from a controller: the camera of its target that
    /// can do it, or a notice that it has none.
    fn drive_started(&mut self, id: &str, drive: [f32; 2]) -> Option<Drive> {
        let (controller, connection, target) = self.controller_sends(id)?;
        let action = if drive[0] == 0.0 {
            CameraAction::FocusRate { rate: drive[1] }
        } else {
            CameraAction::ZoomRate { rate: drive[0] }
        };
        let vehicle_id = self.vehicle(&target).map(|vehicle| vehicle.id.clone())?;
        let camera = self
            .state
            .cameras
            .values()
            .filter(|camera| camera.vehicle_id == vehicle_id && camera.present)
            .find(|camera| camera.information.is_some() && camera.allows(&action));
        let Some(camera) = camera else {
            let name = self.target_name(&target);
            let what = if drive[0] == 0.0 { "focuses" } else { "zooms" };
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle { id: vehicle_id },
                title: format!("{} did not reach {name}'s camera", controller.name),
                body: Some(format!("{name} has no camera that {what}.")),
                actions: Vec::new(),
                condition: None,
                report: None,
            });
            return None;
        };
        Some(Drive {
            vehicle_id,
            camera_id: camera.id.clone(),
            component_id: camera.component_id,
            device_id: camera.device_id,
            connection,
            wanted: [0.0; 2],
            running: [0.0; 2],
            waiting: None,
            refused: [false; 2],
            owed: false,
        })
    }

    /// Asks the camera for what the controller wants now, one command at
    /// a time: a rate that differs from the one running, or, when `step`,
    /// a step while a camera driven by steps is held, so steps go only as
    /// often as the controller says it is held. A drive with nothing held,
    /// nothing running and nothing owed is over.
    fn drive_on(&mut self, id: &str, step_now: bool) {
        let Some(drive) = self.camera_drives.active.get(id) else {
            return;
        };
        if drive.waiting.is_some() || drive.owed {
            return;
        }
        let steps = self
            .camera_drives
            .by_steps
            .get(&drive.camera_id)
            .copied()
            .unwrap_or_default();
        let next = (0..2).find_map(|kind| {
            let wanted = drive.wanted[kind];
            let refused = wanted != 0.0 && drive.refused[kind];
            if steps[kind] {
                (step_now && wanted != 0.0 && !refused).then(|| (kind, step(kind, wanted)))
            } else {
                (wanted != drive.running[kind] && !refused).then(|| (kind, rate(kind, wanted)))
            }
        });
        let Some((kind, action)) = next else {
            if drive.wanted == [0.0; 2] && drive.running == [0.0; 2] {
                self.camera_drives.active.remove(id);
            }
            return;
        };
        let stop = action == rate(kind, 0.0);
        let receipt = if stop {
            self.drive_stop(id, kind)
        } else {
            self.drive_ask(id, &action)
        };
        let Some(drive) = self.camera_drives.active.get_mut(id) else {
            return;
        };
        match receipt {
            Some(receipt_id) => {
                // A rate runs from when it is sent. It stops only once the
                // camera answers its stop.
                if let CameraAction::ZoomRate { rate } | CameraAction::FocusRate { rate } = action
                    && !stop
                {
                    drive.running[kind] = rate;
                }
                drive.waiting = Some((receipt_id, kind));
            }
            // A stop that could not go is owed. A start that could not go
            // is refused until the input is let go, as its notice says.
            None if stop => drive.owed = true,
            None => drive.refused[kind] = true,
        }
    }

    /// A zoom or focus asked for as the controller, through every check a
    /// button's command has, refusals said in a notice.
    fn drive_ask(&mut self, id: &str, action: &CameraAction) -> Option<String> {
        let (controller, connection, target) = self.controller_sends(id)?;
        let drive = self.camera_drives.active.get(id)?;
        let command = Command::Camera {
            component_id: Some(drive.component_id),
            device_id: drive.device_id,
            action: action.clone(),
        };
        self.send_for_controller(&controller, connection, target, &command)
    }

    /// A stop, whatever became of the controller since: confirmed for a
    /// vehicle that is not heard now, since stopping a zoom is never the
    /// risk. Its receipt, or none when it could not go.
    fn drive_stop(&mut self, id: &str, kind: usize) -> Option<String> {
        let drive = self.camera_drives.active.get(id)?;
        let params = CommandSendParams {
            vehicle_id: drive.vehicle_id.clone(),
            command: (&Command::Camera {
                component_id: Some(drive.component_id),
                device_id: drive.device_id,
                action: rate(kind, 0.0),
            })
                .into(),
            confirm: true,
            force: false,
            override_preflight: Vec::new(),
        };
        let connection = drive.connection;
        crate::commands::send_checked(self, connection, params, &[Capability::Control], true)
            .ok()
            .and_then(|(_, sent)| sent)
    }

    /// A command ended (S416): a drive's next step or rate goes, a camera
    /// that refused a rate is driven by steps from now on, and a stop that
    /// was not answered is owed. A controller's photo the camera refused
    /// is waited for no more.
    pub(crate) fn camera_drive_answered(&mut self, receipt: &Receipt) {
        self.camera_drives.photos.retain(|asked| {
            asked.receipt_id != receipt.id || receipt.status == ReceiptStatus::Accepted
        });
        let Some(id) = self
            .camera_drives
            .active
            .iter()
            .find(|(_, drive)| {
                drive
                    .waiting
                    .as_ref()
                    .is_some_and(|(waiting, _)| *waiting == receipt.id)
            })
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let Command::Camera { action, .. } = &receipt.command else {
            return;
        };
        let action = action.clone();
        let Some(drive) = self.camera_drives.active.get_mut(&id) else {
            return;
        };
        let Some((_, kind)) = drive.waiting.take() else {
            return;
        };
        let camera_id = drive.camera_id.clone();
        let mut step_now = false;
        match (receipt.status, action) {
            (
                ReceiptStatus::Accepted,
                CameraAction::ZoomRate { rate } | CameraAction::FocusRate { rate },
            ) if rate == 0.0 => drive.running[kind] = 0.0,
            (ReceiptStatus::Accepted, _) => {}
            // A camera that will not go on at a rate goes by steps.
            (
                ReceiptStatus::Rejected,
                CameraAction::ZoomRate { rate } | CameraAction::FocusRate { rate },
            ) if rate != 0.0 => {
                drive.running[kind] = 0.0;
                self.camera_drives.by_steps.entry(camera_id).or_default()[kind] = true;
                step_now = true;
            }
            // A step refused is not asked again until it is let go.
            (
                ReceiptStatus::Rejected,
                CameraAction::ZoomStep { .. } | CameraAction::FocusStep { .. },
            ) => drive.refused[kind] = true,
            // Its answer to a stop is what it does.
            (ReceiptStatus::Rejected, _) => drive.running[kind] = 0.0,
            // Unanswered: a rate may be running, so its stop is owed.
            (_, CameraAction::ZoomRate { rate } | CameraAction::FocusRate { rate })
                if rate == 0.0 =>
            {
                drive.owed = true;
            }
            _ => {}
        }
        self.drive_on(&id, step_now);
    }

    /// Once a second (S416): a stop owed goes again while its vehicle is
    /// heard on a link that takes it, a drive whose vehicle is gone is
    /// over, and a photo the camera never said of is waited for no more.
    pub fn review_camera_drives(&mut self) {
        let owed: Vec<String> = self
            .camera_drives
            .active
            .iter()
            .filter(|(_, drive)| drive.owed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in owed {
            let Some(vehicle) = self
                .camera_drives
                .active
                .get(&id)
                .and_then(|drive| self.vehicle(&drive.vehicle_id))
                .cloned()
            else {
                self.camera_drives.active.remove(&id);
                continue;
            };
            if vehicle.presence != Presence::Online || self.not_writable(&vehicle.link_id).is_some()
            {
                continue;
            }
            if let Some(drive) = self.camera_drives.active.get_mut(&id) {
                drive.owed = false;
            }
            self.drive_on(&id, false);
        }
        let now = self.clock.now_us();
        self.camera_drives
            .photos
            .retain(|asked| now.saturating_sub(asked.asked_us) < PHOTO_ANSWER_US);
    }

    /// The daemon is stopping (S416): every zoom and focus a controller
    /// keeps running is told to stop while the links still take it, and
    /// nothing more starts. Whether any was.
    pub fn camera_drives_end(&mut self) -> bool {
        self.camera_drives.ended = true;
        let running: Vec<(String, usize)> = self
            .camera_drives
            .active
            .iter()
            .flat_map(|(id, drive)| {
                (0..2)
                    .filter(|kind| drive.running[*kind] != 0.0)
                    .map(|kind| (id.clone(), kind))
            })
            .collect();
        for (id, kind) in &running {
            self.drive_stop(id, *kind);
        }
        !running.is_empty()
    }

    /// A command a controller sent (S416): a photo is waited for, to say
    /// where the pilot is when the camera says it took it.
    pub(crate) fn controller_photo_asked(&mut self, controller_id: &str, receipt_id: &str) {
        let Some(receipt) = self.state.receipts.get(receipt_id) else {
            return;
        };
        let Command::Camera {
            component_id,
            device_id,
            action: CameraAction::TakePhoto,
        } = &receipt.command
        else {
            return;
        };
        let counted = self
            .state
            .cameras
            .values()
            .filter(|camera| camera.vehicle_id == receipt.vehicle_id)
            .filter_map(|camera| Some((camera.id.clone(), camera.capture?.photo_count)))
            .collect();
        let asked = PhotoAsked {
            receipt_id: receipt_id.to_owned(),
            controller_id: controller_id.to_owned(),
            vehicle_id: receipt.vehicle_id.clone(),
            component_id: *component_id,
            device_id: *device_id,
            asked_us: self.clock.now_us(),
            counted,
        };
        self.camera_drives.photos.push(asked);
    }

    /// A photo a camera said it took (S416): the answer to the oldest Take
    /// photo a controller sent it, said in the controller's row and in a
    /// short notice. A photo the camera had already counted when the
    /// controller asked is an earlier one, described late or read from a
    /// backlog, and is no answer to it.
    pub(crate) fn controller_photo_heard(&mut self, camera: &Camera, index: u32, ok: bool) {
        let now = self.clock.now_us();
        let Some(at) = self.camera_drives.photos.iter().position(|asked| {
            asked.vehicle_id == camera.vehicle_id
                && asked
                    .component_id
                    .is_none_or(|id| id == camera.component_id)
                && (asked.component_id.is_none() || asked.device_id == camera.device_id)
                && now.saturating_sub(asked.asked_us) < PHOTO_ANSWER_US
                && asked
                    .counted
                    .get(&camera.id)
                    .is_none_or(|counted| index >= *counted)
        }) else {
            return;
        };
        let asked = self.camera_drives.photos.remove(at);
        let Some(mut controller) = self.state.controllers.get(&asked.controller_id).cloned() else {
            return;
        };
        controller.photo = Some(ControllerPhoto {
            vehicle_id: camera.vehicle_id.clone(),
            camera_id: camera.id.clone(),
            index,
            ok,
            heard_at_us: now,
        });
        controller.version += 1;
        let name = self.target_name(&camera.vehicle_id);
        let number = index.saturating_add(1);
        let (severity, title, body) = if ok {
            (
                Severity::Info,
                format!("Photo {number} taken"),
                format!("{} took it with {name}'s camera.", controller.name),
            )
        } else {
            (
                Severity::Warning,
                format!("Photo {number} failed"),
                format!(
                    "{name}'s camera said the photo {} asked for failed.",
                    controller.name
                ),
            )
        };
        self.commit_as(
            crate::vehicles::data_plane(),
            Change::ControllerUpserted { controller },
        );
        self.raise(Raise {
            severity,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: camera.vehicle_id.clone(),
            },
            title,
            body: Some(body),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }
}

/// Zoom (0) or focus (1) on at a rate, 0 stopping it.
fn rate(kind: usize, rate: f32) -> CameraAction {
    if kind == 0 {
        CameraAction::ZoomRate { rate }
    } else {
        CameraAction::FocusRate { rate }
    }
}

/// One step of zoom (0) or focus (1), the way `wanted` asks.
fn step(kind: usize, wanted: f32) -> CameraAction {
    let steps = if wanted > 0.0 { 1 } else { -1 };
    if kind == 0 {
        CameraAction::ZoomStep { steps }
    } else {
        CameraAction::FocusStep { steps }
    }
}
