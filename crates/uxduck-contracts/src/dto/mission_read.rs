use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissionReadFloat(u64);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NonFiniteMissionFloat {
    non_finite_f64: String,
}

impl JsonSchema for NonFiniteMissionFloat {
    fn schema_name() -> Cow<'static, str> {
        "NonFiniteMissionFloat".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["nonFiniteF64"],
            "properties": {
                "nonFiniteF64": {
                    "type": "string",
                    "minLength": 16,
                    "maxLength": 16,
                    "pattern": "^(?:7ff|fff)[0-9a-f]{13}$"
                }
            }
        })
    }
}

impl JsonSchema for MissionReadFloat {
    fn schema_name() -> Cow<'static, str> {
        "MissionReadFloat".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let tag = generator.subschema_for::<NonFiniteMissionFloat>();
        json_schema!({
            "anyOf": [
                { "type": "number", "minimum": -f64::MAX, "maximum": f64::MAX },
                tag
            ]
        })
    }
}

impl From<f64> for MissionReadFloat {
    fn from(value: f64) -> Self {
        Self(value.to_bits())
    }
}

impl Serialize for MissionReadFloat {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = f64::from_bits(self.0);
        if value.is_finite() {
            return serializer.serialize_f64(value);
        }
        NonFiniteMissionFloat {
            non_finite_f64: format!("{:016x}", self.0),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MissionReadFloat {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Number(f64),
            Tag(NonFiniteMissionFloat),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Number(value) if value.is_finite() => Ok(value.into()),
            Wire::Number(_) => Err(D::Error::custom("a mission read number must be finite")),
            Wire::Tag(tag) => {
                let text = tag.non_finite_f64;
                if text.len() != 16
                    || !text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(D::Error::custom(
                        "a mission read tag needs 16 lowercase hex digits",
                    ));
                }
                let bits = u64::from_str_radix(&text, 16).map_err(D::Error::custom)?;
                if f64::from_bits(bits).is_finite() {
                    return Err(D::Error::custom(
                        "finite mission reads must use JSON numbers",
                    ));
                }
                Ok(Self(bits))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadMissionItem {
    pub command: u16,
    pub frame: u8,
    pub param1: MissionReadFloat,
    pub param2: MissionReadFloat,
    pub param3: MissionReadFloat,
    pub param4: MissionReadFloat,
    pub latitude_deg: MissionReadFloat,
    pub longitude_deg: MissionReadFloat,
    pub altitude_m: MissionReadFloat,
    pub autocontinue: bool,
}

/// Validates a mission read without changing its values.
///
/// # Errors
/// Returns `invalid-value` for a malformed result, retaining scalar paths and causes.
pub fn validate_mission_read_result(
    method: &str,
    result: &serde_json::Value,
) -> Result<(), crate::errors::ApiError> {
    check_mission_read_result(method, result).map_err(|failure| *failure.error)
}

struct MissionReadFailure {
    error: Box<crate::errors::ApiError>,
    representation: bool,
}

fn mission_float_failure(value: &serde_json::Value, path: &str) -> Option<(String, String)> {
    for part in ["items", "fence", "rally"] {
        let Some(items) = value.get(part).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            let Some(item) = item.as_object() else {
                continue;
            };
            for field in [
                "param1",
                "param2",
                "param3",
                "param4",
                "latitudeDeg",
                "longitudeDeg",
                "altitudeM",
            ] {
                let cause = item.get(field).map_or_else(
                    || Some(format!("missing field `{field}`")),
                    |value| {
                        serde_json::from_value::<MissionReadFloat>(value.clone())
                            .err()
                            .map(|error| error.to_string())
                    },
                );
                if let Some(cause) = cause {
                    return Some((format!("{path}.{part}[{index}].{field}"), cause));
                }
            }
        }
    }
    None
}

fn check_mission_read_result(
    method: &str,
    result: &serde_json::Value,
) -> Result<(), MissionReadFailure> {
    use super::mission::{Mission, MissionHistoryResult};

    let invalid = |path: &str, cause: String, representation: bool| MissionReadFailure {
        error: Box::new(crate::errors::ApiError::new(
            crate::errors::ErrorKind::InvalidValue,
            format!("Invalid mission-read result for {method} at {path}: {cause}"),
        )),
        representation,
    };
    let mission = |value: &serde_json::Value, path: &str| {
        if let Some((path, cause)) = mission_float_failure(value, path) {
            return Err(invalid(&path, cause, true));
        }
        serde_json::from_value::<Mission>(value.clone())
            .map(|_| ())
            .map_err(|error| invalid(path, error.to_string(), false))
    };
    match method {
        "mission/refresh" => mission(result, "result"),
        "mission/history" => {
            if let Some(records) = result.get("records").and_then(serde_json::Value::as_array) {
                for (index, record) in records.iter().enumerate() {
                    if let Some((path, cause)) =
                        mission_float_failure(record, &format!("result.records[{index}]"))
                    {
                        return Err(invalid(&path, cause, true));
                    }
                }
            }
            serde_json::from_value::<MissionHistoryResult>(result.clone())
                .map(|_| ())
                .map_err(|error| invalid("result", error.to_string(), false))
        }
        "mission/upload" | "mission/restore" | "plan/upload" => mission(
            result
                .get("mission")
                .ok_or_else(|| invalid("result.mission", "missing mission result".into(), false))?,
            "result.mission",
        ),
        "state/subscribe" => {
            if let Some(missions) = result.get("snapshot").and_then(|s| s.get("missions")) {
                if let Some(missions) = missions.as_array() {
                    for (index, item) in missions.iter().enumerate() {
                        if let Some((path, cause)) = mission_float_failure(
                            item,
                            &format!("result.snapshot.missions[{index}]"),
                        ) {
                            return Err(invalid(&path, cause, true));
                        }
                    }
                }
                serde_json::from_value::<Vec<Mission>>(missions.clone())
                    .map(|_| ())
                    .map_err(|error| invalid("result.snapshot.missions", error.to_string(), false))
            } else {
                Ok(())
            }
        }
        "state/delta" => {
            let changes = result
                .get("changes")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| invalid("params.changes", "missing delta changes".into(), false))?;
            for (index, change) in changes.iter().enumerate() {
                if change["entity"] == "mission"
                    && change["op"] == "upsert"
                    && let Some((path, cause)) = mission_float_failure(
                        &change["mission"],
                        &format!("params.changes[{index}].mission"),
                    )
                {
                    return Err(invalid(&path, cause, true));
                }
            }
            for (index, change) in changes.iter().enumerate() {
                if change["entity"] == "mission" && change["op"] == "upsert" {
                    let path = format!("params.changes[{index}].mission");
                    mission(
                        change
                            .get("mission")
                            .ok_or_else(|| invalid(&path, "missing mission delta".into(), false))?,
                        &path,
                    )?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[must_use]
pub fn mission_read_request(method: &str, params: &serde_json::Value) -> bool {
    matches!(
        method,
        "mission/refresh"
            | "mission/history"
            | "mission/upload"
            | "mission/restore"
            | "plan/upload"
    ) || (method == "state/subscribe"
        && params["entities"]
            .as_array()
            .is_some_and(|entities| entities.iter().any(|entity| entity == "mission")))
        || (method == "state/delta"
            && params["changes"]
                .as_array()
                .is_some_and(|changes| changes.iter().any(|change| change["entity"] == "mission")))
}

#[derive(Default)]
pub struct MissionReadSession {
    unavailable: Option<crate::errors::ApiError>,
}

impl std::fmt::Debug for MissionReadSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MissionReadSession")
            .field("quarantined", &self.unavailable.is_some())
            .finish_non_exhaustive()
    }
}

impl MissionReadSession {
    /// Checks the connection's mission-read quarantine before a request is sent.
    ///
    /// # Errors
    /// Returns the latched `update-required` diagnostic for mission-bearing requests.
    pub fn before_request(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(), crate::errors::ApiError> {
        if mission_read_request(method, params)
            && let Some(error) = &self.unavailable
        {
            return Err(error.clone());
        }
        Ok(())
    }

    /// Validates a successful reply without changing connection authority.
    ///
    /// # Errors
    /// Representation failures latch `update-required`; other validation errors keep their kind.
    pub fn accept_result(
        &mut self,
        method: &str,
        mission_bearing: bool,
        result: &serde_json::Value,
    ) -> Result<(), crate::errors::ApiError> {
        if mission_bearing && let Some(error) = &self.unavailable {
            return Err(error.clone());
        }
        if let Err(failure) = check_mission_read_result(method, result) {
            if !failure.representation {
                return Err(*failure.error);
            }
            let cause = failure.error;
            let error = crate::errors::ApiError::new(
                crate::errors::ErrorKind::UpdateRequired,
                format!(
                    "Mission reads are unavailable on this connection. A compatible peer and client must provide a safely readable, exact mission representation; no numeric default was substituted. Original validator error (code {}): {}",
                    cause.code, cause.message,
                ),
            );
            self.unavailable = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "wire codec fixtures")]
    use super::{MissionReadFloat, MissionReadSession, validate_mission_read_result};
    use crate::errors::{ApiError, ErrorKind};
    use serde_json::{Value, json};

    fn golden_result(method: &str) -> Value {
        let golden: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/goldens/mission-read.json"
        )))
        .unwrap();
        let frame: Value =
            serde_json::from_str(golden["frames"][method].as_str().unwrap()).unwrap();
        frame[if method == "state/delta" {
            "params"
        } else {
            "result"
        }]
        .clone()
    }

    #[test]
    fn quarantine_retains_the_low_level_cause_and_path_for_every_read_scalar() {
        for (method, pointer, path) in [
            ("mission/refresh", "", "result"),
            ("mission/history", "/records/0", "result.records[0]"),
            ("mission/upload", "/mission", "result.mission"),
            ("mission/restore", "/mission", "result.mission"),
            ("plan/upload", "/mission", "result.mission"),
            (
                "state/subscribe",
                "/snapshot/missions/0",
                "result.snapshot.missions[0]",
            ),
            (
                "state/delta",
                "/changes/0/mission",
                "params.changes[0].mission",
            ),
        ] {
            let good = golden_result(method);
            for part in ["items", "fence", "rally"] {
                for field in [
                    "param1",
                    "param2",
                    "param3",
                    "param4",
                    "latitudeDeg",
                    "longitudeDeg",
                    "altitudeM",
                ] {
                    let mut broken = good.clone();
                    *broken
                        .pointer_mut(&format!("{pointer}/{part}/0/{field}"))
                        .unwrap() = Value::Null;
                    let low = validate_mission_read_result(method, &broken).unwrap_err();
                    assert_eq!(low.kind(), ErrorKind::InvalidValue);
                    assert!(
                        low.message.contains(&format!("{path}.{part}[0].{field}")),
                        "{}",
                        low.message
                    );
                    let mut session = MissionReadSession::default();
                    let error = session.accept_result(method, true, &broken).unwrap_err();
                    assert_eq!(error.kind(), ErrorKind::UpdateRequired);
                    assert_eq!(error.code, ErrorKind::UpdateRequired.code());
                    assert!(error.message.contains(&low.message));
                    assert!(error.message.contains("compatible peer and client"));
                    assert!(!error.message.contains("Update Core"));
                    assert!(!error.message.contains("old Core"));
                    assert_eq!(
                        session
                            .before_request("mission/history", &json!({}))
                            .unwrap_err(),
                        error
                    );
                    assert!(session.before_request("vehicle/list", &json!({})).is_ok());
                }
            }
        }
    }

    #[test]
    fn mixed_delta_failures_cannot_mask_representation_quarantine_in_either_order() {
        for scalar_first in [false, true] {
            let mut metadata = golden_result("state/delta")["changes"][0].clone();
            metadata["mission"]["version"] = json!("bad metadata");
            let mut scalar = golden_result("state/delta")["changes"][0].clone();
            scalar["mission"]["items"][0]["param4"] = Value::Null;
            let changes = if scalar_first {
                vec![scalar, metadata]
            } else {
                vec![metadata, scalar]
            };
            let mut session = MissionReadSession::default();
            let invalid = json!({ "changes": changes });
            let low = validate_mission_read_result("state/delta", &invalid).unwrap_err();
            assert!(low.message.contains("param4"), "{}", low.message);
            let error = session
                .accept_result("state/delta", true, &invalid)
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UpdateRequired);
            assert!(error.message.contains(&low.message));
            assert!(
                session
                    .before_request("mission/history", &json!({}))
                    .is_err()
            );
            assert!(session.before_request("vehicle/list", &json!({})).is_ok());
        }
    }

    #[test]
    fn metadata_and_nonmission_errors_keep_their_kind_without_quarantine() {
        let mut session = MissionReadSession::default();
        let mut invalid = golden_result("mission/refresh");
        invalid["version"] = json!("invalid metadata");
        let low = validate_mission_read_result("mission/refresh", &invalid).unwrap_err();
        assert_eq!(low.kind(), ErrorKind::InvalidValue);
        assert_eq!(
            session
                .accept_result("mission/refresh", true, &invalid)
                .unwrap_err(),
            low
        );
        assert!(
            session
                .before_request("mission/history", &json!({}))
                .is_ok()
        );
        assert!(
            session
                .accept_result("mission/refresh", true, &golden_result("mission/refresh"))
                .is_ok()
        );
        let error = session
            .accept_result("state/delta", false, &json!({ "changes": null }))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(
            session
                .before_request("mission/refresh", &json!({}))
                .is_ok()
        );
        assert!(
            session
                .accept_result("about", false, &json!({ "mission": null }))
                .is_ok()
        );
    }

    #[test]
    fn session_debug_exposes_only_quarantine_status() {
        let session = MissionReadSession {
            unavailable: Some(ApiError::new(
                ErrorKind::UpdateRequired,
                "private-diagnostic-sentinel",
            )),
        };
        let text = format!("{session:?}");
        assert!(text.contains("quarantined: true"));
        assert!(!text.contains("private-diagnostic-sentinel"));
        assert!(!text.contains("ApiError"));
        assert!(format!("{:?}", MissionReadSession::default()).contains("quarantined: false"));
    }

    #[test]
    fn read_failure_quarantines_later_and_pending_mission_results_not_other_services() {
        let mut session = MissionReadSession::default();
        let invalid = json!({ "vehicleId": "v", "version": 1, "status": "synced", "items": [{
            "command": 16, "frame": 3, "param1": null, "param2": 0, "param3": 0, "param4": 0,
            "latitudeDeg": 47.25, "longitudeDeg": -1.5, "altitudeM": 40, "autocontinue": true,
        }] });
        assert!(
            session
                .before_request("mission/history", &json!({ "id": "v" }))
                .is_ok()
        );
        assert!(
            session
                .accept_result("mission/refresh", true, &invalid)
                .is_err()
        );
        for method in [
            "mission/history",
            "mission/refresh",
            "mission/restore",
            "mission/upload",
            "plan/upload",
        ] {
            assert!(
                session.before_request(method, &json!({})).is_err(),
                "{method}"
            );
        }
        assert!(
            session
                .before_request("state/subscribe", &json!({ "entities": ["mission"] }))
                .is_err()
        );
        assert!(
            session
                .before_request("state/subscribe", &json!({ "entities": ["vehicle"] }))
                .is_ok()
        );
        assert!(
            session
                .accept_result("mission/history", true, &json!({ "records": [] }))
                .is_err()
        );
        assert!(
            session
                .accept_result(
                    "state/subscribe",
                    false,
                    &json!({ "seq": 7, "snapshot": { "vehicles": [] } })
                )
                .is_ok()
        );
        assert!(session.before_request("about", &json!({})).is_ok());
        assert!(
            session
                .accept_result("about", false, &json!({ "mission": null }))
                .is_ok()
        );
    }

    #[test]
    fn read_floats_round_trip_exact_finite_and_nonfinite_bits() {
        for bits in [
            0_u64,
            (-0.0_f64).to_bits(),
            47.25_f64.to_bits(),
            0x7ff8_0000_0000_0001,
            0xfff8_0000_0000_0042,
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
        ] {
            let value = MissionReadFloat::from(f64::from_bits(bits));
            let json = serde_json::to_string(&value).unwrap();
            let restored: MissionReadFloat = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, value, "{json}");
            if f64::from_bits(bits).is_finite() {
                assert!(!json.contains("nonFiniteF64"));
            } else {
                assert_eq!(json, format!("{{\"nonFiniteF64\":\"{bits:016x}\"}}"));
            }
        }
    }

    #[test]
    fn read_tags_reject_null_overflow_malformed_finite_and_extra_keys() {
        for json in [
            "null",
            "1e400",
            "\"NaN\"",
            "{}",
            "{\"nonFiniteF64\":null}",
            "{\"nonFiniteF64\":\"7FF8000000000001\"}",
            "{\"nonFiniteF64\":\"7ff800000000001\"}",
            "{\"nonFiniteF64\":\"7ff800000000000g\"}",
            "{\"nonFiniteF64\":\"3ff0000000000000\"}",
            "{\"nonFiniteF64\":\"7ff8000000000001\",\"extra\":0}",
            "{\"nonFiniteF64\":\"7ff8000000000001\\n\"}",
            "{\"nonFiniteF64\":\"7ff8000000000001\\r\"}",
            "{\"nonFiniteF64\":\"7ff8000000000001\\u2028\"}",
            "{\"nonFiniteF64\":\"7ff8000000000001\\u2029\"}",
        ] {
            assert!(
                serde_json::from_str::<MissionReadFloat>(json).is_err(),
                "{json}"
            );
        }
    }
}
