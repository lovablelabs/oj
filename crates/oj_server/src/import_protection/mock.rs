// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The virtual modules of Start's import protection a dev server under
//! `behavior: "error"` serves: the marker modules, and the mocks a
//! self-denied file is replaced with. The module text is Start's
//! (`import-protection/virtualModules.ts`).

use base64::Engine;

use super::{MockAccess, Side, MARKER_PREFIX};

const MOCK_MODULE_ID: &str = "tanstack-start-import-protection:mock";
const MOCK_RUNTIME_PREFIX: &str = "tanstack-start-import-protection:mock-runtime:";
/// Vite's browser-visible spelling of a `\0` id.
const BROWSER_VIRTUAL_PREFIX: &str = "/@id/__x00__";

const RUNTIME_SUGGESTION: &str = "Fix: Use createServerFn().handler(() => ...) to keep the logic on the server and call it from the client via an RPC bridge. Use createServerOnlyFn(() => ...) to mark it as server-only (it will throw if accidentally called from the client). Use createIsomorphicFn().client(() => ...).server(() => ...) to provide separate client and server implementations. Move the server-only import out of this file into a separate .server.ts module that is not imported by any client code. To disable these runtime diagnostics, set importProtection.mockAccess: \"off\".";

fn json(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

/// The resolved id of one of import protection's own modules, for `source`
/// as an import wrote it. Start's `resolveInternalVirtualModuleId`.
pub fn resolve_virtual(source: &str) -> Option<String> {
    let source = match source.strip_prefix(BROWSER_VIRTUAL_PREFIX) {
        Some(rest) => format!("\0{rest}"),
        None => source.to_string(),
    };
    let bare = source.strip_prefix('\0').unwrap_or(&source);
    let ours = bare == MOCK_MODULE_ID
        || bare.starts_with(MOCK_RUNTIME_PREFIX)
        || bare.starts_with(MARKER_PREFIX);
    ours.then(|| format!("\0{bare}"))
}

/// The code of a module [`resolve_virtual`] named.
pub fn load_virtual(id: &str) -> Option<String> {
    let bare = id.strip_prefix('\0')?;
    if bare == MOCK_MODULE_ID {
        return Some(mock_code(None));
    }
    if let Some(payload) = bare.strip_prefix(MOCK_RUNTIME_PREFIX) {
        return Some(runtime_module(payload));
    }
    bare.starts_with(MARKER_PREFIX)
        .then(|| "export {}".to_string())
}

/// The module a self-denied file imports its mock from: the one that
/// reports each access in the browser, or the silent one on the server and
/// under `mockAccess: "off"`. Start's `mockRuntimeModuleIdFromViolation`.
pub(super) fn runtime_id(side: Side, file: &str, specifier: &str, access: MockAccess) -> String {
    let mode = match access {
        MockAccess::Off => return MOCK_MODULE_ID.to_string(),
        MockAccess::Error => "error",
        MockAccess::Warn => "warn",
    };
    if side != Side::Client {
        return MOCK_MODULE_ID.to_string();
    }
    let payload = format!(
        "{{\"env\":\"client\",\"importer\":{},\"specifier\":{},\"trace\":[],\"mode\":\"{mode}\"}}",
        json(file),
        json(specifier),
    );
    format!(
        "{MOCK_RUNTIME_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )
}

fn runtime_module(payload: &str) -> String {
    let payload: serde_json::Value = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mode = match text("mode").as_str() {
        "warn" => "warn",
        "off" => "off",
        _ => "error",
    };
    let trace = payload
        .get("trace")
        .filter(|t| t.is_array())
        .map_or_else(|| "[]".to_string(), |t| t.to_string());
    let meta = format!(
        "{{\"env\":{},\"importer\":{},\"specifier\":{},\"trace\":{trace}}}",
        json(&text("env")),
        json(&text("importer")),
        json(&text("specifier")),
    );
    mock_code(Some((&meta, mode)))
}

/// Start's `isValidExportName`.
fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$');
    first && name != "default" && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Start's `generateDevSelfDenialModule`.
pub(super) fn self_denial_module(names: &[String], runtime_id: &str) -> String {
    let mut lines = Vec::new();
    let mut aliased = Vec::new();
    let names = names
        .iter()
        .filter(|n| !n.is_empty() && n.as_str() != "default");
    for (i, name) in names.enumerate() {
        if is_plain_name(name) {
            lines.push(format!("export const {name} = mock.{name};"));
        } else {
            lines.push(format!("const __tss_str_{i} = mock[{}];", json(name)));
            aliased.push(format!("__tss_str_{i} as {}", json(name)));
        }
    }
    if !aliased.is_empty() {
        lines.push(format!("export {{ {} }};", aliased.join(", ")));
    }
    format!(
        "import mock from {};\n{}\nexport default mock;\n",
        json(runtime_id),
        lines.join("\n")
    )
}

/// Start's `generateMockCode`: a proxy any access, call or construction of
/// which gives another one. With `diagnostics` (the meta object and the
/// mode), each first use is reported to the console.
fn mock_code(diagnostics: Option<(&str, &str)>) -> String {
    let name = if diagnostics.is_some() {
        "__createMock"
    } else {
        "createMock"
    };
    let report = |call: &'static str| if diagnostics.is_some() { call } else { "" };
    let preamble = diagnostics.map_or_else(String::new, |(meta, mode)| {
        format!(
            r#"const __meta = {meta};
const __mode = {mode};

const __seen = new Set();
function __report(action, accessPath) {{
  if (__mode === 'off') return;
  const key = action + ':' + accessPath;
  if (__seen.has(key)) return;
  __seen.add(key);

  const traceLines = Array.isArray(__meta.trace) && __meta.trace.length
    ? "\n\nTrace:\n" + __meta.trace.map((t, i) => '  ' + (i + 1) + '. ' + String(t)).join('\n')
    : '';

  const msg =
    '[import-protection] Mocked import used in dev client\n\n' +
    'Denied import: "' + __meta.specifier + '"\n' +
    'Importer: ' + __meta.importer + '\n' +
    'Access: ' + accessPath + ' (' + action + ')' +
    traceLines +
    '\n\n' + {suggestion};

  const err = new Error(msg);
  if (__mode === 'warn') {{
    console.warn(err);
  }} else {{
    console.error(err);
  }}
}}
"#,
            mode = json(mode),
            suggestion = json(RUNTIME_SUGGESTION),
        )
    });
    let set_trap = report(
        r#"
    set(_target, prop) {
      __report('set', name + '.' + String(prop));
      return true;
    },"#,
    );
    format!(
        r#"
{preamble}/* @__NO_SIDE_EFFECTS__ */
function {name}(name) {{
  const fn = function () {{}};
  fn.prototype.name = name;
  const children = Object.create(null);
  const proxy = new Proxy(fn, {{
    get(_target, prop) {{
      if (prop === '__esModule') return true;
      if (prop === 'default') return proxy;
      if (prop === 'caller') return null;
      if (prop === 'then') return (f) => Promise.resolve(f(proxy));
      if (prop === 'catch') return () => Promise.resolve(proxy);
      if (prop === 'finally') return (f) => {{ f(); return Promise.resolve(proxy); }};
      if (prop === Symbol.toPrimitive) {{
        return () => {{
          {to_primitive}
          return '[import-protection mock]';
        }};
      }}
      if (prop === 'toString' || prop === 'valueOf' || prop === 'toJSON') {{
        return () => {{
          {to_string}
          return '[import-protection mock]';
        }};
      }}
      if (typeof prop === 'symbol') return undefined;
      if (!(prop in children)) {{
        children[prop] = {name}(name + '.' + prop);
      }}
      return children[prop];
    }},
    apply() {{
      {call}return {name}(name + '()');
    }},
    construct() {{
      {construct}return {name}('new ' + name);
    }},{set_trap}
  }});
  return proxy;
}}
const mock = /* @__PURE__ */ {name}('mock');
export default mock;
"#,
        to_primitive = report("__report('toPrimitive', name);"),
        to_string = report("__report(String(prop), name);"),
        call = report("__report('call', name + '()');\n      "),
        construct = report("__report('construct', 'new ' + name);\n      "),
    )
}
