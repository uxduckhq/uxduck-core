use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{
    GamepadAxis as Axis, InputMapping, PositionBinding, StickControl,
};

fn position(code: Option<u32>, at: f64) -> PositionBinding {
    PositionBinding {
        axis: Axis::LeftStickX,
        code,
        at,
        command: Command::Disarm,
        hold_ms: None,
    }
}

#[test]
fn a_reused_radio_switch_cannot_be_a_gamepad_driving_stick() {
    let mut mapping = InputMapping::mode_1();
    let disarm = position(Some(65589), -1.0);
    mapping.positions = vec![disarm.clone()];
    let xbox = [(Some(65589), Some(Axis::RightStickY))];
    assert!(mapping.position_conflicts(&disarm, &xbox));
    // A radio's placeholder name beside a code is not an alias.
    assert!(!mapping.position_conflicts(&disarm, &[]));
    mapping.throttle.code = Some(65589);
    assert!(mapping.position_conflicts(&disarm, &[]));
}

#[test]
fn unused_sticks_allow_positions_but_extra_rc_and_overlapping_positions_do_not() {
    let mut mapping = InputMapping::mode_1();
    let disarm = position(None, -1.0);
    mapping.positions = vec![disarm.clone()];
    assert!(mapping.position_conflicts(&disarm, &[]));
    mapping.unused = vec![StickControl::Yaw];
    assert!(!mapping.position_conflicts(&disarm, &[]));
    let mut other = disarm.clone();
    other.at = -0.85;
    mapping.positions.push(other);
    assert!(mapping.position_conflicts(&disarm, &[]));
    mapping.positions[1].at = 1.0;
    assert!(!mapping.position_conflicts(&disarm, &[]));
    mapping
        .rc_channels
        .push(uxduck_core::domain::controller::RcAxisBinding {
            channel: 5,
            input: disarm.binding(),
        });
    assert!(mapping.position_conflicts(&disarm, &[]));
}
