//! Saved plans (ADR-0029, S125): missions kept in UXDuck for a vehicle
//! type, with a fence and rally points. A plan never reaches a vehicle by
//! itself. Every edit is a revision, so undo and redo survive a reload;
//! deleting moves a plan to Deleted for 30 days.

use std::collections::HashMap;

use crate::plan_checks::{Check, Severity};
use serde_json::Value;
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::plan::{
    Plan as PlanDto, PlanAcknowledgeParams, PlanCheck, PlanCheckResult, PlanCheckSeverity,
    PlanCreateParams, PlanExamplesParams, PlanExamplesResult, PlanExportFormat, PlanExportParams,
    PlanExportResult, PlanFile as PlanFileDto, PlanImportParams, PlanImportResult, PlanListParams,
    PlanListResult, PlanNote, PlanPlaceParams, PlanStepParams, PlanUpdateParams, PlanUploadParams,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::mission::MissionItem;
use uxduck_core::domain::parameter::{ParamKind, parameter_id};
use uxduck_core::domain::plan::{
    DELETED_KEPT_US, FenceAction, LatLon, PatternShape, Plan, PlanAcknowledgement, PlanAuthor,
    PlanAutopilot, PlanBody, PlanFrame, PlanRevision, PlanVehicleType, VehicleCopy,
};
use uxduck_core::events::Change;

use crate::plan_files::{self, ExportFormat, PlanFile};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

/// Names and labels are kept short enough to show in a row.
const NAME_CHARS: usize = 100;
/// Files for one import, together.
const IMPORT_BYTES: usize = 8 * 1024 * 1024;

#[allow(clippy::too_many_lines, reason = "one arm per method, each short")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    forget_old_deleted(engine, connection);
    match request {
        ClientRequest::PlanList(PlanListParams { deleted }) => {
            require(Capability::Read)?;
            let mut plans: Vec<&Plan> = engine
                .state
                .plans
                .values()
                // A vehicle's copy of its mission is not one of the plans (P17).
                .filter(|plan| plan.vehicle_copy.is_none())
                .filter(|plan| deleted || plan.deleted_at_us.is_none())
                .collect();
            plans.sort_by_key(|plan| std::cmp::Reverse(plan.changed_at_us));
            to_value(&PlanListResult {
                plans: plans.into_iter().map(PlanDto::from).collect(),
            })
        }
        ClientRequest::PlanGet(IdParams { id }) => {
            require(Capability::Read)?;
            to_value(&PlanDto::from(plan(engine, &id)?))
        }
        ClientRequest::PlanCreate(mut params) => {
            require(Capability::Config)?;
            confirmed_areas(engine, connection, &mut params)?;
            let copy = params.vehicle_copy.clone();
            let body = new_body(params)?;
            let plan = create(engine, connection, body, copy);
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanUpdate(PlanUpdateParams {
            id,
            expected_version,
            body,
        }) => {
            require(Capability::Config)?;
            let mut plan = editable(engine, &id, expected_version)?.clone();
            let body = valid_body(PlanBody::from(&body))?;
            if body == plan.body {
                return to_value(&PlanDto::from(&plan));
            }
            let previous = std::mem::replace(&mut plan.body, body);
            plan.undo = undo_after_edit(&engine.state, &plan.id, &previous);
            plan.redo = 0;
            touch(engine, connection, &mut plan);
            engine.commit(
                connection,
                Change::PlanSaved {
                    plan: plan.clone(),
                    revision: PlanRevision::Edit {
                        previous: Box::new(previous),
                    },
                },
            );
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanUndo(PlanStepParams {
            id,
            expected_version,
            to_version,
        }) => {
            require(Capability::Config)?;
            let plan = editable(engine, &id, expected_version)?.clone();
            let steps = match to_version {
                // Every revision since then: each edit bumped the version
                // once, undo and redo too, so this is an upper bound that
                // stops at the oldest revision kept.
                Some(version) if version < plan.version => u32::try_from(plan.version - version)
                    .unwrap_or(u32::MAX)
                    .min(plan.undo),
                Some(_) => 0,
                None => 1,
            };
            if steps == 0 || plan.undo == 0 {
                return to_value(&PlanDto::from(&plan));
            }
            step(engine, connection, plan, PlanRevision::Undo { steps })
        }
        ClientRequest::PlanRedo(PlanStepParams {
            id,
            expected_version,
            ..
        }) => {
            require(Capability::Config)?;
            let plan = editable(engine, &id, expected_version)?.clone();
            if plan.redo == 0 {
                return to_value(&PlanDto::from(&plan));
            }
            step(engine, connection, plan, PlanRevision::Redo)
        }
        ClientRequest::PlanPlace(PlanPlaceParams { id, name, at }) => {
            require(Capability::Config)?;
            let mut plan = plan(engine, &id)?.clone();
            let name = name.trim().chars().take(NAME_CHARS).collect::<String>();
            if name.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "a place name cannot be empty",
                ));
            }
            plan.place_name = Some(name);
            plan.place_at = Some(LatLon::from(&at));
            plan.version += 1;
            engine.commit(
                connection,
                Change::PlanSaved {
                    plan: plan.clone(),
                    revision: PlanRevision::None,
                },
            );
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanDelete(IdParams { id }) => {
            require(Capability::Config)?;
            let mut plan = plan(engine, &id)?.clone();
            // A vehicle copy is discarded, not kept in Deleted (P17).
            if plan.vehicle_copy.is_some() {
                engine.commit(connection, Change::PlanRemoved { id });
                plan.deleted_at_us = Some(engine.clock.now_us());
                return to_value(&PlanDto::from(&plan));
            }
            if plan.deleted_at_us.is_none() {
                plan.deleted_at_us = Some(engine.clock.now_us());
                plan.version += 1;
                engine.commit(
                    connection,
                    Change::PlanSaved {
                        plan: plan.clone(),
                        revision: PlanRevision::None,
                    },
                );
            }
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanRestore(IdParams { id }) => {
            require(Capability::Config)?;
            let mut plan = plan(engine, &id)?.clone();
            if plan.deleted_at_us.is_some() {
                plan.deleted_at_us = None;
                plan.version += 1;
                engine.commit(
                    connection,
                    Change::PlanSaved {
                        plan: plan.clone(),
                        revision: PlanRevision::None,
                    },
                );
            }
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanImport(PlanImportParams {
            files,
            vehicle_type,
        }) => {
            require(Capability::Config)?;
            let size: usize = files.iter().map(|file| file.text.len()).sum();
            if size > IMPORT_BYTES {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "these files are too large for a plan (8 MB together)",
                ));
            }
            let files: Vec<PlanFile> = files
                .into_iter()
                .map(|file| PlanFile {
                    name: file.name,
                    text: file.text,
                })
                .collect();
            let default_type =
                vehicle_type.map_or(PlanVehicleType::default(), PlanVehicleType::from);
            let imported = plan_files::import(&files, default_type)
                .map_err(|message| ApiError::new(ErrorKind::InvalidValue, message))?;
            let body = valid_body(imported.body)?;
            let plan = create(engine, connection, body, None);
            to_value(&PlanImportResult {
                plan: PlanDto::from(&plan),
                skipped: imported.skipped,
            })
        }
        ClientRequest::PlanExport(PlanExportParams { id, format }) => {
            require(Capability::Read)?;
            let plan = plan(engine, &id)?;
            let format = match format {
                PlanExportFormat::Qgc => ExportFormat::Qgc,
                PlanExportFormat::MissionPlanner => ExportFormat::MissionPlanner,
            };
            let files = plan_files::export(&plan.body, format)
                .into_iter()
                .map(|file| PlanFileDto {
                    name: file.name,
                    text: file.text,
                })
                .collect();
            to_value(&PlanExportResult { files })
        }
        ClientRequest::PlanExamples(PlanExamplesParams { at, heading_deg }) => {
            require(Capability::Read)?;
            let on_globe = at.latitude_deg.is_finite()
                && at.longitude_deg.is_finite()
                && at.latitude_deg.abs() <= 90.0
                && at.longitude_deg.abs() <= 180.0;
            if !on_globe || !heading_deg.unwrap_or(0.0).is_finite() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "examples go at a place on the globe",
                ));
            }
            let at = LatLon {
                latitude_deg: at.latitude_deg,
                longitude_deg: at.longitude_deg,
            };
            let examples = [
                PlanVehicleType::Multicopter,
                PlanVehicleType::Plane,
                PlanVehicleType::Vtol,
                PlanVehicleType::Rover,
                PlanVehicleType::Boat,
                PlanVehicleType::Submarine,
                PlanVehicleType::Blimp,
            ]
            .into_iter()
            .map(|t| {
                let body = crate::plan_examples::example(
                    t,
                    PlanAutopilot::Ardupilot,
                    at,
                    heading_deg.unwrap_or(0.0),
                );
                uxduck_contracts::dto::plan::PlanBody::from(&body)
            })
            .collect();
            to_value(&PlanExamplesResult { examples })
        }
        ClientRequest::PlanCheck(params) => {
            require(Capability::Read)?;
            let plan = plan(engine, &params.id)?.clone();
            let checks = checks_of(engine, &plan.body, params.vehicle_id.as_deref());
            let mut result = PlanCheckResult::default();
            for (c, about, id) in named(&plan.body, checks) {
                let severity = match c.severity {
                    Severity::Error => PlanCheckSeverity::Error,
                    Severity::Warning => PlanCheckSeverity::Warning,
                    Severity::Note => {
                        result.notes.push(PlanNote {
                            code: c.code.to_owned(),
                            item: c.item,
                            message: c.message,
                        });
                        continue;
                    }
                };
                result.checks.push(PlanCheck {
                    severity,
                    acknowledged: severity == PlanCheckSeverity::Warning
                        && plan.acknowledged.iter().any(|a| a.about == about),
                    code: c.code.to_owned(),
                    item: c.item,
                    message: c.message,
                    id: Some(id),
                });
            }
            to_value(&result)
        }
        ClientRequest::PlanAcknowledge(PlanAcknowledgeParams {
            id,
            vehicle_id,
            check_id,
            acknowledged,
        }) => {
            require(Capability::Config)?;
            // A person's word (FB-273): an agent sees the warning as kept,
            // and never keeps one, whatever it was granted.
            if author(engine, connection) == PlanAuthor::Agent {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "Only a person can keep a plan's warning as is.",
                ));
            }
            let mut plan = plan(engine, &id)?.clone();
            let checks = checks_of(engine, &plan.body, vehicle_id.as_deref());
            let Some((found, about, _)) = named(&plan.body, checks)
                .into_iter()
                .find(|(_, _, named)| *named == check_id)
            else {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    "The plan has changed since that warning was shown. Check it again.",
                ));
            };
            // Ground still being fetched goes by itself: nothing to keep.
            if found.severity != Severity::Warning || found.code == "terrain-missing" {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Only a warning can be kept as is. A problem stops the upload until it is fixed.",
                ));
            }
            let before = plan.acknowledged.clone();
            plan.acknowledged.retain(|a| a.about != about);
            if acknowledged {
                plan.acknowledged.push(PlanAcknowledgement {
                    code: found.code.to_owned(),
                    item: found.item,
                    about,
                });
            }
            if plan.acknowledged == before {
                return to_value(&PlanDto::from(&plan));
            }
            // Pushed to other windows as it is, but kept outside the body's
            // version and undo history: no edit, nothing for "Back to when I
            // opened it" to count, and an edit in flight is no conflict.
            engine.commit(
                connection,
                Change::PlanSaved {
                    plan: plan.clone(),
                    revision: PlanRevision::None,
                },
            );
            to_value(&PlanDto::from(&plan))
        }
        ClientRequest::PlanUpload(PlanUploadParams {
            plan_id,
            vehicle_id,
        }) => {
            require(Capability::Control)?;
            let plan = plan(engine, &plan_id)?.clone();
            // Errors stop it (terrain still being fetched does not: the
            // upload waits for it, S120).
            let problems: Vec<String> = checks_of(engine, &plan.body, Some(&vehicle_id))
                .into_iter()
                .filter(|c| c.severity == Severity::Error && c.code != "terrain-missing")
                .map(|c| c.message)
                .collect();
            if !problems.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "{} {} to fix first: {}",
                        problems.len(),
                        if problems.len() == 1 {
                            "problem"
                        } else {
                            "problems"
                        },
                        problems.join(". ")
                    ),
                ));
            }
            if plan.deleted_at_us.is_some() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "this plan is in Deleted. Restore it to upload it",
                ));
            }
            let vehicle_id = engine.writable_vehicle(&vehicle_id)?;
            let px4 = engine
                .vehicle(&vehicle_id)
                .is_some_and(|v| v.autopilot.to_lowercase().contains("px4"));
            let body = if px4 {
                px4_heights(engine, &plan.body, &vehicle_id)?
            } else {
                plan.body.clone()
            };
            let mut job = plan_job(engine, &body, vehicle_id)?;
            job.reply = engine.replying_to.take();
            job.plan_id.clone_from(&plan.id);
            job.plan_version = plan.version;
            engine.upload_plan_mission(connection, job)?;
            Ok(Value::Null)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a plan method")),
    }
}

/// What uploading a body to a vehicle sends: the mission, and on ArduPilot
/// the fence, rally points and fence settings after it. Not tied to a saved
/// plan until `plan_id` is set.
///
/// # Errors
///
/// When a PX4 take-off or landing has no place and nothing gives it one
/// (`mission_items`).
pub(crate) fn plan_job(
    engine: &Engine,
    body: &PlanBody,
    vehicle_id: String,
) -> Result<crate::transfers::PlanJob, ApiError> {
    let ardupilot = engine
        .vehicle(&vehicle_id)
        .is_none_or(|v| v.autopilot.to_lowercase().contains("ardupilot"));
    let blimp = engine
        .vehicle(&vehicle_id)
        .is_some_and(|vehicle| vehicle.vehicle_type == "Airship");
    let fence = plan_files::fence_items(&body.fence)
        .iter()
        .map(as_mission_item)
        .collect::<Vec<_>>();
    let rally = rally_items(body);
    // The same three parts on PX4 (P14); its fence settings are `GF_`.
    let fence_settings = if blimp {
        Vec::new()
    } else if ardupilot {
        fence_settings(engine, &vehicle_id, body)
    } else {
        px4_fence_settings(engine, &vehicle_id, body)
    };
    let fused = engine.telemetry(&vehicle_id).and_then(|telemetry| {
        telemetry
            .positions
            .iter()
            .filter(|p| {
                p.source == "fused"
                    && p.latitude_deg.is_finite()
                    && p.longitude_deg.is_finite()
                    && (p.latitude_deg != 0.0 || p.longitude_deg != 0.0)
            })
            .max_by_key(|p| p.time_us)
            .map(|p| LatLon {
                latitude_deg: p.latitude_deg,
                longitude_deg: p.longitude_deg,
            })
    });
    let items = mission_items(body, ardupilot, fused)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    let px4 = engine
        .vehicle(&vehicle_id)
        .is_some_and(|v| v.autopilot.to_lowercase().contains("px4"));
    let (items, gimbal_handovers) = if px4 {
        px4_gimbal_handovers(items)
    } else {
        (items, 0)
    };
    Ok(crate::transfers::PlanJob {
        reply: None,
        parts: if blimp {
            Vec::new()
        } else {
            vec![(1, fence.clone()), (2, rally.clone())]
        },
        fence,
        rally,
        fence_settings,
        plan_id: String::new(),
        plan_version: 0,
        vehicle_id,
        items,
        home_first: ardupilot,
        placeless: Vec::new(),
        gimbal_handovers,
    })
}

fn plan<'a>(engine: &'a Engine, id: &str) -> Result<&'a Plan, ApiError> {
    engine
        .state
        .plans
        .get(id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no plan {id}")))
}

fn editable<'a>(engine: &'a Engine, id: &str, expected_version: u64) -> Result<&'a Plan, ApiError> {
    let plan = plan(engine, id)?;
    if plan.version != expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            "this plan changed in another window. It now shows the latest",
        );
        error.data.current_version = Some(plan.version);
        return Err(error);
    }
    if plan.deleted_at_us.is_some() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "this plan is in Deleted. Restore it to change it",
        ));
    }
    Ok(plan)
}

/// Each survey `placeAreas` names takes exactly the outline a person
/// confirmed (FB-177), whatever the request gave as its area, so a plan
/// never surveys a shape other than the one the person said yes to.
fn confirmed_areas(
    engine: &Engine,
    connection: ConnectionId,
    params: &mut PlanCreateParams,
) -> Result<(), ApiError> {
    use uxduck_contracts::dto::plan::PatternShape as Shape;
    let areas = std::mem::take(&mut params.place_areas);
    if areas.is_empty() {
        return Ok(());
    }
    let invalid = |message: String| ApiError::new(ErrorKind::InvalidValue, message);
    let Some(body) = params.body.as_mut() else {
        return Err(invalid(
            "placeAreas needs a body with the survey patterns the outlines become the areas of."
                .into(),
        ));
    };
    for (pattern_id, area_id) in areas {
        let area = engine.confirmed_area(&area_id, connection)?;
        let pattern = body
            .patterns
            .iter_mut()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or_else(|| {
                invalid(format!(
                    "placeAreas names pattern {pattern_id}, which the plan does not have."
                ))
            })?;
        let Shape::Survey { area: corners, .. } = &mut pattern.shape else {
            return Err(invalid(format!(
                "Pattern {pattern_id} is not a survey, so it takes no outline."
            )));
        };
        corners.clone_from(&area.points);
    }
    Ok(())
}

fn new_body(params: PlanCreateParams) -> Result<PlanBody, ApiError> {
    let mut body = params.body.as_ref().map(PlanBody::from).unwrap_or_default();
    let vehicle_type = params.vehicle_type.map_or(
        if params.body.is_some() {
            body.vehicle_type
        } else {
            PlanVehicleType::default()
        },
        PlanVehicleType::from,
    );
    let autopilot = params.autopilot.map_or(body.autopilot, Into::into);
    if params.body.is_none() {
        body = PlanBody::new(String::new(), vehicle_type, autopilot);
    } else {
        if body.vehicle_type != vehicle_type && body.items.is_empty() {
            body.frame = vehicle_type.default_frame();
        }
        body.vehicle_type = vehicle_type;
        body.autopilot = autopilot;
    }
    if let Some(name) = params.name {
        body.name = name;
    }
    if body.name.trim().is_empty() {
        body.name = "Untitled plan".into();
    }
    valid_body(body)
}

/// Patterns with ids of their own, places on the globe and sizes that are numbers.
fn valid_patterns(body: &PlanBody) -> Result<(), ApiError> {
    let bad = |message: &str| Err(ApiError::new(ErrorKind::InvalidValue, message.to_owned()));
    let on_globe = |p: &LatLon| {
        p.latitude_deg.is_finite()
            && p.longitude_deg.is_finite()
            && p.latitude_deg.abs() <= 90.0
            && p.longitude_deg.abs() <= 180.0
    };
    let mut ids = std::collections::HashSet::new();
    for pattern in &body.patterns {
        if pattern.id.is_empty() || !ids.insert(pattern.id.as_str()) {
            return bad("each pattern needs an id of its own");
        }
        let (points, numbers): (Vec<&LatLon>, Vec<f64>) = match &pattern.shape {
            PatternShape::Survey {
                area,
                spacing_m,
                angle_deg,
                turnaround_m,
                altitude_m,
                margin_m,
                ..
            } => (
                area.iter().collect(),
                vec![
                    *spacing_m,
                    *angle_deg,
                    *turnaround_m,
                    *altitude_m,
                    *margin_m,
                ],
            ),
            PatternShape::Corridor {
                line,
                width_m,
                spacing_m,
                turnaround_m,
                altitude_m,
            } => (
                line.iter().collect(),
                vec![*width_m, *spacing_m, *turnaround_m, *altitude_m],
            ),
            PatternShape::Orbit {
                center,
                radius_m,
                turns,
                heights_m,
                ..
            } => (
                vec![center],
                [*radius_m, *turns]
                    .into_iter()
                    .chain(heights_m.iter().copied())
                    .collect(),
            ),
            PatternShape::Landing {
                touchdown,
                approach_from,
                approach_altitude_m,
            } => (vec![touchdown, approach_from], vec![*approach_altitude_m]),
        };
        if !points.into_iter().all(on_globe) || !numbers.iter().all(|n| n.is_finite()) {
            return bad("every point and size of a pattern must be a number on the globe");
        }
    }
    Ok(())
}

fn valid_body(mut body: PlanBody) -> Result<PlanBody, ApiError> {
    body.name = body.name.trim().to_string();
    if body.name.is_empty() || body.name.chars().count() > NAME_CHARS {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a plan's name must have 1–100 characters",
        ));
    }
    valid_patterns(&body)?;
    // A pattern's items are made again from it (P11): all of them, or none
    // when it is refused. A spacing that is not above zero, more waypoints
    // than one mission carries, a margin or exclusion zones that leave a
    // survey nothing to fly, or a leg with no way round a zone refuse the
    // change, in words.
    if let Some(refusal) = crate::plan_patterns::expand(&mut body) {
        return Err(ApiError::new(ErrorKind::InvalidValue, refusal));
    }
    let on_globe = |lat: f64, lon: f64| {
        lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
    };
    let items_ok = body
        .items
        .iter()
        .all(|item| on_globe(item.latitude_deg, item.longitude_deg) && item.altitude_m.is_finite());
    let fence_ok = body
        .fence
        .polygons
        .iter()
        .flat_map(|p| &p.points)
        .all(|p| on_globe(p.latitude_deg, p.longitude_deg))
        && body.fence.circles.iter().all(|c| {
            on_globe(c.center.latitude_deg, c.center.longitude_deg)
                && c.radius_m.is_finite()
                && c.radius_m > 0.0
        });
    let rally_ok = body
        .rally
        .iter()
        .all(|r| on_globe(r.latitude_deg, r.longitude_deg) && r.altitude_m.is_finite());
    if !(items_ok && fence_ok && rally_ok) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "every point of a plan must be on the globe",
        ));
    }
    if !body.next_altitude_m.is_finite()
        || !body.min_clearance_m.is_finite()
        || body.min_clearance_m < 0.0
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "heights must be numbers",
        ));
    }
    body.labels = body
        .labels
        .into_iter()
        .map(|label| label.trim().chars().take(NAME_CHARS).collect::<String>())
        .filter(|label| !label.is_empty())
        .collect();
    body.labels.sort();
    body.labels.dedup();
    Ok(body)
}

fn author(engine: &Engine, connection: ConnectionId) -> PlanAuthor {
    let agent = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .is_some_and(|admitted| admitted.kind == ClientKind::Agent);
    if agent {
        PlanAuthor::Agent
    } else {
        PlanAuthor::Person
    }
}

fn touch(engine: &Engine, connection: ConnectionId, plan: &mut Plan) {
    plan.bounds = plan.body.bounds();
    plan.changed_at_us = engine.clock.now_us();
    plan.made_by = author(engine, connection);
    plan.version += 1;
}

fn create(
    engine: &mut Engine,
    connection: ConnectionId,
    body: PlanBody,
    copy: Option<uxduck_contracts::dto::plan::VehicleCopy>,
) -> Plan {
    let now = engine.clock.now_us();
    let vehicle_copy = copy.map(|copy| VehicleCopy {
        vehicle_id: copy.vehicle_id,
        mission_version: copy.mission_version,
        plan_id: copy.plan_id,
    });
    // A vehicle has one copy at a time: a new one replaces it.
    if let Some(copy) = &vehicle_copy {
        let older: Vec<String> = engine
            .state
            .plans
            .values()
            .filter(|p| {
                p.vehicle_copy
                    .as_ref()
                    .is_some_and(|c| c.vehicle_id == copy.vehicle_id)
            })
            .map(|p| p.id.clone())
            .collect();
        for id in older {
            engine.commit(connection, Change::PlanRemoved { id });
        }
    }
    let made_by = author(engine, connection);
    // A person's copy of a vehicle's mission keeps the warnings kept in the
    // plan it came from (FB-273), when that plan's upload became that very
    // mission, read back the same, and the vehicle still has it. An agent's
    // copy, or one of anything else, starts with none.
    let acknowledged = vehicle_copy
        .as_ref()
        .filter(|_| made_by == PlanAuthor::Person)
        .filter(|copy| engine.mission(&copy.vehicle_id).version == copy.mission_version)
        .and_then(|copy| {
            let from = engine.state.plans.get(copy.plan_id.as_ref()?)?;
            from.uploads
                .iter()
                .any(|u| {
                    u.verified
                        && u.vehicle_id == copy.vehicle_id
                        && u.mission_version == copy.mission_version
                })
                .then(|| from.acknowledged.clone())
        })
        .unwrap_or_default();
    let plan = Plan {
        id: format!("plan{}", &engine.random.token()[..16]),
        bounds: body.bounds(),
        body,
        place_name: None,
        place_at: None,
        made_by,
        created_at_us: now,
        changed_at_us: now,
        deleted_at_us: None,
        uploads: Vec::new(),
        undo: 0,
        redo: 0,
        version: 1,
        acknowledged,
        vehicle_copy,
    };
    engine.commit(
        connection,
        Change::PlanSaved {
            plan: plan.clone(),
            revision: PlanRevision::None,
        },
    );
    plan
}

/// How many changes a plan can undo once an edit replaces `previous`: what
/// its ring keeps then, which for a very large plan is fewer than its edits
/// (`MAX_REVISIONS_BYTES`).
pub(crate) fn undo_after_edit(state: &crate::state::State, id: &str, previous: &PlanBody) -> u32 {
    let kept = state
        .plan_revisions
        .get(id)
        .map_or(1, |ring| ring.undo_after_edit(previous));
    u32::try_from(kept).unwrap_or(u32::MAX)
}

/// Undo or redo: the stacks are moved on a copy first so the plan carries
/// the body and counts the change lands on.
fn step(
    engine: &mut Engine,
    connection: ConnectionId,
    mut plan: Plan,
    revision: PlanRevision,
) -> Result<Value, ApiError> {
    let mut revisions = engine
        .state
        .plan_revisions
        .get(&plan.id)
        .cloned()
        .unwrap_or_default();
    let Some(body) = revisions.apply(&plan.body, &revision) else {
        return to_value(&PlanDto::from(&plan));
    };
    plan.body = body;
    plan.undo = u32::try_from(revisions.undo.len()).unwrap_or(u32::MAX);
    plan.redo = u32::try_from(revisions.redo.len()).unwrap_or(u32::MAX);
    touch(engine, connection, &mut plan);
    engine.commit(
        connection,
        Change::PlanSaved {
            plan: plan.clone(),
            revision,
        },
    );
    to_value(&PlanDto::from(&plan))
}

/// Plans in Deleted for more than 30 days go for good.
fn forget_old_deleted(engine: &mut Engine, connection: ConnectionId) {
    let now = engine.clock.now_us();
    let expired: Vec<String> = engine
        .state
        .plans
        .values()
        .filter(|plan| {
            plan.deleted_at_us
                .is_some_and(|at| now.saturating_sub(at) > DELETED_KEPT_US)
        })
        .map(|plan| plan.id.clone())
        .collect();
    for id in expired {
        engine.commit(connection, Change::PlanRemoved { id });
    }
}

/// The wire form of a plan change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::PlanSaved { plan, .. } => {
            vec![EntityChange {
                plan: Some(plan.into()),
                ..upsert(EntityType::Plan, &plan.id)
            }]
        }
        Change::PlanRemoved { id } => {
            vec![EntityChange {
                op: ChangeOp::Remove,
                ..upsert(EntityType::Plan, id)
            }]
        }
        _ => Vec::new(),
    }
}

/// Commands PX4 takes in a global frame (P14, found on PX4 SIH): its
/// navigation commands, a region of interest, fence vertices and circles,
/// and rally points. It refuses any other in one ("Unsupported command"),
/// a landing sequence start and Return to launch among them.
const PX4_GLOBAL: [u16; 17] = [
    16, 17, 18, 19, 21, 22, 31, 84, 85, 195, 201, 5000, 5001, 5002, 5003, 5004, 5100,
];

/// The frame an item goes to PX4 in: `MAV_FRAME_MISSION` (2) for a command
/// it takes only there, as QGroundControl sends them.
#[must_use]
pub fn px4_frame(command: u16, frame: u8) -> u8 {
    if PX4_GLOBAL.contains(&command) {
        frame
    } else {
        2
    }
}

/// PX4's navigation commands, which it flies to the latitude and longitude
/// of as given.
const PX4_GOES_TO: [u16; 9] = [16, 17, 18, 19, 21, 22, 31, 84, 85];

/// A plan's items as the vehicle's mission (S127): on ArduPilot item 0 is
/// home, the planned home where there is one; a parameter left open is
/// sent as 0, as Mission Planner sends it; on PX4 each command is in a
/// frame it takes (`px4_frame`).
///
/// A take-off or landing without a place is 0°, 0° in the plan, which
/// ArduPilot takes as where the vehicle is and PX4 flies to. On PX4 it
/// takes the place of the navigation item before it, else `fused` (where
/// the vehicle is), else the plan's home.
///
/// # Errors
///
/// When a PX4 take-off or landing has no place and none of those is known.
pub fn mission_items(
    body: &PlanBody,
    ardupilot: bool,
    fused: Option<LatLon>,
) -> Result<Vec<MissionItem>, String> {
    let mut items = Vec::with_capacity(body.items.len() + 1);
    if ardupilot {
        let home = body.planned_home;
        items.push(MissionItem {
            command: 16,
            frame: 0,
            param1: 0.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            latitude_deg: home.map_or(0.0, |h| h.latitude_deg),
            longitude_deg: home.map_or(0.0, |h| h.longitude_deg),
            altitude_m: home.and_then(|h| h.altitude_m).unwrap_or(0.0),
            autocontinue: true,
        });
    }
    let mut here = fused.or(body.planned_home.map(|home| LatLon {
        latitude_deg: home.latitude_deg,
        longitude_deg: home.longitude_deg,
    }));
    for (i, item) in body.items.iter().enumerate() {
        let [p1, p2, p3, p4] = item.params.map(|p| p.unwrap_or(0.0));
        let mut place = LatLon {
            latitude_deg: item.latitude_deg,
            longitude_deg: item.longitude_deg,
        };
        if !ardupilot && PX4_GOES_TO.contains(&item.command) {
            if uxduck_core::domain::plan::has_position(item) {
                here = Some(place);
            } else if matches!(item.command, 21 | 22 | 84 | 85) {
                place = here.ok_or_else(|| {
                    format!(
                        "item {} is a {} without a place, and PX4 would fly it at 0°, 0°. The vehicle hasn't said where it is and the plan has no home. Give the item a place or the plan a home, or try again once the vehicle has a position",
                        i + 1,
                        if matches!(item.command, 22 | 84) { "take-off" } else { "landing" }
                    )
                })?;
            }
        }
        items.push(MissionItem {
            command: item.command,
            frame: if ardupilot {
                item.frame
            } else {
                px4_frame(item.command, item.frame)
            },
            param1: p1,
            param2: p2,
            param3: p3,
            param4: p4,
            latitude_deg: place.latitude_deg,
            longitude_deg: place.longitude_deg,
            altitude_m: item.altitude_m,
            autocontinue: item.autocontinue,
        });
    }
    Ok(items)
}

/// `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW`: a plan's Point gimbal step.
const POINT_GIMBAL: u16 = 1000;
/// `MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE`: who a gimbal's manager obeys.
pub const SET_GIMBAL_CONTROL: u16 = 1001;

/// A mission as a PX4 turns its gimbal for it: the command that gives the
/// mission the gimbal ahead of each Point gimbal step, and how many were
/// added.
///
/// PX4's gimbal manager acts on a mission's `DO_GIMBAL_MANAGER_PITCHYAW`
/// only while the autopilot is the gimbal's primary controller, and
/// refuses it otherwise ("`GIMBAL_MANAGER_PITCHYAW` from 1/1 denied, in
/// control: 0/0", seen on PX4 1.17's SITL). Nothing makes the autopilot
/// that by itself: PX4 takes the gimbal for a mission's
/// `DO_MOUNT_CONTROL` only (`MissionBlock::issue_command`).
/// `DO_GIMBAL_MANAGER_CONFIGURE` with -2 and -2 does, which MAVLink
/// defines as "set itself in control (for missions where the own sysid is
/// still unknown)". -1 and -1 leave the secondary controller as it is, and
/// param7 names the gimbal the step turns.
///
/// One goes ahead of each step, not one ahead of the first. A person or a
/// companion computer that takes the gimbal during the mission then has it
/// until the plan's next Point gimbal step, which turns the gimbal as the
/// plan says and as ArduPilot does. With one command for the whole mission
/// PX4 would refuse every step after the gimbal was taken. A step that has
/// the command just ahead of it already, as a mission read from a vehicle
/// has, gets no second one.
#[must_use]
pub fn px4_gimbal_handovers(items: Vec<MissionItem>) -> (Vec<MissionItem>, usize) {
    let mut handed = Vec::with_capacity(items.len());
    let mut added = 0;
    for item in items {
        let has_one = handed
            .last()
            .is_some_and(|before: &MissionItem| before.command == SET_GIMBAL_CONTROL);
        if item.command == POINT_GIMBAL && !has_one {
            handed.push(MissionItem {
                command: SET_GIMBAL_CONTROL,
                // `MAV_FRAME_MISSION`, as `px4_frame` gives the step.
                frame: 2,
                param1: -2.0,
                param2: -2.0,
                param3: -1.0,
                param4: -1.0,
                latitude_deg: 0.0,
                longitude_deg: 0.0,
                altitude_m: item.altitude_m,
                autocontinue: item.autocontinue,
            });
            added += 1;
        }
        handed.push(item);
    }
    (handed, added)
}

/// How many commands `px4_gimbal_handovers` adds to a mission of these
/// commands, for what counts a plan's items before it is uploaded.
pub(crate) fn px4_gimbal_handover_count(commands: impl IntoIterator<Item = u16>) -> usize {
    let mut before = 0;
    let mut count = 0;
    for command in commands {
        if command == POINT_GIMBAL && before != SET_GIMBAL_CONTROL {
            count += 1;
        }
        before = command;
    }
    count
}

/// What an upload says of the commands `px4_gimbal_handovers` added: the
/// vehicle is sent more items than the plan has, so the count is named.
pub(crate) fn px4_gimbal_handover_words(added: usize) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a count said in words")]
    let counted = crate::plan_patterns::grouped(added as f64);
    if added == 1 {
        "1 of them the Set gimbal control command PX4 needs ahead of a Point gimbal step".into()
    } else {
        format!(
            "{counted} of them the Set gimbal control commands PX4 needs ahead of Point gimbal steps"
        )
    }
}

fn as_mission_item(item: &uxduck_core::domain::plan::PlanItem) -> MissionItem {
    let [p1, p2, p3, p4] = item.params.map(|p| p.unwrap_or(0.0));
    MissionItem {
        command: item.command,
        frame: item.frame,
        param1: p1,
        param2: p2,
        param3: p3,
        param4: p4,
        latitude_deg: item.latitude_deg,
        longitude_deg: item.longitude_deg,
        altitude_m: item.altitude_m,
        autocontinue: item.autocontinue,
    }
}

/// Rally points as `NAV_RALLY_POINT` items, in the plan's frame (S128).
fn rally_items(body: &PlanBody) -> Vec<MissionItem> {
    body.rally
        .iter()
        .map(|rally| MissionItem {
            command: 5100,
            frame: body.frame.mav_frame(),
            param1: 0.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            latitude_deg: rally.latitude_deg,
            longitude_deg: rally.longitude_deg,
            altitude_m: rally.altitude_m,
            autocontinue: false,
        })
        .collect()
}

/// `FENCE_TYPE`'s bits: a ceiling, a circle round home, polygons and circles.
const FENCE_ALT_MAX_BIT: u32 = 1;
const FENCE_POLYGON_BIT: u32 = 4;

/// The fence settings a plan's upload writes on ArduPilot (ADR-0029 P10):
/// a plan with a fence turns the polygon and circle fence on, with its
/// ceiling and breach action; one without turns that part off and leaves
/// a ceiling set in the vehicle's parameters as it is, so it still arms.
/// Only values that differ are written; nothing without downloaded
/// parameters.
fn fence_settings(
    engine: &Engine,
    vehicle_id: &str,
    body: &PlanBody,
) -> Vec<(String, f64, ParamKind)> {
    let current = |name: &str| {
        engine
            .state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| (p.value, p.kind))
    };
    let Some((fence_type, type_kind)) = current("FENCE_TYPE") else {
        return Vec::new();
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a small bitmask"
    )]
    let mut bits = fence_type.max(0.0) as u32;
    let has_fence = !body.fence.polygons.is_empty() || !body.fence.circles.is_empty();
    if has_fence {
        bits |= FENCE_POLYGON_BIT;
    } else {
        bits &= !FENCE_POLYGON_BIT;
    }
    let mut writes = Vec::new();
    if let Some(ceiling) = body.fence.settings.ceiling_m {
        bits |= FENCE_ALT_MAX_BIT;
        if let Some((value, kind)) = current("FENCE_ALT_MAX")
            && (value - ceiling).abs() > 0.05
        {
            writes.push(("FENCE_ALT_MAX".to_owned(), ceiling, kind));
        }
    }
    if (f64::from(bits) - fence_type).abs() > 0.5 {
        writes.push(("FENCE_TYPE".to_owned(), f64::from(bits), type_kind));
    }
    let plane = engine
        .vehicle(vehicle_id)
        .is_some_and(|v| v.vehicle_type.to_lowercase().contains("wing"));
    if let Some(action) = body.fence.settings.action {
        let value = match (action, plane) {
            (FenceAction::Report, _) => Some(0.0),
            (FenceAction::ReturnToLaunch, _) => Some(1.0),
            (FenceAction::Land, false) => Some(2.0),
            (FenceAction::Brake, false) => Some(4.0),
            // A plane has no landing or brake on a breach.
            (FenceAction::Land | FenceAction::Brake, true) => None,
        };
        if let (Some(value), Some((now, kind))) = (value, current("FENCE_ACTION"))
            && (now - value).abs() > 0.5
        {
            writes.push(("FENCE_ACTION".to_owned(), value, kind));
        }
    }
    // On while there is a fence to keep; off when nothing is left of it.
    if let Some((enabled, kind)) = current("FENCE_ENABLE") {
        let wanted = if has_fence {
            Some(1.0)
        } else if bits == 0 {
            Some(0.0)
        } else {
            None
        };
        if let Some(wanted) = wanted
            && (enabled - wanted).abs() > 0.5
        {
            writes.push(("FENCE_ENABLE".to_owned(), wanted, kind));
        }
    }
    writes
}

/// PX4's fence settings (P14): `GF_ACTION` for a breach, and the ceiling
/// as `GF_MAX_VER_DIST`, written only where they differ from the vehicle's.
/// PX4 keeps a fence on while it holds one; there is no enable to write.
fn px4_fence_settings(
    engine: &Engine,
    vehicle_id: &str,
    body: &PlanBody,
) -> Vec<(String, f64, ParamKind)> {
    let current = |name: &str| {
        engine
            .state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| (p.value, p.kind))
    };
    let mut writes = Vec::new();
    if let Some(ceiling) = body.fence.settings.ceiling_m
        && let Some((value, kind)) = current("GF_MAX_VER_DIST")
        && (value - ceiling).abs() > 0.05
    {
        writes.push(("GF_MAX_VER_DIST".to_owned(), ceiling, kind));
    }
    if let Some(action) = body.fence.settings.action {
        // 1 warning, 2 hold, 3 return, 5 land.
        let value = match action {
            FenceAction::Report => 1.0,
            FenceAction::Brake => 2.0,
            FenceAction::ReturnToLaunch => 3.0,
            FenceAction::Land => 5.0,
        };
        if let Some((now, kind)) = current("GF_ACTION")
            && (now - value).abs() > 0.5
        {
            writes.push(("GF_ACTION".to_owned(), value, kind));
        }
    }
    writes
}

/// PX4 has no terrain frame in missions (P14): a plan's items above terrain
/// go up as heights above sea level, from the vehicle's dataset at each
/// point; one without a place (a take-off) goes above home. Refused while
/// the ground under one is still being fetched, or where there is none.
pub(crate) fn px4_heights(
    engine: &mut Engine,
    body: &PlanBody,
    vehicle_id: &str,
) -> Result<PlanBody, ApiError> {
    let dataset = engine.terrain_dataset_of(vehicle_id);
    let mut out = body.clone();
    let mut problem = None;
    for (i, item) in out.items.iter_mut().enumerate() {
        if !matches!(item.frame, 10 | 11) {
            continue;
        }
        if !uxduck_core::domain::plan::has_position(item) {
            item.frame = PlanFrame::AboveHome.mav_frame();
            continue;
        }
        let params = uxduck_contracts::dto::terrain::TerrainAtParams {
            latitude_deg: item.latitude_deg,
            longitude_deg: item.longitude_deg,
            dataset: None,
        };
        let ground = engine
            .terrain_answer(dataset, &params)
            .map(|a| match a.ground {
                Ok(uxduck_terrain::lookup::Ground::Height(h)) => Some(h),
                Ok(uxduck_terrain::lookup::Ground::Sea) => Some(0.0),
                _ => None,
            });
        match ground {
            Some(Some(h)) => {
                item.altitude_m += h;
                item.frame = PlanFrame::AboveSeaLevel.mav_frame();
            }
            Some(None) => {
                problem.get_or_insert(format!(
                    "there are no ground heights under item {} to turn its height above terrain into one above sea level, which PX4 needs: plan it above home or above sea level",
                    i + 1
                ));
            }
            None => {
                problem.get_or_insert(format!(
                    "the ground under item {} is still being fetched. PX4 needs heights above sea level, so try again in a moment",
                    i + 1
                ));
            }
        }
    }
    engine.terrain_flush();
    match problem {
        Some(why) => Err(ApiError::new(ErrorKind::InvalidValue, why)),
        None => Ok(out),
    }
}

/// A plan's checks, against the terrain a vehicle would use (or the
/// default dataset), asking for what is not fetched yet.
/// Each check with what it is about (`plan_checks::about`) and the id
/// `plan/acknowledge` names it by: that, and a count for an identical one.
fn named(body: &PlanBody, checks: Vec<Check>) -> Vec<(Check, String, String)> {
    let mut seen: HashMap<String, usize> = HashMap::with_capacity(checks.len());
    checks
        .into_iter()
        .map(|check| {
            let about = crate::plan_checks::about(body, &check);
            let count = seen.entry(about.clone()).or_default();
            let twins = *count;
            *count += 1;
            let id = if twins == 0 {
                about.clone()
            } else {
                format!("{about}.{twins}")
            };
            (check, about, id)
        })
        .collect()
}

fn checks_of(engine: &mut Engine, body: &PlanBody, vehicle_id: Option<&str>) -> Vec<Check> {
    let mut targeted = body.clone();
    if vehicle_id
        .and_then(|id| engine.vehicle(id))
        .is_some_and(|vehicle| vehicle.vehicle_type == "Airship")
    {
        targeted.vehicle_type = PlanVehicleType::Blimp;
    }
    let dataset = vehicle_id.map_or_else(
        || crate::terrain::setting(engine.state.settings.terrain_dataset),
        |id| engine.terrain_dataset_of(id),
    );
    let mut ground = |at: LatLon| {
        let params = uxduck_contracts::dto::terrain::TerrainAtParams {
            latitude_deg: at.latitude_deg,
            longitude_deg: at.longitude_deg,
            dataset: None,
        };
        let answer = engine.terrain_answer(dataset, &params)?;
        Some(match answer.ground {
            Ok(uxduck_terrain::lookup::Ground::Height(h)) => Some(h),
            Ok(uxduck_terrain::lookup::Ground::Sea) => Some(0.0),
            _ => None,
        })
    };
    let checks = crate::plan_checks::check(&targeted, &mut ground);
    engine.terrain_flush();
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(command: u16, gimbal: f64) -> MissionItem {
        MissionItem {
            command,
            frame: 2,
            param1: 0.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            altitude_m: gimbal,
            autocontinue: true,
        }
    }

    /// One command ahead of each Point gimbal step, for the gimbal the step
    /// names, unless the step has one already. The count made before an
    /// upload is the count added.
    #[test]
    fn a_px4_gimbal_handover_goes_ahead_of_each_point_gimbal_step_without_one() {
        for (plan, sent) in [
            (vec![16, 1000, 16], vec![16, 1001, 1000, 16]),
            (vec![1000, 1000], vec![1001, 1000, 1001, 1000]),
            (vec![1001, 1000, 16, 1000], vec![1001, 1000, 16, 1001, 1000]),
            (vec![1001, 16, 1000], vec![1001, 16, 1001, 1000]),
            (vec![16, 205, 197], vec![16, 205, 197]),
            (vec![], vec![]),
        ] {
            let items: Vec<MissionItem> = plan.iter().map(|command| item(*command, 0.0)).collect();
            let (handed, added) = px4_gimbal_handovers(items);
            let commands: Vec<u16> = handed.iter().map(|item| item.command).collect();
            assert_eq!(commands, sent, "{plan:?}");
            assert_eq!(added, sent.len() - plan.len(), "{plan:?}");
            assert_eq!(px4_gimbal_handover_count(plan.clone()), added, "{plan:?}");
        }
        let (handed, _) = px4_gimbal_handovers(vec![item(1000, 154.0)]);
        assert_eq!(
            (
                handed[0].param1,
                handed[0].param2,
                handed[0].param3,
                handed[0].param4
            ),
            (-2.0, -2.0, -1.0, -1.0)
        );
        assert_eq!((handed[0].frame, handed[0].altitude_m), (2, 154.0));
    }

    #[test]
    fn warnings_alike_on_a_large_plan_are_named_at_once() {
        // A 57,000-item survey can give as many warnings, each named by
        // what it is about and a count of those before it alike. Counting
        // them one by one against every earlier warning took seconds.
        let body = PlanBody::default();
        let checks: Vec<Check> = (0..30_000)
            .map(|_| Check {
                severity: Severity::Warning,
                code: "rally-outside-fence",
                item: None,
                message: "a rally point is outside the fence".into(),
            })
            .collect();
        let started = std::time::Instant::now();
        let named = named(&body, checks);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(named[0].2, named[0].1);
        assert_eq!(named[29_999].2, format!("{}.29999", named[0].1));
    }
}
