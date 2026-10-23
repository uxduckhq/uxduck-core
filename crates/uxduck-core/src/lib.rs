//! Domain types, ports, services and policy.
//!
//! Depends on `serde` and `futures` only, never on Tokio, sockets or the file
//! system, so it compiles for `wasm32-unknown-unknown` (ADR-0010).

#![forbid(unsafe_code)]

pub mod domain;
pub mod encoding;
pub mod events;
pub mod order;
pub mod policy;
