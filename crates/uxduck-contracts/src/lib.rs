//! Wire contracts between the daemon and its clients (ADR-0001 D2, D3).
//!
//! Rust DTOs are the source of truth. `cargo xtask contracts` turns them into
//! JSON Schema, wraps it in an OpenAPI 3.1 document, and hey-api generates
//! `packages/protocol` from that.

#![forbid(unsafe_code)]
// `Option<Option<T>>` is the wire's absent, null or value (see `nullable`).
#![allow(clippy::option_option)]

pub mod convert;
pub mod dto;
pub mod errors;
pub mod nullable;
pub mod pdef;
pub mod rpc;
pub mod safe;
pub mod version;
