//! The `uxduck` provider (ADR-0013 P5, S29): the hosted model, for a
//! signed-in daemon with no key of its own. The same APIs as the direct
//! providers, at `<api>/v1/model`, with the 5-minute signed session as the
//! key and headers saying what may be kept and which chat this is. A tool
//! round or a summary says which response it continues, so the hosted
//! model counts it as part of the same chat.

use super::Provider;
use super::anthropic::AnthropicProvider;
use super::openai::OpenAiProvider;
use crate::conversation::Wire;

/// A provider for one turn. `share` is whether the chat's turns may be kept
/// (the conversations switch, and the chat's own "Don't use this chat");
/// `chat` is a random id for the chat, not the daemon's own.
#[must_use]
pub fn provider(
    api: &str,
    session: &str,
    share: bool,
    chat: &str,
    wire: Wire,
) -> Box<dyn Provider> {
    let base = format!("{}/v1/model", api.trim_end_matches('/'));
    let headers = [
        (
            "x-uxduck-share",
            if share { "conversations" } else { "none" },
        ),
        ("x-uxduck-thread", chat),
    ];
    match wire {
        Wire::OpenAi => Box::new(OpenAiProvider::hosted(session, &base, &headers)),
        Wire::Anthropic => Box::new(AnthropicProvider::hosted(session, &base, &headers)),
    }
}
