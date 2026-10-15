//! The script host (ADR-0006 X7): one JavaScript engine, built to
//! WebAssembly once and shipped with UXDuck, that runs TypeScript plugins.
//! The daemon strips a plugin's types natively and hands the JavaScript to
//! this module in its config; this module runs it in the same Extism
//! sandbox, with the same memory and time limits, as any plugin, and
//! exports the same functions. A script exports any of `describe`,
//! `onItems`, `onMessages`, `onFrames`, `onCommand`, `onTick` and `onStart`; it may import the
//! helpers of `@uxduck/sdk`, and nothing else.
//!
//! The engine is Boa, which builds for bare WebAssembly with no C
//! toolchain; the ADR names `QuickJS`, which needs a WASI C library to
//! build. Only this module would change.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::context::time::JsInstant;
use boa_engine::context::{Clock, ContextBuilder};
use boa_engine::module::{MapModuleLoader, Module};
use boa_engine::object::builtins::JsPromise;
use boa_engine::{Context, JsObject, JsString, JsValue, Source, js_string};
use extism_pdk::{FnResult, plugin_fn};

/// The SDK's helpers, the very file `packages/sdk` exports.
const SDK: &str = include_str!("../../../packages/sdk/src/runtime.js");

#[link(wasm_import_module = "extism:host/user")]
unsafe extern "C" {
    /// The daemon's clock, in milliseconds since 1970: the sandbox has none.
    fn uxduck_now_ms() -> i64;
}

fn now_ms() -> i64 {
    // SAFETY: a host function with no arguments, provided to every plugin.
    unsafe { uxduck_now_ms() }
}

/// `Date.now()` and timers from the daemon's clock.
struct HostClock;

impl Clock for HostClock {
    fn now(&self) -> JsInstant {
        let ms = u64::try_from(now_ms()).unwrap_or(0);
        JsInstant::new(
            ms / 1000,
            u32::try_from((ms % 1000) * 1_000_000).unwrap_or(0),
        )
    }

    fn system_time_millis(&self) -> i64 {
        now_ms()
    }
}

/// The running script: its engine and what it exports.
struct Script {
    context: Context,
    exports: JsObject,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

fn fail(message: impl std::fmt::Display) -> extism_pdk::Error {
    extism_pdk::Error::msg(message.to_string())
}

/// Waits for a module's promise: modules here have no real waiting.
fn settle(promise: &JsPromise, context: &mut Context) -> Result<(), extism_pdk::Error> {
    context.run_jobs().map_err(fail)?;
    match promise.state() {
        PromiseState::Fulfilled(_) => Ok(()),
        PromiseState::Rejected(why) => Err(fail(why.to_string(context).map_or_else(
            |_| "the script failed".into(),
            |s| s.to_std_string_escaped(),
        ))),
        PromiseState::Pending => Err(fail("the script waited for something that never came")),
    }
}

fn start() -> Result<Script, extism_pdk::Error> {
    let source = extism_pdk::config::get("script")?
        .ok_or_else(|| fail("no script was given to the script host"))?;
    let loader = Rc::new(MapModuleLoader::new());
    let mut context = ContextBuilder::default()
        .clock(Rc::new(HostClock))
        .module_loader(loader.clone())
        .build()
        .map_err(fail)?;
    let sdk = Module::parse(Source::from_bytes(SDK), None, &mut context).map_err(fail)?;
    loader.insert("@uxduck/sdk", sdk);
    let module = Module::parse(
        Source::from_bytes(source.as_bytes()).with_path(std::path::Path::new("plugin.js")),
        None,
        &mut context,
    )
    .map_err(fail)?;
    let promise = module.load_link_evaluate(&mut context);
    settle(&promise, &mut context)?;
    let exports = module.namespace(&mut context);
    Ok(Script { context, exports })
}

/// Calls an export with JSON in and JSON out; `empty` when it has none.
fn call(name: &str, input: &str, empty: &str) -> FnResult<String> {
    SCRIPT.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(start()?);
        }
        let Some(script) = slot.as_mut() else {
            return Err(fail("the script did not start").into());
        };
        let context = &mut script.context;
        let function = script
            .exports
            .get(JsString::from(name), context)
            .map_err(fail)?;
        let Some(function) = function.as_callable() else {
            return Ok(empty.to_owned());
        };
        let json = context
            .global_object()
            .get(js_string!("JSON"), context)
            .map_err(fail)?;
        let json = json.as_object().ok_or_else(|| fail("no JSON"))?.clone();
        let parse = json.get(js_string!("parse"), context).map_err(fail)?;
        let stringify = json.get(js_string!("stringify"), context).map_err(fail)?;
        let parse = parse.as_callable().ok_or_else(|| fail("no JSON.parse"))?;
        let stringify = stringify
            .as_callable()
            .ok_or_else(|| fail("no JSON.stringify"))?;
        let argument = parse
            .call(
                &JsValue::undefined(),
                &[JsString::from(input).into()],
                context,
            )
            .map_err(fail)?;
        let result = function
            .call(&JsValue::undefined(), &[argument], context)
            .map_err(|why| fail(format!("{name}: {why}")))?;
        if result.is_undefined() || result.is_null() {
            return Ok(empty.to_owned());
        }
        let text = stringify
            .call(&JsValue::undefined(), &[result], context)
            .map_err(fail)?;
        Ok(text
            .to_string(context)
            .map_err(fail)?
            .to_std_string_escaped())
    })
}

#[plugin_fn]
pub fn describe(input: String) -> FnResult<String> {
    call("describe", &input, r#"{"panels":[]}"#)
}

#[plugin_fn]
pub fn on_items(input: String) -> FnResult<String> {
    call("onItems", &input, "{}")
}

#[plugin_fn]
pub fn on_messages(input: String) -> FnResult<String> {
    call("onMessages", &input, "{}")
}

#[plugin_fn]
pub fn on_frames(input: String) -> FnResult<String> {
    call("onFrames", &input, "{}")
}

#[plugin_fn]
pub fn on_command(input: String) -> FnResult<String> {
    call("onCommand", &input, "{}")
}

#[plugin_fn]
pub fn on_tick(input: String) -> FnResult<String> {
    call("onTick", &input, "{}")
}

#[plugin_fn]
pub fn on_start(input: String) -> FnResult<String> {
    call("onStart", &input, "{}")
}

/// Random bytes for the engine (`Math.random`, hash seeds): a plugin has no
/// source of its own in the sandbox, and needs none for safety. The
/// signature is getrandom's, `Result` included.
#[allow(clippy::unnecessary_wraps, reason = "getrandom's signature")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    for n in 0..len {
        let mut x = STATE.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        STATE.store(x, Ordering::Relaxed);
        // SAFETY: `dest` is valid for `len` bytes, as getrandom promises.
        unsafe { dest.add(n).write(x.to_le_bytes()[0]) };
    }
    Ok(())
}
