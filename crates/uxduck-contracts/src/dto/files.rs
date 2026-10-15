//! A vehicle's files on the wire (S121, ADR-0027): listings, small files
//! read whole, and transfers as pushed state. A copy's bytes never cross
//! JSON-RPC: the browser saves it over HTTP with a one-time address (F5).
//! An upload's bytes come in base64, up to [`UPLOAD_MAX`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The largest file `files/upload` takes, in bytes: scripts, missions and
/// small configuration files. Larger ones need UXDuck on the vehicle.
pub const UPLOAD_MAX: u64 = 4 * 1024 * 1024;

/// The most `files/read` returns, in bytes; past it, `truncated`.
pub const READ_MAX: u64 = 64 * 1024;

/// One entry of a vehicle's folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    /// Its full path on the vehicle.
    pub path: String,
    /// A folder rather than a file.
    pub dir: bool,
    /// A file's size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesPathParams {
    pub vehicle_id: String,
    /// A path on the vehicle: `/` its storage's root, `@SYS` the
    /// autopilot's own files on ArduPilot.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesListResult {
    pub vehicle_id: String,
    pub path: String,
    /// Folders first, then files, each by name.
    pub entries: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesReadResult {
    pub vehicle_id: String,
    pub path: String,
    /// The file's text, up to 64 KB; bytes that are not UTF-8 replaced.
    pub text: String,
    /// Bytes read.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// The file goes on past what was read.
    pub truncated: bool,
    /// Not text: most of it is not printable.
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesRemoveParams {
    pub vehicle_id: String,
    pub path: String,
    /// An empty folder rather than a file.
    #[serde(default)]
    pub dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesRenameParams {
    pub vehicle_id: String,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesDownloadParams {
    pub vehicle_id: String,
    pub path: String,
    /// The size the listing gave, for progress before the file opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesUploadParams {
    pub vehicle_id: String,
    /// Where it goes on the vehicle; a file already there is replaced.
    pub path: String,
    /// The file, base64, up to 4 MB.
    pub content_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileTransferParams {
    /// The transfer's id.
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileSaveResult {
    /// A one-time address on this UXDuck that answers with the copy as an
    /// attachment, for a minute.
    pub url: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FileTransferState {
    Queued,
    Running,
    Paused,
    /// The vehicle went away or stopped answering; it resumes when the
    /// vehicle is back.
    Waiting,
    Failed,
    Done,
}

/// A download from a vehicle or an upload to it, pushed as it moves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileTransfer {
    pub id: String,
    pub vehicle_id: String,
    pub direction: TransferDirection,
    /// The file's path on the vehicle.
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub size: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub done: u64,
    pub state: FileTransferState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Bytes a second over the last five seconds, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub rate_bps: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
}
