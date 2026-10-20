//! Native HTTP adapter for the engine's global place lookups. No API keys,
//! cookies, referrers or query-bearing URLs; all provider access stays behind
//! the selected UXDuck API. Network work never blocks the engine task.

use std::time::Duration;

use serde_json::Value;
use uxduck_contracts::errors::ApiError;
use uxduck_engine::places::{self, Order, PlacePort};

use crate::daemon::engine_task::EngineHandle;

pub struct Host {
    api: Option<String>,
    engine: EngineHandle,
}

impl Host {
    #[must_use]
    pub fn new(engine: EngineHandle) -> Self {
        Self {
            api: crate::dev_api::api_url(),
            engine,
        }
    }
}

impl PlacePort for Host {
    fn request(&mut self, order: Order) {
        let api = self.api.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let result = lookup(api.as_deref(), &order).await;
            let _ = engine
                .run(move |e| e.place_lookup_done(&order.id, result))
                .await;
        });
    }
}

async fn lookup(api: Option<&str>, order: &Order) -> Result<Value, ApiError> {
    let api = api.ok_or_else(|| places::failed(Some(503)))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .build()
        .map_err(|_| places::failed(None))?;
    let mut result = client
        .post(format!(
            "{}/v1/places/{}",
            api.trim_end_matches('/'),
            order.action.path()
        ))
        .json(&order.action.body())
        .send()
        .await
        .map_err(|_| places::failed(None))?;
    if !result.status().is_success() {
        return Err(places::failed(Some(result.status().as_u16())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = result.chunk().await.map_err(|_| places::failed(None))? {
        if bytes.len() + chunk.len() > places::MAX_RESPONSE_BYTES {
            return Err(places::failed(None));
        }
        bytes.extend_from_slice(&chunk);
    }
    places::response(&order.action, &bytes)
}
