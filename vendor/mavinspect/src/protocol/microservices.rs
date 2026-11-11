//! MAVLink [microservices](https://mavlink.io/en/services/) support.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::protocol::Dialect;
use bitflags::bitflags;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

macro_rules! microservices_list_macro {
    ($rule: ident) => {
        [
            $rule!(HEARTBEAT),
            $rule!(MISSION),
            $rule!(PARAMETER),
            $rule!(PARAMETER_EXT),
            $rule!(COMMAND),
            $rule!(MANUAL_CONTROL),
            $rule!(CAMERA),
            $rule!(GIMBAL_V1),
            $rule!(GIMBAL_V2),
            $rule!(ARM_AUTH),
            $rule!(IMAGE_TRANSMISSION),
            $rule!(FTP),
            $rule!(LANDING_TARGET),
            $rule!(PING),
            $rule!(PATH_PLANNING),
            $rule!(BATTERY),
            $rule!(TERRAIN),
            $rule!(TUNNEL),
            $rule!(OPEN_DRONE_ID),
            $rule!(HIGH_LATENCY),
            $rule!(COMPONENT_METADATA),
            $rule!(PAYLOAD),
            $rule!(TRAFFIC_MANAGEMENT),
            $rule!(EVENTS_INTERFACE),
            $rule!(TIME_SYNC),
        ]
    };
}

macro_rules! microservice_flag_name {
    ($subject: ident) => {
        stringify!($subject)
    };
}

bitflags! {
    /// Supported MAVLink [microservices](https://mavlink.io/en/services/).
    ///
    /// MAVLink microservices define protocols for various interactions. [`Microservices`] flags are used both for
    /// discovering capabilities within a dialect and filtering out irrelevant messages, enums, and enum entries.
    ///
    /// > At the moment only microservices explicitly listed in [MAVLink docs](https://mavlink.io/en/services/) are
    /// > supported. In future versions of MAVInspect additional microservices will be added (like telemetry).
    ///
    /// To discover which microservices are supported within a dialect, use
    /// [`Dialect::microservices`](super::Dialect::microservices).
    ///
    /// To discover which messages, enums, and command are supported within current microservices set, use:
    ///
    /// * [`Microservices::messages`]
    /// * [`Microservices::enums`]
    /// * [`Microservices::commands`]
    ///
    /// To filter out irrelevant entities, construct a filter from
    /// [`Filter::by_microservices`](super::Filter::by_microservices) and pass it to:
    ///
    /// * [`Dialect::filtered`](super::Dialect::filtered) or
    /// * [`Dialect::retain`](super::Dialect::retain).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Microservices;
    ///
    /// // Define microservices
    /// let microservices = Microservices::HEARTBEAT | Microservices::CAMERA | Microservices::FTP | Microservices::MISSION;
    ///
    /// // Messages should contain `HEARTBEAT` and `FILE_TRANSFER_PROTOCOL`
    /// assert!(microservices.messages().any(|s| s.as_ref() == "HEARTBEAT"));
    /// assert!(microservices.messages().any(|s| s.as_ref() == "FILE_TRANSFER_PROTOCOL"));
    /// // Enums should contain `MAV_MISSION_TYPE`
    /// assert!(microservices.enums().any(|s| s.as_ref() == "MAV_MISSION_TYPE"));
    /// // Commands should contain `MAV_CMD_REQUEST_MESSAGE`
    /// assert!(microservices.commands().any(|s| s.as_ref() == "MAV_CMD_REQUEST_MESSAGE"));
    /// // However, enums won't contain `MAV_CMD` since the latter would imply that ALL commands should be included.
    /// assert!(!microservices.enums().any(|s| s.as_ref() == "MAV_CMD"));
    /// ```
    ///
    /// You could also create an instance of [`Microservices`] from a comma-separated list:
    ///
    /// ```rust
    /// use mavinspect::protocol::Microservices;
    ///
    /// // Define microservices
    /// let microservices = Microservices::from("HEARTBEAT,FTP");
    ///
    /// // Messages should contain `HEARTBEAT` and `FILE_TRANSFER_PROTOCOL`
    /// assert!(microservices.messages().any(|s| s.as_ref() == "HEARTBEAT"));
    /// assert!(microservices.messages().any(|s| s.as_ref() == "FILE_TRANSFER_PROTOCOL"));
    /// ```
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    #[cfg_attr(feature = "serde", serde(transparent))]
    pub struct Microservices: u64 {
        /// Supports [heartbeat](https://mavlink.io/en/services/heartbeat.html) protocol.
        const HEARTBEAT =          1;

        /// Supports [mission](https://mavlink.io/en/services/mission.html) protocol.
        const MISSION =            1 << 1;

        /// Supports [parameter](https://mavlink.io/en/services/parameter.html) protocol.
        const PARAMETER =          1 << 2;

        /// Supports [extended parameter](https://mavlink.io/en/services/parameter_ext.html) protocol.
        const PARAMETER_EXT =      1 << 3;

        /// Supports [command](https://mavlink.io/en/services/command.html) protocol.
        const COMMAND =            1 << 4;

        /// Supports [manual control](https://mavlink.io/en/services/manual_control.html) protocol.
        const MANUAL_CONTROL =     1 << 5;

        /// Supports [camera](https://mavlink.io/en/services/camera.html) protocol.
        const CAMERA =             1 << 6;

        /// Supports [gimbal V1](https://mavlink.io/en/services/gimbal.html) protocol.
        const GIMBAL_V1 =          1 << 7;

        /// Supports [gimbal v2](https://mavlink.io/en/services/gimbal_v2.html) protocol.
        const GIMBAL_V2 =          1 << 8;

        /// Supports [arm authorization](https://mavlink.io/en/services/arm_authorization.html) protocol.
        const ARM_AUTH =           1 << 9;

        /// Supports [image transmission](https://mavlink.io/en/services/image_transmission.html) protocol.
        const IMAGE_TRANSMISSION = 1 << 10;

        /// Supports [file transfer](https://mavlink.io/en/services/ftp.html) protocol.
        const FTP =                1 << 11;

        /// Supports [landing target](https://mavlink.io/en/services/landing_target.html) protocol.
        const LANDING_TARGET =     1 << 12;

        /// Supports [ping](https://mavlink.io/en/services/ping.html) protocol.
        const PING =               1 << 13;

        /// Supports [path planning](https://mavlink.io/en/services/trajectory.html) protocol.
        const PATH_PLANNING =      1 << 14;

        /// Supports [battery](https://mavlink.io/en/services/battery.html) protocol.
        const BATTERY =            1 << 15;

        /// Supports [terrain](https://mavlink.io/en/services/terrain.html) protocol.
        const TERRAIN =            1 << 16;

        /// Supports [tunnel](https://mavlink.io/en/services/tunnel.html) protocol.
        const TUNNEL =             1 << 17;

        /// Supports [Open Drone ID](https://mavlink.io/en/services/opendroneid.html) protocol.
        const OPEN_DRONE_ID =      1 << 18;

        /// Supports [high latency](https://mavlink.io/en/services/high_latency.html) protocol.
        const HIGH_LATENCY =       1 << 19;

        /// Supports [component metadata](https://mavlink.io/en/services/component_information.html) protocol.
        const COMPONENT_METADATA = 1 << 20;

        /// Supports [payload](https://mavlink.io/en/services/payload.html) protocol.
        const PAYLOAD =            1 << 21;

        /// Supports [traffic management](https://mavlink.io/en/services/traffic_management.html) protocol.
        const TRAFFIC_MANAGEMENT = 1 << 22;

        /// Supports [events interface](https://mavlink.io/en/services/events.html) protocol.
        const EVENTS_INTERFACE =   1 << 23;

        /// Supports [time synchronization](https://mavlink.io/en/services/timesync.html) protocol.
        const TIME_SYNC =          1 << 24;
    }
}

impl<S: AsRef<str>> From<S> for Microservices {
    /// Constructs [`Microservices`] from a [`String`]-like object, see [`Microservices::from_flag_names`].
    fn from(value: S) -> Self {
        Microservices::from_flag_names(value)
    }
}

macro_rules! define_specs {
    ($subject: ident) => {
        #[doc = "`"]
        #[doc = stringify!($subject)]
        #[doc = "` required for supported MAVLink micro-services."]
        pub fn $subject(&self) -> impl Iterator<Item = Cow<str>> {
            let mut $subject: Vec<Cow<str>> = Vec::default();

            for (service, spec) in Self::specs() {
                if self.bits() & service.bits() == service.bits() {
                    for &entry in spec.$subject {
                        $subject.push(entry.into())
                    }
                }
            }

            $subject.into_iter()
        }
    };
}

impl Microservices {
    /// Number of all possible microservices.
    pub const COUNT: u8 = 25;

    /// The names of all microservice flags.
    pub const FLAG_NAMES: [&'static str; Self::COUNT as usize] =
        microservices_list_macro!(microservice_flag_name);

    /// Discovers micro-services capabilities from existing dialect.
    pub fn from_dialect(dialect: &Dialect) -> Self {
        let mut microservices = Microservices::default();

        for (msrv, spec) in Self::specs() {
            if spec.supported_in(dialect) {
                microservices |= msrv;
            }
        }

        microservices
    }

    /// Map of microservice `name` -> `flag` (i.e. "HEARTBEAT" -> [`Microservices::HEARTBEAT`]).
    ///
    /// Keys of this map can be used as canonical names for microservices.
    pub fn flags_map() -> HashMap<&'static str, Microservices> {
        macro_rules! name_flag_pair {
            ($subject: ident) => {
                (stringify!($subject), Microservices::$subject)
            };
        }

        let map = microservices_list_macro!(name_flag_pair)
            .to_vec()
            .iter()
            .cloned()
            .collect();

        map
    }

    /// Converts comma-separated list of flag names into [`Microservices`].
    ///
    /// This function supports both `,` and `, ` separated strings.
    ///
    /// Entries will be converted to `SCREAMING_SNAKE_CASE`. Which means that names `TimeSync` or
    /// `high_latency` will be recognized.
    ///
    /// If entry can't be recognized as a flag, it will be skipped.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Microservices;
    ///
    /// // Define microservices
    /// let microservices = Microservices::from_flag_names("HEARTBEAT,FTP");
    ///
    /// // Messages should contain `HEARTBEAT` and `FILE_TRANSFER_PROTOCOL`
    /// assert!(microservices.messages().any(|s| s.as_ref() == "HEARTBEAT"));
    /// assert!(microservices.messages().any(|s| s.as_ref() == "FILE_TRANSFER_PROTOCOL"));
    /// ```
    ///
    /// You could also use simply [`Microservices::from`] since [`From`] is implemented.
    pub fn from_flag_names<S: AsRef<str>>(value: S) -> Microservices {
        let mut microservices = Microservices::default();

        for part in value.as_ref().split(',') {
            let part = heck::AsShoutySnakeCase(part.trim()).to_string();
            for (flag_name, flag) in Self::flags_map() {
                if flag_name == part {
                    microservices |= flag;
                }
            }
        }

        microservices
    }

    /// Messages required for supported MAVLink microservices.
    ///
    /// If [`Self::commands`] is not empty, then all messages required for [Self::COMMAND] protocol will be included.
    pub fn messages(&self) -> impl Iterator<Item=Cow<str>> {
        let mut messages: Vec<Cow<str>> = Vec::default();

        for (service, spec) in Self::specs() {
            if self.bits() & service.bits() == service.bits() {
                for &msg in spec.messages {
                    messages.push(msg.into())
                }
            }
        }

        messages.into_iter()
    }

    /// Produces a comma-separated list of microservice flag names.
    ///
    /// The flags are listed from the lower to the higher bits.
    ///
    /// # Example
    ///
    /// ```rust
    /// use mavinspect::protocol::Microservices;
    ///
    /// assert_eq!(
    ///     (Microservices::FTP | Microservices::HEARTBEAT).join_flag_names(","),
    ///     "HEARTBEAT,FTP"
    /// );
    /// ```
    pub fn join_flag_names(&self, separator: &str) -> String {
        self.iter_names()
            .map(|(key, _)| key)
            .collect::<Vec<_>>()
            .join(separator)
    }

    define_specs!(enums);
    define_specs!(commands);

    fn specs() -> HashMap<Microservices, MicroserviceSpec> {
        macro_rules! flag_spec_pair {
            ($subject: ident) => {
                (Microservices::$subject, $subject)
            };
        }

        microservices_list_macro!(flag_spec_pair)
            .iter()
            .cloned()
            .collect()
    }

    /// Returns a MavLink documentation name for a specified microservice.
    pub fn doc_link<S: AsRef<str>>(key: S) -> Option<&'static str> {
        let map: HashMap<String, &str> = Self::DOC_LINKS_MAP
            .iter()
            .map(|(k, v)| (k.join_flag_names(""), *v))
            .collect();
        map.get(key.as_ref()).copied()
    }

    /// Map from microservice flags to links to corresponding MavLink documentation pages.
    ///
    /// For example, [`HEARTBEAT`](Self::HEARTBEAT) is mapped to
    /// <https://mavlink.io/en/services/heartbeat.html>.
    pub const DOC_LINKS_MAP: [(Self, &'static str); Self::COUNT as usize] = [
        (
            Self::HEARTBEAT,
            "https://mavlink.io/en/services/heartbeat.html",
        ),
        (Self::MISSION, "https://mavlink.io/en/services/mission.html"),
        (
            Self::PARAMETER,
            "https://mavlink.io/en/services/parameter.html",
        ),
        (
            Self::PARAMETER_EXT,
            "https://mavlink.io/en/services/parameter_ext.html",
        ),
        (Self::COMMAND, "https://mavlink.io/en/services/command.html"),
        (
            Self::MANUAL_CONTROL,
            "https://mavlink.io/en/services/manual_control.html",
        ),
        (Self::CAMERA, "https://mavlink.io/en/services/camera.html"),
        (
            Self::GIMBAL_V1,
            "https://mavlink.io/en/services/gimbal.html",
        ),
        (
            Self::GIMBAL_V2,
            "https://mavlink.io/en/services/gimbal_v2.html",
        ),
        (
            Self::ARM_AUTH,
            "https://mavlink.io/en/services/arm_authorization.html",
        ),
        (
            Self::IMAGE_TRANSMISSION,
            "https://mavlink.io/en/services/image_transmission.html",
        ),
        (Self::FTP, "https://mavlink.io/en/services/ftp.html"),
        (
            Self::LANDING_TARGET,
            "https://mavlink.io/en/services/landing_target.html",
        ),
        (Self::PING, "https://mavlink.io/en/services/ping.html"),
        (
            Self::PATH_PLANNING,
            "https://mavlink.io/en/services/trajectory.html",
        ),
        (Self::BATTERY, "https://mavlink.io/en/services/battery.html"),
        (Self::TERRAIN, "https://mavlink.io/en/services/terrain.html"),
        (Self::TUNNEL, "https://mavlink.io/en/services/tunnel.html"),
        (
            Self::OPEN_DRONE_ID,
            "https://mavlink.io/en/services/opendroneid.html",
        ),
        (
            Self::HIGH_LATENCY,
            "https://mavlink.io/en/services/high_latency.html",
        ),
        (
            Self::COMPONENT_METADATA,
            "https://mavlink.io/en/services/component_information.html",
        ),
        (Self::PAYLOAD, "https://mavlink.io/en/services/payload.html"),
        (
            Self::TRAFFIC_MANAGEMENT,
            "https://mavlink.io/en/services/traffic_management.html",
        ),
        (
            Self::EVENTS_INTERFACE,
            "https://mavlink.io/en/services/events.html",
        ),
        (
            Self::TIME_SYNC,
            "https://mavlink.io/en/services/timesync.html",
        ),
    ];
}

#[derive(Clone)]
struct MicroserviceSpec {
    messages: &'static [&'static str],
    enums: &'static [&'static str],
    commands: &'static [&'static str],
}

impl MicroserviceSpec {
    /// Checks that provided MAVLink [`Dialect`] matches the microservice specification.
    pub fn supported_in(&self, dialect: &Dialect) -> bool {
        if !self.supported_for_messages_in(dialect) {
            return false;
        }

        if !self.supported_for_enums_in(dialect) {
            return false;
        }

        if !self.supported_for_commands_in(dialect) {
            return false;
        }

        true
    }

    fn supported_for_messages_in(&self, dialect: &Dialect) -> bool {
        self.match_entities(
            self.messages,
            dialect
                .messages()
                .into_iter()
                .map(|msg| msg.name())
                .collect(),
        )
    }

    fn supported_for_enums_in(&self, dialect: &Dialect) -> bool {
        self.match_entities(
            self.enums,
            dialect.enums().into_iter().map(|enm| enm.name()).collect(),
        )
    }

    fn supported_for_commands_in(&self, dialect: &Dialect) -> bool {
        if !self.commands.is_empty() && !dialect.contains_enum_with_name("MAV_CMD") {
            return false;
        }

        self.match_entities(
            self.commands,
            dialect
                .commands()
                .into_iter()
                .map(|cmd| cmd.name())
                .collect(),
        )
    }

    fn match_entities(&self, patterns: &[&str], entities: Vec<&str>) -> bool {
        if patterns.is_empty() {
            return true;
        }

        let mut found_patterns = HashSet::new();
        let expected_patterns = HashSet::from_iter(patterns.iter().copied());

        for pattern in patterns.iter() {
            if pattern.ends_with("*") {
                if let Some(prefix) = pattern.strip_suffix('*') {
                    for entry in entities.iter() {
                        if entry.starts_with(prefix) {
                            found_patterns.insert(*pattern);
                        }
                    }
                }
            } else {
                for entry in entities.iter() {
                    if entry == pattern {
                        found_patterns.insert(*pattern);
                    }
                }
            }
        }

        expected_patterns.is_subset(&found_patterns)
    }
}

const HEARTBEAT: MicroserviceSpec = MicroserviceSpec {
    messages: &["HEARTBEAT"],
    enums: &[],
    commands: &[],
};

const MISSION: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "MISSION_REQUEST", // / Deprecated
        "MISSION_ITEM",    // \
        "MISSION_REQUEST_LIST",
        "MISSION_COUNT",
        "MISSION_REQUEST_INT",
        "MISSION_ITEM_INT",
        "MISSION_ACK",
        "MISSION_CURRENT",
        "MISSION_SET_CURRENT",
        "STATUSTEXT",
        "MISSION_CLEAR_ALL",
        "MISSION_ITEM_REACHED",
    ],
    enums: &[
        "MAV_MISSION_TYPE",
        "MAV_MISSION_RESULT",
        "MAV_FRAME",
        // `MAV_CMD` is omitted
    ],
    commands: &[
        "MAV_CMD_DO_SET_MISSION_CURRENT",
        "MAV_CMD_NAV_*",
        "MAV_CMD_NAV_FENCE_*",
        "MAV_CMD_DO_*",
        "MAV_CMD_CONDITION_*",
    ],
};

const PARAMETER: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "PARAM_REQUEST_LIST",
        "PARAM_REQUEST_READ",
        "PARAM_SET",
        "PARAM_VALUE",
    ],
    enums: &["MAV_PARAM_TYPE", "MAV_PROTOCOL_CAPABILITY"],
    commands: &[],
};

const PARAMETER_EXT: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "PARAM_EXT_REQUEST_LIST",
        "PARAM_EXT_VALUE",
        "PARAM_EXT_REQUEST_READ",
        "PARAM_EXT_SET",
        "PARAM_EXT_ACK",
    ],
    enums: &["MAV_PARAM_EXT_TYPE", "PARAM_ACK"],
    commands: &[],
};

const COMMAND: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &["MAV_CMD"],
    commands: &["*"],
};

const MANUAL_CONTROL: MicroserviceSpec = MicroserviceSpec {
    messages: &["MANUAL_CONTROL"],
    enums: &[],
    commands: &[],
};

const CAMERA: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "CAMERA_INFORMATION",
        "CAMERA_SETTINGS",
        "VIDEO_STREAM_INFORMATION",
        "VIDEO_STREAM_STATUS",
        "STORAGE_INFORMATION",
        "CAMERA_CAPTURE_STATUS",
        "CAMERA_IMAGE_CAPTURED",
        "CAMERA_FOV_STATUS",
        "CAMERA_TRACKING_IMAGE_STATUS",
        "CAMERA_TRACKING_GEO_STATUS",
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[],
    commands: &[
        "MAV_CMD_REQUEST_MESSAGE",
        "MAV_CMD_SET_CAMERA_MODE",
        "MAV_CMD_STORAGE_FORMAT",
        "MAV_CMD_IMAGE_START_CAPTURE",
        "MAV_CMD_IMAGE_STOP_CAPTURE",
        "MAV_CMD_VIDEO_START_CAPTURE",
        "MAV_CMD_VIDEO_STOP_CAPTURE",
        "MAV_CMD_VIDEO_START_STREAMING",
        "MAV_CMD_VIDEO_STOP_STREAMING",
        "MAV_CMD_REQUEST_CAMERA_SETTINGS",          // /
        "MAV_CMD_REQUEST_CAMERA_INFORMATION",       // |
        "MAV_CMD_REQUEST_VIDEO_STREAM_INFORMATION", // | Deprecated in favor of `MAV_CMD_REQUEST_MESSAGE`
        "MAV_CMD_REQUEST_VIDEO_STREAM_STATUS",      // | (but still mandatory for implementation)
        "MAV_CMD_REQUEST_STORAGE_INFORMATION",      // |
        "MAV_CMD_REQUEST_CAMERA_CAPTURE_STATUS",    // \
        "MAV_CMD_CAMERA_TRACK_POINT",
        "MAV_CMD_CAMERA_TRACK_RECTANGLE",
        "MAV_CMD_CAMERA_STOP_TRACKING",
    ],
};

const GIMBAL_V1: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "MOUNT_ORIENTATION",
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[
        "MAV_MOUNT_MODE",
        // `MAV_CMD` is omitted
    ],
    commands: &[
        "MAV_CMD_DO_MOUNT_CONFIGURE",
        "MAV_CMD_DO_MOUNT_CONTROL",
        "MAV_CMD_DO_MOUNT_CONTROL_QUAT",
        "MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN",
    ],
};

const GIMBAL_V2: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "GIMBAL_MANAGER_INFORMATION",
        "GIMBAL_MANAGER_STATUS",
        "GIMBAL_MANAGER_SET_ATTITUDE",
        "GIMBAL_MANAGER_SET_MANUAL_CONTROL",
        "GIMBAL_MANAGER_SET_PITCHYAW",
        "GIMBAL_DEVICE_INFORMATION",
        "GIMBAL_DEVICE_SET_ATTITUDE",
        "GIMBAL_DEVICE_ATTITUDE_STATUS",
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &["GIMBAL_MANAGER_FLAGS", "GIMBAL_MANAGER_CAP_FLAGS"],
    commands: &[
        "MAV_CMD_REQUEST_MESSAGE",
        "MAV_CMD_SET_MESSAGE_INTERVAL",
        "MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE",
        "MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW",
        "MAV_CMD_DO_SET_ROI_LOCATION",
        "MAV_CMD_DO_SET_ROI_WPNEXT_OFFSET",
        "MAV_CMD_DO_SET_ROI_SYSID",
        "MAV_CMD_DO_SET_ROI_NONE",
        // "MAV_CMD_DO_GIMBAL_MANAGER_TRACK_POINT",
        // "MAV_CMD_DO_GIMBAL_MANAGER_TRACK_RECTANGLE",
    ],
};

const ARM_AUTH: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[
        // `MAV_CMD` is omitted
    ],
    commands: &[
        "MAV_CMD_ARM_AUTHORIZATION_REQUEST",
        "MAV_CMD_COMPONENT_ARM_DISARM",
    ],
};

const IMAGE_TRANSMISSION: MicroserviceSpec = MicroserviceSpec {
    messages: &["DATA_TRANSMISSION_HANDSHAKE", "ENCAPSULATED_DATA"],
    enums: &[],
    commands: &[],
};

const FTP: MicroserviceSpec = MicroserviceSpec {
    messages: &["FILE_TRANSFER_PROTOCOL"],
    enums: &[],
    commands: &[],
};

const LANDING_TARGET: MicroserviceSpec = MicroserviceSpec {
    messages: &["LANDING_TARGET"],
    enums: &[],
    commands: &[],
};

const PING: MicroserviceSpec = MicroserviceSpec {
    messages: &["PING"],
    enums: &[],
    commands: &[],
};

const PATH_PLANNING: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "TRAJECTORY_REPRESENTATION_WAYPOINTS",
        "TRAJECTORY_REPRESENTATION_BEZIER",
    ],
    enums: &[],
    commands: &["*"],
};

const BATTERY: MicroserviceSpec = MicroserviceSpec {
    messages: &["BATTERY_STATUS", "SMART_BATTERY_INFO"],
    enums: &["MAV_BATTERY_FAULT", "MAV_BATTERY_MODE"],
    commands: &[],
};

const TERRAIN: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "TERRAIN_REQUEST",
        "TERRAIN_DATA",
        "TERRAIN_REPORT",
        "TERRAIN_CHECK",
    ],
    enums: &[],
    commands: &[],
};

const TUNNEL: MicroserviceSpec = MicroserviceSpec {
    messages: &["TUNNEL"],
    enums: &["MAV_TUNNEL_PAYLOAD_TYPE"],
    commands: &[],
};

const OPEN_DRONE_ID: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "OPEN_DRONE_ID_BASIC_ID",
        "OPEN_DRONE_ID_LOCATION",
        "OPEN_DRONE_ID_AUTHENTICATION",
        "OPEN_DRONE_ID_SELF_ID",
        "OPEN_DRONE_ID_SYSTEM",
        "OPEN_DRONE_ID_OPERATOR_ID",
        "OPEN_DRONE_ID_MESSAGE_PACK",
        "OPEN_DRONE_ID_ARM_STATUS",
        "OPEN_DRONE_ID_SYSTEM_UPDATE",
    ],
    enums: &["MAV_COMPONENT"],
    commands: &[],
};

const HIGH_LATENCY: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "HIGH_LATENCY2",
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[
        "HL_FAILURE_FLAG",
        // `MAV_CMD` is omitted
    ],
    commands: &["MAV_CMD_CONTROL_HIGH_LATENCY"],
};

const COMPONENT_METADATA: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "COMPONENT_METADATA",
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[
        "COMP_METADATA_TYPE",
        // `MAV_CMD` is omitted
    ],
    commands: &["MAV_CMD_REQUEST_MESSAGE"],
};

const PAYLOAD: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        // Command protocol messages:
        "COMMAND_INT",
        "COMMAND_LONG",
        "COMMAND_ACK",
        "COMMAND_CANCEL",
    ],
    enums: &[
        // `MAV_CMD` is omitted
    ],
    commands: &[
        "MAV_CMD_DO_GRIPPER",
        "MAV_CMD_DO_WINCH",
        "MAV_CMD_ILLUMINATOR_ON_OFF",
        "MAV_CMD_DO_SET_ACTUATOR", // /
        "MAV_CMD_DO_SET_SERVO",    // | Generic payload commands
        "MAV_CMD_DO_SET_RELAY",    // |
        "MAV_CMD_DO_REPEAT_RELAY", // \
        "MAV_CMD_NAV_PAYLOAD_PLACE",
    ],
};

const TRAFFIC_MANAGEMENT: MicroserviceSpec = MicroserviceSpec {
    messages: &["UTM_GLOBAL_POSITION", "ADSB_VEHICLE"],
    enums: &[
        "UTM_FLIGHT_STATE",
        "UTM_DATA_AVAIL_FLAGS",
        "ADSB_ALTITUDE_TYPE",
        "ADSB_EMITTER_TYPE",
        "ADSB_FLAGS",
    ],
    commands: &[],
};

const EVENTS_INTERFACE: MicroserviceSpec = MicroserviceSpec {
    messages: &[
        "EVENT",
        "CURRENT_EVENT_SEQUENCE",
        "REQUEST_EVENT",
        "RESPONSE_EVENT_ERROR",
    ],
    enums: &["MAV_EVENT_CURRENT_SEQUENCE_FLAGS", "MAV_EVENT_ERROR_REASON"],
    commands: &[],
};

const TIME_SYNC: MicroserviceSpec = MicroserviceSpec {
    messages: &["TIMESYNC"],
    enums: &[],
    commands: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microservices_basic() {
        let microservices = Microservices::HEARTBEAT | Microservices::PING;
        assert!(microservices.messages().any(|s| s.as_ref() == "HEARTBEAT"));
        assert!(microservices.messages().any(|s| s.as_ref() == "PING"));
        assert!(!microservices
            .messages()
            .any(|s| s.as_ref() == "COMMAND_INT"));
    }

    #[test]
    fn microservices_flags() {
        for (name, flag) in Microservices::flags_map() {
            assert_eq!(Microservices::from(name), flag);
        }

        let microservices = Microservices::from("HEARTBEAT,COMMAND, MISSION");
        assert!(microservices.contains(Microservices::HEARTBEAT));
        assert!(microservices.contains(Microservices::COMMAND));
        assert!(microservices.contains(Microservices::MISSION));
        assert!(!microservices.contains(Microservices::FTP));

        let all_flags: Vec<&str> = Microservices::flags_map().keys().cloned().collect();

        let microservices = Microservices::from(all_flags.join(","));
        for &flag in Microservices::flags_map().values() {
            assert!(microservices.contains(flag));
        }

        let microservices = Microservices::from(all_flags.join(", "));
        for &flag in Microservices::flags_map().values() {
            assert!(microservices.contains(flag));
        }
    }

    #[test]
    fn microservice_string_flags() {
        for name in Microservices::FLAG_NAMES.iter() {
            assert_eq!(Microservices::from(name).join_flag_names(""), *name);
        }
    }
}
