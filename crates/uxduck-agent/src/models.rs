//! What the agent asks of each model. The output limit is the model's own
//! maximum, from its provider's model page, because reasoning counts
//! towards it and an answer that reaches it is continued (`turn`); effort
//! is set rather than left to the model's default, so the cache and the
//! reasoning stay the same from request to request; and a chat is
//! compacted well before a model's context fills, because every request
//! carries the whole of it.

use crate::conversation::Wire;

/// The models a development build offers with a developer's own keys or
/// the fake, comma-separated; the first is the default. The hosted model
/// offers only what the server says.
pub const DEFAULT_MODELS: &str =
    "gpt-6.1-sol,gpt-6-sol,gpt-6-luna,claude-opus-5-5,claude-sonnet-5-5";

/// Past this many tokens a Claude chat is summarised. The hour-long cache
/// keeps a longer chat cheap to send again (maintainer, 2026-11-14).
const CLAUDE_COMPACT_AT_TOKENS: usize = 150_000;

/// Whether `model`, asked with a developer's own key, takes images in a
/// tool's result: Claude and OpenAI's GPT models do. Open-weight models are
/// not assumed to (FB-244).
#[must_use]
pub fn takes_images(model: &str) -> bool {
    model.starts_with("claude-") || model.starts_with("gpt-") || model == "fake"
}

/// Why `model`, through its company's own API, can't take a camera's
/// frames, in words for the model. None: it can.
#[must_use]
pub fn cannot_see(model: &str) -> Option<String> {
    let family = match Wire::for_model(model) {
        Wire::Anthropic => "Claude",
        Wire::OpenAi => "GPT",
    };
    (!takes_images(model)).then(|| {
        format!(
            "This chat's AI model, {model}, can't take images, so the assistant can't see the \
             camera in this chat. A person can choose a {family} model for this chat."
        )
    })
}

/// The most images `model`'s provider takes in one request, and who that
/// is in words, for the models UXDuck knows a limit for: Anthropic takes
/// 100 for Claude, OpenAI 500 for GPT (FB-244). Past it, the provider
/// refuses the whole request.
#[must_use]
pub fn image_limit(model: &str) -> Option<(usize, &'static str)> {
    if model.contains("claude") {
        Some((100, "Claude takes"))
    } else if model.starts_with("gpt-") || model.starts_with("openai/") {
        Some((500, "OpenAI's GPT models take"))
    } else {
        None
    }
}

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

#[cfg(test)]
mod tests {
    use super::cannot_see;

    #[test]
    fn a_model_by_its_own_key_that_takes_no_images_is_told_which_can() {
        assert_eq!(cannot_see("claude-opus-5-5"), None);
        assert_eq!(cannot_see("gpt-6.1-sol"), None);
        let why = cannot_see("o-mini").unwrap();
        assert!(
            why.contains("o-mini, can't take images") && why.contains("a GPT model"),
            "{why}"
        );
        assert!(!why.contains(';'));
    }
}
