//! Approved parameter baselines (C7): a vehicle's parameters pinned under
//! a name, compared with what it has now, and synced with the account.

use serde_json::{Value, json};
use uxduck_contracts::dto::param_definitions::{
    ParamBaseline as ParamBaselineDto, ParamBaselinesResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::parameter::{MAX_BASELINE_TEXT, ParamBaseline, same_value};
use uxduck_core::events::Change;

use super::queries::{vehicle_values, which_vehicle};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A UTC date, `2026-10-23`, from microseconds since 1970 (civil from days).
fn date(us: u64) -> String {
    let days = i64::try_from(us / 86_400_000_000).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// How many of the vehicle's parameters now differ from a baseline.
fn differ(engine: &Engine, baseline: &ParamBaseline) -> u32 {
    let Ok(vehicle) = which_vehicle(engine, Some(&baseline.vehicle_id)) else {
        return 0;
    };
    let now = vehicle_values(engine, vehicle);
    let names: std::collections::BTreeSet<&String> =
        baseline.values.keys().chain(now.keys()).collect();
    let count = names
        .into_iter()
        .filter(|name| {
            !matches!(
                (baseline.values.get(*name), now.get(*name)),
                (Some(a), Some(b)) if same_value(*a, *b)
            )
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The parameters a vehicle arms with (C7): their digest, and how far they
/// are from its newest baseline. None before its parameters are read.
pub(crate) fn at_arm(
    engine: &Engine,
    vehicle_id: &str,
) -> Option<uxduck_core::domain::flight::FlightParams> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let vehicle = which_vehicle(engine, Some(vehicle_id)).ok()?;
    let id = vehicle.id.clone();
    let values = vehicle_values(engine, vehicle);
    if values.is_empty() {
        return None;
    }
    let mut lines = String::new();
    for (name, value) in &values {
        let _ = writeln!(lines, "{name}={value}");
    }
    let digest =
        Sha256::digest(lines.as_bytes())
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
    let baseline = engine
        .state
        .baselines
        .values()
        .filter(|b| b.vehicle_id == id)
        .max_by_key(|b| b.created_at_us);
    Some(uxduck_core::domain::flight::FlightParams {
        digest,
        count: u32::try_from(values.len()).unwrap_or(u32::MAX),
        baseline_id: baseline.map(|b| b.id.clone()),
        baseline_name: baseline.map(|b| b.name.clone()),
        baseline_differ: baseline.map(|b| differ(engine, b)),
    })
}

fn dto(engine: &Engine, baseline: &ParamBaseline) -> ParamBaselineDto {
    ParamBaselineDto {
        id: baseline.id.clone(),
        vehicle_id: baseline.vehicle_id.clone(),
        name: baseline.name.clone(),
        note: baseline.note.clone(),
        set_by: baseline.set_by.clone(),
        account_id: baseline.account_id.clone(),
        count: u32::try_from(baseline.values.len()).unwrap_or(u32::MAX),
        differ: differ(engine, baseline),
        created_at_us: baseline.created_at_us,
        version: baseline.version,
    }
}

fn clean(text: Option<String>, what: &str) -> Result<Option<String>, ApiError> {
    let text = text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    if text
        .as_ref()
        .is_some_and(|t| t.chars().count() > MAX_BASELINE_TEXT)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("a baseline's {what} is at most {MAX_BASELINE_TEXT} characters"),
        ));
    }
    Ok(text)
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let need = match &request {
        ClientRequest::ParamBaselines(_) => Capability::Read,
        _ => Capability::Config,
    };
    if !granted.contains(&need) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            need.denied_message(),
        ));
    }
    match request {
        ClientRequest::ParamBaselines(params) => {
            let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
            let mut baselines: Vec<&ParamBaseline> = engine
                .state
                .baselines
                .values()
                .filter(|b| b.vehicle_id == vehicle.id)
                .collect();
            baselines.sort_by_key(|b| std::cmp::Reverse(b.created_at_us));
            to_value(&ParamBaselinesResult {
                baselines: baselines.into_iter().map(|b| dto(engine, b)).collect(),
            })
        }
        ClientRequest::ParamBaselineSet(params) => {
            let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
            let values = vehicle_values(engine, vehicle.clone());
            if values.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Download the vehicle's parameters before pinning them as a baseline.",
                ));
            }
            let now = engine.clock.now_us();
            let actor = engine.actor(connection);
            let set_by = actor
                .identity
                .as_ref()
                .map(|i| i.operator_name.clone())
                .or_else(|| {
                    actor
                        .operator_id
                        .as_ref()
                        .and_then(|id| engine.state.operators.get(id))
                        .map(|o| o.name.clone())
                })
                .unwrap_or_else(|| "Someone".into());
            let baseline = ParamBaseline {
                id: format!("bl{}", &engine.random.token()[..16]),
                vehicle_id: vehicle.id.clone(),
                name: clean(params.name, "name")?
                    .unwrap_or_else(|| format!("Approved {}", date(now))),
                note: clean(params.note, "note")?.unwrap_or_default(),
                set_by,
                account_id: actor.identity.as_ref().and_then(|i| i.account_id.clone()),
                values,
                created_at_us: now,
                version: 1,
            };
            engine.commit(
                connection,
                Change::BaselineUpserted {
                    baseline: baseline.clone(),
                },
            );
            to_value(&dto(engine, &baseline))
        }
        ClientRequest::ParamBaselineRemove(params) => {
            if !engine.state.baselines.contains_key(&params.id) {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no baseline {}", params.id),
                ));
            }
            engine.commit(connection, Change::BaselineRemoved { id: params.id });
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a baseline method")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_792_713_600_000_000), "2026-10-23");
        assert_eq!(super::date(951_782_400_000_000), "2000-02-29");
    }
}
