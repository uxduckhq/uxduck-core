//! The agent loop, shared by the daemon (native) and the cloud (wasm32)
//! (ADR-0009). A turn sends the chat's whole conversation to a provider,
//! streams its text, and runs its tool calls as API calls through the host,
//! which makes them as the agent client; then it hands the results back
//! until the provider answers without calling a tool. The conversation
//! comes back whole, for the host to keep and replay (see `conversation`).

pub mod conversation;
pub mod models;
pub mod provider;
pub mod tools;
pub mod turn;

pub use conversation::{Entry, Wire};
pub use provider::{Provider, ProviderError, ProviderEvent, ProviderRequest, Usage};
pub use tools::{Tool, tools};
pub use turn::{Host, Turn, TurnError, TurnOutcome, TurnStats, run_turn};
