#![allow(
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "in-memory handover regression fixtures"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::LinkStatus;
use uxduck_core::domain::vehicle::Observation;
use uxduck_core::policy::admission::Trust;

use super::*;
use crate::commands::{self, Outcome};
use crate::runtime::{Clock, Forget, Ports, Random};
use crate::{EngineConfig, HostAbout, Loaded};

struct Tick(Arc<AtomicU64>);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
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
            *byte = self.0.to_le_bytes()[7];
        }
    }
}

struct Client {
    connection: ConnectionId,
    inbox: UnboundedReceiver<String>,
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    engine.receive(
        client.connection,
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
    );
    loop {
        let frame: Value = serde_json::from_str(&client.inbox.try_recv().unwrap()).unwrap();
        if frame["id"] == 1 {
            assert!(frame.get("result").is_some(), "{method}: {frame}");
            return frame["result"].clone();
        }
    }
}

fn connect(engine: &mut Engine) -> Client {
    let (outbox, inbox) = unbounded();
    let mut client = Client {
        connection: engine.connect(Transport::Socket, Trust::LocalUser, outbox),
        inbox,
    };
    call(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": {"name": "operator-ui", "version": "0.12.0", "kind": "operator-ui"},
            "capabilities": {"protocolVersion": 1}
        }),
    );
    client
}

struct Fixture {
    engine: Engine,
    client: Client,
    clock: Arc<AtomicU64>,
    link: String,
    operation: String,
}

impl Fixture {
    fn new() -> Self {
        let clock = Arc::new(AtomicU64::new(1_000));
        let mut engine = Engine::new(
            EngineConfig {
                daemon_version: "0.12.0".into(),
                capabilities: DaemonCapabilities {
                    simulator: true,
                    ..DaemonCapabilities::default()
                },
                ui_url: "http://127.0.0.1:45000/".into(),
                checkpoint_every: 1000,
                models: Vec::new(),
                agent_provider: "fake".into(),
                about: HostAbout::default(),
            },
            Box::new(Tick(clock.clone())),
            Box::new(Counter(0)),
            Ports::with_persistence(Box::new(Forget)),
            Loaded::default(),
        );
        let mut client = connect(&mut engine);
        let mut plan = call(
            &mut engine,
            &mut client,
            "plan/create",
            json!({"vehicleType": "plane", "name": "Handover"}),
        );
        let plan_id = plan["id"].clone();
        plan["items"] = json!([
            {"command": 22, "frame": 3, "params": [0, 0, 0, null], "latitudeDeg": -35.36, "longitudeDeg": 149.16, "altitudeM": 80, "autocontinue": true},
            {"command": 16, "frame": 3, "params": [0, 0, 0, null], "latitudeDeg": -35.35, "longitudeDeg": 149.16, "altitudeM": 80, "autocontinue": true}
        ]);
        call(
            &mut engine,
            &mut client,
            "plan/update",
            json!({"id": plan_id, "expectedVersion": 1, "body": plan}),
        );
        let started = call(
            &mut engine,
            &mut client,
            "plan/simulate",
            json!({"runs": [{"planId": plan_id}]}),
        );
        let operation = started["operation"]["id"].as_str().unwrap().to_owned();
        let link = started["operation"]["simulation"]["runs"][0]["linkId"]
            .as_str()
            .unwrap()
            .to_owned();
        engine.state.links.get_mut(&link).unwrap().status = LinkStatus::Up;
        engine.vehicle_observed(Observation {
            id: "plane".into(),
            aliases: Vec::new(),
            link_id: link.clone(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "FixedWing".into(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 1_000,
            armed: true,
            mode: Some("AUTO".into()),
        });
        engine.plan_runs.running.get_mut(&link).unwrap().step = Step::Flying;
        engine.update_run(&link, None, |run| run.state = RunState::Flying);
        engine.controller_connected("pad", "Synthetic controller", true);
        Self {
            engine,
            client,
            clock,
            link,
            operation,
        }
    }

    fn claim(&mut self) {
        call(
            &mut self.engine,
            &mut self.client,
            "controller/claim",
            json!({"id": "pad", "mode": "pinned", "vehicleId": "plane"}),
        );
    }

    fn add_other_vehicle(&mut self) {
        let mut link = self.engine.state.links[&self.link].clone();
        link.id = "other-link".into();
        self.engine.state.links.insert(link.id.clone(), link);
        let mut vehicle = self.engine.state.vehicles["plane"].clone();
        vehicle.id = "other".into();
        vehicle.link_id = "other-link".into();
        self.engine
            .state
            .vehicles
            .insert(vehicle.id.clone(), vehicle);
    }

    fn send_mode(&mut self, connection: ConnectionId, mode: &str) -> String {
        commands::send_checked(
            &mut self.engine,
            connection,
            serde_json::from_value(json!({
                "vehicleId": "plane", "command": {"type": "set-mode", "mode": mode}
            }))
            .unwrap(),
            &[Capability::Control],
            true,
        )
        .unwrap()
        .1
        .unwrap()
    }

    fn mode(&mut self, mode: &str) {
        let vehicle = self.engine.state.vehicles["plane"].clone();
        self.engine.vehicle_observed(Observation {
            id: vehicle.id,
            aliases: vehicle.aliases,
            link_id: vehicle.link_id,
            peer: vehicle.peer,
            system_id: vehicle.system_id,
            component_ids: vehicle.component_ids,
            autopilot: vehicle.autopilot,
            vehicle_type: vehicle.vehicle_type,
            uid: vehicle.uid,
            firmware: vehicle.firmware,
            firmware_commit: vehicle.firmware_commit,
            presence: vehicle.presence,
            last_heartbeat_us: vehicle.last_heartbeat_us,
            armed: vehicle.armed,
            mode: Some(mode.into()),
        });
    }

    fn run(&self) -> &SimulationRun {
        &self.engine.state.operations[&self.operation]
            .simulation
            .as_ref()
            .unwrap()
            .runs[0]
    }

    fn stopped(&self) -> bool {
        self.engine.state.links[&self.link].status == LinkStatus::Stopped
    }
}

#[test]
fn accepted_controller_handover_ends_only_the_automated_run() {
    let mut f = Fixture::new();
    f.claim();
    f.mode("AUTO");
    assert_eq!(f.run().state, RunState::Flying);
    let receipt = f.send_mode(f.client.connection, "FBWA");
    assert_eq!(f.run().state, RunState::Flying);
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::EndedEarly);
    assert!(f.run().detail.as_deref().unwrap().contains("control"));
    assert!(!f.stopped());
    assert!(!f.engine.plan_runs.running.contains_key(&f.link));
    f.mode("FBWA");
    f.clock.store(10_000_000_000, Ordering::Relaxed);
    f.engine.drive_plan_runs();
    assert!(!f.stopped());
    assert_eq!(f.engine.state.vehicles["plane"].presence, Presence::Online);
    assert!(f.engine.state.vehicles["plane"].armed);
    assert_eq!(
        f.engine.state.controllers["pad"].target.as_deref(),
        Some("plane")
    );
    call(
        &mut f.engine,
        &mut f.client,
        "sim/stop",
        json!({"id": f.link}),
    );
    assert!(f.stopped());
}

#[test]
fn a_mode_heartbeat_before_its_acknowledgement_does_not_stop_the_simulator() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.mode("FBWA");
    assert!(!f.stopped());
    assert_eq!(f.run().state, RunState::Flying);
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::EndedEarly);
    assert!(!f.stopped());
}

#[test]
fn a_refused_controller_handover_keeps_the_existing_non_auto_cleanup() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine
        .command_result(&receipt, Outcome::Rejected("Not allowed".into()), 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
    assert_eq!(f.run().state, RunState::EndedEarly);
}

#[test]
fn unclaimed_foreign_and_pre_claim_commands_cannot_hand_over_a_run() {
    for case in ["unclaimed", "foreign", "pre-claim"] {
        let mut f = Fixture::new();
        if case != "pre-claim" && case != "unclaimed" {
            f.claim();
        }
        let other = connect(&mut f.engine);
        let connection = if case == "foreign" {
            other.connection
        } else {
            f.client.connection
        };
        let receipt = f.send_mode(connection, "FBWA");
        if case == "pre-claim" {
            f.claim();
        }
        f.engine.command_result(&receipt, Outcome::Accepted, 1);
        assert_eq!(f.run().state, RunState::Flying, "{case}");
        f.mode("FBWA");
        assert!(f.stopped(), "{case}");
    }
}

#[test]
fn releasing_and_reclaiming_does_not_reuse_the_old_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    call(
        &mut f.engine,
        &mut f.client,
        "controller/release",
        json!({"id": "pad"}),
    );
    f.claim();
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn controller_loss_does_not_accept_a_pending_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine.controller_disconnected("pad");
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn an_accepted_auto_command_is_not_a_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "AUTO");
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    f.mode("AUTO");
    assert_eq!(f.run().state, RunState::Flying);
    assert!(!f.stopped());
}

#[test]
fn a_claim_for_another_target_cannot_hand_over_this_run() {
    let mut f = Fixture::new();
    f.add_other_vehicle();
    call(
        &mut f.engine,
        &mut f.client,
        "controller/claim",
        json!({"id": "pad", "mode": "pinned", "vehicleId": "other"}),
    );
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn retargeting_away_and_back_invalidates_the_old_handover() {
    let mut f = Fixture::new();
    f.add_other_vehicle();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    for target in ["other", "plane"] {
        call(
            &mut f.engine,
            &mut f.client,
            "controller/setTarget",
            json!({"id": "pad", "mode": "pinned", "vehicleId": target}),
        );
    }
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn a_replaced_mode_order_cannot_hand_over_the_run_late() {
    let mut f = Fixture::new();
    f.claim();
    let old = f.send_mode(f.client.connection, "FBWA");
    let new = f.send_mode(f.client.connection, "AUTO");
    f.engine.command_result(&old, Outcome::Accepted, 1);
    f.engine.command_result(&new, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn holding_display_changes_do_not_invalidate_the_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine.controller_holding("pad", Command::Rtl, 2_000);
    f.engine.controller_let_go("pad");
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::EndedEarly);
    assert!(!f.stopped());
}

#[test]
fn a_pending_handover_does_not_hide_link_loss() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine.state.vehicles.get_mut("plane").unwrap().presence = Presence::Offline;
    f.clock.store(SILENT_US + 2_000, Ordering::Relaxed);
    f.mode("FBWA");
    assert!(f.stopped());
    assert_eq!(
        f.run().detail.as_deref(),
        Some("Its simulator stopped answering.")
    );
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert!(f.stopped());
}

#[test]
fn a_pending_handover_does_not_hide_disarming_or_run_timeout() {
    for failure in ["disarm", "timeout"] {
        let mut f = Fixture::new();
        f.claim();
        let receipt = f.send_mode(f.client.connection, "FBWA");
        if failure == "disarm" {
            f.engine.state.vehicles.get_mut("plane").unwrap().armed = false;
        } else {
            f.clock.store(4_000_000_000, Ordering::Relaxed);
        }
        f.mode("FBWA");
        assert!(f.stopped(), "{failure}");
        assert_eq!(
            f.run().state,
            if failure == "disarm" {
                RunState::EndedEarly
            } else {
                RunState::TimedOut
            },
            "{failure}"
        );
        f.engine.command_result(&receipt, Outcome::Accepted, 1);
        assert!(f.stopped(), "{failure}");
    }
}

#[test]
fn accepted_ack_checks_failures_even_without_an_intervening_observation() {
    for failure in ["disarm", "timeout", "link-loss", "completed", "breached"] {
        let mut f = Fixture::new();
        f.claim();
        let receipt = f.send_mode(f.client.connection, "FBWA");
        match failure {
            "disarm" => f.engine.state.vehicles.get_mut("plane").unwrap().armed = false,
            "timeout" => f.clock.store(4_000_000_000, Ordering::Relaxed),
            "link-loss" => {
                f.engine.state.vehicles.get_mut("plane").unwrap().presence = Presence::Offline;
                f.clock.store(SILENT_US + 2_000, Ordering::Relaxed);
            }
            "completed" => {
                f.engine
                    .plan_runs
                    .running
                    .get_mut(&f.link)
                    .unwrap()
                    .complete_at_us = Some(1_000);
            }
            "breached" => f
                .engine
                .update_run(&f.link, None, |run| run.breached = true),
            _ => unreachable!(),
        }
        f.engine.command_result(&receipt, Outcome::Accepted, 1);
        assert!(
            !f.run()
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("You took control")),
            "{failure}"
        );
        if matches!(failure, "completed" | "breached") {
            assert_eq!(f.run().state, RunState::Flying, "{failure}");
            assert!(!f.stopped(), "{failure}");
        } else {
            assert!(f.stopped(), "{failure}");
        }
    }
}

#[test]
fn simulator_pause_and_resume_invalidates_pending_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    for paused in [true, false] {
        call(
            &mut f.engine,
            &mut f.client,
            "sim/pause",
            json!({"id": f.link, "paused": paused}),
        );
    }
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}

#[test]
fn stopped_and_restarted_simulations_ignore_late_handover_ack() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    call(
        &mut f.engine,
        &mut f.client,
        "sim/stop",
        json!({"id": f.link}),
    );
    call(
        &mut f.engine,
        &mut f.client,
        "sim/restart",
        json!({"id": f.link}),
    );
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Stopped);
    assert!(!f.engine.plan_runs.running.contains_key(&f.link));
    assert!(!f.stopped());
    f.engine.drive_plan_runs();
    assert_eq!(f.run().state, RunState::Stopped);
}

#[test]
fn a_disconnected_window_cannot_complete_its_pending_handover() {
    let mut f = Fixture::new();
    f.claim();
    let receipt = f.send_mode(f.client.connection, "FBWA");
    f.engine.disconnect(f.client.connection);
    f.client = connect(&mut f.engine);
    call(
        &mut f.engine,
        &mut f.client,
        "controller/takeOver",
        json!({"id": "pad"}),
    );
    f.engine.command_result(&receipt, Outcome::Accepted, 1);
    assert_eq!(f.run().state, RunState::Flying);
    f.mode("FBWA");
    assert!(f.stopped());
}
