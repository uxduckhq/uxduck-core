//! Telemetry layouts (S65, ADR-0009): which tiles the Telemetry tab shows,
//! in what order, and how each is coloured. They belong to the operator and
//! move between computers with the dock's saved layouts. The tab shows the
//! first of: the layout chosen for the vehicle, the operator's own for the
//! vehicle's type, UXDuck's default for the type (the app's own).

use std::collections::BTreeMap;

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
        } else if t.contains("rotor") || t.contains("copter") || t.contains("heli") {
            Self::Copter
        } else {
            Self::Other
        }
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
    /// Always finite: `cleaned` drops any other.
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

/// The most layouts an operator keeps, tiles in one and rules on a tile.
pub const MAX_LAYOUTS: usize = 32;
pub const MAX_TILES: usize = 64;
pub const MAX_RULES: usize = 8;
const MAX_TEXT: usize = 64;

/// Keeps what can be shown: bounded counts, short names, finite numbers, one
/// operator default per type, and choices only of layouts that exist.
#[must_use]
pub fn cleaned(
    layouts: Vec<TelemetryLayout>,
    chosen: BTreeMap<String, String>,
) -> (Vec<TelemetryLayout>, BTreeMap<String, String>) {
    let mut ids: Vec<String> = Vec::new();
    let mut classes: Vec<VehicleClass> = Vec::new();
    let layouts: Vec<TelemetryLayout> = layouts
        .into_iter()
        .filter_map(|mut layout| {
            let name = layout
                .name
                .trim()
                .chars()
                .take(MAX_TEXT)
                .collect::<String>();
            if layout.id.is_empty()
                || layout.id.len() > MAX_TEXT
                || ids.contains(&layout.id)
                || name.is_empty()
            {
                return None;
            }
            if let Some(class) = layout.class {
                if classes.contains(&class) {
                    layout.class = None;
                } else {
                    classes.push(class);
                }
            }
            layout.name = name;
            layout
                .tiles
                .retain(|t| !t.item.is_empty() && t.item.len() <= MAX_TEXT);
            layout.tiles.truncate(MAX_TILES);
            for tile in &mut layout.tiles {
                tile.label = tile
                    .label
                    .take()
                    .map(|l| l.trim().chars().take(MAX_TEXT).collect::<String>())
                    .filter(|l| !l.is_empty());
                tile.decimals = tile.decimals.map(|d| d.min(6));
                tile.rules.retain(|r| r.value.is_finite());
                tile.rules.truncate(MAX_RULES);
            }
            ids.push(layout.id.clone());
            Some(layout)
        })
        .take(MAX_LAYOUTS)
        .collect();
    let chosen = chosen
        .into_iter()
        .filter(|(vehicle, layout)| {
            vehicle.len() <= MAX_TEXT && layouts.iter().any(|l| &l.id == layout)
        })
        .collect();
    (layouts, chosen)
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

    #[test]
    fn layouts_are_cleaned_so_they_always_show() {
        let mut twice = layout("b", Some(VehicleClass::Copter));
        twice.name = "  ".into();
        let mut rules = layout("c", Some(VehicleClass::Copter));
        rules.tiles[0].rules = vec![ColourRule {
            test: RuleTest::Below,
            value: f64::NAN,
            colour: RuleColour::Red,
        }];
        let mut chosen = BTreeMap::new();
        chosen.insert("v1".to_owned(), "gone".to_owned());
        chosen.insert("v2".to_owned(), "c".to_owned());
        let (kept, chosen) = cleaned(
            vec![layout("a", Some(VehicleClass::Copter)), twice, rules],
            chosen,
        );
        assert_eq!(kept.len(), 2, "a nameless layout goes");
        assert_eq!(kept[1].class, None, "one own layout per type");
        assert!(kept[1].tiles[0].rules.is_empty());
        assert_eq!(chosen.len(), 1, "a choice of a layout that is gone goes");
    }

    #[test]
    fn types_from_mavlink() {
        for (mav, class) in [
            ("Quadrotor", VehicleClass::Copter),
            ("Helicopter", VehicleClass::Copter),
            ("Airship", VehicleClass::Airship),
            ("Blimp", VehicleClass::Airship),
            ("VtolTiltrotor", VehicleClass::Plane),
            ("GroundRover", VehicleClass::Rover),
            ("SurfaceBoat", VehicleClass::Boat),
            ("Submarine", VehicleClass::Sub),
            ("Antenna", VehicleClass::Other),
        ] {
            assert_eq!(VehicleClass::of(mav), class, "{mav}");
        }
    }
}
