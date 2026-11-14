#![allow(
    clippy::expect_used,
    reason = "regression fixtures serialize known mappings"
)]

use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{GamepadAxis as Axis, InputMapping, PositionBinding};
use uxduck_core::domain::settings::PilotInputMethod;
use uxduck_engine::controller_input::{Controls, InputEvent, Report};
use uxduck_engine::runtime::ControlRoute;

fn input(axis: Option<Axis>, code: u32, value: f64) -> InputEvent {
    InputEvent::Axis {
        id: "pad".into(),
        axis,
        code: Some(code),
        value,
    }
}

fn route(mapping: InputMapping) -> ControlRoute {
    ControlRoute {
        vehicle_id: "vehicle".into(),
        mapping,
        gripper: None,
        pilot_input_method: PilotInputMethod::Automatic,
    }
}

fn connected() -> Controls {
    let mut controls = Controls::default();
    controls.input(InputEvent::Connected {
        id: "pad".into(),
        name: "Gamepad".into(),
        layout_known: true,
    });
    controls
}

fn disarm(code: u32, hold_ms: Option<u32>) -> PositionBinding {
    PositionBinding {
        axis: Axis::LeftStickX,
        code: Some(code),
        at: -1.0,
        command: Command::Disarm,
        hold_ms,
    }
}

#[test]
fn old_radio_mapping_never_arms_disarms_or_changes_mode_from_driving_sticks() {
    for browser in [false, true] {
        for hold_ms in [None, Some(1000)] {
            let mut controls = if browser {
                Controls::browser()
            } else {
                Controls::default()
            };
            controls.input(InputEvent::Connected {
                id: "pad".into(),
                name: "Gamepad".into(),
                layout_known: true,
            });
            let code = if browser { 3 } else { 65589 };
            let mut mapping = InputMapping::mode_1();
            let down = disarm(code, hold_ms);
            let mut up = down.clone();
            up.at = 1.0;
            up.command = Command::Arm;
            mapping.positions = vec![down, up];
            let steering_code = if browser { 2 } else { 65588 };
            for (at, mode) in [(-1.0, "MANUAL"), (0.0, "HOLD"), (1.0, "AUTO")] {
                mapping.positions.push(PositionBinding {
                    axis: Axis::LeftStickX,
                    code: Some(steering_code),
                    at,
                    command: Command::SetMode { mode: mode.into() },
                    hold_ms,
                });
            }
            // Simulate an old serialized mapping loaded after restarting.
            let stored = serde_json::to_string(&mapping).expect("serialize mapping");
            let restored = serde_json::from_str(&stored).expect("restore mapping");
            controls.route("pad", Some(route(restored)));
            // The alias is learned only after activation: admission alone is insufficient.
            for (axis, code) in [
                (Axis::RightStickY, code),
                (Axis::RightStickX, steering_code),
            ] {
                for value in [0.0, -1.0, 0.0, 1.0, 0.0, -1.0] {
                    let reports = controls.input(input(Some(axis), code, value));
                    assert!(
                        !reports.iter().any(|r| matches!(
                            r,
                            Report::Positioned { .. } | Report::Holding { .. }
                        ))
                    );
                    assert!(controls.due_holds(0).is_empty());
                    assert!(controls.due_holds(2_000_000).is_empty());
                    assert!(!controls.pilot_outputs().is_empty(), "driving still works");
                }
            }
        }
    }
}

#[test]
fn a_missing_raw_code_is_not_a_centred_switch_on_a_known_gamepad() {
    let mut controls = connected();
    let mut mapping = InputMapping::mode_1();
    let mut centre = disarm(999, None);
    centre.at = 0.0;
    mapping.positions = vec![centre];
    controls.route("pad", Some(route(mapping)));
    controls.input(input(None, 999, -1.0));
    controls.input(input(None, 999, 0.0));
    controls.due_holds(0);
    assert!(
        controls
            .due_holds(2_000_000)
            .iter()
            .any(|r| matches!(r, Report::Positioned { .. }))
    );
    // With no sample for a required raw driving input, output also waits.
    let mut mapping = InputMapping::mode_1();
    mapping.throttle.code = Some(12345);
    controls.route("pad", Some(route(mapping)));
    assert!(controls.pilot_outputs().is_empty());
}

#[test]
fn learning_an_alias_cancels_a_command_already_being_held() {
    let mut controls = connected();
    let mut mapping = InputMapping::mode_1();
    mapping.positions = vec![disarm(65589, Some(1000))];
    controls.input(input(None, 65589, 0.0));
    controls.route("pad", Some(route(mapping)));
    assert!(
        controls
            .input(input(None, 65589, -1.0))
            .iter()
            .any(|r| matches!(r, Report::Holding { .. }))
    );
    controls.due_holds(0);
    controls.input(input(Some(Axis::RightStickY), 65589, -1.0));
    assert!(
        !controls
            .due_holds(2_000_000)
            .iter()
            .any(|r| matches!(r, Report::Positioned { .. }))
    );
}
