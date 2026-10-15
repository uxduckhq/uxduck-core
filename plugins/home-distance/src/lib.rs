//! The plugin ABI's example (ADR-0006 X1 to X5, X10): telemetry items
//! (`battery.*`) and a decoded message (`GLOBAL_POSITION_INT`) in; an item
//! with its unit (`home.distance_m`), a notification below 20 % battery,
//! declared commands with a parameter, a panel with every kind of widget,
//! and a 100 m circle around home on the map out.

// Plugin functions return `FnResult` because the ABI says so.
#![allow(clippy::unnecessary_wraps)]

use std::cell::RefCell;
use std::collections::BTreeMap;

use uxduck_sdk::extism_pdk;
use uxduck_sdk::extism_pdk::{FnResult, Json, plugin_fn};
use uxduck_sdk::messages::GlobalPositionInt;
use uxduck_sdk::{
    CommandCall, Description, Effects, ItemBatch, MapFeature, Message, MessageBatch, Panel,
    Severity, Tone, Unit, Widget, circle, notify, point, table, value,
};

/// Below this, the battery notice; it is raised again only after the
/// battery rose back above `RESET_PCT`.
const LOW_PCT: f64 = 20.0;
const RESET_PCT: f64 = 25.0;
/// The circle drawn around home.
const CIRCLE_M: f64 = 100.0;

#[derive(Default)]
struct Vehicle {
    /// Where it started, or was last told home is, in degrees.
    home: Option<(f64, f64)>,
    /// Where it was last heard.
    here: Option<(f64, f64)>,
    warned: bool,
    marks: Vec<Vec<String>>,
}

thread_local! {
    static VEHICLES: RefCell<BTreeMap<String, Vehicle>> = const { RefCell::new(BTreeMap::new()) };
}

/// The distance between two points on the earth, in metres (haversine).
#[must_use]
pub fn distance_m(from: (f64, f64), to: (f64, f64)) -> f64 {
    const EARTH_M: f64 = 6_371_000.0;
    let (lat1, lon1) = (from.0.to_radians(), from.1.to_radians());
    let (lat2, lon2) = (to.0.to_radians(), to.1.to_radians());
    let a = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_M * a.sqrt().asin()
}

#[must_use]
pub fn description() -> Description {
    let help = |text: &str| Some(text.to_owned());
    Description {
        panels: vec![Panel {
            id: "home".into(),
            title: "Home".into(),
            widgets: vec![
                Widget::Text {
                    text: "Distance and battery".into(),
                    heading: true,
                    tone: None,
                    help: help(
                        "Home is where the vehicle was first heard, or where you last chose Set home here.",
                    ),
                },
                Widget::Value {
                    item: "home.distance_m".into(),
                    label: "From home".into(),
                    tone: None,
                    help: help("The straight line along the ground from home to the vehicle."),
                },
                Widget::Gauge {
                    item: "battery.remaining_pct".into(),
                    label: "Battery".into(),
                    min: 0.0,
                    max: 100.0,
                    warn_below: Some(LOW_PCT),
                    warn_above: None,
                    tone: None,
                    help: help(
                        "The charge the flight controller estimates is left. Below 20 % the plugin says so once.",
                    ),
                },
                Widget::Chart {
                    item: "home.distance_m".into(),
                    label: "Distance, last two minutes".into(),
                    window_s: 120,
                    tone: None,
                    help: None,
                },
                Widget::Table {
                    item: "home.marks".into(),
                    label: "Marks".into(),
                    columns: vec!["#".into(), "Label".into(), "From home".into()],
                    tone: Some(Tone::Muted),
                    help: None,
                },
                Widget::Button {
                    label: "Mark".into(),
                    command: "mark".into(),
                    params: None,
                    tone: None,
                    help: None,
                },
                Widget::Button {
                    label: "Set home here".into(),
                    command: "set-home".into(),
                    params: None,
                    tone: None,
                    help: help(
                        "Moves home, and its circle on the map, to where the vehicle is now.",
                    ),
                },
            ],
            tab: false,
            vehicle: false,
        }],
    }
}

/// Home's circle and point on the map.
fn home_features(home: (f64, f64)) -> Vec<MapFeature> {
    let mut ring = circle("home-circle", home.0, home.1, CIRCLE_M);
    ring.label = Some("100 m from home".into());
    ring.fill = true;
    let mut centre = point("home", home.0, home.1);
    centre.tone = Some(Tone::Muted);
    vec![ring, centre]
}

/// Positions: the first is home; each after it, how far from home.
#[must_use]
pub fn messages(batch: &MessageBatch) -> Effects {
    let mut effects = Effects::default();
    VEHICLES.with_borrow_mut(|vehicles| {
        for input in &batch.messages {
            let Some(position) = GlobalPositionInt::from_input(input) else {
                continue;
            };
            // `lat` and `lon` are in degE7.
            let here = (f64::from(position.lat) / 1e7, f64::from(position.lon) / 1e7);
            let vehicle = vehicles.entry(input.vehicle_id.clone()).or_default();
            vehicle.here = Some(here);
            let home = if let Some(home) = vehicle.home {
                home
            } else {
                vehicle.home = Some(here);
                effects.features.extend(home_features(here));
                here
            };
            effects.items.push(value(
                &input.vehicle_id,
                "home.distance_m",
                "From home",
                Unit::Meter,
                distance_m(home, here),
            ));
        }
    });
    effects
}

/// Battery items: a notice when one drops below 20 %.
#[must_use]
pub fn items(batch: &ItemBatch) -> Effects {
    let mut notifications = Vec::new();
    VEHICLES.with_borrow_mut(|vehicles| {
        for item in &batch.items {
            let (Some(remaining), "battery.remaining_pct") = (item.value, item.key.as_str()) else {
                continue;
            };
            let vehicle = vehicles.entry(item.vehicle_id.clone()).or_default();
            if remaining < LOW_PCT && !vehicle.warned {
                vehicle.warned = true;
                notifications.push(notify(
                    &item.vehicle_id,
                    Severity::Warning,
                    &format!("Battery at {} %", remaining.floor()),
                ));
            } else if remaining > RESET_PCT {
                vehicle.warned = false;
            }
        }
    });
    Effects {
        notifications,
        ..Effects::default()
    }
}

/// "Mark", with a label; "Set home here".
#[must_use]
pub fn command(call: &CommandCall) -> Effects {
    // Its commands are a vehicle's; it has no tab of its own.
    let Some(vehicle_id) = call.vehicle_id.as_deref() else {
        return Effects::default();
    };
    VEHICLES.with_borrow_mut(|vehicles| {
        let vehicle = vehicles.entry(vehicle_id.to_owned()).or_default();
        match call.command.as_str() {
            "mark" => {
                let label: String = call.params["label"]
                    .as_str()
                    .unwrap_or("Mark")
                    .chars()
                    .take(80)
                    .collect();
                let from_home = match (vehicle.home, vehicle.here) {
                    (Some(home), Some(here)) => format!("{:.0} m", distance_m(home, here)),
                    _ => "—".into(),
                };
                let number = (vehicle.marks.len() + 1).to_string();
                vehicle.marks.push(vec![number, label.clone(), from_home]);
                Effects {
                    items: vec![table(vehicle_id, "home.marks", "Marks", &vehicle.marks)],
                    notifications: vec![notify(
                        vehicle_id,
                        Severity::Info,
                        &format!("Marked: {label}"),
                    )],
                    log: vec![format!("marked {label}")],
                    ..Effects::default()
                }
            }
            "set-home" => match vehicle.here {
                Some(here) => {
                    vehicle.home = Some(here);
                    Effects {
                        features: home_features(here),
                        items: vec![value(
                            vehicle_id,
                            "home.distance_m",
                            "From home",
                            Unit::Meter,
                            0.0,
                        )],
                        ..Effects::default()
                    }
                }
                None => Effects {
                    log: vec!["no position yet".into()],
                    ..Effects::default()
                },
            },
            other => Effects {
                log: vec![format!("unknown command {other}")],
                ..Effects::default()
            },
        }
    })
}

#[plugin_fn]
pub fn describe(_: Json<serde_json::Value>) -> FnResult<Json<Description>> {
    Ok(Json(description()))
}

#[plugin_fn]
pub fn on_messages(Json(batch): Json<MessageBatch>) -> FnResult<Json<Effects>> {
    Ok(Json(messages(&batch)))
}

#[plugin_fn]
pub fn on_items(Json(batch): Json<ItemBatch>) -> FnResult<Json<Effects>> {
    Ok(Json(items(&batch)))
}

#[plugin_fn]
pub fn on_command(Json(call): Json<CommandCall>) -> FnResult<Json<Effects>> {
    Ok(Json(command(&call)))
}
