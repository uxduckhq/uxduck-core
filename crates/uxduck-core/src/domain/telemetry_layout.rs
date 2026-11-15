//! Telemetry layouts (S65, ADR-0009): which tiles the Telemetry tab shows,
//! in what order, and how each is coloured. They belong to the operator and
//! move between computers with the dock's saved layouts. The tab shows the
//! first of: the layout chosen for the vehicle, the operator's own for the
//! vehicle's type, UXDuck's default for the type (the app's own).

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

/// The kinds of vehicle that get their own default tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleClass {
    Plane,
    Copter,
    Airship,
    Rover,
    Boat,
    Sub,
    Other,
}

impl VehicleClass {
    /// From MAVLink's `MAV_TYPE` name, as a vehicle reports it.
    #[must_use]
    pub fn of(mav_type: &str) -> Self {
        let t = mav_type.to_ascii_lowercase();
        if t.contains("airship") || t.contains("blimp") {
            Self::Airship
        } else if t.contains("submarine") {
            Self::Sub
        } else if t.contains("surface") || t.contains("boat") {
            Self::Boat
        } else if t.contains("ground") || t.contains("rover") {
            Self::Rover
        } else if t.contains("wing") || t.contains("vtol") || t.contains("plane") {
            Self::Plane
        } else if t.contains("rotor")
            || t.contains("copter")
            || t.contains("heli")
            || t == "coaxial"
        {
            Self::Copter
        } else {
            Self::Other
        }
    }

    /// A rover, boat or submarine: it keeps to the ground or the water,
    /// whatever its height above home says, so it is never flying. An
    /// unknown type may fly.
    #[must_use]
    pub const fn never_flies(self) -> bool {
        matches!(self, Self::Rover | Self::Boat | Self::Sub)
    }
}

/// How a rule compares the value with its threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleTest {
    Below,
    AtLeast,
    Equals,
}

/// The colour a rule fills the tile with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleColour {
    Red,
    Amber,
    Green,
}

/// One colour rule: the first that matches fills the tile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColourRule {
    pub test: RuleTest,
    /// Always finite: `cleaned` refuses any other.
    pub value: f64,
    pub colour: RuleColour,
}

// Thresholds are finite (see `cleaned`), so equality is total.
impl Eq for ColourRule {}

/// One tile: a telemetry item, already reduced and named with its unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    /// The telemetry item's key, such as `battery.remaining_pct`.
    pub item: String,
    /// Its label, if not the item's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ColourRule>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryLayout {
    pub id: String,
    pub name: String,
    /// The type it is the operator's own default for; none for a layout
    /// made to choose for particular vehicles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<VehicleClass>,
    pub tiles: Vec<Tile>,
}

/// The longest tile set name, in characters, and the longest id and item
/// key, in bytes. A tile's label has no longest: the window has never had
/// one, so a label is kept whole.
pub const MAX_TEXT: usize = 64;
/// The most decimals a tile shows.
pub const MAX_DECIMALS: u8 = 6;

/// Checks and tidies an operator's tile sets: names and labels trimmed, a
/// blank label gone, and every tile set, tile and colour rule kept, however
/// many there are and however long a label is. Nothing is cut, clamped or
/// left out to make a tile set fit: one that cannot be kept as given is
/// refused, with why. A vehicle's
/// choice of a tile set that is not among them goes, as it does when its
/// tile set is deleted.
///
/// # Errors
///
/// The first thing that cannot be kept, saying which tile set and why.
pub fn cleaned(
    layouts: Vec<TelemetryLayout>,
    chosen: BTreeMap<String, String>,
) -> Result<(Vec<TelemetryLayout>, BTreeMap<String, String>), String> {
    let mut ids: HashSet<String> = HashSet::with_capacity(layouts.len());
    let mut classes: Vec<VehicleClass> = Vec::new();
    let mut kept: Vec<TelemetryLayout> = Vec::with_capacity(layouts.len());
    for mut layout in layouts {
        let name = layout.name.trim().to_owned();
        let length = name.chars().count();
        if name.is_empty() {
            return Err("Name each tile set.".into());
        }
        if length > MAX_TEXT {
            return Err(format!(
                "A tile set's name is at most {MAX_TEXT} characters, and “{name}” is {length}."
            ));
        }
        if layout.id.is_empty() || layout.id.len() > MAX_TEXT {
            return Err(format!("{name}'s id must be 1 to {MAX_TEXT} characters."));
        }
        if !ids.insert(layout.id.clone()) {
            return Err(format!("Two tile sets have the id {}.", layout.id));
        }
        if let Some(class) = layout.class {
            if classes.contains(&class) {
                return Err(format!(
                    "{name} is a second tile set for the same type of vehicle. A type has one of its own."
                ));
            }
            classes.push(class);
        }
        layout.name = name;
        for tile in &mut layout.tiles {
            if tile.item.is_empty() || tile.item.len() > MAX_TEXT {
                return Err(format!(
                    "Each tile in {} shows a telemetry item, by a key of 1 to {MAX_TEXT} characters.",
                    layout.name
                ));
            }
            tile.label = tile
                .label
                .take()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty());
            if tile.decimals.is_some_and(|d| d > MAX_DECIMALS) {
                return Err(format!(
                    "A tile shows at most {MAX_DECIMALS} decimals, and {} in {} asks for more.",
                    tile.item, layout.name
                ));
            }
            if tile.rules.iter().any(|rule| !rule.value.is_finite()) {
                return Err(format!(
                    "A colour rule on {} in {} needs a number to compare with.",
                    tile.item, layout.name
                ));
            }
        }
        kept.push(layout);
    }
    let chosen = chosen
        .into_iter()
        .filter(|(vehicle, layout)| vehicle.len() <= MAX_TEXT && ids.contains(layout))
        .collect();
    Ok((kept, chosen))
}

/// The layout the Telemetry tab shows for a vehicle: the one chosen for it,
/// else the operator's own for its type; `None` for UXDuck's default.
#[must_use]
pub fn for_vehicle<'a>(
    layouts: &'a [TelemetryLayout],
    chosen: &BTreeMap<String, String>,
    vehicle_id: &str,
    class: VehicleClass,
) -> Option<&'a TelemetryLayout> {
    chosen
        .get(vehicle_id)
        .and_then(|id| layouts.iter().find(|l| &l.id == id))
        .or_else(|| layouts.iter().find(|l| l.class == Some(class)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(id: &str, class: Option<VehicleClass>) -> TelemetryLayout {
        TelemetryLayout {
            id: id.into(),
            name: id.into(),
            class,
            tiles: vec![Tile {
                item: "battery.remaining_pct".into(),
                label: None,
                decimals: None,
                rules: Vec::new(),
            }],
        }
    }

    #[test]
    fn the_chosen_layout_then_the_operators_own_then_the_default() {
        let layouts = vec![
            layout("survey", None),
            layout("my-plane", Some(VehicleClass::Plane)),
        ];
        let mut chosen = BTreeMap::new();
        chosen.insert("v1".to_owned(), "survey".to_owned());
        let plane = VehicleClass::of("FixedWing");
        assert_eq!(plane, VehicleClass::Plane);
        assert_eq!(
            for_vehicle(&layouts, &chosen, "v1", plane).unwrap().id,
            "survey"
        );
        assert_eq!(
            for_vehicle(&layouts, &chosen, "v2", plane).unwrap().id,
            "my-plane"
        );
        assert!(for_vehicle(&layouts, &chosen, "v2", VehicleClass::of("GroundRover")).is_none());
    }

    fn rule(value: f64) -> ColourRule {
        ColourRule {
            test: RuleTest::Below,
            value,
            colour: RuleColour::Red,
        }
    }

    #[test]
    fn tile_sets_are_tidied_and_a_choice_of_one_that_is_gone_goes() {
        let mut named = layout("a", Some(VehicleClass::Copter));
        named.name = "  Survey  ".into();
        named.tiles[0].label = Some("  Battery  ".into());
        let mut blank_label = layout("b", None);
        blank_label.tiles[0].label = Some("   ".into());
        let mut chosen = BTreeMap::new();
        chosen.insert("v1".to_owned(), "gone".to_owned());
        chosen.insert("v2".to_owned(), "b".to_owned());
        let (kept, chosen) = cleaned(vec![named, blank_label], chosen).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].name, "Survey");
        assert_eq!(kept[0].tiles[0].label.as_deref(), Some("Battery"));
        assert_eq!(kept[1].tiles[0].label, None);
        assert_eq!(chosen.len(), 1, "a choice of a tile set that is gone goes");
        assert_eq!(chosen["v2"], "b");
    }

    /// Until 2026-11-14 the 33rd tile set, the 65th tile and the ninth
    /// colour rule were dropped without a word.
    #[test]
    fn every_tile_set_tile_and_rule_is_kept_however_many() {
        let sets: Vec<TelemetryLayout> = (0..50)
            .map(|n| {
                let mut set = layout(&format!("set-{n}"), None);
                set.tiles = (0..100)
                    .map(|t| Tile {
                        item: format!("mavlink.RC_CHANNELS.chan{t}_raw"),
                        label: None,
                        decimals: Some(6),
                        rules: (0..12).map(|r| rule(f64::from(r))).collect(),
                    })
                    .collect();
                set
            })
            .collect();
        let (kept, _) = cleaned(sets.clone(), BTreeMap::new()).unwrap();
        assert_eq!(kept, sets);
        assert_eq!(kept.len(), 50);
        assert_eq!(kept[49].tiles.len(), 100);
        assert_eq!(kept[49].tiles[99].rules.len(), 12);
    }

    /// Until 2026-11-14 a name over 64 characters was cut to 64 without a
    /// word, and each of the rest was dropped or changed.
    #[test]
    fn what_cannot_be_kept_as_given_is_refused_with_why() {
        let refused = |edit: fn(&mut TelemetryLayout)| {
            let mut set = layout("a", Some(VehicleClass::Copter));
            set.name = "Survey".into();
            edit(&mut set);
            cleaned(vec![set], BTreeMap::new()).unwrap_err()
        };
        assert_eq!(
            refused(|s| s.name = "n".repeat(65)),
            format!(
                "A tile set's name is at most 64 characters, and “{}” is 65.",
                "n".repeat(65)
            )
        );
        assert_eq!(refused(|s| s.name = "  ".into()), "Name each tile set.");
        assert!(refused(|s| s.id = String::new()).starts_with("Survey's id must be"));
        assert!(refused(|s| s.tiles[0].item = String::new()).starts_with("Each tile in Survey"));
        assert_eq!(
            refused(|s| s.tiles[0].decimals = Some(7)),
            "A tile shows at most 6 decimals, and battery.remaining_pct in Survey asks for more."
        );
        assert!(
            refused(|s| s.tiles[0].rules = vec![rule(f64::NAN)])
                .starts_with("A colour rule on battery.remaining_pct in Survey")
        );
        // A name of exactly 64 characters is kept whole.
        let mut longest = layout("a", None);
        longest.name = "n".repeat(64);
        let (kept, _) = cleaned(vec![longest], BTreeMap::new()).unwrap();
        assert_eq!(kept[0].name.chars().count(), 64);
        // A tile's label was cut to 64 characters too. It is kept whole.
        let mut labelled = layout("a", None);
        labelled.tiles[0].label = Some("l".repeat(70));
        let (kept, _) = cleaned(vec![labelled], BTreeMap::new()).unwrap();
        assert_eq!(kept[0].tiles[0].label, Some("l".repeat(70)));

        let twice = cleaned(vec![layout("a", None), layout("a", None)], BTreeMap::new());
        assert_eq!(twice.unwrap_err(), "Two tile sets have the id a.");
        let two_defaults = cleaned(
            vec![
                layout("a", Some(VehicleClass::Copter)),
                layout("b", Some(VehicleClass::Copter)),
            ],
            BTreeMap::new(),
        );
        assert!(
            two_defaults
                .unwrap_err()
                .starts_with("b is a second tile set for the same type of vehicle.")
        );
    }

    #[test]
    fn types_from_mavlink() {
        for (mav, class) in [
            ("Quadrotor", VehicleClass::Copter),
            ("Helicopter", VehicleClass::Copter),
            ("Coaxial", VehicleClass::Copter),
            ("Airship", VehicleClass::Airship),
            ("Blimp", VehicleClass::Airship),
            ("VtolTiltrotor", VehicleClass::Plane),
            ("GroundRover", VehicleClass::Rover),
            ("SurfaceBoat", VehicleClass::Boat),
            ("Submarine", VehicleClass::Sub),
            ("Antenna", VehicleClass::Other),
        ] {
            assert_eq!(VehicleClass::of(mav), class, "{mav}");
            assert_eq!(
                class.never_flies(),
                matches!(mav, "GroundRover" | "SurfaceBoat" | "Submarine"),
                "{mav}"
            );
        }
    }
}
