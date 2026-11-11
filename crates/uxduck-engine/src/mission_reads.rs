use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;

use crate::{ConnectionId, Engine};

#[cfg(test)]
pub(crate) mod fixtures;

impl Engine {
    pub(crate) fn reads_mission_float_bits(&self, connection: ConnectionId) -> bool {
        self.connections
            .get(&connection)
            .and_then(|state| state.admitted.as_ref())
            .is_some_and(|admitted| admitted.reads.mission_read_float_bits)
    }

    pub(crate) fn require_mission_float_bits(
        &self,
        connection: ConnectionId,
    ) -> Result<(), ApiError> {
        if self.reads_mission_float_bits(connection) {
            return Ok(());
        }
        Err(ApiError::new(
            ErrorKind::UpdateRequired,
            "This client cannot read exact mission values. Update it to support missionReadFloatBits before requesting mission data.",
        ))
    }

    pub(crate) fn check_mission_read_request(
        &self,
        connection: ConnectionId,
        request: &ClientRequest,
    ) -> Result<(), ApiError> {
        let required = match request {
            ClientRequest::StateSubscribe(params) => params
                .entities
                .contains(&uxduck_contracts::dto::state::EntityType::Mission),
            ClientRequest::MissionRefresh(_)
            | ClientRequest::MissionHistory(_)
            | ClientRequest::MissionUpload(_)
            | ClientRequest::MissionRestore(_)
            | ClientRequest::PlanUpload(_) => true,
            _ => false,
        };
        if required {
            self.require_mission_float_bits(connection)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use super::*;
    use crate::runtime::{Clock, Forget, Ports, Random};
    use crate::{EngineConfig, Loaded};
    use futures::channel::mpsc::unbounded;
    use serde_json::{Value, json};
    use uxduck_contracts::dto::initialize::DaemonCapabilities;
    use uxduck_contracts::dto::state::EntityType;
    use uxduck_contracts::dto::trace::Transport;
    use uxduck_core::events::{Actor, Change};
    use uxduck_core::policy::admission::Trust;

    struct Time;
    impl Clock for Time {
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

    #[test]
    fn existing_unsupported_subscription_gets_empty_contiguous_deltas_and_denial_keeps_it() {
        let mut engine = Engine::new(
            EngineConfig {
                daemon_version: "0.13.0".into(),
                capabilities: DaemonCapabilities::default(),
                ui_url: "http://127.0.0.1:45000".into(),
                checkpoint_every: 1000,
                models: vec![],
                agent_provider: "fake".into(),
                about: crate::HostAbout::default(),
            },
            Box::new(Time),
            Box::new(Counter(0)),
            Ports::with_persistence(Box::new(Forget)),
            Loaded::default(),
        );
        let (outbox, mut inbox) = unbounded();
        let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
        engine.receive(
            id,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "clientInfo": { "name": "old-reader", "version": "0.13.0", "kind": "operator-cli" },
            "capabilities": { "protocolVersion": 1 },
        } })
            .to_string(),
        );
        while inbox.try_recv().is_ok() {}
        let subscribed =
            std::collections::BTreeSet::from([EntityType::Mission, EntityType::Vehicle]);
        engine.subscribers.insert(id, subscribed.clone());
        engine.receive(
            id,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "state/subscribe", "params": {
            "entities": ["mission", "operator"],
        } })
            .to_string(),
        );
        assert_eq!(engine.subscribers.get(&id), Some(&subscribed));
        let rejected: Vec<Value> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|text| serde_json::from_str(&text).expect("JSON"))
            .collect();
        assert!(
            rejected
                .iter()
                .any(|frame| frame["error"]["data"]["kind"] == "update-required")
        );
        let mission: uxduck_core::domain::mission::Mission = serde_json::from_value(json!({
            "vehicleId": "v", "items": [], "status": "synced", "version": 1,
        }))
        .expect("mission");
        let before = engine.state.seq;
        for version in [1, 2] {
            engine.commit_as(
                Actor {
                    client_id: "test".into(),
                    operator_id: None,
                    identity: None,
                },
                Change::MissionUpserted {
                    mission: uxduck_core::domain::mission::Mission {
                        version,
                        ..mission.clone()
                    },
                },
            );
        }
        let deltas: Vec<Value> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|text| serde_json::from_str::<Value>(&text).expect("JSON"))
            .filter(|frame| frame["method"] == "state/delta")
            .collect();
        assert_eq!(deltas.len(), 2);
        for (index, delta) in deltas.iter().enumerate() {
            assert_eq!(
                delta["params"]["seq"],
                before + u64::try_from(index).expect("index") + 1
            );
            assert_eq!(delta["params"]["changes"], json!([]));
        }
    }

    #[test]
    fn valid_targets_are_refused_before_orders_state_or_authority_change() {
        use super::fixtures;
        for kind in ["operator-cli", "agent", "third-party", "exporter"] {
            for support in [None, Some(false)] {
                let (mut engine, _ui, orders, plan) = fixtures::setup();
                let mut peer = fixtures::connect(&mut engine, kind, support);
                let subscribed = fixtures::call(
                    &mut engine,
                    &mut peer,
                    "state/subscribe",
                    json!({ "entities": ["vehicle"] }),
                );
                assert_eq!(subscribed["result"]["snapshot"]["vehicles"][0]["id"], "v");
                let before = engine.state.clone();
                let authority = engine.granted(peer.id);
                let subscription = engine.subscribers.get(&peer.id).cloned();
                let items = serde_json::to_value(&engine.state.missions["v"].items).expect("items");
                for (method, params) in [
                    (
                        "state/subscribe",
                        json!({ "entities": ["mission", "operator"] }),
                    ),
                    ("mission/refresh", json!({ "id": "v" })),
                    ("mission/history", json!({ "id": "v" })),
                    (
                        "mission/upload",
                        json!({ "vehicleId": "v", "expectedVersion": 1, "items": items }),
                    ),
                    ("mission/restore", json!({ "recordId": "record" })),
                    ("plan/upload", json!({ "planId": plan, "vehicleId": "v" })),
                ] {
                    let reply = fixtures::call(&mut engine, &mut peer, method, params);
                    assert_eq!(
                        reply["error"]["data"]["kind"], "update-required",
                        "{kind}/{support:?}/{method}: {reply}"
                    );
                    assert!(reply.get("result").is_none());
                    assert_eq!(engine.state, before, "{method} changed domain state");
                    assert_eq!(engine.granted(peer.id), authority);
                    assert_eq!(engine.subscribers.get(&peer.id), subscription.as_ref());
                    assert!(orders.take().is_empty(), "{method} issued an order");
                }
            }
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one subscription and publication protocol scenario"
    )]
    fn tagged_snapshots_and_mixed_global_ticks_reach_only_capable_readers() {
        use super::fixtures;
        let (mut engine, mut capable, orders, _plan) = fixtures::setup();
        let mut old = fixtures::connect(&mut engine, "operator-cli", None);
        engine.state.missions.get_mut("v").expect("mission").items[0].param4 =
            f64::from_bits(0xfff8_0000_0000_0042);
        engine
            .state
            .mission_records
            .get_mut("record")
            .expect("record")
            .items[0]
            .param4 = f64::from_bits(0x7ff8_0000_0000_0001);
        let snapshot = fixtures::call(
            &mut engine,
            &mut capable,
            "state/subscribe",
            json!({ "entities": ["vehicle", "mission"] }),
        );
        assert_eq!(
            snapshot["result"]["snapshot"]["missions"][0]["items"][0]["param4"]["nonFiniteF64"],
            "fff8000000000042"
        );
        let history = fixtures::call(
            &mut engine,
            &mut capable,
            "mission/history",
            json!({ "id": "v" }),
        );
        assert_eq!(
            history["result"]["records"][0]["items"][0]["param4"]["nonFiniteF64"],
            "7ff8000000000001"
        );
        assert!(orders.take().is_empty());
        let refreshed = fixtures::call(
            &mut engine,
            &mut capable,
            "mission/refresh",
            json!({ "id": "v" }),
        );
        assert_eq!(
            refreshed["result"]["items"][0]["param4"]["nonFiniteF64"],
            "fff8000000000042"
        );
        assert_eq!(
            orders.take(),
            ["DownloadMission { vehicle_id: \"v\" }".to_owned()]
        );
        engine.subscribers.insert(
            old.id,
            std::collections::BTreeSet::from([EntityType::Mission, EntityType::Vehicle]),
        );
        fixtures::frames(&mut old);
        fixtures::frames(&mut capable);
        let before = engine.state.seq;
        let actor = Actor {
            client_id: "test".into(),
            operator_id: None,
            identity: None,
        };
        for version in [2, 3] {
            let mut mission = engine.state.missions["v"].clone();
            mission.version = version;
            engine.commit_as(actor.clone(), Change::MissionUpserted { mission });
        }
        engine.commit_as(
            actor,
            Change::VehicleUpserted {
                vehicle: engine.state.vehicles["v"].clone(),
            },
        );
        let old_ticks: Vec<Value> = fixtures::frames(&mut old)
            .into_iter()
            .filter(|frame| frame["method"] == "state/delta")
            .collect();
        let new_ticks: Vec<Value> = fixtures::frames(&mut capable)
            .into_iter()
            .filter(|frame| frame["method"] == "state/delta")
            .collect();
        assert_eq!(old_ticks.len(), 3);
        assert_eq!(new_ticks.len(), 3);
        for (index, (old, new)) in old_ticks.iter().zip(&new_ticks).enumerate() {
            let seq = before + u64::try_from(index).expect("index") + 1;
            assert_eq!(old["params"]["seq"], seq);
            assert_eq!(new["params"]["seq"], seq);
            if index < 2 {
                assert_eq!(old["params"]["changes"], json!([]));
                assert_eq!(
                    new["params"]["changes"][0]["mission"]["items"][0]["param4"]["nonFiniteF64"],
                    "fff8000000000042"
                );
            } else {
                assert_eq!(old["params"]["changes"], new["params"]["changes"]);
                assert_eq!(old["params"]["changes"][0]["entity"], "vehicle");
            }
        }
    }
}
