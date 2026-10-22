// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Node's built-in modules in a browser bundle, as Vite stubs them
//! (vite:resolve's browser-external): an import of `node:crypto` or a bare
//! `crypto` that no installed package answers becomes a stub module.
//!
//! Every stub is CommonJS, matching Vite's: named ESM imports from a CJS
//! module interop into property reads, so a name the stub does not carry is
//! `undefined` at run time instead of a link-time MISSING_EXPORT (`import {
//! createHmac } from "crypto"` in code shared with the server). A build gets
//! Vite's bare `module.exports = {}`; dev gets the warning Proxy of Vite's
//! dependency optimizer stub rather than the throwing one, so a feature probe
//! (`typeof createHmac === "function"`) does not break the page. The Proxy
//! sits on the prototype (`Object.create`) because CJS-to-ESM interop copies
//! own properties and would flatten a bare Proxy to `{}`. A few modules get
//! working stand-ins the framework relies on in the browser.

use std::borrow::Cow;

use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, Plugin, PluginContext, PluginContextResolveOptions,
    SharedLoadPluginContext,
};

const PREFIX: &str = "\0oj-node-shim:";

/// Node's built-in module names without the `node:` scheme (its
/// `builtinModules`, subpaths included, with the legacy aliases a runtime
/// may not list).
const BUILTINS: &[&str] = &[
    "_http_agent",
    "_http_client",
    "_http_common",
    "_http_incoming",
    "_http_outgoing",
    "_http_server",
    "_stream_duplex",
    "_stream_passthrough",
    "_stream_readable",
    "_stream_transform",
    "_stream_wrap",
    "_stream_writable",
    "_tls_common",
    "_tls_wrap",
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

pub(crate) fn is_bare_builtin(spec: &str) -> bool {
    BUILTINS.contains(&spec)
}

const ALS: &str = "class AsyncLocalStorage{getStore(){return this._s}\
run(s,cb,...a){const p=this._s;this._s=s;try{return cb(...a)}finally{this._s=p}}\
enterWith(s){this._s=s}exit(cb,...a){const p=this._s;this._s=undefined;try{return cb(...a)}finally{this._s=p}}\
disable(){this._s=undefined}}module.exports={AsyncLocalStorage};";
const STREAM_WEB: &str = "module.exports={ReadableStream:globalThis.ReadableStream,WritableStream:globalThis.WritableStream,\
TransformStream:globalThis.TransformStream,ByteLengthQueuingStrategy:globalThis.ByteLengthQueuingStrategy,\
CountQueuingStrategy:globalThis.CountQueuingStrategy};";
const STREAM: &str = "class S{on(){return this}once(){return this}emit(){return false}pipe(t){return t}end(){}write(){return true}\
removeListener(){return this}destroy(){}}class Readable extends S{static from(){return new Readable()}}\
class Writable extends S{}class Duplex extends S{}class Transform extends S{}\
class PassThrough extends S{}class Stream extends S{}\
module.exports={Readable,Writable,Duplex,Transform,PassThrough,Stream};";
const PUNYCODE: &str = "const id=(s)=>s;\
module.exports={toUnicode:id,toASCII:id,encode:id,decode:id,ucs2:{decode:()=>[],encode:()=>\"\"}};";

/// The stub source for a builtin (`name` without `node:`).
pub(crate) fn shim_source(name: &str, production: bool) -> String {
    match name {
        "async_hooks" => return ALS.to_string(),
        "stream/web" => return STREAM_WEB.to_string(),
        "stream" => return STREAM.to_string(),
        "punycode" => return PUNYCODE.to_string(),
        _ => {}
    }
    if production {
        return "module.exports = {};".to_string();
    }
    // The name reaches the source only as a JSON string literal: a hostile
    // `node:` tail cannot break out of (or into) the code.
    format!(
        "const name = {};\n\
         module.exports = Object.create(new Proxy({{}}, {{\n  \
         get(_, key) {{\n    \
         if (key !== '__esModule' && key !== '__proto__' && key !== 'constructor' && key !== 'splice') {{\n      \
         console.warn(`Module \"${{name}}\" has been externalized for browser compatibility. Cannot access \"${{name}}.${{String(key)}}\" in client code.`);\n    \
         }}\n  \
         }}\n\
         }}));",
        serde_json::Value::String(name.to_string())
    )
}

#[derive(Debug)]
pub(crate) struct NodeBuiltinShims {
    pub production: bool,
}

impl Plugin for NodeBuiltinShims {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:node-builtin-shims")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId | rolldown_plugin::HookUsage::Load
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let spec = args.specifier;
        let name = match spec.strip_prefix("node:") {
            Some(name) => name,
            None if is_bare_builtin(spec) => {
                // Vite stubs a builtin only once node resolution fails, so an
                // installed package with a builtin's name (npm `events`,
                // `punycode`) wins over the stub. `node:` ids skip the probe:
                // npm cannot own that scheme.
                let found = ctx
                    .resolve(
                        spec,
                        args.importer,
                        Some(PluginContextResolveOptions {
                            import_kind: args.kind,
                            skip_self: true,
                            ..Default::default()
                        }),
                    )
                    .await?;
                if let Ok(found) = found {
                    return Ok(Some(HookResolveIdOutput::from_resolved_id(found)));
                }
                spec
            }
            None => return Ok(None),
        };
        // One module per builtin whatever the spelling: the async_hooks and
        // stream stand-ins are stateful.
        Ok(Some(HookResolveIdOutput::from_id(format!(
            "{PREFIX}{name}"
        ))))
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let Some(name) = args.id.strip_prefix(PREFIX) else {
            return Ok(None);
        };
        Ok(Some(HookLoadOutput {
            code: arcstr::ArcStr::from(shim_source(name, self.production)),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_are_commonjs_and_dev_warns() {
        assert_eq!(shim_source("crypto", true), "module.exports = {};");
        let dev = shim_source("crypto", false);
        assert!(dev.starts_with("const name = \"crypto\";"), "{dev}");
        assert!(
            dev.contains("module.exports = Object.create(new Proxy({}"),
            "{dev}"
        );
        assert!(
            dev.contains("has been externalized for browser compatibility"),
            "{dev}"
        );
        assert!(shim_source("async_hooks", true).contains("class AsyncLocalStorage"));
        assert!(shim_source("stream", false).contains("class Readable"));
        assert!(shim_source("stream/web", true).contains("globalThis.ReadableStream"));
    }

    #[test]
    fn a_hostile_name_stays_inside_its_string() {
        let dev = shim_source("x\";alert(1);//", false);
        assert!(
            dev.starts_with("const name = \"x\\\";alert(1);//\";"),
            "{dev}"
        );
    }

    #[test]
    fn bare_builtins_include_subpaths() {
        for name in [
            "fs",
            "fs/promises",
            "stream/web",
            "timers/promises",
            "path/posix",
        ] {
            assert!(is_bare_builtin(name), "{name}");
        }
        assert!(!is_bare_builtin("react"));
        assert!(!is_bare_builtin("node:fs"));
    }
}
