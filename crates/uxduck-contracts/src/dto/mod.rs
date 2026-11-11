//! Wire DTOs, one module per entity family.

pub mod account;
pub mod analytics;
pub mod app;
pub mod checklist;
pub mod client;
pub mod command;
pub mod companion;
pub mod controller;
pub mod diagnostics;
pub mod dialect;
pub mod edge;
pub mod feedback;
pub mod files;
pub mod find;
pub mod firmware_profile;
pub mod flight;
pub mod gripper;
pub mod history;
pub mod initialize;
pub mod inspect;
pub mod interface;
pub mod journal;
pub mod link;
pub mod location;
pub mod marker;
pub mod messages;
pub mod mission;
pub mod mission_read;
pub mod network;
pub mod notification;
pub mod operation;
pub mod operator;
pub mod pairing;
pub mod param_definitions;
pub mod parameter;
pub mod plan;
pub mod plugin;
pub mod practice;
pub mod preflight;
pub mod release;
pub mod replay;
pub mod search;
pub mod settings;
pub mod setup;
pub mod sim;
pub mod state;
pub mod support;
pub mod sync;
pub mod telemetry;
pub mod telemetry_window;
pub mod terrain;
pub mod thread;
pub mod trace;
pub mod traffic;
pub mod tray;
pub mod tuning;
pub mod update;
pub mod vehicle;
pub mod video;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params or result of a call that carries nothing. Params may be omitted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}
