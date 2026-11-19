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
pub(crate) mod inputs;
pub(crate) mod prod;
pub(crate) mod refresh;
pub(crate) mod route_tree;
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
    /// The runtime the bundles run on (see `oj_server::rolldown_runtime`).
    pub rolldown: tokio::runtime::Handle,
    /// The compiled server-function modules ([`code::ServerFnModules`]).
    pub server_fns: code::ServerFnModules,
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

    /// Start's generated route tree (`tanstackStart`'s `srcDirectory` and
    /// `router.generatedRouteTree`; see `oj_config::start_route_tree`).
    pub fn route_tree(&self) -> PathBuf {
        oj_config::start_route_tree(&self.config, &self.root)
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
            self.cache.join(server_fn_resolver_file(false)),
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
        // Later layers win, and an `import.meta.env.X` they define joins the
        // `import.meta.env` object, as Vite's define plugin folds the user's
        // env defines into the object it replaces `import.meta.env` with.
        oj_env::fold_env_keys_into_object(out)
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
        oj_env::fold_env_keys_into_object(out)
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
            condition_names: conditions.map(oj_resolver::rolldown_conditions),
            symlinks,
            extension_alias: Some(oj_resolver::default_extension_alias()),
            ..Default::default()
        }
    }
}

/// Writes `<cache>/server-fn-resolver.mjs`, the module behind
/// `#tanstack-start-server-fn-resolver`: every server function in the
/// modules under `dirs` ([`server_fn_sources`]; the ids the client stubs
/// call) mapped to the handler export the server rewrite adds. Each module
/// loads on first call through a literal relative `import()`, which the dev
/// module host resolves and the server bundle bundles, as Start's own build
/// resolver does (`importer`, memoizing the loaded module): a static import
/// would evaluate every `*.functions.ts` inside the framework's import of the
/// resolver, a cycle back through `@tanstack/react-start` that can run one
/// before its middleware initializes. `stubbed` adds the modules a client
/// compile stubbed (the build); in dev (`dev`) an unmapped id goes to the
/// lazy lookup ([`lazy_server_fn`]) and each function also answers to
/// Start's own Vite dev id; a build maps oj's ids only, as a Vite build knows
/// only its build ids. Each writes its own file (`server_fn_resolver_file`), so a build run
/// beside a dev server never swaps the dev resolver out. How many functions
/// it maps.
pub(crate) fn write_server_fn_resolver(
    root: &Path,
    dirs: &ServerFnDirs,
    stubbed: &[PathBuf],
    cache: &Path,
    dev: bool,
) -> std::io::Result<usize> {
    // Each module once, at the path its stub id is made from: a link and its
    // target are one module.
    let mut files: Vec<PathBuf> = server_fn_sources(dirs)
        .iter()
        .chain(stubbed)
        .map(|path| module_path(path, dirs.preserve_symlinks).unwrap_or_else(|| path.clone()))
        .collect();
    files.sort();
    files.dedup();
    let mut modules = String::new();
    let mut manifest = String::new();
    let mut count = 0;
    let mut module_count = 0;
    let app_modules = oj_compiler::server_fns::FsModules::new(root);
    for path in &files {
        let Ok(code) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let fns = oj_compiler::server_fns::find_server_fns(&code, &rel, &app_modules);
        if fns.is_empty() {
            continue;
        }
        // One importer per file, shared by the file's functions. It memoizes
        // the loaded module, not the promise (Start's `module ??= await
        // importer()`): a file that failed to resolve is retried on the next
        // call, while one that threw while evaluating stays failed until it is
        // edited, as the engine keeps the errored module (Vite's too).
        let var = format!("__m{module_count}");
        module_count += 1;
        let spec = server_fn_module(cache, path);
        modules.push_str(&format!(
            "let {var};\nconst {var}_load = async () => ({var} ??= await import({spec}));\n"
        ));
        // In dev each function answers to oj's id and to the one Start's
        // plugin gives it in Vite dev, which a file imported outside oj's
        // bundles calls under (a browser `import("/src/x.functions.ts")`).
        for f in fns {
            let handler = json(&format!("{}_createServerFn_handler", f.name));
            let mut ids = vec![oj_compiler::server_fns::server_fn_id(&rel, &f.name)];
            if dev {
                ids.push(oj_compiler::server_fns::vite_dev_server_fn_id(
                    &rel, &f.name,
                ));
            }
            for id in ids {
                manifest.push_str(&format!(
                    "  {}: {{ functionName: {handler}, importer: {var}_load }},\n",
                    json(&id),
                ));
            }
            count += 1;
        }
    }
    // Start's build resolver lookup and errors, word for word. The manifest's
    // null prototype is oj's own: Start's plain object lets an id like
    // `constructor` reach an `Object.prototype` member and fail as a TypeError.
    // Start's client-reference check on `access` is left out as Start leaves
    // it out when SSR provides the functions (`includeClientReferencedCheck:
    // !ssrIsProvider`), which React Start always does.
    // In dev an id with no entry is looked up on demand (`LAZY_SERVER_FN`).
    let (lazy, lookup) = if dev {
        (
            format!(
                "async function lazy(id) {{\n  \
                 let mod;\n  \
                 try {{\n    mod = await import({prefix} + id);\n  }} catch (e) {{\n    \
                 if (String(e?.message ?? e).includes({refused})) return undefined;\n    \
                 throw e;\n  }}\n  \
                 const text = Buffer.from(id, \"base64url\").toString(\"utf8\");\n  \
                 const functionName = text.startsWith(\"{{\") ? JSON.parse(text).export : text.slice(text.lastIndexOf(\"#\") + 1) + \"_createServerFn_handler\";\n  \
                 return {{ functionName, importer: async () => mod }};\n}}\n",
                prefix = json(LAZY_SERVER_FN),
                refused = json(LAZY_SERVER_FN_REFUSED),
            ),
            "manifest[id] ?? (await lazy(id))",
        )
    } else {
        (String::new(), "manifest[id]")
    };
    let out = format!(
        "// Generated by oj. Maps server-function ids to their handlers.\n{modules}const manifest = {{\n  __proto__: null,\n{manifest}}};\n{lazy}\
         export async function getServerFnById(id) {{\n  \
         const info = {lookup};\n  \
         if (!info) throw new Error(\"Server function info not found for \" + id);\n  \
         const mod = await info.importer();\n  \
         if (!mod) throw new Error(\"Server function module not resolved for \" + id);\n  \
         const action = mod[info.functionName];\n  \
         if (!action) throw new Error(\"Server function module export not resolved for serverFn ID: \" + id);\n  \
         return action;\n}}\n"
    );
    std::fs::write(cache.join(server_fn_resolver_file(dev)), out)?;
    // The modules the scan read to answer (a builder's module, the modules
    // re-exporting it): an edit to one may change which calls are server
    // functions in a file that was not edited.
    let mut reads = app_modules.reads();
    reads.sort();
    let mut asked: Vec<String> = app_modules
        .asked()
        .into_iter()
        .map(|base| format!("{SERVER_FN_ASKED}{base}"))
        .collect();
    asked.sort();
    reads.extend(asked);
    let mut unresolved: Vec<String> = app_modules
        .unresolved()
        .into_iter()
        .map(|(from, specifier)| format!("{SERVER_FN_UNRESOLVED}{from}\t{specifier}"))
        .collect();
    unresolved.sort();
    reads.extend(unresolved);
    std::fs::write(cache.join(server_fn_reads_file(dev)), reads.join("\n"))?;
    Ok(count)
}

/// The directories the server-fn resolver scans, and the ones inside them
/// it never reads (the build output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerFnDirs {
    pub roots: Vec<PathBuf>,
    pub excluded: Vec<PathBuf>,
    /// `resolve.preserveSymlinks`: a module's id is its path as reached, not
    /// its real path, as the bundler gives the client stub its id.
    pub preserve_symlinks: bool,
}

/// The directories the server-fn resolver scans: `src/` and `candidates`
/// (Start's source and routes directories),
/// each inside the project but never the project itself, outside
/// `node_modules`, hidden directories and `excluded`, and none inside another.
pub(crate) fn server_fn_dirs(
    root: &Path,
    candidates: impl IntoIterator<Item = PathBuf>,
    excluded: &[PathBuf],
) -> ServerFnDirs {
    let excluded: Vec<PathBuf> = excluded
        .iter()
        .map(|e| crate::ssr_dev::normalize_path(e))
        .collect();
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in std::iter::once(root.join("src")).chain(candidates) {
        let dir = crate::ssr_dev::normalize_path(&dir);
        let Ok(rel) = dir.strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty()
            || !is_scanned_dir(rel)
            || excluded.iter().any(|e| dir.starts_with(e))
        {
            continue;
        }
        if !roots.iter().any(|d| dir.starts_with(d)) {
            roots.retain(|d| !d.starts_with(&dir));
            roots.push(dir);
        }
    }
    ServerFnDirs {
        roots,
        excluded,
        preserve_symlinks: false,
    }
}

/// Whether a directory, relative to a scanned one, is scanned: none of it is
/// `node_modules` or hidden.
fn is_scanned_dir(rel: &Path) -> bool {
    rel.components().all(|c| {
        let name = c.as_os_str().to_string_lossy();
        name != "node_modules" && !name.starts_with('.')
    })
}

/// An extension Start's server-fn compiler transforms (`TRANSFORM_ID_REGEX`)
/// that an import may resolve to: Vite's `DEFAULT_EXTENSIONS` without
/// `.json`, plus `.cjs`/`.cts`.
pub(crate) fn is_module(ext: &str) -> bool {
    matches!(
        ext,
        "mjs" | "js" | "mts" | "ts" | "jsx" | "tsx" | "cjs" | "cts"
    )
}

/// Whether `path` is one of the modules the server-fn resolver scans: a
/// module under one of `dirs`, outside `node_modules`, hidden directories and
/// the excluded ones.
pub(crate) fn is_server_fn_source(dirs: &ServerFnDirs, path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_module)
        && !dirs.excluded.iter().any(|e| path.starts_with(e))
        && dirs.roots.iter().any(|dir| {
            path.strip_prefix(dir)
                .is_ok_and(|rel| rel.parent().is_none_or(is_scanned_dir))
        })
}

/// The modules the server-fn resolver scans ([`is_server_fn_source`]),
/// sorted. Symlinked directories are not followed, so a link back into an
/// ancestor ends the walk.
pub(crate) fn server_fn_sources(dirs: &ServerFnDirs) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = dirs.roots.clone();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if is_scanned_dir(Path::new(&entry.file_name())) && !dirs.excluded.contains(&path) {
                    stack.push(path);
                }
            } else if is_server_fn_source(dirs, &path) {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// What a line of the reads file starts with when it names a relative import
/// the scan asked for, app-relative as written (no extension added).
pub(crate) const SERVER_FN_ASKED: &str = "?";

/// What a line of the reads file starts with when it names an import that
/// is not relative and that nothing answered: the importer, a tab, the
/// specifier.
pub(crate) const SERVER_FN_UNRESOLVED: &str = "!";

/// Beside the resolver, the app-relative paths of the modules its scan read
/// for an imported builder or factory, one per line; then, each after
/// [`SERVER_FN_ASKED`], the relative imports it asked for, which another
/// file may come to answer (a file created where there was none, or one
/// with an extension tried earlier); then, each after
/// [`SERVER_FN_UNRESOLVED`], the other imports nothing answered.
pub(crate) fn server_fn_reads_file(dev: bool) -> &'static str {
    if dev {
        "server-fn-resolver.reads"
    } else {
        "server-fn-resolver.build.reads"
    }
}

/// What the dev resolver imports, followed by an id it has no entry for; the
/// module host answers with the declaring module ([`lazy_server_fn`]).
pub(crate) const LAZY_SERVER_FN: &str = "oj-server-fn:";

/// What the module host's refusal of a [`LAZY_SERVER_FN`] id starts with,
/// which no module's own error says.
pub(crate) const LAZY_SERVER_FN_REFUSED: &str = "oj-server-fn: no server function for";

/// The module declaring the function `id` names: one of `compiled` (a
/// module a compile gave server functions), spelled exactly as `id` does.
pub(crate) fn lazy_server_fn(
    root: &Path,
    id: &str,
    preserve_symlinks: bool,
    compiled: &std::collections::BTreeSet<PathBuf>,
) -> Option<PathBuf> {
    use base64::Engine;
    let text = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())?;
    let (spelled, name) = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(start) => {
            let file = start.get("file")?.as_str()?;
            let file = file.strip_suffix("?tss-serverfn-split")?;
            let file = match file.strip_prefix("/@fs") {
                Some(abs) => abs,
                None => file.strip_prefix('/')?,
            };
            let export = start.get("export")?.as_str()?;
            let name = export.strip_suffix("_createServerFn_handler")?;
            (file.to_string(), name.to_string())
        }
        Err(_) => {
            let (rel, name) = text.rsplit_once('#')?;
            (rel.to_string(), name.to_string())
        }
    };
    let real = module_path(&root.join(&spelled), preserve_symlinks)?;
    let rel = real
        .strip_prefix(root)
        .unwrap_or(&real)
        .to_string_lossy()
        .replace('\\', "/");
    if oj_compiler::server_fns::server_fn_id(&rel, &name) != id
        && oj_compiler::server_fns::vite_dev_server_fn_id(&rel, &name) != id
    {
        return None;
    }
    if !real.is_file()
        || !real
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(is_module)
        || real.components().any(|c| c.as_os_str() == "node_modules")
        || !compiled
            .iter()
            .any(|p| *p == real || module_path(p, preserve_symlinks).as_ref() == Some(&real))
    {
        return None;
    }
    let code = std::fs::read_to_string(&real).ok()?;
    let modules = oj_compiler::server_fns::FsModules::new(root);
    oj_compiler::server_fns::find_server_fns(&code, &rel, &modules)
        .iter()
        .any(|f| f.name == name)
        .then_some(real)
}

/// The path a module's ids are made from: its real path, or under
/// `resolve.preserveSymlinks` its normalized path as reached.
fn module_path(path: &Path, preserve_symlinks: bool) -> Option<PathBuf> {
    if preserve_symlinks {
        let path = crate::ssr_dev::normalize_path(path);
        path.is_file().then_some(path)
    } else {
        std::fs::canonicalize(path).ok()
    }
}

/// The resolver's file in the Start cache: dev's, or the build's.
pub(crate) fn server_fn_resolver_file(dev: bool) -> &'static str {
    if dev {
        "server-fn-resolver.mjs"
    } else {
        "server-fn-resolver.build.mjs"
    }
}

/// The specifier the resolver in `cache` imports the module at `path`
/// under, quoted as it is written there.
pub(crate) fn server_fn_module(cache: &Path, path: &Path) -> String {
    json(&oj_compiler::glob::relative_import(cache, path))
}

fn json(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// The framework's server-function URL prefix (`TSS_SERVER_FN_BASE`).
pub(crate) const SERVER_FN_BASE: &str = "/_serverFn/";

/// The client's route tree generate.mjs writes in the cache dir (see
/// [`route_tree::ClientRouteTree`]).
pub(crate) const CLIENT_ROUTE_TREE: &str = "route-tree.client.js";

/// What the route generator said about the app's routes, while no
/// generation of this dev server has made a tree of them: the client bundle
/// fails with it (see `DevBundler::bundle_async`). Held by the dev server,
/// not in the cache directory, which an `oj build` of the same app shares.
pub(crate) type RouteTreeRefusal = std::sync::Arc<std::sync::Mutex<Option<String>>>;

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
            root.join("src/server/more.js"),
            "export const c = createServerFn().handler(() => 3);\n",
        )
        .unwrap();
        assert_eq!(
            write_server_fn_resolver(&root, &server_fn_dirs(&root, [], &[]), &[], &cache, true)
                .unwrap(),
            3
        );
        let out = std::fs::read_to_string(cache.join("server-fn-resolver.mjs")).unwrap();
        // Loaded on the first call, never imported statically: a static import
        // runs the module inside the framework's import of the resolver.
        assert!(!out.contains("import *"), "{out}");
        // The memo holds the awaited module, so a load that failed to resolve is
        // not kept.
        assert!(
            out.contains(
                "const __m0_load = async () => (__m0 ??= await import(\"../../../src/server/data.ts\"));"
            ),
            "{out}"
        );
        // Both functions of the file share its one importer: one per file.
        assert_eq!(out.matches("await import(\"../").count(), 2, "{out}");
        // Under oj's id and Start's Vite dev id alike.
        for id in [
            oj_compiler::server_fns::server_fn_id("src/server/data.ts", "getB"),
            oj_compiler::server_fns::vite_dev_server_fn_id("src/server/data.ts", "getB"),
        ] {
            assert!(
                out.contains(&format!(
                    "\"{id}\": {{ functionName: \"getB_createServerFn_handler\", importer: __m0_load }},"
                )),
                "{out}"
            );
        }
        // Only the manifest's own keys are ids, never `Object.prototype` names.
        assert!(
            out.contains("const manifest = {\n  __proto__: null,\n"),
            "{out}"
        );
        // In dev an id with no entry is looked up on demand.
        assert!(
            out.contains("const info = manifest[id] ?? (await lazy(id));"),
            "{out}"
        );
        assert!(
            out.contains("await import(\"oj-server-fn:\" + id)"),
            "{out}"
        );
        // Only the host's refusal is a miss, never a module's own error.
        assert!(
            out.contains(".includes(\"oj-server-fn: no server function for\")) return undefined;"),
            "{out}"
        );
        // Each failure throws Start's build resolver error for that step.
        for error in [
            "if (!info) throw new Error(\"Server function info not found for \" + id);",
            "if (!mod) throw new Error(\"Server function module not resolved for \" + id);",
            "if (!action) throw new Error(\"Server function module export not resolved for serverFn ID: \" + id);",
        ] {
            assert!(out.contains(error), "{error}\n{out}");
        }
        // A `.js` module is mapped as a `.ts` one is.
        let id = oj_compiler::server_fns::server_fn_id("src/server/more.js", "c");
        assert!(
            out.contains(&format!(
                "\"{id}\": {{ functionName: \"c_createServerFn_handler\", importer: __m1_load }},"
            )),
            "{out}"
        );
        assert!(!out.contains("plain"), "{out}");
        assert!(out.contains("export async function getServerFnById(id)"));
        // A build maps oj's ids only: a Vite build knows no dev id.
        // Into its own file: the dev resolver stays as it was.
        assert_eq!(
            write_server_fn_resolver(&root, &server_fn_dirs(&root, [], &[]), &[], &cache, false)
                .unwrap(),
            3
        );
        let dev_out = std::fs::read_to_string(cache.join("server-fn-resolver.mjs")).unwrap();
        assert_eq!(dev_out, out);
        let out = std::fs::read_to_string(cache.join("server-fn-resolver.build.mjs")).unwrap();
        let id = oj_compiler::server_fns::server_fn_id("src/server/data.ts", "getB");
        assert!(out.contains(&format!("\"{id}\": ")), "{out}");
        let dev_id = oj_compiler::server_fns::vite_dev_server_fn_id("src/server/data.ts", "getB");
        assert!(!out.contains(&dev_id), "{out}");
        assert!(out.contains("const info = manifest[id];"), "{out}");
        assert!(!out.contains("oj-server-fn:"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_server_fn_id_with_no_entry_resolves_only_to_the_module_declaring_it() {
        let base = tempfile_dir().canonicalize().unwrap();
        let root = base.join("app");
        let fns = "import { createServerFn } from \"@tanstack/react-start\";\nexport const x = createServerFn().handler(async () => 1);\n";
        for (rel, code) in [
            ("app/server/x.functions.ts", fns),
            ("app/src/plain.ts", "export const x = () => 1;\n"),
            ("app/node_modules/pkg/x.functions.ts", fns),
            ("shared/x.functions.ts", fns),
            ("app/src/x.functions.json", fns),
        ] {
            std::fs::create_dir_all(base.join(rel).parent().unwrap()).unwrap();
            std::fs::write(base.join(rel), code).unwrap();
        }
        std::os::unix::fs::symlink(base.join("shared"), root.join("linked")).unwrap();
        let oj = oj_compiler::server_fns::server_fn_id;
        let start = oj_compiler::server_fns::vite_dev_server_fn_id;
        let json_id = |json: &str| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        };
        // The well-formed Start id is accepted, so the variants below are
        // refused for their form alone.
        assert_eq!(
            json_id(
                r#"{"file":"/server/x.functions.ts?tss-serverfn-split","export":"x_createServerFn_handler"}"#
            ),
            start("server/x.functions.ts", "x")
        );
        let outside = base.join("shared/x.functions.ts");
        let outside_rel = outside.to_string_lossy().into_owned();
        // The modules a compile gave server functions; `other/` declares the
        // same function and no compile reached it.
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("other/x.functions.ts"), fns).unwrap();
        let compiled: std::collections::BTreeSet<PathBuf> = [
            root.join("server/x.functions.ts"),
            outside.clone(),
            root.join("node_modules/pkg/x.functions.ts"),
            root.join("src/x.functions.json"),
            root.join("linked/x.functions.ts"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            lazy_server_fn(&root, &oj("other/x.functions.ts", "x"), false, &compiled),
            None
        );
        // In the root, outside it, under oj's id and Start's dev id.
        for (id, file) in [
            (
                oj("server/x.functions.ts", "x"),
                root.join("server/x.functions.ts"),
            ),
            (
                start("server/x.functions.ts", "x"),
                root.join("server/x.functions.ts"),
            ),
            (oj(&outside_rel, "x"), outside.clone()),
            (start(&outside_rel, "x"), outside.clone()),
        ] {
            assert_eq!(
                lazy_server_fn(&root, &id, false, &compiled),
                Some(file),
                "{id}"
            );
        }
        // Refused: a function the file does not declare, a file declaring
        // none, any spelling but the real path's (a traversal, an absolute
        // path inside the root, a symlink), `node_modules`, a non-module, a
        // missing file, an id that is no id.
        for id in [
            oj("server/x.functions.ts", "y"),
            oj("src/plain.ts", "x"),
            oj("src/../server/x.functions.ts", "x"),
            oj("../shared/x.functions.ts", "x"),
            oj(&root.join("server/x.functions.ts").to_string_lossy(), "x"),
            oj("linked/x.functions.ts", "x"),
            oj("node_modules/pkg/x.functions.ts", "x"),
            oj("src/x.functions.json", "x"),
            oj("server/missing.functions.ts", "x"),
            start("../shared/x.functions.ts", "x"),
            "not base64!".to_string(),
            // Padded, percent-encoded, backslashes, a NUL.
            format!("{}==", oj("server/x.functions.ts", "x")),
            oj("server/x.functions.ts", "x").replacen('c', "%63", 1),
            oj("server\\x.functions.ts", "x"),
            oj("server/x.functions.ts\0", "x"),
            // `#` or `?` inside the path.
            oj("server/x.functions.ts#y", "x"),
            oj("server/x.functions.ts?y", "x"),
            // Start's form: `/@id/`, keys reordered or added, another query.
            json_id(
                r#"{"file":"/@id/server/x.functions.ts?tss-serverfn-split","export":"x_createServerFn_handler"}"#,
            ),
            json_id(
                r#"{"export":"x_createServerFn_handler","file":"/server/x.functions.ts?tss-serverfn-split"}"#,
            ),
            json_id(
                r#"{"file":"/server/x.functions.ts?tss-serverfn-split","export":"x_createServerFn_handler","x":1}"#,
            ),
            json_id(
                r#"{"file":"/server/x.functions.ts?x&tss-serverfn-split","export":"x_createServerFn_handler"}"#,
            ),
        ] {
            assert_eq!(lazy_server_fn(&root, &id, false, &compiled), None, "{id}");
        }
        // Under `resolve.preserveSymlinks` a module's id is its path as
        // reached: the link's spelling, never a traversal.
        let linked = root.join("linked/x.functions.ts");
        assert_eq!(
            lazy_server_fn(&root, &oj("linked/x.functions.ts", "x"), true, &compiled),
            Some(linked)
        );
        for id in [
            oj("src/../server/x.functions.ts", "x"),
            oj("linked/../server/x.functions.ts", "x"),
            oj("server/x.functions.ts", "y"),
        ] {
            assert_eq!(lazy_server_fn(&root, &id, true, &compiled), None, "{id}");
        }
        let _ = std::fs::remove_dir_all(&base);
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
