// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The source rewrites a Start bundle applies to the app's code, in the
//! order Vite's plugins apply them: `new URL(..., import.meta.url)` (client
//! bundles), `import.meta.glob`, and TanStack's `createServerFn` split (the
//! RPC stub in the browser, the provider export on the server).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oj_compiler::glob::GlobCall;

use super::facts::AliasEntry;
use oj_compiler::meta_glob::{self, MetaUrlTarget};
use rolldown_plugin::{
    HookTransformArgs, HookTransformOutput, HookTransformReturn, Plugin,
    PluginContextResolveOptions, SharedTransformPluginContext,
};

/// One `import.meta.glob` a bundle expanded, with the module that holds it
/// (glob-scopes.json): the dev server re-expands the importer when a file the
/// call matches is created or deleted, as Vite's import-glob plugin adds the
/// glob's importers to such an update (its `hotUpdate`).
#[derive(Debug, Clone)]
pub(crate) struct GlobScope {
    pub importer: PathBuf,
    pub call: GlobCall,
}

/// Which bundle the code goes to.
#[derive(Debug, Clone)]
pub(crate) enum CodeTarget {
    /// The browser client: URL and glob rewrites everywhere, the server-fn
    /// stub in app code.
    Client,
    /// The server bundle: globs and the server-fn provider in app code.
    Server,
    /// A worker bundle whose entry is `entry`: URL and glob rewrites
    /// everywhere, a worker starting itself getting `self.location.href`.
    Worker { entry: PathBuf },
}

pub(crate) struct StartCodePlugin {
    root: PathBuf,
    target: CodeTarget,
    /// Every glob a client bundle expanded, with its importer: the dev
    /// server's rebundle gate (a file created or deleted that one matches
    /// changes the bundle) and its server graphs (the importer re-expands).
    glob_scopes: Option<Arc<Mutex<Vec<GlobScope>>>>,
    /// The app's `resolve.alias`, which an aliased glob pattern resolves
    /// through (Vite's toAbsoluteGlob -> resolveId).
    alias: Vec<AliasEntry>,
    /// The plugin host's transform runs [`StartCodePlugin::compile`] in its
    /// slot (between the plugins' "pre" and "normal" transforms), so this
    /// plugin leaves it out.
    compiled_in_host: bool,
    /// The plugin host runs Start's code splitter on this bundle's routes,
    /// and so deletes the client's server-only route options itself.
    routes_split_in_host: bool,
    /// The generated route tree (`StartConfig::route_tree`).
    route_tree: PathBuf,
    /// The route files the generated route tree imports (extensionless), by
    /// the tree's modification time.
    route_files: Mutex<Option<(std::time::SystemTime, HashSet<PathBuf>)>>,
    warned: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for StartCodePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartCodePlugin")
    }
}

impl StartCodePlugin {
    pub fn new(root: &Path, target: CodeTarget) -> Self {
        Self {
            root: root.to_path_buf(),
            target,
            glob_scopes: None,
            alias: Vec::new(),
            compiled_in_host: false,
            routes_split_in_host: false,
            route_tree: root.join("src").join("routeTree.gen.ts"),
            route_files: Mutex::new(None),
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// The route tree the app's Start generates, when not Start's default.
    pub fn with_route_tree(mut self, tree: PathBuf) -> Self {
        self.route_tree = tree;
        self
    }

    pub fn with_alias(mut self, alias: Vec<AliasEntry>) -> Self {
        self.alias = alias;
        self
    }

    pub fn compiled_in_host(mut self, yes: bool) -> Self {
        self.compiled_in_host = yes;
        self
    }

    pub fn routes_split_in_host(mut self, yes: bool) -> Self {
        self.routes_split_in_host = yes;
        self
    }

    pub fn recording_globs(mut self, scopes: Arc<Mutex<Vec<GlobScope>>>) -> Self {
        self.glob_scopes = Some(scopes);
        self
    }

    fn warn_once(&self, message: String) {
        if self.warned.lock().unwrap().insert(message.clone()) {
            eprintln!("{message}");
        }
    }
}

/// The files a generated route tree imports (`import { Route as X } from
/// "./routes/x"`), extensionless and absolute.
fn route_tree_imports(tree: &Path, text: &str) -> HashSet<PathBuf> {
    let dir = tree.parent().unwrap_or(tree);
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("import ") {
                return None;
            }
            let (_, rest) = line.rsplit_once(" from ")?;
            let spec = rest.trim().trim_end_matches(';').trim_matches(['\'', '"']);
            let rel = spec.strip_prefix("./")?;
            let path = dir.join(rel);
            // An import naming its extension (`./routes/x.tsx`).
            let stem = match path.extension().and_then(|e| e.to_str()) {
                Some("ts" | "tsx" | "js" | "jsx" | "mjs") => path.with_extension(""),
                _ => path,
            };
            Some(stem)
        })
        .collect()
}

/// App source the server-fn and glob rewrites own: not a dependency, not a
/// virtual module, a script.
fn is_app_code(id: &str) -> bool {
    !id.contains("/node_modules/")
        && !id.starts_with('\0')
        && matches!(
            Path::new(id.split('?').next().unwrap_or(id))
                .extension()
                .and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs")
        )
}

fn is_script(id: &str) -> bool {
    !id.starts_with('\0')
        && matches!(
            Path::new(id.split('?').next().unwrap_or(id))
                .extension()
                .and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts")
        )
}

impl Plugin for StartCodePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-code")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Transform
    }

    async fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let id = args.id;
        if !is_script(id) {
            return Ok(None);
        }
        let app = is_app_code(id);
        let file = PathBuf::from(id.split('?').next().unwrap_or(id));
        let original = args.code.as_str();
        let mut code: Cow<str> = Cow::Borrowed(original);

        let rewrites_urls = !matches!(self.target, CodeTarget::Server);
        let mut urls_changed = false;
        if rewrites_urls && code.contains("import.meta.url") {
            let sites =
                meta_glob::scan_import_meta_urls(&code, id).map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut targets = Vec::with_capacity(sites.len());
            for site in &sites {
                let target = match site.kind.file() {
                    None => MetaUrlTarget::Unresolved,
                    Some(spec) => match self.resolve_url_file(&ctx, spec, id).await? {
                        Some(abs) => match &self.target {
                            CodeTarget::Worker { entry } if *entry == abs => {
                                MetaUrlTarget::SelfWorker
                            }
                            _ => MetaUrlTarget::File(abs),
                        },
                        None => {
                            // Vite's warning for a file that is not there; a
                            // `/` URL is a public file and a scheme no file.
                            if !spec.starts_with('/') && !meta_glob::has_scheme(spec) {
                                self.warn_once(format!(
                                    "{} doesn't exist at build time, it will remain unchanged to be resolved at runtime. If this is intended, you can use the /* @vite-ignore */ comment to suppress this warning.",
                                    &code[site.start..site.end]
                                ));
                            }
                            MetaUrlTarget::Unresolved
                        }
                    },
                };
                targets.push(target);
            }
            let out = meta_glob::apply_import_meta_urls(&code, &sites, &targets);
            if out != code {
                urls_changed = true;
                code = Cow::Owned(out);
            }
        }

        // App code always expands its globs; a dependency only the ones a URL
        // rewrite produced (Vite runs the glob plugin on every module, and a
        // dependency's own globs come prebuilt).
        if (app || urls_changed || matches!(self.target, CodeTarget::Worker { .. }))
            && code.contains("import.meta.glob")
        {
            let mut scopes = Vec::new();
            let alias = |id: &str| self.alias.iter().find_map(|e| e.apply(id));
            let out = meta_glob::transform_glob(&code, &file, &self.root, &alias, &mut scopes)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            // What a glob expands to is read off the disk, not off this
            // module's code: a file created or deleted under a pattern's scan
            // base (every file it can match lives under one) changes the
            // expansion. The directory is this module's watch dependency, as
            // a Rollup plugin's `this.addWatchFile(dir)`: the engine watches
            // it with everything below and re-transforms this module for a
            // change there, which Vite's import-glob plugin does from its
            // `hotUpdate` hook over the watcher of the whole root.
            for base in scopes
                .iter()
                .map(GlobCall::matcher)
                .flat_map(|m| m.bases().map(str::to_string).collect::<Vec<_>>())
            {
                ctx.add_watch_file(&base);
            }
            if let Some(record) = &self.glob_scopes {
                record
                    .lock()
                    .unwrap()
                    .extend(scopes.into_iter().map(|call| GlobScope {
                        importer: file.clone(),
                        call,
                    }));
            }
            if out != code {
                code = Cow::Owned(out);
            }
        }

        if !self.compiled_in_host {
            if let Some(out) = self.compile(&ctx, &code, id).await? {
                code = Cow::Owned(out);
            }
        }

        Ok(match code {
            Cow::Borrowed(_) => None,
            Cow::Owned(out) => {
                // The rewrites keep every line: a line map, as Vite's own
                // (and Start's compiler) return one.
                let map = (out.matches('\n').count() == original.matches('\n').count())
                    .then(|| oj_compiler::line_map_json(id, original, &out))
                    .and_then(|json| {
                        oxc_sourcemap::SourceMap::from_json_string(&json)
                            .ok()
                            .map(|m| m.into_owned())
                    })
                    .map_or(rolldown_plugin::HookTransformOutputMap::Omitted, Into::into);
                Some(HookTransformOutput {
                    code: Some(out),
                    map,
                    ..Default::default()
                })
            }
        })
    }
}

impl StartCodePlugin {
    /// Whether [`StartCodePlugin::compile`] can change `code` at all: its
    /// passes all start from a factory, `createServerFn` or (on the client) a
    /// route factory call named in the source, so a module naming none takes
    /// the plugins' whole chain in one call.
    pub(crate) fn may_compile(&self, id: &str, code: &str) -> bool {
        is_script(id)
            && (oj_compiler::env_fns::has_factory_name(code)
                || code.contains("createServerFn")
                || (matches!(self.target, CodeTarget::Client)
                    && oj_compiler::route_options::names_a_route_factory(code)))
    }

    /// TanStack's start-compiler (`tanstack-start-core::server-fn:<env>`, an
    /// `enforce: "pre"` plugin): the environment functions compiled per
    /// environment in every module, its own packages included, so the other
    /// side's code is not bundled (a worker runs in the browser), then the
    /// `createServerFn` split in app code. The rewritten code, or None.
    pub(crate) async fn compile(
        &self,
        ctx: &SharedTransformPluginContext,
        code: &str,
        id: &str,
    ) -> anyhow::Result<Option<String>> {
        if !is_script(id) {
            return Ok(None);
        }
        let file = PathBuf::from(id.split('?').next().unwrap_or(id));
        let mut code: Cow<str> = Cow::Borrowed(code);
        let env = match self.target {
            CodeTarget::Server => oj_compiler::env_fns::Env::Server,
            CodeTarget::Client | CodeTarget::Worker { .. } => oj_compiler::env_fns::Env::Client,
        };
        let path = file.to_string_lossy();
        let reexported = self.reexported_factories(ctx, &code, &path, id).await?;
        let instances = self.start_instances(ctx, &code, &path, id).await?;
        if let Some(out) =
            oj_compiler::env_fns::rewrite_env_fns_with(&code, &path, env, &reexported, &instances)
                .map_err(|e| anyhow::anyhow!("{id}: {e}"))?
        {
            code = Cow::Owned(out);
        }

        if is_app_code(id) && code.contains("createServerFn") {
            let rel = file
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| file.to_string_lossy().into_owned());
            let rewritten = match self.target {
                CodeTarget::Client => oj_compiler::server_fns::rewrite_client(&code, &rel)
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
                CodeTarget::Server => oj_compiler::server_fns::rewrite_server(&code, &rel),
                CodeTarget::Worker { .. } => None,
            };
            if let Some(out) = rewritten {
                code = Cow::Owned(out);
            }
        }

        // Start's router plugin then runs the code splitter's reference
        // compile on the client's route files with `deleteNodes: ["ssr",
        // "server", "headers"]`: a route's server handlers, and what only
        // they used, never reach the browser. Where a plugin host runs Start's
        // own splitter, it deletes them itself, after it has counted what
        // they share with the component (`computeSharedBindings` on the whole
        // route): deleting them here first would split the route differently.
        // A bundle with no host to run it (no Vite config) has oj delete
        // them.
        if matches!(self.target, CodeTarget::Client)
            && !self.routes_split_in_host
            && is_app_code(id)
            && !id.contains('?')
            && oj_compiler::route_options::names_a_route_factory(&code)
            && self.is_route_file(&file)
        {
            if let Some(out) = oj_compiler::route_options::delete_route_options(
                &code,
                &path,
                oj_compiler::route_options::CLIENT_DELETED_ROUTE_OPTIONS,
            ) {
                code = Cow::Owned(out);
            }
        }
        Ok(match code {
            Cow::Borrowed(_) => None,
            Cow::Owned(out) => Some(out),
        })
    }

    /// Whether `file` is a route the generator knows (the code splitter's
    /// `routesByFile`): one the generated route tree imports.
    fn is_route_file(&self, file: &Path) -> bool {
        let tree = &self.route_tree;
        let Ok(modified) = std::fs::metadata(tree).and_then(|m| m.modified()) else {
            return false;
        };
        let mut cache = self.route_files.lock().unwrap();
        if cache.as_ref().map(|(at, _)| *at) != Some(modified) {
            let text = std::fs::read_to_string(tree).unwrap_or_default();
            *cache = Some((modified, route_tree_imports(tree, &text)));
        }
        let stem = file.with_extension("");
        cache
            .as_ref()
            .is_some_and(|(_, files)| files.contains(&stem))
    }

    /// Vite's resolution of a URL's file: a relative one through the bundler
    /// from the importer; a `/` one from the project root (else a public file,
    /// left as written); any other with preferRelative, the importer's
    /// directory before a package.
    async fn resolve_url_file(
        &self,
        ctx: &SharedTransformPluginContext,
        file: &str,
        importer: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        if meta_glob::has_scheme(file) {
            return Ok(None);
        }
        if let Some(rest) = file.strip_prefix('/') {
            let abs = self.root.join(rest);
            return Ok(abs.is_file().then_some(abs));
        }
        let resolve = |spec: String| self.resolve_file(ctx, spec, importer);
        if file.starts_with('.') {
            return resolve(file.to_string()).await;
        }
        if let Some(found) = resolve(format!("./{file}")).await? {
            return Ok(Some(found));
        }
        resolve(file.to_string()).await
    }

    /// The file `spec` resolves to from `importer`, when it is one on disk.
    async fn resolve_file(
        &self,
        ctx: &SharedTransformPluginContext,
        spec: String,
        importer: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        let r = ctx
            .inner
            .resolve(
                &spec,
                Some(importer),
                Some(PluginContextResolveOptions {
                    skip_self: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(r.ok().and_then(|r| {
            let id = r.id.as_str();
            (!id.starts_with('\0')
                && matches!(r.external, rolldown_common::ResolvedExternal::Bool(false)))
            .then(|| PathBuf::from(id.split(['?', '#']).next().unwrap_or(id)))
        }))
    }

    /// The imports of a factory name from outside `@tanstack/*` whose module
    /// re-exports TanStack's, followed module to module as TanStack resolves
    /// them.
    async fn reexported_factories(
        &self,
        ctx: &SharedTransformPluginContext,
        code: &str,
        file: &str,
        importer: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        use oj_compiler::env_fns::{foreign_factory_imports, reexport_of, ReExport};
        self.imports_declaring(
            ctx,
            foreign_factory_imports(code, file),
            importer,
            |text, path, export| match reexport_of(text, path, export) {
                Some(ReExport::TanStack) => Declares::Yes,
                Some(via) => Declares::Via(via),
                None => Declares::No,
            },
        )
        .await
    }

    /// The imports the module calls `createMiddleware` on whose module
    /// declares the app's Start instance (`createStart(..)`), followed the
    /// same way: TanStack's compiler resolves `startInstance.createMiddleware()`
    /// through the binding to `createStart`.
    async fn start_instances(
        &self,
        ctx: &SharedTransformPluginContext,
        code: &str,
        file: &str,
        importer: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        use oj_compiler::env_fns::{
            declares_start_instance, instance_imports, reexport_of, ReExport,
        };
        self.imports_declaring(
            ctx,
            instance_imports(code, file),
            importer,
            |text, path, export| {
                if declares_start_instance(text, path, export) {
                    return Declares::Yes;
                }
                match reexport_of(text, path, export) {
                    Some(ReExport::TanStack) | None => Declares::No,
                    Some(via) => Declares::Via(via),
                }
            },
        )
        .await
    }

    /// The `(source, name)` imports among `imports` whose module `declares`
    /// the name, directly or through the modules it re-exports it from.
    async fn imports_declaring(
        &self,
        ctx: &SharedTransformPluginContext,
        imports: Vec<(String, String)>,
        importer: &str,
        declares: impl Fn(&str, &str, &str) -> Declares,
    ) -> anyhow::Result<Vec<(String, String)>> {
        use oj_compiler::env_fns::ReExport;
        let mut out = Vec::new();
        for (source, name) in imports {
            let mut queue = vec![(source.clone(), name.clone(), importer.to_string())];
            let mut seen = HashSet::new();
            while let Some((spec, export, from)) = queue.pop() {
                if seen.len() >= 32 || !seen.insert((spec.clone(), export.clone(), from.clone())) {
                    continue;
                }
                let Some(path) = self.resolve_file(ctx, spec, &from).await? else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let path = path.to_string_lossy().into_owned();
                match declares(&text, &path, &export) {
                    Declares::Yes => {
                        out.push((source, name));
                        break;
                    }
                    Declares::Via(ReExport::From { source, name }) => {
                        queue.push((source, name, path))
                    }
                    Declares::Via(ReExport::Stars(sources)) => queue.extend(
                        sources
                            .into_iter()
                            .map(|s| (s, export.clone(), path.clone())),
                    ),
                    Declares::Via(ReExport::TanStack) | Declares::No => {}
                }
            }
        }
        Ok(out)
    }
}

/// Whether a module declares what an import asks of it, or where it
/// re-exports it from.
enum Declares {
    Yes,
    No,
    Via(oj_compiler::env_fns::ReExport),
}

#[cfg(test)]
mod tests {
    use super::*;

    // The code splitter's routesByFile, read off the generated tree: every
    // route import, a dotted file name and a footer import naming its
    // extension included.
    #[test]
    fn the_route_files_are_the_trees_imports() {
        let tree = Path::new("/app/src/routeTree.gen.ts");
        let text = [
            "import { Route as rootRouteImport } from './routes/__root'",
            "import { Route as ApiHookRouteImport } from './routes/api/public/hook'",
            "import { Route as ConsentRouteImport } from './routes/[.]well-known.consent'",
            "import type { getRouter } from './router.tsx'",
            "import { createStart } from \"@tanstack/react-start\";",
            "const x = 1",
        ]
        .join("\n");
        let files = route_tree_imports(tree, &text);
        for want in [
            "/app/src/routes/__root",
            "/app/src/routes/api/public/hook",
            "/app/src/routes/[.]well-known.consent",
            "/app/src/router",
        ] {
            assert!(files.contains(Path::new(want)), "{want}: {files:?}");
        }
        assert_eq!(files.len(), 4, "{files:?}");
    }

    #[test]
    fn app_code_is_the_apps_scripts() {
        assert!(is_app_code("/app/src/a.tsx"));
        assert!(is_app_code("/app/src/a.ts?tsr-split=x"));
        assert!(!is_app_code("/app/node_modules/x/a.js"));
        assert!(!is_app_code("\0virtual"));
        assert!(!is_app_code("/app/src/a.css"));
        assert!(is_script("/app/node_modules/x/a.cjs"));
    }
}
