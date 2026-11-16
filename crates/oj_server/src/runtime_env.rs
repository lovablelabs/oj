// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Runtime env: `import.meta.env` keys whose values the dev server hands the
//! page at request time instead of baking them into compiled modules.
//!
//! Compiles redirect each key to `globalThis.__oj_env.KEY` and import
//! `/@oj/env.js` first ([`oj_compiler::RuntimeDefines`]); that module is
//! rendered here from the values in force, so a rebind changes what one
//! route serves and nothing a compile emitted. The key list
//! (`OJ_RUNTIME_ENV_KEYS`) is fixed at startup; a key's value comes from
//! `POST /@oj/env`, a local-only control endpoint, and wins over the `.env`
//! literal the key had.

use super::*;
use std::borrow::Cow;
use std::collections::BTreeMap;

/// The global the served module defines and compiles read through.
pub(crate) const GLOBAL: &str = "__oj_env";
/// The served module, under the base.
pub(crate) const MODULE_PATH: &str = "/@oj/env.js";
/// The control endpoint (`GET` status, `POST` values), under the base.
pub(crate) const CONTROL_PATH: &str = "/@oj/env";
/// The set values together may not pass this size: they are a handful of
/// strings.
const MAX_SET_BYTES: u64 = 64 * 1024;

/// What `oj dev` was told about runtime keys (`OJ_RUNTIME_ENV_KEYS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnvConfig {
    pub keys: Vec<String>,
}

impl RuntimeEnvConfig {
    /// From the process knobs; `None` when no keys are named.
    pub fn from_knobs(knobs: &oj_env::Knobs) -> Result<Option<Self>, String> {
        let keys = match &knobs.runtime_env_keys {
            Some(spec) => oj_env::parse_runtime_env_keys(spec)?,
            None => Vec::new(),
        };
        if keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(RuntimeEnvConfig { keys }))
    }
}

/// Where the values in force stand against the key list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// Every key has a value set.
    Bound,
    /// Some keys set, the rest on their literals.
    Partial,
    /// No key set: every key serves its literal.
    Unbound,
}

impl Binding {
    fn name(self) -> &'static str {
        match self {
            Binding::Bound => "bound",
            Binding::Partial => "partial",
            Binding::Unbound => "unbound",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Set,
    Fallback,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Set => "set",
            Source::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    set_values: BTreeMap<String, String>,
    /// Bumped whenever the values in force change.
    generation: u64,
}

pub(crate) struct RuntimeEnv {
    keys: Vec<String>,
    /// `import.meta.env.*` defines outside the runtime keys, name and the
    /// JavaScript expression the define replaces reads with.
    statics: Vec<(String, String)>,
    /// The expression a runtime key's define would have baked in, when the
    /// env had one: what the module serves while the key has no value.
    fallbacks: BTreeMap<String, String>,
    module_url: String,
    /// Tells this server's generations from another's: they count from zero
    /// in every image, and a tab outlives a restart.
    boot: String,
    inner: Mutex<Inner>,
}

impl RuntimeEnv {
    /// `defines`: the client define list before redirection.
    pub(crate) fn new(
        cfg: RuntimeEnvConfig,
        defines: &[(String, String)],
        base: Option<&str>,
    ) -> Self {
        let is_runtime = |name: &str| cfg.keys.iter().any(|k| k == name);
        let mut statics = Vec::new();
        let mut fallbacks = BTreeMap::new();
        for (k, v) in defines {
            let Some(name) = k.strip_prefix("import.meta.env.") else {
                continue;
            };
            if is_runtime(name) {
                fallbacks.insert(name.to_string(), v.clone());
            } else {
                statics.push((name.to_string(), v.clone()));
            }
        }
        RuntimeEnv {
            keys: cfg.keys,
            statics,
            fallbacks,
            module_url: dev_html::posix_join(&[base.unwrap_or("/"), MODULE_PATH]),
            boot: boot_id(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The startup line: the keys and where their values come from.
    pub(crate) fn banner(&self) -> String {
        format!(
            "runtime env: {}, values from POST {CONTROL_PATH}",
            self.keys.join(", ")
        )
    }

    pub(crate) fn compiler_defines(&self) -> oj_compiler::RuntimeDefines {
        oj_compiler::RuntimeDefines::new(
            self.keys.iter().map(String::as_str),
            GLOBAL,
            self.module_url.as_str(),
        )
    }

    /// Salts compile cache keys: a compile embeds which keys are redirected.
    pub(crate) fn cache_salt(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for key in &self.keys {
            hasher.update(key.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(self.module_url.as_bytes());
        hasher.finalize().to_hex()[..16].to_string()
    }

    /// Applies `updates` (a `None` unsets): what `POST /@oj/env` carries.
    /// Refuses a key outside the list. True when the pages must reload.
    pub(crate) fn set(&self, updates: BTreeMap<String, Option<String>>) -> Result<bool, String> {
        let unknown: Vec<&str> = updates
            .keys()
            .filter(|k| !self.keys.contains(k))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown runtime env key(s): {}; the keys are {}",
                unknown.join(", "),
                self.keys.join(", ")
            ));
        }
        let mut inner = self.inner.lock().unwrap();
        let mut set_values = inner.set_values.clone();
        for (key, value) in updates {
            match value {
                Some(value) => {
                    set_values.insert(key, value);
                }
                None => {
                    set_values.remove(&key);
                }
            }
        }
        // A limit for the set values together: a restart hands them to the
        // next image as one environment variable, and one too long for the
        // system fails the exec, which ended the server.
        let handed = serde_json::json!(set_values).to_string();
        if handed.len() as u64 > MAX_SET_BYTES {
            return Err(format!(
                "the set values together are larger than {MAX_SET_BYTES} bytes"
            ));
        }
        *carried().lock().unwrap() = (!set_values.is_empty()).then_some(handed);
        // The generation moves when the values in force do.
        let changed = inner.set_values != set_values;
        inner.set_values = set_values;
        if changed {
            inner.generation += 1;
        }
        Ok(changed)
    }

    /// Takes the set values a restarted server handed this one
    /// ([`carried_set`]): the ones for keys still listed, anything else of
    /// it ignored. Nothing is announced: no page is connected yet.
    pub(crate) fn restore_set(&self, carried: &str) {
        let Ok(updates) = parse_updates(carried.as_bytes()) else {
            return;
        };
        let listed = updates
            .into_iter()
            .filter(|(key, _)| self.keys.contains(key))
            .collect();
        let _ = self.set(listed);
    }

    pub(crate) fn state(&self) -> Binding {
        state_of(&self.inner.lock().unwrap(), &self.keys)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// The values set, by key.
    pub(crate) fn values(&self) -> BTreeMap<String, String> {
        self.inner.lock().unwrap().set_values.clone()
    }

    /// The `%KEY%` map for a page: `base` (the literals) with the values set
    /// over it.
    pub(crate) fn html_env<'a>(
        &self,
        base: &'a BTreeMap<String, String>,
    ) -> Cow<'a, BTreeMap<String, String>> {
        let values = self.values();
        if values.is_empty() {
            return Cow::Borrowed(base);
        }
        let mut env = base.clone();
        env.extend(values);
        Cow::Owned(env)
    }

    /// The `GET /@oj/env` body.
    pub(crate) fn status(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let keys: Vec<serde_json::Value> = self
            .keys
            .iter()
            .map(|key| {
                let (value, source) = value_of(&inner, &self.fallbacks, key);
                // A fallback is the expression the define held; the `.env`
                // literal it came from reads back as the value itself.
                let value = match source {
                    Some(Source::Fallback) => {
                        value.map(|expr| serde_json::from_str::<String>(&expr).unwrap_or(expr))
                    }
                    _ => value,
                };
                serde_json::json!({
                    "key": key,
                    "value": value,
                    "source": source.map(Source::name),
                })
            })
            .collect();
        serde_json::json!({
            "state": state_of(&inner, &self.keys).name(),
            "generation": inner.generation,
            "module": self.module_url,
            "keys": keys,
        })
    }

    /// The `/@oj/env.js` body: one object of every `import.meta.env` field
    /// (the literals as the define would have read, the runtime keys as
    /// their values in force, else their literal, else left out so the read
    /// is `undefined`), installed as the global every redirected compile
    /// reads. A plain object, as Vite's per-module `import.meta.env` is: an
    /// assignment to a field (a test shim) stays a no-op on the next load
    /// instead of throwing in strict module code.
    pub(crate) fn module_js(&self) -> String {
        self.render(Form::Module)
    }

    /// The `/@oj/env.js` body for a classic worker, which `importScripts`
    /// it: the object installed as the global, with no export and no name
    /// left in the worker's scope.
    pub(crate) fn classic_js(&self) -> String {
        self.render(Form::Script)
    }

    fn render(&self, form: Form) -> String {
        let inner = self.inner.lock().unwrap();
        let mut fields: Vec<String> = self
            .statics
            .iter()
            .map(|(name, expr)| format!("  {}: {expr}", json_str(name)))
            .collect();
        for key in &self.keys {
            match value_of(&inner, &self.fallbacks, key) {
                (Some(value), Some(Source::Fallback)) => {
                    // The literal is an expression as the define held it.
                    fields.push(format!("  {}: {value}", json_str(key)));
                }
                (Some(value), _) => {
                    fields.push(format!("  {}: {}", json_str(key), json_str(&value)));
                }
                (None, _) => {}
            }
        }
        // A page that loaded these values just before they changed may have
        // had no socket open for the reload: once loaded, it asks for the
        // generation in force and reloads when it is another. Only a number
        // counts, so an answer that is not the server's reloads nothing, and
        // a generation is reloaded for once in a tab: a copy of this module
        // served from a cache (an app's service worker) would otherwise
        // reload the page for as long as it is stale. Once per generation of
        // this server: the next image counts from zero again, and its
        // generation 2 is not the one a tab reloaded for before the restart.
        let check = format!(
            "if (typeof window !== \"undefined\" && typeof fetch === \"function\") {{\n  const check = () => fetch({url}, {{ cache: \"no-store\" }}).then((r) => (r.ok ? r.text() : \"\")).then((g) => {{\n    if (!/^\\d+$/.test(g) || g === {generation}) return;\n    const seen = {boot} + g;\n    try {{ if (sessionStorage.getItem({guard}) === seen) return; sessionStorage.setItem({guard}, seen); }} catch {{}}\n    location.reload();\n  }}, () => {{}});\n  if (document.readyState === \"complete\") check(); else window.addEventListener(\"load\", check, {{ once: true }});\n}}\n",
            url = json_str(&format!("{}?{GENERATION_QUERY}", self.module_url)),
            generation = json_str(&inner.generation.to_string()),
            guard = json_str("__oj_env_reloaded"),
            boot = json_str(&format!("{}:", self.boot)),
        );
        let install = format!(
            "Object.defineProperty(globalThis, {global}, {{ value: env, configurable: true, enumerable: false, writable: false }});\n",
            global = json_str(GLOBAL),
        );
        let object = format!("const env = {{\n{}\n}};\n", fields.join(",\n"));
        match form {
            Form::Module => format!("{object}{install}{check}export default env;\n"),
            // In a block: a script's top-level `const` is the worker's
            // global, where the worker's own `env` would be a redeclaration.
            Form::Script => format!("{{\n{object}{install}}}\n"),
        }
    }
}

/// How the values are served: the module a compile imports, or the script a
/// classic worker loads.
#[derive(Clone, Copy)]
enum Form {
    Module,
    Script,
}

/// The set values of this process's server, as the JSON object a restart
/// hands the next image.
fn carried() -> &'static Mutex<Option<String>> {
    static SET: Mutex<Option<String>> = Mutex::new(None);
    &SET
}

/// What a restart passes on as `OJ_RUNTIME_ENV_SET`: the values set over
/// `POST /@oj/env`, which live nowhere else. Without it a config or `.env`
/// edit, which restarts the server, silently put every key back on its
/// literal.
pub(crate) fn carried_set() -> Option<String> {
    carried().lock().unwrap().clone()
}

/// A name no other server image has: the process and when it was asked.
fn boot_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}-{nanos:x}", std::process::id())
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// A key's value in force and where it came from; the literal when it has
/// none; nothing when the env never had it either.
fn value_of(
    inner: &Inner,
    fallbacks: &BTreeMap<String, String>,
    key: &str,
) -> (Option<String>, Option<Source>) {
    if let Some(v) = inner.set_values.get(key) {
        return (Some(v.clone()), Some(Source::Set));
    }
    match fallbacks.get(key) {
        Some(v) => (Some(v.clone()), Some(Source::Fallback)),
        None => (None, None),
    }
}

fn state_of(inner: &Inner, keys: &[String]) -> Binding {
    // Only listed keys are ever set.
    match inner.set_values.len() {
        0 => Binding::Unbound,
        n if n == keys.len() => Binding::Bound,
        _ => Binding::Partial,
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Only a direct local caller may read or set the values: a request a
/// browser or the preview proxy sends carries `Origin` or `X-Forwarded-Host`.
/// `/@oj/typecheck` and `/@oj/diagnostics` apply the same rule.
pub(crate) fn local_caller(headers: &HeaderMap) -> bool {
    !headers.contains_key(header::ORIGIN) && !headers.contains_key("x-forwarded-host")
}

/// Who may read or set the runtime values: a [`local_caller`] that is not a
/// browser's request (a page served from a trustworthy origin, localhost
/// among them, sends `Sec-Fetch-Site` with every request, its own
/// same-origin `fetch` included, which has no `Origin`; a page on plain
/// http under another name sends none, and its POST still carries `Origin`,
/// so it may at most read the status, which holds what `/@oj/env.js` hands
/// any page) and whose connection comes from this machine. The header rule alone let any
/// machine that reaches the port (`--host 0.0.0.0`) set the values with a
/// plain `curl`; the other endpoints under it only read.
pub(crate) fn control_caller(headers: &HeaderMap, peer: Option<std::net::IpAddr>) -> bool {
    // What a proxy on this machine adds for the caller it relays: its
    // connection is local, the caller's is not.
    const RELAYED: [&str; 5] = [
        "sec-fetch-site",
        "forwarded",
        "x-forwarded-for",
        "x-real-ip",
        "via",
    ];
    local_caller(headers)
        && !RELAYED.iter().any(|name| headers.contains_key(*name))
        // None: a test driving the router directly, with no connection.
        && peer.is_none_or(this_machine)
}

/// Whether `ip` is an address of this machine: loopback, or one of its own
/// interfaces' (a server bound to its LAN address is reached there by
/// a command on this machine, whose connection then comes from that address). Only an
/// address of this machine can be bound.
fn this_machine(ip: std::net::IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || (!ip.is_unspecified() && std::net::UdpSocket::bind((ip, 0)).is_ok())
}

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// `GET /@oj/env.js`; with `?classic`, the script a classic worker loads.
pub(crate) async fn serve_env_module(uri: Uri, State(state): State<Arc<ServerState>>) -> Response {
    let Some(env) = &state.runtime_env else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let query = |name: &str| {
        uri.query()
            .is_some_and(|q| q.split('&').any(|kv| kv == name))
    };
    if query(GENERATION_QUERY) {
        return (
            [
                (header::CONTENT_TYPE, "text/plain"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            env.generation().to_string(),
        )
            .into_response();
    }
    let classic = query(CLASSIC_QUERY);
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-store"),
            // The classic form is a script any page could name in a
            // `<script src>` and then read the global it installs: only the
            // server's own pages and workers load either form.
            (CROSS_ORIGIN_RESOURCE_POLICY, "same-origin"),
        ],
        if classic {
            env.classic_js()
        } else {
            env.module_js()
        },
    )
        .into_response()
}

const CROSS_ORIGIN_RESOURCE_POLICY: header::HeaderName =
    header::HeaderName::from_static("cross-origin-resource-policy");

/// The query asking for the generation of the values in force alone: what
/// a page compares the one it loaded with.
pub(crate) const GENERATION_QUERY: &str = "generation";

/// The query naming the classic-worker form of the module.
pub(crate) const CLASSIC_QUERY: &str = "classic";

/// A compiled worker file's body for its worker type. A compile imports the
/// module with a static `import`, which only a module worker may hold: a
/// classic worker loads the script with `importScripts`, and one of unknown
/// type (`new Worker(url, options)` with options oj cannot read) gets the
/// script itself, on the import's line, which either kind of worker runs,
/// as vite:worker inlines its env for it.
pub(crate) fn worker_body(env: &RuntimeEnv, url: &str, body: String) -> String {
    let worker_type = url.split_once('?').and_then(|(_, query)| {
        let mut kvs = query.split('&');
        kvs.any(|kv| kv == "worker_file")
            .then(|| query.split('&').find_map(|kv| kv.strip_prefix("type=")))
            .flatten()
    });
    let script = json_str(&format!("{}?{CLASSIC_QUERY}", env.module_url));
    match worker_type {
        // Same line count, so the map stands.
        Some("classic") => body.replacen(&env.import(), &format!("importScripts({script});"), 1),
        Some("ignore") => env.inlined(body),
        _ => body,
    }
}

impl RuntimeEnv {
    /// The statement a compile puts first in a module that reads the global.
    fn import(&self) -> String {
        format!("import {};", json_str(&self.module_url))
    }

    /// Whether compiled `code` imports the module.
    pub(crate) fn imported_by(&self, code: &str) -> bool {
        code.contains(&self.import())
    }

    /// `body` with the script itself where its import of the module stood,
    /// which a classic script and a module both run. One line, so the map
    /// stands: the values are JSON, which holds no line break outside the
    /// ones the rendering puts between fields.
    pub(crate) fn inlined(&self, body: String) -> String {
        let script = self.classic_js().replace('\n', " ").trim_end().to_string();
        body.replacen(&self.import(), &script, 1)
    }
}

/// Whether a request for a compiled file loads it as a classic script, or
/// may: a `<script src>` without `type="module"` (a module script is
/// fetched in `cors` mode, a classic one in `no-cors`), or a service
/// worker's script, whose type the request does not say. Neither may hold
/// the static import a compile puts first, so each gets the values inline
/// ([`RuntimeEnv::inlined`]), where the read was a literal before.
pub(crate) fn classic_script_request(headers: &HeaderMap) -> bool {
    let is = |name: &str, value: &str| {
        headers
            .get(name)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(value.as_bytes()))
    };
    is("sec-fetch-dest", "serviceworker")
        || (is("sec-fetch-dest", "script") && is("sec-fetch-mode", "no-cors"))
}

/// `GET /@oj/env` (the status) and `POST /@oj/env` (a JSON object of
/// values, `null` unsetting one), for local callers only.
pub(crate) async fn serve_env_control(
    method: axum::http::Method,
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Response {
    let Some(env) = &state.runtime_env else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let peer = crate::tls::client_conn().map(|conn| conn.addr.ip());
    if !control_caller(&headers, peer) {
        return (StatusCode::FORBIDDEN, no_store(), "").into_response();
    }
    if method == axum::http::Method::POST {
        let updates = match parse_updates(&body) {
            Ok(updates) => updates,
            Err(e) => return (StatusCode::BAD_REQUEST, no_store(), e).into_response(),
        };
        match env.set(updates) {
            Ok(changed) => {
                if changed {
                    announce(&state, env);
                }
            }
            Err(e) => return (StatusCode::BAD_REQUEST, no_store(), e).into_response(),
        }
    } else if method != axum::http::Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        env.status().to_string(),
    )
        .into_response()
}

fn parse_updates(body: &[u8]) -> Result<BTreeMap<String, Option<String>>, String> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let Some(object) = json.as_object() else {
        return Err("body must be a JSON object of key to string (null unsets)".into());
    };
    object
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => Ok((k.clone(), Some(s.clone()))),
            serde_json::Value::Null => Ok((k.clone(), None)),
            other => Err(format!(
                "value for {k} must be a string or null, not {}",
                kind_of(other)
            )),
        })
        .collect()
}

/// The values in force changed: every connected page reloads onto them (a
/// redirected module reads the global the fresh `/@oj/env.js` defines).
pub(crate) fn announce(state: &ServerState, env: &RuntimeEnv) {
    println!(
        "oj: runtime env set -> {} (generation {}) -> full-reload",
        env.state().name(),
        env.generation()
    );
    // Only the pages connected: one loading now compares the generation it
    // was handed with the one in force once it has loaded (the module's
    // check), so no frame is kept for the next socket. A frame kept reloaded
    // the next page to connect though it had loaded the values in force.
    let _ = state
        .reload_tx
        .send(hmr::full_reload_frame("runtime env", None, None));
}

impl ServerState {
    /// The `%KEY%` map for the page being served: the literals, with the
    /// runtime values in force over them.
    pub(crate) fn html_env_now(&self) -> Cow<'_, BTreeMap<String, String>> {
        match &self.runtime_env {
            Some(env) => env.html_env(&self.html_env),
            None => Cow::Borrowed(&self.html_env),
        }
    }
}

#[cfg(test)]
#[path = "runtime_env_tests.rs"]
mod tests;
