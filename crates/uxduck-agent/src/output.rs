//! What the model reads of a tool's result. The host records the result
//! as the daemon gave it, for the chat and its tool card, and MCP clients
//! get it whole. The model keeps every result in its chat for the rest of
//! the chat, so it gets each one smaller: without null fields, with
//! numbers no longer than they mean, and without fields it has no use for
//! or already has.

use serde_json::{Map, Value, json};

/// A successful tool result as the model reads it. Errors are not passed
/// through here: the model gets them as they are.
#[must_use]
pub fn for_model(method: &str, mut result: Value) -> Value {
    trim(method, &mut result);
    compact(result)
}

/// Leaves out null fields and writes each floating-point number short. An
/// `f32` widened reads as the shortest number that is the same `f32`, as
/// `show_value` reads a parameter, so the model gives back the value the
/// vehicle holds: `0.10000000149011612` reads `0.1`, and a tuned gain of
/// `0.00312674` or `4e-8` reads as it is. Any other number is rounded to
/// 7 decimal places, so a coordinate keeps about a centimetre, and a whole
/// number such as `1500.0` reads `1500`. Integers, empty arrays and
/// `false` stay, since they can mean something, and so do nulls in arrays,
/// whose places count.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(fields) => fields
            .into_iter()
            .filter(|(_, field)| !field.is_null())
            .map(|(name, field)| (name, compact(field)))
            .collect(),
        Value::Array(items) => items.into_iter().map(compact).collect(),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::float_cmp,
                    reason = "an f32 widened reads as the f32 it was"
                )]
                let rounded: f64 = if f64::from(float as f32) == float {
                    (float as f32).to_string().parse().unwrap_or(float)
                } else {
                    format!("{float:.7}").parse().unwrap_or(float)
                };
                #[allow(
                    clippy::float_cmp,
                    clippy::cast_possible_truncation,
                    reason = "a whole number within f64's exact integers"
                )]
                let whole = (rounded == rounded.trunc() && rounded.abs() < 9_007_199_254_740_992.0)
                    .then_some(rounded as i64);
                whole.map_or_else(|| json!(rounded), Value::from)
            }
            _ => Value::Number(number),
        },
        other => other,
    }
}

/// The objects in one of a result's lists.
fn entries<'a>(
    result: &'a mut Value,
    list: &str,
) -> impl Iterator<Item = &'a mut Map<String, Value>> {
    result
        .get_mut(list)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

/// Leaves out what the model has no use for in the results it reads most,
/// or has already: the call named the vehicle.
fn trim(method: &str, result: &mut Value) {
    match method {
        // One item per key: which component and message sent it is the
        // app's to know.
        "telemetry/get" => {
            for item in entries(result, "items") {
                item.remove("componentId");
                item.remove("source");
            }
        }
        "vehicle/messages" => {
            for message in entries(result, "messages") {
                message.remove("vehicleId");
            }
        }
        // The answer names the vehicle once, and an armed entry's checklist
        // record is its flight's too (`flight/list`): its text sums it up.
        "vehicle/history" => {
            for entry in entries(result, "entries") {
                entry.remove("vehicleId");
                if entry.get("writtenAtUs") == entry.get("happenedAtUs") {
                    entry.remove("writtenAtUs");
                }
                if let Some(event) = entry
                    .get_mut("kind")
                    .and_then(|kind| kind.get_mut("event"))
                    .and_then(Value::as_object_mut)
                    && event.get("type").and_then(Value::as_str) == Some("armed")
                {
                    event.remove("record");
                }
            }
        }
        // How old a vehicle's state may be to send one is the daemon's
        // concern. Most commands are as risky in flight as on the ground, a
        // safety command is allowed offline and most others warn, and a mode's
        // label only names it (the model's command_list says so).
        "command/list" => {
            for command in entries(result, "commands") {
                command.remove("maxAgeMs");
                if command.get("riskInFlight") == command.get("risk") {
                    command.remove("riskInFlight");
                }
                let usual = if command.get("class") == Some(&json!("safety")) {
                    "allow"
                } else {
                    "warn"
                };
                if command.get("offlineBehaviour") == Some(&json!(usual)) {
                    command.remove("offlineBehaviour");
                }
                let named = command
                    .get("command")
                    .and_then(|sent| sent.get("mode"))
                    .and_then(Value::as_str)
                    .map(|mode| json!(format!("Set mode {mode}")));
                if named.is_some() && command.get("label") == named.as_ref() {
                    command.remove("label");
                }
            }
        }
        // An exercise the vehicle cannot run needs no steps: what it is and
        // why not. The app shows a step's hint when it is due: the model
        // needs what it says.
        "practice/list" => {
            for exercise in entries(result, "exercises") {
                if exercise.contains_key("unavailable") {
                    exercise.retain(|name, _| {
                        matches!(
                            name.as_str(),
                            "id" | "title" | "goal" | "vehicles" | "unavailable"
                        )
                    });
                }
                if let Some(steps) = exercise.get_mut("steps").and_then(Value::as_array_mut) {
                    for step in steps.iter_mut().filter_map(Value::as_object_mut) {
                        step.remove("hintAfterS");
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(
    clippy::unreadable_literal,
    reason = "numbers as a vehicle sent them, to compare at a glance"
)]
mod tests {
    use serde_json::json;

    use super::{compact, for_model};

    #[test]
    fn null_fields_are_left_out_and_everything_else_stays() {
        assert_eq!(
            compact(json!({
                "value": null,
                "text": "RTK fixed",
                "nested": { "gone": null, "kept": false },
                "empty": [],
                "list": [null, 1, { "x": null }],
                "armed": false,
                "zero": 0,
            })),
            json!({
                "text": "RTK fixed",
                "nested": { "kept": false },
                "empty": [],
                "list": [null, 1, {}],
                "armed": false,
                "zero": 0,
            })
        );
    }

    #[test]
    fn float_noise_is_trimmed_to_its_shortest_form() {
        let compacted = compact(json!({
            "widened": 0.10000000149011612,
            "volts": 14.199999809265137,
            "hover": 0.37499067187309265,
            "roll": 0.071786335383572,
            "yaw": 354.26306713023877,
            "tiny": 0.000_000_04,
            "negative": -0.000_123_456_78,
            "whole": 1500.0,
            "minusZero": -0.0,
            "huge": 1.0e20,
        }));
        assert_eq!(
            compacted,
            json!({
                "widened": 0.1,
                "volts": 14.2,
                "hover": 0.37499067,
                "roll": 0.0717863,
                "yaw": 354.2630671,
                "tiny": 0,
                "negative": -0.0001235,
                "whole": 1500,
                "minusZero": 0,
                "huge": 1.0e20,
            })
        );
        assert_eq!(
            compacted.to_string(),
            r#"{"hover":0.37499067,"huge":1e+20,"minusZero":0,"negative":-0.0001235,"roll":0.0717863,"tiny":0,"volts":14.2,"whole":1500,"widened":0.1,"yaw":354.2630671}"#
        );
    }

    #[test]
    fn a_tuned_gain_reads_as_the_vehicle_holds_it() {
        let held = [
            0.003_126_74_f32,
            0.000_000_04,
            0.000_123_456_78,
            1.5e-5,
            0.135,
            -0.0001,
        ];
        assert_eq!(
            compact(json!(held.map(f64::from))).to_string(),
            "[0.00312674,4e-8,0.00012345678,0.000015,0.135,-0.0001]"
        );
    }

    /// The model gives a parameter's value back as `param/set`'s
    /// `expectedValue`, which the engine compares with the value it holds.
    #[test]
    fn a_parameter_read_is_given_back_as_the_value_the_engine_holds() {
        use uxduck_core::domain::parameter::same_value;
        let given_back = |held: f64| {
            let read = for_model(
                "param/get",
                json!({ "parameters": [{ "name": "ATC_RAT_RLL_D", "value": held }] }),
            );
            let as_sent: serde_json::Value = serde_json::from_str(&read.to_string()).unwrap();
            as_sent["parameters"][0]["value"].as_f64().unwrap()
        };
        let tuned = [
            0.003_126_74_f32,
            0.000_000_04,
            1e-45,
            f32::MIN_POSITIVE,
            f32::MAX,
            -0.0,
            1500.0,
        ];
        let across = (0..=u32::MAX)
            .step_by(65_537)
            .map(f32::from_bits)
            .filter(|held| held.is_finite());
        for held in tuned.into_iter().chain(across).map(f64::from) {
            assert!(same_value(given_back(held), held), "{held:e}");
        }
    }

    #[test]
    fn coordinates_keep_seven_decimal_places() {
        let compacted = compact(json!({
            "latitudeDeg": -35.363_261_412_345_67,
            "longitudeDeg": 149.165_229_6,
            "altitudeM": 614.05,
        }));
        assert_eq!(
            compacted.to_string(),
            r#"{"altitudeM":614.05,"latitudeDeg":-35.3632614,"longitudeDeg":149.1652296}"#
        );
    }

    #[test]
    fn integers_are_untouched() {
        let integers = json!({
            "timeUs": 1_794_639_263_091_431_u64,
            "big": u64::MAX,
            "negative": i64::MIN,
            "count": 3,
        });
        assert_eq!(compact(integers.clone()), integers);
    }

    #[test]
    fn the_results_read_most_lose_what_the_model_has_no_use_for() {
        let telemetry = for_model(
            "telemetry/get",
            json!({
                "vehicleId": "v1",
                "items": [{ "key": "battery.voltage_v", "label": "Battery", "unit": "volt",
                    "value": 12.600000381469727, "source": "power", "componentId": 1,
                    "timeUs": 1_794_639_263_091_431_u64 }],
                "positions": [{ "source": "gps", "componentId": 1, "latitudeDeg": -35.3632616 }],
            }),
        );
        assert_eq!(
            telemetry,
            json!({
                "vehicleId": "v1",
                "items": [{ "key": "battery.voltage_v", "label": "Battery", "unit": "volt",
                    "value": 12.6, "timeUs": 1_794_639_263_091_431_u64 }],
                "positions": [{ "source": "gps", "componentId": 1, "latitudeDeg": -35.3632616 }],
            })
        );

        let messages = for_model(
            "vehicle/messages",
            json!({ "messages": [{ "vehicleId": "v1", "text": "PreArm: Battery 1 low voltage failsafe",
                "severity": 2, "count": 2, "rate": null }] }),
        );
        assert_eq!(
            messages,
            json!({ "messages": [{ "text": "PreArm: Battery 1 low voltage failsafe", "severity": 2, "count": 2 }] })
        );

        let history = for_model(
            "vehicle/history",
            json!({ "vehicleId": "v1", "total": 2, "entries": [
                { "vehicleId": "v1", "happenedAtUs": 5, "writtenAtUs": 5, "kind": { "type": "event",
                    "event": { "type": "armed", "text": "Armed with its checklist: 4 of 6 steps passed",
                        "flightId": "f1", "record": { "steps": [] } } } },
                { "vehicleId": "v1", "happenedAtUs": 3, "writtenAtUs": 4, "kind": { "type": "note",
                    "text": "Replaced the propellers", "record": "kept" } },
            ] }),
        );
        assert_eq!(
            history,
            json!({ "vehicleId": "v1", "total": 2, "entries": [
                { "happenedAtUs": 5, "kind": { "type": "event",
                    "event": { "type": "armed", "text": "Armed with its checklist: 4 of 6 steps passed",
                        "flightId": "f1" } } },
                { "happenedAtUs": 3, "writtenAtUs": 4, "kind": { "type": "note",
                    "text": "Replaced the propellers", "record": "kept" } },
            ] })
        );
    }

    #[test]
    fn commands_and_exercises_lose_what_the_model_is_told_or_has_no_use_for() {
        let commands = for_model(
            "command/list",
            json!({ "commands": [
                { "class": "critical", "label": "Arm", "maxAgeMs": 5000, "offlineBehaviour": "block",
                    "risk": { "level": "confirm" }, "riskInFlight": { "level": "confirm" } },
                { "class": "critical", "label": "Disarm", "maxAgeMs": 5000, "offlineBehaviour": "allow",
                    "risk": { "level": "none" },
                    "riskInFlight": { "level": "dangerous", "reason": "It falls." } },
                { "class": "critical", "label": "Take off", "offlineBehaviour": "warn" },
                { "class": "mission", "command": { "type": "set-mode", "mode": "FBWB" },
                    "label": "Set mode FBWB", "offlineBehaviour": "warn" },
                { "class": "mission", "command": { "type": "set-mode", "mode": "CRUISE" },
                    "label": "Cruise", "offlineBehaviour": "allow" },
                { "class": "safety", "command": { "type": "set-mode", "mode": "QRTL" },
                    "label": "Set mode QRTL", "offlineBehaviour": "allow" },
                { "class": "safety", "label": "Return", "offlineBehaviour": "warn" },
            ] }),
        );
        assert_eq!(
            commands,
            json!({ "commands": [
                { "class": "critical", "label": "Arm", "offlineBehaviour": "block",
                    "risk": { "level": "confirm" } },
                { "class": "critical", "label": "Disarm", "offlineBehaviour": "allow",
                    "risk": { "level": "none" },
                    "riskInFlight": { "level": "dangerous", "reason": "It falls." } },
                { "class": "critical", "label": "Take off" },
                { "class": "mission", "command": { "type": "set-mode", "mode": "FBWB" } },
                { "class": "mission", "command": { "type": "set-mode", "mode": "CRUISE" },
                    "label": "Cruise", "offlineBehaviour": "allow" },
                { "class": "safety", "command": { "type": "set-mode", "mode": "QRTL" } },
                { "class": "safety", "label": "Return", "offlineBehaviour": "warn" },
            ] })
        );

        let practice = for_model(
            "practice/list",
            json!({ "exercises": [
                { "id": "mode-tour", "title": "Mode tour", "goal": "Loiter, then Land.",
                    "vehicles": ["multicopter"], "steps": [{ "text": "Choose Loiter.",
                        "hint": "Loiter is in the mode list.", "hintAfterS": 60.0 }] },
                { "id": "mode-tour-vtol", "title": "Mode tour", "goal": "Loiter, then Land.",
                    "vehicles": ["vtol"], "audience": "first-flight", "steps": [{ "text": "Choose QLoiter." }],
                    "unavailable": "This exercise is for a simulated VTOL plane." },
            ] }),
        );
        assert_eq!(
            practice,
            json!({ "exercises": [
                { "id": "mode-tour", "title": "Mode tour", "goal": "Loiter, then Land.",
                    "vehicles": ["multicopter"], "steps": [{ "text": "Choose Loiter.",
                        "hint": "Loiter is in the mode list." }] },
                { "id": "mode-tour-vtol", "title": "Mode tour", "goal": "Loiter, then Land.",
                    "vehicles": ["vtol"], "unavailable": "This exercise is for a simulated VTOL plane." },
            ] })
        );
    }

    #[test]
    fn other_results_are_only_compacted() {
        let vehicle = json!({ "id": "v1", "commandAvailable": true, "componentIds": [1],
            "source": "kept", "maxAgeMs": 5000, "vehicleId": "v1", "mode": null });
        assert_eq!(
            for_model("vehicle/get", vehicle),
            json!({ "id": "v1", "commandAvailable": true, "componentIds": [1],
                "source": "kept", "maxAgeMs": 5000, "vehicleId": "v1" })
        );
    }
}
