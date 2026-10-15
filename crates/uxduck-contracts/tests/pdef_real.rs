//! ArduPilot's real `apm.pdef.json` and `apm.pdef.xml` (S67). They are GPL,
//! so they are not in this repository: set `UXDUCK_PDEF_DIR` to a folder
//! holding both, as `param_parse.py --format json` and `--format xml` write
//! them for one vehicle, to run this. Skips without it.

use std::path::PathBuf;

use uxduck_contracts::pdef::{from_json, from_xml, merge};
use uxduck_core::domain::param_definitions::{
    DefinitionSet, DefinitionSource, Firmware, UserLevel,
};

#[test]
fn real_definitions_convert_with_every_variant() {
    let Some(dir) = std::env::var_os("UXDUCK_PDEF_DIR").map(PathBuf::from) else {
        eprintln!("skipped: UXDUCK_PDEF_DIR is not set");
        return;
    };
    let json = std::fs::read_to_string(dir.join("apm.pdef.json")).unwrap();
    let xml = std::fs::read_to_string(dir.join("apm.pdef.xml")).unwrap();
    let (firmware, json) = from_json(&json).unwrap();
    let (xml_firmware, xml) = from_xml(&xml).unwrap();
    let firmware = firmware.expect("the file names its firmware");
    assert_eq!(xml_firmware, Some(firmware));
    assert!(json.len() > 1000, "{} parameters", json.len());
    assert!(xml.len() >= json.len(), "the XML keeps every variant");
    // The JSON leaves out a few the XML has (Copter 4.5.7's
    // FLTMODE_GCSBLOCK), never the other way round.
    let missing: Vec<&str> = json
        .iter()
        .filter(|j| !xml.iter().any(|x| x.name == j.name))
        .map(|j| j.name.as_str())
        .collect();
    assert!(missing.is_empty(), "only in the JSON: {missing:?}");
    let set = DefinitionSet {
        source: DefinitionSource::Ardupilot,
        firmware,
        version: None,
        commit: None,
        parameters: merge(xml, &json),
    };
    let standard = set
        .parameters
        .iter()
        .filter(|p| p.user == UserLevel::Standard)
        .count();
    assert!(standard > 100);
    assert!(set.parameters.iter().any(|p| !p.values.is_empty()));
    assert!(set.parameters.iter().any(|p| !p.bitmask.is_empty()));
    assert!(set.parameters.iter().all(|p| !p.group.is_empty()));
    if firmware == Firmware::ArduCopter {
        let rtl = set.find("RTL_ALT", false).unwrap();
        assert_eq!(rtl.units.as_deref(), Some("cm"));
        let multi = set.find("ATC_RAT_PIT_D", false).unwrap();
        let heli = set.find("ATC_RAT_PIT_D", true).unwrap();
        assert!(multi.variant.is_none());
        assert!(heli.variant.as_deref().is_some_and(|v| v.contains("Heli")));
        assert_ne!(multi.range, heli.range, "each frame its own range");
    }
    let size = serde_json::to_vec(&set).unwrap().len();
    eprintln!(
        "{}: {} definitions, {} KB as JSON",
        firmware.name(),
        set.parameters.len(),
        size / 1024
    );
}
