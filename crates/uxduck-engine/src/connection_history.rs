//! Each vehicle connection over time (ADR-0004 2026-11-16), for the graph
//! under a connection on the vehicle's page: a sample a second from the
//! data plane of every connection a vehicle is heard on, with what its open
//! cameras received over it, its radio's noise and buffer and the MAVLink it
//! carried (2026-11-22). Kept in memory, never journaled.

use std::collections::{BTreeMap, VecDeque};

use uxduck_contracts::dto::vehicle::{
    ConnectionHistory, ConnectionSample, VehicleConnectionHistory,
};
use uxduck_core::domain::telemetry::LinkHealth;
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_mavlink::session::Radio;

use crate::Engine;
use crate::video::route_in_use;

/// How far back a connection's samples go. They are kept in memory, one a
/// second for each connection of each vehicle, so half an hour, a flight's
/// worth of a link, is what a long run holds rather than all of it.
pub const KEPT_US: u64 = 30 * 60 * 1_000_000;

/// How one of a vehicle's connections is doing now, from the data plane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionReading {
    pub vehicle_id: String,
    pub link_id: String,
    pub health: LinkHealth,
    /// Its telemetry radio, while it reports.
    pub radio: Radio,
    /// The MAVLink received and sent on the connection, in bits a second,
    /// once it has been measured for a second.
    pub throughput: Option<(u64, u64)>,
}

/// Each connection's samples, by vehicle and link id, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Histories {
    kept: BTreeMap<(String, String), VecDeque<ConnectionSample>>,
}

impl Engine {
    /// Every connection of every vehicle, from the data plane once a second
    /// (ADR-0004 2026-11-16). A replay's vehicle has none. Of two addresses
    /// on one connection, the one heard last is its second.
    pub fn connections_sampled(&mut self, readings: Vec<ConnectionReading>) {
        let now_us = self.clock.now_us();
        for reading in readings {
            let Some(vehicle) = self
                .state
                .vehicles
                .get(&reading.vehicle_id)
                .filter(|v| !v.replay)
            else {
                continue;
            };
            let sample = ConnectionSample {
                time_us: now_us,
                in_use: route_in_use(vehicle).is_some_and(|r| r.link_id == reading.link_id),
                down: !vehicle
                    .connections
                    .iter()
                    .flat_map(|c| &c.routes)
                    .any(|r| r.link_id == reading.link_id && r.reachable),
                round_trip_ms: reading.health.round_trip_ms,
                loss_percent: reading.health.loss_percent,
                age_ms: reading.health.age_ms,
                camera_bps: self.camera_bps_on(vehicle, &reading.link_id),
                rssi_percent: reading.radio.rssi_percent,
                remote_rssi_percent: reading.radio.remote_rssi_percent,
                noise_percent: reading.radio.noise_percent,
                remote_noise_percent: reading.radio.remote_noise_percent,
                radio_buffer_percent: reading.radio.buffer_free_percent,
                received_bps: reading.throughput.map(|(received, _)| received),
                sent_bps: reading.throughput.map(|(_, sent)| sent),
            };
            let kept = self
                .connection_history
                .kept
                .entry((reading.vehicle_id, reading.link_id))
                .or_default();
            match kept.back_mut() {
                Some(last) if last.time_us == now_us => {
                    if sample
                        .age_ms
                        .is_some_and(|age| last.age_ms.is_none_or(|was| age < was))
                    {
                        *last = sample;
                    }
                }
                _ => kept.push_back(sample),
            }
        }
        let since = now_us.saturating_sub(KEPT_US);
        self.connection_history.kept.retain(|_, kept| {
            while kept.front().is_some_and(|s| s.time_us < since) {
                kept.pop_front();
            }
            !kept.is_empty()
        });
    }

    /// `vehicle/connectionHistory`: each of the vehicle's connections that
    /// has samples, in the vehicle's order of them, its samples after
    /// `after_us`.
    pub(crate) fn connection_history(
        &self,
        vehicle: &Vehicle,
        after_us: Option<u64>,
    ) -> VehicleConnectionHistory {
        let routes = vehicle
            .connections
            .iter()
            .flat_map(|c| &c.routes)
            .map(|r| r.link_id.as_str());
        let gone = self
            .connection_history
            .kept
            .keys()
            .filter(|(id, _)| *id == vehicle.id)
            .map(|(_, link_id)| link_id.as_str());
        let mut links: Vec<&str> = Vec::new();
        for link_id in routes.chain(gone) {
            if !links.contains(&link_id) {
                links.push(link_id);
            }
        }
        let connections = links
            .into_iter()
            .filter_map(|link_id| {
                let kept = self
                    .connection_history
                    .kept
                    .get(&(vehicle.id.clone(), link_id.to_owned()))?;
                Some(ConnectionHistory {
                    link_id: link_id.to_owned(),
                    samples: kept
                        .iter()
                        .filter(|s| after_us.is_none_or(|after| s.time_us > after))
                        .cloned()
                        .collect(),
                })
            })
            .collect();
        VehicleConnectionHistory {
            vehicle_id: vehicle.id.clone(),
            now_us: self.clock.now_us(),
            kept_s: u32::try_from(KEPT_US / 1_000_000).unwrap_or(u32::MAX),
            connections,
        }
    }
}
