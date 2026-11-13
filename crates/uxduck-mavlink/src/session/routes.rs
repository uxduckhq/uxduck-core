use super::{
    BTreeMap, CommandOutcome, Config, Effect, Failure, Message, Observation, Presence, Produced,
    REORDERING_MS, Session, files, part_effect, presence_at,
};
use std::collections::BTreeSet;
use uxduck_core::domain::link::{Link, LinkPolicy};
use uxduck_core::domain::vehicle::{VehicleConnections, VehicleRoute};

type Address = (String, String, u8);
const RECOVERED_US: u64 = 10_000_000;

#[derive(Debug, Default)]
pub(super) struct Routing {
    pub replay: bool,
    pub command: BTreeMap<String, Address>,
    pub telemetry: BTreeMap<String, Address>,
    order: Vec<String>,
    links: BTreeMap<String, (bool, u64)>,
    outside_preference: BTreeSet<String>,
    unavailable: BTreeSet<String>,
    send_failed: BTreeSet<String>,
    send_failed_peers: BTreeSet<(String, String)>,
    healthy_since: BTreeMap<Address, u64>,
    health: BTreeMap<Address, uxduck_core::domain::telemetry::LinkHealth>,
    published: BTreeMap<String, Observation>,
    epochs: BTreeMap<String, u64>,
    reasons: BTreeMap<String, String>,
    /// Per-vehicle timestamps, never sequence numbers from different channels.
    telemetry_boot: BTreeMap<(String, u32), u32>,
    switched_at: BTreeMap<String, u64>,
}

impl Session {
    /// Recorded sources are independent comparison timelines, even when they
    /// contain the same UID. They must not select or suppress each other.
    #[must_use]
    pub fn for_replay(config: Config) -> Self {
        let mut session = Self::new(config);
        session.routing.replay = true;
        session
    }
    #[must_use]
    pub fn redundant_sources(&self, link: &str, peer: &str, system: u8) -> Vec<(String, String)> {
        let Some(vehicle) = self
            .vehicles
            .get(&(link.into(), peer.into(), system))
            .filter(|v| v.observation.uid.is_some())
        else {
            return Vec::new();
        };
        self.vehicles
            .iter()
            .filter(|(_, v)| v.observation.id == vehicle.observation.id)
            .map(|(key, _)| (key.0.clone(), key.1.clone()))
            .collect()
    }

    pub fn set_connection_order(&mut self, order: Vec<String>) {
        self.routing.order = order;
    }

    pub fn configure_link(&mut self, link: &Link) {
        if link.kind.is_forward() || link.kind.is_simulator() {
            self.routing.outside_preference.insert(link.id.clone());
        } else {
            self.routing.outside_preference.remove(&link.id);
        }
        self.routing.links.insert(
            link.id.clone(),
            (
                link.policy == LinkPolicy::ReadWrite && !link.kind.is_forward(),
                link.created_at_us,
            ),
        );
        if link.status.is_off() || link.kind.is_forward() {
            self.routing.unavailable.insert(link.id.clone());
        }
    }

    /// Invalidate the old socket's evidence, without forgetting the vehicle
    /// or its unavailable alternatives. A new generation needs fresh traffic.
    pub fn transport_lost(&mut self, link_id: &str, now_us: u64) -> Vec<Effect> {
        self.routing.unavailable.insert(link_id.to_owned());
        for (key, vehicle) in &mut self.vehicles {
            if key.0 == link_id {
                vehicle.observation.last_heartbeat_us = 0;
                vehicle.observation.presence = Presence::Offline;
                vehicle.pending = Produced::default();
                self.routing.healthy_since.remove(key);
            }
        }
        self.reconcile(Vec::new(), now_us)
    }

    pub fn transport_restored(&mut self, link_id: &str) {
        self.routing.unavailable.remove(link_id);
    }

    pub fn transport_send_health(&mut self, link: &str, failed: bool) {
        if failed {
            self.routing.send_failed.insert(link.to_owned());
        } else {
            self.routing.send_failed.remove(link);
        }
    }

    /// A listener answers each peer from one socket, so a failed send
    /// blocks only the routes through the peers it failed for.
    pub fn transport_peer_send_health(&mut self, link: &str, failed: BTreeSet<String>) {
        self.routing.send_failed_peers.retain(|(l, _)| l != link);
        self.routing
            .send_failed_peers
            .extend(failed.into_iter().map(|peer| (link.to_owned(), peer)));
    }

    pub fn route_health(
        &mut self,
        address: Address,
        health: uxduck_core::domain::telemetry::LinkHealth,
    ) {
        self.routing.health.insert(address, health);
    }
    #[must_use]
    pub fn telemetry_address(&self, id: &str) -> Option<&Address> {
        self.routing.telemetry.get(id)
    }
    fn rank(&self, link: &str) -> u32 {
        let mut links = self
            .routing
            .links
            .keys()
            .cloned()
            .chain(self.vehicles.keys().map(|k| k.0.clone()))
            .chain(self.routing.order.iter().cloned())
            .filter(|id| !self.routing.outside_preference.contains(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        links.sort_by_key(|l| self.priority(l));
        u32::try_from(links.iter().position(|l| l == link).unwrap_or(0) + 1).unwrap_or(u32::MAX)
    }
    fn priority(&self, link: &str) -> (usize, u64, String) {
        (
            self.routing
                .order
                .iter()
                .position(|id| id == link)
                .unwrap_or(self.routing.order.len()),
            self.routing
                .links
                .get(link)
                .map_or(0, |(_, created)| *created),
            link.to_owned(),
        )
    }
    /// Where a command goes: the selected route, else, for a vehicle no
    /// route reaches now, the usable address it was last heard on, so a
    /// command sent without a recent heartbeat, such as RTL, still goes out.
    pub(super) fn command_address(&self, vehicle_id: &str) -> Option<Address> {
        self.find(vehicle_id).or_else(|| {
            self.vehicles
                .iter()
                .filter(|(_, v)| {
                    v.observation.id == vehicle_id
                        || v.observation.aliases.iter().any(|a| a == vehicle_id)
                })
                .max_by_key(|(key, v)| {
                    (
                        !self.routing.unavailable.contains(&key.0) && self.writable(key),
                        v.observation.last_heartbeat_us,
                    )
                })
                .map(|(key, _)| key.clone())
        })
    }
    fn send_failed(&self, key: &Address) -> bool {
        self.routing.send_failed.contains(&key.0)
            || self
                .routing
                .send_failed_peers
                .contains(&(key.0.clone(), key.1.clone()))
    }
    fn writable(&self, key: &Address) -> bool {
        !self.send_failed(key)
            && self
                .routing
                .links
                .get(&key.0)
                .is_none_or(|(writable, _)| *writable)
    }
    fn reachable(&self, key: &Address, now_us: u64) -> bool {
        !self.routing.unavailable.contains(&key.0)
            && self.vehicles.get(key).is_some_and(|v| {
                self.paused.contains(&key.0)
                    || (v.observation.presence != Presence::Offline
                        && presence_at(self.config, v.observation.last_heartbeat_us, now_us)
                            == Presence::Online)
            })
    }

    pub(super) fn accept_telemetry(
        &mut self,
        key: &Address,
        message: &Message,
        received_us: u64,
    ) -> bool {
        if self.routing.replay {
            return true;
        }
        let Some(vehicle) = self.vehicles.get(key) else {
            return true;
        };
        let id = &vehicle.observation.id;
        if self
            .routing
            .telemetry
            .get(id)
            .is_some_and(|selected| selected != key)
        {
            return false;
        }
        if self
            .routing
            .switched_at
            .get(id)
            .is_some_and(|at| received_us < *at)
        {
            return false;
        }
        let (kind, boot) = match message {
            Message::Attitude { time_boot_ms, .. } => (30, *time_boot_ms),
            Message::AttitudeQuaternion { time_boot_ms, .. } => (31, *time_boot_ms),
            Message::GlobalPositionInt { time_boot_ms, .. } => (33, *time_boot_ms),
            _ => return true,
        };
        let entry = self
            .routing
            .telemetry_boot
            .entry((id.clone(), kind))
            .or_insert(boot);
        // During handover an older sample is not a reboot. On a stable path
        // only reordering is dropped: further back is a restart, which the
        // reboot detector must see.
        let handover = self
            .routing
            .switched_at
            .get(id)
            .is_some_and(|at| received_us.saturating_sub(*at) < RECOVERED_US);
        if boot < *entry && (handover || entry.saturating_sub(boot) <= REORDERING_MS) {
            return false;
        }
        *entry = boot;
        true
    }

    /// One aggregate observation per UID, independent of arrival order.
    #[allow(
        clippy::too_many_lines,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "one aggregate selection and bounded wire health projection per vehicle"
    )]
    pub(super) fn reconcile(&mut self, mut effects: Vec<Effect>, now_us: u64) -> Vec<Effect> {
        effects.append(&mut self.deferred_effects);
        if self.routing.replay {
            return effects;
        }
        let observed = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Vehicle(v) => Some(v.id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        effects.retain(|effect| !matches!(effect, Effect::Vehicle(_)));
        let mut grouped: BTreeMap<String, Vec<Address>> = BTreeMap::new();
        for (key, vehicle) in &self.vehicles {
            grouped
                .entry(vehicle.observation.id.clone())
                .or_default()
                .push(key.clone());
        }
        self.routing
            .command
            .retain(|id, _| grouped.contains_key(id));
        self.routing
            .telemetry
            .retain(|id, _| grouped.contains_key(id));
        for (id, mut keys) in grouped {
            keys.sort_by_key(|key| (self.priority(&key.0), key.1.clone(), key.2));
            for key in &keys {
                if self.reachable(key, now_us) && !self.send_failed(key) {
                    self.routing
                        .healthy_since
                        .entry(key.clone())
                        .or_insert(now_us);
                } else {
                    self.routing.healthy_since.remove(key);
                }
            }
            let previous = self.routing.command.get(&id).cloned();
            let best = keys
                .iter()
                .find(|key| self.reachable(key, now_us) && self.writable(key))
                .cloned();
            let current_usable = previous
                .as_ref()
                .is_some_and(|key| self.reachable(key, now_us) && self.writable(key));
            let selected = if current_usable && best != previous {
                // Returning to preference waits for stable health and an idle,
                // disarmed vehicle. Failure always permits immediate handover.
                let stable = best.as_ref().is_some_and(|key| {
                    self.routing
                        .healthy_since
                        .get(key)
                        .is_some_and(|at| now_us.saturating_sub(*at) >= RECOVERED_US)
                });
                let busy = previous.as_ref().is_some_and(|key| self.route_busy(key));
                let armed = previous
                    .as_ref()
                    .and_then(|key| self.vehicles.get(key))
                    .is_some_and(|v| v.observation.armed);
                if stable && !busy && !armed {
                    best
                } else {
                    previous.clone()
                }
            } else {
                best
            };
            if selected != previous {
                if let (Some(old), Some(new)) = (&previous, &selected)
                    && let Some(source) = self.vehicles.get(old).cloned()
                    && let Some(target) = self.vehicles.get_mut(new)
                    && source.observation.uid.is_some()
                    && source.observation.id == target.observation.id
                {
                    // Vehicle state belongs to its reliable UID. Protocol transactions
                    // and queued controls remain tied to the old route and are ended.
                    target.rc_input = source.rc_input;
                    target.boot_instant_us = source.boot_instant_us;
                    target.boot_announced = source.boot_announced;
                    target.last_time_boot_ms = source.last_time_boot_ms;
                    target.plan_ids = source.plan_ids;
                }
                if let Some(old) = &previous
                    && (selected.is_some() || self.routing.unavailable.contains(&old.0))
                {
                    effects.extend(self.interrupt_route(old));
                }
                *self.routing.epochs.entry(id.clone()).or_default() += 1;
                let reason = match (&previous, &selected) {
                    (Some(old), Some(new)) => format!(
                        "Using {} because {}",
                        new.0,
                        if current_usable {
                            "the preferred connection recovered".into()
                        } else {
                            format!("{} stopped responding or became unavailable", old.0)
                        }
                    ),
                    (None, Some(new)) => format!(
                        "Using the highest-preference available connection {}",
                        new.0
                    ),
                    (_, None) => "No command connection is available".into(),
                };
                self.routing.reasons.insert(id.clone(), reason);
            }
            if let Some(key) = &selected {
                self.routing.command.insert(id.clone(), key.clone());
            } else {
                self.routing.command.remove(&id);
            }
            let telemetry = selected
                .clone()
                .or_else(|| keys.iter().find(|key| self.reachable(key, now_us)).cloned());
            if self.routing.telemetry.get(&id) != telemetry.as_ref() {
                if self.routing.telemetry.contains_key(&id) && telemetry.is_some() {
                    self.routing.switched_at.insert(id.clone(), now_us);
                    for key in &keys {
                        if let Some(v) = self.vehicles.get_mut(key) {
                            v.pending = Produced::default();
                        }
                    }
                }
                if let Some(key) = &telemetry {
                    self.routing.telemetry.insert(id.clone(), key.clone());
                } else {
                    self.routing.telemetry.remove(&id);
                }
            }
            let projected = telemetry
                .as_ref()
                .or(previous.as_ref())
                .filter(|key| self.vehicles.contains_key(*key))
                .unwrap_or(&keys[0]);
            let mut observation = self.vehicles[projected].observation.clone();
            observation.last_heartbeat_us = keys
                .iter()
                .filter_map(|key| self.vehicles.get(key))
                .map(|v| v.observation.last_heartbeat_us)
                .max()
                .unwrap_or(0);
            observation.presence = if telemetry.is_some() {
                Presence::Online
            } else if keys.iter().any(|key| {
                !self.routing.unavailable.contains(&key.0)
                    && presence_at(
                        self.config,
                        self.vehicles[key].observation.last_heartbeat_us,
                        now_us,
                    ) == Presence::Stale
            }) {
                Presence::Stale
            } else {
                Presence::Offline
            };
            observation.aliases = keys
                .iter()
                .flat_map(|key| self.vehicles[key].observation.aliases.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            observation.connections = Some(VehicleConnections {
                routes: keys
                    .iter()
                    .map(|key| {
                        let reachable = self.reachable(key, now_us);
                        let writable = self.writable(key);
                        VehicleRoute {
                            link_id: key.0.clone(),
                            peer: key.1.clone(),
                            system_id: key.2,
                            priority: self.rank(&key.0),
                            reachable,
                            writable,
                            selected: selected.as_ref() == Some(key),
                            telemetry: telemetry.as_ref() == Some(key),
                            last_heartbeat_us: self.vehicles[key].observation.last_heartbeat_us,
                            round_trip_us: self
                                .routing
                                .health
                                .get(key)
                                .and_then(|h| h.round_trip_ms)
                                .map(|v| (v * 1000.0).round().max(0.0) as u64),
                            loss_per_mille: self
                                .routing
                                .health
                                .get(key)
                                .and_then(|h| h.loss_percent)
                                .map(|v| (v * 10.0).round().clamp(0.0, 1000.0) as u32),
                            last_receive_age_ms: self
                                .routing
                                .health
                                .get(key)
                                .and_then(|h| h.age_ms),
                            reason: if !reachable {
                                "Not responding"
                            } else if !writable {
                                "Receiving only"
                            } else if selected.as_ref() == Some(key) {
                                "In use"
                            } else {
                                "Available"
                            }
                            .into(),
                        }
                    })
                    .collect(),
                command_available: selected.is_some(),
                reason: self
                    .routing
                    .reasons
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "No command connection is available".into()),
                epoch: self.routing.epochs.get(&id).copied().unwrap_or(0),
            });
            let changed = self.routing.published.get(&id).is_none_or(|old| {
                let mut before = old.connections.clone();
                let mut after = observation.connections.clone();
                if let Some(c) = &mut before {
                    for r in &mut c.routes {
                        r.last_heartbeat_us = 0;
                    }
                }
                if let Some(c) = &mut after {
                    for r in &mut c.routes {
                        r.last_heartbeat_us = 0;
                    }
                }
                before != after || old.presence != observation.presence || observed.contains(&id)
            });
            if changed {
                self.routing
                    .published
                    .insert(id.clone(), observation.clone());
                effects.push(Effect::Vehicle(Box::new(observation)));
            }
        }
        effects
    }

    fn route_busy(&self, key: &Address) -> bool {
        self.commands.iter().any(|p| p.key == *key)
            || self.vehicles.get(key).is_some_and(|v| {
                v.params.is_some()
                    || v.ftp.busy()
                    || !v.batch.is_empty()
                    || !v.writes.is_empty()
                    || v.mission_upload.is_some()
                    || v.mission_download.is_some()
                    || v.manual_sent.is_some()
                    || v.rc_overridden.iter().any(|b| *b)
            })
    }

    pub(super) fn interrupt_route(&mut self, key: &Address) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.commands.retain(|p| {
            if p.key != *key {
                return true;
            }
            effects.push(Effect::CommandDone {
                receipt_id: p.order.receipt_id.clone(),
                outcome: CommandOutcome::Timeout,
                attempts: p.attempts,
            });
            false
        });
        let Some(v) = self.vehicles.get_mut(key) else {
            return effects;
        };
        let id = v.observation.id.clone();
        let lost = || {
            Failure::Rejected("The connection changed. The operation's outcome may be unknown. Read the vehicle state before retrying".into())
        };
        if v.params.take().is_some() {
            effects.push(Effect::Parameters {
                vehicle_id: id.clone(),
                result: Err(lost()),
                defaults: BTreeMap::new(),
            });
        }
        if let Some(download) = v.mission_download.take() {
            effects.push(part_effect(id.clone(), download.protocol.kind, Err(lost())));
        }
        if let Some(upload) = v.mission_upload.take() {
            effects.push(Effect::MissionUploaded {
                order_id: upload.protocol.order_id,
                result: Err(lost()),
            });
        }
        for event in v.ftp.interrupted(
            "The connection changed. The operation's outcome may be unknown",
            true,
        ) {
            match event {
                files::Event::Parameters(_) => effects.push(Effect::Parameters {
                    vehicle_id: id.clone(),
                    result: Err(lost()),
                    defaults: BTreeMap::new(),
                }),
                files::Event::ParamsWritten { writes, .. } => {
                    for (order_id, _) in writes {
                        effects.push(Effect::ParamWritten {
                            order_id,
                            result: Err(lost()),
                            attempts: 1,
                        });
                    }
                }
                event => effects.push(Effect::File {
                    vehicle_id: id.clone(),
                    event,
                }),
            }
        }
        // PARAM_SET writes have their own order IDs and must finish visibly.
        for write in v.writes.drain(..) {
            effects.push(Effect::ParamWritten {
                order_id: write.protocol.order_id,
                result: Err(lost()),
                attempts: 1,
            });
        }
        for (order_id, _) in v.batch.drain(..) {
            effects.push(Effect::ParamWritten {
                order_id,
                result: Err(lost()),
                attempts: 0,
            });
        }
        v.manual_sent = None;
        v.rc_overridden = [false; 18];
        v.pending = Produced::default();
        effects
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "routing test fixtures and assertions")]
    use super::super::{CommandOrder, Param, decode};
    use super::*;
    use crate::sim::FakeVehicle;

    fn link(id: &str, write: bool) -> Link {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"kind":{"type":"udp-client","host":"127.0.0.1","port":15010},"policy":if write {"read-write"} else {"read"},"topology":"auto","status":"up","createdAtUs":0,"version":1})).unwrap()
    }
    fn discover(session: &mut Session, fake: &mut FakeVehicle, link: &str, now: u64) -> String {
        let mut work = session.ingest(link, "peer", &fake.heartbeat(), now);
        while !work.is_empty() {
            let mut next = Vec::new();
            for effect in work {
                if let Effect::Send { bytes, .. } = effect {
                    for reply in fake.answer(&bytes) {
                        next.extend(session.ingest(link, "peer", &reply, now));
                    }
                }
            }
            work = next;
        }
        session
            .vehicles
            .values()
            .find(|v| v.observation.link_id == link && v.observation.uid.is_some())
            .unwrap()
            .observation
            .id
            .clone()
    }
    fn selected(session: &Session, id: &str) -> Option<String> {
        session.find(id).map(|key| key.0)
    }

    #[test]
    fn overlapping_connections_select_independently_and_keep_unavailable_routes() {
        let mut s = Session::default();
        s.set_connection_order(vec!["ethernet".into(), "radio".into(), "4g".into()]);
        for id in ["ethernet", "radio", "4g"] {
            s.configure_link(&link(id, true));
        }
        let mut buggy = FakeVehicle::new(1, 100);
        let mut scout = FakeVehicle::new(2, 200);
        let mut rover = FakeVehicle::new(3, 300);
        let buggy_id = discover(&mut s, &mut buggy, "ethernet", 1);
        discover(&mut s, &mut buggy, "radio", 1);
        discover(&mut s, &mut buggy, "4g", 1);
        discover(&mut s, &mut scout, "radio", 1);
        discover(&mut s, &mut rover, "4g", 1);
        // Find by actual UID, because radio already carries Buggy.
        let scout_id = s.vehicle_at("radio", "peer", 2).unwrap().to_owned();
        let rover_id = s.vehicle_at("4g", "peer", 3).unwrap().to_owned();
        assert_eq!(selected(&s, &buggy_id).as_deref(), Some("ethernet"));
        assert_eq!(selected(&s, &scout_id).as_deref(), Some("radio"));
        assert_eq!(selected(&s, &rover_id).as_deref(), Some("4g"));
        for (link, fake) in [
            ("radio", &mut buggy),
            ("4g", &mut rover),
            ("radio", &mut scout),
        ] {
            s.ingest(link, "peer", &fake.heartbeat(), 4_000_002);
        }
        s.tick(4_000_002);
        assert_eq!(selected(&s, &buggy_id).as_deref(), Some("radio"));
        assert_eq!(selected(&s, &scout_id).as_deref(), Some("radio"));
        let view = s.routing.published[&buggy_id].connections.as_ref().unwrap();
        assert_eq!(view.routes.len(), 3);
        assert!(!view.routes[0].reachable);
        s.transport_lost("radio", 4_000_003);
        assert_eq!(selected(&s, &scout_id), None, "no undiscovered backup");
        assert_eq!(selected(&s, &rover_id).as_deref(), Some("4g"));
        assert_eq!(
            s.routes().len(),
            1,
            "no provisional aliases remain as command routes"
        );
    }

    #[test]
    fn recovery_waits_ten_seconds_and_receive_only_paths_never_carry_commands() {
        let mut s = Session::default();
        s.set_connection_order(vec!["a".into(), "b".into(), "read".into()]);
        for id in ["a", "b"] {
            s.configure_link(&link(id, true));
        }
        s.configure_link(&link("read", false));
        let mut fake = FakeVehicle::new(1, 100);
        let id = discover(&mut s, &mut fake, "a", 1);
        discover(&mut s, &mut fake, "b", 1);
        discover(&mut s, &mut fake, "read", 1);
        s.transport_lost("a", 2);
        assert_eq!(selected(&s, &id).as_deref(), Some("b"));
        s.transport_restored("a");
        for second in 1..=12 {
            let now = second * 1_000_000;
            for l in ["b", "a", "read"] {
                s.ingest(l, "peer", &fake.heartbeat(), now);
            }
            if second < 11 {
                assert_eq!(selected(&s, &id).as_deref(), Some("b"));
            }
        }
        assert_eq!(selected(&s, &id).as_deref(), Some("a"));
        s.transport_lost("a", 12_000_001);
        s.transport_lost("b", 12_000_002);
        assert_eq!(selected(&s, &id), None);
        assert_eq!(s.routing.published[&id].presence, Presence::Online);
        assert!(
            !s.routing.published[&id]
                .connections
                .as_ref()
                .unwrap()
                .command_available
        );
        assert!(s.send_to(&id, |_, _| vec![1]).is_empty());
    }

    #[test]
    fn a_switch_ends_an_uncertain_command_without_replay_or_a_new_retry_budget() {
        let mut s = Session::default();
        s.set_connection_order(vec!["a".into(), "b".into()]);
        let mut fake = FakeVehicle::new(1, 100);
        let id = discover(&mut s, &mut fake, "a", 1);
        discover(&mut s, &mut fake, "b", 1);
        let sent = s.command(
            CommandOrder {
                receipt_id: "arm".into(),
                vehicle_id: id,
                command: uxduck_core::domain::command::Command::Arm,
                force: false,
            },
            2,
        );
        assert!(
            sent.iter()
                .any(|e| matches!(e, Effect::Send { link_id, .. } if link_id == "a"))
        );
        let ended = s.transport_lost("a", 3);
        assert!(ended.iter().any(|e| matches!(e, Effect::CommandDone { receipt_id, outcome: CommandOutcome::Timeout, .. } if receipt_id == "arm")));
        assert!(!s.tick(1_000_000).iter().any(|e| matches!(e, Effect::Send { bytes, .. } if matches!(decode(bytes).map(|d| d.message), Some(Message::CommandLong { command: 400, .. })))));
    }
    #[test]
    fn a_command_goes_only_on_the_selected_route() {
        let mut s = Session::default();
        s.set_connection_order(vec!["b".into(), "a".into()]);
        let mut fake = FakeVehicle::new(1, 100);
        let id = discover(&mut s, &mut fake, "b", 1);
        discover(&mut s, &mut fake, "a", 1);
        assert_eq!(selected(&s, &id).as_deref(), Some("b"));
        let sent = s.command(
            CommandOrder {
                receipt_id: "arm".into(),
                vehicle_id: id,
                command: uxduck_core::domain::command::Command::Arm,
                force: false,
            },
            2,
        );
        let links: Vec<_> = sent
            .iter()
            .filter_map(|e| match e {
                Effect::Send { link_id, .. } => Some(link_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(links, ["b"]);
    }
    #[test]
    fn a_vehicle_no_route_reaches_is_commanded_where_it_was_last_heard() {
        let mut s = Session::default();
        s.set_connection_order(vec!["a".into(), "b".into()]);
        let mut fake = FakeVehicle::new(1, 100);
        let id = discover(&mut s, &mut fake, "a", 1);
        discover(&mut s, &mut fake, "b", 2);
        s.tick(10_000_000);
        assert_eq!(selected(&s, &id), None);
        let sent = s.command(
            CommandOrder {
                receipt_id: "rtl".into(),
                vehicle_id: id,
                command: uxduck_core::domain::command::Command::Rtl,
                force: false,
            },
            10_000_000,
        );
        let links: Vec<_> = sent
            .iter()
            .filter_map(|e| match e {
                Effect::Send { link_id, .. } => Some(link_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(links, ["b"]);
    }

    #[test]
    fn a_failed_send_to_one_peer_moves_only_the_vehicle_behind_it() {
        let mut s = Session::default();
        s.set_connection_order(vec!["eth".into(), "radio".into()]);
        let mut ids = Vec::new();
        for (system, uid, peer) in [(1, 100, "buggy"), (2, 200, "scout")] {
            let mut fake = FakeVehicle::new(system, uid);
            for link in ["eth", "radio"] {
                let mut work = s.ingest(link, peer, &fake.heartbeat(), 1);
                while !work.is_empty() {
                    let mut next = Vec::new();
                    for effect in work {
                        if let Effect::Send { bytes, .. } = effect {
                            for reply in fake.answer(&bytes) {
                                next.extend(s.ingest(link, peer, &reply, 1));
                            }
                        }
                    }
                    work = next;
                }
            }
            ids.push(s.vehicle_at("eth", peer, system).unwrap().to_owned());
        }
        assert_eq!(selected(&s, &ids[0]).as_deref(), Some("eth"));
        assert_eq!(selected(&s, &ids[1]).as_deref(), Some("eth"));
        s.transport_peer_send_health("eth", ["buggy".to_owned()].into());
        s.tick(2);
        assert_eq!(selected(&s, &ids[0]).as_deref(), Some("radio"));
        assert_eq!(selected(&s, &ids[1]).as_deref(), Some("eth"));
    }

    #[test]
    fn same_system_number_is_not_identity_and_old_telemetry_cannot_cross_handover() {
        let mut s = Session::default();
        let mut first = FakeVehicle::new(1, 100);
        let mut other = FakeVehicle::new(1, 200);
        s.ingest("a", "peer", &first.heartbeat(), 1);
        s.ingest("b", "peer", &first.heartbeat(), 1);
        assert_ne!(s.vehicle_at("a", "peer", 1), s.vehicle_at("b", "peer", 1));
        let mut s = Session::default();
        let id = discover(&mut s, &mut first, "a", 2);
        let other_id = discover(&mut s, &mut other, "c", 2);
        assert_ne!(id, other_id);
        discover(&mut s, &mut first, "b", 2);
        let a = ("a".into(), "peer".into(), 1);
        let b = ("b".into(), "peer".into(), 1);
        let message = |boot| {
            decode(&crate::sim::attitude_frame((1, 1), (1.0, 2.0, 3.0), boot))
                .unwrap()
                .message
        };
        assert!(s.accept_telemetry(&a, &message(5000), 3));
        assert!(!s.accept_telemetry(&b, &message(5000), 3));
        s.transport_lost("a", 4);
        assert!(!s.accept_telemetry(&b, &message(4900), 5));
        assert!(!s.accept_telemetry(&b, &message(5100), 3));
        assert!(s.accept_telemetry(&b, &message(5100), 5));
    }

    #[test]
    fn a_restart_soon_after_boot_reaches_the_reboot_detector() {
        let mut s = Session::default();
        let mut fake = FakeVehicle::new(1, 100);
        discover(&mut s, &mut fake, "a", 1);
        let a = ("a".into(), "peer".into(), 1);
        let message = |boot| {
            decode(&crate::sim::attitude_frame((1, 1), (1.0, 2.0, 3.0), boot))
                .unwrap()
                .message
        };
        assert!(s.accept_telemetry(&a, &message(3000), 2));
        assert!(!s.accept_telemetry(&a, &message(2000), 3));
        assert!(s.accept_telemetry(&a, &message(100), 4));
    }

    #[test]
    fn route_changes_fail_transfers_visibly_and_explicit_probes_keep_their_route() {
        let mut s = Session::default();
        let mut fake = FakeVehicle::new(1, 100);
        let id = discover(&mut s, &mut fake, "a", 1);
        discover(&mut s, &mut fake, "b", 1);
        s.download_mission(&id, 2);
        s.set_parameter(
            "write",
            &id,
            Param {
                name: "TEST".into(),
                value: 1.0,
                kind: 9,
            },
            false,
            2,
        );
        s.file(&id, "read", files::Op::List { path: "/".into() }, 2);
        let effects = s.transport_lost("a", 3);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Mission { result: Err(_), .. }))
        );
        assert!(effects.iter().any(
            |e| matches!(e, Effect::ParamWritten {order_id,result:Err(_),..} if order_id=="write")
        ));
        assert!(effects.iter().any(|e|matches!(e, Effect::File {event:files::Event::Done {id,result:Err(_)},..} if id=="read")));
        let probes = s.send_address(&("a".into(), "peer".into(), 1), |_, _| vec![42]);
        assert!(
            probes
                .iter()
                .all(|e| matches!(e,Effect::Send {link_id,..} if link_id=="a"))
        );
        assert_eq!(selected(&s, &id).as_deref(), Some("b"));
        assert!(s.commands.is_empty());
        let old = &s.vehicles[&("a".into(), "peer".into(), 1)];
        assert!(old.mission_download.is_none() && old.writes.is_empty() && !old.ftp.busy());
    }
}
