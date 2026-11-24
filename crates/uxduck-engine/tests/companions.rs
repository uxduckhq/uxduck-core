//! Companion computers in the vehicle profile (S296, ADR-0049 O7, O8):
//! added by host name with the next free onboard id, a private key kept in
//! the credential store and never sent, checked through a fake port in
//! place of the daemon's SSH, a refused host key keeping the one pinned,
//! and removing one forgetting its key. The profile survives a restart.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::companion::CompanionClock;
use uxduck_core::domain::companion_cameras::{
    CameraLinger, CameraWatchdog, CompanionCameraPath, CompanionCameras, CompanionMicrophone,
    CompanionMicrophoneDevice, PicturePreset, VehicleRecordMode, VehicleRecording,
};
use uxduck_core::domain::companion_software::{
    InstallAction, InstallRecord, InstallState, InstallStep, SoftwareFound,
};
use uxduck_core::domain::link::Link;
use uxduck_core::domain::notification::{Action, NotificationAction, NotificationState, Severity};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::companion_software::InstallAnswer;
use uxduck_engine::companions::{CheckAnswer, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::{
    Clock, CommandOrder, Credentials, Forget, LinkPort, NoAgent, NoSystem, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        5_000_000
    }
}

struct Counter(u64);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *byte = (self.0 >> 56) as u8;
        }
    }
}

struct NoLinks;

impl LinkPort for NoLinks {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
}

#[derive(Clone, Default)]
struct Keys(Arc<Mutex<BTreeMap<String, String>>>);

impl Credentials for Keys {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let mut all = self.0.lock().unwrap();
        if value.is_empty() {
            all.remove(name);
        } else {
            all.insert(name.to_owned(), value.to_owned());
        }
        Ok(())
    }
    fn has(&self, name: &str) -> bool {
        self.0.lock().unwrap().contains_key(name)
    }
}

#[derive(Clone, Default)]
struct Ssh(Arc<Mutex<Vec<CompanionOrder>>>);

impl CompanionPort for Ssh {
    fn request(&mut self, order: CompanionOrder) {
        self.0.lock().unwrap().push(order);
    }
}

fn engine(state: State, keys: &Keys) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:45000/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(NoAgent),
        credentials: Box::new(keys.clone()),
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(7)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    )
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut client = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = call(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": "ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    loop {
        let frame: Value =
            serde_json::from_str(&client.inbox.try_recv().expect("a reply")).unwrap();
        if frame["id"] == id {
            return frame;
        }
        client.queued.push_back(frame);
    }
}

fn ok(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    let reply = call(engine, client, method, params);
    assert!(reply.get("result").is_some(), "{method}: {reply}");
    reply["result"].clone()
}

fn refused(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> String {
    let reply = call(engine, client, method, params);
    reply["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{method} was accepted: {reply}"))
        .to_owned()
}

fn heard(engine: &mut Engine) {
    observed(engine, false);
}

fn observed(engine: &mut Engine, armed: bool) {
    engine.vehicle_observed(Observation {
        connections: None,
        id: "v".into(),
        aliases: vec![],
        link_id: "l".into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed,
        mode: None,
    });
}

const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";

#[test]
fn computers_are_added_with_free_ids_and_their_keys_are_kept_apart() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-vision.local", "user": "pi" }),
    );
    let first = &vehicle["companions"][0];
    assert_eq!(first["name"], "pi-vision.local");
    assert_eq!(first["componentId"], 191);
    assert_eq!(first["role"], "report");
    assert!(first.get("key").is_none());

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-fc.local", "port": 2222, "role": "forward", "name": "Flight Pi", "privateKey": KEY }),
    );
    let second = vehicle["companions"][1].clone();
    assert_eq!(second["componentId"], 192);
    assert_eq!(second["key"], true);
    assert!(
        !vehicle.to_string().contains("OPENSSH PRIVATE KEY"),
        "the key is never sent"
    );
    let key_name = format!("companion-key-{}", second["id"].as_str().unwrap());
    assert!(keys.has(&key_name));

    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "PI-VISION.local" })
        )
        .contains("already")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "-oProxyCommand=sh" })
        )
        .contains("not a host name")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi-2.local", "role": "forward" })
        )
        .contains("already has a computer forwarding")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi-3.local", "privateKey": "hunter2" })
        )
        .contains("not a private key")
    );

    // The profile survives a restart.
    let restarted = engine.state().clone();
    let again = self::engine(
        serde_json::from_value(serde_json::to_value(&restarted).unwrap()).unwrap(),
        &keys,
    );
    assert_eq!(
        again.state().vehicles["v"].companions,
        restarted.vehicles["v"].companions
    );

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/remove",
        json!({ "vehicleId": "v", "computerId": second["id"] }),
    );
    assert_eq!(vehicle["companions"].as_array().unwrap().len(), 1);
    assert!(!keys.has(&key_name), "its key is forgotten with it");
}

/// A vehicle has four companion computers at most, and the number is
/// MAVLink's: it keeps four ids for onboard computers (191 to 194), and
/// each computer reports as one of them (ADR-0049 O3). The refusal says so.
#[test]
fn a_fifth_computer_is_refused_and_told_why() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let mut second = Value::Null;
    for (n, id) in (191..=194).enumerate() {
        let vehicle = ok(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": format!("pi-{}.local", n + 1) }),
        );
        assert_eq!(vehicle["companions"][n]["componentId"], id);
        if id == 192 {
            second = vehicle["companions"][n]["id"].clone();
        }
    }
    let name = engine.state().vehicles["v"].name.clone();
    assert_eq!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi-5.local" })
        ),
        format!(
            "{name} has four companion computers, and MAVLink has four ids for onboard computers (191 to 194), one for each. Remove one to add another."
        )
    );
    assert_eq!(engine.state().vehicles["v"].companions.len(), 4);

    // One removed, the next computer takes the id it leaves.
    ok(
        &mut engine,
        &mut ui,
        "computer/remove",
        json!({ "vehicleId": "v", "computerId": second }),
    );
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-5.local" }),
    );
    assert_eq!(vehicle["companions"][3]["host"], "pi-5.local");
    assert_eq!(vehicle["companions"][3]["componentId"], 192);
}

#[test]
fn a_computer_is_renamed_and_keeps_its_address() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "192.168.50.20", "user": "pi" }),
    );
    let id = vehicle["companions"][0]["id"].clone();

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/rename",
        json!({ "vehicleId": "v", "computerId": id, "name": "  Camera Pi " }),
    );
    let computer = &vehicle["companions"][0];
    assert_eq!(computer["name"], "Camera Pi");
    assert_eq!(computer["host"], "192.168.50.20");
    assert_eq!(computer["user"], "pi");
    assert_eq!(
        engine.state().vehicles["v"].companions[0].name,
        "Camera Pi",
        "the name is kept"
    );

    assert_eq!(
        refused(
            &mut engine,
            &mut ui,
            "computer/rename",
            json!({ "vehicleId": "v", "computerId": id, "name": "   " })
        ),
        "Give the computer a name."
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/rename",
            json!({ "vehicleId": "v", "computerId": "cc000000000000", "name": "Pi" })
        )
        .contains("not found")
    );
    assert_eq!(engine.state().vehicles["v"].companions[0].name, "Camera Pi");
}

/// Buggy (2026-11-15): the gateway was added first and given 191, but the
/// camera Pi's own `uxduck companion` was pinned to 191 when it was set up
/// by hand, so the camera Pi's clock showed on the gateway's card.
#[test]
fn a_computer_takes_the_id_its_uxduck_sends_as() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let add = |engine: &mut Engine, ui: &mut Client, host: &str| {
        let vehicle = ok(
            engine,
            ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": host }),
        );
        let added = vehicle["companions"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        added["id"].as_str().unwrap().to_owned()
    };
    let gateway = add(&mut engine, &mut ui, "192.168.50.21");
    let camera = add(&mut engine, &mut ui, "192.168.50.20");
    let ids = |engine: &Engine| {
        engine.state().vehicles["v"]
            .companions
            .iter()
            .map(|c| (c.host.clone(), c.component_id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&engine),
        [("192.168.50.21".into(), 191), ("192.168.50.20".into(), 192)]
    );
    let found = |reports_as| Found {
        os: Some("Debian GNU/Linux 13 (trixie)".into()),
        arch: Some("aarch64".into()),
        uxduck: Some("0.16.0".into()),
        host_key: Some("SHA256:pinned".into()),
        clock: None,
        reports_as,
        network: None,
        cameras: None,
        software: None,
    };
    let check = |engine: &mut Engine, ui: &mut Client, computer: &str, reports_as| {
        ok(
            engine,
            ui,
            "computer/check",
            json!({ "vehicleId": "v", "computerId": computer }),
        );
        engine.companion_checked(CheckAnswer {
            vehicle_id: "v".into(),
            computer_id: computer.to_owned(),
            result: Ok(found(reports_as)),
        });
    };

    check(&mut engine, &mut ui, &gateway, None);
    assert_eq!(
        ids(&engine)[0].1,
        191,
        "no UXDuck reporting on it: its id stays"
    );
    check(&mut engine, &mut ui, &camera, Some(191));
    assert_eq!(
        ids(&engine),
        [("192.168.50.21".into(), 192), ("192.168.50.20".into(), 191)],
        "the camera Pi takes the id it sends as, and the gateway moves aside"
    );
    let saved = &engine.state().vehicles["v"].companions[1];
    assert_eq!(saved.check.as_ref().unwrap().reports_as, Some(191));

    ok(
        &mut engine,
        &mut ui,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": camera }),
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: camera.clone(),
        result: Err("ssh: connect to host 192.168.50.20 port 22: No route to host".into()),
    });
    assert_eq!(
        ids(&engine)[1].1,
        191,
        "a failed check keeps what it learned"
    );

    check(&mut engine, &mut ui, &gateway, Some(191));
    assert_eq!(
        ids(&engine),
        [("192.168.50.21".into(), 191), ("192.168.50.20".into(), 191)],
        "both send as 191, so both keep it and their cards say so"
    );
}

#[test]
#[allow(clippy::too_many_lines, reason = "one check's whole story")]
fn a_check_reports_what_the_computer_runs_and_keeps_its_pinned_key() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-vision.local", "user": "pi" }),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let params = json!({ "vehicleId": "v", "computerId": id });

    // Without SSH (a browser), a check is refused with where to do it.
    assert!(
        refused(&mut engine, &mut ui, "computer/check", params.clone()).contains("UXDuck Desktop")
    );

    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(&mut engine, &mut ui, "computer/check", params.clone());
    assert_eq!(vehicle["companions"][0]["check"]["state"], "checking");
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { vehicle_id, computer, password: None, .. }] if vehicle_id == "v" && computer.host == "pi-vision.local")
    );

    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            network: None,
            cameras: None,
            software: None,
            os: Some("Debian GNU/Linux 12 (bookworm)".into()),
            arch: Some("aarch64".into()),
            uxduck: None,
            host_key: Some("SHA256:first".into()),
            clock: Some(CompanionClock {
                offset_ms: -712_800_000,
                within_ms: 240,
                synchronised: Some(false),
                ntp: Some(true),
                service: Some("systemd-timesyncd".into()),
            }),
            reports_as: None,
        }),
    });
    let check = engine.state().vehicles["v"].companions[0]
        .check
        .clone()
        .unwrap();
    assert_eq!(check.os.as_deref(), Some("Debian GNU/Linux 12 (bookworm)"));
    assert_eq!(check.uxduck, None);
    assert_eq!(check.host_key.as_deref(), Some("SHA256:first"));
    assert_eq!(
        check.clock.as_ref().map(|c| c.offset_ms),
        Some(-712_800_000)
    );
    let wire = serde_json::to_value(uxduck_contracts::dto::vehicle::Vehicle::from(
        &engine.state().vehicles["v"],
    ))
    .unwrap();
    assert_eq!(
        wire["companions"][0]["check"]["clock"],
        json!({
            "offsetMs": -712_800_000,
            "withinMs": 240,
            "synchronised": false,
            "ntp": true,
            "service": "systemd-timesyncd",
        })
    );

    // A password goes to the host for this check alone, never onto the vehicle.
    let with_password = json!({ "vehicleId": "v", "computerId": id, "password": "raspberry" });
    let vehicle = ok(&mut engine, &mut ui, "computer/check", with_password);
    assert!(!vehicle.to_string().contains("raspberry"));
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { password: Some(p), .. }] if p.0 == "raspberry" && !format!("{p:?}").contains("raspberry"))
    );
    assert!(!format!("{:?}", engine.state()).contains("raspberry"));

    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Err("check completed".into()),
    });
    // A refused change keeps the key it pinned, so the next check refuses too.
    ok(&mut engine, &mut ui, "computer/check", params.clone());
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { computer, .. }] if computer.check.as_ref().and_then(|c| c.host_key.as_deref()) == Some("SHA256:first"))
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Err("pi-vision.local's host key changed".into()),
    });
    let check = engine.state().vehicles["v"].companions[0]
        .check
        .clone()
        .unwrap();
    assert!(check.problem.unwrap().contains("changed"));
    assert_eq!(check.host_key.as_deref(), Some("SHA256:first"));

    ok(&mut engine, &mut ui, "computer/remove", params);
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert_eq!(orders, [CompanionOrder::Forget { computer_id: id }]);
}

#[test]
fn network_changes_require_a_pin_and_disarm_and_never_persist_passwords() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let request = json!({"vehicleId":"v","computerId":id,"wifi":{"ssid":"Field hotspot","security":"wpa-psk","password":"keep-this-secret","priority":10,"hidden":false,"metered":true}});
    assert!(
        refused(&mut engine, &mut ui, "computer/network", request.clone())
            .contains("Check the computer first")
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            ..Found::default()
        }),
    });
    let result = ok(&mut engine, &mut ui, "computer/network", request.clone());
    assert_eq!(result["companions"][0]["check"]["state"], "checking");
    assert!(!result.to_string().contains("keep-this-secret"));
    assert!(!format!("{:?}", engine.state()).contains("keep-this-secret"));
    let restarted = self::engine(engine.state().clone(), &keys);
    assert_eq!(
        restarted.state().vehicles["v"].companions[0]
            .check
            .as_ref()
            .unwrap()
            .state,
        uxduck_core::domain::companion::CheckState::Failed
    );
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Network {input,..}] if input.0.contains("keep-this-secret"))
    );
    assert!(!format!("{orders:?}").contains("keep-this-secret"));
    assert!(
        refused(&mut engine, &mut ui, "computer/network", request.clone())
            .contains("already running")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/check",
            json!({"vehicleId":"v","computerId":id})
        )
        .contains("already running")
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Err("permission denied".into()),
    });
    assert_eq!(
        engine.state().vehicles["v"].companions[0]
            .check
            .as_ref()
            .unwrap()
            .host_key
            .as_deref(),
        Some("SHA256:known")
    );
    let mut state = engine.state().clone();
    state.vehicles.get_mut("v").unwrap().armed = true;
    let mut armed = self::engine(state, &keys);
    armed.set_companions(Box::new(ssh));
    let mut ui = connect(&mut armed);
    assert!(refused(&mut armed, &mut ui, "computer/network", request).contains("Disarm"));
    assert!(refused(&mut armed, &mut ui, "computer/network", json!({"vehicleId":"v","computerId":id,"connectivity":{"uri":"https://example.test/204","interval":60}})).contains("Disarm"));
    // Read-only status remains available while armed.
    ok(
        &mut armed,
        &mut ui,
        "computer/network",
        json!({"vehicleId":"v","computerId":id}),
    );
}

fn cameras(linger_at_risk: bool) -> CompanionCameras {
    CompanionCameras {
        microphones: Vec::new(),
        sound_problem: None,
        unit: Some("mediamtx.service".into()),
        config: Some("/etc/mediamtx/mediamtx.yml".into()),
        api: Some("127.0.0.1:9997".into()),
        drop_in: Some("/etc/systemd/system/mediamtx.service.d/60-uxduck-camera.conf".into()),
        linger: Some(CameraLinger {
            user: "pi".into(),
            lingering: !linger_at_risk,
            remove_ipc: true,
        }),
        paths: vec![
            CompanionCameraPath {
                microphone: None,
                name: "cam".into(),
                source: "rpiCamera".into(),
                camera_id: Some(0),
                sensor: Some("imx708".into()),
                settings: Some(PicturePreset::Detail.settings()),
                set_by_uxduck: false,
                on_demand: false,
                ready: true,
                readers: 1,
                bytes: 4_000_000,
                record: Some(VehicleRecording {
                    path: "cam_record".into(),
                    mode: VehicleRecordMode::Armed,
                    settings: None,
                }),
            },
            CompanionCameraPath {
                microphone: None,
                name: "front_cam".into(),
                source: "rpiCamera".into(),
                camera_id: Some(1),
                sensor: None,
                settings: None,
                set_by_uxduck: false,
                on_demand: false,
                ready: true,
                readers: 0,
                bytes: 0,
                record: None,
            },
            CompanionCameraPath {
                microphone: None,
                name: "relay".into(),
                source: "rtsp".into(),
                camera_id: None,
                sensor: None,
                settings: None,
                set_by_uxduck: false,
                on_demand: true,
                ready: false,
                readers: 0,
                bytes: 0,
                record: None,
            },
        ],
        watchdog: CameraWatchdog::default(),
        applied: None,
        problem: None,
        record_problem: None,
    }
}

/// A camera's microphone (ADR-0067 M3, M14) is chosen like its record
/// mode: checked against the last read, written over the pinned SSH one
/// change at a time, and refused while armed.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario from the first read to the armed refusal"
)]
fn a_cameras_microphone_is_checked_against_the_last_read_and_refused_while_armed() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let mut read = cameras(false);
    read.microphones = vec![CompanionMicrophoneDevice {
        id: "usb:3-1.2".into(),
        name: "USB PnP Sound Device".into(),
        card: "Device".into(),
        usb: true,
    }];
    read.paths[0].microphone = Some(CompanionMicrophone {
        choice: "auto".into(),
        device: Some("usb:3-1.2".into()),
    });
    let answer = |engine: &mut Engine| {
        engine.companion_checked(CheckAnswer {
            vehicle_id: "v".into(),
            computer_id: id.clone(),
            result: Ok(Found {
                host_key: Some("SHA256:known".into()),
                cameras: Some(read.clone()),
                ..Found::default()
            }),
        });
    };
    answer(&mut engine);
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/check",
        json!({"vehicleId":"v","computerId":id}),
    );
    assert_eq!(
        vehicle["companions"][0]["check"]["cameras"]["paths"][0]["microphone"],
        json!({"choice": "auto", "device": "usb:3-1.2"})
    );
    assert_eq!(
        vehicle["companions"][0]["check"]["cameras"]["microphones"],
        json!([{"id": "usb:3-1.2", "name": "USB PnP Sound Device", "card": "Device", "usb": true}])
    );
    answer(&mut engine);
    ssh.0.lock().unwrap().clear();
    let choose = |path: &str, choice: &str| json!({"vehicleId":"v","computerId":id,"microphone":{"path":path,"choice":choice}});
    for (params, why) in [
        (choose("front_cam", "auto"), "does not send front_cam"),
        (
            choose("cam", "Blue Yeti"),
            "Blue Yeti is not a microphone on pi.local",
        ),
        (choose("cam", "usb:"), "is not a microphone"),
        (choose("cam", "usb:3"), "is not a microphone"),
        (choose("cam", "usb:3-1."), "is not a microphone"),
        (choose("cam", "card:"), "is not a microphone"),
        (choose("cam", "card:Dev\"ice"), "is not a microphone"),
        (
            json!({"vehicleId":"v","computerId":id,"record":{"path":"cam","mode":"always"},"microphone":{"path":"cam","choice":"none"}}),
            "one camera change",
        ),
    ] {
        let error = refused(&mut engine, &mut ui, "computer/cameras", params);
        assert!(error.contains(why), "{error}");
        assert!(!error.contains(';'), "{error}");
    }
    assert!(ssh.0.lock().unwrap().is_empty());

    for choice in ["usb:3-1.2", "none", "auto", "card:Snowball", "usb:1-2.4"] {
        ok(
            &mut engine,
            &mut ui,
            "computer/cameras",
            choose("cam", choice),
        );
        let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
        let [CompanionOrder::Cameras { input, .. }] = &orders[..] else {
            panic!("{orders:?}");
        };
        let input: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            input["microphone"],
            json!({"path": "cam", "choice": choice})
        );
        assert!(input["record"].is_null() && input["apply"].is_null());
        assert!(
            refused(
                &mut engine,
                &mut ui,
                "computer/cameras",
                choose("cam", "none")
            )
            .contains("already running"),
            "the same operation lock as every camera change"
        );
        answer(&mut engine);
    }

    let mut state = engine.state().clone();
    state.vehicles.get_mut("v").unwrap().armed = true;
    let mut armed = self::engine(state, &keys);
    armed.set_companions(Box::new(ssh.clone()));
    let mut ui = connect(&mut armed);
    assert!(
        refused(
            &mut armed,
            &mut ui,
            "computer/cameras",
            choose("cam", "none")
        )
        .contains("Disarm the vehicle before changing its cameras.")
    );
    assert!(ssh.0.lock().unwrap().is_empty());
}

fn picture(path: &str, settings: Value) -> Value {
    json!({"path": path, "settings": settings})
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario from the first read to the armed refusal"
)]
fn camera_changes_are_checked_serialized_with_networks_and_refused_while_armed() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let read = json!({"vehicleId":"v","computerId":id});
    assert!(
        refused(&mut engine, &mut ui, "computer/cameras", read.clone())
            .contains("Check the computer first")
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            ..Found::default()
        }),
    });
    let detail = serde_json::to_value(PicturePreset::Detail.settings()).unwrap();
    let low = serde_json::to_value(PicturePreset::LowLatency.settings()).unwrap();
    let apply = |path: &str, settings: &Value, despite: bool| {
        let mut picture = picture(path, settings.clone());
        if despite {
            picture["despiteLinger"] = true.into();
        }
        json!({"vehicleId":"v","computerId":id,"apply":picture})
    };
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/cameras",
            apply("cam", &low, false)
        )
        .contains("Read the cameras on")
    );

    let result = ok(&mut engine, &mut ui, "computer/cameras", read.clone());
    assert_eq!(result["companions"][0]["check"]["state"], "checking");
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(matches!(&orders[..], [CompanionOrder::Cameras { input, .. }] if input == "{}"));
    let wifi = json!({"vehicleId":"v","computerId":id,"connectivity":{"uri":"https://example.test/204","interval":60}});
    for (method, params) in [
        ("computer/cameras", read.clone()),
        ("computer/network", wifi),
        ("computer/check", read.clone()),
    ] {
        assert!(refused(&mut engine, &mut ui, method, params).contains("already running"));
    }
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            cameras: Some(cameras(true)),
            ..Found::default()
        }),
    });
    let vehicle = ok(&mut engine, &mut ui, "computer/check", read.clone());
    assert_eq!(
        vehicle["companions"][0]["check"]["cameras"]["paths"][0]["preset"],
        "detail"
    );
    ssh.0.lock().unwrap().clear();
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            ..Found::default()
        }),
    });
    assert!(
        engine.state().vehicles["v"].companions[0]
            .check
            .as_ref()
            .unwrap()
            .cameras
            .is_some(),
        "a check keeps the last cameras read"
    );

    let mut fast = low.clone();
    fast["fps"] = 200.into();
    for (params, why) in [
        (apply("nowhere", &low, true), "has not seen nowhere"),
        (apply("relay", &low, true), "Only a Raspberry Pi camera"),
        (apply("front_cam", &low, true), "without _"),
        (apply("cam", &fast, true), "120"),
        (apply("cam", &low, false), "lingering"),
        (
            json!({"vehicleId":"v","computerId":id,"linger":true,"watchdog":"install"}),
            "one camera change",
        ),
        (
            json!({"vehicleId":"v","computerId":id,"record":{"path":"relay","mode":"always"}}),
            "does not record relay on the vehicle",
        ),
    ] {
        let error = refused(&mut engine, &mut ui, "computer/cameras", params);
        assert!(error.contains(why), "{error}");
    }
    assert!(ssh.0.lock().unwrap().is_empty());

    ok(
        &mut engine,
        &mut ui,
        "computer/cameras",
        apply("cam", &low, true),
    );
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    let [CompanionOrder::Cameras { input, .. }] = &orders[..] else {
        panic!("{orders:?}");
    };
    let input: Value = serde_json::from_str(input).unwrap();
    assert_eq!(input["apply"]["path"], "cam");
    assert_eq!(input["apply"]["settings"], low);
    assert_eq!(input["linger"], false);
    assert!(input["record"].is_null());

    // When a camera records on the vehicle (ADR-0066 R9), written over SSH.
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            cameras: Some(cameras(true)),
            ..Found::default()
        }),
    });
    let always = json!({"vehicleId":"v","computerId":id,"record":{"path":"cam","mode":"always"}});
    let vehicle = ok(&mut engine, &mut ui, "computer/check", read.clone());
    assert_eq!(
        vehicle["companions"][0]["check"]["cameras"]["paths"][0]["record"],
        json!({"path": "cam_record", "mode": "armed"})
    );
    ssh.0.lock().unwrap().clear();
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            ..Found::default()
        }),
    });
    ok(&mut engine, &mut ui, "computer/cameras", always.clone());
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    let [CompanionOrder::Cameras { input, .. }] = &orders[..] else {
        panic!("{orders:?}");
    };
    let input: Value = serde_json::from_str(input).unwrap();
    assert_eq!(input["record"], json!({"path": "cam", "mode": "always"}));
    assert!(input["apply"].is_null());

    let restarted = self::engine(engine.state().clone(), &keys);
    let check = restarted.state().vehicles["v"].companions[0]
        .check
        .clone()
        .unwrap();
    assert_eq!(
        check.state,
        uxduck_core::domain::companion::CheckState::Failed
    );
    assert!(check.problem.unwrap().contains("cameras again"));
    assert!(check.cameras.is_some());

    let mut state = restarted.state().clone();
    state.vehicles.get_mut("v").unwrap().armed = true;
    let mut armed = self::engine(state, &keys);
    armed.set_companions(Box::new(ssh.clone()));
    let mut ui = connect(&mut armed);
    for params in [
        apply("cam", &detail, true),
        json!({"vehicleId":"v","computerId":id,"linger":true}),
        json!({"vehicleId":"v","computerId":id,"watchdog":"install"}),
        json!({"vehicleId":"v","computerId":id,"watchdog":"remove"}),
        always,
    ] {
        assert!(refused(&mut armed, &mut ui, "computer/cameras", params).contains("Disarm"));
    }
    ok(&mut armed, &mut ui, "computer/cameras", read);
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(matches!(&orders[..], [CompanionOrder::Cameras { input, .. }] if input == "{}"));
}

/// A person's change that arrives during a background network read waits for
/// it and runs when it answers, and a restart marks it interrupted while it
/// waits. Another person's change, running or waiting, still refuses one. A
/// waiting change is dropped if the vehicle arms first, and a read still goes.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one computer through waiting, running, refusing and arming"
)]
fn a_change_waits_for_a_background_read_and_refuses_while_another_change_runs() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let answer = |cameras: Option<CompanionCameras>| CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            cameras,
            ..Found::default()
        }),
    };
    engine.companion_checked(answer(None));
    let sent = |ssh: &Ssh| std::mem::take(&mut *ssh.0.lock().unwrap());
    let state = |engine: &Engine| {
        engine.state().vehicles["v"].companions[0]
            .check
            .clone()
            .unwrap()
    };
    let background = json!({"vehicleId":"v","computerId":id,"background":true});
    let watchdog = json!({"vehicleId":"v","computerId":id,"watchdog":"install"});
    let wifi = json!({"vehicleId":"v","computerId":id,"connectivity":{"uri":"https://example.test/204","interval":60}});

    ok(&mut engine, &mut ui, "computer/network", background.clone());
    assert!(matches!(&sent(&ssh)[..], [CompanionOrder::Network { .. }]));
    let waiting = ok(&mut engine, &mut ui, "computer/cameras", watchdog.clone());
    assert_eq!(waiting["companions"][0]["check"]["state"], "checking");
    assert!(sent(&ssh).is_empty(), "the change waits for the read");
    for (method, params) in [
        ("computer/network", wifi.clone()),
        ("computer/cameras", watchdog.clone()),
        ("computer/check", json!({"vehicleId":"v","computerId":id})),
    ] {
        let error = refused(&mut engine, &mut ui, method, params);
        assert!(error.contains("already running"), "{method}: {error}");
    }

    let restarted = self::engine(engine.state().clone(), &keys);
    let interrupted = state(&restarted);
    assert_eq!(
        interrupted.state,
        uxduck_core::domain::companion::CheckState::Failed
    );
    assert!(interrupted.problem.unwrap().contains("UXDuck restarted"));

    engine.companion_checked(answer(None));
    let [CompanionOrder::Cameras { input, .. }] = &sent(&ssh)[..] else {
        panic!("the waiting change was not sent");
    };
    assert_eq!(
        serde_json::from_str::<Value>(input).unwrap()["watchdog"],
        "install"
    );
    assert_eq!(
        state(&engine).state,
        uxduck_core::domain::companion::CheckState::Checking
    );
    for (method, params) in [
        ("computer/network", wifi.clone()),
        ("computer/network", background.clone()),
        ("computer/cameras", watchdog.clone()),
    ] {
        let error = refused(&mut engine, &mut ui, method, params);
        assert!(error.contains("already running"), "{method}: {error}");
    }
    assert!(sent(&ssh).is_empty());

    engine.companion_checked(answer(Some(cameras(false))));
    assert_eq!(
        state(&engine).state,
        uxduck_core::domain::companion::CheckState::Reached
    );
    ok(&mut engine, &mut ui, "computer/network", wifi);
    assert!(matches!(&sent(&ssh)[..], [CompanionOrder::Network { .. }]));
    engine.companion_checked(answer(None));

    ok(&mut engine, &mut ui, "computer/network", background.clone());
    sent(&ssh);
    ok(&mut engine, &mut ui, "computer/cameras", watchdog);
    observed(&mut engine, true);
    assert!(engine.state().vehicles["v"].armed);
    engine.companion_checked(answer(None));
    assert!(
        sent(&ssh).is_empty(),
        "nothing changes once the vehicle armed"
    );
    let stopped = state(&engine);
    assert_eq!(
        stopped.state,
        uxduck_core::domain::companion::CheckState::Failed
    );
    assert!(
        stopped
            .problem
            .unwrap()
            .contains("armed before the change started, so nothing changed")
    );

    ok(&mut engine, &mut ui, "computer/network", background);
    sent(&ssh);
    let read = json!({"vehicleId":"v","computerId":id});
    ok(&mut engine, &mut ui, "computer/cameras", read);
    engine.companion_checked(answer(None));
    assert!(
        matches!(&sent(&ssh)[..], [CompanionOrder::Cameras { input, .. }] if input == "{}"),
        "a waiting read still goes while armed"
    );
}

/// The text of an OpenSSH private key whose header names \`cipher\`, which
/// is all UXDuck reads to tell whether it has a passphrase (FB-268).
fn openssh_text(cipher: &str) -> String {
    use base64::Engine as _;
    let field = |bytes: &[u8]| {
        [
            &u32::try_from(bytes.len()).unwrap().to_be_bytes()[..],
            bytes,
        ]
        .concat()
    };
    let public = [field(b"ssh-ed25519"), field(&[7; 32])].concat();
    let kdf: &[u8] = if cipher == "none" { b"none" } else { b"bcrypt" };
    let body = [
        b"openssh-key-v1\0".to_vec(),
        field(cipher.as_bytes()),
        field(kdf),
        field(b""),
        1u32.to_be_bytes().to_vec(),
        field(&public),
        field(&[0; 64]),
    ]
    .concat();
    format!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(body)
    )
}

/// FB-268: a key a check cannot use is refused when it is added, with what
/// to do: one with a passphrase, which a check has no one to type, and a
/// `PuTTY` key, which ssh cannot read.
#[test]
fn keys_a_check_cannot_use_are_refused_with_what_to_do() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let add = |engine: &mut Engine, ui: &mut Client, key: &str| {
        refused(
            engine,
            ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi.local", "privateKey": key }),
        )
    };
    for locked in [
        openssh_text("aes256-ctr"),
        "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nAAAA\n-----END RSA PRIVATE KEY-----".to_owned(),
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----".to_owned(),
    ] {
        let why = add(&mut engine, &mut ui, &locked);
        assert!(why.contains("has a passphrase"), "{why}");
        assert!(why.contains("ssh-agent"), "{why}");
        assert!(!why.contains(';'), "{why}");
    }
    let why = add(
        &mut engine,
        &mut ui,
        "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\n",
    );
    assert!(why.contains("PuTTYgen"), "{why}");
    assert!(keys.0.lock().unwrap().is_empty(), "nothing was kept");

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi.local", "privateKey": openssh_text("none") }),
    );
    assert_eq!(vehicle["companions"][0]["key"], true);
}

/// FB-268: a failed check keeps what kind of problem it was and what ssh
/// said, and the next check clears both.
#[test]
fn a_failed_check_keeps_its_kind_and_what_ssh_said() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    engine.set_companions(Box::new(Ssh::default()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi.local", "user": "uxduck" }),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Err(uxduck_engine::companions::CheckFailure {
            problem: "pi.local refused the password for uxduck.".into(),
            kind: uxduck_core::domain::companion::CheckProblem::Password,
            detail: Some("uxduck@pi.local: Permission denied (publickey,password).".into()),
        }),
    });
    let wire = serde_json::to_value(uxduck_contracts::dto::companion::CompanionComputer::from(
        &engine.state().vehicles["v"].companions[0],
    ))
    .unwrap();
    assert_eq!(wire["check"]["state"], "failed");
    assert_eq!(wire["check"]["problemKind"], "password");
    assert_eq!(
        wire["check"]["detail"],
        "uxduck@pi.local: Permission denied (publickey,password)."
    );
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": id }),
    );
    let check = &vehicle["companions"][0]["check"];
    assert_eq!(check["state"], "checking");
    assert!(check.get("problemKind").is_none());
    assert!(check.get("detail").is_none());
}

/// Buggy (2026-11-16): with the ground radio unplugged, the gateway answers
/// only at its Tailscale address and the camera Pi only through the
/// gateway. Both are kept with the computers, and only a route SSH can
/// follow is taken.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one vehicle's routes from the first through every refusal to a restart"
)]
fn a_computer_keeps_its_other_addresses_and_the_computer_it_goes_through() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let mut add = |engine: &mut Engine, host: &str| {
        let vehicle = ok(
            engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": host, "user": "pi" }),
        );
        let computers = vehicle["companions"].as_array().unwrap();
        computers[computers.len() - 1]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let gateway = add(&mut engine, "192.168.50.21");
    let camera = add(&mut engine, "192.168.50.20");
    let third = add(&mut engine, "pi-3.local");
    let mut ui = connect(&mut engine);
    let route = |computer: &str, other_hosts: Value, through: Value| json!({ "vehicleId": "v", "computerId": computer, "otherHosts": other_hosts, "through": through });

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/route",
        route(&gateway, json!([" 100.76.177.45 ", ""]), Value::Null),
    );
    assert_eq!(
        vehicle["companions"][0]["otherHosts"],
        json!(["100.76.177.45"])
    );
    assert_eq!(vehicle["companions"][0]["host"], "192.168.50.21");
    assert!(vehicle["companions"][0].get("through").is_none());

    for (other_hosts, said) in [
        (json!(["192.168.50.21"]), "192.168.50.21 is listed twice."),
        (
            json!(["gateway.local", "Gateway.local"]),
            "Gateway.local is listed twice.",
        ),
    ] {
        assert_eq!(
            refused(
                &mut engine,
                &mut ui,
                "computer/route",
                route(&gateway, other_hosts, Value::Null)
            ),
            said
        );
    }
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route(&gateway, json!(["-oProxyCommand=x"]), Value::Null)
        )
        .contains("is not a host name")
    );
    assert_eq!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route(&camera, json!([]), json!(camera))
        ),
        "A computer cannot be reached through itself."
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route(&camera, json!([]), json!("cc000000000000"))
        )
        .starts_with("That computer is not one of ")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route("cc000000000000", json!([]), Value::Null)
        )
        .contains("not found")
    );

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/route",
        route(&camera, json!([]), json!(gateway)),
    );
    assert_eq!(vehicle["companions"][1]["through"], json!(gateway));
    assert_eq!(
        vehicle["companions"][0]["otherHosts"],
        json!(["100.76.177.45"]),
        "another computer's route is left as it was"
    );

    let name = |at: usize| engine_name(&vehicle, at);
    assert_eq!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route(&third, json!([]), json!(camera))
        ),
        format!(
            "{} is itself reached through another computer. Choose one UXDuck reaches at its own addresses.",
            name(1)
        )
    );
    assert_eq!(
        refused(
            &mut engine,
            &mut ui,
            "computer/route",
            route(&gateway, json!(["100.76.177.45"]), json!(third))
        ),
        format!(
            "Another computer is reached through {}, so it cannot go through one itself.",
            name(0)
        )
    );
    let kept = &engine.state().vehicles["v"].companions;
    assert_eq!(kept[0].other_hosts, ["100.76.177.45"]);
    assert_eq!(kept[0].through, None, "a refused route keeps what was kept");
    assert_eq!(kept[1].through.as_deref(), Some(gateway.as_str()));

    let restarted = self::engine(engine.state().clone(), &keys);
    let kept = &restarted.state().vehicles["v"].companions;
    assert_eq!(kept[0].other_hosts, ["100.76.177.45"]);
    assert_eq!(kept[1].through.as_deref(), Some(gateway.as_str()));

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/route",
        route(&camera, json!([]), json!("")),
    );
    assert!(
        vehicle["companions"][1].get("through").is_none(),
        "an empty choice is None"
    );
}

fn engine_name(vehicle: &Value, at: usize) -> String {
    vehicle["companions"][at]["name"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn removing_a_computer_others_go_through_leaves_them_direct() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let gateway = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "192.168.50.21" }),
    )["companions"][0]["id"]
        .clone();
    let camera = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "192.168.50.20" }),
    )["companions"][1]["id"]
        .clone();
    ok(
        &mut engine,
        &mut ui,
        "computer/route",
        json!({ "vehicleId": "v", "computerId": camera, "otherHosts": ["cam.local"], "through": gateway }),
    );
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/remove",
        json!({ "vehicleId": "v", "computerId": gateway }),
    );
    assert_eq!(vehicle["companions"][0]["id"], camera);
    assert!(vehicle["companions"][0].get("through").is_none());
    assert_eq!(vehicle["companions"][0]["otherHosts"], json!(["cam.local"]));
    assert_eq!(engine.state().vehicles["v"].companions[0].through, None);
}

/// SSH needs the computer a check or change goes through, with its pinned
/// host key, so every order carries it.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one computer's check, camera and network orders through the same hop"
)]
fn checks_and_changes_carry_the_computer_they_go_through() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let mut ids = Vec::new();
    for host in ["192.168.50.21", "192.168.50.20"] {
        let vehicle = ok(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": host }),
        );
        let id = vehicle["companions"][ids.len()]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        engine.companion_checked(CheckAnswer {
            vehicle_id: "v".into(),
            computer_id: id.clone(),
            result: Ok(Found {
                host_key: Some(format!("SHA256:{host}")),
                ..Found::default()
            }),
        });
        ids.push(id);
    }
    let (gateway, camera) = (ids[0].clone(), ids[1].clone());
    ok(
        &mut engine,
        &mut ui,
        "computer/route",
        json!({ "vehicleId": "v", "computerId": gateway, "otherHosts": ["100.76.177.45"] }),
    );
    ok(
        &mut engine,
        &mut ui,
        "computer/route",
        json!({ "vehicleId": "v", "computerId": camera, "through": gateway }),
    );
    ssh.0.lock().unwrap().clear();
    let hop = |order: &CompanionOrder| match order {
        CompanionOrder::Check {
            computer, through, ..
        }
        | CompanionOrder::Network {
            computer, through, ..
        }
        | CompanionOrder::Cameras {
            computer, through, ..
        }
        | CompanionOrder::Install {
            computer, through, ..
        } => (
            computer.id.clone(),
            through.as_ref().map(|t| {
                (
                    t.id.clone(),
                    t.other_hosts.clone(),
                    t.check.as_ref().and_then(|c| c.host_key.clone()),
                )
            }),
        ),
        CompanionOrder::Forget { .. } => panic!("not a check"),
    };
    let read = json!({ "vehicleId": "v", "computerId": camera });
    for (method, params) in [
        ("computer/check", read.clone()),
        ("computer/cameras", read.clone()),
        ("computer/network", read.clone()),
    ] {
        ok(&mut engine, &mut ui, method, params);
        let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
        assert_eq!(orders.len(), 1, "{method}");
        assert_eq!(
            hop(&orders[0]),
            (
                camera.clone(),
                Some((
                    gateway.clone(),
                    vec!["100.76.177.45".to_owned()],
                    Some("SHA256:192.168.50.21".to_owned())
                ))
            ),
            "{method}"
        );
        engine.companion_checked(CheckAnswer {
            vehicle_id: "v".into(),
            computer_id: camera.clone(),
            result: Ok(Found {
                host_key: Some("SHA256:192.168.50.20".into()),
                ..Found::default()
            }),
        });
    }
    ok(
        &mut engine,
        &mut ui,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": gateway }),
    );
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert_eq!(hop(&orders[0]), (gateway, None));
}

fn older_computer(engine: &mut Engine, id: &str, version: &str, uses: &[&str]) {
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.into(),
        result: Ok(Found {
            host_key: Some("SHA256:known".into()),
            arch: Some("aarch64".into()),
            uxduck: Some(version.into()),
            software: Some(SoftwareFound {
                systemd: true,
                sudo: Some(true),
                bits: Some(64),
                config: !uses.is_empty(),
                uses: uses.iter().map(|u| (*u).to_owned()).collect(),
                ..SoftwareFound::default()
            }),
            ..Found::default()
        }),
    });
}

fn update_notices(engine: &Engine) -> Vec<(String, NotificationState, u64)> {
    engine
        .state()
        .notifications
        .values()
        .filter(|n| {
            n.condition
                .as_deref()
                .is_some_and(|c| c.starts_with("companion-update:"))
        })
        .map(|n| (n.title.clone(), n.state, n.version))
        .collect()
}

/// S297: Install, Update and Remove show their plan first, run only on a
/// checked computer of a disarmed vehicle, one at a time, and keep how they
/// went with the computer; one a restart cut short is marked failed.
#[test]
#[allow(clippy::too_many_lines, reason = "one update's whole story")]
fn an_update_shows_its_plan_and_runs_only_while_disarmed() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let update =
        json!({"vehicleId":"v","computerId":id,"action":"update","to":"udp:192.168.1.10:14550"});
    assert!(
        refused(&mut engine, &mut ui, "computer/installPlan", update.clone()).contains("first")
    );
    assert!(refused(&mut engine, &mut ui, "computer/install", update.clone()).contains("first"));
    older_computer(&mut engine, &id, "0.0.9", &[]);
    let software = ok(
        &mut engine,
        &mut ui,
        "computer/software",
        json!({"vehicleId":"v"}),
    );
    assert_eq!(software["ground"], "0.1.0");
    assert_eq!(
        software["computers"][0]["status"], "update-available",
        "{software}"
    );
    assert_eq!(software["computers"][0]["offer"], "update");
    let plan = ok(&mut engine, &mut ui, "computer/installPlan", update.clone());
    assert_eq!(plan["version"], "0.1.0");
    assert_eq!(plan["target"], "aarch64-unknown-linux-gnu");
    assert!(
        plan["unit"].as_str().unwrap().contains("User=uxduck"),
        "{plan}"
    );
    assert!(
        plan["unit"]
            .as_str()
            .unwrap()
            .contains("--to udp:192.168.1.10:14550"),
        "{plan}"
    );
    assert!(ssh.0.lock().unwrap().is_empty(), "a plan changes nothing");

    observed(&mut engine, true);
    assert!(refused(&mut engine, &mut ui, "computer/install", update.clone()).contains("Disarm"));
    assert!(ssh.0.lock().unwrap().is_empty());
    observed(&mut engine, false);

    let running = ok(&mut engine, &mut ui, "computer/install", update.clone());
    assert_eq!(
        running["companions"][0]["install"]["state"], "running",
        "{running}"
    );
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Install { plan, .. }] if plan.version.as_deref() == Some("0.1.0") && plan.action == InstallAction::Update),
        "{orders:?}"
    );
    assert!(
        refused(&mut engine, &mut ui, "computer/install", update.clone())
            .contains("already running")
    );

    let restarted = self::engine(engine.state().clone(), &keys);
    let cut = restarted.state().vehicles["v"].companions[0]
        .install
        .clone()
        .unwrap();
    assert_eq!(cut.state, InstallState::Failed);
    assert!(cut.problem.unwrap().contains("restarted"));

    engine.companion_installed(InstallAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        record: InstallRecord {
            action: InstallAction::Update,
            state: InstallState::Done,
            at_us: 0,
            version: Some("0.1.0".into()),
            steps: vec![InstallStep {
                title: "Write uxduck-companion.service".into(),
                ok: true,
                output: String::new(),
            }],
            problem: None,
        },
        found: Some(Found {
            host_key: Some("SHA256:known".into()),
            arch: Some("aarch64".into()),
            uxduck: Some("0.1.0".into()),
            software: Some(SoftwareFound {
                systemd: true,
                sudo: Some(true),
                bits: Some(64),
                link: Some("/opt/uxduck/0.1.0/uxduck".into()),
                ..SoftwareFound::default()
            }),
            ..Found::default()
        }),
    });
    let software = ok(
        &mut engine,
        &mut ui,
        "computer/software",
        json!({"vehicleId":"v"}),
    );
    assert_eq!(
        software["computers"][0]["status"], "up-to-date",
        "{software}"
    );
    assert_eq!(software["computers"][0]["managed"], true);
    assert_eq!(software["computers"][0]["install"]["state"], "done");
    // The computer is free again for the next change.
    ok(
        &mut engine,
        &mut ui,
        "computer/installPlan",
        json!({"vehicleId":"v","computerId":id,"action":"remove"}),
    );
    ok(
        &mut engine,
        &mut ui,
        "computer/install",
        json!({"vehicleId":"v","computerId":id,"action":"remove"}),
    );
}

/// S297, ADR-0061 B10: a connected, disarmed vehicle whose computer runs an
/// older UXDuck says so once per version, never while armed, and takes it
/// back when the computer is up to date or the setting is turned off.
#[test]
fn an_older_uxduck_is_noticed_once_per_version_and_only_on_the_ground() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    observed(&mut engine, true);
    engine.set_companions(Box::new(Ssh::default()));
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({"vehicleId":"v","host":"pi.local"}),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    older_computer(&mut engine, &id, "0.0.9", &["recorder"]);
    engine.review_companion_software();
    assert!(update_notices(&engine).is_empty(), "never while armed");

    observed(&mut engine, false);
    engine.review_companion_software();
    engine.review_companion_software();
    let notices = update_notices(&engine);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].0.ends_with("runs UXDuck 0.0.9"), "{notices:?}");
    let notice = engine
        .state()
        .notifications
        .values()
        .find(|n| n.title == notices[0].0)
        .unwrap()
        .clone();
    assert_eq!(
        notice.severity,
        Severity::Warning,
        "a feature it uses is missing"
    );
    assert!(
        notice
            .body
            .as_deref()
            .unwrap()
            .contains("This computer has 0.1.0. Update while disarmed.")
    );
    assert!(!notice.body.as_deref().unwrap().contains(';'));
    assert!(matches!(
        &notice.actions[..],
        [NotificationAction { action: Action::OpenCompanionSoftware { computer_id, .. }, .. }] if *computer_id == id
    ));

    older_computer(&mut engine, &id, "0.1.0", &["recorder"]);
    engine.review_companion_software();
    let resolved = update_notices(&engine);
    assert!(
        resolved.iter().all(|n| n.1 == NotificationState::Resolved),
        "{resolved:?}"
    );
    engine.review_companion_software();
    assert_eq!(
        update_notices(&engine),
        resolved,
        "a resolved notice is left alone"
    );

    older_computer(&mut engine, &id, "0.0.8", &[]);
    engine.review_companion_software();
    assert_eq!(
        update_notices(&engine)
            .iter()
            .filter(|n| n.1 != NotificationState::Resolved)
            .count(),
        1
    );
    let version = engine.state().settings.version;
    ok(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({"expectedVersion": version, "companionUpdateNotices": false}),
    );
    engine.review_companion_software();
    assert!(
        update_notices(&engine)
            .iter()
            .all(|n| n.1 == NotificationState::Resolved)
    );
}
