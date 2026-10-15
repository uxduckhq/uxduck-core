//! What the agent asks of each model. The output limit leaves room for
//! reasoning, which counts towards it; effort is set rather than left to
//! the model's default, so the cache and the reasoning stay the same from
//! request to request; and a chat is compacted well before a model's
//! context fills, because every request carries the whole of it.

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
            max_output_tokens: 32_000,
            effort: Some("medium"),
            compact_at_tokens: 150_000,
            fallback: Some("claude-sonnet-5-5"),
        },
        // Anthropic's starting point for multistep tool use on Sonnet 5.5.
        "claude-sonnet-5-5" => ModelSettings {
            max_output_tokens: 32_000,
            effort: Some("medium"),
            compact_at_tokens: 150_000,
            fallback: None,
        },
        claude if claude.starts_with("claude-") => ModelSettings {
            max_output_tokens: 32_000,
            effort: None,
            compact_at_tokens: 150_000,
            fallback: None,
        },
        _ => ModelSettings {
            max_output_tokens: 16_000,
            effort: None,
            compact_at_tokens: 100_000,
            fallback: None,
        },
    }
}
