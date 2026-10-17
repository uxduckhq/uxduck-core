//! Remote vehicles in the app (S86, ADR-0003 C8): a window asks
//! `remote/watch` for another of the account's computers, and this UXDuck
//! watches it through the relay as its own signed-in account, sending what
//! it sees as `remote/changed` to the windows that asked. Read only, and
//! never journaled: it is the other computer's record, not this one's.
//!
//! The daemon holds the relay connections (`RemoteViewPort`); the engine
//! keeps who watches what, so the relay is asked only for what some window
//! shows, and every connection ends when the last window looking closes.
//!
//! A window may also play one of that computer's cameras (S84): the
//! daemon's viewer asks the relay for it and fetches a read grant, which
//! comes back in the computer's `RemoteView`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::edge::{RemoteView, RemoteWatchParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// What windows watch: each computer, with the vehicles open in detail.
pub type Wanted = BTreeMap<String, BTreeSet<String>>;

/// The daemon's viewers on the relay.
pub trait RemoteViewPort: Send + std::fmt::Debug {
    /// Watches these until called again; empty stops every one.
    fn want(&mut self, wanted: Wanted);

    /// Plays these cameras, by computer, until called again (S84).
    fn want_video(&mut self, _cameras: Wanted) {}
}

#[derive(Debug, Default)]
pub(crate) struct RemoteViewing {
    port: Option<Box<dyn RemoteViewPort>>,
    /// Each window's computer, and its vehicle in detail.
    watchers: BTreeMap<ConnectionId, (String, Option<String>)>,
    /// Each window's camera playing from away, with its computer (S84).
    cameras: BTreeMap<ConnectionId, (String, String)>,
    /// The latest of each computer watched.
    views: BTreeMap<String, RemoteView>,
}

impl Engine {
    pub fn set_remote_view(&mut self, port: Box<dyn RemoteViewPort>) {
        self.remote_viewing.port = Some(port);
    }

    fn rewant(&mut self) {
        let mut wanted = Wanted::new();
        for (computer, vehicle) in self.remote_viewing.watchers.values() {
            let details = wanted.entry(computer.clone()).or_default();
            details.extend(vehicle.clone());
        }
        self.remote_viewing
            .views
            .retain(|computer, _| wanted.contains_key(computer));
        let mut cameras = Wanted::new();
        for (computer, camera) in self.remote_viewing.cameras.values() {
            cameras
                .entry(computer.clone())
                .or_default()
                .insert(camera.clone());
        }
        if let Some(port) = self.remote_viewing.port.as_mut() {
            port.want(wanted);
            port.want_video(cameras);
        }
    }

    pub(crate) fn drop_remote_watch(&mut self, connection: ConnectionId) {
        let cameras = self.remote_viewing.cameras.remove(&connection).is_some();
        if self.remote_viewing.watchers.remove(&connection).is_some() || cameras {
            self.rewant();
        }
    }

    /// What the daemon's viewer of a computer sees now.
    pub fn remote_viewed(&mut self, view: RemoteView) {
        let computer = view.computer_id.clone();
        let watching: Vec<ConnectionId> = self
            .remote_viewing
            .watchers
            .iter()
            .filter(|(_, (c, _))| *c == computer)
            .map(|(id, _)| *id)
            .collect();
        if watching.is_empty() {
            return;
        }
        let frame = changed(&view);
        self.remote_viewing.views.insert(computer, view);
        for connection in watching {
            self.send(connection, &frame);
        }
    }
}

fn changed(view: &RemoteView) -> Value {
    let frame = json!({ "jsonrpc": "2.0", "method": "remote/changed", "params": view });
    debug_assert!(
        serde_json::from_value::<ServerNotification>(
            json!({ "method": "remote/changed", "params": frame["params"] })
        )
        .is_ok()
    );
    frame
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    let ClientRequest::RemoteWatch(RemoteWatchParams {
        computer_id,
        vehicle_id,
        camera_id,
    }) = request
    else {
        return Err(ApiError::new(ErrorKind::Internal, "not remote/watch"));
    };
    let Some(computer) = computer_id else {
        engine.drop_remote_watch(connection);
        return to_value(&Empty {});
    };
    if engine.remote_viewing.port.is_none() {
        return Err(ApiError::new(
            ErrorKind::InvalidRequest,
            "watching another computer needs UXDuck Desktop on this one",
        ));
    }
    if engine.state.sync.device_id == computer {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "that is this computer. Its vehicles are here already",
        ));
    }
    engine
        .remote_viewing
        .watchers
        .insert(connection, (computer.clone(), vehicle_id));
    match camera_id {
        Some(camera) => {
            engine
                .remote_viewing
                .cameras
                .insert(connection, (computer.clone(), camera));
        }
        None => {
            engine.remote_viewing.cameras.remove(&connection);
        }
    }
    engine.rewant();
    // What is known already, at once.
    if let Some(view) = engine.remote_viewing.views.get(&computer) {
        let frame = changed(view);
        engine.send(connection, &frame);
    }
    to_value(&Empty {})
}
