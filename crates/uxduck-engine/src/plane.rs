//! Between the MAVLink session and the engine (ADR-0004): how the engine's
//! orders start in the session, and what each of the session's effects
//! means for the engine. Both composition roots drive it: the daemon from
//! its link tasks, the browser worker from the simulator's serial buffer
//! (S20), so it lives here rather than in either.

use uxduck_core::domain::mission::MissionItem;
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_mavlink::decode::MissionItemInt;
use uxduck_mavlink::session::{self, Effect, Session};
use uxduck_mavlink::transfers::{Failure as Lost, Param};

use crate::Engine;
use crate::commands::Outcome;
use crate::runtime::{CommandOrder, FileOp, TransferOrder};
use crate::transfers::{Failure, ProgressKind, Reported};
use crate::vehicle_files::FileReport;
use uxduck_mavlink::files;

/// A change for the engine, run where the engine is.
pub type Report = Box<dyn FnOnce(&mut Engine) + Send>;

/// Where one of the session's effects goes.
pub enum Routed {
    /// To the engine.
    Engine(Report),
    /// Out on a link, to a peer.
    Send {
        link_id: String,
        peer: String,
        bytes: Vec<u8>,
    },
}

impl std::fmt::Debug for Routed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(_) => f.write_str("Engine"),
            Self::Send { link_id, peer, .. } => {
                write!(f, "Send {{ link_id: {link_id}, peer: {peer} }}")
            }
        }
    }
}

/// Where an effect goes: out on a link, or to the engine.
#[must_use]
pub fn route(effect: Effect) -> Routed {
    match effect {
        Effect::Send {
            link_id,
            peer,
            bytes,
        } => Routed::Send {
            link_id,
            peer,
            bytes,
        },
        Effect::Vehicle(seen) => {
            let seen = observation(*seen);
            Routed::Engine(Box::new(move |e| e.vehicle_observed(seen)))
        }
        Effect::Telemetry(update) => {
            Routed::Engine(Box::new(move |e| e.telemetry_observed(update)))
        }
        Effect::Traffic { vehicle_id, report } => {
            Routed::Engine(Box::new(move |e| e.vehicle_traffic(&vehicle_id, report)))
        }
        Effect::CommandDone {
            receipt_id,
            outcome,
            attempts,
        } => {
            let outcome = match outcome {
                session::CommandOutcome::Accepted => Outcome::Accepted,
                session::CommandOutcome::Rejected(why) => Outcome::Rejected(why),
                session::CommandOutcome::Timeout => Outcome::Timeout,
                session::CommandOutcome::NotSent(why) => Outcome::NotSent(why),
            };
            Routed::Engine(Box::new(move |e| {
                e.command_result(&receipt_id, outcome, attempts);
            }))
        }
        other => Routed::Engine(transfer_report(other).unwrap_or_else(|| Box::new(|_| {}))),
    }
}

/// Starts a command in the session.
pub fn start_command(session: &mut Session, order: CommandOrder, now_us: u64) -> Vec<Effect> {
    session.command(
        session::CommandOrder {
            receipt_id: order.receipt_id,
            vehicle_id: order.vehicle_id,
            command: order.command,
            force: order.force,
        },
        now_us,
    )
}

/// Starts a transfer in the session.
pub fn start_transfer(session: &mut Session, order: TransferOrder, now_us: u64) -> Vec<Effect> {
    match order {
        TransferOrder::DownloadParameters { vehicle_id } => {
            session.download_parameters(&vehicle_id, now_us)
        }
        TransferOrder::SetParameter {
            order_id,
            vehicle_id,
            name,
            value,
            kind,
            by_file,
        } => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the protocol carries an f32"
            )]
            let param = Param {
                name,
                value: value as f32,
                kind: kind.to_mav(),
            };
            session.set_parameter(&order_id, &vehicle_id, param, by_file, now_us)
        }
        TransferOrder::DownloadMission { vehicle_id } => {
            session.download_mission(&vehicle_id, now_us)
        }
        TransferOrder::DownloadMissionPart { vehicle_id, kind } => {
            session.download_mission_part(&vehicle_id, kind, now_us)
        }
        TransferOrder::UploadMissionPart {
            order_id,
            vehicle_id,
            items,
            kind,
        } => {
            let items = items
                .iter()
                .enumerate()
                .map(|(seq, item)| {
                    let mut item = to_mavlink((seq, item));
                    item.mission_type = kind;
                    item
                })
                .collect();
            session.upload_mission_part(&order_id, &vehicle_id, items, kind, now_us)
        }
        TransferOrder::UploadMission {
            order_id,
            vehicle_id,
            items,
        } => {
            let items = items.iter().enumerate().map(to_mavlink).collect();
            session.upload_mission(&order_id, &vehicle_id, items, now_us)
        }
        TransferOrder::File { vehicle_id, id, op } => {
            let op = match op {
                FileOp::List { path } => files::Op::List { path },
                FileOp::Read { path, from } => files::Op::Read { path, from },
                FileOp::Write { path, bytes } => files::Op::Write { path, bytes },
                FileOp::Remove { path } => files::Op::Remove { path },
                FileOp::RemoveDir { path } => files::Op::RemoveDir { path },
                FileOp::MakeDir { path } => files::Op::MakeDir { path },
                FileOp::Rename { from, to } => files::Op::Rename { from, to },
            };
            session.file(&vehicle_id, &id, op, now_us)
        }
        TransferOrder::CancelFile { vehicle_id, id } => {
            session.cancel_file(&vehicle_id, &id, now_us)
        }
        TransferOrder::CancelTransfer { vehicle_id, kind } => session.cancel_transfer(
            &vehicle_id,
            match kind {
                ProgressKind::Parameters => session::TransferKind::Parameters,
                ProgressKind::MissionDownload => session::TransferKind::MissionDownload,
                ProgressKind::MissionUpload => session::TransferKind::MissionUpload,
            },
        ),
        TransferOrder::TerrainData {
            vehicle_id,
            block,
            groups,
            mask,
            per_second,
        } => session.terrain_data(&vehicle_id, block, groups, mask, per_second, now_us),
        TransferOrder::TerrainCheck {
            vehicle_id,
            lat,
            lon,
        } => session.terrain_check(&vehicle_id, lat, lon, now_us),
    }
}

/// A file job's event, as the engine takes it (S121).
fn file_report(event: files::Event) -> Option<FileReport> {
    Some(match event {
        files::Event::Listed { id, entries } => FileReport::Listed {
            id,
            entries: entries.into_iter().map(|e| (e.name, e.size)).collect(),
        },
        files::Event::Opened { id, size } => FileReport::Opened { id, size },
        files::Event::Data { id, offset, bytes } => FileReport::Data { id, offset, bytes },
        files::Event::Sent { id, done, total } => FileReport::Sent { id, done, total },
        files::Event::Done { id, result } => FileReport::Done {
            id,
            result: result.map_err(failure),
        },
        files::Event::Crc { .. }
        | files::Event::Parameters(_)
        | files::Event::ParamsWritten { .. } => return None,
    })
}

/// What to tell the engine about a transfer effect, if it is one.
#[allow(clippy::too_many_lines)]
fn transfer_report(effect: Effect) -> Option<Report> {
    Some(match effect {
        Effect::Booted {
            vehicle_id,
            boot_instant_us,
            restarted,
        } => Box::new(move |e| e.vehicle_booted(&vehicle_id, boot_instant_us, restarted)),
        Effect::Parameters {
            vehicle_id,
            result,
            defaults,
        } => {
            let result = result
                .map(|all| all.into_iter().map(reported).collect())
                .map_err(failure);
            let defaults = defaults
                .into_iter()
                .map(|(name, value)| (name, f64::from(value)))
                .collect();
            Box::new(move |e| e.parameters_downloaded(&vehicle_id, result, &defaults))
        }
        Effect::ParamValue {
            vehicle_id,
            param,
            station,
        } => {
            let param = reported(param);
            Box::new(move |e| e.parameter_reported(&vehicle_id, param, station))
        }
        Effect::ParamWritten {
            order_id,
            result,
            attempts,
        } => {
            let result = result.map(reported).map_err(failure);
            Box::new(move |e| e.parameter_written(&order_id, result, attempts))
        }
        Effect::Mission { vehicle_id, result } => {
            let result = result
                .map(|items| items.iter().map(from_mavlink).collect())
                .map_err(failure);
            Box::new(move |e| e.mission_downloaded(&vehicle_id, result))
        }
        Effect::MissionPart {
            vehicle_id,
            kind,
            result,
        } => {
            let result = result
                .map(|items| items.iter().map(from_mavlink).collect())
                .map_err(failure);
            Box::new(move |e| e.mission_part_downloaded(&vehicle_id, kind, result))
        }
        Effect::MissionUploaded { order_id, result } => {
            let result = result.map_err(failure);
            Box::new(move |e| e.mission_uploaded(&order_id, result))
        }
        Effect::Progress {
            vehicle_id,
            transfer,
            done,
            total,
            attempt,
        } => {
            let kind = match transfer {
                session::TransferKind::Parameters => ProgressKind::Parameters,
                session::TransferKind::MissionDownload => ProgressKind::MissionDownload,
                session::TransferKind::MissionUpload => ProgressKind::MissionUpload,
            };
            Box::new(move |e| e.transfer_progress(&vehicle_id, kind, done, total, attempt))
        }
        Effect::StatusText {
            vehicle_id,
            component_id,
            severity,
            text,
        } => Box::new(move |e| e.vehicle_message(&vehicle_id, component_id, severity, &text)),
        Effect::VideoStream {
            vehicle_id,
            component_id,
            stream_id,
            uri,
            ..
        } => Box::new(move |e| {
            e.video_stream_announced(&vehicle_id, component_id, stream_id, &uri);
        }),
        Effect::File { event, .. } => {
            let report = file_report(event)?;
            Box::new(move |e| e.file_reported(report))
        }
        Effect::Modes { vehicle_id, modes } => {
            Box::new(move |e| e.vehicle_modes_listed(&vehicle_id, modes))
        }
        Effect::TerrainRequest {
            vehicle_id,
            block,
            mask,
        } => Box::new(move |e| e.terrain_requested(&vehicle_id, block, mask)),
        Effect::TerrainChecked {
            vehicle_id,
            lat,
            lon,
            height,
        } => Box::new(move |e| e.terrain_checked(&vehicle_id, lat, lon, height)),
        Effect::Vehicle(_)
        | Effect::Telemetry(_)
        | Effect::Traffic { .. }
        | Effect::CommandDone { .. }
        | Effect::Send { .. } => {
            return None;
        }
    })
}

/// The session's view of a vehicle, as the engine records it.
#[must_use]
pub fn observation(seen: session::Observation) -> Observation {
    Observation {
        id: seen.id,
        aliases: seen.aliases,
        link_id: seen.link_id,
        peer: seen.peer,
        system_id: seen.system_id,
        component_ids: seen.component_ids,
        autopilot: seen.autopilot,
        vehicle_type: seen.vehicle_type,
        uid: seen.uid,
        firmware: seen.firmware,
        firmware_commit: seen.firmware_commit,
        presence: match seen.presence {
            session::Presence::Online => Presence::Online,
            session::Presence::Stale => Presence::Stale,
            session::Presence::Offline => Presence::Offline,
        },
        last_heartbeat_us: seen.last_heartbeat_us,
        armed: seen.armed,
        mode: seen.mode,
    }
}

fn reported(param: Param) -> Reported {
    Reported {
        name: param.name,
        value: f64::from(param.value),
        kind: ParamKind::from_mav(param.kind),
    }
}

fn failure(lost: Lost) -> Failure {
    match lost {
        Lost::NotSent(why) => Failure::NotSent(why),
        Lost::Rejected(why) => Failure::Rejected(why),
        Lost::Timeout => Failure::Timeout,
    }
}

/// Degrees to the protocol's 1e7 fixed point.
#[allow(clippy::cast_possible_truncation, reason = "clamped to i32 first")]
fn e7(degrees: f64) -> i32 {
    (degrees * 1e7)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[allow(clippy::cast_possible_truncation, reason = "the protocol carries f32")]
fn to_mavlink((seq, item): (usize, &MissionItem)) -> MissionItemInt {
    MissionItemInt {
        seq: u16::try_from(seq).unwrap_or(u16::MAX),
        frame: item.frame,
        command: item.command,
        current: false,
        autocontinue: item.autocontinue,
        params: [
            item.param1 as f32,
            item.param2 as f32,
            item.param3 as f32,
            item.param4 as f32,
        ],
        x: e7(item.latitude_deg),
        y: e7(item.longitude_deg),
        z: item.altitude_m as f32,
        mission_type: 0,
    }
}

fn from_mavlink(item: &MissionItemInt) -> MissionItem {
    MissionItem {
        command: item.command,
        frame: item.frame,
        param1: f64::from(item.params[0]),
        param2: f64::from(item.params[1]),
        param3: f64::from(item.params[2]),
        param4: f64::from(item.params[3]),
        latitude_deg: f64::from(item.x) / 1e7,
        longitude_deg: f64::from(item.y) / 1e7,
        altitude_m: f64::from(item.z),
        autocontinue: item.autocontinue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_items_survive_the_trip_through_the_protocol() {
        let item = MissionItem {
            command: 16,
            frame: 3,
            param1: 5.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            latitude_deg: -35.363_261_1,
            longitude_deg: 149.165_230_2,
            altitude_m: 30.5,
            autocontinue: true,
        };
        let there = to_mavlink((2, &item));
        assert_eq!((there.seq, there.x), (2, -353_632_611));
        assert_eq!(from_mavlink(&there), item);
    }
}
