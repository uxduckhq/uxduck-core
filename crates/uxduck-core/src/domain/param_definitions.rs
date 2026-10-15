//! Parameter definitions (ADR-0021): what a parameter means, beyond its name
//! and value, for one firmware at one version. ArduPilot writes them in its
//! source; `uxduck-contracts` converts its `apm.pdef.json` and `.xml` into
//! this schema, which is ours so upstream can change without our wire
//! changing (P1).

use serde::{Deserialize, Serialize};

/// The ArduPilot firmware a vehicle runs, which decides its parameters (P3).
/// A helicopter runs `ArduCopter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Firmware {
    ArduCopter,
    ArduPlane,
    Rover,
    ArduSub,
    AntennaTracker,
    Blimp,
}

impl Firmware {
    pub const ALL: [Self; 6] = [
        Self::ArduCopter,
        Self::ArduPlane,
        Self::Rover,
        Self::ArduSub,
        Self::AntennaTracker,
        Self::Blimp,
    ];

    /// From the heartbeat's `MAV_TYPE`, as decoded (`Quadrotor`, `FixedWing`,
    /// ...): every VTOL type is a plane, a boat runs Rover.
    #[must_use]
    pub fn of(mav_type: &str) -> Option<Self> {
        match mav_type {
            "Quadrotor" | "Hexarotor" | "Octorotor" | "Tricopter" | "Helicopter" | "Coaxial"
            | "Dodecarotor" | "Decarotor" => Some(Self::ArduCopter),
            t if t == "FixedWing" || t.starts_with("Vtol") => Some(Self::ArduPlane),
            "GroundRover" | "SurfaceBoat" => Some(Self::Rover),
            "Submarine" => Some(Self::ArduSub),
            "AntennaTracker" => Some(Self::AntennaTracker),
            "Airship" => Some(Self::Blimp),
            _ => None,
        }
    }

    /// As ArduPilot's `param_parse.py --vehicle` names it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ArduCopter => "ArduCopter",
            Self::ArduPlane => "ArduPlane",
            Self::Rover => "Rover",
            Self::ArduSub => "ArduSub",
            Self::AntennaTracker => "AntennaTracker",
            Self::Blimp => "Blimp",
        }
    }

    /// Its short name, which most of its release tags begin with:
    /// `Copter-4.5.7` (`ArduSub`'s begin `ArduSub-`).
    #[must_use]
    pub const fn tag_prefix(self) -> &'static str {
        match self {
            Self::ArduCopter => "Copter",
            Self::ArduPlane => "Plane",
            Self::Rover => "Rover",
            Self::ArduSub => "Sub",
            Self::AntennaTracker => "Tracker",
            Self::Blimp => "Blimp",
        }
    }

    /// From either name: `ArduCopter` or `Copter`, in any case.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| {
            f.name().eq_ignore_ascii_case(text) || f.tag_prefix().eq_ignore_ascii_case(text)
        })
    }
}

/// Where a set came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum DefinitionSource {
    /// ArduPilot's own, from UXDuck's catalogue (P2).
    Ardupilot,
    /// Added by a person, usually for their fork (P5).
    #[serde(rename_all = "camelCase")]
    Own { label: String },
}

/// Standard or Advanced: ArduPilot's `@User`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UserLevel {
    Standard,
    #[default]
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamRange {
    pub low: f64,
    pub high: f64,
}

/// One of a parameter's values and its meaning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValueName {
    pub value: f64,
    pub name: String,
}

/// One bit of a bitmask parameter and its meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitName {
    pub bit: u8,
    pub name: String,
}

/// What one parameter means.
#[expect(clippy::struct_excessive_bools, reason = "ArduPilot's own flags")]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamDefinition {
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// The prefix it is documented under, such as `BATT_` or `RTL_`.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ParamRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub increment: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ValueName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bitmask: Vec<BitName>,
    #[serde(default)]
    pub user: UserLevel,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reboot_required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub volatile: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub calibration: bool,
    /// The library that documents this variant, such as
    /// `AC_AttitudeControl_Heli`, when ArduPilot documents the name more than
    /// once for different builds of the same firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Every parameter of one firmware build (P1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionSet {
    pub source: DefinitionSource,
    pub firmware: Firmware,
    /// `4.5.7`, or `latest` for the development build's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The git commit it was generated from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Sorted by name; a name appears more than once only for its variants.
    pub parameters: Vec<ParamDefinition>,
}

impl DefinitionSet {
    /// The definition of `name` for this vehicle: a helicopter takes a
    /// helicopter's variant, anything else the first that is not one.
    #[must_use]
    pub fn find(&self, name: &str, helicopter: bool) -> Option<&ParamDefinition> {
        let start = self.parameters.partition_point(|p| p.name.as_str() < name);
        let variants = self.parameters[start..]
            .iter()
            .take_while(|p| p.name == name);
        let is_heli =
            |p: &ParamDefinition| p.variant.as_deref().is_some_and(|v| v.contains("Heli"));
        let mut first = None;
        let mut fallback = None;
        for p in variants {
            fallback.get_or_insert(p);
            if is_heli(p) == helicopter {
                first = Some(p);
                break;
            }
        }
        first.or(fallback)
    }

    /// Sorts by name, keeping variants in the order they were documented.
    pub fn sort(&mut self) {
        self.parameters.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Its name for people: "Copter 4.5.7", "Copter latest", or a person's
    /// own label.
    #[must_use]
    pub fn title(&self) -> String {
        match &self.source {
            DefinitionSource::Own { label } => label.clone(),
            DefinitionSource::Ardupilot => match &self.version {
                Some(version) => format!("{} {version}", self.firmware.tag_prefix()),
                None => self.firmware.tag_prefix().to_owned(),
            },
        }
    }
}

/// A firmware version as ArduPilot reports it: major, minor, patch and
/// whether it is a release (P3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl Version {
    /// From `4.5.7`, `4.5.8-dev` or `ArduCopter V4.5.7`: the first three
    /// dotted numbers.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let start = text.find(|c: char| c.is_ascii_digit())?;
        let mut parts = text[start..]
            .split(|c: char| !c.is_ascii_digit())
            .map(str::parse::<u8>);
        let major = parts.next()?.ok()?;
        let minor = parts.next()?.ok()?;
        let patch = parts.next().and_then(Result::ok).unwrap_or(0);
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How well a vehicle's set matches what it runs (P3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Match {
    /// A set the person bound to this vehicle, or to its firmware.
    Own,
    /// Generated from the commit the vehicle was built from.
    Commit,
    /// The release the vehicle runs.
    Exact,
    /// Another release, or the development build's: close, not certain.
    Approximate,
}

/// A set on offer to choose from: a catalogue entry or a person's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub firmware: Firmware,
    /// `4.5.7`, or `latest`.
    pub version: Option<String>,
    pub commit: Option<String>,
    /// For a person's own: bound to one vehicle, or to every vehicle of
    /// its firmware; neither means only by commit.
    pub vehicle_id: Option<String>,
    pub own: bool,
}

/// The vehicle a set is chosen for.
#[derive(Clone, Copy, Debug)]
pub struct ChoosingFor<'a> {
    pub vehicle_id: &'a str,
    pub firmware: Firmware,
    pub version: Option<Version>,
    /// The first 8 bytes of the git hash it was built from, as hex.
    pub commit: Option<&'a str>,
}

/// P3's order: a set bound to this vehicle; one bound to its firmware; one
/// whose commit matches; the catalogue's for the exact version; the nearest
/// older release of the same major and minor; then `latest`. Never another
/// firmware's.
#[must_use]
pub fn choose<'c>(
    candidates: &'c [Candidate],
    vehicle: &ChoosingFor<'_>,
) -> Option<(&'c Candidate, Match)> {
    let same = candidates
        .iter()
        .filter(|c| c.firmware == vehicle.firmware)
        .collect::<Vec<_>>();
    let commit_matches = |c: &Candidate| {
        matches!((c.commit.as_deref(), vehicle.commit), (Some(a), Some(b))
            if !b.is_empty() && (a.starts_with(b) || b.starts_with(a)))
    };
    if let Some(c) = same
        .iter()
        .find(|c| c.own && c.vehicle_id.as_deref() == Some(vehicle.vehicle_id))
    {
        return Some((c, Match::Own));
    }
    if let Some(c) = same.iter().find(|c| c.own && commit_matches(c)) {
        return Some((c, Match::Own));
    }
    if let Some(c) = same
        .iter()
        .find(|c| c.own && c.vehicle_id.is_none() && c.commit.is_none())
    {
        return Some((c, Match::Own));
    }
    let catalogue = same.iter().filter(|c| !c.own);
    if let Some(c) = catalogue.clone().find(|c| commit_matches(c)) {
        return Some((c, Match::Commit));
    }
    let released = |c: &&&Candidate| c.version.as_deref().and_then(Version::parse);
    if let Some(version) = vehicle.version {
        if let Some(c) = catalogue.clone().find(|c| released(c) == Some(version)) {
            return Some((c, Match::Exact));
        }
        let nearest = catalogue
            .clone()
            .filter_map(|c| released(&c).map(|v| (v, c)))
            .filter(|(v, _)| v.major == version.major && v.minor == version.minor && *v < version)
            .max_by_key(|(v, _)| *v);
        if let Some((_, c)) = nearest {
            return Some((c, Match::Approximate));
        }
    }
    catalogue
        .clone()
        .find(|c| c.version.as_deref() == Some("latest"))
        .map(|c| (*c, Match::Approximate))
}

/// Words that mean the same in ArduPilot's names and descriptions (P8).
const SYNONYMS: [(&str, &str); 8] = [
    ("rtl", "return"),
    ("fs", "failsafe"),
    ("batt", "battery"),
    ("alt", "altitude"),
    ("wp", "waypoint"),
    ("thr", "throttle"),
    ("gcs", "ground station"),
    ("arspd", "airspeed"),
];

impl ParamDefinition {
    /// How well a query (lower case) matches what this parameter means: 2
    /// when every word, or its ArduPilot abbreviation, is in the display
    /// name, 1 when each is in the display name, the description, the name
    /// or a value's or bit's name, else 0. The name itself is matched by the
    /// caller.
    #[must_use]
    pub fn meaning_score(&self, query: &str) -> u8 {
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.is_empty() {
            return 0;
        }
        let display = self.display_name.to_lowercase();
        let mut text = format!(
            "{} {display} {}",
            self.name.to_lowercase().replace('_', " "),
            self.description.to_lowercase()
        );
        for name in self
            .values
            .iter()
            .map(|v| &v.name)
            .chain(self.bitmask.iter().map(|b| &b.name))
        {
            text.push(' ');
            text.push_str(&name.to_lowercase());
        }
        let found = |haystack: &str, word: &str| {
            haystack.contains(word)
                || SYNONYMS.iter().any(|(a, b)| {
                    (word == *a && haystack.contains(b)) || (word == *b && haystack.contains(a))
                })
        };
        if words.iter().all(|w| found(&display, w)) {
            2
        } else {
            u8::from(words.iter().all(|w| found(&text, w)))
        }
    }
}

/// Where a vehicle's definitions have got (P3, P4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DefinitionsStatus {
    /// Being chosen or downloaded.
    Loading,
    Ready,
    /// None fits this vehicle: names and values only.
    Missing,
    /// The chosen set could not be had: names and values only.
    Failed,
}

/// Which set describes a vehicle's parameters, and how well it fits. Keyed
/// by vehicle id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleDefinitions {
    pub vehicle_id: String,
    pub status: DefinitionsStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_id: Option<String>,
    /// The set's name for people: "Copter 4.5.7", or a person's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    /// The one quiet line the Parameters tab shows, when there is one: an
    /// approximate set, or why there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A set a person added (P5); its parameters are kept by the host, not in
/// the journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnSet {
    pub id: String,
    pub label: String,
    pub firmware: Firmware,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Bound to vehicles built from this commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Bound to this vehicle. With neither, to every vehicle of its firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub parameter_count: u32,
    pub added_at_us: u64,
    /// Where a CI publishes it, fetched again to stay current (S74). Such
    /// a set moves to the person's other computers with sync, which fetch
    /// it themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When it was last fetched from its URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_us: Option<u64>,
}

impl OwnSet {
    /// Its version for sync: when it was last fetched, else added.
    #[must_use]
    pub fn updated_at_us(&self) -> u64 {
        self.fetched_at_us.unwrap_or(self.added_at_us)
    }

    #[must_use]
    pub fn candidate(&self) -> Candidate {
        Candidate {
            id: self.id.clone(),
            firmware: self.firmware,
            version: self.version.clone(),
            commit: self.commit.clone(),
            vehicle_id: self.vehicle_id.clone(),
            own: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[(&str, Option<&str>)]) -> DefinitionSet {
        let mut set = DefinitionSet {
            source: DefinitionSource::Ardupilot,
            firmware: Firmware::ArduCopter,
            version: Some("4.5.7".into()),
            commit: None,
            parameters: names
                .iter()
                .map(|(name, variant)| ParamDefinition {
                    name: (*name).into(),
                    variant: variant.map(Into::into),
                    ..ParamDefinition::default()
                })
                .collect(),
        };
        set.sort();
        set
    }

    #[test]
    fn a_helicopter_takes_its_own_variant_and_a_quad_the_other() {
        let set = set(&[
            ("ATC_RAT_PIT_D", None),
            ("ATC_RAT_PIT_D", Some("AC_AttitudeControl_Heli")),
            ("RTL_ALT", None),
        ]);
        assert_eq!(set.find("ATC_RAT_PIT_D", false).unwrap().variant, None);
        assert_eq!(
            set.find("ATC_RAT_PIT_D", true).unwrap().variant.as_deref(),
            Some("AC_AttitudeControl_Heli")
        );
        assert!(set.find("RTL_ALT", true).is_some(), "one variant fits all");
        assert!(set.find("NOPE", false).is_none());
    }

    #[test]
    fn a_parameter_is_found_by_what_it_means() {
        let definition = ParamDefinition {
            name: "BATT_FS_LOW_ACT".into(),
            display_name: "Low battery failsafe action".into(),
            description: "What the vehicle does when the battery runs low.".into(),
            values: vec![ValueName {
                value: 2.0,
                name: "RTL".into(),
            }],
            ..ParamDefinition::default()
        };
        assert_eq!(definition.meaning_score("battery failsafe"), 2);
        assert_eq!(definition.meaning_score("batt fs"), 2, "abbreviations fold");
        assert_eq!(definition.meaning_score("runs low"), 1);
        assert_eq!(
            definition.meaning_score("return"),
            1,
            "a value's name, as RTL"
        );
        assert_eq!(definition.meaning_score("compass"), 0);
        assert_eq!(definition.meaning_score(""), 0);
    }

    #[test]
    fn firmware_from_mav_type() {
        assert_eq!(Firmware::of("Helicopter"), Some(Firmware::ArduCopter));
        assert_eq!(Firmware::of("VtolTiltrotor"), Some(Firmware::ArduPlane));
        assert_eq!(Firmware::of("SurfaceBoat"), Some(Firmware::Rover));
        assert_eq!(Firmware::of("Submarine"), Some(Firmware::ArduSub));
        assert_eq!(Firmware::of("Airship"), Some(Firmware::Blimp));
        assert_eq!(Firmware::of("Gcs"), None);
        assert_eq!(Firmware::parse("copter"), Some(Firmware::ArduCopter));
    }

    #[test]
    fn versions_parse_from_what_vehicles_say() {
        let v = |text| Version::parse(text).map(|v| v.to_string());
        assert_eq!(v("4.5.7").as_deref(), Some("4.5.7"));
        assert_eq!(v("4.5.8-dev").as_deref(), Some("4.5.8"));
        assert_eq!(v("ArduCopter V4.6.0").as_deref(), Some("4.6.0"));
        assert_eq!(v("latest"), None);
    }

    fn candidate(id: &str, firmware: Firmware, version: &str) -> Candidate {
        Candidate {
            id: id.into(),
            firmware,
            version: Some(version.into()),
            commit: None,
            vehicle_id: None,
            own: false,
        }
    }

    #[test]
    fn the_choice_follows_p3s_order() {
        let mut all = vec![
            candidate("c-4.5.6", Firmware::ArduCopter, "4.5.6"),
            candidate("c-4.5.7", Firmware::ArduCopter, "4.5.7"),
            candidate("c-latest", Firmware::ArduCopter, "latest"),
            candidate("p-4.5.9", Firmware::ArduPlane, "4.5.9"),
        ];
        let mut vehicle = ChoosingFor {
            vehicle_id: "v1",
            firmware: Firmware::ArduCopter,
            version: Version::parse("4.5.7"),
            commit: Some("deadbeef"),
        };
        let chosen = |all: &[Candidate], vehicle: &ChoosingFor<'_>| {
            choose(all, vehicle).map(|(c, m)| (c.id.clone(), m))
        };
        assert_eq!(
            chosen(&all, &vehicle),
            Some(("c-4.5.7".into(), Match::Exact))
        );
        vehicle.version = Version::parse("4.5.8-dev");
        assert_eq!(
            chosen(&all, &vehicle),
            Some(("c-4.5.7".into(), Match::Approximate)),
            "the nearest older release"
        );
        vehicle.version = Version::parse("4.7.0");
        assert_eq!(
            chosen(&all, &vehicle),
            Some(("c-latest".into(), Match::Approximate)),
            "no release of 4.7: the development build's"
        );
        all[0].commit = Some("deadbeef00".into());
        assert_eq!(
            chosen(&all, &vehicle),
            Some(("c-4.5.6".into(), Match::Commit))
        );
        all.push(Candidate {
            id: "fork".into(),
            firmware: Firmware::ArduCopter,
            version: None,
            commit: None,
            vehicle_id: Some("v1".into()),
            own: true,
        });
        assert_eq!(chosen(&all, &vehicle), Some(("fork".into(), Match::Own)));
        vehicle.firmware = Firmware::ArduSub;
        assert_eq!(chosen(&all, &vehicle), None, "never another firmware's");
    }
}
