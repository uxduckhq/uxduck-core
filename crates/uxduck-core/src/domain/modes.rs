//! Flight modes by vehicle family: `custom_mode` numbers in the heartbeat to
//! names people know, and back for `MAV_CMD_DO_SET_MODE`. ArduPilot's names
//! are its own (as Mission Planner shows them); PX4's are the words
//! QGroundControl shows, such as `Position` and `Hold`, and PX4's own names
//! (`POSCTL`, `AUTO.LOITER`) are understood too.

/// Which ArduPilot firmware a `MAV_TYPE` runs, or PX4, for its mode numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Copter,
    Plane,
    Rover,
    /// `ArduSub` (S37).
    Sub,
    Blimp,
    /// PX4, whatever it flies: one set of modes, its `custom_mode` the main
    /// mode in bits 16 to 23 and the sub mode in bits 24 to 31.
    Px4,
}

/// How much a mode flies the vehicle itself (`Family::kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeKind {
    /// It flies, drives or lands itself: a mission, home, a hold in place.
    Automatic,
    /// It holds something, a height or a position, while you steer.
    Assisted,
    /// You fly it; at most it keeps itself level.
    Manual,
}

impl Family {
    /// Every family, in the order a list shows them.
    pub const ALL: [Self; 6] = [
        Self::Copter,
        Self::Plane,
        Self::Rover,
        Self::Sub,
        Self::Blimp,
        Self::Px4,
    ];

    /// A key for the wire: `copter`, `plane`, `rover`, `sub` or `px4`.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Copter => "copter",
            Self::Plane => "plane",
            Self::Rover => "rover",
            Self::Sub => "sub",
            Self::Blimp => "blimp",
            Self::Px4 => "px4",
        }
    }

    /// For a heading over its modes.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Copter => "Multicopters",
            Self::Plane => "Planes",
            Self::Rover => "Rovers and boats",
            Self::Sub => "Submarines",
            Self::Blimp => "Blimps",
            Self::Px4 => "PX4",
        }
    }

    /// From the heartbeat's `MAV_AUTOPILOT` and `MAV_TYPE`, as decoded
    /// (`Px4`, `Quadrotor`): PX4's modes for PX4, otherwise ArduPilot's.
    #[must_use]
    pub fn for_vehicle(autopilot: &str, mav_type: &str) -> Option<Self> {
        if autopilot.eq_ignore_ascii_case("px4") {
            Some(Self::Px4)
        } else {
            Self::of(mav_type)
        }
    }

    /// PX4's main and sub mode for `DO_SET_MODE`'s `param2` and `param3`.
    #[must_use]
    pub fn px4_parts(custom_mode: u32) -> (u32, u32) {
        ((custom_mode >> 16) & 0xff, custom_mode >> 24)
    }

    /// From the heartbeat's `MAV_TYPE`, as decoded (`Quadrotor`, `FixedWing`, ...).
    #[must_use]
    pub fn of(mav_type: &str) -> Option<Self> {
        match mav_type {
            "Quadrotor" | "Hexarotor" | "Octorotor" | "Tricopter" | "Helicopter" | "Coaxial"
            | "Dodecarotor" | "Decarotor" => Some(Self::Copter),
            t if t == "FixedWing" || t.starts_with("Vtol") => Some(Self::Plane),
            "GroundRover" | "SurfaceBoat" => Some(Self::Rover),
            "Submarine" => Some(Self::Sub),
            "Airship" => Some(Self::Blimp),
            _ => None,
        }
    }

    fn table(self) -> &'static [(u32, &'static str)] {
        match self {
            Self::Copter => COPTER,
            Self::Plane => PLANE,
            Self::Rover => ROVER,
            Self::Sub => SUB,
            Self::Blimp => BLIMP,
            Self::Px4 => PX4,
        }
    }

    #[must_use]
    pub fn name(self, custom_mode: u32) -> Option<&'static str> {
        self.table()
            .iter()
            .find(|(n, _)| *n == custom_mode)
            .map(|(_, name)| *name)
    }

    #[must_use]
    pub fn number(self, name: &str) -> Option<u32> {
        let aliases: &[(u32, &str)] = if self == Self::Px4 { PX4_NAMES } else { &[] };
        self.table()
            .iter()
            .chain(aliases)
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .map(|(number, _)| *number)
    }

    /// The modes people choose most, first in the menu. `mav_type` is the
    /// heartbeat's, as decoded: a plane with a quadplane (`Vtol…`) has its
    /// hover modes among them, a plain plane its hand-flown ones.
    #[must_use]
    pub fn choices(self, mav_type: &str) -> &'static [&'static str] {
        match self {
            Self::Copter => &[
                "LOITER",
                "ALT_HOLD",
                "POSHOLD",
                "STABILIZE",
                "GUIDED",
                "AUTO",
                "BRAKE",
                "RTL",
                "SMART_RTL",
                "LAND",
            ],
            Self::Plane if is_vtol(mav_type) => &[
                "FBWA", "FBWB", "CRUISE", "QLOITER", "QHOVER", "LOITER", "GUIDED", "AUTO", "RTL",
                "QRTL", "QLAND",
            ],
            Self::Plane => &[
                "FBWA",
                "FBWB",
                "CRUISE",
                "STABILIZE",
                "MANUAL",
                "LOITER",
                "GUIDED",
                "AUTO",
                "RTL",
            ],
            Self::Rover => &[
                "MANUAL",
                "STEERING",
                "HOLD",
                "LOITER",
                "GUIDED",
                "AUTO",
                "RTL",
                "SMART_RTL",
            ],
            Self::Sub => &[
                "MANUAL",
                "STABILIZE",
                "ALT_HOLD",
                "POSHOLD",
                "SURFACE",
                "AUTO",
            ],
            Self::Blimp => &[
                "MANUAL", "VELOCITY", "LOITER", "RTL", "AUTO", "HOLD", "LAND",
            ],
            Self::Px4 => &[
                "Position",
                "Altitude",
                "Stabilized",
                "Hold",
                "Mission",
                "Return",
                "Land",
            ],
        }
    }

    /// Modes a vehicle puts itself in, never chosen: left out of the lists.
    fn is_hidden(self, name: &str) -> bool {
        let hidden: &[&str] = match self {
            Self::Copter => &["AVOID_ADSB", "AUTO_RTL"],
            Self::Plane => &["INITIALISING", "AVOID_ADSB"],
            Self::Rover => &["INITIALISING"],
            Self::Sub | Self::Blimp => &[],
            Self::Px4 => &["Ready"],
        };
        hidden.iter().any(|h| h.eq_ignore_ascii_case(name))
    }

    /// Whether an airframe has a mode from its family's table: Plane's `Q`
    /// modes need a quadplane (a `Vtol…` type; `ArduPlane` reports fixed wing
    /// for one unless `Q_MAV_TYPE` says otherwise, so the engine passes a
    /// `Vtol…` type for it), Copter's AUTOROTATE a helicopter, PX4's VTOL
    /// Takeoff a VTOL. `None` stands for every airframe.
    #[must_use]
    pub fn has(self, mav_type: Option<&str>, name: &str) -> bool {
        let Some(mav_type) = mav_type else {
            return true;
        };
        match self {
            Self::Plane
                if name.starts_with('Q') || name.eq_ignore_ascii_case("LOITER_ALT_QLAND") =>
            {
                is_vtol(mav_type)
            }
            Self::Copter if name.eq_ignore_ascii_case("AUTOROTATE") => mav_type == "Helicopter",
            Self::Px4 if name.eq_ignore_ascii_case("VTOL Takeoff") => is_vtol(mav_type),
            _ => true,
        }
    }

    /// Every mode an airframe can be put in: the usual ones (`choices`)
    /// first, then the rest of the table in its order, each marked when it
    /// is one of the rest, for a menu's second group. `None` lists every
    /// airframe's, with a VTOL's usual ones.
    #[must_use]
    pub fn offered(self, mav_type: Option<&str>) -> Vec<(&'static str, bool)> {
        let usual = self.choices(mav_type.unwrap_or("VtolQuadrotor"));
        let mut all: Vec<(&'static str, bool)> = usual
            .iter()
            .filter(|mode| self.has(mav_type, mode))
            .map(|mode| (*mode, false))
            .collect();
        all.extend(
            self.table()
                .iter()
                .map(|(_, name)| *name)
                .filter(|name| {
                    !usual.iter().any(|u| u.eq_ignore_ascii_case(name))
                        && !self.is_hidden(name)
                        && self.has(mav_type, name)
                })
                .map(|name| (name, true)),
        );
        all
    }

    /// What a mode does, in a few plain words, for the mode menu and the
    /// agent (maintainer, 2026-10-03: see what a mode does before choosing it).
    #[must_use]
    pub fn describe(self, name: &str) -> Option<&'static str> {
        let table = match self {
            Self::Copter => COPTER_WORDS,
            Self::Plane => PLANE_WORDS,
            Self::Rover => ROVER_WORDS,
            Self::Sub => SUB_WORDS,
            Self::Blimp => BLIMP_WORDS,
            Self::Px4 => PX4_WORDS,
        };
        let name = if self == Self::Px4 {
            self.number(name).and_then(|n| self.name(n)).unwrap_or(name)
        } else {
            name
        };
        table
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, words)| *words)
    }

    /// How much a mode flies the vehicle itself, for the mode menu's groups
    /// (maintainer, 2026-10-13: a sentence under every mode was a wall of
    /// text). `None` for a mode the table does not know.
    #[must_use]
    pub fn kind(self, name: &str) -> Option<ModeKind> {
        let (automatic, manual): (&[&str], &[&str]) = match self {
            Self::Copter => (
                &[
                    "AUTO",
                    "GUIDED",
                    "GUIDED_NOGPS",
                    "RTL",
                    "SMART_RTL",
                    "AUTO_RTL",
                    "LAND",
                    "BRAKE",
                    "CIRCLE",
                    "FOLLOW",
                    "AVOID_ADSB",
                    "THROW",
                    "FLIP",
                    "SYSTEMID",
                    "AUTOROTATE",
                ],
                &["STABILIZE", "ACRO", "DRIFT", "TURTLE"],
            ),
            Self::Plane => (
                &[
                    "CIRCLE",
                    "AUTO",
                    "RTL",
                    "LOITER",
                    "TAKEOFF",
                    "AVOID_ADSB",
                    "GUIDED",
                    "INITIALISING",
                    "QLAND",
                    "QRTL",
                    "THERMAL",
                    "LOITER_ALT_QLAND",
                    "AUTOLAND",
                ],
                &[
                    "MANUAL",
                    "STABILIZE",
                    "TRAINING",
                    "ACRO",
                    "FBWA",
                    "QSTABILIZE",
                    "QACRO",
                ],
            ),
            Self::Rover => (
                &[
                    "HOLD",
                    "LOITER",
                    "FOLLOW",
                    "DOCK",
                    "CIRCLE",
                    "AUTO",
                    "RTL",
                    "SMART_RTL",
                    "GUIDED",
                    "INITIALISING",
                ],
                &["MANUAL", "ACRO", "STEERING", "SIMPLE"],
            ),
            Self::Sub => (
                &["AUTO", "GUIDED", "CIRCLE", "SURFACE", "MOTOR_DETECT"],
                &["STABILIZE", "ACRO", "MANUAL"],
            ),
            Self::Blimp => (&["LAND", "RTL", "AUTO", "HOLD"], &["MANUAL"]),
            Self::Px4 => (
                &[
                    "Orbit",
                    "Ready",
                    "Takeoff",
                    "Hold",
                    "Mission",
                    "Return",
                    "Land",
                    "Follow Me",
                    "Precision Land",
                    "VTOL Takeoff",
                    "Offboard",
                ],
                &["Manual", "Acro", "Stabilized"],
            ),
        };
        let number = self.number(name)?;
        let name = self.name(number)?;
        let is = |list: &[&str]| list.iter().any(|m| m.eq_ignore_ascii_case(name));
        Some(if is(automatic) {
            ModeKind::Automatic
        } else if is(manual) {
            ModeKind::Manual
        } else {
            ModeKind::Assisted
        })
    }

    /// Whether a mode flies without GPS: the ones to choose when it is lost
    /// (maintainer, 2026-10-13: FBWA had no tag). Only modes known to work
    /// with no GPS fix at all; a mode that may is left out.
    #[must_use]
    pub fn without_gps(self, name: &str) -> bool {
        let free: &[&str] = match self {
            Self::Copter => &[
                "STABILIZE",
                "ACRO",
                "ALT_HOLD",
                "SPORT",
                "LAND",
                "FLIP",
                "AUTOTUNE",
                "GUIDED_NOGPS",
                "FLOWHOLD",
                "SYSTEMID",
                "TURTLE",
            ],
            Self::Plane => &[
                "MANUAL",
                "CIRCLE",
                "STABILIZE",
                "TRAINING",
                "ACRO",
                "FBWA",
                "FBWB",
                "AUTOTUNE",
                "QSTABILIZE",
                "QHOVER",
                "QLAND",
                "QACRO",
            ],
            Self::Rover => &["MANUAL", "ACRO", "STEERING", "HOLD"],
            Self::Sub => &[
                "MANUAL",
                "STABILIZE",
                "ACRO",
                "ALT_HOLD",
                "SURFACE",
                "MOTOR_DETECT",
                "SURFTRAK",
            ],
            Self::Blimp => &["MANUAL"],
            Self::Px4 => &["Manual", "Stabilized", "Acro", "Altitude"],
        };
        let Some(name) = self.number(name).and_then(|n| self.name(n)) else {
            return false;
        };
        free.iter().any(|f| f.eq_ignore_ascii_case(name))
    }

    /// Modes that make a vehicle safer on their own, so they may be sent
    /// without a recent heartbeat (ADR-0004 `offlineBehaviour: allow`).
    #[must_use]
    pub fn is_safe(self, name: &str) -> bool {
        let safe: &[&str] = match self {
            Self::Copter => &["RTL", "LAND", "BRAKE", "LOITER", "SMART_RTL"],
            Self::Plane => &["RTL", "LOITER", "QLAND", "QRTL"],
            Self::Rover => &["HOLD", "RTL", "SMART_RTL"],
            Self::Sub => &["SURFACE", "ALT_HOLD", "POSHOLD"],
            Self::Blimp => &["RTL", "LAND", "HOLD", "LOITER"],
            Self::Px4 => &["Return", "Land", "Hold", "Precision Land"],
        };
        safe.iter().any(|s| s.eq_ignore_ascii_case(name))
    }
}

/// A mode as a vehicle lists it itself (`AVAILABLE_MODES`; ArduPilot 4.6
/// and PX4 1.15 onwards): its number, its own name and MAVLink's
/// properties. The vehicle's list beats the table, as it has the modes its
/// firmware and its parameters give it, Lua-scripted ones included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableMode {
    pub number: u32,
    /// The firmware's own name, which can differ from the table's
    /// (`FLY_BY_WIRE_A` for FBWA); the table's is shown where it has one.
    pub name: String,
    /// `MAV_MODE_PROPERTY_ADVANCED`: for the second group of a menu.
    pub advanced: bool,
    /// Not `MAV_MODE_PROPERTY_NOT_USER_SELECTABLE`.
    pub selectable: bool,
}

/// `MAV_MODE_PROPERTY_ADVANCED`.
const ADVANCED: u32 = 1;
/// `MAV_MODE_PROPERTY_NOT_USER_SELECTABLE`.
const NOT_USER_SELECTABLE: u32 = 2;

impl AvailableMode {
    /// From the message's `properties` bits.
    #[must_use]
    pub fn new(number: u32, name: &str, properties: u32) -> Self {
        Self {
            number,
            name: name.to_owned(),
            advanced: properties & ADVANCED != 0,
            selectable: properties & NOT_USER_SELECTABLE == 0,
        }
    }
}

impl Family {
    /// The modes a vehicle lists itself, matched to the table by number:
    /// in the table's order and under its names where it knows them, then
    /// the rest (a scripted mode) under the vehicle's names, as advanced.
    /// Modes the vehicle says nobody chooses are left out, as are the
    /// table's hidden ones and those its airframe is not thought to have.
    #[must_use]
    pub fn offered_from(
        self,
        mav_type: Option<&str>,
        listed: &[AvailableMode],
    ) -> Vec<(String, bool)> {
        let choosable = |number: u32| listed.iter().find(|m| m.selectable && m.number == number);
        let mut all: Vec<(String, bool)> = self
            .offered(mav_type)
            .into_iter()
            .filter_map(|(name, advanced)| {
                let mode = choosable(self.number(name)?)?;
                Some((name.to_owned(), advanced || mode.advanced))
            })
            .collect();
        all.extend(
            listed
                .iter()
                .filter(|mode| mode.selectable && self.name(mode.number).is_none())
                .map(|mode| (mode.name.clone(), true)),
        );
        all
    }

    /// A mode's name from a vehicle's own list, for a number the table does
    /// not know (a scripted mode).
    #[must_use]
    pub fn listed_name(listed: &[AvailableMode], number: u32) -> Option<&str> {
        listed
            .iter()
            .find(|m| m.number == number)
            .map(|m| m.name.as_str())
    }

    /// A mode's number from a vehicle's own list, by the name it gives it.
    #[must_use]
    pub fn listed_number(listed: &[AvailableMode], name: &str) -> Option<u32> {
        listed
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| m.number)
    }
}

/// A plane with a quadplane, a tiltrotor or a tailsitter, by its `MAV_TYPE`.
#[must_use]
pub fn is_vtol(mav_type: &str) -> bool {
    mav_type.starts_with("Vtol")
}

const COPTER: &[(u32, &str)] = &[
    (0, "STABILIZE"),
    (1, "ACRO"),
    (2, "ALT_HOLD"),
    (3, "AUTO"),
    (4, "GUIDED"),
    (5, "LOITER"),
    (6, "RTL"),
    (7, "CIRCLE"),
    (9, "LAND"),
    (11, "DRIFT"),
    (13, "SPORT"),
    (14, "FLIP"),
    (15, "AUTOTUNE"),
    (16, "POSHOLD"),
    (17, "BRAKE"),
    (18, "THROW"),
    (19, "AVOID_ADSB"),
    (20, "GUIDED_NOGPS"),
    (21, "SMART_RTL"),
    (22, "FLOWHOLD"),
    (23, "FOLLOW"),
    (24, "ZIGZAG"),
    (25, "SYSTEMID"),
    (26, "AUTOROTATE"),
    (27, "AUTO_RTL"),
    (28, "TURTLE"),
];

const PLANE: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "CIRCLE"),
    (2, "STABILIZE"),
    (3, "TRAINING"),
    (4, "ACRO"),
    (5, "FBWA"),
    (6, "FBWB"),
    (7, "CRUISE"),
    (8, "AUTOTUNE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "LOITER"),
    (13, "TAKEOFF"),
    (14, "AVOID_ADSB"),
    (15, "GUIDED"),
    (16, "INITIALISING"),
    (17, "QSTABILIZE"),
    (18, "QHOVER"),
    (19, "QLOITER"),
    (20, "QLAND"),
    (21, "QRTL"),
    (22, "QAUTOTUNE"),
    (23, "QACRO"),
    (24, "THERMAL"),
    (25, "LOITER_ALT_QLAND"),
    (26, "AUTOLAND"),
];

const ROVER: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "ACRO"),
    (3, "STEERING"),
    (4, "HOLD"),
    (5, "LOITER"),
    (6, "FOLLOW"),
    (7, "SIMPLE"),
    (8, "DOCK"),
    (9, "CIRCLE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "SMART_RTL"),
    (15, "GUIDED"),
    (16, "INITIALISING"),
];

/// ArduPilot Blimp modes, from the pinned Blimp/mode.h.
const BLIMP_WORDS: &[(&str, &str)] = &[
    ("LAND", "Stops actuators; the blimp may drift"),
    ("MANUAL", "You control the fins or motors directly"),
    ("VELOCITY", "You choose its speed and direction"),
    ("LOITER", "Holds position and height"),
    ("RTL", "Returns to its starting point"),
    ("AUTO", "Follows the mission"),
    ("HOLD", "Stops moving"),
];

const BLIMP: &[(u32, &str)] = &[
    (0, "LAND"),
    (1, "MANUAL"),
    (2, "VELOCITY"),
    (3, "LOITER"),
    (4, "RTL"),
    (5, "AUTO"),
    (6, "HOLD"),
];

/// `ArduSub`'s `custom_mode` numbers (S37).
const SUB: &[(u32, &str)] = &[
    (0, "STABILIZE"),
    (1, "ACRO"),
    (2, "ALT_HOLD"),
    (3, "AUTO"),
    (4, "GUIDED"),
    (7, "CIRCLE"),
    (9, "SURFACE"),
    (16, "POSHOLD"),
    (19, "MANUAL"),
    (20, "MOTOR_DETECT"),
    (21, "SURFTRAK"),
];

/// PX4's `custom_mode` numbers, main mode << 16 | sub mode << 24
/// (`px4_custom_mode.h`), with the words QGroundControl shows.
const PX4: &[(u32, &str)] = &[
    (1 << 16, "Manual"),
    (2 << 16, "Altitude"),
    (3 << 16, "Position"),
    (3 << 16 | 1 << 24, "Orbit"),
    (3 << 16 | 2 << 24, "Position Slow"),
    (4 << 16 | 1 << 24, "Ready"),
    (4 << 16 | 2 << 24, "Takeoff"),
    (4 << 16 | 3 << 24, "Hold"),
    (4 << 16 | 4 << 24, "Mission"),
    (4 << 16 | 5 << 24, "Return"),
    (4 << 16 | 6 << 24, "Land"),
    (4 << 16 | 8 << 24, "Follow Me"),
    (4 << 16 | 9 << 24, "Precision Land"),
    (4 << 16 | 10 << 24, "VTOL Takeoff"),
    (5 << 16, "Acro"),
    (6 << 16, "Offboard"),
    (7 << 16, "Stabilized"),
];

/// PX4's own names for the same modes, as MAVROS and PX4's shell use them,
/// accepted when a mode is asked for by name.
const PX4_NAMES: &[(u32, &str)] = &[
    (1 << 16, "MANUAL"),
    (2 << 16, "ALTCTL"),
    (3 << 16, "POSCTL"),
    (4 << 16 | 2 << 24, "AUTO.TAKEOFF"),
    (4 << 16 | 3 << 24, "AUTO.LOITER"),
    (4 << 16 | 4 << 24, "AUTO.MISSION"),
    (4 << 16 | 5 << 24, "AUTO.RTL"),
    (4 << 16 | 6 << 24, "AUTO.LAND"),
    (4 << 16 | 8 << 24, "AUTO.FOLLOW_TARGET"),
    (4 << 16 | 9 << 24, "AUTO.PRECLAND"),
    (4 << 16 | 10 << 24, "AUTO.VTOL_TAKEOFF"),
];

/// What each mode does, in a few plain words (`Family::describe`). Modes
/// that are easy to mix up say what tells them apart: what the sticks do,
/// what it holds by itself, and whether it needs GPS.
const COPTER_WORDS: &[(&str, &str)] = &[
    (
        "STABILIZE",
        "The sticks control roll and pitch. Releasing the sticks levels the vehicle. You control throttle.",
    ),
    (
        "ACRO",
        "The sticks control rotation rates. Releasing the sticks does not level the vehicle.",
    ),
    (
        "ALT_HOLD",
        "The vehicle holds altitude but can drift with the wind. GPS is not required.",
    ),
    ("AUTO", "Flies the mission"),
    ("GUIDED", "Goes where UXDuck or the agent sends it"),
    (
        "LOITER",
        "The vehicle holds position and altitude using GPS. The sticks control horizontal movement.",
    ),
    ("RTL", "Climbs, flies straight home and lands"),
    ("CIRCLE", "Circles a point, facing it"),
    ("LAND", "Lands straight down where it is"),
    ("DRIFT", "Steers like a plane, banking into turns"),
    (
        "SPORT",
        "The vehicle holds altitude. The sticks control turns, climb and descent.",
    ),
    ("FLIP", "Flips over once, then carries on"),
    ("AUTOTUNE", "Tunes its own controls while it flies"),
    (
        "POSHOLD",
        "The sticks control the vehicle’s tilt directly. Releasing the sticks brakes the vehicle and holds its position.",
    ),
    ("BRAKE", "Stops as fast as it can and hovers there"),
    ("THROW", "Starts its motors when thrown"),
    ("AVOID_ADSB", "Steers clear of aircraft it hears"),
    (
        "GUIDED_NOGPS",
        "The vehicle follows attitude and climb commands from a computer. GPS is not required.",
    ),
    ("SMART_RTL", "Flies home the way it came, then lands"),
    (
        "FLOWHOLD",
        "The vehicle holds position using an optical flow sensor. GPS is not required.",
    ),
    ("FOLLOW", "Follows another vehicle"),
    ("ZIGZAG", "Flies back and forth between two points"),
    ("SYSTEMID", "Shakes itself to measure how it responds"),
    ("AUTOROTATE", "A helicopter gliding down without power"),
    ("AUTO_RTL", "Skips to the end of its mission and lands"),
    ("TURTLE", "Flips itself upright after a crash"),
];

const PLANE_WORDS: &[(&str, &str)] = &[
    (
        "MANUAL",
        "The sticks control the vehicle’s control surfaces directly, without stabilization.",
    ),
    (
        "CIRCLE",
        "The vehicle maintains a gentle bank to fly in a circle. GPS is not required.",
    ),
    (
        "STABILIZE",
        "Releasing the sticks levels the vehicle. Roll angle is not limited.",
    ),
    (
        "TRAINING",
        "You fly directly until it banks or pitches too far, then it stops you",
    ),
    (
        "ACRO",
        "Holds whatever attitude you leave it in, even upside down",
    ),
    (
        "FBWA",
        "The roll and pitch sticks request angles within configured limits. Centering them returns to wings level and trimmed pitch, which does not hold altitude. You control throttle.",
    ),
    (
        "FBWB",
        "The roll stick sets bank angle. The pitch stick requests a climb or descent rate; centered, it holds altitude. The autopilot controls pitch and motor power. With an airspeed sensor, throttle normally selects airspeed: minimum stick requests minimum airspeed, not motor off. Without one, mid-stick and below use cruise throttle, and above mid-stick raises it. Flight options can change this behavior.",
    ),
    (
        "CRUISE",
        "Uses FBWB’s height and speed controls, and holds a GPS ground track when you release the roll and rudder sticks.",
    ),
    ("AUTOTUNE", "You fly as in FBWA while it tunes its controls"),
    ("AUTO", "Flies the mission"),
    ("RTL", "Flies home and circles there"),
    ("LOITER", "Circles where it is, holding its height"),
    ("TAKEOFF", "Takes off and climbs to a set height"),
    ("AVOID_ADSB", "Steers clear of aircraft it hears"),
    (
        "GUIDED",
        "Flies to where UXDuck or the agent sends it, and circles",
    ),
    ("INITIALISING", "Starting up"),
    (
        "QSTABILIZE",
        "The vehicle stabilizes roll and pitch while hovering. You control steering and throttle.",
    ),
    (
        "QHOVER",
        "The vehicle holds altitude while hovering but can drift with the wind. GPS is not required.",
    ),
    (
        "QLOITER",
        "Hovers in place, holding its position and height by GPS",
    ),
    ("QLAND", "Lands straight down where it is"),
    ("QRTL", "Flies home and lands straight down"),
    ("QAUTOTUNE", "Tunes its hover controls"),
    (
        "QACRO",
        "The sticks control rotation rates while hovering. Releasing the sticks does not level the vehicle.",
    ),
    ("THERMAL", "Circles in rising air to gain height"),
    ("LOITER_ALT_QLAND", "Circles down, then lands straight down"),
    ("AUTOLAND", "Lands on the runway it took off from"),
];

const ROVER_WORDS: &[(&str, &str)] = &[
    (
        "MANUAL",
        "You control steering and throttle directly, without assistance.",
    ),
    (
        "ACRO",
        "The vehicle maintains the turn rate commanded by the steering stick.",
    ),
    (
        "STEERING",
        "The vehicle maintains the speed and turn rate commanded by the sticks.",
    ),
    ("HOLD", "Stops and stays stopped"),
    (
        "LOITER",
        "Stays near where it is, moving back when it drifts",
    ),
    ("FOLLOW", "Follows another vehicle"),
    ("SIMPLE", "The sticks steer by compass, not by its nose"),
    ("DOCK", "Drives into its dock"),
    ("CIRCLE", "Drives in a circle"),
    ("AUTO", "Drives the mission"),
    ("RTL", "Drives straight home"),
    ("SMART_RTL", "Drives home the way it came"),
    ("GUIDED", "Goes where UXDuck or the agent sends it"),
    ("INITIALISING", "Starting up"),
];

const SUB_WORDS: &[(&str, &str)] = &[
    (
        "STABILIZE",
        "You control the vehicle’s movement. The vehicle stabilizes roll and pitch.",
    ),
    (
        "ACRO",
        "The sticks control rotation rates. The vehicle does not level automatically.",
    ),
    (
        "ALT_HOLD",
        "The vehicle holds depth but can drift with the current. You control horizontal movement.",
    ),
    ("AUTO", "Runs the mission"),
    ("GUIDED", "Goes where UXDuck or the agent sends it"),
    ("CIRCLE", "Circles a point"),
    ("SURFACE", "Comes up to the surface"),
    ("POSHOLD", "Holds its position and depth"),
    (
        "MANUAL",
        "You control the thrusters directly, without stabilization.",
    ),
    ("MOTOR_DETECT", "Works out which way its motors push"),
    ("SURFTRAK", "Holds its height above the seabed"),
];

const PX4_WORDS: &[(&str, &str)] = &[
    ("Manual", "You control the vehicle directly."),
    (
        "Altitude",
        "The vehicle holds altitude but can drift with the wind. GPS is not required.",
    ),
    (
        "Position",
        "The vehicle holds position and altitude using GPS. The sticks control movement.",
    ),
    ("Orbit", "Circles a point"),
    ("Position Slow", "Like Position, with slower sticks"),
    ("Ready", "Armed and waiting"),
    ("Takeoff", "Takes off and climbs to a set height"),
    ("Hold", "Stops and stays where it is"),
    ("Mission", "Flies the mission"),
    ("Return", "Flies home and lands"),
    ("Land", "Lands straight down where it is"),
    ("Follow Me", "Follows the person with the controller"),
    ("Precision Land", "Lands on a marker it can see"),
    ("VTOL Takeoff", "Takes off straight up, then flies forward"),
    (
        "Acro",
        "The sticks control rotation rates. Releasing the sticks does not level the vehicle.",
    ),
    ("Offboard", "Flies as a companion computer tells it"),
    (
        "Stabilized",
        "The sticks control roll and pitch. Releasing the sticks levels the vehicle. You control throttle.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_per_family() {
        let copter = Family::of("Quadrotor").unwrap();
        assert_eq!(copter.name(6), Some("RTL"));
        assert_eq!(copter.number("guided"), Some(4));
        assert_eq!(Family::of("FixedWing").unwrap().number("RTL"), Some(11));
        assert_eq!(Family::of("VtolTiltrotor"), Some(Family::Plane));
        assert!(Family::of("Gcs").is_none());
        let sub = Family::of("Submarine").unwrap();
        assert_eq!(sub.name(2), Some("ALT_HOLD"));
        assert_eq!(sub.number("surface"), Some(9));
        assert!(copter.is_safe("land") && !copter.is_safe("GUIDED"));
    }

    #[test]
    fn modes_are_grouped_by_how_much_they_fly_themselves() {
        assert_eq!(Family::Copter.kind("loiter"), Some(ModeKind::Assisted));
        assert_eq!(Family::Copter.kind("RTL"), Some(ModeKind::Automatic));
        assert_eq!(Family::Copter.kind("STABILIZE"), Some(ModeKind::Manual));
        assert_eq!(Family::Plane.kind("FBWB"), Some(ModeKind::Assisted));
        assert_eq!(Family::Plane.kind("LOITER"), Some(ModeKind::Automatic));
        // PX4's own names are the same modes.
        assert_eq!(Family::Px4.kind("AUTO.RTL"), Some(ModeKind::Automatic));
        assert_eq!(Family::Px4.kind("POSCTL"), Some(ModeKind::Assisted));
        assert_eq!(Family::Copter.kind("NOT_A_MODE"), None);
        // Every usual mode of every family has a group of its own words:
        // none of the lists names a mode the table does not have.
        for family in Family::ALL {
            for (_, name) in family.table() {
                assert!(family.kind(name).is_some(), "{family:?} {name}");
            }
        }
    }

    #[test]
    fn modes_that_need_no_gps_say_so() {
        assert!(Family::Copter.without_gps("ALT_HOLD"));
        assert!(Family::Copter.without_gps("STABILIZE"));
        assert!(Family::Plane.without_gps("FBWA"));
        assert!(Family::Plane.without_gps("FBWB"));
        assert!(Family::Plane.without_gps("QHOVER"));
        assert!(Family::Px4.without_gps("ALTCTL"));
        assert!(!Family::Copter.without_gps("LOITER"));
        assert!(!Family::Plane.without_gps("CRUISE"));
        assert!(!Family::Plane.without_gps("QLOITER"));
        // Every mode whose words say it needs no GPS is in the list.
        for family in Family::ALL {
            for (_, name) in family.table() {
                if family
                    .describe(name)
                    .is_some_and(|w| w.ends_with("no GPS needed"))
                {
                    assert!(family.without_gps(name), "{family:?} {name}");
                }
            }
        }
    }

    #[test]
    fn every_mode_says_what_it_does() {
        for (family, table) in [
            (Family::Copter, COPTER),
            (Family::Plane, PLANE),
            (Family::Rover, ROVER),
            (Family::Sub, SUB),
            (Family::Px4, PX4),
        ] {
            for (_, name) in table {
                assert!(family.describe(name).is_some(), "{family:?} {name}");
            }
        }
        assert_eq!(
            Family::Plane.describe("qloiter"),
            Some("Hovers in place, holding its position and height by GPS")
        );
        // PX4's own names find the same words.
        assert_eq!(
            Family::Px4.describe("AUTO.RTL"),
            Family::Px4.describe("Return")
        );
    }

    #[test]
    fn every_mode_is_offered_to_the_airframe_that_has_it() {
        fn names(offered: Vec<(&'static str, bool)>) -> Vec<&'static str> {
            offered.into_iter().map(|(m, _)| m).collect()
        }
        // A quadplane has its hover modes; a plain plane has none of them.
        let vtol = Family::Plane.offered(Some("VtolQuadrotor"));
        assert!(vtol.iter().any(|(m, more)| *m == "QHOVER" && !more));
        assert!(vtol.iter().any(|(m, more)| *m == "QSTABILIZE" && *more));
        let plane = names(Family::Plane.offered(Some("FixedWing")));
        assert!(!plane.iter().any(|m| m.starts_with('Q')), "{plane:?}");
        assert!(plane.contains(&"FBWB") && plane.contains(&"THERMAL"));
        assert!(!plane.contains(&"INITIALISING"));
        // Every family offers its whole table but the hidden modes.
        for (family, table, hidden) in [
            (Family::Copter, COPTER, 2),
            (Family::Plane, PLANE, 2),
            (Family::Rover, ROVER, 1),
            (Family::Sub, SUB, 0),
            (Family::Blimp, BLIMP, 0),
            (Family::Px4, PX4, 1),
        ] {
            let all = names(family.offered(None));
            assert_eq!(all.len(), table.len() - hidden, "{family:?} {all:?}");
            let mut sorted = all.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), all.len(), "{family:?} repeats a mode");
        }
        // The usual ones come first, for the top of the menu.
        let copter = Family::Copter.offered(Some("Quadrotor"));
        assert_eq!(copter[0], ("LOITER", false));
        assert!(copter.iter().any(|(m, more)| *m == "FLIP" && *more));
        assert!(!names(copter.clone()).contains(&"AUTOROTATE"));
        assert!(names(Family::Copter.offered(Some("Helicopter"))).contains(&"AUTOROTATE"));
    }

    #[test]
    fn a_vehicles_own_list_beats_the_table() {
        // A plane with its quadplane on, as ArduPlane lists it: its own
        // names, a scripted mode, and one nobody chooses.
        let listed = [
            AvailableMode::new(5, "FLY_BY_WIRE_A", 0),
            AvailableMode::new(6, "FLY_BY_WIRE_B", 0),
            AvailableMode::new(19, "QLOITER", 0),
            AvailableMode::new(8, "AUTOTUNE", ADVANCED),
            AvailableMode::new(16, "INITIALISING", NOT_USER_SELECTABLE),
            AvailableMode::new(100, "Survey", 0),
        ];
        let offered = Family::Plane.offered_from(Some("VtolQuadrotor"), &listed);
        assert_eq!(
            offered,
            vec![
                ("FBWA".to_owned(), false),
                ("FBWB".to_owned(), false),
                ("QLOITER".to_owned(), false),
                ("AUTOTUNE".to_owned(), true),
                ("Survey".to_owned(), true),
            ]
        );
        // No CRUISE: the vehicle did not list it.
        assert!(!offered.iter().any(|(m, _)| m == "CRUISE"));
        assert_eq!(Family::listed_name(&listed, 100), Some("Survey"));
        assert_eq!(Family::listed_number(&listed, "survey"), Some(100));
        assert_eq!(Family::listed_number(&listed, "CRUISE"), None);
    }

    #[test]
    fn px4_has_its_own_modes_whatever_it_flies() {
        let px4 = Family::for_vehicle("Px4", "Quadrotor").unwrap();
        assert_eq!(px4, Family::Px4);
        assert_eq!(
            Family::for_vehicle("Ardupilotmega", "Quadrotor"),
            Some(Family::Copter)
        );
        // A hovering quad's heartbeat in AUTO.LOITER, as PX4 sends it.
        assert_eq!(px4.name(0x0304_0000), Some("Hold"));
        assert_eq!(px4.name(0x0003_0000), Some("Position"));
        let land = px4.number("land").unwrap();
        assert_eq!(Family::px4_parts(land), (4, 6));
        // PX4's own names are understood too.
        assert_eq!(px4.number("AUTO.LAND"), Some(land));
        assert_eq!(px4.number("posctl"), px4.number("Position"));
        assert!(px4.is_safe("Return") && !px4.is_safe("Mission"));
    }
    #[test]
    fn airships_use_blimp_modes() {
        let family = Family::for_vehicle("Ardupilotmega", "Airship").unwrap();
        assert_eq!(family, Family::Blimp);
        assert_eq!(family.name(0), Some("LAND"));
        assert_eq!(family.name(1), Some("MANUAL"));
        assert_eq!(family.number("loiter"), Some(3));
        assert_eq!(family.number("hold"), Some(6));
        assert!(family.number("GUIDED").is_none());
        assert!(family.describe("VELOCITY").is_some());
    }
}
