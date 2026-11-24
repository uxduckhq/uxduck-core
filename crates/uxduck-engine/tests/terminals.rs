//! Terminals on companion computers (S422, S424, ADR-0065) through a fake
//! port in place of the daemon's `ssh`: who may open one, who asks an
//! operator first and how each answer lasts, the ticket that works once and
//! only for the client that opened it, what ends one, and what the journal
//! and the inspector's tap keep of it. Never a byte typed or shown, which
//! the engine never has, nor a command one was opened to run.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::approval::{ApprovalState, Remember};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::terminal::{ComputerRun, TerminalEnd, TerminalKind};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::companions::{CheckAnswer, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::{
    Clock, CommandOrder, Credentials, LinkPort, NoAgent, NoSystem, Persistence, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::terminals::{
    Collected, Keeps, RUN_DEADLINE_S, RUN_KEEPS, RunReport, TerminalOrder, TerminalPort,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, PairingState};

const NOW_US: u64 = 5_000_000;
const NOT_THE_OPENER: &str = "Only the client that opened this terminal can attach to it.";
const NOT_THE_HOLDER: &str =
    "Only the client that opened this terminal, or an operator, can change or end it.";
const NOT_FOR_AGENTS: &str = "An agent or a plugin cannot open a terminal on a vehicle's computer.";
const COMMAND_ONLY_HERE: &str =
    "Only uxduck ssh on the computer UXDuck runs on runs one command. Open a terminal instead.";
const NOT_ALLOWED: &str = "The operator did not allow this terminal.";
const NOBODY_ANSWERED: &str =
    "Nobody answered on the computer UXDuck runs on in time, so the terminal was not opened.";
const OPERATOR_ANSWERS: &str =
    "Only an operator on the computer UXDuck runs on can answer a request for a terminal.";
/// The phone as the client list names it: what it is and its address.
const PHONE: &str = "iPhone (192.168.1.31)";
const USED: &str = "This ticket was already used, or no terminal gave it.";
/// The approval deadline (ADR-0005).
const DEADLINE_US: u64 = 60_000_000;
const PINNED: &str = "SHA256:ld0pRnPj6iBw1VlXhDyqJMO6rdQ6axMIiQSimgq72bg";
const PHONE_PAGE: &str = "http://192.168.1.20:45000";

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        NOW_US
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
struct Journal(Arc<Mutex<Vec<Event>>>);

impl Persistence for Journal {
    fn append(&mut self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

struct Keys;

impl Credentials for Keys {
    fn set(&mut self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn has(&self, _: &str) -> bool {
        false
    }
}

struct NoChecks;

impl CompanionPort for NoChecks {
    fn request(&mut self, _: CompanionOrder) {}
}

/// What the engine asked of the daemon's `ssh`.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Open {
        terminal: String,
        computer: String,
        size: (u16, u16),
        command: Option<String>,
    },
    /// An agent's command (S425): no socket, and this much of what it
    /// prints kept for its answer.
    Run {
        terminal: String,
        computer: String,
        command: String,
        keeps: Keeps,
    },
    Resize(String, u16, u16),
    Close(String, TerminalEnd),
}

#[derive(Clone, Default)]
struct Ssh {
    asked: Arc<Mutex<Vec<Asked>>>,
    /// Why nothing starts, when set: no `ssh` on this computer.
    missing: Arc<Mutex<Option<String>>>,
}

impl Ssh {
    fn take(&self) -> Vec<Asked> {
        std::mem::take(&mut self.asked.lock().unwrap())
    }
}

impl TerminalPort for Ssh {
    fn open(&mut self, order: TerminalOrder) -> Result<(), String> {
        if let Some(why) = self.missing.lock().unwrap().clone() {
            return Err(why);
        }
        if let Some(keeps) = order.collect {
            self.asked.lock().unwrap().push(Asked::Run {
                terminal: order.terminal_id,
                computer: order.computer.id.clone(),
                command: order.command.map(|command| command.0).unwrap_or_default(),
                keeps,
            });
            return Ok(());
        }
        self.asked.lock().unwrap().push(Asked::Open {
            terminal: order.terminal_id,
            computer: order.computer.id.clone(),
            size: (order.cols, order.rows),
            command: order.command.map(|command| command.0),
        });
        Ok(())
    }
    fn resize(&mut self, terminal_id: &str, cols: u16, rows: u16) {
        self.asked
            .lock()
            .unwrap()
            .push(Asked::Resize(terminal_id.to_owned(), cols, rows));
    }
    fn close(&mut self, terminal_id: &str, end: TerminalEnd) {
        self.asked
            .lock()
            .unwrap()
            .push(Asked::Close(terminal_id.to_owned(), end));
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
    /// What `initialize` said it may do.
    granted: Value,
}

impl Client {
    /// Everything that arrived and no call took as its answer, oldest first.
    fn arrived(&mut self) -> Vec<Value> {
        let mut frames: Vec<Value> = self.queued.drain(..).collect();
        while let Ok(text) = self.inbox.try_recv() {
            frames.push(serde_json::from_str(&text).unwrap());
        }
        frames
    }

    /// The answer to a request sent earlier, if it has arrived. Anything
    /// else that arrived is dropped.
    fn answer_to(&mut self, id: i64) -> Option<Value> {
        self.arrived().into_iter().find(|frame| frame["id"] == id)
    }
}

/// The terminal and ticket of a `terminal/open` answer.
fn opened(reply: &Value) -> (String, String) {
    assert!(reply.get("result").is_some(), "{reply}");
    (
        reply["result"]["terminalId"].as_str().unwrap().to_owned(),
        reply["result"]["ticket"].as_str().unwrap().to_owned(),
    )
}

struct Harness {
    engine: Engine,
    ssh: Ssh,
    journal: Journal,
}

fn engine_with(state: State, journal: &Journal) -> Engine {
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
        persistence: Box::new(journal.clone()),
        agent: Box::new(NoAgent),
        credentials: Box::new(Keys),
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

impl Harness {
    /// A daemon with `ssh`, a vehicle heard and named Alpha, and nobody
    /// connected.
    fn new() -> Self {
        let journal = Journal::default();
        let mut engine = engine_with(State::default(), &journal);
        let ssh = Ssh::default();
        engine.set_terminals(Box::new(ssh.clone()));
        engine.set_companions(Box::new(NoChecks));
        let mut harness = Self {
            engine,
            ssh,
            journal,
        };
        harness.observed(false);
        harness
    }

    fn observed(&mut self, armed: bool) {
        self.engine.vehicle_observed(Observation {
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

    fn connect(&mut self, transport: Transport, trust: Trust, name: &str, kind: &str) -> Client {
        self.connect_declaring(transport, trust, name, kind, false)
    }

    /// As `connect`, declaring whether it shows approvals, as UXDuck's page
    /// does.
    fn connect_declaring(
        &mut self,
        transport: Transport,
        trust: Trust,
        name: &str,
        kind: &str,
        approvals: bool,
    ) -> Client {
        self.connect_showing(transport, trust, name, kind, approvals, true)
    }

    /// As `connect_declaring`, saying whether it shows the command an
    /// agent asks to run, as UXDuck's page does from S425 on.
    fn connect_showing(
        &mut self,
        transport: Transport,
        trust: Trust,
        name: &str,
        kind: &str,
        approvals: bool,
        shell_commands: bool,
    ) -> Client {
        let (outbox, inbox) = unbounded();
        let id = self.engine.connect(transport, trust, outbox);
        let mut client = Client {
            id,
            inbox,
            queued: VecDeque::new(),
            next_id: 0,
            granted: Value::Null,
        };
        let init = self.call(
            &mut client,
            "initialize",
            json!({
                "clientInfo": { "name": name, "version": "0.1.0", "kind": kind },
                "capabilities": { "protocolVersion": 1, "markerShapes": true, "missionReadFloatBits": true, "approvals": approvals, "shellCommands": shell_commands }
            }),
        );
        assert!(init.get("result").is_some(), "{init}");
        client.granted = init["result"]["granted"].clone();
        client
    }

    /// UXDuck's page in the browser paired on this computer: an operator
    /// that shows approvals.
    fn page(&mut self) -> Client {
        self.connect_declaring(
            Transport::WebSocket,
            Trust::PairedBrowser {
                origin: "http://127.0.0.1:45000".into(),
            },
            "uxduck-web",
            "operator-ui",
            true,
        )
    }

    /// A phone or tablet that pairs on the network with `access`: its
    /// record and how the daemon knows its sockets.
    fn pair(&mut self, operator: &mut Client, agent: &str, access: &str) -> (String, Trust) {
        let (id, code) = self
            .engine
            .request_network_pairing(PHONE_PAGE, Some(agent.into()), "192.168.1.31".into())
            .expect("asked");
        self.ok(
            operator,
            "pairing/approve",
            json!({ "code": code, "access": access }),
        );
        let PairingState::Approved { session, .. } = self.engine.poll_pairing(&id) else {
            panic!("not approved")
        };
        let record = uxduck_engine::pairing::network_client(&session);
        let trust = Trust::NetworkBrowser {
            origin: PHONE_PAGE.into(),
            client: record.clone(),
        };
        (record, trust)
    }

    /// UXDuck's page on a paired device, connected.
    fn device(&mut self, trust: &Trust) -> Client {
        self.connect_declaring(
            Transport::WebSocket,
            trust.clone(),
            "uxduck-web",
            "operator-ui",
            true,
        )
    }

    /// Sends a request and leaves its answer to arrive: the id it has.
    fn send(&mut self, client: &mut Client, method: &str, params: Value) -> i64 {
        client.next_id += 1;
        let id = client.next_id;
        self.engine.receive(
            client.id,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        id
    }

    /// Asks for a terminal and leaves the answer to arrive.
    fn ask(&mut self, client: &mut Client, computer: &str) -> i64 {
        self.send(
            client,
            "terminal/open",
            json!({ "computerId": computer, "cols": 80, "rows": 24 }),
        )
    }

    /// The one approval waiting for an answer.
    fn pending(&self) -> uxduck_core::domain::approval::Approval {
        let approvals = &self.engine.state().approvals;
        assert_eq!(approvals.len(), 1, "{approvals:?}");
        approvals.values().next().unwrap().clone()
    }

    /// An operator answers the one approval waiting.
    fn answer(&mut self, operator: &mut Client, allow: bool, remember: &str) {
        let id = self.pending().id;
        self.ok(
            operator,
            "approval/resolve",
            json!({ "id": id, "allow": allow, "remember": remember }),
        );
    }

    /// How each approval ended, in the journal's order.
    fn answers(&self) -> Vec<(ApprovalState, Option<Remember>)> {
        self.journal
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match &event.change {
                Change::ApprovalUpserted { approval }
                    if approval.state != ApprovalState::Pending =>
                {
                    Some((approval.state, approval.remember))
                }
                _ => None,
            })
            .collect()
    }

    /// `uxduck ssh`, or any program of the person's on the local socket.
    fn cli(&mut self) -> Client {
        self.connect(
            Transport::Socket,
            Trust::LocalUser,
            "uxduck-cli",
            "operator-cli",
        )
    }

    fn call(&mut self, client: &mut Client, method: &str, params: Value) -> Value {
        client.next_id += 1;
        let id = client.next_id;
        self.engine.receive(
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

    fn ok(&mut self, client: &mut Client, method: &str, params: Value) -> Value {
        let reply = self.call(client, method, params);
        assert!(reply.get("result").is_some(), "{method}: {reply}");
        reply["result"].clone()
    }

    /// A computer on the vehicle, by its id, checked unless `checked` is
    /// false.
    fn computer(&mut self, client: &mut Client, name: &str, checked: bool) -> String {
        let vehicle = self.ok(
            client,
            "computer/add",
            json!({ "vehicleId": "v", "host": format!("{}.local", name.to_lowercase().replace(' ', "-")), "name": name }),
        );
        let id = vehicle["companions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|computer| computer["name"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        if checked {
            self.ok(
                client,
                "computer/check",
                json!({ "vehicleId": "v", "computerId": id }),
            );
            self.engine.companion_checked(CheckAnswer {
                vehicle_id: "v".into(),
                computer_id: id.clone(),
                result: Ok(Found {
                    os: Some("Debian".into()),
                    arch: Some("aarch64".into()),
                    uxduck: None,
                    host_key: Some(PINNED.into()),
                    clock: None,
                    reports_as: None,
                    network: None,
                    cameras: None,
                    software: None,
                }),
            });
        }
        id
    }

    fn open(&mut self, client: &mut Client, computer: &str) -> (String, String) {
        let opened = self.ok(
            client,
            "terminal/open",
            json!({ "computerId": computer, "cols": 80, "rows": 24 }),
        );
        (
            opened["terminalId"].as_str().unwrap().to_owned(),
            opened["ticket"].as_str().unwrap().to_owned(),
        )
    }

    fn ends(&self) -> Vec<(TerminalEnd, Option<i32>)> {
        self.journal
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match &event.change {
                Change::TerminalEnded {
                    end, exit_status, ..
                } => Some((*end, *exit_status)),
                _ => None,
            })
            .collect()
    }
}

fn error(reply: &Value) -> (&str, &str) {
    (
        reply["error"]["data"]["kind"]
            .as_str()
            .unwrap_or_else(|| panic!("not an error: {reply}")),
        reply["error"]["message"].as_str().unwrap_or_default(),
    )
}

#[test]
fn a_terminal_opens_with_a_ticket_and_is_daemon_state_until_it_ends() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    let (terminal, ticket) = h.open(&mut cli, &computer);
    assert!(terminal.starts_with("tm") && ticket.starts_with("tk"));
    assert_eq!(
        h.ssh.take(),
        [Asked::Open {
            terminal: terminal.clone(),
            computer: computer.clone(),
            size: (80, 24),
            command: None,
        }]
    );
    // Listed with its computer, the client holding it and when it opened.
    let snapshot = h.ok(
        &mut cli,
        "state/subscribe",
        json!({ "entities": ["terminal"] }),
    );
    let listed = &snapshot["snapshot"]["terminals"];
    assert_eq!(listed.as_array().unwrap().len(), 1, "{snapshot}");
    assert_eq!(listed[0]["id"], terminal.as_str());
    assert_eq!(listed[0]["vehicleId"], "v");
    assert_eq!(listed[0]["computerId"], computer.as_str());
    assert_eq!(listed[0]["clientName"], "uxduck-cli");
    assert!(
        listed[0]["clientId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(listed[0]["kind"], "interactive");
    assert_eq!(listed[0]["openedAtUs"], NOW_US);

    // The socket lets its opener in and the session sizes with the window.
    assert_eq!(
        h.engine.terminal_attach(&ticket, &Trust::LocalUser),
        Ok(terminal.clone())
    );
    h.ok(
        &mut cli,
        "terminal/resize",
        json!({ "terminalId": terminal, "cols": 132, "rows": 41 }),
    );
    assert_eq!(h.ssh.take(), [Asked::Resize(terminal.clone(), 132, 41)]);

    // The remote shell exits 7: journaled with its status, and gone.
    h.engine
        .terminal_ended(&terminal, TerminalEnd::Exited, Some(7));
    assert!(h.engine.state().terminals.is_empty());
    assert_eq!(h.ends(), [(TerminalEnd::Exited, Some(7))]);
    let removed = cli
        .inbox
        .try_recv()
        .ok()
        .map(|text| serde_json::from_str::<Value>(&text).unwrap())
        .expect("a delta");
    assert_eq!(removed["params"]["changes"][0]["entity"], "terminal");
    assert_eq!(removed["params"]["changes"][0]["op"], "remove");
    // Its socket closing afterwards forgets it at the host and journals
    // nothing more.
    h.engine.terminal_socket_closed(&terminal);
    assert_eq!(h.ssh.take(), [Asked::Close(terminal, TerminalEnd::HungUp)]);
    assert_eq!(h.ends().len(), 1);
}

#[test]
fn the_ticket_works_once_and_only_for_the_client_that_opened_the_terminal() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);

    // Used twice: refused the second time.
    let (terminal, ticket) = h.open(&mut cli, &computer);
    assert_eq!(
        h.engine.terminal_attach(&ticket, &Trust::LocalUser),
        Ok(terminal)
    );
    assert_eq!(
        h.engine.terminal_attach(&ticket, &Trust::LocalUser),
        Err(USED.to_owned())
    );
    assert_eq!(
        h.engine.terminal_attach("tk-made-up", &Trust::LocalUser),
        Err(USED.to_owned())
    );

    // Presented by someone else, a page with this computer's own session
    // included: refused, and the terminal it would have opened is ended,
    // since its one ticket is spent.
    for other in [
        Trust::PairedBrowser {
            origin: "http://127.0.0.1:45000".into(),
        },
        Trust::NetworkBrowser {
            origin: PHONE_PAGE.into(),
            client: "phone".into(),
        },
    ] {
        let (terminal, ticket) = h.open(&mut cli, &computer);
        h.ssh.take();
        assert_eq!(
            h.engine.terminal_attach(&ticket, &other),
            Err(NOT_THE_OPENER.to_owned())
        );
        assert_eq!(
            h.ssh.take(),
            [Asked::Close(terminal, TerminalEnd::Unclaimed)]
        );
        assert_eq!(
            h.engine.terminal_attach(&ticket, &Trust::LocalUser),
            Err(USED.to_owned())
        );
    }

    // A page on this computer that opened one attaches as itself.
    let page = Trust::PairedBrowser {
        origin: "http://127.0.0.1:45000".into(),
    };
    let mut ui = h.connect(
        Transport::WebSocket,
        page.clone(),
        "uxduck-web",
        "operator-ui",
    );
    let (terminal, ticket) = h.open(&mut ui, &computer);
    assert_eq!(h.engine.terminal_attach(&ticket, &page), Ok(terminal));

    // A ticket whose opener has gone opens nothing.
    let mut leaving = h.cli();
    let (_, ticket) = h.open(&mut leaving, &computer);
    h.engine.disconnect(leaving.id);
    assert_eq!(
        h.engine.terminal_attach(&ticket, &Trust::LocalUser),
        Err(USED.to_owned())
    );
}

#[test]
fn a_ticket_nobody_uses_lapses_and_hangs_the_session_up() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    let (terminal, ticket) = h.open(&mut cli, &computer);
    h.ssh.take();
    h.engine.expire_terminal_tickets(NOW_US + 29_999_999);
    assert!(h.ssh.take().is_empty(), "not before 30 s");
    h.engine.expire_terminal_tickets(NOW_US + 30_000_000);
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(terminal, TerminalEnd::Unclaimed)]
    );
    assert!(h.engine.state().terminals.is_empty());
    assert_eq!(h.ends(), [(TerminalEnd::Unclaimed, None)]);
    assert!(
        h.engine
            .terminal_attach(&ticket, &Trust::LocalUser)
            .is_err()
    );

    // One that was attached to does not lapse.
    let (_, ticket) = h.open(&mut cli, &computer);
    h.engine
        .terminal_attach(&ticket, &Trust::LocalUser)
        .unwrap();
    h.ssh.take();
    h.engine.expire_terminal_tickets(NOW_US + 3_600_000_000);
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.engine.state().terminals.len(), 1);
}

/// ADR-0065 T7. Until S424 a phone and an agent were refused outright. Now
/// the person's own clients on this computer still open one unasked, an
/// agent is still refused, and a paired phone asks an operator.
#[test]
fn the_persons_own_clients_open_a_terminal_unasked_and_nobody_else_does() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    let (terminal, _) = h.open(&mut cli, &computer);
    h.ssh.take();
    assert_eq!(
        cli.granted,
        json!([
            "read", "control", "config", "approve", "location", "terminal"
        ])
    );

    // A phone on the network, paired to watch and fly.
    let (_, trust) = h.pair(&mut cli, "Mozilla/5.0 (iPhone)", "fly");
    let mut phone = h.device(&trust);
    // An agent on the local socket, as `uxduck mcp` connects one.
    let mut agent = h.connect(Transport::Socket, Trust::LocalUser, "Claude Code", "agent");

    // The agent is refused with a coded error, never held to be asked.
    for (method, params) in [
        (
            "terminal/open",
            json!({ "computerId": computer, "cols": 80, "rows": 24 }),
        ),
        (
            "terminal/resize",
            json!({ "terminalId": terminal, "cols": 80, "rows": 24 }),
        ),
        ("terminal/close", json!({ "terminalId": terminal })),
    ] {
        let reply = h.call(&mut agent, method, params);
        assert_eq!(
            error(&reply),
            ("capability-denied", NOT_FOR_AGENTS),
            "{method}"
        );
    }
    assert!(h.engine.state().approvals.is_empty());

    // The phone cannot touch a terminal it does not hold, and its own
    // request opens nothing by itself: it waits for an operator.
    for (method, params) in [
        (
            "terminal/resize",
            json!({ "terminalId": terminal, "cols": 80, "rows": 24 }),
        ),
        ("terminal/close", json!({ "terminalId": terminal })),
    ] {
        let reply = h.call(&mut phone, method, params);
        assert_eq!(
            error(&reply),
            ("capability-denied", NOT_THE_HOLDER),
            "{method}"
        );
    }
    let asked = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(asked).is_none(), "held, not answered");
    assert_eq!(h.pending().capability, Capability::Terminal);
    assert!(h.ssh.take().is_empty(), "nothing reached ssh");
    assert_eq!(h.engine.state().terminals.len(), 1);

    // The page of the browser paired on this computer is the person's own.
    let mut ui = h.page();
    h.open(&mut ui, &computer);
    // And any of them ends a terminal another holds (T4), which its holder
    // is told was an operator's doing.
    h.ok(&mut ui, "terminal/close", json!({ "terminalId": terminal }));
    assert!(
        h.ssh
            .take()
            .contains(&Asked::Close(terminal, TerminalEnd::EndedByOperator))
    );
    assert_eq!(h.ends(), [(TerminalEnd::EndedByOperator, None)]);
}

#[test]
fn missing_pieces_say_so_before_anything_runs() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let unchecked = h.computer(&mut cli, "Vision", false);
    let checked = h.computer(&mut cli, "Camera Pi", true);
    let open = |computer: &str| json!({ "computerId": computer, "cols": 80, "rows": 24 });

    let reply = h.call(&mut cli, "terminal/open", open(&unchecked));
    assert_eq!(
        error(&reply),
        (
            "invalid-value",
            "Vision has not been checked yet, so UXDuck does not know its host key. Check it first on Alpha's page."
        )
    );
    let reply = h.call(&mut cli, "terminal/open", open("cc-nope"));
    assert_eq!(
        error(&reply),
        ("not-found", "No companion computer has the id cc-nope.")
    );
    let reply = h.call(
        &mut cli,
        "terminal/open",
        json!({ "computerId": checked, "cols": 0, "rows": 24 }),
    );
    assert_eq!(
        error(&reply),
        (
            "invalid-value",
            "A terminal is at least one column wide and one row high."
        )
    );
    // No ssh on this computer: the host's own words, with how to get it.
    let missing = "UXDuck reaches companion computers with OpenSSH's ssh command, and it is not installed on this computer. Install your system's OpenSSH client package, such as openssh-client.";
    *h.ssh.missing.lock().unwrap() = Some(missing.to_owned());
    let reply = h.call(&mut cli, "terminal/open", open(&checked));
    assert_eq!(error(&reply), ("invalid-value", missing));
    assert!(h.ssh.take().is_empty(), "nothing ran");
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.ends().is_empty(), "and nothing was journaled");

    let reply = h.call(
        &mut cli,
        "terminal/close",
        json!({ "terminalId": "tm-nope" }),
    );
    assert_eq!(
        error(&reply),
        ("not-found", "No terminal with the id tm-nope is open.")
    );

    // A daemon with no ssh at all, such as UXDuck in a browser.
    let journal = Journal::default();
    let mut bare = Harness {
        engine: engine_with(h.engine.state().clone(), &journal),
        ssh: Ssh::default(),
        journal,
    };
    let mut cli = bare.cli();
    let reply = bare.call(&mut cli, "terminal/open", open(&checked));
    assert_eq!(
        error(&reply),
        (
            "invalid-value",
            "Use UXDuck Desktop or the uxduck daemon to open a terminal. They have SSH."
        )
    );
}

#[test]
fn a_terminal_has_no_arming_gate_no_fixed_number_and_no_wait_behind_a_check() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    // Armed (T8).
    h.observed(true);
    assert!(h.engine.state().vehicles["v"].armed);
    // And a check of the same computer under way, which holds its one
    // operation at a time (T12).
    h.ok(
        &mut cli,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": computer }),
    );
    let reply = h.call(
        &mut cli,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": computer }),
    );
    assert!(
        reply.get("error").is_some(),
        "the check is still running: {reply}"
    );
    let opened: Vec<String> = (0..40).map(|_| h.open(&mut cli, &computer).0).collect();
    assert_eq!(h.engine.state().terminals.len(), 40);
    assert_eq!(h.ssh.take().len(), 40);
    // They all stay open while it flies.
    h.observed(true);
    assert_eq!(h.engine.state().terminals.len(), opened.len());
}

#[test]
fn a_terminal_ends_with_its_socket_its_connection_and_the_daemon() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);

    // Its socket closes (T13).
    let (terminal, ticket) = h.open(&mut cli, &computer);
    h.engine
        .terminal_attach(&ticket, &Trust::LocalUser)
        .unwrap();
    h.ssh.take();
    h.engine.terminal_socket_closed(&terminal);
    assert_eq!(h.ssh.take(), [Asked::Close(terminal, TerminalEnd::HungUp)]);
    assert!(h.engine.state().terminals.is_empty());

    // The connection that opened it goes, with one attached to and one not.
    let mut other = h.cli();
    let (first, ticket) = h.open(&mut other, &computer);
    h.engine
        .terminal_attach(&ticket, &Trust::LocalUser)
        .unwrap();
    let (second, _) = h.open(&mut other, &computer);
    let (mine, _) = h.open(&mut cli, &computer);
    h.ssh.take();
    h.engine.disconnect(other.id);
    let mut closed = h.ssh.take();
    closed.sort_by_key(|asked| format!("{asked:?}"));
    let mut expected = vec![
        Asked::Close(first, TerminalEnd::HungUp),
        Asked::Close(second, TerminalEnd::HungUp),
    ];
    expected.sort_by_key(|asked| format!("{asked:?}"));
    assert_eq!(closed, expected);
    assert_eq!(
        h.engine.state().terminals.keys().collect::<Vec<_>>(),
        [&mine],
        "another client's stays"
    );

    // UXDuck stops: none is left signed in.
    h.engine.stop_terminals();
    assert_eq!(h.ssh.take(), [Asked::Close(mine, TerminalEnd::Stopped)]);
    assert!(h.engine.state().terminals.is_empty());
    assert_eq!(
        h.ends(),
        [
            (TerminalEnd::HungUp, None),
            (TerminalEnd::HungUp, None),
            (TerminalEnd::HungUp, None),
            (TerminalEnd::Stopped, None),
        ]
    );
}

#[test]
fn terminals_a_killed_daemon_left_open_are_ended_on_the_next_start() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    h.open(&mut cli, &computer);
    h.open(&mut cli, &computer);
    let left: State =
        serde_json::from_value(serde_json::to_value(h.engine.state()).unwrap()).unwrap();
    assert_eq!(left.terminals.len(), 2);
    let journal = Journal::default();
    let again = engine_with(left, &journal);
    assert!(again.state().terminals.is_empty());
    let ended: Vec<TerminalEnd> = journal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match &event.change {
            Change::TerminalEnded { end, .. } => Some(*end),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [TerminalEnd::Stopped, TerminalEnd::Stopped]);
}

#[test]
fn the_journal_the_tap_and_sync_keep_who_and_when_and_never_the_command() {
    let mut h = Harness::new();
    let mut cli = h.cli();
    let computer = h.computer(&mut cli, "Camera Pi", true);
    let mut tracer = h.cli();
    h.ok(&mut tracer, "trace/subscribe", json!({}));
    while tracer.inbox.try_recv().is_ok() {}

    let secret = "journalctl -u mediamtx --marker-4f1d2c";
    let opened = h.ok(
        &mut cli,
        "terminal/open",
        json!({ "computerId": computer, "cols": 80, "rows": 24, "command": secret }),
    );
    let ticket = opened["ticket"].as_str().unwrap().to_owned();
    let terminal = opened["terminalId"].as_str().unwrap().to_owned();
    // The command reaches ssh, and nothing else.
    assert_eq!(
        h.ssh.take(),
        [Asked::Open {
            terminal: terminal.clone(),
            computer: computer.clone(),
            size: (80, 24),
            command: Some(secret.to_owned()),
        }]
    );
    h.engine
        .terminal_ended(&terminal, TerminalEnd::Exited, Some(0));

    // The journal: that it opened, who, which computer, when and how, and
    // how it ended.
    let events = h.journal.0.lock().unwrap().clone();
    let (opened_event, terminal_opened) = events
        .iter()
        .find_map(|event| match &event.change {
            Change::TerminalOpened { terminal } => Some((event, terminal.clone())),
            _ => None,
        })
        .expect("journaled");
    assert_eq!(terminal_opened.computer_id, computer);
    assert_eq!(terminal_opened.vehicle_id, "v");
    assert_eq!(terminal_opened.client_name, "uxduck-cli");
    assert_eq!(terminal_opened.kind, TerminalKind::Command);
    assert_eq!(terminal_opened.opened_at_us, NOW_US);
    assert!(
        !opened_event.actor.client_id.is_empty() && !terminal_opened.client_id.is_empty(),
        "the connection that did it, and its record in the client list"
    );
    assert_eq!(h.ends(), [(TerminalEnd::Exited, Some(0))]);
    let journal = serde_json::to_string(&events).unwrap();
    let state = serde_json::to_string(h.engine.state()).unwrap();
    for kept in [&journal, &state] {
        assert!(
            !kept.contains("marker-4f1d2c"),
            "the command is kept nowhere"
        );
        assert!(!kept.contains("journalctl"));
        assert!(!kept.contains(&ticket), "nor the ticket");
    }

    // The inspector's tap: the method and the computer, with the command
    // and the ticket redacted.
    let mut frames = Vec::new();
    while let Ok(text) = tracer.inbox.try_recv() {
        frames.push(serde_json::from_str::<Value>(&text).unwrap());
    }
    let tapped = serde_json::to_string(&frames).unwrap();
    assert!(
        !tapped.contains("marker-4f1d2c") && !tapped.contains("journalctl"),
        "{tapped}"
    );
    assert!(!tapped.contains(&ticket), "{tapped}");
    let request = frames
        .iter()
        .find(|frame| {
            frame["params"]["method"] == "terminal/open" && frame["params"]["direction"] == "in"
        })
        .expect("the request is tapped");
    assert_eq!(
        request["params"]["payload"]["params"],
        json!({ "computerId": computer, "cols": 80, "rows": 24, "command": "[redacted]" })
    );
    assert!(
        tapped.contains(&terminal),
        "the answer's terminal id is not a secret"
    );

    // Sync sends nothing of a terminal.
    for event in &events {
        if matches!(
            event.change,
            Change::TerminalOpened { .. } | Change::TerminalEnded { .. }
        ) {
            assert!(uxduck_engine::sync::exportable(event).is_none());
        }
    }
}

// S424, ADR-0065 T7: a terminal from another computer, with an operator's
// approval.

/// A daemon with a checked Camera Pi, an operator's page that shows
/// approvals, and a phone paired to watch and fly: its page, its record and
/// how the daemon knows its sockets.
fn with_phone() -> (Harness, Client, String, Client, String, Trust) {
    let mut h = Harness::new();
    let mut ui = h.page();
    let computer = h.computer(&mut ui, "Camera Pi", true);
    let (record, trust) = h.pair(&mut ui, "Mozilla/5.0 (iPhone)", "fly");
    let phone = h.device(&trust);
    ui.arrived();
    (h, ui, computer, phone, record, trust)
}

#[test]
fn pairing_grants_no_terminal_whatever_was_chosen() {
    let mut h = Harness::new();
    let mut ui = h.page();
    let computer = h.computer(&mut ui, "Camera Pi", true);
    for (agent, access, granted) in [
        ("Mozilla/5.0 (iPhone)", "watch", json!(["read"])),
        ("Mozilla/5.0 (iPad)", "fly", json!(["read", "control"])),
    ] {
        let (record, trust) = h.pair(&mut ui, agent, access);
        let mut device = h.device(&trust);
        assert_eq!(device.granted, granted, "{access}");
        assert!(
            !h.engine.state().clients[&record]
                .capabilities
                .contains(&Capability::Terminal),
            "{access}"
        );
        // So its terminal waits for an operator, and opens nothing.
        let asked = h.ask(&mut device, &computer);
        assert!(device.answer_to(asked).is_none(), "{access}");
        assert!(h.ssh.take().is_empty(), "{access}");
        h.answer(&mut ui, false, "once");
    }
}

#[test]
fn a_terminal_from_another_computer_is_held_and_asked_in_the_operators_words() {
    let (mut h, mut ui, computer, mut phone, record, _) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(asked).is_none(), "held");
    assert!(h.ssh.take().is_empty(), "nothing runs before the answer");
    assert!(h.engine.state().terminals.is_empty());

    // The approval names the client, the vehicle and the computer, and
    // nothing else (T9).
    let approval = h.pending();
    assert_eq!(approval.client_id, record);
    assert_eq!(approval.client_name, PHONE);
    assert_eq!(approval.capability, Capability::Terminal);
    assert_eq!(approval.method, "terminal/open");
    assert_eq!(approval.summary, "open a terminal on Alpha › Camera Pi");
    assert_eq!(
        approval.question.as_deref(),
        Some("iPhone (192.168.1.31) wants a terminal on Alpha › Camera Pi")
    );
    assert_eq!(approval.deadline_us, NOW_US + DEADLINE_US);
    let notice = h
        .engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some(format!("approval:{}", approval.id).as_str()))
        .cloned()
        .expect("a notice");
    assert_eq!(
        notice.title,
        "iPhone (192.168.1.31) wants a terminal on Alpha › Camera Pi"
    );
    assert_eq!(
        notice.body.as_deref(),
        Some(
            "A terminal signs in to Camera Pi over SSH, as UXDuck does to check it. Not allowed in 60 s if nobody answers."
        )
    );
    let labels: Vec<&str> = notice.actions.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Allow once",
            "Allow for this connection",
            "Always allow",
            "Deny"
        ]
    );
    // The operator's page is told.
    let told = ui.arrived();
    assert!(
        told.iter()
            .any(|frame| frame["method"] == "approval/requested"
                && frame["params"]["id"] == approval.id.as_str()),
        "{told:?}"
    );
}

#[test]
fn allow_once_opens_this_terminal_and_the_next_one_asks_again() {
    let (mut h, mut ui, computer, mut phone, record, trust) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "once");
    let (terminal, ticket) = opened(&phone.answer_to(asked).expect("answered"));
    assert_eq!(
        h.ssh.take(),
        [Asked::Open {
            terminal: terminal.clone(),
            computer: computer.clone(),
            size: (80, 24),
            command: None,
        }]
    );
    // Held by the phone's record, under the name the client list shows.
    let held = &h.engine.state().terminals[&terminal];
    assert_eq!(
        (held.client_id.as_str(), held.client_name.as_str()),
        (record.as_str(), PHONE)
    );
    assert_eq!(h.engine.terminal_attach(&ticket, &trust), Ok(terminal));
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Once))]
    );

    // Once is once: nothing was stored, and the next one waits again.
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    let again = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(again).is_none());
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.pending().capability, Capability::Terminal);
}

#[test]
fn allow_for_this_connection_lasts_as_long_as_the_connection() {
    let (mut h, mut ui, computer, mut phone, record, trust) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "connection");
    let frames = phone.arrived();
    // The page is told what it may do now, and the terminal opens.
    let told = frames
        .iter()
        .find(|frame| frame["method"] == "client/capabilitiesChanged")
        .expect("told");
    assert_eq!(
        told["params"]["granted"],
        json!(["read", "control", "terminal"])
    );
    opened(
        frames
            .iter()
            .find(|frame| frame["id"] == asked)
            .expect("answered"),
    );
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Connection))]
    );

    // The same connection opens another unasked.
    h.open(&mut phone, &computer);
    assert!(h.engine.state().approvals.is_empty());
    // Nothing was stored: another connection of the same device asks, and
    // so does this device once it has reconnected.
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    let mut second = h.device(&trust);
    assert_eq!(second.granted, json!(["read", "control"]));
    let asked = h.ask(&mut second, &computer);
    assert!(second.answer_to(asked).is_none());
    h.answer(&mut ui, false, "once");
    h.engine.disconnect(phone.id);
    let mut back = h.device(&trust);
    let asked = h.ask(&mut back, &computer);
    assert!(back.answer_to(asked).is_none());
    assert_eq!(h.pending().client_id, record);
}

#[test]
fn always_allow_stores_the_capability_and_the_next_terminal_does_not_ask() {
    let (mut h, mut ui, computer, mut phone, record, trust) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "always");
    opened(&phone.answer_to(asked).expect("answered"));
    assert_eq!(
        h.engine.state().clients[&record].capabilities,
        [Capability::Read, Capability::Control, Capability::Terminal]
    );
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Always))]
    );
    // Journaled in the record, so it outlives the daemon.
    assert!(h.journal.0.lock().unwrap().iter().any(|event| matches!(
        &event.change,
        Change::ClientUpserted { client }
            if client.id == record && client.capabilities.contains(&Capability::Terminal)
    )));

    // The next one opens unasked, on this connection and on a new one.
    h.open(&mut phone, &computer);
    let mut later = h.device(&trust);
    assert_eq!(later.granted, json!(["read", "control", "terminal"]));
    h.open(&mut later, &computer);
    assert!(h.engine.state().approvals.is_empty());
    // It is still not an operator: it ends only its own terminals.
    let mut cli = h.cli();
    let (other, _) = h.open(&mut cli, &computer);
    let reply = h.call(&mut later, "terminal/close", json!({ "terminalId": other }));
    assert_eq!(error(&reply), ("capability-denied", NOT_THE_HOLDER));
}

#[test]
fn a_denied_terminal_opens_nothing_and_says_so_plainly() {
    let (mut h, mut ui, computer, mut phone, record, _) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, false, "once");
    let reply = phone.answer_to(asked).expect("answered");
    assert_eq!(error(&reply), ("capability-denied", NOT_ALLOWED));
    assert!(h.ssh.take().is_empty());
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.engine.state().approvals.is_empty());
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    assert_eq!(h.answers(), [(ApprovalState::Denied, None)]);
    // And its notice is gone from the operator's page.
    assert!(h.engine.state().notifications.values().all(|n| {
        !n.condition
            .as_deref()
            .unwrap_or_default()
            .starts_with("approval:")
    }));
}

#[test]
fn with_no_operator_there_a_terminal_is_refused_at_the_deadline() {
    let (mut h, ui, computer, mut phone, _, _) = with_phone();
    // Nobody who could answer is connected.
    h.engine.disconnect(ui.id);
    let asked = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(asked).is_none(), "held until the deadline");
    assert_eq!(h.pending().capability, Capability::Terminal);

    h.engine.expire_approvals(NOW_US + DEADLINE_US - 1);
    assert!(phone.answer_to(asked).is_none(), "not before 60 s");
    h.engine.expire_approvals(NOW_US + DEADLINE_US);
    let reply = phone.answer_to(asked).expect("answered at the deadline");
    assert_eq!(error(&reply), ("capability-denied", NOBODY_ANSWERED));
    assert!(h.ssh.take().is_empty());
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.engine.state().approvals.is_empty());
    // Journaled, with the safe default.
    assert_eq!(h.answers(), [(ApprovalState::Expired, None)]);
}

#[test]
fn the_asking_client_never_sees_or_answers_its_own_request() {
    let (mut h, mut ui, computer, mut phone, record, _) = with_phone();
    // Both pages follow approvals and notices, as UXDuck's page does.
    for client in [&mut ui, &mut phone] {
        h.ok(
            client,
            "state/subscribe",
            json!({ "entities": ["approval", "notification"] }),
        );
    }
    let asked = h.ask(&mut phone, &computer);
    let approval = h.pending();

    // The operator's page gets the approval and its notice.
    let shown = serde_json::to_string(&ui.arrived()).unwrap();
    assert!(shown.contains(&approval.id), "{shown}");
    assert!(shown.contains("wants a terminal"), "{shown}");
    // The phone gets neither: not as it happens, and not when it asks for
    // the state again or lists approvals.
    let seen = serde_json::to_string(&phone.arrived()).unwrap();
    assert!(!seen.contains(&approval.id), "{seen}");
    assert!(!seen.contains("wants a terminal"), "{seen}");
    let again = h.ok(
        &mut phone,
        "state/subscribe",
        json!({ "entities": ["approval", "notification"] }),
    );
    assert_eq!(again["snapshot"]["approvals"], json!([]), "{again}");
    assert_eq!(again["snapshot"]["notifications"], json!([]), "{again}");

    // Even allowed to answer approvals, it cannot answer its own.
    let version = h.engine.state().clients[&record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record, "expectedVersion": version, "capabilities": ["read", "control", "approve"] }),
    );
    let listed = h.ok(&mut phone, "approval/list", json!({}));
    assert_eq!(listed["approvals"], json!([]), "{listed}");
    let reply = h.call(
        &mut phone,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert_eq!(
        error(&reply),
        (
            "capability-denied",
            "a client cannot answer its own approval"
        )
    );
    // Nor can another device that was allowed to answer approvals: a
    // terminal is an operator's to allow.
    let (tablet_record, tablet_trust) = h.pair(&mut ui, "Mozilla/5.0 (iPad)", "fly");
    let version = h.engine.state().clients[&tablet_record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": tablet_record, "expectedVersion": version, "capabilities": ["read", "control", "approve"] }),
    );
    let mut tablet = h.device(&tablet_trust);
    let reply = h.call(
        &mut tablet,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert_eq!(error(&reply), ("capability-denied", OPERATOR_ANSWERS));
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.pending().id, approval.id, "still waiting");
    assert!(phone.answer_to(asked).is_none());

    // When it is answered the phone gets its answer and still no approval.
    h.answer(&mut ui, false, "once");
    let frames = phone.arrived();
    let seen = serde_json::to_string(&frames).unwrap();
    assert!(!seen.contains("wants a terminal"), "{seen}");
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "approval/resolved"),
        "{seen}"
    );
    let reply = frames.iter().find(|frame| frame["id"] == asked).unwrap();
    assert_eq!(error(reply), ("capability-denied", NOT_ALLOWED));
}

#[test]
fn taking_terminal_away_ends_that_clients_terminals_at_once() {
    let (mut h, mut ui, computer, mut phone, record, trust) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "always");
    let (first, _) = opened(&phone.answer_to(asked).expect("answered"));
    let (second, _) = h.open(&mut phone, &computer);
    // Someone else's terminal, which stays.
    let mut cli = h.cli();
    let (mine, _) = h.open(&mut cli, &computer);
    h.ssh.take();
    phone.arrived();

    let version = h.engine.state().clients[&record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record, "expectedVersion": version, "capabilities": ["read", "control"] }),
    );
    let mut closed = h.ssh.take();
    closed.sort_by_key(|asked| format!("{asked:?}"));
    let mut expected = vec![
        Asked::Close(first, TerminalEnd::CapabilityRemoved),
        Asked::Close(second, TerminalEnd::CapabilityRemoved),
    ];
    expected.sort_by_key(|asked| format!("{asked:?}"));
    assert_eq!(closed, expected);
    assert_eq!(
        h.engine.state().terminals.keys().collect::<Vec<_>>(),
        [&mine]
    );
    assert_eq!(
        h.ends(),
        [
            (TerminalEnd::CapabilityRemoved, None),
            (TerminalEnd::CapabilityRemoved, None)
        ]
    );
    // The page is told, and its next terminal asks again.
    let told = phone.arrived();
    assert!(
        told.iter()
            .any(|frame| frame["method"] == "client/capabilitiesChanged"
                && frame["params"]["granted"] == json!(["read", "control"])),
        "{told:?}"
    );
    let asked = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(asked).is_none());
    h.answer(&mut ui, true, "connection");
    let (third, _) = opened(&phone.answer_to(asked).expect("answered"));
    h.ssh.take();

    // What was allowed for a connection is taken away by an edit too.
    let version = h.engine.state().clients[&record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record, "expectedVersion": version, "capabilities": ["read"] }),
    );
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(third, TerminalEnd::CapabilityRemoved)]
    );
    let asked = h.ask(&mut phone, &computer);
    assert!(phone.answer_to(asked).is_none(), "it asks again");
    h.answer(&mut ui, true, "once");
    let (once, _) = opened(&phone.answer_to(asked).expect("answered"));
    h.ssh.take();

    // An edit that takes no terminal away ends none: this one was allowed
    // once, and the device never held the capability since.
    let version = h.engine.state().clients[&record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record, "expectedVersion": version, "capabilities": ["read", "control"] }),
    );
    assert!(h.ssh.take().is_empty());
    assert!(h.engine.state().terminals.contains_key(&once));
    // Forgetting the device disconnects it, which ends what it held (T13).
    h.ok(&mut ui, "client/forget", json!({ "id": record }));
    assert_eq!(h.ssh.take(), [Asked::Close(once, TerminalEnd::HungUp)]);
    let _ = trust;
}

#[test]
fn an_operator_ends_any_terminal_and_its_holder_is_told_so() {
    let (mut h, mut ui, computer, mut phone, _, _) = with_phone();
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "connection");
    let (terminal, _) = opened(&phone.answer_to(asked).expect("answered"));
    h.ssh.take();

    // The open terminal is listed for every page with its holder's name.
    let snapshot = h.ok(
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["terminal"] }),
    );
    let listed = &snapshot["snapshot"]["terminals"];
    assert_eq!(listed[0]["id"], terminal.as_str());
    assert_eq!(listed[0]["clientName"], PHONE);
    assert_eq!(listed[0]["openedAtUs"], NOW_US);

    // An operator ends it: the host tells the phone's socket whose doing.
    h.ok(&mut ui, "terminal/close", json!({ "terminalId": terminal }));
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(terminal, TerminalEnd::EndedByOperator)]
    );
    assert_eq!(
        TerminalEnd::EndedByOperator.told(),
        Some("Ended by the operator.")
    );
    assert!(TerminalEnd::CapabilityRemoved.told().is_some());
    assert_eq!(TerminalEnd::Closed.told(), None);
    assert!(h.engine.state().terminals.is_empty());

    // The holder closing its own is a plain close, and it resizes its own.
    let (own, _) = h.open(&mut phone, &computer);
    h.ssh.take();
    h.ok(
        &mut phone,
        "terminal/resize",
        json!({ "terminalId": own, "cols": 100, "rows": 30 }),
    );
    h.ok(&mut phone, "terminal/close", json!({ "terminalId": own }));
    assert_eq!(
        h.ssh.take(),
        [
            Asked::Resize(own.clone(), 100, 30),
            Asked::Close(own, TerminalEnd::Closed)
        ]
    );
    assert_eq!(
        h.ends(),
        [
            (TerminalEnd::EndedByOperator, None),
            (TerminalEnd::Closed, None)
        ]
    );
}

#[test]
fn a_ticket_stolen_by_another_paired_device_opens_nothing_and_ends_the_session() {
    let (mut h, mut ui, computer, mut phone, _, trust) = with_phone();
    // A second device, paired and connected through the same listener.
    let (_, thief) = h.pair(&mut ui, "Mozilla/5.0 (iPad)", "fly");
    let _tablet = h.device(&thief);
    assert_ne!(trust, thief);

    let Trust::NetworkBrowser { client, .. } = &trust else {
        unreachable!()
    };
    // The same record from another site is not that client either, nor is
    // a program on this computer.
    let elsewhere = Trust::NetworkBrowser {
        origin: "http://192.168.1.99:45000".into(),
        client: client.clone(),
    };
    for other in [thief, elsewhere, Trust::LocalUser] {
        let asked = h.ask(&mut phone, &computer);
        h.answer(&mut ui, true, "once");
        let (terminal, ticket) = opened(&phone.answer_to(asked).expect("answered"));
        h.ssh.take();
        assert_eq!(
            h.engine.terminal_attach(&ticket, &other),
            Err(NOT_THE_OPENER.to_owned()),
            "{other:?}"
        );
        // Its one ticket is spent: the session is ended, and not even its
        // opener attaches now.
        assert_eq!(
            h.ssh.take(),
            [Asked::Close(terminal, TerminalEnd::Unclaimed)]
        );
        assert!(h.engine.state().terminals.is_empty());
        assert_eq!(
            h.engine.terminal_attach(&ticket, &trust),
            Err(USED.to_owned())
        );
    }

    // The phone itself attaches to its own, once.
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "once");
    let (terminal, ticket) = opened(&phone.answer_to(asked).expect("answered"));
    assert_eq!(h.engine.terminal_attach(&ticket, &trust), Ok(terminal));
    assert_eq!(
        h.engine.terminal_attach(&ticket, &trust),
        Err(USED.to_owned())
    );
    // A ticket whose opener has gone opens nothing.
    let asked = h.ask(&mut phone, &computer);
    h.answer(&mut ui, true, "once");
    let (_, ticket) = opened(&phone.answer_to(asked).expect("answered"));
    h.engine.disconnect(phone.id);
    assert_eq!(
        h.engine.terminal_attach(&ticket, &trust),
        Err(USED.to_owned())
    );
}

#[test]
fn an_agent_is_refused_whatever_it_was_allowed_and_asking_runs_no_command() {
    let (mut h, mut ui, computer, mut phone, _, _) = with_phone();
    let mut agent = h.connect(Transport::Socket, Trust::LocalUser, "Claude Code", "agent");
    // Even holding the capability, as an operator's edit could give it.
    let record = h
        .engine
        .state()
        .clients
        .values()
        .find(|client| client.name == "Claude Code")
        .cloned()
        .expect("a record");
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record.id, "expectedVersion": record.version, "capabilities": ["read", "control", "config", "terminal"] }),
    );
    let reply = h.call(
        &mut agent,
        "terminal/open",
        json!({ "computerId": computer, "cols": 80, "rows": 24 }),
    );
    assert_eq!(error(&reply), ("capability-denied", NOT_FOR_AGENTS));
    assert!(h.engine.state().approvals.is_empty(), "never asked for");
    assert!(h.ssh.take().is_empty());

    // A client that asks cannot have one command run: refused, not held.
    let reply = h.call(
        &mut phone,
        "terminal/open",
        json!({ "computerId": computer, "cols": 80, "rows": 24, "command": "id" }),
    );
    assert_eq!(error(&reply), ("capability-denied", COMMAND_ONLY_HERE));
    // Nor is an operator asked to allow a terminal that could not open.
    let unchecked = h.computer(&mut ui, "Vision", false);
    let reply = h.call(
        &mut phone,
        "terminal/open",
        json!({ "computerId": unchecked, "cols": 80, "rows": 24 }),
    );
    assert_eq!(
        error(&reply),
        (
            "invalid-value",
            "Vision has not been checked yet, so UXDuck does not know its host key. Check it first on Alpha's page."
        )
    );
    let reply = h.call(
        &mut phone,
        "terminal/open",
        json!({ "computerId": "cc-nothing", "cols": 80, "rows": 24 }),
    );
    assert_eq!(error(&reply).0, "not-found");
    assert!(h.engine.state().approvals.is_empty());
    assert!(h.ssh.take().is_empty());

    // Any other client that is not the person's own asks, as the phone
    // does: a third party's program on this computer.
    let mut tool = h.connect(
        Transport::Socket,
        Trust::LocalUser,
        "ground-tool",
        "third-party",
    );
    let asked = h.ask(&mut tool, &computer);
    assert!(tool.answer_to(asked).is_none());
    assert_eq!(
        h.pending().question.as_deref(),
        Some("ground-tool wants a terminal on Alpha › Camera Pi")
    );
}

// S425, ADR-0065 T14: an agent runs one command on a companion computer.

const AGENT: &str = "Claude Code";
const RUN_QUESTION: &str = "Claude Code wants to run a command on Alpha › Camera Pi";
const RUN_BODY: &str = "Read all of the command first. It runs as Camera Pi's SSH user, and what it prints goes to Claude Code and its AI model. Allow for this connection and Always allow also let Claude Code run later commands on any companion computer without showing them to you first. Each one is listed on its computer's card. Not allowed in 60 s if nobody answers.";
const RUN_NOT_ALLOWED: &str = "The operator did not allow this command, so it was not run. Tell the person, and do not ask again unless they ask.";
const RUN_NOBODY: &str = "Nobody answered on the computer UXDuck runs on in time, so the command was not run. Tell the person, and do not ask again unless they ask.";
const RUN_FOR_AGENTS: &str = "Only an agent, such as the assistant or an MCP client, runs a command with computer/run. A person opens a terminal on the computer instead.";

/// All of one stream, as the host keeps it when it fits.
fn whole(bytes: &[u8]) -> Collected {
    Collected {
        first: bytes.to_vec(),
        last: Vec::new(),
        bytes: bytes.len() as u64,
    }
}

/// The host's report on a command that exited on its own.
fn printed(stdout: &str, stderr: &str, status: i32) -> RunReport {
    RunReport {
        stdout: whole(stdout.as_bytes()),
        stderr: whole(stderr.as_bytes()),
        own: Some((TerminalEnd::Exited, Some(status), None)),
    }
}

/// The host's report on a command the engine ended.
fn cut_short(stdout: &str) -> RunReport {
    RunReport {
        stdout: whole(stdout.as_bytes()),
        stderr: Collected::default(),
        own: None,
    }
}

impl Harness {
    /// An agent outside UXDuck, as `uxduck mcp` connects one.
    fn agent(&mut self) -> Client {
        self.connect(Transport::Socket, Trust::LocalUser, AGENT, "agent")
    }

    /// An operator sets what a client may do, as in Settings.
    fn set_capabilities(&mut self, operator: &mut Client, name: &str, capabilities: Value) {
        let (id, version) = {
            let record = self
                .engine
                .state()
                .clients
                .values()
                .find(|client| client.name == name)
                .expect("a record");
            (record.id.clone(), json!(record.version))
        };
        self.ok(
            operator,
            "client/setCapabilities",
            json!({ "id": id, "expectedVersion": version, "capabilities": capabilities }),
        );
    }

    /// Asks to run a command and leaves the answer to arrive.
    fn run(&mut self, client: &mut Client, computer: &str, command: &str) -> i64 {
        self.send(
            client,
            "computer/run",
            json!({ "computerId": computer, "command": command }),
        )
    }

    /// The one command the engine asked `ssh` to run since the last look,
    /// by its session's id: this command exactly, collected within the
    /// bounds of a tool answer.
    fn started(&self, command: &str) -> String {
        let asked = self.ssh.take();
        match asked.as_slice() {
            [
                Asked::Run {
                    terminal,
                    command: ran,
                    keeps,
                    ..
                },
            ] => {
                assert_eq!(ran, command);
                assert_eq!(*keeps, RUN_KEEPS);
                terminal.clone()
            }
            other => panic!("not one run: {other:?}"),
        }
    }

    /// Every state of every run's record, in the journal's order.
    fn records(&self) -> Vec<ComputerRun> {
        self.journal
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match &event.change {
                Change::ComputerRunUpserted { run } => Some(run.clone()),
                _ => None,
            })
            .collect()
    }
}

/// A daemon with a checked Camera Pi, an operator's page that shows
/// approvals, and an agent connected that was allowed nothing yet: its
/// connection and its record.
fn with_agent() -> (Harness, Client, String, Client, String) {
    let mut h = Harness::new();
    let mut ui = h.page();
    let computer = h.computer(&mut ui, "Camera Pi", true);
    let agent = h.agent();
    let record = h
        .engine
        .state()
        .clients
        .values()
        .find(|client| client.name == AGENT)
        .expect("a record")
        .id
        .clone();
    ui.arrived();
    (h, ui, computer, agent, record)
}

/// As `with_agent`, with the agent always allowed, as "Always allow" or
/// Settings leaves it.
fn with_allowed_agent() -> (Harness, Client, String, Client, String) {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    h.set_capabilities(&mut ui, AGENT, json!(["read", "terminal"]));
    agent.arrived();
    ui.arrived();
    (h, ui, computer, agent, record)
}

#[test]
fn an_agents_command_is_held_and_the_operator_is_shown_the_exact_command() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let command = "systemctl status mediamtx --no-pager | tail -n 20";
    let asked = h.run(&mut agent, &computer, command);
    assert!(agent.answer_to(asked).is_none(), "held");
    assert!(h.ssh.take().is_empty(), "nothing runs before the answer");
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.engine.state().computer_runs.is_empty());

    // The approval names the agent, the vehicle and the computer, and
    // holds the command exactly as it was given.
    let approval = h.pending();
    assert_eq!(approval.client_id, record);
    assert_eq!(approval.client_name, AGENT);
    assert_eq!(approval.capability, Capability::Terminal);
    assert_eq!(approval.method, "computer/run");
    assert_eq!(approval.summary, "run a command on Alpha › Camera Pi");
    assert_eq!(approval.question.as_deref(), Some(RUN_QUESTION));
    assert_eq!(approval.shell_command.as_deref(), Some(command));
    assert_eq!(approval.deadline_us, NOW_US + DEADLINE_US);

    // Its notice says where what the command prints goes, and what the
    // two lasting answers mean. The command is not in its words, which a
    // feedback bundle carries: it is the approval's own.
    let notice = h
        .engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some(format!("approval:{}", approval.id).as_str()))
        .cloned()
        .expect("a notice");
    assert_eq!(notice.title, RUN_QUESTION);
    assert_eq!(notice.body.as_deref(), Some(RUN_BODY));
    let words = serde_json::to_string(&notice).unwrap();
    assert!(!words.contains("systemctl"), "{words}");
    let labels: Vec<&str> = notice.actions.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Allow once",
            "Allow for this connection",
            "Always allow",
            "Deny"
        ]
    );

    // The operator's page is given the command as a person reads it, and
    // never the raw one beside it.
    let told = ui.arrived();
    let requested = told
        .iter()
        .find(|frame| {
            frame["method"] == "approval/requested" && frame["params"]["id"] == approval.id.as_str()
        })
        .unwrap_or_else(|| panic!("{told:?}"));
    assert_eq!(requested["params"]["shellCommandShown"], command);
    assert_eq!(requested["params"]["clientName"], AGENT);
    assert!(requested["params"].get("shellCommand").is_none());
    let listed = h.ok(&mut ui, "approval/list", json!({}));
    assert_eq!(listed["approvals"][0]["shellCommandShown"], command);
}

#[test]
fn nothing_in_a_command_hides_from_the_operator() {
    let (mut h, mut ui, computer, mut agent, _) = with_agent();
    let harm = "rm -rf /home/pi/flights";
    let padded = format!("echo {} ; {harm}", "a".repeat(2000));
    let cases = [
        // After 2,000 characters of padding: shown whole, as it is.
        (padded.clone(), padded),
        // After a newline, which a one-line field would drop or fold.
        (format!("uptime\n{harm}"), format!("uptime⏎\n{harm}")),
        // After a carriage return, which a terminal draws over the start.
        (format!("uptime\r{harm}"), format!("uptime⟨U+000D⟩{harm}")),
        // Inside a right-to-left override, which draws it backwards.
        (
            format!("true \u{202e}; {harm} #\u{202c}"),
            format!("true ⟨U+202E⟩; {harm} #⟨U+202C⟩"),
        ),
        // A tab, an escape sequence that hides text, and a zero-width
        // space inside a word.
        (
            format!("ls\t\u{1b}[8m; {harm}\u{1b}[0m r\u{200b}m"),
            format!("ls⇥⟨U+001B⟩[8m; {harm}⟨U+001B⟩[0m r⟨U+200B⟩m"),
        ),
    ];
    for (command, shown) in cases {
        let asked = h.run(&mut agent, &computer, &command);
        let approval = h.pending();
        // The data keeps the command exactly, to the last character.
        assert_eq!(approval.shell_command.as_deref(), Some(command.as_str()));
        // What a page is given holds every character of it, each one
        // visible, with the harmful part readable where it is.
        let told = ui.arrived();
        let requested = told
            .iter()
            .find(|frame| frame["method"] == "approval/requested")
            .unwrap_or_else(|| panic!("{told:?}"));
        let given = requested["params"]["shellCommandShown"].as_str().unwrap();
        assert_eq!(given, shown);
        assert!(given.contains(harm), "{given}");
        assert!(
            given.chars().count() >= command.chars().count(),
            "nothing shortened"
        );
        assert!(
            !given.chars().any(|c| c != '\n'
                && (c.is_control()
                    || ('\u{2000}'..='\u{200f}').contains(&c)
                    || ('\u{2028}'..='\u{202f}').contains(&c)
                    || ('\u{2060}'..='\u{206f}').contains(&c))),
            "nothing unseen is left in it: {given:?}"
        );
        h.answer(&mut ui, false, "once");
        let reply = agent.answer_to(asked).expect("answered");
        assert_eq!(error(&reply), ("capability-denied", RUN_NOT_ALLOWED));
        ui.arrived();
    }
    assert!(h.ssh.take().is_empty());
}

#[test]
fn allow_once_runs_this_command_and_the_next_one_asks_again() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let asked = h.run(&mut agent, &computer, "uname -a");
    h.answer(&mut ui, true, "once");
    assert!(
        agent.answer_to(asked).is_none(),
        "answered when the command ends"
    );
    let terminal = h.started("uname -a");

    // It runs in a session of its own kind, held by the agent, which the
    // open terminals list for every operator.
    let held = h.engine.state().terminals[&terminal].clone();
    assert_eq!(held.kind, TerminalKind::Run);
    assert_eq!(
        (held.client_id.as_str(), held.client_name.as_str()),
        (record.as_str(), AGENT)
    );
    assert_eq!(held.computer_id, computer);
    // And its record says the operator was shown it first.
    let run = h.engine.state().computer_runs[&terminal].clone();
    assert_eq!(run.shell_command, "uname -a");
    assert!(run.shown_first);
    assert_eq!((run.end, run.deadline_s), (None, RUN_DEADLINE_S));

    h.engine
        .computer_ran(&terminal, printed("Linux camera-pi 6.6\n", "", 0));
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(
        reply["result"],
        json!({
            "runId": terminal,
            "ended": "exited",
            "exitStatus": 0,
            "durationMs": 0,
            "stdout": "Linux camera-pi 6.6\n",
            "stderr": "",
            "stdoutBytes": 20,
            "stderrBytes": 0
        })
    );
    assert!(h.engine.state().terminals.is_empty());
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Once))]
    );

    // Once is once: nothing was stored, and the next one waits again.
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    let again = h.run(&mut agent, &computer, "uptime");
    assert!(agent.answer_to(again).is_none());
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.pending().shell_command.as_deref(), Some("uptime"));
}

#[test]
fn allow_for_this_connection_runs_later_commands_unshown_and_records_each() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let asked = h.run(&mut agent, &computer, "uptime");
    h.answer(&mut ui, true, "connection");
    let first = h.started("uptime");
    h.engine.computer_ran(&first, printed("up 3 days\n", "", 0));
    assert_eq!(
        agent.answer_to(asked).expect("answered")["result"]["stdout"],
        "up 3 days\n"
    );
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Connection))]
    );

    // A later command runs without being shown to anyone first, on any
    // companion computer, and its record says so.
    let other = h.computer(&mut ui, "Vision", true);
    let later = h.run(&mut agent, &other, "cat /etc/os-release");
    assert!(h.engine.state().approvals.is_empty(), "not asked");
    let second = h.started("cat /etc/os-release");
    h.engine.computer_ran(&second, printed("Debian\n", "", 0));
    assert!(agent.answer_to(later).is_some());
    let runs = &h.engine.state().computer_runs;
    assert!(runs[&first].shown_first);
    assert!(!runs[&second].shown_first);
    assert_eq!(runs[&second].shell_command, "cat /etc/os-release");
    assert_eq!(runs[&second].computer_name, "Vision");

    // Nothing was stored: another connection of the same agent asks, and
    // so does this one once it has reconnected.
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    let mut second_connection = h.agent();
    let asked = h.run(&mut second_connection, &computer, "id");
    assert!(second_connection.answer_to(asked).is_none());
    h.answer(&mut ui, false, "once");
    h.engine.disconnect(agent.id);
    let mut back = h.agent();
    let asked = h.run(&mut back, &computer, "id");
    assert!(back.answer_to(asked).is_none());
    assert_eq!(h.pending().client_id, record);
    assert!(h.ssh.take().is_empty());
}

#[test]
fn always_allow_stores_the_capability_and_later_commands_still_leave_a_record() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let asked = h.run(&mut agent, &computer, "uptime");
    h.answer(&mut ui, true, "always");
    let first = h.started("uptime");
    h.engine.computer_ran(&first, printed("up\n", "", 0));
    assert!(agent.answer_to(asked).is_some());
    assert!(
        h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    assert_eq!(
        h.answers(),
        [(ApprovalState::Allowed, Some(Remember::Always))]
    );

    // The next one runs unasked, on this connection and on a new one, and
    // each is journaled with its command and that nobody was shown it.
    let later = h.run(&mut agent, &computer, "df -h");
    let second = h.started("df -h");
    h.engine.computer_ran(&second, printed("/ 40%\n", "", 0));
    assert!(agent.answer_to(later).is_some());
    let mut new = h.agent();
    let asked = h.run(&mut new, &computer, "free -m");
    let third = h.started("free -m");
    h.engine.computer_ran(&third, printed("Mem\n", "", 0));
    assert!(new.answer_to(asked).is_some());
    assert!(h.engine.state().approvals.is_empty());
    let journaled: Vec<(String, bool, Option<TerminalEnd>)> = h
        .records()
        .into_iter()
        .map(|run| (run.shell_command, run.shown_first, run.end))
        .collect();
    let (running, ended) = (None, Some(TerminalEnd::Exited));
    assert_eq!(
        journaled,
        [
            ("uptime".to_owned(), true, running),
            ("uptime".to_owned(), true, ended),
            ("df -h".to_owned(), false, running),
            ("df -h".to_owned(), false, ended),
            ("free -m".to_owned(), false, running),
            ("free -m".to_owned(), false, ended),
        ]
    );
    // An agent still opens no terminal, whatever it holds.
    let reply = h.call(
        &mut agent,
        "terminal/open",
        json!({ "computerId": computer, "cols": 80, "rows": 24 }),
    );
    assert_eq!(error(&reply), ("capability-denied", NOT_FOR_AGENTS));
}

#[test]
fn a_denied_command_is_not_run_and_the_agent_is_told_not_to_ask_again() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let command = "sudo systemctl restart mediamtx";
    let asked = h.run(&mut agent, &computer, command);
    h.answer(&mut ui, false, "once");
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(error(&reply), ("capability-denied", RUN_NOT_ALLOWED));
    assert_eq!(reply["error"]["data"]["kind"], "capability-denied");
    assert!(h.ssh.take().is_empty());
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.engine.state().computer_runs.is_empty());
    assert!(h.records().is_empty(), "no run to record");
    assert!(h.engine.state().approvals.is_empty());
    assert!(
        !h.engine.state().clients[&record]
            .capabilities
            .contains(&Capability::Terminal)
    );
    // The journal keeps what was asked and that it was denied.
    let denied: Vec<Option<String>> = h
        .journal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match &event.change {
            Change::ApprovalUpserted { approval } if approval.state == ApprovalState::Denied => {
                Some(approval.shell_command.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(denied, [Some(command.to_owned())]);
    // Its notice is gone from the operator's page.
    assert!(h.engine.state().notifications.values().all(|n| {
        !n.condition
            .as_deref()
            .unwrap_or_default()
            .starts_with("approval:")
    }));
}

#[test]
fn with_no_operator_there_a_command_is_refused_at_the_deadline() {
    let (mut h, ui, computer, mut agent, _) = with_agent();
    h.engine.disconnect(ui.id);
    let asked = h.run(&mut agent, &computer, "uptime");
    assert!(agent.answer_to(asked).is_none(), "held until the deadline");
    h.engine.expire_approvals(NOW_US + DEADLINE_US - 1);
    assert!(agent.answer_to(asked).is_none(), "not before 60 s");
    h.engine.expire_approvals(NOW_US + DEADLINE_US);
    let reply = agent.answer_to(asked).expect("answered at the deadline");
    assert_eq!(error(&reply), ("capability-denied", RUN_NOBODY));
    assert!(h.ssh.take().is_empty());
    assert!(h.engine.state().computer_runs.is_empty());
    assert_eq!(h.answers(), [(ApprovalState::Expired, None)]);
}

#[test]
fn only_an_operator_answers_and_the_agent_never_sees_its_own_request() {
    let (mut h, mut ui, computer, mut agent, _) = with_agent();
    h.ok(
        &mut agent,
        "state/subscribe",
        json!({ "entities": ["approval", "notification"] }),
    );
    let command = "cat /home/pi/.ssh/id_ed25519";
    let asked = h.run(&mut agent, &computer, command);
    let approval = h.pending();

    // The agent gets neither the approval nor its notice, as it happens
    // or when it asks.
    let seen = serde_json::to_string(&agent.arrived()).unwrap();
    assert!(!seen.contains(&approval.id), "{seen}");
    assert!(!seen.contains("wants to run a command"), "{seen}");
    let again = h.ok(
        &mut agent,
        "state/subscribe",
        json!({ "entities": ["approval", "notification"] }),
    );
    assert_eq!(again["snapshot"]["approvals"], json!([]), "{again}");
    assert_eq!(again["snapshot"]["notifications"], json!([]), "{again}");

    // Even allowed to answer approvals, it cannot answer its own.
    h.set_capabilities(&mut ui, AGENT, json!(["read", "approve"]));
    let reply = h.call(
        &mut agent,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert_eq!(
        error(&reply),
        (
            "capability-denied",
            "a client cannot answer its own approval"
        )
    );
    // Nor can a paired device that was allowed to answer approvals, nor
    // another agent: a command on a vehicle's computer is an operator's
    // to allow.
    let (tablet_record, tablet_trust) = h.pair(&mut ui, "Mozilla/5.0 (iPad)", "fly");
    let version = h.engine.state().clients[&tablet_record].version;
    h.ok(
        &mut ui,
        "client/setCapabilities",
        json!({ "id": tablet_record, "expectedVersion": version, "capabilities": ["read", "control", "approve"] }),
    );
    let mut tablet = h.device(&tablet_trust);
    let mut other =
        h.connect_declaring(Transport::Socket, Trust::LocalUser, "Codex", "agent", true);
    h.set_capabilities(&mut ui, "Codex", json!(["read", "approve"]));
    for client in [&mut tablet, &mut other] {
        let reply = h.call(
            client,
            "approval/resolve",
            json!({ "id": approval.id, "allow": true, "remember": "always" }),
        );
        assert_eq!(error(&reply), ("capability-denied", OPERATOR_ANSWERS));
    }
    // And neither is shown the command, which only those who answer read:
    // not when they ask for the state or list approvals, and not as a
    // second request arrives while they follow both.
    for client in [&mut tablet, &mut other] {
        let snapshot = h.ok(
            client,
            "state/subscribe",
            json!({ "entities": ["approval", "notification"] }),
        );
        assert_eq!(snapshot["snapshot"]["approvals"], json!([]), "{snapshot}");
        assert_eq!(
            snapshot["snapshot"]["notifications"],
            json!([]),
            "{snapshot}"
        );
        let listed = h.ok(client, "approval/list", json!({}));
        assert_eq!(listed["approvals"], json!([]), "{listed}");
        client.arrived();
    }
    let mut third = h.connect(Transport::Socket, Trust::LocalUser, "Cursor", "agent");
    let second = h.run(&mut third, &computer, "cat /etc/shadow");
    let waiting: Vec<String> = h.engine.state().approvals.keys().cloned().collect();
    assert_eq!(waiting.len(), 2);
    for client in [&mut tablet, &mut other, &mut agent] {
        let seen = serde_json::to_string(&client.arrived()).unwrap();
        assert!(!seen.contains("shadow"), "{seen}");
        assert!(!seen.contains("wants to run a command"), "{seen}");
        assert!(!seen.contains("shellCommandShown"), "{seen}");
    }
    // The operator's page is shown both.
    let shown = serde_json::to_string(&ui.arrived()).unwrap();
    assert!(shown.contains("cat /etc/shadow"), "{shown}");
    let later = waiting.iter().find(|id| **id != approval.id).unwrap();
    h.ok(
        &mut ui,
        "approval/resolve",
        json!({ "id": later, "allow": false, "remember": "once" }),
    );
    assert!(third.answer_to(second).is_some());
    for client in [&mut tablet, &mut other] {
        let seen = serde_json::to_string(&client.arrived()).unwrap();
        assert!(!seen.contains("shadow"), "{seen}");
        assert!(!seen.contains("wants to run a command"), "{seen}");
    }
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.pending().id, approval.id, "still waiting");
    assert!(agent.answer_to(asked).is_none());

    // The operator's own program answers, as its page does.
    let mut cli = h.cli();
    h.answer(&mut cli, true, "once");
    h.started(command);
}

const CANNOT_SHOW: &str = "This request is to run a command, and this client cannot show the command. Answer it in an up-to-date UXDuck window, or with `uxduck approvals`.";

#[test]
fn an_operators_page_that_cannot_show_a_command_is_not_asked_and_cannot_answer() {
    let (mut h, mut ui, computer, mut agent, _) = with_agent();
    // UXDuck's page from before S425, still open on the operator's
    // computer: an operator that shows approvals and says nothing of
    // commands. It would draw the answers without the command.
    let mut old = h.connect_showing(
        Transport::WebSocket,
        Trust::PairedBrowser {
            origin: "http://127.0.0.1:45000".into(),
        },
        "uxduck-web",
        "operator-ui",
        true,
        false,
    );
    h.ok(
        &mut old,
        "state/subscribe",
        json!({ "entities": ["approval", "notification"] }),
    );
    old.arrived();
    ui.arrived();
    let command = "sudo systemctl stop mediamtx";
    let asked = h.run(&mut agent, &computer, command);
    let approval = h.pending();

    // It gets neither the request nor its notice, as it happens or when it asks.
    let seen = serde_json::to_string(&old.arrived()).unwrap();
    assert!(!seen.contains(&approval.id), "{seen}");
    assert!(!seen.contains("wants to run a command"), "{seen}");
    let again = h.ok(
        &mut old,
        "state/subscribe",
        json!({ "entities": ["approval", "notification"] }),
    );
    assert_eq!(again["snapshot"]["approvals"], json!([]), "{again}");
    assert_eq!(again["snapshot"]["notifications"], json!([]), "{again}");
    let listed = h.ok(&mut old, "approval/list", json!({}));
    assert_eq!(listed["approvals"], json!([]), "{listed}");

    // And it cannot answer for a command it never showed, either way.
    for allow in [true, false] {
        let reply = h.call(
            &mut old,
            "approval/resolve",
            json!({ "id": approval.id, "allow": allow, "remember": "always" }),
        );
        assert_eq!(error(&reply), ("capability-denied", CANNOT_SHOW));
    }
    assert!(h.ssh.take().is_empty());
    assert_eq!(h.pending().id, approval.id, "still waiting");
    assert!(agent.answer_to(asked).is_none());

    // The page that shows the command was sent it, whole, and answers.
    let shown = serde_json::to_string(&ui.arrived()).unwrap();
    assert!(shown.contains(command), "{shown}");
    h.answer(&mut ui, true, "once");
    h.started(command);
}

#[test]
fn a_command_is_an_agents_and_nobody_elses_to_run() {
    let (mut h, mut ui, computer, mut phone, _, _) = with_phone();
    // A person on a paired device has the Terminal tab, and a third
    // party's program has no command of its own, whatever either was
    // allowed.
    let mut tool = h.connect(
        Transport::Socket,
        Trust::LocalUser,
        "ground-tool",
        "third-party",
    );
    h.set_capabilities(&mut ui, PHONE, json!(["read", "control", "terminal"]));
    h.set_capabilities(&mut ui, "ground-tool", json!(["read", "terminal"]));
    for client in [&mut phone, &mut tool] {
        let reply = h.call(
            client,
            "computer/run",
            json!({ "computerId": computer, "command": "id" }),
        );
        assert_eq!(error(&reply), ("capability-denied", RUN_FOR_AGENTS));
    }
    assert!(h.engine.state().approvals.is_empty(), "never asked for");
    assert!(h.ssh.take().is_empty());
    assert!(h.records().is_empty());

    // The person's own program runs one unasked, and it is recorded as
    // any other is.
    let mut cli = h.cli();
    let asked = h.run(&mut cli, &computer, "id");
    let terminal = h.started("id");
    h.engine
        .computer_ran(&terminal, printed("uid=1000\n", "", 0));
    assert!(cli.answer_to(asked).is_some());
    let run = &h.engine.state().computer_runs[&terminal];
    assert_eq!(run.client_name, "uxduck-cli");
    assert!(!run.shown_first);
}

#[test]
fn what_a_command_needs_is_said_before_an_operator_is_asked() {
    let (mut h, mut ui, computer, mut agent, _) = with_agent();
    let unchecked = h.computer(&mut ui, "Vision", false);
    for (params, kind, message) in [
        (
            json!({ "computerId": "cc-nothing", "command": "id" }),
            "not-found",
            format!(
                "No companion computer has the id cc-nothing. The companion computers are: Camera Pi ({computer}) on Alpha, Vision ({unchecked}) on Alpha."
            ),
        ),
        (
            json!({ "computerId": unchecked, "command": "id" }),
            "invalid-value",
            "Vision has not been checked yet, so UXDuck does not know its host key. A person checks it first on Alpha's page.".to_owned(),
        ),
        (
            json!({ "computerId": computer, "command": "  \n" }),
            "invalid-value",
            "Give the command to run.".to_owned(),
        ),
        (
            json!({ "computerId": computer, "command": "id\u{0}" }),
            "invalid-value",
            "A command cannot hold a NUL character, because ssh cannot be given one.".to_owned(),
        ),
        (
            json!({ "computerId": computer, "command": "id", "deadlineS": 0 }),
            "invalid-value",
            "deadlineS is at least 1 second.".to_owned(),
        ),
    ] {
        let reply = h.call(&mut agent, "computer/run", params);
        assert_eq!(error(&reply), (kind, message.as_str()));
    }
    assert!(h.engine.state().approvals.is_empty(), "never asked for");
    assert!(h.ssh.take().is_empty());

    // No `ssh` on the computer UXDuck runs on: said in its own words, and
    // nothing is left behind.
    h.set_capabilities(&mut ui, AGENT, json!(["read", "terminal"]));
    let why = "UXDuck found no ssh on this computer. Install OpenSSH's client.";
    *h.ssh.missing.lock().unwrap() = Some(why.to_owned());
    let reply = h.call(
        &mut agent,
        "computer/run",
        json!({ "computerId": computer, "command": "id" }),
    );
    assert_eq!(error(&reply), ("invalid-value", why));
    assert!(h.engine.state().terminals.is_empty());
    assert!(h.records().is_empty());
}

#[test]
fn a_command_has_no_arming_gate_and_no_wait_behind_a_check() {
    let (mut h, mut ui, computer, mut agent, _) = with_allowed_agent();
    // Armed (T8), and a check of the same computer under way (T12).
    h.observed(true);
    assert!(h.engine.state().vehicles["v"].armed);
    h.ok(
        &mut ui,
        "computer/check",
        json!({ "vehicleId": "v", "computerId": computer }),
    );
    let asked = h.run(&mut agent, &computer, "uptime");
    let terminal = h.started("uptime");
    h.engine.computer_ran(&terminal, printed("up\n", "", 0));
    assert_eq!(
        agent.answer_to(asked).expect("answered")["result"]["exitStatus"],
        0
    );
}

#[test]
fn the_answer_holds_both_streams_apart_the_exit_status_and_what_ssh_said() {
    let (mut h, _ui, computer, mut agent, _) = with_allowed_agent();
    let asked = h.run(&mut agent, &computer, "ls /nope; echo done");
    let terminal = h.started("ls /nope; echo done");
    h.engine.computer_ran(
        &terminal,
        printed("done\n", "ls: cannot access '/nope'\n", 2),
    );
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(reply["result"]["stdout"], "done\n");
    assert_eq!(reply["result"]["stderr"], "ls: cannot access '/nope'\n");
    assert_eq!(reply["result"]["exitStatus"], 2);
    assert_eq!(reply["result"]["ended"], "exited");
    assert!(reply["result"].get("notes").is_none(), "{reply}");
    let run = &h.engine.state().computer_runs[&terminal];
    assert_eq!(
        (run.end, run.exit_status, run.ended_at_us),
        (Some(TerminalEnd::Exited), Some(2), Some(NOW_US))
    );

    // `ssh` itself failing is an error in the host's words, such as a host
    // key that is not the one UXDuck saw.
    let asked = h.run(&mut agent, &computer, "uptime");
    let terminal = h.started("uptime");
    let why = "Camera Pi's host key is not the one UXDuck saw when it checked it. Check it again on Alpha's page.";
    h.engine.computer_ran(
        &terminal,
        RunReport {
            stdout: Collected::default(),
            stderr: Collected::default(),
            own: Some((TerminalEnd::Failed, None, Some(why.to_owned()))),
        },
    );
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(error(&reply), ("invalid-value", why));
    assert_eq!(
        h.engine.state().computer_runs[&terminal].end,
        Some(TerminalEnd::Failed)
    );
    assert!(h.engine.state().terminals.is_empty());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one answer, whole, cut in the middle, and with bytes that are not text"
)]
fn an_answer_keeps_the_start_and_the_end_and_says_exactly_what_it_left_out() {
    let (mut h, _ui, computer, mut agent, _) = with_allowed_agent();
    assert_eq!(
        RUN_KEEPS,
        Keeps {
            stdout_first: 4_000,
            stdout_last: 4_000,
            stderr_first: 1_000,
            stderr_last: 1_000,
        }
    );
    let asked = h.run(&mut agent, &computer, "journalctl -u mediamtx");
    let terminal = h.started("journalctl -u mediamtx");
    h.engine.computer_ran(
        &terminal,
        RunReport {
            stdout: Collected {
                first: vec![b'a'; 4_000],
                last: vec![b'z'; 4_000],
                bytes: 100_000,
            },
            stderr: Collected {
                first: vec![b'e'; 1_000],
                last: vec![b'f'; 1_000],
                bytes: 5_000,
            },
            own: Some((TerminalEnd::Exited, Some(0), None)),
        },
    );
    let reply = agent.answer_to(asked).expect("answered");
    let result = &reply["result"];
    assert_eq!(
        result["stdout"],
        format!(
            "{}\n[UXDuck left out 92000 bytes here]\n{}",
            "a".repeat(4_000),
            "z".repeat(4_000)
        )
    );
    assert_eq!(
        result["stderr"],
        format!(
            "{}\n[UXDuck left out 3000 bytes here]\n{}",
            "e".repeat(1_000),
            "f".repeat(1_000)
        )
    );
    assert_eq!(result["stdoutBytes"], 100_000);
    assert_eq!(result["stderrBytes"], 5_000);
    assert_eq!(
        result["stdoutLeftOut"],
        json!({ "bytes": 92_000, "afterBytes": 4_000 })
    );
    assert_eq!(
        result["stderrLeftOut"],
        json!({ "bytes": 3_000, "afterBytes": 1_000 })
    );
    assert_eq!(
        result["notes"],
        json!([
            "Standard output was 100000 bytes. Its first 4000 and its last 4000 are here, and the 92000 between them were left out, because a tool answer holds only so much. Run it again with less output, such as through head, tail or grep.",
            "Standard error was 5000 bytes. Its first 1000 and its last 1000 are here, and the 3000 between them were left out, because a tool answer holds only so much. Run it again with less output, such as through head, tail or grep."
        ])
    );
    // The record says how much was left out, and never what was printed.
    let run = &h.engine.state().computer_runs[&terminal];
    assert_eq!(run.left_out_bytes, 95_000);
    // The whole answer at its bounds fits what the assistant's chat takes
    // of one tool result, which is the reason for the bounds.
    let held: usize = ["stdout", "stderr"]
        .iter()
        .map(|key| result[key].as_str().unwrap().chars().count())
        .sum::<usize>()
        + result["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note.as_str().unwrap().chars().count())
            .sum::<usize>();
    assert!(held < 12_000, "{held}");

    // A cut that falls inside a character leaves its pieces out too, and
    // counts them, instead of showing them as bytes that are not text.
    let asked = h.run(&mut agent, &computer, "cat prices.txt");
    let terminal = h.started("cat prices.txt");
    let mut first = vec![b'a'; 3_998];
    first.extend_from_slice(&"€".as_bytes()[..2]);
    let mut last = "€".as_bytes()[1..].to_vec();
    last.extend_from_slice(&vec![b'z'; 3_998]);
    h.engine.computer_ran(
        &terminal,
        RunReport {
            stdout: Collected {
                first,
                last,
                bytes: 9_000,
            },
            stderr: Collected::default(),
            own: Some((TerminalEnd::Exited, Some(0), None)),
        },
    );
    let reply = agent.answer_to(asked).expect("answered");
    let result = &reply["result"];
    assert_eq!(
        result["stdout"],
        format!(
            "{}\n[UXDuck left out 1004 bytes here]\n{}",
            "a".repeat(3_998),
            "z".repeat(3_998)
        )
    );
    assert_eq!(
        result["stdoutLeftOut"],
        json!({ "bytes": 1_004, "afterBytes": 3_998 })
    );
    assert_eq!(result["notes"].as_array().unwrap().len(), 1, "{result}");

    // Bytes that are not UTF-8 text are replaced where a person and a
    // model can see it, and the answer says so.
    let asked = h.run(&mut agent, &computer, "head -c 16 /dev/video0");
    let terminal = h.started("head -c 16 /dev/video0");
    h.engine.computer_ran(
        &terminal,
        RunReport {
            stdout: whole(b"ok \xff\xfe end\n"),
            stderr: Collected::default(),
            own: Some((TerminalEnd::Exited, Some(0), None)),
        },
    );
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(reply["result"]["stdout"], "ok \u{fffd}\u{fffd} end\n");
    assert_eq!(reply["result"]["stdoutBytes"], 10);
    assert_eq!(
        reply["result"]["notes"],
        json!(["Standard output held bytes that are not UTF-8 text. Each is shown as \u{fffd}."])
    );
}

#[test]
fn a_command_that_never_ends_is_stopped_at_its_deadline_and_answers_with_what_it_printed() {
    let (mut h, _ui, computer, mut agent, _) = with_allowed_agent();
    // The caller's own deadline.
    let asked = h.send(
        &mut agent,
        "computer/run",
        json!({ "computerId": computer, "command": "journalctl -f", "deadlineS": 5 }),
    );
    let terminal = h.started("journalctl -f");
    assert_eq!(h.engine.state().computer_runs[&terminal].deadline_s, 5);
    h.engine.stop_runs_at_deadline(NOW_US + 4_999_999);
    assert!(h.ssh.take().is_empty(), "not before its deadline");
    h.engine.stop_runs_at_deadline(NOW_US + 5_000_000);
    // The host is told to end it, which kills it on the computer.
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(terminal.clone(), TerminalEnd::Deadline)]
    );
    assert!(h.engine.state().terminals.is_empty());
    assert_eq!(
        h.engine.state().computer_runs[&terminal].end,
        Some(TerminalEnd::Deadline)
    );
    assert_eq!(h.ends(), [(TerminalEnd::Deadline, None)]);
    assert!(
        agent.answer_to(asked).is_none(),
        "the answer waits for what it printed"
    );
    h.engine
        .computer_ran(&terminal, cut_short("line 1\nline 2\n"));
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(
        reply["result"],
        json!({
            "runId": terminal,
            "ended": "deadline",
            "durationMs": 0,
            "stdout": "line 1\nline 2\n",
            "stderr": "",
            "stdoutBytes": 14,
            "stderrBytes": 0,
            "notes": ["Stopped at its deadline of 5 s, and ended on the computer. What it printed until then is here."]
        })
    );
    // Stopping again at a later second does nothing more.
    h.engine.stop_runs_at_deadline(NOW_US + 9_000_000);
    assert!(h.ssh.take().is_empty());

    // With no deadline given it has 30 s.
    let asked = h.run(&mut agent, &computer, "tail -f /var/log/syslog");
    let terminal = h.started("tail -f /var/log/syslog");
    assert_eq!(RUN_DEADLINE_S, 30);
    h.engine.stop_runs_at_deadline(NOW_US + 29_999_999);
    assert!(h.ssh.take().is_empty());
    h.engine.stop_runs_at_deadline(NOW_US + 30_000_000);
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(terminal.clone(), TerminalEnd::Deadline)]
    );
    h.engine.computer_ran(&terminal, cut_short(""));
    assert_eq!(
        agent.answer_to(asked).expect("answered")["result"]["notes"][0],
        "Stopped at its deadline of 30 s, and ended on the computer. What it printed until then is here."
    );
}

#[test]
fn an_operator_sees_a_running_command_and_ends_it() {
    let (mut h, mut ui, computer, mut agent, _) = with_allowed_agent();
    let asked = h.run(&mut agent, &computer, "sleep 600");
    let terminal = h.started("sleep 600");

    // It is listed with the open terminals, under the agent's name, as
    // what it is.
    let snapshot = h.ok(
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["terminal"] }),
    );
    let listed = &snapshot["snapshot"]["terminals"];
    assert_eq!(listed[0]["id"], terminal.as_str());
    assert_eq!(listed[0]["kind"], "run");
    assert_eq!(listed[0]["clientName"], AGENT);
    assert_eq!(listed[0]["computerId"], computer.as_str());
    // And its record is read while it runs, with its command.
    let runs = h.ok(&mut ui, "computer/runs", json!({ "computerId": computer }));
    assert_eq!(runs["runs"][0]["id"], terminal.as_str());
    assert_eq!(runs["runs"][0]["shellCommandShown"], "sleep 600");
    assert!(runs["runs"][0].get("end").is_none());

    // The operator ends it.
    h.ok(&mut ui, "terminal/close", json!({ "terminalId": terminal }));
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(terminal.clone(), TerminalEnd::EndedByOperator)]
    );
    assert!(h.engine.state().terminals.is_empty());
    h.engine.computer_ran(&terminal, cut_short("partial\n"));
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(reply["result"]["ended"], "ended-by-operator");
    assert_eq!(reply["result"]["stdout"], "partial\n");
    assert_eq!(
        reply["result"]["notes"],
        json!(["The operator stopped it. What it printed until then is here."])
    );
    let runs = h.ok(&mut ui, "computer/runs", json!({ "computerId": computer }));
    assert_eq!(runs["runs"][0]["end"], "ended-by-operator");
}

#[test]
fn taking_terminal_away_stops_that_agents_running_command_and_the_next_asks() {
    let (mut h, mut ui, computer, mut agent, _) = with_allowed_agent();
    let asked = h.run(&mut agent, &computer, "sleep 600");
    let terminal = h.started("sleep 600");
    h.set_capabilities(&mut ui, AGENT, json!(["read"]));
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(
            terminal.clone(),
            TerminalEnd::CapabilityRemoved
        )]
    );
    h.engine.computer_ran(&terminal, cut_short(""));
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(reply["result"]["ended"], "capability-removed");
    assert_eq!(
        reply["result"]["notes"],
        json!([
            "The operator stopped it and no longer allows commands from this client without asking. What it printed until then is here."
        ])
    );
    let again = h.run(&mut agent, &computer, "uptime");
    assert!(agent.answer_to(again).is_none(), "it asks again");
    assert_eq!(h.pending().shell_command.as_deref(), Some("uptime"));
    assert!(h.ssh.take().is_empty());
}

#[test]
fn a_command_ends_with_its_request_its_connection_and_the_daemon() {
    let (mut h, _ui, computer, mut agent, _) = with_allowed_agent();
    // The agent cancels its request: the command is stopped, and nobody
    // waits for its answer.
    let asked = h.run(&mut agent, &computer, "sleep 600");
    let cancelled = h.started("sleep 600");
    h.engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "method": "cancel", "params": { "id": asked } }).to_string(),
    );
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(cancelled.clone(), TerminalEnd::HungUp)]
    );
    h.engine.computer_ran(&cancelled, cut_short("partial\n"));
    assert!(agent.answer_to(asked).is_none(), "nobody waits for it");
    assert_eq!(
        h.engine.state().computer_runs[&cancelled].end,
        Some(TerminalEnd::HungUp)
    );

    // Its connection goes (T13).
    h.run(&mut agent, &computer, "sleep 601");
    let left = h.started("sleep 601");
    h.engine.disconnect(agent.id);
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(left.clone(), TerminalEnd::HungUp)]
    );
    h.engine.computer_ran(&left, cut_short(""));
    assert_eq!(
        h.engine.state().computer_runs[&left].end,
        Some(TerminalEnd::HungUp)
    );

    // UXDuck stops, and a daemon that was killed says so on its next
    // start: no record is left running.
    let mut agent = h.agent();
    h.run(&mut agent, &computer, "sleep 602");
    let running = h.started("sleep 602");
    let killed: State =
        serde_json::from_value(serde_json::to_value(h.engine.state()).unwrap()).unwrap();
    assert_eq!(killed.computer_runs[&running].end, None);
    let journal = Journal::default();
    let again = engine_with(killed, &journal);
    assert!(again.state().terminals.is_empty());
    assert_eq!(
        again.state().computer_runs[&running].end,
        Some(TerminalEnd::Stopped)
    );
    h.engine.stop_terminals();
    assert_eq!(
        h.ssh.take(),
        [Asked::Close(running.clone(), TerminalEnd::Stopped)]
    );
    assert_eq!(
        h.engine.state().computer_runs[&running].end,
        Some(TerminalEnd::Stopped)
    );
    assert!(h.engine.state().terminals.is_empty());
}

#[test]
fn the_journal_keeps_who_ran_what_where_and_how_it_ended_and_never_what_it_printed() {
    let (mut h, mut ui, computer, mut agent, record) = with_agent();
    let command = "cat /etc/mediamtx.yml --marker-7c1e9a";
    let asked = h.run(&mut agent, &computer, command);
    h.answer(&mut ui, true, "once");
    let terminal = h.started(command);
    h.engine.computer_ran(
        &terminal,
        printed(
            "rtspPassword: marker-out-52b8\n",
            "warn marker-err-9d07\n",
            3,
        ),
    );
    let reply = agent.answer_to(asked).expect("answered");
    assert_eq!(reply["result"]["stdout"], "rtspPassword: marker-out-52b8\n");

    // The receipt: journaled when it starts and again when it ends, by
    // the agent's connection.
    let events = h.journal.0.lock().unwrap().clone();
    let recorded: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.change {
            Change::ComputerRunUpserted { run } => Some((event, run.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 2);
    let (started_event, started) = &recorded[0];
    assert_eq!(
        *started,
        ComputerRun {
            id: terminal.clone(),
            vehicle_id: "v".into(),
            vehicle_name: "Alpha".into(),
            computer_id: computer.clone(),
            computer_name: "Camera Pi".into(),
            client_id: record.clone(),
            client_name: AGENT.into(),
            shell_command: command.into(),
            shown_first: true,
            deadline_s: 30,
            started_at_us: NOW_US,
            ended_at_us: None,
            end: None,
            exit_status: None,
            left_out_bytes: 0,
        }
    );
    assert!(!started_event.actor.client_id.is_empty());
    assert_eq!(
        recorded[1].1,
        ComputerRun {
            ended_at_us: Some(NOW_US),
            end: Some(TerminalEnd::Exited),
            exit_status: Some(3),
            ..started.clone()
        }
    );

    // The command is in the journal and the state. What it printed is in
    // neither.
    let journal = serde_json::to_string(&events).unwrap();
    let state = serde_json::to_string(h.engine.state()).unwrap();
    for kept in [&journal, &state] {
        assert!(kept.contains("marker-7c1e9a"));
        assert!(!kept.contains("marker-out-52b8"), "what it printed");
        assert!(!kept.contains("marker-err-9d07"), "nor on standard error");
    }
    // Sync sends nothing of a run, of its session or of its approval.
    for event in &events {
        if matches!(
            event.change,
            Change::ComputerRunUpserted { .. }
                | Change::TerminalOpened { .. }
                | Change::TerminalEnded { .. }
                | Change::ApprovalUpserted { .. }
        ) {
            assert!(uxduck_engine::sync::exportable(event).is_none());
        }
    }
    let synced = serde_json::to_string(
        &events
            .iter()
            .filter_map(uxduck_engine::sync::exportable)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!synced.contains("marker-"), "{synced}");
}

#[test]
fn an_operator_reads_what_agents_ran_and_nobody_else_does() {
    let (mut h, mut ui, computer, mut agent, record) = with_allowed_agent();
    let other = h.computer(&mut ui, "Vision", true);
    // More than the state holds of one computer.
    for n in 0..25 {
        let command = format!("echo {n}\t# on the Pi");
        let asked = h.run(&mut agent, &computer, &command);
        let terminal = h.started(&command);
        h.engine.computer_ran(&terminal, printed("ok\n", "", 0));
        assert!(agent.answer_to(asked).is_some());
    }
    let asked = h.run(&mut agent, &other, "uptime");
    let terminal = h.started("uptime");
    h.engine.computer_ran(&terminal, printed("up\n", "", 0));
    assert!(agent.answer_to(asked).is_some());

    // The list holds the latest 20 of each computer and says so. The
    // journal keeps every one.
    let listed = h.ok(&mut ui, "computer/runs", json!({ "computerId": computer }));
    assert_eq!(listed["keptPerComputer"], 20);
    let runs = listed["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 20);
    for run in runs {
        assert_eq!(run["computerId"], computer.as_str());
        assert_eq!(run["computerName"], "Camera Pi");
        assert_eq!(run["vehicleName"], "Alpha");
        assert_eq!(run["clientId"], record.as_str());
        assert_eq!(run["clientName"], AGENT);
        assert_eq!(run["shownFirst"], false);
        assert_eq!(run["end"], "exited");
        assert_eq!(run["exitStatus"], 0);
        // As a person reads it, and never the raw command beside it.
        let shown = run["shellCommandShown"].as_str().unwrap();
        assert!(shown.starts_with("echo ") && shown.ends_with("⇥# on the Pi"));
        assert!(run.get("shellCommand").is_none());
    }
    let everywhere = h.ok(&mut ui, "computer/runs", json!({}));
    assert_eq!(everywhere["runs"].as_array().unwrap().len(), 21);
    let journaled: std::collections::BTreeSet<String> =
        h.records().into_iter().map(|run| run.id).collect();
    assert_eq!(journaled.len(), 26);

    // What agents ran is the operator's to read: not an agent's, and not
    // a paired device's.
    let (_, trust) = h.pair(&mut ui, "Mozilla/5.0 (iPhone)", "fly");
    let mut phone = h.device(&trust);
    for client in [&mut agent, &mut phone] {
        let reply = h.call(client, "computer/runs", json!({}));
        assert_eq!(
            error(&reply),
            ("capability-denied", "only an operator client may do this")
        );
    }
}

#[test]
fn the_tap_shows_neither_a_command_nor_what_it_printed() {
    let (mut h, mut ui, computer, mut agent, _) = with_agent();
    h.ok(
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["approval", "notification", "terminal"] }),
    );
    let mut tracer = h.cli();
    h.ok(&mut tracer, "trace/subscribe", json!({}));
    while tracer.inbox.try_recv().is_ok() {}

    let command = "cat /etc/mediamtx.yml --marker-7c1e9a";
    let asked = h.run(&mut agent, &computer, command);
    h.ok(&mut ui, "approval/list", json!({}));
    h.answer(&mut ui, true, "once");
    let terminal = h.started(command);
    h.engine.computer_ran(
        &terminal,
        printed(
            "rtspPassword: marker-out-52b8\n",
            "warn marker-err-9d07\n",
            0,
        ),
    );
    assert!(agent.answer_to(asked).is_some());
    h.ok(&mut ui, "computer/runs", json!({}));
    // And one that is denied, whose answer is an error.
    let denied = h.run(&mut agent, &computer, "rm --marker-7c1e9a");
    h.answer(&mut ui, false, "once");
    assert!(agent.answer_to(denied).is_some());

    let mut frames = Vec::new();
    while let Ok(text) = tracer.inbox.try_recv() {
        frames.push(serde_json::from_str::<Value>(&text).unwrap());
    }
    let tapped = serde_json::to_string(&frames).unwrap();
    for marker in [
        "marker-7c1e9a",
        "marker-out-52b8",
        "marker-err-9d07",
        "mediamtx.yml",
    ] {
        assert!(!tapped.contains(marker), "{marker}: {tapped}");
    }
    // The tap still shows that it happened: the request with its
    // parameters hidden, its answer hidden, and the approval the
    // operator's page was sent with the command hidden.
    let request = frames
        .iter()
        .find(|frame| {
            frame["params"]["method"] == "computer/run" && frame["params"]["direction"] == "in"
        })
        .expect("the request is tapped");
    assert_eq!(request["params"]["payload"]["params"], "[redacted]");
    assert!(
        frames
            .iter()
            .any(|frame| frame["params"]["direction"] == "out"
                && frame["params"]["payload"]["result"] == "[redacted]"),
        "{tapped}"
    );
    let requested = frames
        .iter()
        .find(|frame| frame["params"]["method"] == "approval/requested")
        .expect("the approval is tapped");
    assert_eq!(
        requested["params"]["payload"]["params"]["shellCommandShown"],
        "[redacted]"
    );
    assert_eq!(
        requested["params"]["payload"]["params"]["clientName"],
        AGENT
    );
}
