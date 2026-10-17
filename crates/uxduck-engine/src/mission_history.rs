//! A vehicle's missions over time (ADR-0029 P7, S134): every different
//! mission a vehicle has held, kept once by its content, with how it came
//! (found, uploaded from a plan, a simulation's example, or changed by
//! something else) and who uploaded it. Recorded whenever UXDuck knows the
//! vehicle's whole mission: after a download, a plan's read-back, or an
//! upload with no read-back. Each change is also in the vehicle's history.

use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::mission::{MissionItem, MissionOrigin, MissionRecord};
use uxduck_core::events::{Actor, Change};

use crate::Engine;

/// The same item to within what the protocol carries: positions to 1e-7°,
/// heights to a centimetre, parameters to four decimals; `_INT` frames are
/// their plain twins.
fn canonical(items: &[MissionItem], out: &mut String) {
    use std::fmt::Write;
    for item in items {
        let frame = match item.frame {
            5 => 0,
            6 => 3,
            11 => 10,
            other => other,
        };
        let q = |x: f64, by: f64| {
            if x.is_finite() {
                (x * by).round()
            } else {
                f64::NAN
            }
        };
        let _ = write!(
            out,
            "{} {frame} {} {} {} {} {} {} {};",
            item.command,
            q(item.param1, 1e4),
            q(item.param2, 1e4),
            q(item.param3, 1e4),
            q(item.param4, 1e4),
            q(item.latitude_deg, 1e7),
            q(item.longitude_deg, 1e7),
            q(item.altitude_m, 1e2),
        );
    }
}

/// FNV-1a: stable across runs and builds, unlike the standard hasher.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A record's id: the vehicle and its content.
fn record_id(
    vehicle_id: &str,
    items: &[MissionItem],
    fence: &[MissionItem],
    rally: &[MissionItem],
) -> String {
    let mut text = String::new();
    canonical(items, &mut text);
    text.push('|');
    canonical(fence, &mut text);
    text.push('|');
    canonical(rally, &mut text);
    format!("{vehicle_id}:{:016x}", fnv(&text))
}

/// A mission put back from its history names no plan: it keeps the plan
/// it first came from.
fn carried(origin: MissionOrigin, before: Option<&MissionOrigin>) -> MissionOrigin {
    match (origin, before) {
        (
            MissionOrigin::Uploaded {
                plan_id: None,
                plan_name: None,
                verified,
            },
            Some(MissionOrigin::Uploaded {
                plan_id, plan_name, ..
            }),
        ) => MissionOrigin::Uploaded {
            plan_id: plan_id.clone(),
            plan_name: plan_name.clone(),
            verified,
        },
        (origin, _) => origin,
    }
}

impl Engine {
    /// The next mission recorded for a vehicle came this way: an upload
    /// UXDuck just made, recorded once its mission is known.
    pub(crate) fn mission_arriving(
        &mut self,
        vehicle_id: &str,
        origin: MissionOrigin,
        actor: Actor,
    ) {
        self.transfers
            .arriving
            .insert(vehicle_id.to_owned(), (origin, actor));
    }

    /// A vehicle's records, newest first.
    #[must_use]
    pub fn mission_history(&self, vehicle_id: &str) -> Vec<&MissionRecord> {
        let mut records: Vec<&MissionRecord> = self
            .state
            .mission_records
            .values()
            .filter(|r| r.vehicle_id == vehicle_id)
            .collect();
        records.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.last_seen_us.cmp(&a.last_seen_us)));
        records
    }

    /// Records the vehicle's mission as UXDuck now knows it, when it is not
    /// the one recorded last: new, or held before and back again.
    pub(crate) fn record_mission(&mut self, vehicle_id: &str) {
        let arriving = self.transfers.arriving.remove(vehicle_id);
        let Some(mission) = self.state.missions.get(vehicle_id) else {
            return;
        };
        let ardupilot = self
            .vehicle(vehicle_id)
            .is_none_or(|v| v.autopilot.to_lowercase().contains("ardupilot"));
        // ArduPilot's item 0 is home, which moves with the vehicle.
        let items: Vec<MissionItem> = mission
            .items
            .iter()
            .skip(usize::from(ardupilot))
            .cloned()
            .collect();
        let (fence, rally) = (mission.fence.clone(), mission.rally.clone());
        let history = self.mission_history(vehicle_id);
        let empty = items.is_empty() && fence.is_empty() && rally.is_empty();
        // Nothing on it, and nothing before: nothing to keep.
        if empty && history.is_empty() {
            return;
        }
        let id = record_id(vehicle_id, &items, &fence, &rally);
        let latest = history.first().map(|r| r.id.clone());
        let known = self.state.mission_records.get(&id).cloned();
        let now = self.clock.now_us();
        let seq = self.state.seq + 1;
        let (origin, actor) = match arriving {
            Some((origin, actor)) => (origin, Some(actor)),
            None if history.is_empty() => (MissionOrigin::Found, None),
            None if latest.as_deref() == Some(id.as_str()) => return,
            None => (MissionOrigin::ChangedElsewhere, None),
        };
        let uploaded = matches!(origin, MissionOrigin::Uploaded { .. });
        let origin = carried(origin, known.as_ref().map(|r| &r.origin));
        let record = match known {
            // The same mission uploaded again while it is the one held: who
            // and how, not a new holding.
            Some(mut record) if latest.as_deref() == Some(id.as_str()) => {
                if !uploaded && !matches!(origin, MissionOrigin::Example) {
                    return;
                }
                record.origin = origin;
                record.client_id = actor.as_ref().map(|a| a.client_id.clone());
                record.operator_id = actor.and_then(|a| a.operator_id);
                record.last_seen_us = now;
                record.seq = seq;
                record
            }
            Some(mut record) => {
                record.origin = origin;
                record.client_id = actor.as_ref().map(|a| a.client_id.clone());
                record.operator_id = actor.and_then(|a| a.operator_id);
                record.last_seen_us = now;
                record.seq = seq;
                record.times += 1;
                record
            }
            None => MissionRecord {
                id: id.clone(),
                vehicle_id: vehicle_id.to_owned(),
                items,
                fence,
                rally,
                origin,
                client_id: actor.as_ref().map(|a| a.client_id.clone()),
                operator_id: actor.and_then(|a| a.operator_id),
                first_seen_us: now,
                last_seen_us: now,
                times: 1,
                seq,
            },
        };
        // "Mission uploaded from Ridge survey: 38 items, a fence".
        let how = record
            .origin_words()
            .replacen("Its example", "the example", 1);
        let mut first = how.chars();
        let how = first
            .next()
            .map(|c| c.to_lowercase().chain(first).collect::<String>())
            .unwrap_or_default();
        let text = format!("Mission {how}: {}", record.counts());
        self.commit_as(
            crate::history::actor(),
            Change::MissionRecorded {
                record: record.clone(),
            },
        );
        self.record(
            vehicle_id,
            HistoryEvent::MissionChanged {
                record_id: record.id,
                text,
            },
        );
    }

    /// The vehicle's latest record, an upload, read back the same or not.
    pub(crate) fn mission_record_verified(&mut self, vehicle_id: &str, same: bool) {
        let Some(mut record) = self
            .mission_history(vehicle_id)
            .first()
            .map(|r| (*r).clone())
        else {
            return;
        };
        let MissionOrigin::Uploaded { verified, .. } = &mut record.origin else {
            return;
        };
        *verified = Some(same);
        self.commit_as(crate::history::actor(), Change::MissionRecorded { record });
    }
}
