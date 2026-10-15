//! Write plugins in Rust (ADR-0006, plugin ABI v2).
//!
//! A plugin is a WebAssembly module with these functions, each taking and
//! returning JSON:
//!
//! - `describe` → [`Description`]: its panels.
//! - `on_items` ([`ItemBatch`]) → [`Effects`]: the telemetry items its
//!   `plugin.json` subscribes to, by key or `prefix.*`, already in units.
//! - `on_messages` ([`MessageBatch`]) → [`Effects`]: MAVLink messages by
//!   name, decoded. Read one as its type with [`Message::from_input`]:
//!   [`messages::GlobalPositionInt`] and every other message of the
//!   ArduPilot dialect, with MAVLink's own field names and raw units.
//! - `on_frames` ([`FrameBatch`]) → [`Effects`]: raw frames by id, for
//!   messages the dialect does not have; [`payload`] decodes one.
//! - `on_command` ([`CommandCall`]) → [`Effects`]: one of the commands its
//!   manifest declares, with its parameters.
//!
//! Each subscription has its own cadence in the manifest. [`Effects`] are
//! what the plugin wants done: commands for vehicles, sent as the plugin
//! through the ordinary command path; messages by name ([`Message::send_to`]),
//! only those its manifest lists in `sends`; telemetry items with a unit,
//! whose key ends in the unit's suffix (`home.distance_m`); notifications;
//! and log lines. The types are the contracts' own.
//!
//! ```ignore
//! use extism_pdk::{FnResult, Json, plugin_fn};
//! use uxduck_sdk::messages::GlobalPositionInt;
//! use uxduck_sdk::{Effects, Message, MessageBatch};
//!
//! #[plugin_fn]
//! pub fn on_messages(Json(batch): Json<MessageBatch>) -> FnResult<Json<Effects>> {
//!     for input in &batch.messages {
//!         if let Some(position) = GlobalPositionInt::from_input(input) {
//!             // relative_alt is in millimetres: GlobalPositionInt::unit("relative_alt")
//!             let _height_m = f64::from(position.relative_alt) / 1000.0;
//!         }
//!     }
//!     Ok(Json(Effects::default()))
//! }
//! ```

use base64::Engine as _;
use serde_json::{Map, Value};

#[cfg(target_arch = "wasm32")]
pub use extism_pdk;
pub use uxduck_contracts::dto::command::{Command, Risk};
pub use uxduck_contracts::dto::notification::Severity;
pub use uxduck_contracts::dto::plugin::{
    CommandCall, Description, Effects, Frame, FrameBatch, ItemBatch, ItemInput, MapFeature,
    MessageBatch, MessageInput, Panel, PluginNotification, ProducedItem, SendCommand, SendMessage,
    Tone, Widget,
};
pub use uxduck_contracts::dto::telemetry::Unit;

/// A MAVLink message of the ArduPilot dialect as a type (X3).
pub trait Message: serde::Serialize + serde::de::DeserializeOwned {
    /// MAVLink's name, such as `GLOBAL_POSITION_INT`.
    const NAME: &'static str;
    const ID: u32;
    /// Each field that has a unit, as MAVLink defines it, such as
    /// `("relative_alt", "mm")`.
    const UNITS: &'static [(&'static str, &'static str)];

    /// This message, if the input is one.
    #[must_use]
    fn from_input(input: &MessageInput) -> Option<Self> {
        if input.name != Self::NAME {
            return None;
        }
        serde_json::from_value(Value::Object(input.fields.clone())).ok()
    }

    /// An effect that sends it to a vehicle, encoded by the host.
    #[must_use]
    fn send_to(&self, vehicle_id: &str) -> SendMessage {
        let fields = match serde_json::to_value(self) {
            Ok(Value::Object(fields)) => fields,
            _ => Map::new(),
        };
        SendMessage {
            vehicle_id: vehicle_id.to_owned(),
            name: Self::NAME.to_owned(),
            fields,
        }
    }

    /// A field's unit, such as `mm`.
    #[must_use]
    fn unit(field: &str) -> Option<&'static str> {
        Self::UNITS
            .iter()
            .find(|(name, _)| *name == field)
            .map(|(_, unit)| *unit)
    }
}

/// Every message of the ArduPilot dialect, generated from its definitions.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    nonstandard_style,
    rustdoc::all,
    reason = "generated from MAVLink's definitions"
)]
pub mod messages {
    /// MAVLink's NaN arrives as JSON null.
    pub(crate) fn float<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::Deserialize<'de> + Nan,
    {
        Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_else(T::nan))
    }

    pub(crate) fn floats<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::Deserialize<'de> + Nan,
    {
        Ok(Vec::<Option<T>>::deserialize(deserializer)?
            .into_iter()
            .map(|v| v.unwrap_or_else(T::nan))
            .collect())
    }

    pub trait Nan {
        fn nan() -> Self;
    }
    impl Nan for f32 {
        fn nan() -> Self {
            f32::NAN
        }
    }
    impl Nan for f64 {
        fn nan() -> Self {
            f64::NAN
        }
    }

    use serde::Deserialize as _;

    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

/// A raw frame's payload. MAVLink 2 drops trailing zeros: read missing
/// bytes as zero.
#[must_use]
pub fn payload(frame: &Frame) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(&frame.payload)
        .unwrap_or_default()
}

/// A `MAV_CMD` for a vehicle, by number.
#[must_use]
pub fn command_long(vehicle_id: &str, command: u16, params: &[f32]) -> SendCommand {
    SendCommand {
        vehicle_id: vehicle_id.to_owned(),
        command: Command::Long {
            command,
            params: params.to_vec(),
        },
    }
}

/// A numeric telemetry item for a vehicle; its key ends in its unit's
/// suffix, such as `home.distance_m` for metres.
#[must_use]
pub fn value(vehicle_id: &str, key: &str, label: &str, unit: Unit, value: f64) -> ProducedItem {
    ProducedItem {
        vehicle_id: Some(vehicle_id.to_owned()),
        key: key.to_owned(),
        label: label.to_owned(),
        unit,
        value: Some(value),
        text: None,
        rate_per_s: None,
    }
}

/// A text telemetry item for a vehicle.
#[must_use]
pub fn text(vehicle_id: &str, key: &str, label: &str, text: &str) -> ProducedItem {
    ProducedItem {
        vehicle_id: Some(vehicle_id.to_owned()),
        key: key.to_owned(),
        label: label.to_owned(),
        unit: Unit::None,
        value: None,
        text: Some(text.to_owned()),
        rate_per_s: None,
    }
}

/// Rows for a `table` widget: an item whose text holds them.
#[must_use]
pub fn table(vehicle_id: &str, key: &str, label: &str, rows: &[Vec<String>]) -> ProducedItem {
    text(
        vehicle_id,
        key,
        label,
        &serde_json::to_string(rows).unwrap_or_else(|_| "[]".into()),
    )
}

/// A point on the map, at a latitude and longitude in degrees.
#[must_use]
pub fn point(id: &str, latitude_deg: f64, longitude_deg: f64) -> MapFeature {
    MapFeature {
        id: id.to_owned(),
        geometry: serde_json::json!({ "type": "Point", "coordinates": [longitude_deg, latitude_deg] }),
        label: None,
        tone: None,
        fill: false,
        dashed: false,
    }
}

/// A circle on the map: a polygon of 64 sides around a centre in degrees,
/// `radius_m` metres across the ground.
#[must_use]
pub fn circle(id: &str, latitude_deg: f64, longitude_deg: f64, radius_m: f64) -> MapFeature {
    const EARTH_M: f64 = 6_371_000.0;
    const SIDES: u32 = 64;
    let lat = latitude_deg.to_radians();
    let ring: Vec<Value> = (0..=SIDES)
        .map(|n| {
            let angle = f64::from(n % SIDES) / f64::from(SIDES) * std::f64::consts::TAU;
            let north = radius_m * angle.cos() / EARTH_M;
            let east = radius_m * angle.sin() / (EARTH_M * lat.cos());
            serde_json::json!([
                longitude_deg + east.to_degrees(),
                latitude_deg + north.to_degrees()
            ])
        })
        .collect();
    MapFeature {
        id: id.to_owned(),
        geometry: serde_json::json!({ "type": "Polygon", "coordinates": [ring] }),
        label: None,
        tone: None,
        fill: false,
        dashed: false,
    }
}

/// A notification about a vehicle, under the plugin's name.
#[must_use]
pub fn notify(vehicle_id: &str, severity: Severity, title: &str) -> PluginNotification {
    PluginNotification {
        vehicle_id: Some(vehicle_id.to_owned()),
        severity,
        title: title.to_owned(),
        body: None,
    }
}

#[cfg(test)]
mod tests {
    use super::messages::{CommandAck, GlobalPositionInt, Heartbeat};
    use super::*;

    #[test]
    fn a_decoded_message_reads_as_its_type_with_its_units() {
        let mut fields = Map::new();
        fields.insert("command".into(), Value::from(2000));
        fields.insert("result".into(), Value::from(0));
        let input = MessageInput {
            vehicle_id: "v".into(),
            system_id: 1,
            component_id: 1,
            name: "COMMAND_ACK".into(),
            fields,
        };
        let ack = CommandAck::from_input(&input).expect("an ack");
        assert_eq!((ack.command, ack.result), (2000, 0));
        assert!(Heartbeat::from_input(&input).is_none(), "another message");
        assert_eq!(GlobalPositionInt::unit("relative_alt"), Some("mm"));
        assert_eq!(GlobalPositionInt::NAME, "GLOBAL_POSITION_INT");
        assert_eq!(GlobalPositionInt::ID, 33);
        let heartbeat = Heartbeat {
            r#type: 6,
            ..Heartbeat::default()
        };
        let sent = heartbeat.send_to("v");
        assert_eq!(sent.name, "HEARTBEAT");
        assert_eq!(sent.fields["type"], 6);
    }

    #[test]
    fn every_message_the_host_decodes_reads_as_its_type_and_back() {
        use uxduck_mavlink::named;
        for name in named::names() {
            // Every field zero but text, which the host gives as "".
            let decoded = named::decode(named::id_of(name).expect("known"), &[]).expect(name);
            let back = messages::round_trip(name, decoded.fields.clone())
                .unwrap_or_else(|| panic!("{name} does not read as its type"));
            assert_eq!(back, Value::Object(decoded.fields), "{name}");
        }
    }

    #[test]
    fn a_circle_is_closed_and_its_radius_across_the_ground() {
        let feature = circle("c", -35.0, 149.0, 100.0);
        let ring = feature.geometry["coordinates"][0].as_array().unwrap();
        assert_eq!(ring.first(), ring.last(), "closed");
        assert_eq!(ring.len(), 65);
        // Due north, 100 m: about 0.0009 degrees of latitude.
        let north = ring[0][1].as_f64().unwrap() - -35.0;
        assert!((north - 0.000_899).abs() < 0.000_001, "{north}");
        let rows = table("v", "home.marks", "Marks", &[vec!["1".into(), "A".into()]]);
        assert_eq!(rows.text.as_deref(), Some(r#"[["1","A"]]"#));
    }

    #[test]
    fn a_float_that_was_nan_reads_as_nan() {
        let mut fields = Map::new();
        fields.insert("voltage_battery".into(), Value::from(12_000));
        let mut status = fields.clone();
        status.insert("roll".into(), Value::Null);
        let input = MessageInput {
            vehicle_id: "v".into(),
            system_id: 1,
            component_id: 1,
            name: "ATTITUDE".into(),
            fields: status,
        };
        let attitude = messages::Attitude::from_input(&input).expect("attitude");
        assert!(attitude.roll.is_nan());
        assert_eq!(
            payload(&Frame {
                vehicle_id: "v".into(),
                message_id: 77,
                system_id: 1,
                component_id: 1,
                payload: "0Ac=".into(),
            }),
            vec![0xD0, 0x07]
        );
    }
}
