// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's bundles on oj's own rolldown: the production client,
//! server and Cloudflare Worker builds, and the dev client bundle with its
//! hot updates. The bundler is the rolldown oj links (`BundlerBuilder` for a
//! build, `rolldown_dev`'s `DevEngine` for hot updates), the same one
//! `oj build` drives for any other app. The app's Vite plugins run in plugin
//! hosts (one per environment) behind [`host::StartHostPlugin`]; what the
//! TanStack Vite plugin would inject (the framework's entry aliases, the
//! client side of `createServerFn`, the asset and stylesheet conventions)
//! comes from the plugins in this module.

pub(crate) mod alias;
pub(crate) mod assets;
pub(crate) mod chunks;
pub(crate) mod cloudflare;
pub(crate) mod code;
pub(crate) mod dev;
pub(crate) mod errors;
pub(crate) mod facts;
pub(crate) mod host;
pub(crate) mod hosts;
pub(crate) mod hot;
pub(crate) mod prod;
pub(crate) mod refresh;
pub(crate) mod shims;

use std::path::{Path, PathBuf};

/// Which command a bundle serves: the dev server's client bundle, or `oj build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Serve,
    Build,
}

/// What every Start bundle of one command is configured from.
pub(crate) struct StartConfig {
    pub root: PathBuf,
    /// `.oj-cache/v1/start`: the framework entries oj writes (client-entry,
    /// manifest, plugin adapters, ...) and the dev bundle's outputs.
    pub cache: PathBuf,
    pub command: Command,
    /// Vite's `--mode`.
    pub mode: String,
    pub config: oj_config::OjConfig,
    pub app_env: oj_env::AppEnv,
    /// Vite's `base`, normalized (`/`, `/x/` or `./`).
    pub base: String,
    /// The config's `@stylexjs/unplugin`, run natively in every app bundle.
    pub stylex: Option<std::sync::Arc<oj_stylex::plugin::StylexPlugin>>,
}

/// TanStack Start's load-env plugin: `Object.assign(process.env,
/// loadEnv(mode, root, ""))`, so server code sees every var of the mode's
/// `.env` files, not only `envPrefix` ones. A var the shell sets keeps the
/// shell's value.
pub(crate) fn dotenv_process_vars(
    env: &oj_env::Env,
    app_env: &oj_env::AppEnv,
) -> Vec<(String, String)> {
    app_env
        .loaded()
        .iter()
        .filter(|(k, _)| env.var_os(k).is_none())
        .cloned()
        .collect()
}

impl StartConfig {
    /// The prefix the dev server's urls take under `base` (see
    /// [`oj_server::start_dev_url_base`]).
    pub fn dev_url_base(&self) -> String {
        oj_server::start_dev_url_base(&self.base)
    }

    pub fn node_env(&self) -> &str {
        self.app_env.node_env()
    }

    pub fn is_production(&self) -> bool {
        self.app_env.is_production()
    }

    /// The app's router entry (`#tanstack-router-entry`): the package.json
    /// `imports` mapping wins over the `src/router` convention, as in the SSR
    /// host, since the framework makes this path configurable.
    pub fn router_entry(&self) -> PathBuf {
        router_entry(&self.root)
    }

    /// `#tanstack-start-entry`: the app's `src/start.*` (or its `imports`
    /// mapping) as upstream's resolveEntry finds it, else oj's stub of the
    /// framework's default entry (`export const startInstance = undefined`).
    /// The client needs the real file too: `functionMiddleware` wraps
    /// server-fn calls in the browser.
    pub fn start_entry(&self) -> PathBuf {
        crate::start_host::app_start_entry(&self.root)
            .unwrap_or_else(|| self.cache.join("start-entry.ts"))
    }

    /// The app's own Start server entry (`tanstackStart({ server: { entry } })`),
    /// when it configures one; else the bundle enters at oj's.
    pub fn server_entry(&self) -> Option<PathBuf> {
        crate::start_dev::configured_start_server_entry(&self.config, &self.root)
    }

    /// The framework seam every client bundle fills in (what TanStack's Vite
    /// plugin resolves): the router, the start instance, the plugin adapter
    /// list, the manifest, the injected head scripts and the fn stubs.
    pub fn client_aliases(&self) -> Vec<(String, PathBuf)> {
        vec![
            ("#tanstack-router-entry".into(), self.router_entry()),
            ("#tanstack-start-entry".into(), self.start_entry()),
            (
                "#tanstack-start-plugin-adapters".into(),
                self.cache.join("plugin-adapters.ts"),
            ),
            (
                "tanstack-start-manifest:v".into(),
                self.cache.join("manifest.ts"),
            ),
            (
                "tanstack-start-injected-head-scripts:v".into(),
                self.cache.join("injected-head-scripts.ts"),
            ),
            (
                "@tanstack/start-fn-stubs".into(),
                self.cache.join("fn-stubs.mjs"),
            ),
        ]
    }

    /// The server bundle's seam: the client's, plus the server-fn resolver
    /// and oj's handler as `@tanstack/react-start/server-entry` (so an app's
    /// own server entry wrapping it composes as under Vite).
    pub fn server_aliases(&self) -> Vec<(String, PathBuf)> {
        let mut out = self.client_aliases();
        out.push((
            "#tanstack-start-server-fn-resolver".into(),
            self.cache.join("server-fn-resolver.mjs"),
        ));
        out.push((
            "@tanstack/react-start/server-entry".into(),
            self.cache.join("server-entry.tsx"),
        ));
        out
    }

    /// The client environment's export conditions: Vite's defaults with the
    /// user's `resolve.conditions`, dev or prod flavored by the command.
    pub fn client_conditions(&self) -> Vec<String> {
        oj_config::resolve_conditions_for(&self.config, "client", self.command == Command::Serve)
    }

    /// The defines of a client bundle, in application order (a later key
    /// wins): `process.env` as the Start runtime reads it, `global`,
    /// `import.meta.env`, then the config's `define`, the client
    /// environment's own and what plugin config hooks added, as Vite's define
    /// plugin layers them.
    pub fn client_defines(
        &self,
        env_delta: &[(String, String)],
        plugin_defines: &[(String, String)],
    ) -> Vec<(String, String)> {
        let node_env = self.node_env();
        let process_env = serde_json::json!({
            "NODE_ENV": node_env,
            "TSS_SERVER_FN_BASE": SERVER_FN_BASE,
        })
        .to_string();
        let mut out = vec![
            ("process.env".to_string(), process_env),
            (
                "process.env.NODE_ENV".to_string(),
                serde_json::Value::String(node_env.to_string()).to_string(),
            ),
            ("global".to_string(), "globalThis".to_string()),
        ];
        out.extend(self.app_env.import_meta_env_defines(
            oj_env::get(),
            env_delta,
            &self.base,
            false,
        ));
        if self.command == Command::Build {
            out.push(("import.meta.hot".into(), "undefined".into()));
        }
        out.extend(oj_config::config_defines(&self.config));
        out.extend(oj_config::environment_defines(&self.config, "client"));
        out.extend(plugin_defines.iter().cloned());
        out
    }

    /// The server bundle's defines: NODE_ENV and the server-fn base the
    /// framework reads, `import.meta.env` for the server, then the config's.
    pub fn server_defines(
        &self,
        env_name: &str,
        env_delta: &[(String, String)],
        plugin_defines: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut out = vec![
            (
                "process.env.NODE_ENV".to_string(),
                serde_json::Value::String(self.node_env().to_string()).to_string(),
            ),
            (
                "process.env.TSS_SERVER_FN_BASE".to_string(),
                serde_json::Value::String(SERVER_FN_BASE.to_string()).to_string(),
            ),
            ("import.meta.hot".to_string(), "undefined".to_string()),
        ];
        out.extend(self.app_env.import_meta_env_defines(
            oj_env::get(),
            env_delta,
            &self.base,
            true,
        ));
        out.extend(oj_config::config_defines(&self.config));
        out.extend(oj_config::environment_defines(&self.config, env_name));
        out.extend(plugin_defines.iter().cloned());
        out
    }

    /// The client bundle's banner: the `process` / `global` globals the
    /// framework and its dependencies read at module scope.
    pub fn client_banner(&self) -> String {
        let env = serde_json::json!({
            "NODE_ENV": self.node_env(),
            "TSS_SERVER_FN_BASE": SERVER_FN_BASE,
        });
        format!(
            "globalThis.process=globalThis.process||{{env:{env}}};globalThis.global=globalThis.global||globalThis;"
        )
    }

    /// The app's JSX settings for oj's own TS and JSX lowering.
    pub fn jsx_config(&self) -> oj_compiler::JsxConfig {
        crate::build::bundle_graph::jsx_config(&self.config)
    }

    /// rolldown's JSX transform: automatic by default, the config's runtime,
    /// import source and pragmas, `development` following NODE_ENV.
    pub fn transform_options(&self) -> rolldown_common::BundlerTransformOptions {
        let jsx = oj_config::jsx_settings(&self.config);
        let classic = jsx.runtime.as_deref() == Some("classic");
        rolldown_common::BundlerTransformOptions {
            jsx: Some(rolldown_common::Either::Right(
                rolldown_common::JsxOptions {
                    runtime: Some(if classic { "classic" } else { "automatic" }.to_string()),
                    development: Some(!self.is_production()),
                    import_source: if classic { None } else { jsx.import_source },
                    pragma: if classic { jsx.pragma } else { None },
                    pragma_frag: if classic { jsx.pragma_frag } else { None },
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    /// The client bundle's main fields: the top-level `resolve.mainFields`
    /// (Vite's client environment takes them; the default is browser-first).
    pub fn client_main_fields(&self) -> Option<Vec<String>> {
        oj_config::resolve_main_fields(&self.config)
    }

    /// A server bundle's main fields: the ssr environment's own list, else
    /// Vite's server default. The top-level list is the client's and never
    /// applies (Vite drops it from what other environments inherit), or a
    /// package's `browser` build would land in the server bundle.
    pub fn server_main_fields(&self) -> Vec<String> {
        oj_config::ssr_resolve_main_fields(&self.config)
            .unwrap_or_else(oj_resolver::default_server_main_fields)
    }

    /// rolldown's resolver for a Start bundle: the framework seam as aliases
    /// (the app's own `resolve.alias` runs as a plugin ahead of the app's
    /// plugins, as Vite's alias plugin does), the config's extensions, and the
    /// environment's main fields and export conditions.
    pub fn resolve_options(
        &self,
        aliases: &[(String, PathBuf)],
        main_fields: Option<Vec<String>>,
        conditions: Option<Vec<String>>,
    ) -> rolldown_common::ResolveOptions {
        let alias: Vec<(String, Vec<Option<String>>)> = aliases
            .iter()
            .map(|(find, target)| {
                (
                    find.clone(),
                    vec![Some(target.to_string_lossy().into_owned())],
                )
            })
            .collect();
        let symlinks = self
            .config
            .resolve
            .as_ref()
            .and_then(|r| r.preserve_symlinks)
            .map(|preserve| !preserve);
        rolldown_common::ResolveOptions {
            alias: Some(alias),
            extensions: oj_config::resolve_extensions(&self.config),
            main_fields: main_fields.map(oj_resolver::with_main_fallback),
            condition_names: conditions,
            symlinks,
            extension_alias: Some(oj_resolver::default_extension_alias()),
            ..Default::default()
        }
    }
}

/// Writes `<cache>/server-fn-resolver.mjs`, the module behind
/// `#tanstack-start-server-fn-resolver`: every server function under `src/`
/// (the ids the client stubs call) mapped to the handler export the server
/// rewrite adds, through static relative imports so it resolves in the dev
/// module host and in the bundled server alike (a runtime `import` of a
/// `.ts` file would fail in the bundle). How many functions it maps.
pub(crate) fn write_server_fn_resolver(root: &Path, cache: &Path) -> std::io::Result<usize> {
    let mut files = Vec::new();
    let mut stack = vec![root.join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("ts" | "tsx")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut imports = String::new();
    let mut manifest = String::new();
    let mut count = 0;
    for path in &files {
        let Ok(code) = std::fs::read_to_string(path) else {
            continue;
        };
        let fns = oj_compiler::server_fns::find_server_fns(&code);
        if fns.is_empty() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let var = format!("__m{count}");
        let spec = oj_compiler::glob::relative_import(cache, path);
        imports.push_str(&format!("import * as {var} from {};\n", json(&spec)));
        for f in fns {
            manifest.push_str(&format!(
                "  {}: {{ functionName: {}, module: {var} }},\n",
                json(&oj_compiler::server_fns::server_fn_id(&rel, &f.name)),
                json(&format!("{}_createServerFn_handler", f.name)),
            ));
            count += 1;
        }
    }
    let out = format!(
        "// Generated by oj. Maps server-function ids to their handlers.\n{imports}const manifest = {{\n{manifest}}};\n\
         export async function getServerFnById(id) {{\n  const info = manifest[id];\n  \
         if (!info) throw new Error(\"oj: server function not found: \" + id);\n  \
         return info.module[info.functionName];\n}}\n"
    );
    std::fs::write(cache.join("server-fn-resolver.mjs"), out)?;
    Ok(count)
}

fn json(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// The framework's server-function URL prefix (`TSS_SERVER_FN_BASE`).
pub(crate) const SERVER_FN_BASE: &str = "/_serverFn/";

/// The router entry for `root`: package.json `imports["#tanstack-router-entry"]`
/// when it names an existing file, else `src/router.{tsx,ts,jsx,js}`, else
/// `src/router.tsx` (so the miss is reported against the conventional path).
pub(crate) fn router_entry(root: &Path) -> PathBuf {
    let declared = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("imports")?
                .get("#tanstack-router-entry")?
                .as_str()
                .map(String::from)
        })
        .map(|target| root.join(target))
        .filter(|p| p.is_file());
    if let Some(p) = declared {
        return p;
    }
    for ext in ["tsx", "ts", "jsx", "js"] {
        let p = root.join(format!("src/router.{ext}"));
        if p.is_file() {
            return p;
        }
    }
    root.join("src/router.tsx")
}

/// Vite's `base`: `""` and `./` are relative, anything else an absolute
/// prefix with both slashes.
pub(crate) fn normalize_base(base: Option<&str>) -> String {
    match base.unwrap_or("/") {
        "" | "./" => "./".to_string(),
        "/" => "/".to_string(),
        b => format!("/{}/", b.trim_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_router_entry_prefers_the_declared_import_then_the_extension_ladder() {
        let dir = tempfile_dir();
        let root = dir.as_path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(
            router_entry(root),
            root.join("src/router.tsx"),
            "the default"
        );
        std::fs::write(root.join("src/router.ts"), "").unwrap();
        assert_eq!(router_entry(root), root.join("src/router.ts"));
        std::fs::write(root.join("src/router.tsx"), "").unwrap();
        assert_eq!(router_entry(root), root.join("src/router.tsx"), "tsx first");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/routes.ts"), "").unwrap();
        std::fs::write(
            root.join("package.json"),
            r##"{ "imports": { "#tanstack-router-entry": "./app/routes.ts" } }"##,
        )
        .unwrap();
        assert_eq!(router_entry(root), root.join("./app/routes.ts"));
        std::fs::write(
            root.join("package.json"),
            r##"{ "imports": { "#tanstack-router-entry": "./app/missing.ts" } }"##,
        )
        .unwrap();
        assert_eq!(
            router_entry(root),
            root.join("src/router.tsx"),
            "a declared file that is not there falls back"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_server_fn_resolver_maps_every_id_to_its_handler() {
        let root = tempfile_dir();
        let cache = root.join(".oj-cache/v1/start");
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            root.join("src/server/data.ts"),
            "export const getA = createServerFn().handler(async () => 1);\nexport const getB = createServerFn({ method: \"POST\" }).handler(async () => 2);\n",
        )
        .unwrap();
        std::fs::write(root.join("src/plain.tsx"), "export const x = 1;\n").unwrap();
        std::fs::write(
            root.join("src/server/skip.js"),
            "export const c = createServerFn().handler(() => 3);\n",
        )
        .unwrap();
        assert_eq!(write_server_fn_resolver(&root, &cache).unwrap(), 2);
        let out = std::fs::read_to_string(cache.join("server-fn-resolver.mjs")).unwrap();
        assert!(
            out.contains("import * as __m0 from \"../../../src/server/data.ts\";"),
            "{out}"
        );
        let id = oj_compiler::server_fns::server_fn_id("src/server/data.ts", "getB");
        assert!(
            out.contains(&format!(
                "\"{id}\": {{ functionName: \"getB_createServerFn_handler\", module: __m0 }},"
            )),
            "{out}"
        );
        assert!(!out.contains("skip.js") && !out.contains("plain"), "{out}");
        assert!(out.contains("export async function getServerFnById(id)"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bases_normalize_like_vite() {
        assert_eq!(normalize_base(None), "/");
        assert_eq!(normalize_base(Some("")), "./");
        assert_eq!(normalize_base(Some("./")), "./");
        assert_eq!(normalize_base(Some("app")), "/app/");
        assert_eq!(normalize_base(Some("/app/")), "/app/");
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oj-start-bundle-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod bundle_tests;
