//! A vehicle's files (S121, ADR-0027): listings and reads answered when the
//! vehicle answers, downloads as journaled transfers that resume from their
//! copy, pause, wait for the vehicle and survive a restart of UXDuck.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, FileOp, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem, Ports,
    Random, TransferOrder,
};
use uxduck_engine::transfers::Failure;
use uxduck_engine::vehicle_files::{FileReport, FilesPort};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const S: u64 = 1_000_000;

#[derive(Clone, Default)]
struct Time(Arc<AtomicU64>);

impl Time {
    fn advance(&self, us: u64) {
        self.0.fetch_add(us, Ordering::SeqCst);
    }
}

impl Clock for Time {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
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

#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<TransferOrder>>>);

impl Orders {
    fn take(&self) -> Vec<TransferOrder> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The one file job ordered: its id and what it does.
    fn file(&self) -> (String, FileOp) {
        let all = self.take();
        let files: Vec<(String, FileOp)> = all
            .into_iter()
            .filter_map(|o| match o {
                TransferOrder::File { id, op, .. } => Some((id, op)),
                _ => None,
            })
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        files.into_iter().next().unwrap()
    }
}

impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn transfer(&mut self, order: TransferOrder) {
        self.0.lock().unwrap().push(order);
    }
}

/// A copy's bytes, and whether it is whole.
type Copy = (Vec<u8>, bool);

/// Copies in memory, by local path; partial until finished.
#[derive(Clone, Default)]
struct Disk(Arc<Mutex<BTreeMap<String, Copy>>>);

impl Disk {
    fn get(&self, local: &str) -> Option<(Vec<u8>, bool)> {
        self.0.lock().unwrap().get(local).cloned()
    }
}

impl FilesPort for Disk {
    fn copied(&self, local: &str) -> u64 {
        self.get(local).map_or(0, |(b, _)| b.len() as u64)
    }
    fn write(&mut self, local: &str, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let mut disk = self.0.lock().unwrap();
        let (copy, _) = disk.entry(local.to_owned()).or_default();
        if (copy.len() as u64) < offset {
            return Err("a gap".into());
        }
        copy.truncate(usize::try_from(offset).unwrap());
        copy.extend_from_slice(bytes);
        Ok(())
    }
    fn finish(&mut self, local: &str) -> Result<(), String> {
        let mut disk = self.0.lock().unwrap();
        disk.entry(local.to_owned()).or_default().1 = true;
        Ok(())
    }
    fn discard(&mut self, local: &str) {
        self.0.lock().unwrap().remove(local);
    }
    fn grant(&mut self, local: &str, name: &str) -> Option<String> {
        self.get(local)
            .filter(|(_, whole)| *whole)
            .map(|_| format!("/api/vehicle-files/{name}?token=t"))
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn send(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Option<Value> {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            return Some(frame);
        }
        client.queued.push_back(frame);
    }
    None
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    send(engine, client, method, params).expect("a reply")
}

/// The reply to the last request, once it came.
fn answer(client: &mut Client) -> Value {
    let id = client.next_id;
    std::iter::from_fn(|| client.inbox.try_recv().ok())
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .chain(client.queued.drain(..))
        .find(|f| f["id"] == id)
        .expect("an answer")
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
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
            "clientInfo": { "name": kind, "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

struct Bench {
    engine: Engine,
    ui: Client,
    orders: Orders,
    disk: Disk,
    time: Time,
    link_id: String,
}

fn bench() -> Bench {
    let orders = Orders::default();
    let time = Time::default();
    time.advance(1_000);
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(orders.clone()),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(time.clone()),
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    );
    let disk = Disk::default();
    engine.set_files(Box::new(disk.clone()));
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let mut bench = Bench {
        engine,
        ui,
        orders,
        disk,
        time,
        link_id,
    };
    bench.seen(Presence::Online);
    bench.orders.take();
    bench
}

impl Bench {
    fn seen(&mut self, presence: Presence) {
        self.engine.vehicle_observed(Observation {
            id: "v".into(),
            aliases: Vec::new(),
            link_id: self.link_id.clone(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence,
            last_heartbeat_us: 5,
            armed: false,
            mode: None,
        });
    }

    fn call(&mut self, method: &str, params: Value) -> Option<Value> {
        send(&mut self.engine, &mut self.ui, method, params)
    }

    fn transfer(&self, id: &str) -> Value {
        serde_json::to_value(&self.engine.state().file_transfers[id]).unwrap()
    }

    fn data(&mut self, id: &str, offset: u64, bytes: &[u8]) {
        self.engine.file_reported(FileReport::Data {
            id: id.into(),
            offset,
            bytes: bytes.to_vec(),
        });
    }
}

#[test]
fn a_listing_is_answered_when_the_vehicle_answers_folders_first() {
    let mut b = bench();
    assert!(
        b.call(
            "files/list",
            json!({ "vehicleId": "v", "path": "/APM/LOGS/" })
        )
        .is_none(),
        "answered later"
    );
    let (id, op) = b.orders.file();
    assert_eq!(
        op,
        FileOp::List {
            path: "/APM/LOGS".into()
        }
    );
    b.engine.file_reported(FileReport::Listed {
        id: id.clone(),
        entries: vec![
            ("00000002.BIN".into(), Some(20_000)),
            ("old".into(), None),
            ("00000001.BIN".into(), Some(3_000)),
        ],
    });
    let reply = answer(&mut b.ui);
    let entries = &reply["result"]["entries"];
    assert_eq!(entries[0]["name"], "old");
    assert_eq!(entries[0]["dir"], true);
    assert_eq!(entries[1]["path"], "/APM/LOGS/00000001.BIN");
    assert_eq!(entries[2]["size"], 20_000);
    // The job's own end that follows is nobody's.
    b.engine
        .file_reported(FileReport::Done { id, result: Ok(()) });
}

#[test]
fn a_small_file_is_read_whole_and_a_large_one_cut_at_64_kb() {
    let mut b = bench();
    b.call(
        "files/read",
        json!({ "vehicleId": "v", "path": "@SYS/uarts.txt" }),
    );
    let (id, _) = b.orders.file();
    b.data(&id, 0, b"SERIAL2 UART5 TX = 0\n");
    b.engine
        .file_reported(FileReport::Done { id, result: Ok(()) });
    let reply = answer(&mut b.ui);
    assert!(
        reply["result"]["text"]
            .as_str()
            .unwrap()
            .contains("SERIAL2")
    );
    assert_eq!(reply["result"]["truncated"], false);
    assert_eq!(reply["result"]["binary"], false);

    b.call(
        "files/read",
        json!({ "vehicleId": "v", "path": "/APM/LOGS/1.BIN" }),
    );
    let (id, _) = b.orders.file();
    for i in 0..300 {
        b.data(&id, i * 239, &[0xa3; 239]);
    }
    let reply = answer(&mut b.ui);
    assert_eq!(reply["result"]["truncated"], true);
    assert_eq!(reply["result"]["binary"], true);
    assert!(
        b.orders
            .take()
            .iter()
            .any(|o| matches!(o, TransferOrder::CancelFile { id: c, .. } if *c == id)),
        "the rest is not read"
    );
}

#[test]
fn a_refusal_is_said_in_words_and_the_system_files_are_read_only() {
    let mut b = bench();
    b.call(
        "files/remove",
        json!({ "vehicleId": "v", "path": "/APM/LOGS/1.BIN" }),
    );
    let (id, op) = b.orders.file();
    assert_eq!(
        op,
        FileOp::Remove {
            path: "/APM/LOGS/1.BIN".into()
        }
    );
    b.engine.file_reported(FileReport::Done {
        id,
        result: Err(Failure::Rejected("the vehicle does not allow that".into())),
    });
    let reply = answer(&mut b.ui);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .ends_with("the vehicle does not allow that"),
        "{reply}"
    );
    let refused = b
        .call(
            "files/upload",
            json!({ "vehicleId": "v", "path": "@SYS/uarts.txt", "contentBase64": "eA==" }),
        )
        .unwrap();
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("read-only")
    );
}

#[test]
fn a_download_is_copied_in_order_measured_and_saved() {
    let mut b = bench();
    let reply = b
        .call(
            "files/download",
            json!({ "vehicleId": "v", "path": "/APM/LOGS/00000003.BIN", "size": 60_000 }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    assert_eq!(reply["result"]["state"], "queued");
    assert_eq!(reply["result"]["size"], 60_000);
    let (job, op) = b.orders.file();
    assert_eq!(job, id);
    assert_eq!(
        op,
        FileOp::Read {
            path: "/APM/LOGS/00000003.BIN".into(),
            from: 0
        }
    );
    b.engine.file_reported(FileReport::Opened {
        id: id.clone(),
        size: 60_000,
    });
    assert_eq!(b.transfer(&id)["state"], "running");
    // 12 KB a second for five seconds.
    for second in 0..5 {
        b.time.advance(S);
        b.data(&id, second * 12_000, &[7; 12_000]);
    }
    let running = b.transfer(&id);
    assert_eq!(running["done"], 60_000);
    assert_eq!(running["rateBps"], 12_000);
    b.engine.file_reported(FileReport::Done {
        id: id.clone(),
        result: Ok(()),
    });
    let done = b.transfer(&id);
    assert_eq!(done["state"], "done");
    assert!(done.get("rateBps").is_none());
    let (copy, whole) = b.disk.get("v/APM/LOGS/00000003.BIN").unwrap();
    assert!(whole);
    assert_eq!(copy.len(), 60_000);
    let saved = b.call("files/save", json!({ "id": id })).unwrap();
    assert_eq!(saved["result"]["name"], "00000003.BIN");
    assert!(
        saved["result"]["url"]
            .as_str()
            .unwrap()
            .starts_with("/api/vehicle-files/")
    );
    // Put away: the row and the copy go.
    b.call("files/cancel", json!({ "id": id })).unwrap();
    assert!(!b.engine.state().file_transfers.contains_key(&id));
    assert!(b.disk.get("v/APM/LOGS/00000003.BIN").is_none());
}

#[test]
fn a_file_done_within_a_second_is_as_big_as_its_copy() {
    let mut b = bench();
    let reply = b
        .call(
            "files/download",
            json!({ "vehicleId": "v", "path": "/sitl.log", "size": 1_095 }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    b.engine.file_reported(FileReport::Opened {
        id: id.clone(),
        size: 1_095,
    });
    // All of it within the second after it opened: nothing journaled since.
    b.data(&id, 0, &[1; 1_000]);
    b.data(&id, 1_000, &[1; 95]);
    b.engine.file_reported(FileReport::Done {
        id: id.clone(),
        result: Ok(()),
    });
    let done = b.transfer(&id);
    assert_eq!(done["state"], "done");
    assert_eq!(done["done"], 1_095);
    assert_eq!(done["size"], 1_095);
}

#[test]
fn a_paused_download_resumes_from_its_copy() {
    let mut b = bench();
    let reply = b
        .call(
            "files/download",
            json!({ "vehicleId": "v", "path": "/APM/LOGS/00000002.BIN" }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    b.orders.take();
    b.data(&id, 0, &[1; 5_000]);
    let paused = b.call("files/pause", json!({ "id": id })).unwrap();
    assert_eq!(paused["result"]["state"], "paused");
    assert!(matches!(
        b.orders.take().as_slice(),
        [TransferOrder::CancelFile { .. }]
    ));
    // A late answer from the stopped job changes nothing.
    b.data(&id, 5_000, &[1; 100]);
    assert_eq!(b.transfer(&id)["done"], 5_000);
    let resumed = b.call("files/resume", json!({ "id": id })).unwrap();
    assert_eq!(resumed["result"]["state"], "queued");
    assert_eq!(resumed["result"]["done"], 5_000);
    let (_, op) = b.orders.file();
    assert_eq!(
        op,
        FileOp::Read {
            path: "/APM/LOGS/00000002.BIN".into(),
            from: 5_000
        }
    );
}

#[test]
fn a_download_waits_for_its_vehicle_and_goes_on_when_it_is_back() {
    let mut b = bench();
    let reply = b
        .call(
            "files/download",
            json!({ "vehicleId": "v", "path": "/APM/LOGS/00000002.BIN" }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    b.orders.take();
    b.data(&id, 0, &[1; 2_000]);
    b.engine.file_reported(FileReport::Done {
        id: id.clone(),
        result: Err(Failure::Timeout),
    });
    let waiting = b.transfer(&id);
    assert_eq!(waiting["state"], "waiting");
    assert!(waiting["detail"].as_str().unwrap().contains("no answer"));
    // Offline: nothing tried.
    b.seen(Presence::Offline);
    b.time.advance(10 * S);
    b.engine.resume_waiting_files(b.time.now_us());
    assert!(b.orders.take().is_empty());
    // Back: it goes on from its copy.
    b.seen(Presence::Online);
    b.engine.resume_waiting_files(b.time.now_us());
    let (_, op) = b.orders.file();
    assert_eq!(
        op,
        FileOp::Read {
            path: "/APM/LOGS/00000002.BIN".into(),
            from: 2_000
        }
    );
    assert_eq!(b.transfer(&id)["state"], "queued");
}

#[test]
fn a_download_running_when_uxduck_stopped_waits_with_its_copy() {
    let mut b = bench();
    let reply = b
        .call(
            "files/download",
            json!({ "vehicleId": "v", "path": "/APM/LOGS/00000002.BIN" }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    b.data(&id, 0, &[1; 3_000]);
    // Started again: the port is given again, as the daemon does.
    b.engine.set_files(Box::new(b.disk.clone()));
    let waiting = b.transfer(&id);
    assert_eq!(waiting["state"], "waiting");
    assert_eq!(waiting["done"], 3_000);
}

#[test]
fn an_upload_sends_its_bytes_and_a_failure_is_kept_in_words() {
    let mut b = bench();
    let reply = b
        .call(
            "files/upload",
            json!({ "vehicleId": "v", "path": "/APM/scripts/hello.lua", "contentBase64": "aGVsbG8=" }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    assert_eq!(reply["result"]["direction"], "upload");
    assert_eq!(reply["result"]["size"], 5);
    let (_, op) = b.orders.file();
    assert_eq!(
        op,
        FileOp::Write {
            path: "/APM/scripts/hello.lua".into(),
            bytes: b"hello".to_vec()
        }
    );
    b.engine.file_reported(FileReport::Sent {
        id: id.clone(),
        done: 5,
        total: 5,
    });
    b.engine.file_reported(FileReport::Done {
        id: id.clone(),
        result: Ok(()),
    });
    assert_eq!(b.transfer(&id)["state"], "done");

    let reply = b
        .call(
            "files/upload",
            json!({ "vehicleId": "v", "path": "/APM/scripts/full.lua", "contentBase64": "aGVsbG8=" }),
        )
        .unwrap();
    let id = reply["result"]["id"].as_str().unwrap().to_owned();
    b.engine.file_reported(FileReport::Done {
        id: id.clone(),
        result: Err(Failure::Rejected("the vehicle's storage is full".into())),
    });
    let failed = b.transfer(&id);
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["detail"], "the vehicle's storage is full");
    let too_big = b
        .call(
            "files/upload",
            json!({ "vehicleId": "v", "path": "/APM/x.bin",
                    "contentBase64": base64_of(&vec![0u8; 5 * 1024 * 1024]) }),
        )
        .unwrap();
    assert!(
        too_big["error"]["message"]
            .as_str()
            .unwrap()
            .contains("4 MB")
    );
}

fn base64_of(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn downloads_need_a_place_to_keep_copies() {
    let mut b = bench();
    // An engine without the port, as the browser's own is.
    let orders = Orders::default();
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(orders),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let _ = &mut b;
    let mut engine = Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:47800/".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(Time::default()),
        Box::new(Counter(1)),
        ports,
        Loaded::default(),
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14551 } }),
    );
    engine.vehicle_observed(Observation {
        id: "w".into(),
        aliases: Vec::new(),
        link_id: link["result"]["id"].as_str().unwrap().into(),
        peer: "p".into(),
        system_id: 2,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    let reply = call(
        &mut engine,
        &mut ui,
        "files/download",
        json!({ "vehicleId": "w", "path": "/APM/LOGS/1.BIN" }),
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("UXDuck Desktop"),
        "{reply}"
    );
}
