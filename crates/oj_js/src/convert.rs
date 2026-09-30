// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! JSON across the Rust/V8 boundary, through V8's own JSON parser rather than
//! serde_v8: with serde_json's "arbitrary_precision" feature enabled anywhere
//! in the build, serde_v8 serializes numbers as an internal struct, which V8
//! sees as an object.

use deno_core::v8;
use deno_runtime::worker::MainWorker;

use crate::EngineError;

pub(crate) fn js_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Js(e.to_string())
}

/// The result of a call's promise, as `call_with_args` settles it.
pub(crate) type SettledResult = Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>;

pub(crate) fn settled_to_json(
    worker: &mut MainWorker,
    result: SettledResult,
) -> Result<serde_json::Value, EngineError> {
    match result {
        Ok(global) => {
            deno_core::scope!(scope, &mut worker.js_runtime);
            let local = v8::Local::new(scope, global);
            Ok(v8_to_json(scope, local))
        }
        Err(e) => Err(EngineError::Js(e.to_string())),
    }
}

pub(crate) fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &serde_json::Value,
) -> Result<v8::Local<'s, v8::Value>, EngineError> {
    let text = serde_json::to_string(value).map_err(js_err)?;
    let text = v8::String::new(scope, &text)
        .ok_or_else(|| EngineError::Js("argument is too large for V8".into()))?;
    v8::json::parse(scope, text).ok_or_else(|| EngineError::Js("argument is not valid JSON".into()))
}

/// `null` when the value has no JSON representation (functions, undefined,
/// cycles).
pub(crate) fn v8_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> serde_json::Value {
    v8::json::stringify(scope, value)
        .map(|text| text.to_rust_string_lossy(scope))
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}
