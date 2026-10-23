//! Model providers behind one trait (ADR-0009): the OpenAI Responses and
//! Anthropic Messages APIs, both used statelessly (see `conversation`); the
//! hosted model in front of either; a scripted one for tests; and a wrapper
//! that retries, times out and falls back.

#[cfg(feature = "http")]
pub mod anthropic;
pub mod fake;
#[cfg(feature = "http")]
pub mod hosted;
#[cfg(feature = "http")]
mod http;
pub mod openai;
pub mod resilient;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::conversation::Wire;
use crate::tools::Tool;

/// Waits, with the host's timer; the loop and providers have none.
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// One request: the whole conversation, as the provider's own items.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    /// The same for every request, so it caches.
    pub instructions: String,
    pub conversation: Vec<Value>,
    pub tools: Vec<Tool>,
    pub max_output_tokens: u32,
    pub effort: Option<&'static str>,
    /// The response this one continues: a tool round, or a summary. The
    /// hosted model counts it as part of the same chat.
    pub continues: Option<String>,
    /// Groups requests that share a prefix, for OpenAI's cache.
    pub cache_key: Option<String>,
}

/// Tokens one response used. `input_tokens` is the part read at full price;
/// the whole prompt is that plus what was read from and written to the
/// cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Earlier reasoning the provider set aside because the history did not
    /// match what produced it; more than zero is a bug to look into.
    pub thinking_dropped: u32,
}

impl Usage {
    pub fn add(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.thinking_dropped += other.thinking_dropped;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The response is complete: its id, the items to append to the
    /// conversation exactly as they came, and what it used.
    Completed {
        response_id: String,
        output: Vec<Value>,
        usage: Usage,
    },
    /// The request was sent again after a busy answer or a dropped
    /// connection.
    Retried,
    /// The model stayed busy, so another answers from here.
    FellBack {
        model: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("no API key is set for this model")]
    NoKey,
    #[error("the provider refused the request: {0}")]
    Refused(String),
    /// The hosted model's own answer, such as a trial used up (S29).
    #[error("{0}")]
    Account(String),
    /// Overloaded or rate-limited: worth sending again, after `retry_after`
    /// when the provider says.
    #[error("the model is busy: {message}")]
    Busy {
        message: String,
        retry_after: Option<Duration>,
    },
    /// The connection failed or stalled: worth sending again.
    #[error("the provider could not be reached: {0}")]
    Transport(String),
    /// Something is in the way, such as a network's sign-in page: sending
    /// again will not help.
    #[error("{0}")]
    Network(String),
}

impl ProviderError {
    /// Whether sending the same request again may work.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Busy { .. } | Self::Transport(_))
    }
}

pub trait Provider: Send + Sync {
    /// The items this provider's conversations are made of.
    fn wire(&self) -> Wire;

    /// Starts a response and streams its events.
    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>;
}
