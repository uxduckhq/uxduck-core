//! MAVLink: framing, decoding, identity, presence and tlog, as logic over
//! bytes and time (ADR-0004, ADR-0010). Only [`links`], behind the `native`
//! feature, touches sockets, so the rest builds for `wasm32-unknown-unknown`
//! and the browser simulator (S20) can run it.
//!
//! The only crate that sees MAVLink library types (`mavio` today, Maviola
//! for serial and TCP links from S7).

pub mod camera;
pub mod commands;
pub mod decode;
pub mod dialect;
pub mod fake_files;
pub mod fake_terrain;
pub mod files;
pub mod framing;
pub mod ground_traffic;
pub mod identity;
#[cfg(feature = "named")]
pub mod named;
/// ArduPilot modes, shared with the engine.
pub use uxduck_core::domain::modes;
pub mod producers;
mod rc_input;
pub mod router;
pub mod session;
pub mod sim;
pub mod terrain;
pub mod tlog;
pub mod traffic;
pub mod transfers;
pub mod usb_ids;

#[cfg(feature = "native")]
pub mod links;
#[cfg(feature = "native")]
pub mod probe;

/// The daemon's own MAVLink identity when it sends: system 255, the
/// conventional ground station, and component 190 (`MAV_COMP_ID_MISSIONPLANNER`).
pub const GCS_SYSTEM_ID: u8 = 255;
pub const GCS_COMPONENT_ID: u8 = 190;
