//! `uxduck-web`: the browser's own daemon (S20, ADR-0011 M2), a second
//! composition root beside the installed one. It runs in a Web Worker with
//! the engine, the RPC router and the MAVLink session; pages reach it
//! through a `MessagePort`, and its only links are simulated vehicles.
//!
//! [`root`] is the root itself and has no JavaScript in it. On `wasm32`,
//! [`WebDaemon`] wraps it for the worker with wasm-bindgen.

pub mod agent;
pub mod gamepads;
pub mod root;

#[cfg(target_arch = "wasm32")]
mod web {
    use js_sys::{Function, Uint8Array};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::{JsCast, JsValue};

    use crate::root::{Browser, Output, Store, Write};

    /// Wall-clock time from `Date.now()`.
    #[derive(Clone, Copy, Debug)]
    struct DateClock;

    impl uxduck_engine::runtime::Clock for DateClock {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds since 1970 are positive and fit"
        )]
        fn now_us(&self) -> u64 {
            (js_sys::Date::now() * 1000.0) as u64
        }
    }

    /// `crypto.getRandomValues`, in a worker or a window.
    #[derive(Debug)]
    struct CryptoRandom;

    impl uxduck_engine::runtime::Random for CryptoRandom {
        fn fill(&mut self, bytes: &mut [u8]) {
            let crypto = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))
                .ok()
                .and_then(|c| c.dyn_into::<web_sys::Crypto>().ok());
            if let Some(crypto) = crypto {
                let _ = crypto.get_random_values_with_u8_array(bytes);
            }
        }
    }

    /// A blob URL for a vehicle file's copy (S121): the page saves it from
    /// there, as a download. Made in the worker, it opens in its pages.
    fn save_url(bytes: &[u8]) -> Option<String> {
        let parts = js_sys::Array::of1(&Uint8Array::from(bytes));
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).ok()?;
        web_sys::Url::create_object_url_with_blob(&blob).ok()
    }

    fn revoke_url(url: &str) {
        let _ = web_sys::Url::revoke_object_url(url);
    }

    /// The worker's handle on the daemon. Every method returns after
    /// handing its outputs to the callbacks given at construction.
    #[wasm_bindgen]
    #[derive(Debug)]
    pub struct WebDaemon {
        browser: Browser,
        /// `(connection, text)`: a frame for a page.
        post: Function,
        /// `(kind, json, seq)`: `"event"` lines and `"checkpoint"` states.
        journal: Function,
        /// `(json)`: a simulator to start or stop.
        simulator: Function,
        /// `(linkId, bytes)`: bytes for a simulator's serial port.
        link_write: Function,
        /// `(request, url)`: fetch it and call `fetched` (S67).
        fetch: Function,
        /// `(op, key, bytes, request)`: terrain files in the Origin Private
        /// File System (S23). `"read"` answers with `stored(request, …)`,
        /// `"write"` keeps `bytes`, `"remove"` removes every file under
        /// `key`.
        store: Function,
        agent: Function,
    }

    #[wasm_bindgen]
    impl WebDaemon {
        /// Restores from the journal and starts.
        ///
        /// # Errors
        /// When the checkpoint cannot be read.
        #[wasm_bindgen(constructor)]
        #[expect(
            clippy::too_many_arguments,
            reason = "the worker's JavaScript constructor has a fixed positional callback ABI"
        )]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "wasm-bindgen passes an optional string by value"
        )]
        pub fn new(
            version: &str,
            checkpoint: Option<String>,
            events: &str,
            post: Function,
            journal: Function,
            simulator: Function,
            link_write: Function,
            fetch: Function,
            store: Function,
            agent: Function,
        ) -> Result<WebDaemon, JsError> {
            let (browser, outputs) = Browser::new(
                version,
                Box::new(DateClock),
                Box::new(DateClock),
                Box::new(CryptoRandom),
                checkpoint.as_deref(),
                events,
            )
            .map_err(|e| JsError::new(&e.0))?;
            let mut browser = browser;
            let mut outputs = outputs;
            outputs.extend(browser.keep_files(save_url, revoke_url));
            let daemon = Self {
                browser,
                post,
                journal,
                simulator,
                link_write,
                fetch,
                store,
                agent,
            };
            daemon.hand(outputs);
            Ok(daemon)
        }

        /// A page connects; returns its connection number.
        pub fn connect(&mut self) -> u32 {
            let (connection, outputs) = self.browser.connect();
            self.hand(outputs);
            connection
        }

        pub fn receive(&mut self, connection: u32, text: &str) {
            let outputs = self.browser.receive(connection, text);
            self.hand(outputs);
        }

        pub fn disconnect(&mut self, connection: u32) {
            let outputs = self.browser.disconnect(connection);
            self.hand(outputs);
        }

        /// The simulator for `link_id` started.
        #[wasm_bindgen(js_name = linkUp)]
        pub fn link_up(&mut self, link_id: &str) {
            let outputs = self.browser.link_status(link_id, true, None);
            self.hand(outputs);
        }

        /// The simulator for `link_id` stopped or could not start.
        #[wasm_bindgen(js_name = linkDown)]
        pub fn link_down(&mut self, link_id: &str, reason: &str) {
            let outputs = self
                .browser
                .link_status(link_id, false, Some(reason.to_owned()));
            self.hand(outputs);
        }

        /// Bytes from a simulator's serial port.
        #[wasm_bindgen(js_name = linkReceived)]
        pub fn link_received(&mut self, link_id: &str, bytes: &[u8]) {
            let outputs = self.browser.link_received(link_id, bytes);
            self.hand(outputs);
        }

        /// What `fetch` asked for: its bytes, or why there are none.
        #[allow(
            clippy::needless_pass_by_value,
            reason = "wasm-bindgen passes optional values by value"
        )]
        pub fn fetched(&mut self, request: u32, bytes: Option<Vec<u8>>, error: Option<String>) {
            let result = bytes.ok_or_else(|| error.unwrap_or_else(|| "no answer".into()));
            let outputs = self.browser.fetched(request, result);
            self.hand(outputs);
        }

        /// What `store` was asked to read: its bytes, or none.
        #[allow(
            clippy::needless_pass_by_value,
            reason = "wasm-bindgen passes optional values by value"
        )]
        pub fn stored(&mut self, request: u32, bytes: Option<Vec<u8>>) {
            let outputs = self.browser.stored(request, bytes);
            self.hand(outputs);
        }

        /// Call every 100 ms.
        pub fn tick(&mut self) {
            let outputs = self.browser.tick();
            self.hand(outputs);
        }

        /// This browser's work as a JSON array of changes, for an installed
        /// daemon's `journal/import`.
        #[must_use]
        pub fn export(&self) -> String {
            serde_json::to_string(&self.browser.export()).unwrap_or_default()
        }

        /// Identity hint from a page, with no session token or provider key.
        pub fn account(&mut self, connection: u32, json: &str) {
            let outputs = self.browser.account(connection, json);
            self.hand(outputs);
        }

        #[wasm_bindgen(js_name = agentEvent)]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "wasm-bindgen passes optional strings by value"
        )]
        pub fn agent_event(&mut self, request: u64, json: Option<String>, error: Option<String>) {
            let outputs = self
                .browser
                .agent_event(request, json.as_deref(), error.as_deref());
            self.hand(outputs);
        }

        fn hand(&self, outputs: Vec<Output>) {
            let this = JsValue::NULL;
            for output in outputs {
                let _ = match output {
                    Output::Agent(order) => self.agent.call1(
                        &this,
                        &JsValue::from(serde_json::to_string(&order).unwrap_or_default()),
                    ),
                    Output::Post { connection, text } => {
                        self.post
                            .call2(&this, &JsValue::from(connection), &JsValue::from(text))
                    }
                    Output::Journal(Write::Event(line)) => self.journal.call3(
                        &this,
                        &JsValue::from_str("event"),
                        &JsValue::from(line),
                        &JsValue::UNDEFINED,
                    ),
                    Output::Journal(Write::Checkpoint { seq, state }) => self.journal.call3(
                        &this,
                        &JsValue::from_str("checkpoint"),
                        &JsValue::from(state),
                        &JsValue::from(seq),
                    ),
                    Output::Simulator(order) => self.simulator.call1(
                        &this,
                        &JsValue::from(serde_json::to_string(&order).unwrap_or_default()),
                    ),
                    Output::LinkWrite { link_id, bytes } => self.link_write.call2(
                        &this,
                        &JsValue::from(link_id),
                        &Uint8Array::from(bytes.as_slice()),
                    ),
                    Output::Fetch { request, url } => {
                        self.fetch
                            .call2(&this, &JsValue::from(request), &JsValue::from(url))
                    }
                    Output::PlaceLookup {
                        request,
                        path,
                        body,
                    } => self.fetch.call3(
                        &this,
                        &JsValue::from(request),
                        &JsValue::from(path),
                        &JsValue::from(body),
                    ),
                    Output::Store(store) => {
                        let (op, key, bytes, request) = match store {
                            Store::Read { request, key } => ("read", key, None, Some(request)),
                            Store::Write { key, bytes } => ("write", key, Some(bytes), None),
                            Store::Remove { prefix } => ("remove", prefix, None, None),
                        };
                        let args = js_sys::Array::of4(
                            &JsValue::from_str(op),
                            &JsValue::from(key),
                            &bytes.map_or(JsValue::UNDEFINED, |b| {
                                Uint8Array::from(b.as_slice()).into()
                            }),
                            &request.map_or(JsValue::UNDEFINED, JsValue::from),
                        );
                        self.store.apply(&this, &args)
                    }
                };
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::WebDaemon;
