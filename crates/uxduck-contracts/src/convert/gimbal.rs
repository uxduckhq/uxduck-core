use uxduck_core::domain::gimbal as core;

use crate::dto::gimbal as dto;

/// The capabilities UXDuck has names for, by their bit.
const CAPABILITIES: [(u32, dto::GimbalCapability); 9] = [
    (1, dto::GimbalCapability::Retract),
    (2, dto::GimbalCapability::Neutral),
    (4, dto::GimbalCapability::RollAxis),
    (32, dto::GimbalCapability::PitchAxis),
    (256, dto::GimbalCapability::YawAxis),
    (512, dto::GimbalCapability::YawFollow),
    (1024, dto::GimbalCapability::YawLock),
    (2048, dto::GimbalCapability::InfiniteYaw),
    (131_072, dto::GimbalCapability::PointAtPlace),
];

/// `GIMBAL_DEVICE_ERROR_FLAGS` by their bit. The first three bits are the
/// limits it was sent past, which are not failures.
const FAILURES: [(u32, dto::GimbalFailure); 7] = [
    (8, dto::GimbalFailure::Encoder),
    (16, dto::GimbalFailure::Power),
    (32, dto::GimbalFailure::Motor),
    (64, dto::GimbalFailure::Software),
    (128, dto::GimbalFailure::Comms),
    (256, dto::GimbalFailure::Calibrating),
    (512, dto::GimbalFailure::NoManager),
];

/// An angle a manager gave in radians, to a tenth of a degree.
fn tenth(degrees: f32) -> f64 {
    (f64::from(degrees) * 10.0).round() / 10.0
}

impl From<core::GimbalLimits> for dto::GimbalLimits {
    fn from(v: core::GimbalLimits) -> Self {
        Self {
            roll_min_deg: v.roll_min_deg.map(tenth),
            roll_max_deg: v.roll_max_deg.map(tenth),
            pitch_min_deg: v.pitch_min_deg.map(tenth),
            pitch_max_deg: v.pitch_max_deg.map(tenth),
            yaw_min_deg: v.yaw_min_deg.map(tenth),
            yaw_max_deg: v.yaw_max_deg.map(tenth),
        }
    }
}

impl From<core::GimbalController> for dto::GimbalController {
    fn from(v: core::GimbalController) -> Self {
        Self {
            system_id: v.system_id,
            component_id: v.component_id,
        }
    }
}

impl From<core::GimbalHolder> for dto::GimbalHolder {
    fn from(v: core::GimbalHolder) -> Self {
        match v {
            core::GimbalHolder::Unknown => Self::Unknown,
            core::GimbalHolder::NoOne => Self::NoOne,
            core::GimbalHolder::Uxduck => Self::Uxduck,
            core::GimbalHolder::Autopilot => Self::Autopilot,
            core::GimbalHolder::Mission => Self::Mission,
            core::GimbalHolder::Other => Self::Other,
        }
    }
}

impl From<core::GimbalAction> for dto::GimbalAction {
    fn from(v: core::GimbalAction) -> Self {
        match v {
            core::GimbalAction::TakeControl => Self::TakeControl,
            core::GimbalAction::ReleaseControl => Self::ReleaseControl,
            core::GimbalAction::Point {
                pitch_deg,
                yaw_deg,
                lock,
            } => Self::Point {
                pitch_deg,
                yaw_deg,
                lock,
            },
            core::GimbalAction::Centre => Self::Centre,
            core::GimbalAction::Follow => Self::Follow,
            core::GimbalAction::HoldNorth => Self::HoldNorth,
            core::GimbalAction::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m,
            } => Self::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m,
            },
        }
    }
}

impl From<dto::GimbalAction> for core::GimbalAction {
    fn from(v: dto::GimbalAction) -> Self {
        match v {
            dto::GimbalAction::TakeControl => Self::TakeControl,
            dto::GimbalAction::ReleaseControl => Self::ReleaseControl,
            dto::GimbalAction::Point {
                pitch_deg,
                yaw_deg,
                lock,
            } => Self::Point {
                pitch_deg,
                yaw_deg,
                lock,
            },
            dto::GimbalAction::Centre => Self::Centre,
            dto::GimbalAction::Follow => Self::Follow,
            dto::GimbalAction::HoldNorth => Self::HoldNorth,
            dto::GimbalAction::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m,
            } => Self::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m,
            },
        }
    }
}

impl From<&core::Gimbal> for dto::Gimbal {
    fn from(v: &core::Gimbal) -> Self {
        Self {
            id: v.id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            component_id: v.component_id,
            device_id: v.device_id,
            device_component_id: v.device_component_id,
            device_silent_since_us: v.device_silent_since_us,
            present: v.present,
            capabilities: CAPABILITIES
                .into_iter()
                .filter(|(bit, _)| v.capabilities & bit != 0)
                .map(|(_, capability)| capability)
                .collect(),
            flags: v.capabilities,
            limits: v.limits.into(),
            holder: v.holder.into(),
            primary: v.primary.map(Into::into),
            secondary: v.secondary.map(Into::into),
            yaw_locked: v.yaw_locked,
            retracted: v.retracted,
            neutral: v.neutral,
            failures: FAILURES
                .into_iter()
                .filter(|(bit, _)| v.failure_flags & bit != 0)
                .map(|(_, failure)| failure)
                .collect(),
            version: v.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gimbal_says_what_it_can_do_in_words_and_its_limits_to_a_tenth() {
        let gimbal = core::Gimbal {
            id: core::Gimbal::id_of("v", 1, 1),
            vehicle_id: "v".into(),
            component_id: 1,
            device_id: 1,
            device_component_id: Some(154),
            device_silent_since_us: None,
            present: true,
            // What ArduPilot 4.8 says of the simulated gimbal (S406).
            capabilities: 206_847,
            limits: core::GimbalLimits {
                pitch_min_deg: Some(-114.018_59),
                pitch_max_deg: Some(33.976_4),
                ..core::GimbalLimits::default()
            },
            primary: Some(core::GimbalController {
                system_id: 255,
                component_id: 190,
            }),
            secondary: None,
            holder: core::GimbalHolder::Uxduck,
            yaw_locked: Some(false),
            retracted: false,
            neutral: false,
            failure_flags: 0,
            version: 3,
        };
        let wire = dto::Gimbal::from(&gimbal);
        assert_eq!(
            wire.capabilities,
            [
                dto::GimbalCapability::Retract,
                dto::GimbalCapability::Neutral,
                dto::GimbalCapability::RollAxis,
                dto::GimbalCapability::PitchAxis,
                dto::GimbalCapability::YawAxis,
                dto::GimbalCapability::YawFollow,
                dto::GimbalCapability::YawLock,
                dto::GimbalCapability::PointAtPlace,
            ]
        );
        assert_eq!(wire.flags, 206_847);
        assert_eq!(wire.limits.pitch_min_deg, Some(-114.0));
        assert_eq!(wire.limits.pitch_max_deg, Some(34.0));
        assert_eq!(wire.limits.yaw_min_deg, None);
        let json = serde_json::to_value(&wire).expect("JSON");
        assert_eq!(json["holder"], "uxduck");
        assert_eq!(json["primary"]["systemId"], 255);
        assert_eq!(json["deviceComponentId"], 154);
        assert!(json.get("secondary").is_none());
        assert!(json.get("retracted").is_none());
    }

    #[test]
    fn a_gimbal_says_what_is_wrong_with_it_in_words_and_nothing_while_it_is_well() {
        let with = |failure_flags: u32| {
            let gimbal = core::Gimbal {
                id: core::Gimbal::id_of("v", 1, 1),
                vehicle_id: "v".into(),
                component_id: 1,
                device_id: 1,
                device_component_id: Some(154),
                device_silent_since_us: None,
                present: true,
                capabilities: 206_847,
                limits: core::GimbalLimits::default(),
                primary: None,
                secondary: None,
                holder: core::GimbalHolder::NoOne,
                yaw_locked: Some(false),
                retracted: false,
                neutral: false,
                failure_flags,
                version: 1,
            };
            dto::Gimbal::from(&gimbal)
        };
        // `GIMBAL_DEVICE_ERROR_FLAGS`: a motor error and a power error.
        assert_eq!(
            with(32 | 16).failures,
            [dto::GimbalFailure::Power, dto::GimbalFailure::Motor]
        );
        assert_eq!(
            serde_json::to_value(with(32)).expect("JSON")["failures"],
            serde_json::json!(["motor"])
        );
        // Each of the seven, in the order of their bits.
        assert_eq!(
            serde_json::to_value(with(0x3f8)).expect("JSON")["failures"],
            serde_json::json!([
                "encoder",
                "power",
                "motor",
                "software",
                "comms",
                "calibrating",
                "no-manager"
            ])
        );
        // A limit it was sent past is not a failure, and a gimbal that is
        // well says nothing.
        assert!(with(1 | 2 | 4).failures.is_empty());
        let well = serde_json::to_value(with(0)).expect("JSON");
        assert!(well.get("failures").is_none(), "{well}");
    }

    #[test]
    fn an_action_goes_both_ways() {
        for action in [
            core::GimbalAction::TakeControl,
            core::GimbalAction::ReleaseControl,
            core::GimbalAction::Point {
                pitch_deg: -30.0,
                yaw_deg: 45.0,
                lock: true,
            },
            core::GimbalAction::Centre,
            core::GimbalAction::Follow,
            core::GimbalAction::HoldNorth,
            core::GimbalAction::LookAt {
                latitude_deg: 47.4,
                longitude_deg: 8.5,
                altitude_amsl_m: Some(500.0),
            },
        ] {
            let wire: dto::GimbalAction = action.into();
            assert_eq!(core::GimbalAction::from(wire), action);
            // The wire's JSON is the core's own.
            assert_eq!(
                serde_json::to_value(wire).expect("JSON"),
                serde_json::to_value(action).expect("JSON")
            );
        }
    }
}
