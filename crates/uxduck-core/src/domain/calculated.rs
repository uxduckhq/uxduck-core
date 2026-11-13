//! A person's calculated values (ADR-0056 K1): a name, a formula over the
//! vehicle's telemetry, and a unit, kept on their operator and synced with
//! it. The daemon works them out as telemetry items in the `custom` group.

use serde::{Deserialize, Serialize};

use super::formula::{CALCULATED_GROUP, Formula};
use super::telemetry::{Unit, key_says_unit};

/// The most calculated values an operator keeps.
pub const MAX_CALCULATED: usize = 32;
/// The longest name, in characters.
pub const MAX_LABEL_CHARS: usize = 48;
/// The longest key after `custom.`, in characters.
pub const MAX_KEY_CHARS: usize = 40;
/// The longest unit of a person's own, such as "km/h", in characters.
pub const MAX_UNIT_TEXT_CHARS: usize = 12;
/// The most decimals a value is shown with.
pub const MAX_DECIMALS: u8 = 6;

/// One calculated value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalculatedItem {
    /// Its item's key, `custom.` and a name ending in its unit's suffix,
    /// such as `custom.power` or `custom.sag_v`. Kept when it is renamed.
    pub key: String,
    /// For people, such as "Power".
    pub label: String,
    pub formula: String,
    /// The unit its formula's answer is in. A unit UXDuck knows is shown
    /// in the operator's units, as the vehicle's own items are.
    pub unit: Unit,
    /// A unit of the person's own, such as "km/h", shown as written and
    /// never converted; only with no unit or a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_text: Option<String>,
    /// How many decimals to show; UXDuck's choice for the unit when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
}

/// Checks and tidies an operator's calculated values: names trimmed, and
/// each key, name, formula and unit as ADR-0056 says.
///
/// # Errors
///
/// The first value that is not, saying which and why.
pub fn clean_calculated(items: &[CalculatedItem]) -> Result<Vec<CalculatedItem>, String> {
    if items.len() > MAX_CALCULATED {
        return Err(format!(
            "An operator keeps at most {MAX_CALCULATED} calculated values."
        ));
    }
    let mut out: Vec<CalculatedItem> = Vec::with_capacity(items.len());
    for item in items {
        let label = item.label.trim();
        if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
            return Err(format!(
                "Name each calculated value, in at most {MAX_LABEL_CHARS} characters."
            ));
        }
        let name = item.key.strip_prefix(CALCULATED_GROUP).unwrap_or_default();
        if name.is_empty()
            || name.chars().count() > MAX_KEY_CHARS
            || name.contains('.')
            || !key_says_unit(&item.key, item.unit)
        {
            let suffix = item
                .unit
                .suffix()
                .map_or_else(String::new, |s| format!(", ending in {s}"));
            return Err(format!(
                "{label}'s key must be custom. and up to {MAX_KEY_CHARS} lowercase letters, digits and _{suffix}."
            ));
        }
        if out.iter().any(|o| o.key == item.key) {
            return Err(format!("Two calculated values are both {}.", item.key));
        }
        Formula::parse(&item.formula).map_err(|e| format!("{label}: {e}"))?;
        let unit_text = item
            .unit_text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if let Some(text) = unit_text {
            if !matches!(item.unit, Unit::None | Unit::Count) {
                return Err(format!(
                    "{label} has a unit UXDuck knows, so it cannot have one of your own too."
                ));
            }
            if text.chars().count() > MAX_UNIT_TEXT_CHARS {
                return Err(format!(
                    "{label}'s unit is at most {MAX_UNIT_TEXT_CHARS} characters."
                ));
            }
        }
        if item.decimals.is_some_and(|d| d > MAX_DECIMALS) {
            return Err(format!("{label} shows at most {MAX_DECIMALS} decimals."));
        }
        out.push(CalculatedItem {
            key: item.key.clone(),
            label: label.to_owned(),
            formula: item.formula.trim().to_owned(),
            unit: item.unit,
            unit_text: unit_text.map(str::to_owned),
            decimals: item.decimals,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn power() -> CalculatedItem {
        CalculatedItem {
            key: "custom.power".into(),
            label: " Power ".into(),
            formula: "battery.voltage_v * battery.current_a".into(),
            unit: Unit::None,
            unit_text: Some(" W ".into()),
            decimals: Some(0),
        }
    }

    #[test]
    fn tidies_what_it_keeps() {
        let kept = clean_calculated(&[power()]).unwrap();
        assert_eq!(kept[0].label, "Power");
        assert_eq!(kept[0].unit_text.as_deref(), Some("W"));
    }

    #[test]
    fn refuses_what_is_not_a_calculated_value() {
        let with = |edit: fn(&mut CalculatedItem)| {
            let mut item = power();
            edit(&mut item);
            clean_calculated(&[item]).unwrap_err()
        };
        assert!(with(|i| i.label = " ".into()).starts_with("Name each"));
        assert!(with(|i| i.key = "battery.power".into()).contains("key must be custom."));
        assert!(with(|i| i.unit = Unit::Volt).contains("ending in _v"));
        assert!(
            with(|i| i.key = "custom.power_v".into()).contains("key must be"),
            "no unit, so no unit's suffix"
        );
        assert!(with(|i| i.formula = "battery.voltage_v *".into()).starts_with("Power: "));
        assert!(with(|i| i.decimals = Some(7)).contains("at most 6 decimals"));
        assert!(with(|i| i.unit_text = Some("kilowatt hours".into())).contains("at most 12"));
        assert!(
            with(|i| {
                i.key = "custom.power_v".into();
                i.unit = Unit::Volt;
            })
            .contains("cannot have one of your own")
        );
        assert!(
            clean_calculated(&[power(), power()])
                .unwrap_err()
                .starts_with("Two calculated values")
        );
        assert!(
            clean_calculated(&vec![power(); MAX_CALCULATED + 1])
                .unwrap_err()
                .starts_with("An operator keeps at most")
        );
    }
}
