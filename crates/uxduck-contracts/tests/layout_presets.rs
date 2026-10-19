use uxduck_contracts::dto::operator as dto;
use uxduck_core::domain::operator::{DockNode, Layout, TabKind};

#[test]
fn adopted_presets_keep_their_version_baseline_and_customisation_on_the_wire() {
    let mut core = Layout::initial();
    core.dock = DockNode::of(&[TabKind::Map, TabKind::Parameters]);
    core.saved[0].dock = core.dock.clone();
    let wire = dto::Layout::from(&core);
    let json = serde_json::to_value(&wire).expect("a layout serialises");
    let back: dto::Layout = serde_json::from_value(json).expect("the wire layout deserialises");
    assert_eq!(Layout::from(back), core);
}

#[test]
fn the_simple_default_matches_the_shared_web_fixture() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../fixtures/operator_update.request.simple-view.json"
    ))
    .expect("a valid fixture");
    let wire = dto::Layout::from(&Layout::initial());
    assert_eq!(
        serde_json::to_value(wire).expect("a serialisable layout"),
        fixture["params"]["layout"]
    );
}

#[test]
fn old_built_ins_migrate_but_named_copies_keep_their_arrangement() {
    let mut old = Layout::initial();
    let stale = DockNode::of(&[TabKind::Map]);
    old.dock = stale.clone();
    old.saved[0].dock = stale.clone();
    let preset = old.saved[0].preset.as_mut().unwrap();
    preset.version = 3;
    preset.base = stale.clone();
    old.saved.push(uxduck_core::domain::operator::SavedLayout {
        id: "survey".into(),
        name: "Survey".into(),
        dock: stale.clone(),
        preset: None,
    });
    let restored: Layout = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(restored.dock, Layout::simple_dock());
    assert_eq!(restored.saved[1].dock, stale);
    assert_eq!(
        restored.saved[0].preset.as_ref().unwrap().version,
        uxduck_core::domain::operator::LAYOUT_PRESET_VERSION
    );
    old.current = Some("survey".into());
    let restored: Layout = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(restored.dock, stale);
    assert_eq!(restored.saved[0].dock, Layout::simple_dock());
}
