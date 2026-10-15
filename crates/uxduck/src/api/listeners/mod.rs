//! One module per transport. Each accepts connections, admits them with a
//! `Trust`, and moves frames between the connection and the engine task.

pub mod http;
pub mod local;
