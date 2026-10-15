//! Each chat's conversation as its provider saw it (`agent/transcripts/` in
//! the data directory), kept after every turn that ended well so the next
//! one replays it byte for byte: the provider's prompt cache then reads the
//! whole chat instead of writing it again, and Anthropic's check of earlier
//! reasoning passes (see `uxduck_agent::conversation`).
//!
//! A transcript is used only while it still matches the chat: the same
//! provider, and exactly the messages the journal has before the new one.
//! Anything else (another provider, a turn that failed, a chat synced from
//! another computer) is rebuilt from the journal instead. Not journaled and
//! not synced: it is a cache of what was sent, holding the same words as
//! the journal and the providers' opaque reasoning tokens.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uxduck_agent::Wire;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Saved {
    pub wire: Wire,
    pub model: String,
    /// The journal's items for the chat when this was kept, in order.
    pub covers: Vec<String>,
    pub context: Option<String>,
    pub context_sent: bool,
    /// The last response, which a summary of the chat continues.
    pub last_response: Option<String>,
    pub conversation: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Transcripts {
    dir: PathBuf,
}

impl Transcripts {
    #[must_use]
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            dir: data_dir.join("agent").join("transcripts"),
        }
    }

    fn path(&self, thread_id: &str) -> Option<PathBuf> {
        let safe = !thread_id.is_empty()
            && thread_id.len() <= 64
            && thread_id.chars().all(|c| c.is_ascii_alphanumeric());
        safe.then(|| self.dir.join(format!("{thread_id}.json")))
    }

    /// The kept transcript, if it still matches: the same wire, covering
    /// exactly `items`.
    #[must_use]
    pub fn matching(&self, thread_id: &str, wire: Wire, items: &[String]) -> Option<Saved> {
        let text = std::fs::read(self.path(thread_id)?).ok()?;
        let saved: Saved = serde_json::from_slice(&text).ok()?;
        (saved.wire == wire && saved.covers == items).then_some(saved)
    }

    /// Removes the transcripts of chats that are no longer live, such as
    /// deleted ones, keeping those `live` says yes to.
    pub fn prune(&self, live: impl Fn(&str) -> bool) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let thread = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if !live(thread) || path.extension().is_none_or(|ext| ext != "json") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Keeps a transcript, replacing the last; a failed write only costs
    /// the next turn a rebuild.
    pub fn keep(&self, thread_id: &str, saved: &Saved) {
        let Some(path) = self.path(thread_id) else {
            return;
        };
        let Ok(text) = serde_json::to_vec(saved) else {
            return;
        };
        let partial = path.with_extension("json.partial");
        let written = std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(&partial, text))
            .and_then(|()| std::fs::rename(&partial, &path));
        if let Err(error) = written {
            tracing::warn!(%error, "a chat's transcript was not kept; the next turn rebuilds it");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_transcript_is_used_only_while_it_matches_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = Transcripts::new(dir.path());
        let saved = Saved {
            wire: Wire::Anthropic,
            model: "claude-opus-5-5".into(),
            covers: vec!["it1".into(), "it2".into()],
            context: None,
            context_sent: false,
            last_response: Some("msg_1".into()),
            conversation: vec![json!({ "role": "user", "content": [] })],
        };
        transcripts.keep("th1", &saved);
        let items = ["it1".to_owned(), "it2".to_owned()];
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items),
            Some(saved.clone())
        );
        assert_eq!(
            transcripts.matching("th1", Wire::OpenAi, &items),
            None,
            "another provider"
        );
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items[..1]),
            None,
            "the chat changed"
        );
        assert_eq!(
            transcripts.matching("../etc", Wire::Anthropic, &items),
            None
        );

        transcripts.keep("th2", &saved);
        transcripts.prune(|thread| thread == "th2");
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items),
            None,
            "a deleted chat's goes"
        );
        assert!(
            transcripts
                .matching("th2", Wire::Anthropic, &items)
                .is_some()
        );
    }
}
