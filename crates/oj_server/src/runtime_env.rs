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
    /// `file` with its directory canonicalized, as the watcher reports
    /// paths; taken again once the directory exists ([`Self::armed`]), since
    /// a directory missing at boot has no real path yet.
    file_canonical: Mutex<Option<PathBuf>>,
    /// Whether the server listens yet ([`Self::serving`]).
    serving: std::sync::atomic::AtomicBool,
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
        let file_canonical = Mutex::new(file.as_ref().map(|f| canonical_parent(f)));
        let env = RuntimeEnv {
            keys: cfg.keys,
            file,
            file_canonical,
            serving: std::sync::atomic::AtomicBool::new(false),
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
        self.file.as_deref() == Some(path)
            || self.file_canonical.lock().unwrap().as_deref() == Some(path)
    }

    /// Whether a watcher path is in the values file's directory: a write
    /// there may replace what the file names without touching its own name
    /// (a mounted volume swaps the directory a symlink points into).
    pub(crate) fn beside_file(&self, path: &Path) -> bool {
        let dir = |file: Option<&Path>| file.and_then(Path::parent).map(Path::to_path_buf);
        let parent = path.parent();
        parent.is_some()
            && (parent == dir(self.file.as_deref()).as_deref()
                || parent == dir(self.file_canonical.lock().unwrap().as_deref()).as_deref())
    }

    /// The values file's directory came under watch: its real path is known
    /// now, when it did not exist at boot.
    pub(crate) fn armed(&self) {
        if let Some(file) = &self.file {
            *self.file_canonical.lock().unwrap() = Some(canonical_parent(file));
        }
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

    /// Re-reads the values file. True when the pages must reload: the
    /// values in force changed, or documents stopped waiting.
    pub(crate) fn load_file(&self) -> bool {
        let Some(file) = &self.file else {
            return false;
        };
        let (values, state) = read_file(file, &self.keys);
        let mut inner = self.inner.lock().unwrap();
        let before = (effective(&inner, &self.keys), state_of(&inner, &self.keys));
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
    /// Refuses a key outside the list. True when the pages must reload.
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
        let before = (effective(&inner, &self.keys), state_of(&inner, &self.keys));
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
        *carried().lock().unwrap() =
            (!inner.set_values.is_empty()).then(|| serde_json::json!(inner.set_values).to_string());
        Ok(self.settle(&mut inner, before))
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
            .filter(|(key, _)| self.keys.iter().any(|k| k.key == *key))
            .collect();
        let _ = self.set(listed);
    }

    /// After a change to either layer: bumps the generation when the values
    /// in force moved and brings the ready file in line. True when the pages
    /// must reload: the values moved, or documents no longer wait (the file
    /// was removed while pending, so nothing bound and no value changed, yet
    /// a tab on the hold page would sit there with nothing to tell it).
    fn settle(&self, inner: &mut Inner, before: (BTreeMap<String, String>, Binding)) -> bool {
        let changed = effective(inner, &self.keys) != before.0;
        if changed {
            inner.generation += 1;
        }
        let state = state_of(inner, &self.keys);
        self.publish_ready(state);
        changed || (before.1 == Binding::Pending && state != Binding::Pending)
    }

    fn publish_ready(&self, state: Binding) {
        let Some(ready) = &self.ready_file else {
            return;
        };
        // Ready says a server answers with every value: not before it
        // listens, and not once it was told to stop.
        let serving = self.serving.load(std::sync::atomic::Ordering::SeqCst)
            && !STOPPING.load(std::sync::atomic::Ordering::SeqCst);
        let result = if state == Binding::Bound && serving {
            ready_files().lock().unwrap().insert(ready.clone());
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

    /// The server listens: the ready file may say so from here on.
    pub(crate) fn serving(&self) {
        self.serving
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.publish_ready(self.state());
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
                // A fallback is the expression the define held; the `.env`
                // literal it came from reads back as the value itself.
                let value = match source {
                    Some(Source::Fallback) => {
                        value.map(|expr| serde_json::from_str::<String>(&expr).unwrap_or(expr))
                    }
                    _ => value,
                };
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
        // A page that loaded these values just before they changed may have
        // had no socket open for the reload: once loaded, it asks for the
        // generation in force and reloads when it is another. Only a number
        // counts, so an answer that is not the server's reloads nothing, and
        // a generation is reloaded for once in a tab: a copy of this module
        // served from a cache (an app's service worker) would otherwise
        // reload the page for as long as it is stale.
        let check = format!(
            "if (typeof window !== \"undefined\" && typeof fetch === \"function\") {{\n  const check = () => fetch({url}, {{ cache: \"no-store\" }}).then((r) => (r.ok ? r.text() : \"\")).then((g) => {{\n    if (!/^\\d+$/.test(g) || g === {generation}) return;\n    try {{ if (sessionStorage.getItem({guard}) === g) return; sessionStorage.setItem({guard}, g); }} catch {{}}\n    location.reload();\n  }}, () => {{}});\n  if (document.readyState === \"complete\") check(); else window.addEventListener(\"load\", check, {{ once: true }});\n}}\n",
            url = json_str(&format!("{}?{GENERATION_QUERY}", self.module_url)),
            generation = json_str(&inner.generation.to_string()),
            guard = json_str("__oj_env_reloaded"),
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
/// edit, which restarts the server, silently put every key back on the
/// file's value or its literal.
pub(crate) fn carried_set() -> Option<String> {
    carried().lock().unwrap().clone()
}

/// The ready files this process wrote, for [`clear_ready_files`].
fn ready_files() -> &'static Mutex<std::collections::BTreeSet<PathBuf>> {
    static FILES: std::sync::OnceLock<Mutex<std::collections::BTreeSet<PathBuf>>> =
        std::sync::OnceLock::new();
    FILES.get_or_init(Default::default)
}

/// Set once the server is told to stop: a value bound after that writes no
/// ready file.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Removes every ready file this process wrote: a stopped server serves no
/// value, so nothing is ready. Called when the server is told to stop; a
/// process killed outright leaves the file, which the next start removes.
pub fn clear_ready_files() {
    STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
    for file in std::mem::take(&mut *ready_files().lock().unwrap()) {
        let _ = std::fs::remove_file(file);
    }
}

impl Drop for RuntimeEnv {
    fn drop(&mut self) {
        if let Some(ready) = &self.ready_file {
            if ready_files().lock().unwrap().remove(ready) {
                let _ = std::fs::remove_file(ready);
            }
        }
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

/// Reads the values file: a regular file, or a symlink to one (a mounted
/// volume publishes its files as symlinks), of at most [`MAX_FILE_BYTES`]
/// holding a JSON object, each key's field a string (an empty one is a
/// value: a backend with no project id publishes `""`). A field missing or
/// null leaves the key pending; any other shape makes the whole file
/// invalid, since a value of the wrong type is a writer bug rather than a
/// backend still starting.
pub(crate) fn read_file(file: &Path, keys: &[RuntimeKey]) -> (BTreeMap<String, String>, FileState) {
    let meta = match std::fs::metadata(file) {
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
    // What was looked at above may not be what the name leads to now (a
    // link moved in between): the file opened is looked at again, and read
    // no further than the limit.
    let read = || -> std::io::Result<Option<Vec<u8>>> {
        use std::io::Read as _;
        let opened = std::fs::File::open(file)?;
        if !opened.metadata()?.is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        opened.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    };
    match read() {
        Ok(Some(bytes)) if bytes.len() as u64 > MAX_FILE_BYTES => (
            BTreeMap::new(),
            FileState::Invalid(format!("larger than {MAX_FILE_BYTES} bytes")),
        ),
        Ok(Some(bytes)) => parse_values(&bytes, keys),
        Ok(None) => (
            BTreeMap::new(),
            FileState::Invalid("not a regular file".into()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (BTreeMap::new(), FileState::Absent),
        Err(e) => (BTreeMap::new(), FileState::Invalid(e.to_string())),
    }
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

/// Who may read or set the runtime values: a [`local_caller`] that is not a
/// browser's request at all (every browser request carries `Sec-Fetch-Site`,
/// the page's own same-origin `fetch` included, which has no `Origin`) and
/// whose connection comes from this machine. The header rule alone let any
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
/// `oj env`, whose connection then comes from that address). Only an
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
        ],
        if classic {
            env.classic_js()
        } else {
            env.module_js()
        },
    )
        .into_response()
}

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
    let load = match worker_type {
        Some("classic") => format!("importScripts({script});"),
        // One line, so the map stands: the values are JSON, which holds no
        // line break outside the ones this rendering puts between fields.
        Some("ignore") => env.classic_js().replace('\n', " ").trim_end().to_string(),
        _ => return body,
    };
    // Same line count, so the map stands.
    body.replacen(&format!("import {};", json_str(&env.module_url)), &load, 1)
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
    // Only the pages connected: one loading now compares the generation it
    // was handed with the one in force once it has loaded (the module's
    // check), and the hold page asks again by itself, so no frame is kept
    // for the next socket. A frame kept reloaded the next page to connect
    // though it had loaded the values in force.
    let _ = state
        .reload_tx
        .send(hmr::full_reload_frame("runtime env", None, env.file()));
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
    /// reaches it as it reaches every other page, and asking again by itself
    /// every two seconds, for a binding made before its socket opened.
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
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>oj: runtime env pending</title><meta http-equiv=\"refresh\" content=\"2\"><script type=\"module\" src=\"{}\"{nonce}></script></head><body><p>oj: runtime env pending. The page loads once the backend publishes its values.</p></body></html>\n",
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

/// What `oj env` reports for a dev server started without the keys.
pub const NOT_CONFIGURED: &str = "the dev server runs without OJ_RUNTIME_ENV_KEYS";

/// Asks the dev server running for `root` for its runtime env (the status
/// `GET /@oj/env` answers), applying `updates` first when given (a `None`
/// unsets a key): what `oj env` does. `Err` says why there is no answer: no
/// server runs for `root`, it runs without the keys, it refused the update
/// (a key outside the list), or it did not answer.
pub async fn env_via_server(
    root: &Path,
    updates: Option<&BTreeMap<String, Option<String>>>,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let url = typecheck::advertised(root)?.http_url(CONTROL_PATH, "oj env")?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout.min(Duration::from_secs(30)))
        .build()
        .map_err(|e| e.to_string())?;
    let request = match updates {
        Some(updates) => client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_string(updates).map_err(|e| e.to_string())?),
        None => client.get(&url),
    };
    let response = request.send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    match status {
        reqwest::StatusCode::NOT_FOUND => Err(NOT_CONFIGURED.to_string()),
        reqwest::StatusCode::BAD_REQUEST => Err(String::from_utf8_lossy(&body).trim().to_string()),
        status if !status.is_success() => Err(format!("{url}: {status}")),
        _ => serde_json::from_slice(&body).map_err(|e| format!("{url}: {e}")),
    }
}

#[cfg(test)]
#[path = "runtime_env_tests.rs"]
mod tests;
