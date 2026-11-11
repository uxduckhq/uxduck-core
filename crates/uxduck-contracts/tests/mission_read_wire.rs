#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test fixtures")]

use serde_json::{Value, json};
use uxduck_contracts::dto::{mission as wire, state};
use uxduck_core::domain::{mission as domain, parameter::TransferStatus};

fn read_items() -> Vec<domain::MissionItem> {
    [
        47.25_f64.to_bits(),
        (-0.0_f64).to_bits(),
        0_u64,
        0x7ff8_0000_0000_0001,
        0xfff8_0000_0000_0042,
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
    ]
    .into_iter()
    .map(|bits| {
        let value = f64::from_bits(bits);
        domain::MissionItem {
            command: 32001,
            frame: 255,
            param1: value,
            param2: value,
            param3: value,
            param4: value,
            latitude_deg: value,
            longitude_deg: value,
            altitude_m: value,
            autocontinue: false,
        }
    })
    .collect()
}

fn golden() -> String {
    let items = read_items();
    let mission = wire::Mission::from(&domain::Mission {
        vehicle_id: "wire-vehicle".into(),
        items: items.clone(),
        fence: items.clone(),
        rally: items.clone(),
        status: TransferStatus::Synced,
        version: 7,
        detail: None,
        boot_instant_us: Some(123),
        synced_at_us: Some(456),
        progress: None,
    });
    let record = wire::MissionRecord::from(&domain::MissionRecord {
        id: "wire-record".into(),
        vehicle_id: "wire-vehicle".into(),
        items: items.clone(),
        fence: items.clone(),
        rally: items,
        origin: domain::MissionOrigin::Uploaded {
            plan_id: Some("wire-plan".into()),
            plan_name: Some("Wire fixture".into()),
            verified: Some(false),
        },
        client_id: None,
        operator_id: None,
        first_seen_us: 123,
        last_seen_us: 456,
        times: 2,
        seq: 42,
    });
    let snapshot = state::StateSubscribeResult {
        seq: 41,
        snapshot: state::Snapshot {
            missions: Some(vec![mission.clone()]),
            ..Default::default()
        },
    };
    let change: state::EntityChange = serde_json::from_value(json!({
        "entity": "mission", "id": "wire-vehicle", "op": "upsert", "mission": mission,
    }))
    .unwrap();
    let delta = state::StateDelta {
        seq: 42,
        ts_us: 456,
        actor: state::Actor {
            client_id: "wire-fixture".into(),
            operator_id: None,
            identity: None,
        },
        changes: vec![change],
    };
    let upload = wire::MissionUploadResult {
        status: uxduck_contracts::dto::parameter::WriteStatus::Accepted,
        detail: None,
        mission: mission.clone(),
    };
    let restore = wire::MissionRestoreResult {
        status: uxduck_contracts::dto::plan::PlanUploadStatus::Different,
        detail: None,
        notes: vec!["Exact readback; not a writable plan".into()],
        mission: mission.clone(),
    };
    let plan: uxduck_contracts::dto::plan::Plan =
        serde_json::from_str(include_str!("../fixtures/plan_create.result.json")).unwrap();
    let plan_upload = uxduck_contracts::dto::plan::PlanUploadResult {
        status: uxduck_contracts::dto::plan::PlanUploadStatus::Different,
        detail: None,
        notes: vec![],
        plan,
        mission: mission.clone(),
    };
    let mut value = json!({
        "mission": mission, "snapshot": snapshot, "delta": delta,
        "history": wire::MissionHistoryResult { records: vec![record] },
        "upload": upload, "restore": restore, "planUpload": plan_upload,
    });
    let frames = read_frames(&value);
    value["frames"] = Value::Object(frames);
    let mut text = serde_json::to_string_pretty(&value).unwrap();
    text.push('\n');
    text
}

fn read_frames(value: &Value) -> serde_json::Map<String, Value> {
    let mut frames = serde_json::Map::new();
    for (method, field) in [
        ("mission/refresh", "mission"),
        ("state/subscribe", "snapshot"),
        ("mission/history", "history"),
        ("mission/upload", "upload"),
        ("mission/restore", "restore"),
        ("plan/upload", "planUpload"),
    ] {
        frames.insert(
            method.into(),
            Value::String(
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0", "id": 0, "result": value[field],
                }))
                .unwrap(),
            ),
        );
    }
    frames.insert(
        "state/delta".into(),
        Value::String(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0", "method": "state/delta", "params": value["delta"],
            }))
            .unwrap(),
        ),
    );
    frames
}

#[test]
fn production_mission_read_wire_golden() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/mission-read.json");
    let generated = golden();
    if std::env::var("UXDUCK_UPDATE_MISSION_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
    }
    let current = std::fs::read_to_string(&path).expect(
        "mission-read golden is required; regenerate with UXDUCK_UPDATE_MISSION_GOLDEN=1 cargo test -p uxduck-contracts --test mission_read_wire",
    );
    assert_eq!(
        current, generated,
        "production mission read wire golden drift"
    );
    let parsed: Value = serde_json::from_str(&current).unwrap();
    for (method, field) in [
        ("mission/refresh", "mission"),
        ("state/subscribe", "snapshot"),
        ("state/delta", "delta"),
        ("mission/history", "history"),
        ("mission/upload", "upload"),
        ("mission/restore", "restore"),
        ("plan/upload", "planUpload"),
    ] {
        uxduck_contracts::dto::mission_read::validate_mission_read_result(method, &parsed[field])
            .unwrap();
    }
}

#[test]
fn read_tags_and_null_do_not_become_write_inputs() {
    let mut item = json!({
        "command": 32001, "frame": 255, "param1": 0, "param2": 0,
        "param3": 0, "param4": 0, "latitudeDeg": 47.25, "longitudeDeg": -1.5,
        "altitudeM": 40, "autocontinue": false,
    });
    for field in [
        "param1",
        "param2",
        "param3",
        "param4",
        "latitudeDeg",
        "longitudeDeg",
        "altitudeM",
    ] {
        for invalid in [Value::Null, json!({"nonFiniteF64":"7ff8000000000001"})] {
            let original = item[field].clone();
            item[field] = invalid;
            assert!(
                serde_json::from_value::<wire::MissionUploadParams>(json!({
                    "vehicleId": "v", "expectedVersion": 0, "items": [item],
                }))
                .is_err(),
                "{field}"
            );
            item[field] = original;
        }
    }
}
