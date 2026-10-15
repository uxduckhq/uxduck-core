//! The camera-manager plugin: a panel with camera buttons, sent as MAVLink
//! camera-protocol commands, and telemetry items counting what the camera
//! confirmed, read from decoded `COMMAND_ACK`s (plugin ABI v2). Written
//! against the SDK only, to keep it honest (ADR-0006).

// Plugin functions return `FnResult` because the ABI says so, even when they
// cannot fail.
#![allow(clippy::unnecessary_wraps)]

use std::cell::RefCell;
use std::collections::BTreeMap;

// The `plugin_fn` macro refers to `extism_pdk` by name.
use uxduck_sdk::extism_pdk;
use uxduck_sdk::extism_pdk::{FnResult, Json, plugin_fn};
use uxduck_sdk::messages::CommandAck;
use uxduck_sdk::{
    CommandCall, Description, Effects, Message, MessageBatch, Panel, Unit, Widget, command_long,
    text, value,
};

/// `MAV_CMD_IMAGE_START_CAPTURE`, `MAV_CMD_VIDEO_START_CAPTURE`,
/// `MAV_CMD_VIDEO_STOP_CAPTURE` and `MAV_CMD_SET_CAMERA_ZOOM`.
const PHOTO: u16 = 2000;
const VIDEO_START: u16 = 2500;
const VIDEO_STOP: u16 = 2501;
const ZOOM: u16 = 531;

/// What the camera of each vehicle has confirmed.
#[derive(Default)]
struct Camera {
    photos: u32,
    recording: bool,
}

thread_local! {
    static CAMERAS: RefCell<BTreeMap<String, Camera>> = const { RefCell::new(BTreeMap::new()) };
}

/// The panel; everything else happens in answer to it.
#[must_use]
pub fn description() -> Description {
    let button = |label: &str, command: &str| Widget::Button {
        label: label.into(),
        command: command.into(),
        params: None,
        tone: None,
        help: None,
    };
    Description {
        panels: vec![Panel {
            id: "camera".into(),
            title: "Camera".into(),
            widgets: vec![
                Widget::Value {
                    item: "camera.photos".into(),
                    label: "Photos".into(),
                    tone: None,
                    help: Some("Photos the camera said it took since the plugin started.".into()),
                },
                Widget::Value {
                    item: "camera.video".into(),
                    label: "Video".into(),
                    tone: None,
                    help: None,
                },
                button("Take a photo", "photo"),
                button("Start video", "video-start"),
                button("Stop video", "video-stop"),
                button("Zoom in", "zoom-in"),
                button("Zoom out", "zoom-out"),
            ],
            tab: false,
            vehicle: false,
        }],
    }
}

/// A button: the camera-protocol command for it.
#[must_use]
pub fn command(call: &CommandCall) -> Effects {
    // Every command is a vehicle's camera's; it has no tab of its own.
    let Some(vehicle) = call.vehicle_id.as_deref() else {
        return Effects::default();
    };
    let send = match call.command.as_str() {
        // Camera 0 (all), no interval, one image.
        "photo" => command_long(vehicle, PHOTO, &[0.0, 0.0, 1.0, 0.0]),
        "video-start" => command_long(vehicle, VIDEO_START, &[0.0, 0.0]),
        "video-stop" => command_long(vehicle, VIDEO_STOP, &[0.0]),
        // Zoom type 0: a step in or out.
        "zoom-in" => command_long(vehicle, ZOOM, &[0.0, 1.0]),
        "zoom-out" => command_long(vehicle, ZOOM, &[0.0, -1.0]),
        other => {
            return Effects {
                log: vec![format!("unknown command {other}")],
                ..Effects::default()
            };
        }
    };
    Effects {
        commands: vec![send],
        ..Effects::default()
    }
}

/// `MAV_RESULT_ACCEPTED`.
const ACCEPTED: u8 = 0;

/// Acknowledgements: count what the camera confirmed, and say so.
#[must_use]
pub fn messages(batch: &MessageBatch) -> Effects {
    let mut items = Vec::new();
    CAMERAS.with_borrow_mut(|cameras| {
        for input in &batch.messages {
            let Some(ack) = CommandAck::from_input(input) else {
                continue;
            };
            if ack.result != ACCEPTED {
                continue;
            }
            let vehicle = &input.vehicle_id;
            let camera = cameras.entry(vehicle.clone()).or_default();
            match ack.command {
                PHOTO => camera.photos += 1,
                VIDEO_START => camera.recording = true,
                VIDEO_STOP => camera.recording = false,
                _ => continue,
            }
            items.push(value(
                vehicle,
                "camera.photos",
                "Photos",
                Unit::Count,
                f64::from(camera.photos),
            ));
            items.push(text(
                vehicle,
                "camera.video",
                "Video",
                if camera.recording {
                    "Recording"
                } else {
                    "Stopped"
                },
            ));
        }
    });
    Effects {
        items,
        ..Effects::default()
    }
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
