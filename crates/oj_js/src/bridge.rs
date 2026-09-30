// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The JS→Rust bridge for a hooked engine (the in-process plugin host),
//! installed as plain globals on the worker before any module runs:
//! `globalThis.__oj_post(json)` delivers parsed JSON on the engine's
//! [`EngineHooks::post`] channel; `globalThis.__oj_rpc(method, argsJson)`
//! calls [`EngineHooks::rpc`] synchronously ON the isolate thread and returns
//! the result's JSON text (or `null`, or throws with the handler's error).
//!
//! The hooks ride each function's `data` slot as a `v8::External` — V8's own
//! way of giving a callback its state — and are owned by the runtime's
//! `OpState`, so the pointer lives exactly as long as the isolate that can
//! call it.

use deno_core::v8;
use deno_runtime::worker::MainWorker;
use tokio::sync::mpsc;

/// Synchronous host-side handler for `__oj_rpc` calls. Runs on the isolate
/// thread while a hook awaits, so it must not block on that same engine.
/// A trait object, not a generic: the handler closes over dev-server state
/// this crate cannot name, and the call is one indirection either way.
pub type RpcHandler =
    Box<dyn Fn(&str, &[serde_json::Value]) -> Result<serde_json::Value, String> + Send>;

/// JS→Rust bridges given to [`crate::JsEngine::spawn`].
pub struct EngineHooks {
    /// Receives every `__oj_post(json)` as parsed JSON, in call order.
    pub post: mpsc::UnboundedSender<serde_json::Value>,
    /// Answers `__oj_rpc(method, argsJson)`; absent, the call throws.
    pub rpc: Option<RpcHandler>,
}

/// Installs the bridge globals on the worker's main context. The hooks are
/// parked in `OpState` (dropped with the runtime) and each callback receives
/// a pointer to them through its function `data`.
pub(crate) fn install(worker: &mut MainWorker, hooks: EngineHooks) {
    let hooks = Box::new(hooks);
    let ptr: *const EngineHooks = &*hooks;
    worker.js_runtime.op_state().borrow_mut().put(hooks);

    deno_core::scope!(scope, &mut worker.js_runtime);
    let external = v8::External::new(scope, ptr as *mut std::ffi::c_void);
    let context = scope.get_current_context();
    let global = context.global(scope);
    let post_fn = v8::Function::builder(post_callback)
        .data(external.into())
        .build(scope)
        .unwrap();
    let post_key = v8::String::new(scope, "__oj_post").unwrap();
    global.set(scope, post_key.into(), post_fn.into());
    let rpc_fn = v8::Function::builder(rpc_callback)
        .data(external.into())
        .build(scope)
        .unwrap();
    let rpc_key = v8::String::new(scope, "__oj_rpc").unwrap();
    global.set(scope, rpc_key.into(), rpc_fn.into());
}

/// The hooks a callback was built with. The `OpState` entry outlives every
/// JS invocation (it drops with the runtime), so the pointer is live.
fn hooks<'a>(args: &v8::FunctionCallbackArguments<'a>) -> &'a EngineHooks {
    let external = v8::Local::<v8::External>::try_from(args.data())
        .expect("bridge callbacks are built with their hooks as data");
    unsafe { &*(external.value() as *const EngineHooks) }
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
        // A dropped receiver means the Rust side abandoned the engine; the
        // push is then noise, not an error the JS side can act on.
        Ok(value) => {
            let _ = hooks(&args).post.send(value);
        }
        Err(e) => throw(scope, &format!("__oj_post takes one JSON string: {e}")),
    }
}

fn rpc_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let Some(handler) = hooks(&args).rpc.as_ref() else {
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
