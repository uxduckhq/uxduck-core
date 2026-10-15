//! Retries, timeouts and a fallback model around any provider. A request
//! that fails before anything streams (busy, rate-limited, a dropped
//! connection) is sent again after a backoff, or after the provider's
//! `retry-after`; one that stays busy goes to the fallback model, on the
//! same API, if there is one. Once an answer has started streaming nothing
//! is retried, since part of it has already been shown, but a stream that
//! stalls is ended rather than left hanging.

use std::sync::Arc;
use std::time::Duration;

use futures::future::{Either, select};
use futures::stream::{self, BoxStream, StreamExt};

use super::{Provider, ProviderError, ProviderEvent, ProviderRequest, Sleep};
use crate::conversation::Wire;

pub struct Resilient {
    inner: Arc<dyn Provider>,
    sleep: Sleep,
    /// Sends of one request, the first included.
    attempts: u32,
    /// How long to wait for the first event of an answer.
    first_event: Duration,
    /// How long an answer may go quiet once it has started.
    idle: Duration,
    fallback: Option<String>,
}

impl std::fmt::Debug for Resilient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resilient")
            .field("attempts", &self.attempts)
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl Resilient {
    #[must_use]
    pub fn new(inner: Box<dyn Provider>, sleep: Sleep) -> Self {
        Self {
            inner: Arc::from(inner),
            sleep,
            attempts: 3,
            first_event: Duration::from_secs(120),
            idle: Duration::from_secs(120),
            fallback: None,
        }
    }

    /// The model asked instead when this one stays busy.
    #[must_use]
    pub fn with_fallback(mut self, model: Option<&str>) -> Self {
        self.fallback = model.map(str::to_owned);
        self
    }

    #[must_use]
    pub const fn with_timeouts(mut self, first_event: Duration, idle: Duration) -> Self {
        self.first_event = first_event;
        self.idle = idle;
        self
    }

    #[must_use]
    pub const fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }
}

/// Exponential backoff, or what the provider asked for, capped.
fn backoff(attempt: u32, error: &ProviderError) -> Duration {
    let asked = match error {
        ProviderError::Busy { retry_after, .. } => *retry_after,
        _ => None,
    };
    asked
        .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(4)))
        .min(Duration::from_secs(30))
}

enum State {
    Send {
        request: ProviderRequest,
        attempt: u32,
        fell_back: bool,
    },
    Streaming(BoxStream<'static, Result<ProviderEvent, ProviderError>>),
    Done,
}

struct Shared {
    inner: Arc<dyn Provider>,
    sleep: Sleep,
    attempts: u32,
    first_event: Duration,
    idle: Duration,
    fallback: Option<String>,
}

/// The next item of `stream`, or `None` inside `Some` when `limit` passed.
async fn within(
    sleep: &Sleep,
    limit: Duration,
    stream: &mut BoxStream<'static, Result<ProviderEvent, ProviderError>>,
) -> Option<Option<Result<ProviderEvent, ProviderError>>> {
    match select(stream.next(), sleep(limit)).await {
        Either::Left((next, _)) => Some(next),
        Either::Right(((), _)) => None,
    }
}

async fn step(
    shared: Arc<Shared>,
    state: State,
) -> Option<(Result<ProviderEvent, ProviderError>, State)> {
    match state {
        State::Done => None,
        State::Streaming(mut stream) => match within(&shared.sleep, shared.idle, &mut stream).await
        {
            None => Some((
                Err(ProviderError::Transport(
                    "the model stopped answering".into(),
                )),
                State::Done,
            )),
            Some(Some(Err(error))) => Some((Err(error), State::Done)),
            Some(Some(item)) => Some((item, State::Streaming(stream))),
            Some(None) => None,
        },
        State::Send {
            mut request,
            attempt,
            fell_back,
        } => {
            let mut stream = shared.inner.respond(request.clone());
            let error = match within(&shared.sleep, shared.first_event, &mut stream).await {
                Some(Some(Ok(event))) => return Some((Ok(event), State::Streaming(stream))),
                Some(Some(Err(error))) => error,
                Some(None) => ProviderError::Transport("the answer was empty".into()),
                None => ProviderError::Transport("no answer came in time".into()),
            };
            if !error.retryable() {
                return Some((Err(error), State::Done));
            }
            if attempt + 1 < shared.attempts {
                (shared.sleep)(backoff(attempt, &error)).await;
                return Some((
                    Ok(ProviderEvent::Retried),
                    State::Send {
                        request,
                        attempt: attempt + 1,
                        fell_back,
                    },
                ));
            }
            match (&shared.fallback, fell_back) {
                (Some(model), false) if *model != request.model => {
                    model.clone_into(&mut request.model);
                    Some((
                        Ok(ProviderEvent::FellBack {
                            model: model.clone(),
                        }),
                        State::Send {
                            request,
                            attempt: 0,
                            fell_back: true,
                        },
                    ))
                }
                _ => Some((Err(error), State::Done)),
            }
        }
    }
}

impl Provider for Resilient {
    fn wire(&self) -> Wire {
        self.inner.wire()
    }

    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let shared = Arc::new(Shared {
            inner: self.inner.clone(),
            sleep: self.sleep.clone(),
            attempts: self.attempts.max(1),
            first_event: self.first_event,
            idle: self.idle,
            fallback: self.fallback.clone(),
        });
        let start = State::Send {
            request,
            attempt: 0,
            fell_back: false,
        };
        stream::unfold(start, move |state| step(shared.clone(), state)).boxed()
    }
}
