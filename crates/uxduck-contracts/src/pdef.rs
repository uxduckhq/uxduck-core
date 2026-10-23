//! ArduPilot's parameter documentation, as `param_parse.py` writes it, into
//! UXDuck's definition sets (ADR-0021 P1, S67).
//!
//! Neither of its formats is whole. `apm.pdef.json` is keyed by name, so a
//! name documented twice (Copter's multicopter and helicopter attitude gains)
//! keeps only the last, though it says which library that one came from.
//! `apm.pdef.xml` keeps every variant but not where each came from. The
//! catalogue reads both and [`merge`]s them; a file a person adds is read
//! alone with [`read`].

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde_json::Value;
use uxduck_core::domain::param_definitions::{
    BitName, DefinitionSet, Firmware, ParamDefinition, ParamRange, UserLevel, ValueName,
};

/// What a file held: ArduPilot's documentation, or one of our own sets.
#[derive(Debug)]
pub enum Read {
    /// `apm.pdef.json` or `apm.pdef.xml`, with the firmware when the file
    /// names it.
    Pdef {
        firmware: Option<Firmware>,
        parameters: Vec<ParamDefinition>,
    },
    Set(DefinitionSet),
}

/// Reads either of ArduPilot's formats, or a set in our own.
///
/// # Errors
/// When the text is none of them, or holds no parameters.
pub fn read(text: &str) -> Result<Read, String> {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with('<') {
        let (firmware, parameters) = from_xml(text)?;
        return Ok(Read::Pdef {
            firmware,
            parameters,
        });
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON or XML: {e}"))?;
    if value.get("parameters").is_some() && value.get("firmware").is_some() {
        let mut set: DefinitionSet = serde_json::from_value(value)
            .map_err(|e| format!("not a UXDuck definition set: {e}"))?;
        set.sort();
        return Ok(Read::Set(set));
    }
    let (firmware, parameters) = from_json_value(&value)?;
    Ok(Read::Pdef {
        firmware,
        parameters,
    })
}

/// `apm.pdef.json`.
///
/// # Errors
/// When it is not JSON in that shape, or holds no parameters.
pub fn from_json(text: &str) -> Result<(Option<Firmware>, Vec<ParamDefinition>), String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    from_json_value(&value)
}

fn from_json_value(value: &Value) -> Result<(Option<Firmware>, Vec<ParamDefinition>), String> {
    let groups = value
        .as_object()
        .ok_or("not ArduPilot's apm.pdef.json: not an object")?;
    let mut firmware = None;
    let mut parameters = Vec::new();
    for (group, entries) in groups {
        if group == "json" {
            continue;
        }
        if let Some(f) = Firmware::parse(group) {
            firmware = Some(f);
        }
        let Some(entries) = entries.as_object() else {
            continue;
        };
        for (name, fields) in entries {
            let Some(fields) = fields.as_object() else {
                continue;
            };
            let text = |key: &str| fields.get(key).and_then(Value::as_str);
            let flag = |key: &str| text(key).is_some_and(truthy);
            let range = fields.get("Range").and_then(|r| {
                Some(ParamRange {
                    low: number(r.get("low")?.as_str()?)?,
                    high: number(r.get("high")?.as_str()?)?,
                })
            });
            let pairs = |key: &str| -> Vec<(String, String)> {
                fields
                    .get(key)
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            parameters.push(ParamDefinition {
                name: name.clone(),
                display_name: text("DisplayName").unwrap_or_default().trim().to_owned(),
                description: text("Description").unwrap_or_default().trim().to_owned(),
                group: group_of(group, name),
                units: text("Units").map(str::to_owned),
                range,
                increment: text("Increment").and_then(number),
                values: values(pairs("Values")),
                bitmask: bits(pairs("Bitmask")),
                user: user(text("User")),
                reboot_required: flag("RebootRequired"),
                read_only: flag("ReadOnly"),
                volatile: flag("Volatile"),
                calibration: flag("Calibration"),
                variant: text("path").map(str::to_owned),
            });
        }
    }
    if parameters.is_empty() {
        return Err("no parameters in it".to_owned());
    }
    parameters.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((firmware, parameters))
}

/// `apm.pdef.xml`, every variant kept in the order it was documented.
///
/// # Errors
/// When it is not XML in that shape, or holds no parameters.
pub fn from_xml(text: &str) -> Result<(Option<Firmware>, Vec<ParamDefinition>), String> {
    let mut reader = Reader::from_str(text);
    let mut firmware = None;
    let mut group = String::new();
    let mut parameters: Vec<ParamDefinition> = Vec::new();
    let mut current: Option<ParamDefinition> = None;
    // What the text that follows belongs to.
    let mut field: Option<String> = None;
    let mut value_code: Option<String> = None;
    let mut in_values = false;
    let mut content = String::new();
    let bad = |e: &dyn std::fmt::Display| format!("not ArduPilot's apm.pdef.xml: {e}");
    loop {
        let event = reader.read_event().map_err(|e| bad(&e))?;
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let empty = matches!(event, Event::Empty(_));
                match tag.name().as_ref() {
                    "parameters" => {
                        group = attribute(tag, "name").unwrap_or_default();
                        if let Some(f) = Firmware::parse(&group) {
                            firmware = Some(f);
                        }
                    }
                    "param" => {
                        let full = attribute(tag, "name").unwrap_or_default();
                        // Vehicle parameters are written `ArduCopter:RTL_ALT`.
                        let name = full.rsplit(':').next().unwrap_or_default().to_owned();
                        let definition = ParamDefinition {
                            group: group_of(&group, &name),
                            name,
                            display_name: attribute(tag, "humanName")
                                .unwrap_or_default()
                                .trim()
                                .to_owned(),
                            description: attribute(tag, "documentation")
                                .unwrap_or_default()
                                .trim()
                                .to_owned(),
                            user: user(attribute(tag, "user").as_deref()),
                            ..ParamDefinition::default()
                        };
                        if empty {
                            parameters.push(definition);
                        } else {
                            current = Some(definition);
                        }
                    }
                    "field" => {
                        content.clear();
                        field = (!empty).then(|| attribute(tag, "name")).flatten();
                    }
                    "values" => in_values = true,
                    "value" if in_values => {
                        content.clear();
                        value_code = (!empty).then(|| attribute(tag, "code")).flatten();
                    }
                    _ => {}
                }
            }
            Event::Text(raw) => content.push_str(&raw.xml10_content()),
            Event::CData(raw) => content.push_str(&raw.xml10_content()),
            Event::GeneralRef(raw) => {
                let reference = format!("&{};", raw.as_ref());
                content.push_str(&quick_xml::escape::unescape(&reference).map_err(|e| bad(&e))?);
            }
            Event::End(ref tag) => match tag.name().as_ref() {
                "param" => parameters.extend(current.take()),
                "field" => {
                    if let (Some(definition), Some(name)) = (current.as_mut(), field.take()) {
                        apply_field(definition, &name, content.trim());
                    }
                    content.clear();
                }
                "value" => {
                    if let (Some(definition), Some(code)) = (current.as_mut(), value_code.take())
                        && let Some(value) = number(&code)
                    {
                        definition.values.push(ValueName {
                            value,
                            name: content.trim().to_owned(),
                        });
                    }
                    content.clear();
                }
                "values" => in_values = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if parameters.is_empty() {
        return Err("no parameters in it".to_owned());
    }
    // Stable, so variants keep their documented order.
    parameters.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((firmware, parameters))
}

/// Every variant from the XML, each labelled with its library where the JSON
/// says which one it kept.
#[must_use]
pub fn merge(xml: Vec<ParamDefinition>, json: &[ParamDefinition]) -> Vec<ParamDefinition> {
    let labelled: BTreeMap<&str, &ParamDefinition> = json
        .iter()
        .filter(|p| p.variant.is_some())
        .map(|p| (p.name.as_str(), p))
        .collect();
    let mut merged = xml;
    for definition in &mut merged {
        if let Some(from_json) = labelled.get(definition.name.as_str()) {
            let unlabelled = ParamDefinition {
                variant: None,
                ..(*from_json).clone()
            };
            if *definition == unlabelled {
                definition.variant.clone_from(&from_json.variant);
            }
        }
    }
    merged
}

fn apply_field(definition: &mut ParamDefinition, name: &str, text: &str) {
    match name {
        "Range" => {
            let mut ends = text.split_whitespace().filter_map(number);
            if let (Some(low), Some(high)) = (ends.next(), ends.next()) {
                definition.range = Some(ParamRange { low, high });
            }
        }
        "Increment" => definition.increment = number(text),
        "Units" => definition.units = Some(text.to_owned()),
        "Bitmask" => definition.bitmask = bits(split_list(text)),
        "RebootRequired" => definition.reboot_required = truthy(text),
        "ReadOnly" => definition.read_only = truthy(text),
        "Volatile" => definition.volatile = truthy(text),
        "Calibration" => definition.calibration = truthy(text),
        _ => {}
    }
}

/// `0:Roll,1:Pitch`, where a name may hold a comma.
fn split_list(text: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in text.split(',') {
        match part.split_once(':') {
            Some((code, name)) if number(code.trim()).is_some() => {
                pairs.push((code.trim().to_owned(), name.trim().to_owned()));
            }
            _ => {
                if let Some(last) = pairs.last_mut() {
                    last.1.push(',');
                    last.1.push_str(part);
                }
            }
        }
    }
    pairs
}

fn values(pairs: Vec<(String, String)>) -> Vec<ValueName> {
    let mut values: Vec<ValueName> = pairs
        .into_iter()
        .filter_map(|(code, name)| {
            Some(ValueName {
                value: number(&code)?,
                name,
            })
        })
        .collect();
    values.sort_by(|a, b| a.value.total_cmp(&b.value));
    values
}

fn bits(pairs: Vec<(String, String)>) -> Vec<BitName> {
    let mut bits: Vec<BitName> = pairs
        .into_iter()
        .filter_map(|(code, name)| {
            let bit = code.trim().parse::<u8>().ok().filter(|b| *b < 64)?;
            Some(BitName { bit, name })
        })
        .collect();
    bits.sort_by_key(|b| b.bit);
    bits
}

/// The vehicle's own group is named after it (or `VEHICLE`): its parameters
/// are grouped by their own prefix instead.
fn group_of(group: &str, name: &str) -> String {
    let own = group.is_empty()
        || group.eq_ignore_ascii_case("VEHICLE")
        || Firmware::parse(group).is_some();
    if own {
        name.find('_')
            .map_or_else(|| name.to_owned(), |end| name[..=end].to_owned())
    } else {
        group.to_owned()
    }
}

fn user(text: Option<&str>) -> UserLevel {
    if text.is_some_and(|t| t.eq_ignore_ascii_case("Standard")) {
        UserLevel::Standard
    } else {
        UserLevel::Advanced
    }
}

fn truthy(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

fn number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

fn attribute(tag: &BytesStart<'_>, name: &str) -> Option<String> {
    tag.try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(std::borrow::Cow::into_owned)
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "numbers read exactly from text")]
mod tests {
    use super::*;

    const JSON: &str = include_str!("../fixtures/pdef/copter.pdef.json");
    const XML: &str = include_str!("../fixtures/pdef/copter.pdef.xml");

    fn find<'a>(all: &'a [ParamDefinition], name: &str) -> &'a ParamDefinition {
        all.iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn json_reads_every_field() {
        let (firmware, all) = from_json(JSON).unwrap();
        assert_eq!(firmware, Some(Firmware::ArduCopter));
        let rtl = find(&all, "RTL_ALT");
        assert_eq!(rtl.display_name, "RTL Altitude");
        assert_eq!(rtl.group, "RTL_");
        assert_eq!(rtl.units.as_deref(), Some("cm"));
        assert_eq!(
            rtl.range,
            Some(ParamRange {
                low: 200.0,
                high: 300_000.0
            })
        );
        assert_eq!(rtl.user, UserLevel::Standard);
        let fs = find(&all, "FS_THR_ENABLE");
        assert_eq!(fs.values[2].value, 3.0);
        assert_eq!(fs.values[2].name, "Enabled always Land");
        let log = find(&all, "LOG_BITMASK");
        assert_eq!(
            log.bitmask.iter().map(|b| b.bit).collect::<Vec<_>>(),
            [0, 1, 2, 10],
            "bits sorted as numbers"
        );
        assert!(find(&all, "FRAME_CLASS").reboot_required);
        assert_eq!(find(&all, "BATT_FS_LOW_ACT").group, "BATT_");
        let baro = find(&all, "BARO1_GND_PRESS");
        assert!(baro.read_only && baro.volatile);
        assert!(find(&all, "INS_ACCOFFS_X").calibration);
        assert_eq!(find(&all, "INS_ACC_ID").user, UserLevel::Advanced);
        let gains = all.iter().filter(|p| p.name == "ATC_RAT_PIT_D").count();
        assert_eq!(gains, 1, "JSON keeps one variant");
    }

    #[test]
    fn xml_reassembles_entities_and_cdata_before_applying_fields() {
        let (_, parameters) = from_xml(
            r#"<paramfile><parameters name="ArduCopter">
            <param name="TEST" humanName="A &amp; B">
                <field name="Units">m&#x2f;s</field>
                <values><value code="1">A &amp; B <![CDATA[<ready>]]> &#937;</value></values>
            </param></parameters></paramfile>"#,
        )
        .unwrap();
        assert_eq!(parameters[0].display_name, "A & B");
        assert_eq!(parameters[0].units.as_deref(), Some("m/s"));
        assert_eq!(parameters[0].values[0].name, "A & B <ready> Ω");
        assert!(from_xml(r#"<parameters><param name="X"><field name="Units">&unknown;</field></param></parameters>"#).is_err());
    }

    #[test]
    fn xml_keeps_every_variant_and_the_json_labels_them() {
        let (firmware, xml) = from_xml(XML).unwrap();
        assert_eq!(firmware, Some(Firmware::ArduCopter));
        let (_, json) = from_json(JSON).unwrap();
        // The same parameters, field for field, apart from the variants.
        let single = |all: &[ParamDefinition]| {
            all.iter()
                .filter(|p| p.name != "ATC_RAT_PIT_D")
                .cloned()
                .map(|p| ParamDefinition { variant: None, ..p })
                .collect::<Vec<_>>()
        };
        assert_eq!(single(&xml), single(&json));
        let merged = merge(xml, &json);
        let gains: Vec<_> = merged
            .iter()
            .filter(|p| p.name == "ATC_RAT_PIT_D")
            .map(|p| (p.variant.as_deref(), p.range.unwrap().high))
            .collect();
        assert_eq!(
            gains,
            [(None, 0.05), (Some("AC_AttitudeControl_Heli"), 0.02)]
        );
        let set = DefinitionSet {
            source: uxduck_core::domain::param_definitions::DefinitionSource::Ardupilot,
            firmware: Firmware::ArduCopter,
            version: Some("4.5.7".into()),
            commit: None,
            parameters: merged,
        };
        assert_eq!(
            set.find("ATC_RAT_PIT_D", false)
                .unwrap()
                .range
                .unwrap()
                .high,
            0.05
        );
        assert_eq!(
            set.find("ATC_RAT_PIT_D", true).unwrap().range.unwrap().high,
            0.02
        );
    }

    #[test]
    fn read_tells_the_formats_apart() {
        assert!(matches!(read(JSON), Ok(Read::Pdef { .. })));
        assert!(matches!(read(XML), Ok(Read::Pdef { .. })));
        let (_, parameters) = from_json(JSON).unwrap();
        let own = DefinitionSet {
            source: uxduck_core::domain::param_definitions::DefinitionSource::Own {
                label: "My fork".into(),
            },
            firmware: Firmware::ArduCopter,
            version: None,
            commit: Some("deadbeef".into()),
            parameters,
        };
        let text = serde_json::to_string(&own).unwrap();
        assert!(matches!(read(&text), Ok(Read::Set(set)) if set == own));
        assert!(read("hello").is_err());
        assert!(read("{}").is_err());
    }

    #[test]
    fn a_bitmask_name_may_hold_a_comma() {
        assert_eq!(
            split_list("0:Roll, pitch and yaw,1:Throttle"),
            [
                ("0".to_owned(), "Roll, pitch and yaw".to_owned()),
                ("1".to_owned(), "Throttle".to_owned())
            ]
        );
    }
}
