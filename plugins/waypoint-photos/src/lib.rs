//! The waypoint-photos plugin: an automation. At every waypoint a vehicle
//! says it reached (`MISSION_ITEM_REACHED`) it has the vehicle's camera take
//! a photo with UXDuck's own camera command, the one "Photo to card" in the
//! Camera tab sends, and it counts what the camera answered from decoded
//! `COMMAND_ACK`s (plugin ABI v2). Written against the SDK only, to keep it
//! honest (ADR-0006). It took the place of the camera-manager example, whose
//! buttons the core's own camera controls now are (ADR-0063 Y12).

// Plugin functions return `FnResult` because the ABI says so, even when they
// cannot fail.
#![allow(clippy::unnecessary_wraps)]

use std::cell::RefCell;
use std::collections::BTreeMap;

// The `plugin_fn` macro refers to `extism_pdk` by name.
use uxduck_sdk::extism_pdk;
use uxduck_sdk::extism_pdk::{FnResult, Json, plugin_fn};
use uxduck_sdk::messages::{CommandAck, MissionItemReached};
use uxduck_sdk::{
    CameraAction, CommandCall, Description, Effects, Message, MessageBatch, Panel, ProducedItem,
    Unit, Widget, camera_command, text, value,
};

/// `MAV_CMD_IMAGE_START_CAPTURE`: what the camera command for one photo goes
/// out as, and so what the camera's acknowledgement names.
const PHOTO: u16 = 2000;
/// `MAV_RESULT_ACCEPTED` and `MAV_RESULT_IN_PROGRESS`.
const ACCEPTED: u8 = 0;
const IN_PROGRESS: u8 = 5;

/// What one vehicle's camera was asked for and has answered.
#[derive(Default)]
struct Photos {
    /// What the photo still unanswered is of, such as "Waypoint 3".
    waiting: Option<String>,
    taken: u32,
    refused: u32,
}

thread_local! {
    static PHOTOS: RefCell<BTreeMap<String, Photos>> = const { RefCell::new(BTreeMap::new()) };
}

/// The panel: the counts, what became of the last photo, and a test photo.
#[must_use]
pub fn description() -> Description {
    let count = |item: &str, label: &str, help: &str| Widget::Value {
        item: item.into(),
        label: label.into(),
        tone: None,
        help: Some(help.into()),
    };
    Description {
        panels: vec![Panel {
            id: "waypoint-photos".into(),
            title: "Waypoint photos".into(),
            widgets: vec![
                count(
                    "waypoint_photos.taken",
                    "Photos taken",
                    "Photos the camera accepted since the plugin started: one at each waypoint the vehicle reached, and each test photo.",
                ),
                count(
                    "waypoint_photos.refused",
                    "Refused",
                    "Photos the camera refused. History says why, such as a full card.",
                ),
                Widget::Value {
                    item: "waypoint_photos.last".into(),
                    label: "Last photo".into(),
                    tone: None,
                    help: None,
                },
                Widget::Button {
                    label: "Take a test photo".into(),
                    command: "test-photo".into(),
                    params: None,
                    tone: None,
                    help: Some(
                        "One photo now, to check the camera answers before the mission.".into(),
                    ),
                },
            ],
            tab: false,
            vehicle: false,
        }],
    }
}

/// A vehicle's counts, and what became of its last photo.
fn items(vehicle: &str, photos: &Photos, last: &str) -> Vec<ProducedItem> {
    vec![
        value(
            vehicle,
            "waypoint_photos.taken",
            "Photos taken",
            Unit::Count,
            f64::from(photos.taken),
        ),
        value(
            vehicle,
            "waypoint_photos.refused",
            "Refused",
            Unit::Count,
            f64::from(photos.refused),
        ),
        text(vehicle, "waypoint_photos.last", "Last photo", last),
    ]
}

/// Has the vehicle's camera take one photo, and keeps what it is of until
/// the camera answers. No camera is named: UXDuck picks the vehicle's own.
fn photo(vehicle: &str, of: String, effects: &mut Effects) {
    PHOTOS.with_borrow_mut(|all| {
        let photos = all.entry(vehicle.to_owned()).or_default();
        effects.items.extend(items(
            vehicle,
            photos,
            &format!("{of}: waiting for the camera"),
        ));
        photos.waiting = Some(of);
    });
    effects
        .commands
        .push(camera_command(vehicle, CameraAction::TakePhoto));
}

/// The button, the action and the agent's tool: one photo now.
#[must_use]
pub fn command(call: &CommandCall) -> Effects {
    let mut effects = Effects::default();
    match (call.command.as_str(), call.vehicle_id.as_deref()) {
        ("test-photo", Some(vehicle)) => photo(vehicle, "Test photo".into(), &mut effects),
        // It has no tab of its own, so a photo is always a vehicle's.
        ("test-photo", None) => {}
        (other, _) => effects.log.push(format!("unknown command {other}")),
    }
    effects
}

/// A waypoint reached: a photo there. An acknowledgement: what the camera
/// said to the photo still unanswered. An acknowledgement does not say who
/// asked, so a photo someone else asked for in that moment counts as this
/// plugin's.
#[must_use]
pub fn messages(batch: &MessageBatch) -> Effects {
    let mut effects = Effects::default();
    for input in &batch.messages {
        let vehicle = &input.vehicle_id;
        if let Some(reached) = MissionItemReached::from_input(input) {
            photo(vehicle, format!("Waypoint {}", reached.seq), &mut effects);
            continue;
        }
        let Some(ack) = CommandAck::from_input(input) else {
            continue;
        };
        // In progress is not the answer yet: the camera says more.
        if ack.command != PHOTO || ack.result == IN_PROGRESS {
            continue;
        }
        PHOTOS.with_borrow_mut(|all| {
            let Some(photos) = all.get_mut(vehicle) else {
                return;
            };
            let Some(of) = photos.waiting.take() else {
                return;
            };
            let said = if ack.result == ACCEPTED {
                photos.taken += 1;
                "taken"
            } else {
                photos.refused += 1;
                "the camera refused"
            };
            effects
                .items
                .extend(items(vehicle, photos, &format!("{of}: {said}")));
        });
    }
    effects
}

#[plugin_fn]
pub fn describe(_: Json<serde_json::Value>) -> FnResult<Json<Description>> {
    Ok(Json(description()))
}

#[plugin_fn]
pub fn on_command(Json(call): Json<CommandCall>) -> FnResult<Json<Effects>> {
    Ok(Json(command(&call)))
}

#[plugin_fn]
pub fn on_messages(Json(batch): Json<MessageBatch>) -> FnResult<Json<Effects>> {
    Ok(Json(messages(&batch)))
}
