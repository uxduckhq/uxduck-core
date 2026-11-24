//! Autopilot logs from anywhere (S457, ADR-0039 "Autopilot logs from
//! anywhere"): `logs/inspect` reads `.bin` and `.ulg` logs without copying
//! them, and `logs/import` copies them into this computer's logs, where
//! S426 ties every copy to its flights whatever way it arrived. A log can be
//! hundreds of megabytes, so the host reads and copies it off the engine's
//! task, as it does a replay's log (`System::replay_work`), and the engine
//! matches it to the Logbook's flights by the rule `logs/cards` uses.

use serde_json::Value;
use uxduck_contracts::dto::replay::{
    CopiedLog, InspectedLog, LogFilesParams, LogsImportResult, LogsInspectResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;

use crate::replay::Worked as ReplayWorkedKind;
use crate::rpc::handlers::to_value;
use crate::runtime::{LogSource, ReplayWork, ReplayWorked};
use crate::{ConnectionId, Engine};

/// What the host's work on logs made, for whoever asked.
pub(crate) struct Worked {
    reply: Option<(ConnectionId, Value)>,
    done: Done,
    /// The uploads it imported, let go of once it has.
    uploads: Vec<String>,
}

enum Done {
    Inspected(Vec<InspectedLog>),
    Imported(LogsImportResult),
}

/// The files asked for, paths first, then bytes, then uploads: each one
/// the asking connection uploaded whole. A name with a folder in it keeps
/// only its file name, so bytes are never written outside the logs.
fn sources(
    engine: &Engine,
    connection: Option<ConnectionId>,
    params: LogFilesParams,
) -> Result<Vec<LogSource>, ApiError> {
    let mut sources: Vec<LogSource> = params
        .paths
        .into_iter()
        .map(LogSource::Path)
        .chain(params.files.into_iter().map(|file| LogSource::Bytes {
            name: file.name,
            data: file.data,
        }))
        .collect();
    for id in &params.attachments {
        let (found, path) = match connection {
            Some(connection) => engine.attached_path(connection, id)?,
            None => return Err(ApiError::new(ErrorKind::InvalidParams, "No upload here.")),
        };
        sources.push(LogSource::Uploaded {
            name: found.name,
            path,
        });
    }
    if sources.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "Give the logs by `paths`, `files` or `attachments`.",
        ));
    }
    Ok(sources)
}

pub(crate) fn handle(
    engine: &mut Engine,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let (needed, params, import) = match request {
        ClientRequest::LogsInspect(params) => (Capability::Read, params, false),
        ClientRequest::LogsImport(params) => (Capability::Config, params, true),
        _ => return Err(ApiError::new(ErrorKind::Internal, "not a logs method")),
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            needed.denied_message(),
        ));
    }
    let connection = engine
        .replying_to
        .as_ref()
        .map(|(connection, _)| *connection);
    let uploads = if import {
        params.attachments.clone()
    } else {
        Vec::new()
    };
    let sources = sources(engine, connection, params)?;
    let Some(logs) = engine.system.autopilot_logs() else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Autopilot logs are imported by UXDuck on a computer, not in a browser.",
        ));
    };
    let reply = engine.replying_to.take();
    let work = ReplayWork::new(move || {
        let done = if import {
            Done::Imported(logs.import(&sources))
        } else {
            Done::Inspected(sources.iter().map(|source| logs.inspect(source)).collect())
        };
        ReplayWorked(ReplayWorkedKind::Logs(Worked {
            reply,
            done,
            uploads,
        }))
    });
    if let Some(worked) = engine.system.replay_work(work) {
        engine.replay_worked(worked);
    }
    Ok(Value::Null)
}

impl Engine {
    /// The host read or imported logs: each matched to the Logbook's
    /// flights it covers, and whoever asked answered.
    pub(crate) fn logs_worked(&mut self, worked: Worked) {
        let Worked {
            reply,
            done,
            uploads,
        } = worked;
        for id in &uploads {
            self.forget_upload(id);
        }
        let result = match done {
            Done::Inspected(mut logs) => {
                for log in &mut logs {
                    self.place_log(log);
                }
                to_value(&LogsInspectResult { logs })
            }
            Done::Imported(mut result) => {
                // A log that covers no flight is an entry of its own (S459).
                self.add_log_entries();
                for imported in result.imported.iter_mut().chain(&mut result.skipped) {
                    self.place_log(&mut imported.log);
                }
                to_value(&result)
            }
        };
        let Some((connection, id)) = reply else {
            return;
        };
        let frame = match result {
            Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(why) => crate::rpc::router::error(&id, &why),
        };
        self.send(connection, &frame);
    }

    /// The flights a log covers, by the rule a copied log is matched with
    /// (S426), and the vehicle its system id names here.
    fn place_log(&self, log: &mut InspectedLog) {
        let mut copied = [CopiedLog {
            path: log.path.clone().unwrap_or_default(),
            name: log.name.clone(),
            bytes: log.bytes,
            span: log.span.clone(),
            ..CopiedLog::default()
        }];
        self.match_copied_logs(&mut copied);
        let [copied] = copied;
        log.flights = copied.flights;
        let Some(span) = log.span.as_ref() else {
            return;
        };
        log.vehicle = log
            .flights
            .first()
            .and_then(|flight| self.state.flights.get(&flight.id))
            .map(|flight| flight.vehicle_name.clone())
            .or_else(|| {
                let uid = span.board.as_ref()?.uid.as_str();
                self.state
                    .vehicles
                    .values()
                    .find(|v| !v.replay && v.uid.as_deref() == Some(uid))
                    .map(|v| v.name.clone())
            })
            .or_else(|| {
                let mut named: Vec<&str> = self
                    .state
                    .vehicles
                    .values()
                    .filter(|v| !v.replay && v.system_id == span.system_id)
                    .map(|v| v.name.as_str())
                    .collect();
                named.sort_unstable();
                named.dedup();
                match named.as_slice() {
                    [only] => Some((*only).to_owned()),
                    _ => None,
                }
            });
    }
}
