//! Moving a browser's work into an installed daemon (ADR-0011, S20).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The browser's work, as the changes that would create it: operators,
/// chats with their messages, folders and saved places, each as the
/// journal's `...Upserted` change carrying the whole entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalImportParams {
    /// Where it comes from, such as `browser 1f3a…`; recorded on every
    /// change it makes.
    pub source: String,
    /// The journal's changes, as it writes them. Other kinds, such as
    /// simulated vehicles and their links, are left out.
    pub changes: Vec<serde_json::Value>,
}

/// What an import did. Importing the same work again imports nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalImportResult {
    /// Entities new here, or newer than the copy here.
    pub imported: u32,
    /// Entities already here at the same or a newer version, and changes of
    /// kinds that do not move.
    pub skipped: u32,
}
