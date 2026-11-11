//! An operation (ADR-0039 R16): the thing above a mission, one event with
//! the flights that were part of it, so four vehicles launched at 1 am and
//! two more at 3 am are reopened as one. Made by itself where flights of
//! two or more vehicles overlap or follow within 30 minutes; a person can
//! make one, name it and add or remove flights. Nobody files a single
//! flight anywhere.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Flights of different vehicles this close are one operation.
pub const FOLLOW_US: u64 = 30 * 60 * 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: String,
    /// The person's name for it; none for one made by itself, shown as
    /// "24 Oct · 4 vehicles".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Made by itself and never edited: grouping may change it. Once a
    /// person edits it, it is theirs and grouping leaves it alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// Its flights, by id, oldest first.
    pub flight_ids: Vec<String>,
    /// From its first flight's start to its last flight's end.
    pub from_us: u64,
    pub to_us: u64,
    /// Videos of the whole event (R8), by recording id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_ids: Vec<String>,
    /// How far a person moved each flight's clock to match the others in
    /// its replay (R17), by flight id, in microseconds: kept, so the
    /// operation opens matched next time and on other computers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub clock_offsets_us: BTreeMap<String, i64>,
    /// One of its flights is kept in the account (C9), so it goes there too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// Made by `plan/simulate` (ADR-0011 M18): its runs and how each went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<Simulation>,
    pub version: u64,
}

/// Plans flown by simulated vehicles at once (ADR-0011 M18), one run each.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Simulation {
    pub runs: Vec<SimulationRun>,
    /// The runs UXDuck recommended for this computer when it started.
    pub recommended: u32,
    /// The longest a run may fly, in the simulator's minutes.
    pub limit_min: u32,
}

impl Simulation {
    #[must_use]
    pub fn over(&self) -> bool {
        self.runs.iter().all(|r| r.state.over())
    }
}

/// One simulated vehicle flying one plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationRun {
    /// The simulation's link, kept stopped once the run is over.
    pub link_id: String,
    pub vehicle_name: String,
    pub plan_id: String,
    pub plan_name: String,
    pub speedup: u8,
    /// Wind in whole metres a second, and the way it blows from, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_mps: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_from_deg: Option<u16>,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preparing: bool,
    /// Why it ended as it did, in a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// It went outside the plan's fence at some point.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub breached: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    /// Its simulator starting, until the vehicle is ready.
    Starting,
    /// The plan going up, with its fence and wind.
    Uploading,
    /// Armed in its mission.
    Flying,
    /// Flew its mission to the end.
    Finished,
    /// Disarmed, or stopped flying it, before its mission's end.
    EndedEarly,
    /// Still flying at the time limit.
    TimedOut,
    /// It never got going: the reason is its detail.
    Failed,
    /// Its simulation was stopped, or UXDuck stopped, before it ended.
    Stopped,
}

impl RunState {
    #[must_use]
    pub fn over(self) -> bool {
        !matches!(self, Self::Starting | Self::Uploading | Self::Flying)
    }
}

/// One flight as grouping sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span<'a> {
    pub id: &'a str,
    pub vehicle_id: &'a str,
    pub from_us: u64,
    pub to_us: u64,
}

/// Chains of flights that overlap or follow within [`FOLLOW_US`] of each
/// other, oldest first; only chains of two or more vehicles are kept.
#[must_use]
pub fn chains<'a>(mut flights: Vec<Span<'a>>) -> Vec<Vec<Span<'a>>> {
    flights.sort_by_key(|f| (f.from_us, f.id));
    let mut chains: Vec<Vec<Span<'a>>> = Vec::new();
    let mut reach = 0u64;
    for flight in flights {
        match chains.last_mut() {
            Some(chain) if flight.from_us <= reach.saturating_add(FOLLOW_US) => {
                reach = reach.max(flight.to_us);
                chain.push(flight);
            }
            _ => {
                reach = flight.to_us;
                chains.push(vec![flight]);
            }
        }
    }
    chains.retain(|chain| {
        let first = chain[0].vehicle_id;
        chain.iter().any(|f| f.vehicle_id != first)
    });
    chains
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1_000_000;

    fn span<'a>(id: &'a str, vehicle_id: &'a str, from: u64, to: u64) -> Span<'a> {
        Span {
            id,
            vehicle_id,
            from_us: from * MIN,
            to_us: to * MIN,
        }
    }

    fn ids<'a>(chains: &[Vec<Span<'a>>]) -> Vec<Vec<&'a str>> {
        chains
            .iter()
            .map(|c| c.iter().map(|f| f.id).collect())
            .collect()
    }

    #[test]
    fn overlapping_and_following_flights_of_two_vehicles_are_one() {
        // Two at once; a third 20 minutes after the first two end.
        let flights = vec![
            span("a", "alpha", 0, 10),
            span("b", "bravo", 5, 12),
            span("c", "alpha", 32, 40),
        ];
        assert_eq!(ids(&chains(flights)), [["a", "b", "c"]]);
    }

    #[test]
    fn a_chain_reaches_on_through_each_flight() {
        // Each within 30 minutes of the one before: one chain over two hours.
        let flights = vec![
            span("a", "alpha", 0, 10),
            span("b", "bravo", 35, 45),
            span("c", "alpha", 70, 80),
            span("d", "bravo", 105, 115),
        ];
        assert_eq!(ids(&chains(flights)), [["a", "b", "c", "d"]]);
    }

    #[test]
    fn two_hours_apart_are_two_and_one_vehicle_is_never_one() {
        let flights = vec![
            span("a", "alpha", 0, 10),
            span("b", "bravo", 5, 12),
            // Two hours later: alone.
            span("c", "alpha", 130, 140),
            // One vehicle's flights back to back: never an operation.
            span("d", "charlie", 300, 310),
            span("e", "charlie", 315, 320),
        ];
        assert_eq!(ids(&chains(flights)), [["a", "b"]]);
    }
}
