// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The JS→Rust bridge for a hooked engine (the in-process plugin host):
//! a push channel and a synchronous ctx-RPC callback, installed as plain
//! globals on the worker before any module runs.
//!
//! `globalThis.__oj_post(json)` parses the string and delivers it on the
//! engine's [`EngineHooks::post`] channel — the in-process replacement for a
//! sidecar's control-plane stdout pushes. A value return cannot be spliced by
//! a plugin's `console.log`, so none of the line-framing (control tokens,
//! re-push-until-ACK) survives the move.
//!
//! `globalThis.__oj_rpc(method, argsJson)` calls [`EngineHooks::rpc`]
//! synchronously ON the isolate thread and returns the result's JSON text (or
//! `null`, or throws with the handler's error). The handlers oj installs are
//! plain synchronous functions (resolver lookups, a file read + compile), so
//! a sync op keeps the whole reverse-RPC reply machinery out of the protocol.
//!
//! Both callbacks find their state in thread-locals: the crate's invariant is
//! one isolate per thread, so the thread IS the engine identity, and no
//! deno_core extension (with its snapshot/ops-visibility constraints) is
//! needed.

use std::cell::RefCell;
use std::sync::Arc;

use deno_core::v8;
use deno_runtime::worker::MainWorker;
use tokio::sync::mpsc;

/// Synchronous host-side handler for `__oj_rpc` calls. Runs on the isolate
/// thread while a hook awaits, so it must not block on that same engine.
pub type RpcHandler =
    Arc<dyn Fn(&str, &[serde_json::Value]) -> Result<serde_json::Value, String> + Send + Sync>;

/// JS→Rust bridges given to [`crate::JsEngine::spawn_with_hooks`].
pub struct EngineHooks {
    /// Receives every `__oj_post(json)` as parsed JSON, in call order.
    pub post: mpsc::UnboundedSender<serde_json::Value>,
    /// Answers `__oj_rpc(method, argsJson)`; absent, the call throws.
    pub rpc: Option<RpcHandler>,
}

thread_local! {
    static POST: RefCell<Option<mpsc::UnboundedSender<serde_json::Value>>> =
        const { RefCell::new(None) };
    static RPC: RefCell<Option<RpcHandler>> = const { RefCell::new(None) };
}

/// Installs the bridge globals on the worker's main context and parks the
/// hook state in this thread's slots. Engine-thread only, before any job.
pub(crate) fn install(worker: &mut MainWorker, hooks: EngineHooks) {
    POST.with(|slot| *slot.borrow_mut() = Some(hooks.post));
    RPC.with(|slot| *slot.borrow_mut() = hooks.rpc);
    deno_core::scope!(scope, &mut worker.js_runtime);
    let context = scope.get_current_context();
    let global = context.global(scope);
    let post_key = v8::String::new(scope, "__oj_post").unwrap();
    let post_fn = v8::Function::new(scope, post_callback).unwrap();
    global.set(scope, post_key.into(), post_fn.into());
    let rpc_key = v8::String::new(scope, "__oj_rpc").unwrap();
    let rpc_fn = v8::Function::new(scope, rpc_callback).unwrap();
    global.set(scope, rpc_key.into(), rpc_fn.into());
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message).unwrap_or_else(|| v8::String::empty(scope));
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}

fn post_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let text = args.get(0).to_rust_string_lossy(scope);
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => POST.with(|slot| {
            if let Some(tx) = slot.borrow().as_ref() {
                // A dropped receiver means the Rust side abandoned the engine;
                // the push is then noise, not an error the JS side can act on.
                let _ = tx.send(value);
            }
        }),
        Err(e) => throw(scope, &format!("__oj_post takes one JSON string: {e}")),
    }
}

fn rpc_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let handler = RPC.with(|slot| slot.borrow().clone());
    let Some(handler) = handler else {
        throw(scope, "__oj_rpc has no handler on this engine");
        return;
    };
    let method = args.get(0).to_rust_string_lossy(scope);
    let raw_args = args.get(1).to_rust_string_lossy(scope);
    let parsed: Vec<serde_json::Value> = match serde_json::from_str(&raw_args) {
        Ok(serde_json::Value::Array(items)) => items,
        Ok(_) | Err(_) => Vec::new(),
    };
    // A handler panic must not unwind across the V8 callback boundary.
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&method, &parsed)))
            .unwrap_or_else(|_| Err(format!("__oj_rpc handler panicked running {method}")));
    match result {
        Ok(serde_json::Value::Null) => rv.set_null(),
        Ok(value) => match serde_json::to_string(&value)
            .ok()
            .and_then(|text| v8::String::new(scope, &text))
        {
            Some(text) => rv.set(text.into()),
            None => throw(scope, "__oj_rpc result is not representable"),
        },
        Err(message) => throw(scope, &message),
    }
}
