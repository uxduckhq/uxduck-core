//! What the agent asks of each model. The output limit is the model's own
//! maximum, from its provider's model page, because reasoning counts
//! towards it and an answer that reaches it is continued (`turn`); effort
//! is set rather than left to the model's default, so the cache and the
//! reasoning stay the same from request to request; and a chat is
//! compacted well before a model's context fills, because every request
//! carries the whole of it.

/// The models a development build offers with a developer's own keys or
/// the fake, comma-separated; the first is the default. The hosted model
/// offers only what the server says.
pub const DEFAULT_MODELS: &str =
    "gpt-6.1-sol,gpt-6-sol,gpt-6-luna,claude-opus-5-5,claude-sonnet-5-5";

/// Past this many tokens a Claude chat is summarised. The hour-long cache
/// keeps a longer chat cheap to send again (maintainer, 2026-11-14).
const CLAUDE_COMPACT_AT_TOKENS: usize = 150_000;

/// Settings for one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSettings {
    pub max_output_tokens: u32,
    /// The Messages API's `output_config.effort`; none leaves the default.
    pub effort: Option<&'static str>,
    /// Past this many tokens, the chat is summarised before the next turn.
    pub compact_at_tokens: usize,
    /// Asked instead, on the same API, when this one stays overloaded.
    pub fallback: Option<&'static str>,
}

/// The settings for `model`.
#[must_use]
pub fn settings(model: &str) -> ModelSettings {
    match model {
        // Opus 5.5 always thinks; `medium` is its default, pinned here.
        "claude-opus-5-5" => ModelSettings {
            max_output_tokens: 128_000,
            effort: Some("medium"),
            compact_at_tokens: CLAUDE_COMPACT_AT_TOKENS,
            fallback: Some("claude-sonnet-5-5"),
        },
        // Anthropic's starting point for multistep tool use on Sonnet 5.5.
        "claude-sonnet-5-5" => ModelSettings {
            max_output_tokens: 128_000,
            effort: Some("medium"),
            compact_at_tokens: CLAUDE_COMPACT_AT_TOKENS,
            fallback: None,
        },
        claude if claude.starts_with("claude-") => ModelSettings {
            max_output_tokens: 32_000,
            effort: None,
            compact_at_tokens: CLAUDE_COMPACT_AT_TOKENS,
            fallback: None,
        },
        // https://developers.openai.com/api/docs/models/gpt-6.1-sol,
        // https://developers.openai.com/api/docs/models/gpt-6-sol and
        // https://developers.openai.com/api/docs/models/gpt-6-luna:
        // "128,000 max output tokens" each (read 2026-11-14).
        "gpt-6.1-sol" | "gpt-6-sol" | "gpt-6-luna" => ModelSettings {
            max_output_tokens: 128_000,
            effort: None,
            compact_at_tokens: 100_000,
            fallback: None,
        },
        // Every other model, as `models.ts` has it. That includes the ones
        // OpenRouter serves (Kimi K3.1 Pro, GLM-5.3, DeepSeek V4-Pro and
        // V4.1-Flash): each OpenRouter endpoint sets its own output limit,
        // GLM-5.3's is 32,768 on one of them, and the hosted model holds
        // them to 16,000.
        _ => ModelSettings {
            max_output_tokens: 16_000,
            effort: None,
            compact_at_tokens: 100_000,
            fallback: None,
        },
    }
}
