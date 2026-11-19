//! A vehicle's gimbals as they are found, held and pointed (ADR-0063 Y6,
//! S406): the MAVLink gimbal protocol's manager, which says what its
//! gimbal can do and who is in control of it.

use serde::{Deserialize, Serialize};

/// `GIMBAL_MANAGER_CAP_FLAGS`: the bits read here.
pub const CAP_RETRACT: u32 = 1;
pub const CAP_NEUTRAL: u32 = 2;
pub const CAP_YAW_FOLLOW: u32 = 512;
pub const CAP_YAW_LOCK: u32 = 1024;
pub const CAP_POINT_AT_PLACE: u32 = 131_072;

/// A MAVLink system and component, as a gimbal's manager names who is in
/// control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GimbalController {
    pub system_id: u8,
    pub component_id: u8,
}

/// Who has a gimbal, as people are told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalHolder {
    /// Its manager has not said yet.
    #[default]
    Unknown,
    /// No one: it does what its manager does by itself.
    NoOne,
    /// This ground station.
    Uxduck,
    /// The vehicle's autopilot.
    Autopilot,
    /// The mission the vehicle is flying, which points it. An ArduPilot
    /// says nothing of this in its manager's status (S406), so UXDuck
    /// works it out from the mission and the mode.
    Mission,
    /// Another ground station or component: `primary` says which.
    Other,
}

impl GimbalHolder {
    /// Whether pointing the gimbal takes it from someone, so that a person
    /// is asked first.
    #[must_use]
    pub fn held_elsewhere(self) -> bool {
        matches!(self, Self::Autopilot | Self::Mission | Self::Other)
    }
}

/// How far a gimbal turns, in degrees: roll, pitch from the horizon with
/// down negative, and yaw from the nose with right positive. None where
/// its manager does not say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GimbalLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_min_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_max_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_min_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_max_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_min_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_max_deg: Option<f32>,
}

/// A gimbal of a vehicle: one its manager answers for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gimbal {
    /// `<vehicle id>/<manager's component id>/<device id>`.
    pub id: String,
    pub vehicle_id: String,
    /// The MAVLink component of its manager: the autopilot's, or the
    /// gimbal's own when it manages itself. Commands go there.
    pub component_id: u8,
    /// What its manager calls it: 1 to 6 for one the manager drives
    /// itself, or the MAVLink component of the gimbal device.
    pub device_id: u8,
    /// The gimbal device's own MAVLink component, when it has one that is
    /// heard: what a camera names as the gimbal it sits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_component_id: Option<u8>,
    /// When the gimbal device's own heartbeat was last heard, in
    /// microseconds since the Unix epoch, while it is quieter than its
    /// vehicle: its heartbeats stopped and the autopilot's went on. An
    /// autopilot still answers for such a gimbal as its manager, so this
    /// is the one sign that it is not there. None while it is heard, and
    /// for a device that never was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_silent_since_us: Option<u64>,
    /// Whether its manager's heartbeats are heard. A gimbal that is gone
    /// is kept, and asked again when it returns.
    pub present: bool,
    /// `GIMBAL_MANAGER_CAP_FLAGS`: what it can do.
    pub capabilities: u32,
    pub limits: GimbalLimits,
    /// Who its manager says is in control, and who may also move it. None
    /// for no one, and before the manager has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<GimbalController>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<GimbalController>,
    pub holder: GimbalHolder,
    /// Whether its yaw is held from north while the vehicle turns, and not
    /// from the nose: from the device's own flags, once it has said them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_locked: Option<bool>,
    /// Tucked away, as the device says.
    #[serde(default)]
    pub retracted: bool,
    /// At its neutral position, as the device says.
    #[serde(default)]
    pub neutral: bool,
    /// `GIMBAL_DEVICE_ERROR_FLAGS`: what the device says is wrong with it,
    /// such as a motor or an encoder that failed. Zero for nothing.
    #[serde(default)]
    pub failure_flags: u32,
    pub version: u64,
}

impl Gimbal {
    /// The id of a vehicle's gimbal.
    #[must_use]
    pub fn id_of(vehicle_id: &str, component_id: u8, device_id: u8) -> String {
        format!("{vehicle_id}/{component_id}/{device_id}")
    }

    /// Whether it can be told a place to look at.
    #[must_use]
    pub fn points_at_places(&self) -> bool {
        self.capabilities & CAP_POINT_AT_PLACE != 0
    }

    /// Whether it sits on the gimbal a camera names
    /// (`CAMERA_INFORMATION.gimbal_device_id`): the device's component, or
    /// 1 to 6 for one with none.
    #[must_use]
    pub fn named_by_camera(&self, gimbal_device_id: u8) -> bool {
        gimbal_device_id != 0
            && (self.device_component_id == Some(gimbal_device_id)
                || self.device_id == gimbal_device_id)
    }
}

/// What to have a gimbal do (ADR-0063 Y6, S406): each is one command of
/// the MAVLink gimbal protocol, which its manager acknowledges.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum GimbalAction {
    /// Puts this ground station in primary control
    /// (`MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE`). UXDuck sends it by itself
    /// before the first movement.
    TakeControl,
    /// Gives primary control up, so that the next to ask has it.
    ReleaseControl,
    /// Points it: pitch from the horizon with down negative, and yaw with
    /// right positive (`MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW`). The yaw is
    /// from the vehicle's nose and turns with it, or with `lock` from
    /// north and held there.
    Point {
        pitch_deg: f32,
        yaw_deg: f32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lock: bool,
    },
    /// Level and straight ahead, following the nose.
    Centre,
    /// Lets the yaw turn with the vehicle's nose, from where it points
    /// now.
    Follow,
    /// Holds the yaw from north while the vehicle turns, from where it
    /// points now.
    HoldNorth,
    /// Keeps looking at a place (`MAV_CMD_DO_SET_ROI_LOCATION`). With no
    /// height, UXDuck uses the height of the ground there.
    LookAt {
        latitude_deg: f64,
        longitude_deg: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        altitude_amsl_m: Option<f64>,
    },
}

impl GimbalAction {
    /// For people: "Point the gimbal", "Centre the gimbal".
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::TakeControl => "Take control of the gimbal",
            Self::ReleaseControl => "Give back control of the gimbal",
            Self::Point { .. } => "Point the gimbal",
            Self::Centre => "Centre the gimbal",
            Self::Follow => "Have the gimbal follow the nose",
            Self::HoldNorth => "Have the gimbal hold north",
            Self::LookAt { .. } => "Point the gimbal at a place",
        }
        .into()
    }

    /// Whether it moves the gimbal, so that control is taken first.
    #[must_use]
    pub fn moves(&self) -> bool {
        !matches!(self, Self::TakeControl | Self::ReleaseControl)
    }

    /// Whether it takes the gimbal from whoever holds it: every movement,
    /// and taking control itself.
    #[must_use]
    pub fn takes(&self) -> bool {
        !matches!(self, Self::ReleaseControl)
    }

    /// Why it cannot be sent, when its numbers are not ones a gimbal can
    /// be given.
    #[must_use]
    pub fn invalid(&self) -> Option<&'static str> {
        match *self {
            Self::Point {
                pitch_deg, yaw_deg, ..
            } if !(pitch_deg.is_finite()
                && yaw_deg.is_finite()
                && (-180.0..=180.0).contains(&pitch_deg)
                && (-360.0..=360.0).contains(&yaw_deg)) =>
            {
                Some("a gimbal's pitch is within 180° and its yaw within 360°")
            }
            Self::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m,
            } if !((-90.0..=90.0).contains(&latitude_deg)
                && (-180.0..=180.0).contains(&longitude_deg)
                && altitude_amsl_m.is_none_or(f64::is_finite)) =>
            {
                Some("a place needs a latitude within 90° and a longitude within 180°")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_reads_as_the_wire_says() {
        let point: GimbalAction =
            serde_json::from_str(r#"{"type":"point","pitchDeg":-30.0,"yawDeg":45.0}"#)
                .expect("a point");
        assert_eq!(
            point,
            GimbalAction::Point {
                pitch_deg: -30.0,
                yaw_deg: 45.0,
                lock: false
            }
        );
        assert_eq!(
            serde_json::to_string(&point).expect("JSON"),
            r#"{"type":"point","pitchDeg":-30.0,"yawDeg":45.0}"#
        );
        assert_eq!(
            serde_json::to_string(&GimbalAction::HoldNorth).expect("JSON"),
            r#"{"type":"hold-north"}"#
        );
        let place: GimbalAction =
            serde_json::from_str(r#"{"type":"look-at","latitudeDeg":47.4,"longitudeDeg":8.5}"#)
                .expect("a place");
        assert_eq!(
            place,
            GimbalAction::LookAt {
                latitude_deg: 47.4,
                longitude_deg: 8.5,
                altitude_amsl_m: None
            }
        );
    }

    #[test]
    fn numbers_a_gimbal_cannot_be_given_are_refused() {
        let point = |pitch_deg, yaw_deg| GimbalAction::Point {
            pitch_deg,
            yaw_deg,
            lock: false,
        };
        assert!(point(-90.0, 180.0).invalid().is_none());
        assert!(point(f32::NAN, 0.0).invalid().is_some());
        assert!(point(0.0, 400.0).invalid().is_some());
        let place = |latitude_deg| GimbalAction::LookAt {
            latitude_deg,
            longitude_deg: 8.5,
            altitude_amsl_m: None,
        };
        assert!(place(47.4).invalid().is_none());
        assert!(place(91.0).invalid().is_some());
        assert!(GimbalAction::Centre.invalid().is_none());
    }

    #[test]
    fn only_another_holder_is_asked_about() {
        for (holder, elsewhere) in [
            (GimbalHolder::Unknown, false),
            (GimbalHolder::NoOne, false),
            (GimbalHolder::Uxduck, false),
            (GimbalHolder::Autopilot, true),
            (GimbalHolder::Mission, true),
            (GimbalHolder::Other, true),
        ] {
            assert_eq!(holder.held_elsewhere(), elsewhere, "{holder:?}");
        }
        assert!(GimbalAction::Centre.moves() && GimbalAction::Centre.takes());
        assert!(!GimbalAction::TakeControl.moves() && GimbalAction::TakeControl.takes());
        assert!(!GimbalAction::ReleaseControl.moves() && !GimbalAction::ReleaseControl.takes());
    }

    #[test]
    fn a_camera_names_its_gimbal_by_component_or_by_number() {
        let gimbal = Gimbal {
            id: Gimbal::id_of("v", 1, 1),
            vehicle_id: "v".into(),
            component_id: 1,
            device_id: 1,
            device_component_id: Some(154),
            device_silent_since_us: None,
            present: true,
            capabilities: CAP_POINT_AT_PLACE | CAP_YAW_LOCK,
            limits: GimbalLimits::default(),
            primary: None,
            secondary: None,
            holder: GimbalHolder::NoOne,
            yaw_locked: None,
            retracted: false,
            neutral: false,
            failure_flags: 0,
            version: 1,
        };
        assert_eq!(gimbal.id, "v/1/1");
        assert!(gimbal.named_by_camera(154) && gimbal.named_by_camera(1));
        assert!(!gimbal.named_by_camera(0) && !gimbal.named_by_camera(2));
        assert!(gimbal.points_at_places());
    }
}
