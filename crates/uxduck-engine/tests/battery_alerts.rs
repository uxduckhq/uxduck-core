//! What the autopilot says of its batteries (UXD-253): `BATTERY_STATUS`'s
//! charge state and `ArduPilot`'s "Battery 1 is low …" trip the telemetry
//! alarms' one "Battery low" or "Battery critical" (S369) beside the
//! vehicle's own levels, never a second notice, and a failed or unhealthy
//! battery, or `SYS_STATUS`'s battery bit, is an alarm of its own until it
//! clears.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::parameter::{ParamKind, Parameter};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, LinkPort, MemoryCredentials, NoAgent, NoPlugins, NoSystem, Persistence,
    Ports, Random, TransferOrder,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const S: u64 = 1_000_000;

#[derive(Clone, Default)]
struct Time(Arc<AtomicU64>);

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

/// What the engine sent to vehicles: nothing, in every test here.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<String>>>);

impl LinkPort for Wire {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.0.lock().unwrap().push(format!("{order:?}"));
    }
    fn transfer(&mut self, order: TransferOrder) {
        self.0.lock().unwrap().push(format!("{order:?}"));
    }
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

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            assert!(frame.get("error").is_none(), "{method}: {frame}");
            return frame["result"].clone();
        }
        client.queued.push_back(frame);
    }
    panic!("no reply to {method}")
}

struct Bench {
    engine: Engine,
    ui: Client,
    wire: Wire,
    journal: Journal,
    time: Time,
    link_id: String,
}

fn agreed(mut state: State) -> State {
    let operator = serde_json::from_value(json!({
        "id": "op-agreed",
        "name": "Agreed",
        "units": "metric",
        "coordinateFormat": "decimal",
        "terms": { "version": uxduck_contracts::version::TERMS_VERSION, "acceptedAtMs": 1 },
        "version": 1
    }))
    .unwrap();
    state.operators.insert("op-agreed".into(), operator);
    state
}

fn bench(autopilot: &str) -> Bench {
    bench_with(autopilot, &[])
}

/// A bench whose vehicle has these parameters, with the operator's shares
/// off so only the vehicle's own levels and its autopilot speak.
fn bench_with(autopilot: &str, parameters: &[(&str, f64)]) -> Bench {
    let mut state = agreed(State::default());
    state.settings.battery_low_pct = 0;
    state.settings.battery_critical_pct = 0;
    for (name, value) in parameters {
        let parameter = Parameter {
            vehicle_id: "v".into(),
            name: (*name).into(),
            value: *value,
            kind: ParamKind::Real32,
            default: None,
        };
        state.parameters.insert(parameter.id(), parameter);
    }
    let wire = Wire::default();
    let journal = Journal::default();
    let time = Time::default();
    time.0.store(1_000, Ordering::SeqCst);
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
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(wire.clone()),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(time.clone()),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    );
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut ui = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    call(
        &mut engine,
        &mut ui,
        "initialize",
        json!({
            "clientInfo": { "name": "operator-ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    let mut bench = Bench {
        engine,
        ui,
        wire,
        journal,
        time,
        link_id,
    };
    bench.observe(autopilot, true);
    bench
}

impl Bench {
    fn observe(&mut self, autopilot: &str, armed: bool) {
        self.engine.vehicle_observed(Observation {
            connections: None,
            id: "v".into(),
            aliases: Vec::new(),
            link_id: self.link_id.clone(),
            peer: "127.0.0.1:5761".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: autopilot.into(),
            vehicle_type: "GroundRover".into(),
            uid: None,
            firmware: Some("4.5.1".into()),
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: self.time.now_us(),
            armed,
            mode: Some("MANUAL".into()),
        });
    }

    fn item(&self, key: &str, value: Option<f64>, text: Option<&str>) -> TelemetryItem {
        TelemetryItem {
            key: key.into(),
            label: key.into(),
            unit: Unit::None,
            value,
            text: text.map(str::to_owned),
            source: "health".into(),
            component_id: 1,
            time_us: self.time.now_us(),
        }
    }

    fn telemetry(&mut self, items: Vec<TelemetryItem>) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items,
        });
    }

    /// The pack's voltage and mAh used, as `SYS_STATUS` and
    /// `BATTERY_STATUS` say them.
    fn readings(&mut self, volts: f64, used: f64) {
        let items = vec![
            self.item("battery.voltage_v", Some(volts), None),
            self.item("battery.consumed_mah", Some(used), None),
        ];
        self.telemetry(items);
    }

    /// `BATTERY_STATUS`'s charge state for a battery, as the producers say it.
    fn charge_state(&mut self, group: &str, state: u8, words: &str) {
        let item = self.item(
            &format!("{group}.charge_state"),
            Some(f64::from(state)),
            Some(words),
        );
        self.telemetry(vec![item]);
    }

    fn message(&mut self, text: &str) {
        self.engine.vehicle_message("v", 1, 4, text);
    }

    /// The vehicle's battery notifications that are not resolved: the
    /// battery alarm and a battery's own fault.
    fn alerts(&mut self) -> Vec<Value> {
        self.notices()
            .into_iter()
            .filter(|n| {
                n["condition"].as_str().is_some_and(|c| {
                    c.starts_with("battery:") || c.starts_with("telemetry:battery-")
                })
            })
            .collect()
    }

    /// Every notification about the vehicle that is not resolved.
    fn notices(&mut self) -> Vec<Value> {
        let state = call(
            &mut self.engine,
            &mut self.ui,
            "state/subscribe",
            json!({ "entities": ["notification"] }),
        );
        state["snapshot"]["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["state"] != "resolved")
            .cloned()
            .collect()
    }

    /// `<group>.alert` as the Cockpit reads it.
    fn alert_item(&self, group: &str) -> Option<(Option<f64>, Option<String>)> {
        self.engine
            .telemetry("v")?
            .items
            .iter()
            .find(|i| i.key == format!("{group}.alert"))
            .map(|i| (i.value, i.text.clone()))
    }

    /// How many battery notifications were ever raised, each journaled
    /// with a new id.
    fn raised(&self) -> usize {
        let mut ids: Vec<String> = self
            .journal
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.change {
                Change::NotificationUpserted { notification }
                    if notification.condition.as_deref().is_some_and(|c| {
                        c.starts_with("battery:") || c.starts_with("telemetry:battery-")
                    }) =>
                {
                    Some(notification.id.clone())
                }
                _ => None,
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids.len()
    }

    /// Past the battery alarm's 3 s hold.
    fn hold(&self) {
        self.time.0.fetch_add(3 * S, Ordering::SeqCst);
    }
}

#[test]
fn its_voltage_charge_state_and_message_all_saying_low_then_critical_are_one_alarm() {
    let mut b = bench_with(
        "Ardupilotmega",
        &[("BATT_LOW_VOLT", 10.5), ("BATT_CRT_VOLT", 10.0)],
    );
    b.readings(10.4, 1234.0);
    b.charge_state("battery", 2, "Low");
    b.message("Battery 1 is low 10.40V used 1234 mAh");
    assert!(
        b.notices().is_empty(),
        "held 3 s, and the message is no notice of its own: {:?}",
        b.notices()
    );

    b.hold();
    b.readings(10.4, 1240.0);
    b.charge_state("battery", 2, "Low");
    let low = b.notices();
    assert_eq!(low.len(), 1, "{low:?}");
    assert_eq!(low[0]["condition"], "telemetry:battery-low:v");
    assert_eq!(low[0]["title"], "Alpha: Battery low");
    assert_eq!(low[0]["severity"], "alarm");
    assert_eq!(b.raised(), 1);
    assert_eq!(
        b.alert_item("battery"),
        Some((Some(1.0), Some("low".into())))
    );

    b.readings(9.9, 1800.0);
    b.charge_state("battery", 3, "Critical");
    b.message("Battery 1 is critical 9.90V used 1800 mAh");
    b.hold();
    b.readings(9.9, 1810.0);
    b.charge_state("battery", 3, "Critical");
    let critical = b.notices();
    assert_eq!(critical.len(), 1, "{critical:?}");
    assert_eq!(critical[0]["condition"], "telemetry:battery-critical:v");
    assert_eq!(critical[0]["title"], "Alpha: Battery critical");
    assert_eq!(b.raised(), 2, "low, then critical in its place");
    assert_eq!(
        b.alert_item("battery"),
        Some((Some(2.0), Some("critical".into())))
    );

    // Still in the vehicle's messages.
    let log = call(
        &mut b.engine,
        &mut b.ui,
        "vehicle/messages",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(log["messages"].as_array().unwrap().len(), 2);

    // Disarmed: resolved, and the Cockpit's word goes.
    b.observe("Ardupilotmega", false);
    b.readings(9.9, 1810.0);
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
    assert_eq!(b.alert_item("battery"), Some((None, None)));
    assert!(
        b.wire.0.lock().unwrap().is_empty(),
        "nothing sent to the vehicle"
    );
}

#[test]
fn the_charge_state_alone_trips_the_alarm_and_names_the_battery() {
    for autopilot in ["Px4", "Ardupilotmega"] {
        let mut b = bench_with(autopilot, &[("BATT_LOW_VOLT", 0.0)]);
        b.readings(10.84, 1234.0);
        b.charge_state("battery", 1, "OK");
        b.hold();
        b.readings(10.84, 1234.0);
        b.charge_state("battery", 1, "OK");
        assert!(b.alerts().is_empty(), "{autopilot}: OK says nothing");

        b.charge_state("battery", 2, "Low");
        b.hold();
        b.readings(10.84, 1234.0);
        b.charge_state("battery", 2, "Low");
        let low = b.alerts();
        assert_eq!(low.len(), 1, "{autopilot}: {low:?}");
        assert_eq!(low[0]["title"], "Alpha: Battery low");
        assert_eq!(
            low[0]["body"],
            "The autopilot says Battery 1 is low. 10.84 V, 1234 mAh used. Bring it back and land."
        );

        // PX4's emergency is the critical alarm.
        b.charge_state("battery", 4, "Emergency");
        b.hold();
        b.charge_state("battery", 4, "Emergency");
        let critical = b.alerts();
        assert_eq!(critical.len(), 1, "{autopilot}: {critical:?}");
        assert_eq!(critical[0]["title"], "Alpha: Battery critical");
        assert_eq!(
            critical[0]["body"],
            "The autopilot says Battery 1 is at its emergency level. 10.84 V, 1234 mAh used. Land now."
        );
        assert_eq!(
            b.alert_item("battery"),
            Some((Some(2.0), Some("critical".into())))
        );
    }

    // A second battery's charge state: the alarm names it, and its tiles say it.
    let mut b = bench("Ardupilotmega");
    let pack = vec![
        b.item("battery_2.voltage_v", Some(21.3), None),
        b.item("battery_2.consumed_mah", Some(4000.0), None),
    ];
    b.telemetry(pack);
    b.charge_state("battery_2", 3, "Critical");
    b.hold();
    b.charge_state("battery_2", 3, "Critical");
    let alerts = b.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(
        alerts[0]["body"],
        "The autopilot says Battery 2 is critical. 21.3 V, 4000 mAh used. Land now."
    );
    assert_eq!(
        b.alert_item("battery_2"),
        Some((Some(2.0), Some("critical".into())))
    );
    assert_eq!(b.alert_item("battery"), None);

    // Disarmed, the preflight's: nothing is raised.
    let mut b = bench("Px4");
    b.observe("Px4", false);
    b.charge_state("battery", 3, "Critical");
    b.hold();
    b.charge_state("battery", 3, "Critical");
    assert!(b.alerts().is_empty());
    assert_eq!(b.alert_item("battery"), None);
}

#[test]
fn ardupilots_message_alone_trips_the_alarm_while_armed() {
    let mut b = bench("Ardupilotmega");
    b.readings(11.1, 900.0);
    b.message("Battery 1 is low 10.80V used 1234 mAh");
    b.hold();
    b.readings(11.1, 900.0);
    let alerts = b.notices();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["title"], "Alpha: Battery low");
    assert_eq!(
        alerts[0]["body"],
        "The autopilot says Battery 1 is low. 10.8 V, 1234 mAh used. Bring it back and land.",
        "the autopilot's own numbers"
    );
}

#[test]
fn a_failed_or_unhealthy_battery_is_an_alarm_of_its_own_until_it_clears() {
    let mut b = bench("Px4");
    let mut cells: Vec<TelemetryItem> = (1..=4)
        .map(|n| b.item(&format!("battery.cell_{n}_v"), Some(3.4), None))
        .collect();
    cells.push(b.item("battery.consumed_mah", Some(4200.0), None));
    cells.push(b.item("battery.faults", Some(16.0), Some("Over temperature")));
    b.telemetry(cells);
    b.charge_state("battery", 5, "Failed");
    assert!(b.alerts().is_empty(), "held first");
    b.hold();
    b.charge_state("battery", 5, "Failed");
    let alerts = b.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["condition"], "battery:v:0");
    assert_eq!(alerts[0]["severity"], "alarm", "said aloud");
    assert_eq!(alerts[0]["title"], "Alpha: Battery failed");
    assert_eq!(
        alerts[0]["body"],
        "13.6 V, 4200 mAh used. Faults: Over temperature. Land as soon as it is safe."
    );
    assert_eq!(
        b.alert_item("battery"),
        Some((Some(2.0), Some("failed".into())))
    );
    // Said again with every BATTERY_STATUS: nothing new.
    b.charge_state("battery", 5, "Failed");
    assert_eq!(b.raised(), 1);

    b.charge_state("battery", 1, "OK");
    b.hold();
    b.charge_state("battery", 1, "OK");
    assert!(b.alerts().is_empty());
    assert_eq!(b.alert_item("battery"), Some((None, None)));

    // A second battery's own charge state raises and resolves its notice.
    b.charge_state("battery_2", 6, "Unhealthy");
    b.hold();
    b.charge_state("battery_2", 6, "Unhealthy");
    let alerts = b.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["condition"], "battery:v:1");
    assert_eq!(alerts[0]["title"], "Alpha: Battery 2 unhealthy");
    b.charge_state("battery_2", 1, "OK");
    b.hold();
    b.charge_state("battery_2", 1, "OK");
    assert!(b.alerts().is_empty());
}

/// A battery fault that flickers does not flap (UXD-253): a reading that
/// goes from a fault to well and back every half second raises nothing, a
/// fault that holds 3 s raises one notice, flicker after that keeps it as
/// it is, and it resolves only once the battery has read well for 3 s.
/// `say` gives one reading, true for the fault.
fn a_flickering_fault_is_one_notice(mut b: Bench, say: impl Fn(&mut Bench, bool)) {
    let half = |b: &Bench| {
        b.time.0.fetch_add(S / 2, Ordering::SeqCst);
    };
    let flicker = |b: &mut Bench| {
        for n in 0..20 {
            say(b, n % 2 == 0);
            half(b);
        }
    };
    flicker(&mut b);
    assert!(b.alerts().is_empty(), "nothing while it flickers");
    assert_eq!(b.raised(), 0);

    for _ in 0..6 {
        say(&mut b, true);
        half(&mut b);
    }
    say(&mut b, true);
    let raised = b.alerts();
    assert_eq!(raised.len(), 1, "after its hold: {raised:?}");
    assert_eq!(b.raised(), 1);

    flicker(&mut b);
    say(&mut b, true);
    let kept = b.alerts();
    assert_eq!(kept.len(), 1, "never resolved while it flickers");
    assert_eq!(kept[0]["id"], raised[0]["id"], "never raised again");
    assert_eq!(b.raised(), 1);

    half(&mut b);
    say(&mut b, false);
    for _ in 0..5 {
        half(&mut b);
        say(&mut b, false);
    }
    assert_eq!(b.alerts().len(), 1, "well for 2.5 s: still up");
    half(&mut b);
    say(&mut b, false);
    assert!(b.alerts().is_empty(), "well for 3 s: resolved");
    assert_eq!(b.raised(), 1);
}

#[test]
fn a_charge_state_that_flickers_unhealthy_is_one_notice() {
    a_flickering_fault_is_one_notice(bench("Px4"), |b, fault| {
        if fault {
            b.charge_state("battery", 6, "Unhealthy");
        } else {
            b.charge_state("battery", 1, "OK");
        }
    });
}

#[test]
fn a_health_bit_that_flickers_is_one_notice() {
    let mut b = bench_with("Ardupilotmega", &[("BATT_LOW_VOLT", 10.5)]);
    b.readings(11.6, 300.0);
    a_flickering_fault_is_one_notice(b, |b, fault| {
        b.readings(11.6, 300.0);
        let bit = b.item("battery.healthy", Some(if fault { 0.0 } else { 1.0 }), None);
        b.telemetry(vec![bit]);
    });
}

/// A fault's hold ends on time between battery frames, on whatever
/// telemetry comes next, while its last reading is fresh (5 s). A fault
/// said once and then not again goes stale before anything raises it.
#[test]
fn a_faults_hold_ends_between_battery_frames() {
    let mut b = bench("Px4");
    b.charge_state("battery", 5, "Failed");
    b.hold();
    b.readings(15.0, 100.0);
    assert_eq!(b.alerts().len(), 1, "raised on the next telemetry");
    b.charge_state("battery", 1, "OK");
    b.hold();
    b.readings(15.0, 100.0);
    assert!(b.alerts().is_empty(), "resolved on the next telemetry");

    b.charge_state("battery", 5, "Failed");
    b.time.0.fetch_add(6 * S, Ordering::SeqCst);
    b.readings(15.0, 100.0);
    assert!(b.alerts().is_empty(), "stale before its hold was seen");
}

/// `ArduPilot`'s "Battery 2 is missing" is said once, as its failsafe
/// starts, and nothing but the battery's charge state says when it is
/// back, so it is no fault of its own that could stay up for good.
#[test]
fn no_battery_fault_outlives_its_cause() {
    let mut b = bench("Ardupilotmega");
    b.message("Battery 2 is missing, last: 0.00V used 0 mAh");
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
    b.hold();
    b.readings(12.0, 100.0);
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
    let log = call(
        &mut b.engine,
        &mut b.ui,
        "vehicle/messages",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(
        log["messages"].as_array().unwrap().len(),
        1,
        "a message as before"
    );
    assert_eq!(b.alert_item("battery_2"), None);
}

/// `SYS_STATUS`'s battery bit from an autopilot with no charge state, a
/// real fault: a notice once it has held as long as the battery alarm,
/// resolved when healthy, and nothing for a moment's flicker.
#[test]
fn the_health_bit_raises_after_its_hold_and_resolves_when_healthy() {
    let mut b = bench_with("Ardupilotmega", &[("BATT_LOW_VOLT", 10.5)]);
    b.readings(11.6, 300.0);
    b.telemetry(vec![b.item("battery.healthy", Some(0.0), None)]);
    assert!(b.alerts().is_empty(), "held first");
    b.time.0.fetch_add(S, Ordering::SeqCst);
    b.telemetry(vec![b.item("battery.healthy", Some(1.0), None)]);
    b.hold();
    b.telemetry(vec![b.item("battery.healthy", Some(0.0), None)]);
    assert!(b.alerts().is_empty(), "a flicker starts its hold again");

    b.hold();
    b.telemetry(vec![b.item("battery.healthy", Some(0.0), None)]);
    let alerts = b.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["severity"], "alarm");
    assert_eq!(alerts[0]["title"], "Alpha: Battery unhealthy");
    assert_eq!(
        alerts[0]["body"],
        "11.6 V, 300 mAh used. The autopilot says it is not healthy. Land as soon as it is safe."
    );
    b.telemetry(vec![b.item("battery.healthy", Some(1.0), None)]);
    b.hold();
    b.telemetry(vec![b.item("battery.healthy", Some(1.0), None)]);
    assert!(b.alerts().is_empty());

    // With a charge state, the bit says nothing more.
    let mut b = bench("Ardupilotmega");
    b.charge_state("battery", 1, "OK");
    b.telemetry(vec![b.item("battery.healthy", Some(0.0), None)]);
    b.hold();
    b.telemetry(vec![b.item("battery.healthy", Some(0.0), None)]);
    assert!(b.alerts().is_empty());
}

fn unhealthy(b: &mut Bench) {
    let bit = b.item("battery.healthy", Some(0.0), None);
    b.telemetry(vec![bit]);
}

/// `ArduPilot` sends the bit as `healthy() && !has_failsafed()`, and its
/// battery failsafe stays until it restarts: after a sign of one the bit
/// is no fault until it reads healthy again. A voltage failsafe, then
/// disarmed on the ground as the pack recovers.
#[test]
fn a_voltage_failsafe_then_disarmed_and_recovered_is_no_unhealthy_battery() {
    let mut b = bench_with("Ardupilotmega", &[("BATT_LOW_VOLT", 10.5)]);
    b.readings(10.3, 1500.0);
    b.message("Battery 1 is low 10.30V used 1500 mAh");
    unhealthy(&mut b);
    b.hold();
    b.readings(10.3, 1500.0);
    unhealthy(&mut b);
    assert_eq!(b.alerts().len(), 1);
    assert_eq!(b.alerts()[0]["title"], "Alpha: Battery low");
    b.observe("Ardupilotmega", false);
    b.readings(11.8, 1500.0);
    unhealthy(&mut b);
    b.hold();
    b.readings(11.8, 1500.0);
    unhealthy(&mut b);
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
}

/// A sag below its level clears the bit, and it stays clear after the
/// pack recovers.
#[test]
fn a_sag_that_recovers_is_no_unhealthy_battery() {
    let mut b = bench_with("Ardupilotmega", &[("BATT_LOW_VOLT", 10.5)]);
    b.readings(10.3, 1500.0);
    unhealthy(&mut b);
    b.readings(11.5, 1500.0);
    b.hold();
    unhealthy(&mut b);
    b.hold();
    unhealthy(&mut b);
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
}

/// A failsafe on mAh used, which the voltage never shows: the bit can come
/// before the message, and the hold lets the message say it.
#[test]
fn a_failsafe_on_mah_used_is_no_unhealthy_battery() {
    let mut b = bench_with(
        "Ardupilotmega",
        &[("BATT_LOW_VOLT", 10.5), ("BATT_LOW_MAH", 2000.0)],
    );
    b.readings(11.8, 1500.0);
    unhealthy(&mut b);
    b.time.0.fetch_add(S, Ordering::SeqCst);
    b.message("Battery 1 is low 11.80V used 1500 mAh");
    b.hold();
    b.readings(11.8, 1500.0);
    unhealthy(&mut b);
    let alerts = b.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["title"], "Alpha: Battery low");
    b.observe("Ardupilotmega", false);
    unhealthy(&mut b);
    b.hold();
    unhealthy(&mut b);
    assert!(b.alerts().is_empty(), "{:?}", b.alerts());
    // Restarted, healthy: a later fault is said again.
    b.telemetry(vec![b.item("battery.healthy", Some(1.0), None)]);
    unhealthy(&mut b);
    b.hold();
    unhealthy(&mut b);
    assert_eq!(b.alerts()[0]["title"], "Alpha: Battery unhealthy");
}
