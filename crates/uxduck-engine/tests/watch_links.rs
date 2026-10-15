#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test helpers and scenarios"
)]

//! Watch links at the owner's computer (S95, ADR-0023): `watchLink/*` go
//! to the account and come back journaled, with the guests' count from the
//! relay; turning off "Let my account watch this computer" and signing out
//! end them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::edge::{EdgeState, EdgeStatus, WatchViewers};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::Change;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::account::{AccountAction, AccountOrder, AccountPort};
use uxduck_engine::runtime::{
    Clock, MemoryCredentials, NoAgent, NoLinks, NoSystem, Persistence, Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, State};

struct Fixed;

impl Clock for Fixed {
    fn now_us(&self) -> u64 {
        1_791_000_000_000_000
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

/// The journal, kept to read back.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<Change>>>);

impl Persistence for Journal {
    fn append(&mut self, event: &uxduck_core::events::Event) {
        self.0.lock().unwrap().push(event.change.clone());
    }
    fn checkpoint(&mut self, _: &State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

/// The account's end: the orders it was given.
#[derive(Clone, Default)]
struct Account(Arc<Mutex<Vec<AccountOrder>>>);

impl AccountPort for Account {
    fn request(&mut self, order: AccountOrder) {
        self.0.lock().unwrap().push(order);
    }
}

impl Account {
    fn take(&self) -> Vec<AccountOrder> {
        std::mem::take(&mut *self.0.lock().unwrap())
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
    reply(client, id)
}

fn reply(client: &mut Client, id: i64) -> Option<Value> {
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            return Some(frame);
        }
        client.queued.push_back(frame);
    }
    None
}

fn setup() -> (Engine, Client, Account, Journal) {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let journal = Journal::default();
    let ports = Ports {
        persistence: Box::new(journal.clone()),
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(Fixed),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: State::default(),
            sessions: vec![],
        },
    );
    let account = Account::default();
    engine.set_account(Box::new(account.clone()));
    engine.vehicle_observed(Observation {
        id: "v1".into(),
        aliases: vec![],
        link_id: "udp".into(),
        peer: "127.0.0.1:5761".into(),
        system_id: 1,
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
    let (tx, inbox) = unbounded();
    let id = engine.connect(Transport::Internal, Trust::LocalUser, tx);
    let mut ui = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    send(
        &mut engine,
        &mut ui,
        "initialize",
        json!({
            "clientInfo": { "name": "ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    )
    .unwrap();
    (engine, ui, account, journal)
}

fn signed_in(engine: &mut Engine) {
    engine.edge_changed(EdgeStatus {
        state: EdgeState::Connected,
        round_trip_ms: None,
        clock_offset_ms: None,
        problem: None,
    });
}

#[test]
#[cfg(debug_assertions)]
fn development_server_selection_uses_the_account_port_and_refuses_armed_vehicles() {
    use uxduck_contracts::dto::account::AccountEnvironment;
    let (mut engine, mut ui, account, _) = setup();
    assert!(
        send(
            &mut engine,
            &mut ui,
            "account/setEnvironment",
            json!({ "environment": "production" })
        )
        .is_none()
    );
    let order = account.take().pop().unwrap();
    assert_eq!(
        order.action,
        AccountAction::SetEnvironment {
            environment: Some(AccountEnvironment::Production)
        }
    );
    assert!(!engine.restart_requested());
    engine.account_done(&order.id, Ok(json!({ "restartRequired": true })));
    let id = ui.next_id;
    assert_eq!(
        reply(&mut ui, id).unwrap()["result"]["restartRequired"],
        true
    );

    engine.vehicle_observed(Observation {
        id: "v1".into(),
        aliases: vec![],
        link_id: "udp".into(),
        peer: "127.0.0.1:5761".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 6,
        armed: true,
        mode: None,
    });
    let denied = send(
        &mut engine,
        &mut ui,
        "account/setEnvironment",
        json!({ "environment": "staging" }),
    )
    .unwrap();
    assert_eq!(denied["error"]["data"]["kind"], "confirmation-required");
    assert!(account.take().is_empty());
}

#[test]
fn an_agent_cannot_change_the_daemons_account_server() {
    let (mut engine, _, account, _) = setup();
    let (tx, inbox) = unbounded();
    let mut agent = Client {
        id: engine.connect(Transport::Socket, Trust::LocalUser, tx),
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    send(
        &mut engine,
        &mut agent,
        "initialize",
        json!({
            "clientInfo": { "name": "agent", "version": "0.1.0", "kind": "agent" },
            "capabilities": { "protocolVersion": 1 }
        }),
    )
    .unwrap();
    let denied = send(
        &mut engine,
        &mut agent,
        "account/setEnvironment",
        json!({ "environment": "production" }),
    )
    .unwrap();
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    assert!(account.take().is_empty());
}

/// The journal's watch link entries, in order.
fn journaled(journal: &Journal) -> Vec<Change> {
    journal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|c| {
            matches!(
                c,
                Change::WatchLinkCreated { .. } | Change::WatchLinkRevoked { .. }
            )
        })
        .cloned()
        .collect()
}

#[test]
fn a_watch_link_is_made_listed_with_its_guests_and_revoked_through_the_account() {
    let (mut engine, mut ui, account, journal) = setup();
    // Signed out: nothing to list, and nothing to make.
    let listed = send(&mut engine, &mut ui, "watchLink/list", json!({})).unwrap();
    assert_eq!(listed["result"], json!({ "links": [] }));
    let refused = send(
        &mut engine,
        &mut ui,
        "watchLink/create",
        json!({ "vehicleId": "v1", "flownBy": "Ben" }),
    )
    .unwrap();
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );

    signed_in(&mut engine);
    assert!(
        send(
            &mut engine,
            &mut ui,
            "watchLink/create",
            json!({ "vehicleId": "v1", "flownBy": "Ben", "hours": 2 }),
        )
        .is_none(),
        "answered when the account is"
    );
    let [order] = account.take().try_into().unwrap();
    let AccountAction::CreateWatchLink {
        device,
        vehicle_id,
        vehicle_name,
        flown_by,
        hours,
    } = order.action
    else {
        panic!("a watch link made")
    };
    assert!(device.starts_with("dv"));
    assert_eq!(vehicle_id.as_deref(), Some("v1"));
    assert!(vehicle_name.is_some());
    assert_eq!((flown_by.as_str(), hours), ("Ben", Some(2)));
    let link = json!({
        "id": "wl0000000000000001",
        "deviceId": device,
        "vehicleId": "v1",
        "flownBy": "Ben",
        "createdAtMs": 1,
        "expiresAtMs": 7_200_001,
    });
    engine.account_done(
        &order.id,
        Ok(json!({ "link": link, "url": "https://app.uxduck.com/watch/token" })),
    );
    let made = {
        let id = ui.next_id;
        reply(&mut ui, id)
    }
    .unwrap();
    assert_eq!(made["result"]["url"], "https://app.uxduck.com/watch/token");
    assert_eq!(
        journaled(&journal),
        vec![Change::WatchLinkCreated {
            link: "wl0000000000000001".into(),
            vehicle: Some("v1".into()),
            expires_at_ms: 7_200_001,
        }]
    );

    // The relay says how many watch; the list carries it, and the vehicle's count.
    engine.watch_viewers(vec![WatchViewers {
        link_id: "wl0000000000000001".into(),
        vehicle_id: Some("v1".into()),
        count: 12,
    }]);
    assert_eq!(engine.watching("v1"), 12);
    assert_eq!(engine.watching("v2"), 0);
    // Pushed to the operator's window for the sidebar, and asked for again.
    let mut pushed = Vec::new();
    while let Ok(text) = ui.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["method"] == "watchLink/viewersChanged" {
            pushed.push(frame["params"].clone());
        }
    }
    assert_eq!(
        pushed,
        [json!({ "vehicles": [{ "vehicleId": "v1", "watching": 12 }] })]
    );
    let viewers = send(&mut engine, &mut ui, "watchLink/viewers", json!({})).unwrap();
    assert_eq!(viewers["result"]["vehicles"][0]["watching"], 12);
    // The same numbers again push nothing.
    engine.watch_viewers(vec![WatchViewers {
        link_id: "wl0000000000000001".into(),
        vehicle_id: Some("v1".into()),
        count: 12,
    }]);
    assert!(ui.inbox.try_recv().is_err());
    send(&mut engine, &mut ui, "watchLink/list", json!({}));
    let [order] = account.take().try_into().unwrap();
    assert!(matches!(order.action, AccountAction::WatchLinks { .. }));
    engine.account_done(&order.id, Ok(json!({ "links": [link] })));
    let listed = {
        let id = ui.next_id;
        reply(&mut ui, id)
    }
    .unwrap();
    assert_eq!(listed["result"]["links"][0]["watching"], 12);
    assert_eq!(
        listed["result"]["links"][0]["link"]["id"],
        "wl0000000000000001"
    );

    send(
        &mut engine,
        &mut ui,
        "watchLink/revoke",
        json!({ "linkId": "wl0000000000000001" }),
    );
    let [order] = account.take().try_into().unwrap();
    assert_eq!(
        order.action,
        AccountAction::RevokeWatchLink {
            id: "wl0000000000000001".into()
        }
    );
    engine.account_done(&order.id, Ok(json!({})));
    assert_eq!(
        {
            let id = ui.next_id;
            reply(&mut ui, id)
        }
        .unwrap()["result"],
        json!({})
    );
    assert_eq!(
        journaled(&journal).last(),
        Some(&Change::WatchLinkRevoked {
            link: Some("wl0000000000000001".into())
        })
    );
}

#[test]
fn turning_off_watching_from_the_account_or_signing_out_ends_every_link() {
    let (mut engine, mut ui, account, _journal) = setup();
    signed_in(&mut engine);
    // Made only while the account may watch.
    let version = engine.state().settings.version;
    let updated = send(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": version, "watchFromAccount": false }),
    )
    .unwrap();
    assert!(updated.get("result").is_some(), "{updated}");
    let [order] = account.take().try_into().unwrap();
    assert!(matches!(
        order.action,
        AccountAction::RevokeWatchLinks { .. }
    ));
    let refused = send(
        &mut engine,
        &mut ui,
        "watchLink/create",
        json!({ "flownBy": "Ben" }),
    )
    .unwrap();
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("View this computer’s vehicles remotely"),
        "{refused}"
    );

    send(&mut engine, &mut ui, "account/signOut", json!({}));
    let [order] = account.take().try_into().unwrap();
    assert!(
        matches!(order.action, AccountAction::SignOut { device: Some(ref d) } if d.starts_with("dv")),
        "{:?}",
        order.action
    );
}
