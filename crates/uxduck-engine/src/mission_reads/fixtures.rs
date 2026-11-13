#![allow(
    clippy::unwrap_used,
    reason = "in-memory admission and completion fixtures"
)]

use crate::runtime::{Clock, CommandOrder, Forget, LinkPort, Ports, Random, TransferOrder};
use crate::{ConnectionId, Engine, EngineConfig};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::mission::{MissionItem, MissionOrigin, MissionRecord};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;

#[derive(Clone, Default)]
pub(crate) struct Orders(Arc<Mutex<Vec<String>>>);
impl Orders {
    pub(crate) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}
impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {
        self.0.lock().unwrap().push("open".into());
    }
    fn close(&mut self, _: &str) {
        self.0.lock().unwrap().push("close".into());
    }
    fn command(&mut self, order: CommandOrder) {
        self.0.lock().unwrap().push(format!("{order:?}"));
    }
    fn transfer(&mut self, order: TransferOrder) {
        self.0.lock().unwrap().push(format!("{order:?}"));
    }
}
struct Tick;
impl Clock for Tick {
    fn now_us(&self) -> u64 {
        1_000
    }
}
struct Counter(u8);
impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_add(37);
            *byte = self.0;
        }
    }
}
pub(crate) struct Client {
    pub(crate) id: ConnectionId,
    pub(crate) inbox: UnboundedReceiver<String>,
}
pub(crate) fn frames(client: &mut Client) -> Vec<Value> {
    std::iter::from_fn(|| client.inbox.try_recv().ok())
        .map(|text| serde_json::from_str(&text).unwrap())
        .collect()
}
pub(crate) fn send(engine: &mut Engine, client: &Client, method: &str, params: Value) {
    let mut request = json!({ "jsonrpc": "2.0", "id": method, "method": method });
    request["params"] = params;
    engine.receive(client.id, &request.to_string());
}
pub(crate) fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    send(engine, client, method, params);
    frames(client)
        .into_iter()
        .find(|frame| frame["id"] == method)
        .unwrap()
}
pub(crate) fn connect(engine: &mut Engine, kind: &str, support: Option<bool>) -> Client {
    let (outbox, inbox) = unbounded();
    let mut client = Client {
        id: engine.connect(Transport::Socket, Trust::LocalUser, outbox),
        inbox,
    };
    let mut capabilities = json!({ "protocolVersion": 1, "approvals": true });
    if let Some(support) = support {
        capabilities["missionReadFloatBits"] = json!(support);
    }
    let initialized = call(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": format!("mission-reader-{kind}-{support:?}"), "version": "0.13.0", "kind": kind },
            "capabilities": capabilities,
        }),
    );
    assert!(initialized.get("result").is_some(), "{initialized}");
    client
}
#[allow(
    clippy::too_many_lines,
    reason = "complete in-memory vehicle and plan fixture"
)]
pub(crate) fn setup() -> (Engine, Client, Orders, String) {
    let mut engine = Engine::new(
        EngineConfig {
            daemon_version: "0.13.0".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:45000".into(),
            checkpoint_every: 1_000,
            models: vec![],
            agent_provider: "fake".into(),
            about: crate::HostAbout::default(),
        },
        Box::new(Tick),
        Box::new(Counter(0)),
        Ports::with_persistence(Box::new(Forget)),
        crate::Loaded::default(),
    );
    let orders = Orders::default();
    engine.links = Box::new(orders.clone());
    let mut ui = connect(&mut engine, "operator-ui", Some(true));
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    engine.vehicle_observed(Observation {
        connections: None,
        id: "v".into(),
        aliases: vec![],
        link_id: link["result"]["id"].as_str().unwrap().into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 1_000,
        armed: false,
        mode: None,
    });
    let item = MissionItem {
        command: 16,
        frame: 3,
        param1: 0.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        latitude_deg: 47.25,
        longitude_deg: -1.5,
        altitude_m: 40.0,
        autocontinue: true,
    };
    let mission = serde_json::from_value(
        json!({ "vehicleId": "v", "items": [item], "status": "synced", "version": 1 }),
    )
    .unwrap();
    engine.state.missions.insert("v".into(), mission);
    engine.state.mission_records.insert(
        "record".into(),
        MissionRecord {
            id: "record".into(),
            vehicle_id: "v".into(),
            items: vec![item],
            fence: vec![],
            rally: vec![],
            origin: MissionOrigin::Found,
            client_id: None,
            operator_id: None,
            first_seen_us: 1_000,
            last_seen_us: 1_000,
            times: 1,
            seq: engine.state.seq,
        },
    );
    let created = call(
        &mut engine,
        &mut ui,
        "plan/create",
        json!({ "vehicleType": "multicopter" }),
    );
    let plan = created["result"]["id"].as_str().unwrap().to_owned();
    let mut body = created["result"].clone();
    body["items"] = json!([{ "command": 16, "frame": 3, "params": [0, 0, 0, 0], "latitudeDeg": 47.25, "longitudeDeg": -1.5, "altitudeM": 40, "autocontinue": true }]);
    let updated = call(
        &mut engine,
        &mut ui,
        "plan/update",
        json!({ "id": plan, "expectedVersion": 1, "body": body }),
    );
    assert!(updated.get("result").is_some(), "{updated}");
    orders.take();
    frames(&mut ui);
    (engine, ui, orders, plan)
}
