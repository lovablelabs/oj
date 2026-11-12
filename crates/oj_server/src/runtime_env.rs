// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Runtime env: `import.meta.env` keys whose values the dev server hands the
//! page at request time instead of baking them into compiled modules.
//!
//! Compiles redirect each key to `globalThis.__oj_env.KEY` and import
//! `/@oj/env.js` first ([`oj_compiler::RuntimeDefines`]); that module is
//! rendered here from the values in force, so a rebind changes what one
//! route serves and nothing a compile emitted. Values come from a watched
//! JSON file (`OJ_RUNTIME_ENV_FILE`) and from `POST /@oj/env`, a local-only
//! control endpoint, a set value winning over the file's and the file's over
//! the `.env` literal the key had. While the file is on disk but incomplete,
//! documents answer 503 so a page never boots against half a backend; the
//! modules behind them still serve, so the reload after binding is warm.
//! `OJ_RUNTIME_ENV_READY_FILE`, when named, exists exactly while every key
//! has a value.

use super::*;
use oj_env::RuntimeKey;
use std::borrow::Cow;
use std::collections::BTreeMap;

/// The global the served module defines and compiles read through.
pub(crate) const GLOBAL: &str = "__oj_env";
/// The served module, under the base.
pub(crate) const MODULE_PATH: &str = "/@oj/env.js";
/// The control endpoint (`GET` status, `POST` values), under the base.
pub(crate) const CONTROL_PATH: &str = "/@oj/env";
/// A values file past this size is refused: it holds a handful of strings.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// What `oj dev` was told about runtime keys (`OJ_RUNTIME_ENV_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnvConfig {
    pub keys: Vec<RuntimeKey>,
    /// The JSON file the keys' values come from, watched while the server runs.
    pub file: Option<PathBuf>,
    /// Exists exactly while every key has a value.
    pub ready_file: Option<PathBuf>,
}

impl RuntimeEnvConfig {
    /// From the process knobs; `None` when no keys are named. A file or ready
    /// file without keys is an error, since it could never apply.
    pub fn from_knobs(knobs: &oj_env::Knobs) -> Result<Option<Self>, String> {
        let keys = match &knobs.runtime_env_keys {
            Some(spec) => oj_env::parse_runtime_env_keys(spec)?,
            None => Vec::new(),
        };
        if keys.is_empty() {
            if knobs.runtime_env_file.is_some() || knobs.runtime_env_ready_file.is_some() {
                return Err(
                    "OJ_RUNTIME_ENV_FILE / OJ_RUNTIME_ENV_READY_FILE need OJ_RUNTIME_ENV_KEYS to name the keys they serve".into(),
                );
            }
            return Ok(None);
        }
        Ok(Some(RuntimeEnvConfig {
            keys,
            file: knobs.runtime_env_file.clone(),
            ready_file: knobs.runtime_env_ready_file.clone(),
        }))
    }
}

/// What the values file holds, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileState {
    /// No file configured, or none on disk.
    Absent,
    /// On disk, but these keys have no value in it (the `{"pending":true}`
    /// a backend publishes before its credentials exist, or a partial write).
    Pending { missing: Vec<String> },
    /// Every key has a value.
    Complete,
    /// Not a readable JSON object of strings; nothing of it is used.
    Invalid(String),
}

impl FileState {
    fn name(&self) -> &'static str {
        match self {
            FileState::Absent => "absent",
            FileState::Pending { .. } => "pending",
            FileState::Complete => "complete",
            FileState::Invalid(_) => "invalid",
        }
    }
}

/// Where the values in force stand against the key list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// Every key has a runtime value.
    Bound,
    /// The file is on disk but the keys are not all covered: documents wait.
    Pending,
    /// No file on disk; some keys set, the rest on their literals.
    Partial,
    /// No runtime value anywhere: every key serves its literal.
    Unbound,
}

impl Binding {
    fn name(self) -> &'static str {
        match self {
            Binding::Bound => "bound",
            Binding::Pending => "pending",
            Binding::Partial => "partial",
            Binding::Unbound => "unbound",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Set,
    File,
    Fallback,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Set => "set",
            Source::File => "file",
            Source::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    file_values: BTreeMap<String, String>,
    file_state: Option<FileState>,
    set_values: BTreeMap<String, String>,
    /// Bumped whenever the values in force change.
    generation: u64,
}

pub(crate) struct RuntimeEnv {
    keys: Vec<RuntimeKey>,
    file: Option<PathBuf>,
    /// `file` with its directory canonicalized, as the watcher reports paths.
    file_canonical: Option<PathBuf>,
    ready_file: Option<PathBuf>,
    /// `import.meta.env.*` defines outside the runtime keys, name and the
    /// JavaScript expression the define replaces reads with.
    statics: Vec<(String, String)>,
    /// The expression a runtime key's define would have baked in, when the
    /// env had one: what the module serves while the key has no value.
    fallbacks: BTreeMap<String, String>,
    module_url: String,
    inner: Mutex<Inner>,
}

impl RuntimeEnv {
    /// `defines`: the client define list before redirection. Reads the file
    /// once, so the first request already sees it.
    pub(crate) fn new(
        cfg: RuntimeEnvConfig,
        defines: &[(String, String)],
        base: Option<&str>,
    ) -> Self {
        let is_runtime = |name: &str| cfg.keys.iter().any(|k| k.key == name);
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
        // Absolute, so the watcher's absolute event paths match the file and
        // a later chdir (a plugin host moves to the app root) changes nothing.
        let file = cfg.file.as_deref().map(absolute);
        let file_canonical = file.as_ref().map(|f| canonical_parent(f));
        let env = RuntimeEnv {
            keys: cfg.keys,
            file,
            file_canonical,
            ready_file: cfg.ready_file.as_deref().map(absolute),
            statics,
            fallbacks,
            module_url: dev_html::posix_join(&[base.unwrap_or("/"), MODULE_PATH]),
            inner: Mutex::new(Inner::default()),
        };
        env.load_file();
        // Without a file nothing above ran: a ready file a previous run left
        // must still go, since nothing is bound yet.
        env.publish_ready(env.state());
        env
    }

    /// The startup line: the keys and where their values come from.
    pub(crate) fn banner(&self) -> String {
        let keys: Vec<&str> = self.keys.iter().map(|k| k.key.as_str()).collect();
        let source = match &self.file {
            Some(file) => format!(
                "file {} ({})",
                file.display(),
                describe(
                    &self
                        .inner
                        .lock()
                        .unwrap()
                        .file_state
                        .clone()
                        .unwrap_or(FileState::Absent)
                )
            ),
            None => "POST /@oj/env only".to_string(),
        };
        format!("runtime env: {} from {source}", keys.join(", "))
    }

    pub(crate) fn compiler_defines(&self) -> oj_compiler::RuntimeDefines {
        oj_compiler::RuntimeDefines::new(
            self.keys.iter().map(|k| k.key.as_str()),
            GLOBAL,
            self.module_url.as_str(),
        )
    }

    pub(crate) fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Whether a watcher path is the values file.
    pub(crate) fn is_file(&self, path: &Path) -> bool {
        self.file.as_deref() == Some(path) || self.file_canonical.as_deref() == Some(path)
    }

    /// Salts compile cache keys: a compile embeds which keys are redirected.
    pub(crate) fn cache_salt(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for key in &self.keys {
            hasher.update(key.key.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(self.module_url.as_bytes());
        hasher.finalize().to_hex()[..16].to_string()
    }

    /// Re-reads the values file. True when the values in force changed.
    pub(crate) fn load_file(&self) -> bool {
        let Some(file) = &self.file else {
            return false;
        };
        let (values, state) = read_file(file, &self.keys);
        let mut inner = self.inner.lock().unwrap();
        let before = effective(&inner, &self.keys);
        if inner.file_state.is_some() && inner.file_state.as_ref() != Some(&state) {
            // An invalid or incomplete file holds every document: say so,
            // since the values map it leaves empty shows nothing.
            eprintln!(
                "oj: runtime env file {}: {}",
                file.display(),
                describe(&state)
            );
        }
        inner.file_values = values;
        inner.file_state = Some(state);
        self.settle(&mut inner, before)
    }

    /// Applies `updates` (a `None` unsets): what `POST /@oj/env` carries.
    /// Refuses a key outside the list. True when the values in force changed.
    pub(crate) fn set(&self, updates: BTreeMap<String, Option<String>>) -> Result<bool, String> {
        let unknown: Vec<&str> = updates
            .keys()
            .filter(|k| !self.keys.iter().any(|key| key.key == **k))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown runtime env key(s): {}; the keys are {}",
                unknown.join(", "),
                self.keys
                    .iter()
                    .map(|k| k.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut inner = self.inner.lock().unwrap();
        let before = effective(&inner, &self.keys);
        for (key, value) in updates {
            match value {
                Some(value) => {
                    inner.set_values.insert(key, value);
                }
                None => {
                    inner.set_values.remove(&key);
                }
            }
        }
        Ok(self.settle(&mut inner, before))
    }

    /// After a change to either layer: bumps the generation when the values
    /// in force moved and brings the ready file in line.
    fn settle(&self, inner: &mut Inner, before: BTreeMap<String, String>) -> bool {
        let changed = effective(inner, &self.keys) != before;
        if changed {
            inner.generation += 1;
        }
        self.publish_ready(state_of(inner, &self.keys));
        changed
    }

    fn publish_ready(&self, state: Binding) {
        let Some(ready) = &self.ready_file else {
            return;
        };
        let result = if state == Binding::Bound {
            std::fs::write(ready, b"1\n")
        } else {
            match std::fs::remove_file(ready) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        };
        if let Err(e) = result {
            eprintln!("oj: runtime env ready file {}: {e}", ready.display());
        }
    }

    pub(crate) fn state(&self) -> Binding {
        state_of(&self.inner.lock().unwrap(), &self.keys)
    }

    /// Documents wait while the file is on disk but incomplete.
    pub(crate) fn pending(&self) -> bool {
        self.state() == Binding::Pending
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// The values in force (set over file), by key.
    pub(crate) fn values(&self) -> BTreeMap<String, String> {
        effective(&self.inner.lock().unwrap(), &self.keys)
    }

    /// The `%KEY%` map for a page: `base` (the literals) with the values in
    /// force over it.
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
                let (value, source) = value_of(&inner, &self.fallbacks, &key.key);
                serde_json::json!({
                    "key": key.key,
                    "field": key.field,
                    "value": value,
                    "source": source.map(Source::name),
                })
            })
            .collect();
        let file_state = inner.file_state.clone().unwrap_or(FileState::Absent);
        let mut file = serde_json::json!({
            "path": self.file.as_ref().map(|f| f.display().to_string()),
            "state": file_state.name(),
        });
        match &file_state {
            FileState::Pending { missing } => file["missing"] = serde_json::json!(missing),
            FileState::Invalid(error) => file["error"] = serde_json::json!(error),
            _ => {}
        }
        serde_json::json!({
            "state": state_of(&inner, &self.keys).name(),
            "generation": inner.generation,
            "module": self.module_url,
            "keys": keys,
            "file": file,
            "readyFile": self.ready_file.as_ref().map(|f| f.display().to_string()),
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
        let inner = self.inner.lock().unwrap();
        let mut fields: Vec<String> = self
            .statics
            .iter()
            .map(|(name, expr)| format!("  {}: {expr}", json_str(name)))
            .collect();
        for key in &self.keys {
            match value_of(&inner, &self.fallbacks, &key.key) {
                (Some(value), Some(Source::Fallback)) => {
                    // The literal is an expression as the define held it.
                    fields.push(format!("  {}: {value}", json_str(&key.key)));
                }
                (Some(value), _) => {
                    fields.push(format!("  {}: {}", json_str(&key.key), json_str(&value)));
                }
                (None, _) => {}
            }
        }
        format!(
            "const env = {{\n{}\n}};\nObject.defineProperty(globalThis, {global}, {{ value: env, configurable: true, enumerable: false, writable: false }});\nexport default env;\n",
            fields.join(",\n"),
            global = json_str(GLOBAL),
        )
    }
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn describe(state: &FileState) -> String {
    match state {
        FileState::Absent => "absent".into(),
        FileState::Complete => "complete".into(),
        FileState::Pending { missing } => format!("pending, {} missing", missing.join(", ")),
        FileState::Invalid(error) => format!("invalid, {error}"),
    }
}

/// The file's directory canonicalized (it exists before the file does) with
/// the file name back on, as the watcher names the file.
fn canonical_parent(file: &Path) -> PathBuf {
    match (file.parent(), file.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| file.to_path_buf()),
        _ => file.to_path_buf(),
    }
}

fn effective(inner: &Inner, keys: &[RuntimeKey]) -> BTreeMap<String, String> {
    keys.iter()
        .filter_map(|key| {
            inner
                .set_values
                .get(&key.key)
                .or_else(|| inner.file_values.get(&key.key))
                .map(|v| (key.key.clone(), v.clone()))
        })
        .collect()
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
    if let Some(v) = inner.file_values.get(key) {
        return (Some(v.clone()), Some(Source::File));
    }
    match fallbacks.get(key) {
        Some(v) => (Some(v.clone()), Some(Source::Fallback)),
        None => (None, None),
    }
}

fn state_of(inner: &Inner, keys: &[RuntimeKey]) -> Binding {
    let values = effective(inner, keys);
    if values.len() == keys.len() {
        return Binding::Bound;
    }
    match &inner.file_state {
        Some(FileState::Pending { .. }) | Some(FileState::Invalid(_)) => Binding::Pending,
        _ if values.is_empty() => Binding::Unbound,
        _ => Binding::Partial,
    }
}

/// Reads the values file: a regular file of at most [`MAX_FILE_BYTES`]
/// holding a JSON object, each key's field a string (an empty one is a
/// value: a backend with no project id publishes `""`). A field missing or
/// null leaves the key pending; any other shape makes the whole file
/// invalid, since a value of the wrong type is a writer bug rather than a
/// backend still starting.
pub(crate) fn read_file(file: &Path, keys: &[RuntimeKey]) -> (BTreeMap<String, String>, FileState) {
    let meta = match std::fs::symlink_metadata(file) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (BTreeMap::new(), FileState::Absent)
        }
        Err(e) => return (BTreeMap::new(), FileState::Invalid(e.to_string())),
    };
    if !meta.is_file() {
        return (
            BTreeMap::new(),
            FileState::Invalid("not a regular file".into()),
        );
    }
    if meta.len() > MAX_FILE_BYTES {
        return (
            BTreeMap::new(),
            FileState::Invalid(format!("larger than {MAX_FILE_BYTES} bytes")),
        );
    }
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => return (BTreeMap::new(), FileState::Invalid(e.to_string())),
    };
    parse_values(&bytes, keys)
}

pub(crate) fn parse_values(
    bytes: &[u8],
    keys: &[RuntimeKey],
) -> (BTreeMap<String, String>, FileState) {
    let json: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(json) => json,
        Err(e) => {
            return (
                BTreeMap::new(),
                FileState::Invalid(format!("not JSON: {e}")),
            )
        }
    };
    let Some(object) = json.as_object() else {
        return (
            BTreeMap::new(),
            FileState::Invalid("not a JSON object".into()),
        );
    };
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for key in keys {
        match object.get(&key.field) {
            Some(serde_json::Value::String(s)) => {
                values.insert(key.key.clone(), s.clone());
            }
            None | Some(serde_json::Value::Null) => {
                missing.push(key.key.clone());
            }
            Some(other) => {
                return (
                    BTreeMap::new(),
                    FileState::Invalid(format!(
                        "field {} for {} is not a string ({})",
                        key.field,
                        key.key,
                        kind_of(other)
                    )),
                );
            }
        }
    }
    let state = if missing.is_empty() {
        FileState::Complete
    } else {
        FileState::Pending { missing }
    };
    (values, state)
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

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// `GET /@oj/env.js`.
pub(crate) async fn serve_env_module(State(state): State<Arc<ServerState>>) -> Response {
    let Some(env) = &state.runtime_env else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        env.module_js(),
    )
        .into_response()
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
    if !local_caller(&headers) {
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
                    announce(&state, env, "set");
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
pub(crate) fn announce(state: &ServerState, env: &RuntimeEnv, how: &str) {
    println!(
        "oj: runtime env {how} -> {} (generation {}) -> full-reload",
        env.state().name(),
        env.generation()
    );
    // Buffered for a page still booting, as a dep re-optimization's reload is.
    hmr::send_or_buffer(
        state,
        hmr::full_reload_frame("runtime env", None, env.file()),
    );
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

    /// The 503 a document gets while the runtime env is pending: a page
    /// carrying the HMR client, so the reload that follows the binding
    /// reaches it as it reaches every other page.
    pub(crate) fn runtime_env_hold(&self) -> Option<Response> {
        let env = self.runtime_env.as_ref()?;
        if !env.pending() {
            return None;
        }
        let nonce = self
            .csp_nonce
            .as_deref()
            .map(|n| format!(" nonce=\"{}\"", dev_html::escape_html(n)))
            .unwrap_or_default();
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>oj: runtime env pending</title><script type=\"module\" src=\"{}\"{nonce}></script></head><body><p>oj: runtime env pending. The page loads once the backend publishes its values.</p></body></html>\n",
            dev_html::escape_html(&self.oj_paths.client),
        );
        Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                    (header::RETRY_AFTER, "1"),
                ],
                body,
            )
                .into_response(),
        )
    }
}

#[cfg(test)]
#[path = "runtime_env_tests.rs"]
mod tests;
