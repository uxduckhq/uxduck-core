//! Each chat's conversation as its provider saw it (`agent/transcripts/` in
//! the data directory), kept after every turn that ended well so the next
//! one replays it byte for byte: the provider's prompt cache then reads the
//! whole chat instead of writing it again, and Anthropic's check of earlier
//! reasoning passes (see `uxduck_agent::conversation`).
//!
//! A transcript is used only while it still matches the chat: the same
//! provider, and exactly the messages the journal has before the new one.
//! And only while the provider likely still caches it: after its
//! `Provider::cached_for_ms` (55 minutes for Claude, which is cached for
//! an hour, 25 for GPT-6, cached for half an hour, and 5 for other models)
//! the replay would be written to the cache again in
//! full, so the slimmer rebuild is sent instead. Anything else (another
//! provider, a turn that failed, a chat synced from another computer, a
//! cache that lapsed) is rebuilt from the journal instead. Not journaled and
//! not synced: it is a cache of what was sent, holding the same words as
//! the journal and the providers' opaque reasoning tokens.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uxduck_agent::Wire;
use uxduck_agent::conversation::cache_warm;

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
    /// When it was last sent to the provider, in Unix milliseconds; 0 when
    /// not known, as for a transcript kept before this was recorded.
    #[serde(default)]
    pub sent_at_ms: u64,
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
    /// exactly `items`, and likely still at `now_ms` (Unix milliseconds) in
    /// the cache of a provider that keeps it `cached_for_ms`.
    #[must_use]
    pub fn matching(
        &self,
        thread_id: &str,
        wire: Wire,
        items: &[String],
        cached_for_ms: u64,
        now_ms: u64,
    ) -> Option<Saved> {
        let text = std::fs::read(self.path(thread_id)?).ok()?;
        let saved: Saved = serde_json::from_slice(&text).ok()?;
        (saved.wire == wire
            && saved.covers == items
            && cache_warm(cached_for_ms, saved.sent_at_ms, now_ms))
        .then_some(saved)
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
    use uxduck_agent::conversation::{CACHE_LIFETIME_MS, HOUR_CACHE_REPLAY_MS};

    use super::*;

    #[test]
    fn a_transcript_is_used_only_while_it_matches_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = Transcripts::new(dir.path());
        let sent = 1_700_000_000_000;
        let saved = Saved {
            wire: Wire::Anthropic,
            model: "claude-opus-5-5".into(),
            covers: vec!["it1".into(), "it2".into()],
            context: None,
            context_sent: false,
            last_response: Some("msg_1".into()),
            conversation: vec![json!({ "role": "user", "content": [] })],
            sent_at_ms: sent,
        };
        transcripts.keep("th1", &saved);
        let items = ["it1".to_owned(), "it2".to_owned()];
        let now = sent + 60_000;
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items, CACHE_LIFETIME_MS, now),
            Some(saved.clone())
        );
        assert_eq!(
            transcripts.matching("th1", Wire::OpenAi, &items, CACHE_LIFETIME_MS, now),
            None,
            "another provider"
        );
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items[..1], CACHE_LIFETIME_MS, now),
            None,
            "the chat changed"
        );
        assert_eq!(
            transcripts.matching("../etc", Wire::Anthropic, &items, CACHE_LIFETIME_MS, now),
            None
        );

        transcripts.keep("th2", &saved);
        transcripts.prune(|thread| thread == "th2");
        assert_eq!(
            transcripts.matching("th1", Wire::Anthropic, &items, CACHE_LIFETIME_MS, now),
            None,
            "a deleted chat's goes"
        );
        assert!(
            transcripts
                .matching("th2", Wire::Anthropic, &items, CACHE_LIFETIME_MS, now)
                .is_some()
        );
    }

    #[test]
    fn a_transcript_is_used_only_while_the_provider_likely_caches_it() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = Transcripts::new(dir.path());
        let sent = 1_700_000_000_000;
        let items = ["it1".to_owned()];
        transcripts.keep(
            "th1",
            &Saved {
                wire: Wire::Anthropic,
                model: "claude-opus-5-5".into(),
                covers: items.to_vec(),
                context: None,
                context_sent: false,
                last_response: None,
                conversation: Vec::new(),
                sent_at_ms: sent,
            },
        );
        let at = |now| transcripts.matching("th1", Wire::Anthropic, &items, CACHE_LIFETIME_MS, now);
        assert!(at(sent + CACHE_LIFETIME_MS).is_some());
        assert!(
            at(sent + CACHE_LIFETIME_MS + 1).is_none(),
            "the cache lapsed"
        );
        let hour =
            |now| transcripts.matching("th1", Wire::Anthropic, &items, HOUR_CACHE_REPLAY_MS, now);
        assert!(
            hour(sent + CACHE_LIFETIME_MS + 1).is_some(),
            "an own key's hour cache"
        );
        assert!(hour(sent + HOUR_CACHE_REPLAY_MS).is_some());
        assert!(hour(sent + HOUR_CACHE_REPLAY_MS + 1).is_none());

        let old = r#"{"wire":"anthropic","model":"claude-opus-5-5","covers":["it1"],"context":null,"contextSent":false,"lastResponse":null,"conversation":[]}"#;
        std::fs::write(dir.path().join("agent/transcripts/th1.json"), old).unwrap();
        assert!(
            at(sent).is_none(),
            "kept before the time was, so not known to be warm"
        );
    }
}
