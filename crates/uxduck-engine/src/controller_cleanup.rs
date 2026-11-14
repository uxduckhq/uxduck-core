//! Explicit controller resets and mapping deletion. Never edits a shared
//! mapping as a side effect of resetting one device.

use serde_json::{Value, json};
use uxduck_contracts::dto::controller::MappingRemoveParams;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::controller::{Controller, InputMapping, TargetMode};
use uxduck_core::events::Change;

use crate::{ConnectionId, Engine};

const BUILT_IN: &[&str] = &[
    "mode-1",
    "mode-2",
    "radio",
    "radio-taer",
    "expresslrs-bluetooth",
    "expresslrs-4-bluetooth",
    "sim-keyboard",
];

/// The caller has checked the controller is unheld. Give it a fresh mapping
/// from the factory defaults, never from an edited built-in or another device.
pub(crate) fn reset(engine: &mut Engine, connection: ConnectionId, mut controller: Controller) {
    let raw = engine.system.controller_inputs();
    let known = raw
        .controllers
        .iter()
        .find(|c| c.id == controller.id)
        .map_or_else(
            || {
                engine
                    .state
                    .input_mappings
                    .get(&controller.mapping_id)
                    .map_or(controller.mapping_id.starts_with("mode-"), |m| {
                        [m.roll, m.pitch, m.yaw, m.throttle]
                            .iter()
                            .all(|b| b.code.is_none())
                    })
            },
            |c| c.layout_known,
        );
    let mut fresh = if known {
        InputMapping::mode_2()
    } else {
        InputMapping::for_radio(&controller.name)
    };
    fresh.id = format!("mapping-{}", &engine.random.token()[..16]);
    fresh.name.clone_from(&controller.name);
    fresh.buttons.clear();
    fresh.positions.clear();
    fresh.actions.clear();
    fresh.rc_channels.clear();
    engine.commit(
        connection,
        Change::InputMappingUpserted {
            mapping: fresh.clone(),
        },
    );

    let actor = engine.actor(connection);
    let operators: Vec<_> = engine
        .state
        .operators
        .values()
        .filter(|o| {
            actor.operator_id.as_deref() == Some(o.id.as_str())
                || o.controller_mappings
                    .iter()
                    .any(|m| m.controller_id == controller.id)
        })
        .cloned()
        .collect();
    for mut operator in operators {
        operator
            .controller_mappings
            .retain(|m| m.controller_id != controller.id);
        operator.choose_mapping(&controller.id, &controller.name, None, &fresh.id);
        operator.version += 1;
        engine.commit(connection, Change::OperatorUpserted { operator });
    }
    controller.mapping_id = fresh.id;
    controller.mode = TargetMode::FollowsSelection;
    controller.target = None;
    controller.pending_target = None;
    controller.input_pause = None;
    controller.holding = None;
    controller.version += 1;
    let id = controller.id.clone();
    if controller.connected {
        engine.commit(connection, Change::ControllerUpserted { controller });
    } else {
        engine.commit(connection, Change::ControllerRemoved { id: id.clone() });
    }
    engine.route(&id);
}

pub(crate) fn remove_mapping(
    engine: &mut Engine,
    connection: ConnectionId,
    params: MappingRemoveParams,
) -> Result<Value, ApiError> {
    if BUILT_IN.contains(&params.id.as_str()) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Built-in mappings cannot be deleted.",
        ));
    }
    let mapping = engine
        .state
        .input_mappings
        .get(&params.id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "This mapping no longer exists."))?;
    if mapping.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            "This mapping changed while you were reviewing it. Review it again before deleting.",
        );
        error.data.current_version = Some(mapping.version);
        return Err(error);
    }
    let controllers: Vec<_> = engine
        .state
        .controllers
        .values()
        .filter(|c| c.mapping_id == params.id)
        .cloned()
        .collect();
    if let Some(held) = controllers
        .iter()
        .find(|c| c.claim.is_some() || engine.live.claimant.contains_key(&c.id))
    {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("Release {} before deleting this mapping.", held.name),
        ));
    }
    // Validate every user first. No partial reset when one controller is held.
    for controller in controllers {
        crate::controllers::forget(engine, connection, &controller.id)?;
    }
    let operators: Vec<_> = engine
        .state
        .operators
        .values()
        .filter(|o| {
            o.controller_mappings
                .iter()
                .any(|m| m.mapping_id == params.id)
        })
        .cloned()
        .collect();
    for mut operator in operators {
        operator
            .controller_mappings
            .retain(|m| m.mapping_id != params.id);
        operator.version += 1;
        engine.commit(connection, Change::OperatorUpserted { operator });
    }
    engine.commit(connection, Change::InputMappingRemoved { id: params.id });
    Ok(json!({}))
}
