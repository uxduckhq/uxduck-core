//! A vehicle's files (S121, ADR-0027): copies UXDuck makes of them and
//! files it sends, as transfers that belong to UXDuck rather than to a
//! window (F4). The bytes live in the "Vehicle files" storage category; the
//! journal keeps where each transfer stands.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferDirection {
    /// From the vehicle to a copy on this computer.
    Download,
    /// From this computer to the vehicle.
    Upload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferState {
    /// Waiting for the vehicle's file transfer, behind another.
    Queued,
    Running,
    /// Stopped by someone; resumes from where it stopped.
    Paused,
    /// The vehicle went away or stopped answering: it resumes when the
    /// vehicle is back.
    Waiting,
    Failed,
    Done,
}

impl TransferState {
    /// Still to happen: queued, running or waiting for the vehicle.
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }
}

/// One download or upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTransfer {
    pub id: String,
    pub vehicle_id: String,
    pub direction: TransferDirection,
    /// The file's path on the vehicle.
    pub path: String,
    /// Its name, the path's last part.
    pub name: String,
    /// Where the copy is kept, relative to "Vehicle files": the vehicle's
    /// id and the path on the vehicle. Downloads only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// The whole file's bytes, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Bytes copied or sent so far.
    pub done: u64,
    pub state: TransferState,
    /// Why it failed or waits, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Bytes a second over the last five seconds, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_bps: Option<u64>,
    pub started_at_us: u64,
    pub updated_at_us: u64,
}
