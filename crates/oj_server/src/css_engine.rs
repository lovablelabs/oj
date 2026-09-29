// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! In-process CSS toolchain: Tailwind/PostCSS, Less/Stylus and Svelte compiles
//! run on an embedded JS engine ([`oj_js::JsEngine`]) resolving the app's own
//! node_modules, replacing the former node sidecar processes. One lazily
//! spawned engine per kind, so a plain app pays for nothing and a Tailwind
//! scan never queues behind a Svelte compile.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oj_js::{EngineConfig, EngineError, JsEngine};

/// Per-request deadline in dev, matching the old sidecar's request timeout.
pub const DEV_DEADLINE: Duration = Duration::from_secs(20);
/// Per-request deadline for the build's one-shot compiles (the old path had
/// none and could hang forever; that was a bug, not a contract).
pub const BUILD_DEADLINE: Duration = Duration::from_secs(60);
/// Per-request deadline for the Start server's stylesheet host.
pub const START_DEADLINE: Duration = Duration::from_secs(30);

const TAILWIND_JS: &str = include_str!("assets/css-tailwind.mjs");
const PREPROCESS_JS: &str = include_str!("assets/css-preprocess.mjs");
const SVELTE_JS: &str = include_str!("assets/svelte-compile.mjs");

/// Marker the engine modules prefix onto a resolution failure of the toolchain
/// package itself, so the host prints the "is X installed?" hint.
const MISSING_PACKAGE_MARKER: &str = "OJ_MISSING_PACKAGE ";

/// One CSS compile kind (tailwind/postcss, less/stylus, or svelte) backed by
/// its own engine. Spawn lazily: the engine thread and V8 isolate exist only
/// once a request of that kind arrives.
///
/// The isolate carries the same heap cap as the plugin host (the toolchains
/// it runs are app-controlled JS, and Tailwind's config loader is a known
/// per-compile module leak), and the cap degrades the same way: the running
/// compile fails with the memory-limit error while the engine is replaced
/// with a fresh one, so the NEXT compile runs on a clean heap instead of the
/// doubled-cap heap the unwind left behind. Callers hold this struct in
/// once-cells; the swap lives inside so every one of them heals.
pub struct CssEngine {
    /// Generation-stamped so concurrent memory-limit failures revive once.
    /// Compiles hold the read guard across the whole engine call on purpose:
    /// the alternative (clone an Arc<JsEngine> under a short lock and call
    /// outside it) lets a replaced engine's isolate be dropped from an async
    /// context by whichever compile finishes last. The cost is that a
    /// straggler compile can hold revive's write lock out until its own
    /// deadline; acceptable for compiles that are normally sub-second.
    engine: tokio::sync::RwLock<(u64, JsEngine)>,
    root: PathBuf,
    script: String,
    base: String,
    kind: &'static str,
    deadline: Duration,
    memory_limit_bytes: usize,
    /// Plugin-host parity for the replacement engine too: bounded attempts
    /// with spacing, so an app that legitimately needs more heap than the cap
    /// degrades to failing compiles instead of booting a fresh V8 isolate per
    /// keystroke forever.
    revive_attempts: std::sync::atomic::AtomicU32,
    last_revive: std::sync::Mutex<Option<std::time::Instant>>,
    /// The PostCSS config path `find_postcss_config` located, handed to the
    /// tailwind module per request (the old sidecar carried it as an env var).
    postcss_config: Option<String>,
}

impl CssEngine {
    pub async fn tailwind(root: &Path, deadline: Duration) -> anyhow::Result<std::sync::Arc<Self>> {
        let postcss_config =
            crate::find_postcss_config(root).map(|p| p.to_string_lossy().into_owned());
        Self::spawn(
            root,
            "css-tailwind.mjs",
            TAILWIND_JS,
            "tailwind",
            deadline,
            postcss_config,
            shared_memory_limit(),
        )
        .await
    }

    pub async fn preprocess(
        root: &Path,
        deadline: Duration,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        Self::spawn(
            root,
            "css-preprocess.mjs",
            PREPROCESS_JS,
            "css preprocessor",
            deadline,
            None,
            shared_memory_limit(),
        )
        .await
    }

    pub async fn svelte(root: &Path, deadline: Duration) -> anyhow::Result<std::sync::Arc<Self>> {
        Self::spawn(
            root,
            "svelte-compile.mjs",
            SVELTE_JS,
            "svelte compiler",
            deadline,
            None,
            shared_memory_limit(),
        )
        .await
    }

    /// `memory_limit_bytes` is a plain usize on purpose: `EngineConfig`'s
    /// own field is an Option whose None means uncapped, and an uncapped CSS
    /// engine is the bug this struct exists to prevent — the type makes it
    /// unwritable here. Production callers pass [`shared_memory_limit`].
    async fn spawn(
        root: &Path,
        name: &str,
        js: &'static str,
        kind: &'static str,
        deadline: Duration,
        postcss_config: Option<String>,
        memory_limit_bytes: usize,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        let script = oj_cache::cache_root(root).join(name);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, js)?;

        let engine = Self::spawn_engine(root, deadline, memory_limit_bytes)
            .await
            .map_err(|e| anyhow::anyhow!("cannot start the {kind} engine: {e}"))?;
        Ok(std::sync::Arc::new(CssEngine {
            engine: tokio::sync::RwLock::new((0, engine)),
            root: root.to_path_buf(),
            script: script.to_string_lossy().into_owned(),
            base: root.display().to_string(),
            kind,
            deadline,
            memory_limit_bytes,
            revive_attempts: std::sync::atomic::AtomicU32::new(0),
            last_revive: std::sync::Mutex::new(None),
            postcss_config,
        }))
    }

    async fn spawn_engine(
        root: &Path,
        deadline: Duration,
        memory_limit_bytes: usize,
    ) -> Result<JsEngine, EngineError> {
        let config = EngineConfig {
            root: root.to_path_buf(),
            memory_limit_bytes: Some(memory_limit_bytes),
            default_deadline: Some(deadline),
            // Tailwind recompiles per edit and its toolchain state lives in
            // this isolate: the between-edits idle is exactly where the
            // high-water pages should come back.
            idle_shrink_after: Some(oj_js::DEFAULT_IDLE_SHRINK_AFTER),
            code_cache_dir: Some(crate::engine_code_cache_dir(root)),
        };
        // JsEngine::spawn blocks until the isolate is up; keep it off the
        // async workers.
        tokio::task::spawn_blocking(move || JsEngine::spawn(config))
            .await
            .map_err(|e| EngineError::Boot(e.to_string()))?
    }

    /// Replaces the engine after a memory-limit (or closed-engine) failure,
    /// unless another failing call already did: the generation stamp makes
    /// concurrent losers no-ops. The failing call still reports its error;
    /// only the NEXT compile runs on the fresh heap — plugin-host semantics,
    /// including its attempt limit and spacing (an app whose compile
    /// legitimately outgrows the cap fails its compiles instead of respawning
    /// isolates per keystroke forever).
    async fn revive(&self, seen_generation: u64, trigger: &EngineError) {
        use std::sync::atomic::Ordering;
        let attempts = self.revive_attempts.load(Ordering::SeqCst);
        if attempts >= crate::plugins::PLUGIN_HOST_RESPAWN_LIMIT {
            return;
        }
        if let Ok(last) = self.last_revive.lock() {
            if last.is_some_and(|at| at.elapsed() < crate::plugins::PLUGIN_HOST_RESPAWN_SPACING) {
                return;
            }
        }
        let mut slot = self.engine.write().await;
        if slot.0 != seen_generation {
            return;
        }
        if let Ok(mut last) = self.last_revive.lock() {
            *last = Some(std::time::Instant::now());
        }
        match Self::spawn_engine(&self.root, self.deadline, self.memory_limit_bytes).await {
            Ok(fresh) => {
                slot.0 += 1;
                slot.1 = fresh;
                let attempts = self.revive_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let cause = match trigger {
                    EngineError::MemoryLimit => format!(
                        "hit its JS memory limit ({}MB; raise OJ_PLUGIN_MEMORY_MB)",
                        self.memory_limit_bytes / (1024 * 1024)
                    ),
                    _ => "shut down".to_string(),
                };
                eprintln!(
                    "oj: the {} engine {cause} and was replaced ({attempts}/{})",
                    self.kind,
                    crate::plugins::PLUGIN_HOST_RESPAWN_LIMIT
                );
            }
            Err(e) => eprintln!("oj: the {} engine could not be replaced: {e}", self.kind),
        }
    }

    /// Compiles a stylesheet requested by its dev-server url (`/src/a.css`,
    /// `/@fs/...`), rooted at the app base like the old sidecar protocol.
    pub async fn compile(&self, css: &str, from: &str) -> Result<String, String> {
        self.compile_with(css, from, serde_json::Value::Null).await
    }

    /// `options` is the user's `css.preprocessorOptions.<lang>` object
    /// (Less/Stylus options), handed to the preprocessor as-is.
    pub async fn compile_with(
        &self,
        css: &str,
        from: &str,
        options: serde_json::Value,
    ) -> Result<String, String> {
        let from = absolute_from(&self.base, from);
        self.request(css, from, options, true).await
    }

    /// Compiles from an absolute file path (the build and the Start host know
    /// the file, not a served url). `dev` reaches the svelte compiler's
    /// `dev`/`hmr` flags.
    pub async fn compile_path(
        &self,
        css: &str,
        from: &Path,
        options: serde_json::Value,
        dev: bool,
    ) -> Result<String, String> {
        self.request(css, from.to_string_lossy().into_owned(), options, dev)
            .await
    }

    async fn request(
        &self,
        css: &str,
        from: String,
        options: serde_json::Value,
        dev: bool,
    ) -> Result<String, String> {
        let request = serde_json::json!({
            "base": self.base,
            "css": css,
            "from": from,
            "options": options,
            "dev": dev,
            "postcssConfig": self.postcss_config,
        });
        let (generation, result) = {
            let slot = self.engine.read().await;
            (
                slot.0,
                slot.1
                    .call(self.script.clone(), "compile", vec![request])
                    .await,
            )
        };
        match result {
            Ok(serde_json::Value::String(css)) => Ok(css),
            Ok(other) => Err(format!(
                "{} compile produced no output ({other})",
                self.kind
            )),
            Err(e) => {
                // A blown or dead isolate never comes back on its own; swap in
                // a fresh one so the next compile works, and fail this one.
                if matches!(e, EngineError::MemoryLimit | EngineError::Closed) {
                    self.revive(generation, &e).await;
                }
                Err(map_engine_error(self.kind, self.deadline, e))
            }
        }
    }
}

/// The one heap cap for every embedded engine: the plugin host's Node-parity
/// resolution (OJ_PLUGIN_MEMORY_MB, then NODE_OPTIONS --max-old-space-size,
/// then 4096MB).
fn shared_memory_limit() -> usize {
    crate::plugins::plugin_host_memory_mb() * 1024 * 1024
}

fn map_engine_error(kind: &str, deadline: Duration, e: EngineError) -> String {
    match e {
        EngineError::Js(message) => match missing_package(&message) {
            Some(package) => format!("{kind} compile failed (is {package} installed?)"),
            None => strip_stack(&message).to_string(),
        },
        EngineError::Deadline => {
            format!("{kind} compile timed out after {}s", deadline.as_secs())
        }
        EngineError::MemoryLimit => format!("{kind} compile exceeded the JS memory limit"),
        EngineError::Boot(e) => format!("cannot start the {kind} engine: {e}"),
        EngineError::Closed => format!("the {kind} engine is shut down"),
    }
}

/// The package named by the module's missing-package marker, if the error is a
/// resolution failure for the toolchain package itself.
fn missing_package(message: &str) -> Option<&str> {
    let rest = &message[message.find(MISSING_PACKAGE_MARKER)? + MISSING_PACKAGE_MARKER.len()..];
    let name = rest.split(':').next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// A thrown error's stack frames add noise the sidecars never sent; keep the
/// message lines only.
fn strip_stack(message: &str) -> &str {
    match message.find("\n    at ") {
        Some(idx) => message[..idx].trim_end(),
        None => message.trim_end(),
    }
}

fn absolute_from(base: &str, from: &str) -> String {
    let clean = from.split(['?', '#']).next().unwrap_or(from);
    if let Some(fs) = clean.strip_prefix("/@fs") {
        return fs.to_string();
    }
    if let Some(rel) = clean.strip_prefix('/') {
        return Path::new(base).join(rel).display().to_string();
    }
    Path::new(base).join(clean).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_from_roots_urls_at_base() {
        assert_eq!(
            absolute_from("/app/web", "/src/styles/globals.css"),
            "/app/web/src/styles/globals.css"
        );
        assert_eq!(
            absolute_from("/app/web", "/src/styles/globals.css?direct"),
            "/app/web/src/styles/globals.css"
        );
        assert_eq!(
            absolute_from("/app/web", "/@fs/pkg/dist/theme.css"),
            "/pkg/dist/theme.css"
        );
        assert_eq!(
            absolute_from("/app/web", "relative/x.css"),
            "/app/web/relative/x.css"
        );
    }

    #[test]
    fn a_missing_toolchain_package_maps_to_the_install_hint() {
        let err = EngineError::Js(
            "Uncaught (in promise) Error: OJ_MISSING_PACKAGE less: Cannot find module 'less'\n    at load (file:///x/.oj-cache/css-preprocess.mjs:20:11)".into(),
        );
        assert_eq!(
            map_engine_error("css preprocessor", DEV_DEADLINE, err),
            "css preprocessor compile failed (is less installed?)"
        );
        let err = EngineError::Js(
            "Error: OJ_MISSING_PACKAGE tailwindcss: cannot resolve '@tailwindcss/node' from /app"
                .into(),
        );
        assert_eq!(
            map_engine_error("tailwind", DEV_DEADLINE, err),
            "tailwind compile failed (is tailwindcss installed?)"
        );
    }

    #[test]
    fn a_compile_error_keeps_its_message_without_stack_frames() {
        let err = EngineError::Js(
            "CssSyntaxError: /app/src/a.css:1:1: Unclosed block\n    at Input.error (file:///x)"
                .into(),
        );
        assert_eq!(
            map_engine_error("tailwind", DEV_DEADLINE, err),
            "CssSyntaxError: /app/src/a.css:1:1: Unclosed block"
        );
    }

    #[test]
    fn limits_map_to_the_timeout_shape() {
        assert_eq!(
            map_engine_error("tailwind", DEV_DEADLINE, EngineError::Deadline),
            "tailwind compile timed out after 20s"
        );
        assert_eq!(
            map_engine_error("svelte compiler", BUILD_DEADLINE, EngineError::Deadline),
            "svelte compiler compile timed out after 60s"
        );
        assert_eq!(
            map_engine_error("tailwind", DEV_DEADLINE, EngineError::MemoryLimit),
            "tailwind compile exceeded the JS memory limit"
        );
    }

    fn app_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"css-engine-test","version":"1.0.0"}"#,
        )
        .unwrap();
        dir
    }

    fn stub_less(root: &Path, index_cjs: &str) {
        let dir = root.join("node_modules/less");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"less","version":"1.0.0","main":"index.cjs"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.cjs"), index_cjs).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_preprocessor_reports_the_install_hint_through_the_engine() {
        let root = app_root();
        let engine = CssEngine::preprocess(root.path(), DEV_DEADLINE)
            .await
            .unwrap();
        let err = engine
            .compile(".box { color: red }", "/src/a.less")
            .await
            .unwrap_err();
        assert_eq!(err, "css preprocessor compile failed (is less installed?)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hung_compile_hits_the_deadline_and_the_engine_survives() {
        let root = app_root();
        stub_less(
            root.path(),
            "module.exports = { FileManager: class {}, render(css) { if (css.includes('hang')) { for (;;) {} } return Promise.resolve({ css: css + '/*ok*/' }); } };",
        );
        let engine = CssEngine::preprocess(root.path(), Duration::from_secs(1))
            .await
            .unwrap();
        // Outer guard: this test's failure mode is an infinite hang (the
        // deadline never classifying), which must fail fast, not wedge CI.
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            engine.compile(".hang {}", "/src/a.less"),
        )
        .await
        .expect("the compile deadline never fired: the engine is wedged")
        .unwrap_err();
        assert_eq!(err, "css preprocessor compile timed out after 1s");
        // The isolate is un-poisoned: the next request compiles.
        let out = engine.compile(".fine {}", "/src/a.less").await.unwrap();
        assert_eq!(out, ".fine {}/*ok*/");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_blown_heap_fails_the_compile_and_the_next_one_runs_fresh() {
        // The engine carries the plugin host's heap cap; a toolchain that
        // eats the whole heap (Tailwind's per-compile config-module leak is
        // the production shape) must fail THAT compile with the memory-limit
        // error and leave a fresh engine behind, not a wedged doubled-cap
        // isolate that grows to the node's ceiling.
        let root = app_root();
        stub_less(
            root.path(),
            "const hog = []; module.exports = { FileManager: class {}, render(css) { \
             if (css.includes('boom')) { for (;;) hog.push(new Array(1024 * 1024).fill(Math.random())); } \
             return Promise.resolve({ css: css + '/*ok*/' }); } };",
        );
        let engine = CssEngine::spawn(
            root.path(),
            "css-preprocess.mjs",
            PREPROCESS_JS,
            "css preprocessor",
            DEV_DEADLINE,
            None,
            128 * 1024 * 1024,
        )
        .await
        .unwrap();
        let err = engine.compile(".boom {}", "/src/a.less").await.unwrap_err();
        assert_eq!(err, "css preprocessor compile exceeded the JS memory limit");
        // revive() ran inside the failing call: the replacement compiles.
        let out = engine.compile(".fine {}", "/src/a.less").await.unwrap();
        assert_eq!(out, ".fine {}/*ok*/");
        assert_eq!(engine.engine.read().await.0, 1, "one revive, stamped");
    }
}
