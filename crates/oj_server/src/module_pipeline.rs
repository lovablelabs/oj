use super::*;
use std::cell::RefCell;
use std::time::SystemTime;

/// A file's mtime and size: the warm-path key into `mtime_keys`.
type FileStamp = (SystemTime, u64);

type Compiled = (String, Arc<CachedModule>);

pub(crate) async fn ensure_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<(String, Arc<CachedModule>), String> {
    let ext = file.extension().and_then(|e| e.to_str());
    let react_svg = ext == Some("svg")
        && url
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv == "react"));
    let is_svelte = ext == Some("svelte");
    // A plain `.svg` with a transform plugin active may be componentized (svgr):
    // route it through the transform pipeline; unmatched svg falls back to a URL asset.
    let svgr_candidate = !react_svg
        && state.plugins_have_transform
        && ext == Some("svg")
        && query_asset_kind(url.split_once('?').map(|(_, q)| q)).is_none();

    if !react_svg && !svgr_candidate && is_asset_path(file) {
        return Ok(register_uncached(state, url, url_asset_module(url)));
    }

    let stamp = file_stamp(file).await;
    if let Some(hit) = warm_hit(state, url, stamp) {
        return Ok(hit);
    }

    let is_dep = is_dep_module(url, file);
    let source = load_source(state, file, url, is_dep).await?;
    record_glob_importer(state, file, url, &source);
    if ext == Some("css") && is_tailwind_css(&source) {
        let css = compile_tailwind(state, url, &source).await?;
        let module = CachedModule {
            is_boundary: true,
            kind: "css".into(),
            ..code_module(css)
        };
        return Ok(register_uncached(state, url, module));
    }

    let is_server = is_server_module(file) && !is_dep;
    let CompileKey {
        key,
        class_field_semantics,
    } = compile_key(state, file, url, &source, is_server, stamp);

    if let Some(module) = memory_hit(state, url, &key) {
        return Ok((key, module));
    }

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(url.to_string()).or_default())
    };
    let _guard = lock.lock().await;

    if let Some(module) = memory_hit(state, url, &key) {
        return Ok((key, module));
    }
    if let Some(module) = persistent_hit(state, &key, is_dep) {
        memory_put(state, url, &key, &module);
        register_in_graph(state, url, &module);
        replay_module_parsed(state, file, &key, is_dep, is_server).await;
        return Ok((key, module));
    }

    if is_server {
        let code = server_fn_stub(&oj_compiler::exports(&source, file), url);
        let module = Arc::new(code_module(code));
        store_module(state, url, &key, &module);
        return Ok((key, module));
    }

    let transformed = plugin_transform(state, file, url, source, is_dep).await?;
    let source = if is_preprocessor(url) {
        preprocess_less_stylus(state, url, transformed.code).await?
    } else {
        transformed.code
    };
    let mut css = CssState::default();
    let source = css_prepass(state, file, url, source, &mut css).await?;

    let source = if react_svg {
        svgr::svg_to_component(&source)
    } else {
        source
    };
    // An svg candidate still raw after the plugin transform was not matched by svgr:
    // serve it as a URL asset like Vite; otherwise compile it as `.svg.tsx` below.
    let svgr_componentized = svgr_candidate && !source.trim_start().starts_with('<');
    if svgr_candidate && !svgr_componentized {
        return Ok(register_uncached(state, url, url_asset_module(url)));
    }
    let source = if is_svelte {
        run_svelte_engine(state, url, &source)
            .await
            .map_err(|e| format!("svelte compile error for {url}: {e}"))?
    } else {
        source
    };

    let compile_file = if react_svg || svgr_componentized {
        file.with_extension("svg.tsx")
    } else if is_svelte {
        file.with_extension("svelte.js")
    } else {
        file.to_path_buf()
    };
    let kind = match ext {
        Some("json") => CompileKind::Json,
        Some(e) if is_style_ext(e) => CompileKind::Css,
        _ => CompileKind::Script,
    };
    let dep_map = match kind {
        CompileKind::Script => state.optimized.ready().await,
        _ => Arc::new(optimize::DepMap::new()),
    };
    let job = CompileJob {
        state: Arc::clone(state),
        kind,
        file: compile_file,
        importer_abs: file.to_string_lossy().into_owned(),
        dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
        url: url.to_string(),
        source,
        is_dep,
        is_server,
        is_svelte,
        class_field_semantics,
        css,
        plugin_maps: transformed.maps,
        dep_map,
    };
    let compiled = tokio::task::spawn_blocking(move || job.run()).await;

    let module = match compiled {
        Ok(Ok(mut module)) => {
            module.watch_files = transformed.watch_files;
            Arc::new(module)
        }
        Ok(Err(err)) => {
            if is_unresolved_import_error(&err) {
                let clean = url.split('?').next().unwrap_or(url).to_string();
                state.resolve_failed.lock().unwrap().insert(clean);
                // Vite clears the importer's isSelfAccepting here (#9534) so the update
                // a later `create` triggers climbs to a boundary the page did load.
                state
                    .graph
                    .lock()
                    .unwrap()
                    .set_self_accepting(Path::new(url), false);
            }
            return Err(err);
        }
        Err(join_err) => return Err(format!("compiler task failed: {join_err}")),
    };
    store_module(state, url, &key, &module);
    state
        .resolve_failed
        .lock()
        .unwrap()
        .remove(url.split('?').next().unwrap_or(url));
    if state.plugins_use_module_parsed && !is_dep && !is_server {
        state.parsed_fired.lock().unwrap().insert(key.clone());
    }
    Ok((key, module))
}

/// A module with just `code`; every other field empty.
fn code_module(code: String) -> CachedModule {
    CachedModule {
        is_boundary: false,
        hot: None,
        kind: String::new(),
        code,
        map_json: None,
        imports: Vec::new(),
        require_map: Vec::new(),
        css_exports: Vec::new(),
        fs_allow: Vec::new(),
        watch_files: Vec::new(),
        import_bindings: Vec::new(),
    }
}

/// `export default "<url>"`: how a non-compiled asset import is served.
fn url_asset_module(url: &str) -> CachedModule {
    let clean = url.split('?').next().unwrap_or(url);
    code_module(format!(
        "export default {};\n",
        serde_json::Value::String(clean.to_string())
    ))
}

fn register_uncached(state: &ServerState, url: &str, module: CachedModule) -> Compiled {
    let module = Arc::new(module);
    register_in_graph(state, url, &module);
    (String::new(), module)
}

fn memory_hit(state: &ServerState, url: &str, key: &str) -> Option<Arc<CachedModule>> {
    let module = memory_get(state, url, key)?;
    register_in_graph(state, url, &module);
    Some(module)
}

fn store_module(state: &ServerState, url: &str, key: &str, module: &Arc<CachedModule>) {
    if state.persistent_cache {
        let _ = state
            .cache_writes
            .try_send((key.to_string(), Arc::clone(module)));
    }
    memory_put(state, url, key, module);
    register_in_graph(state, url, module);
}

async fn file_stamp(file: &Path) -> Option<FileStamp> {
    let meta = tokio::fs::metadata(file).await.ok()?;
    meta.modified().ok().map(|mtime| (mtime, meta.len()))
}

/// The memory-cached module for an unchanged file, found without reading it.
fn warm_hit(state: &ServerState, url: &str, stamp: Option<FileStamp>) -> Option<Compiled> {
    let (mtime, size) = stamp?;
    let key = state
        .mtime_keys
        .lock()
        .unwrap()
        .get(url)
        .filter(|(t, s, _)| *t == mtime && *s == size)
        .map(|(_, _, k)| k.clone())?;
    let module = memory_hit(state, url, &key)?;
    Some((key, module))
}

/// The plugin hook id: the file path WITH the url's query, like Vite (the router
/// code-splitter emits a different variant per query, keyed off the id).
fn hook_id(file: &Path, url: &str) -> String {
    match url.split_once('?') {
        Some((_, q)) => format!("{}?{}", file.display(), q),
        None => file.to_string_lossy().into_owned(),
    }
}

/// The module source: a plugin `load` result, else the file on disk.
async fn load_source(
    state: &ServerState,
    file: &Path,
    url: &str,
    is_dep: bool,
) -> Result<String, String> {
    if let Some(code) = plugin_load(state, file, url, is_dep).await? {
        return Ok(code);
    }
    bytes_to_string(
        tokio::fs::read(file)
            .await
            .map_err(|err| format!("read error for {url}: {err}"))?,
    )
    .map_err(|err| format!("read error for {url}: {err}"))
}

// Vite parity: plugin `load` runs before the fs read, so a plugin can replace an
// on-disk file. Gated to app source plus optimizeDeps.exclude'd deps (those go
// through plugin hooks in Vite too; pre-bundled deps never do), and reached only
// on a cold module, so the warm path pays no per-request RPC.
async fn plugin_load(
    state: &ServerState,
    file: &Path,
    url: &str,
    is_dep: bool,
) -> Result<Option<String>, String> {
    let dep_wants_load = is_dep
        && (state.optimize_view.is_excluded(file) || {
            let path = file.to_string_lossy();
            state.dep_load_res.iter().any(|re| re.is_match(&path))
        });
    if !state.plugins_have_load || (is_dep && !dep_wants_load) {
        return Ok(None);
    }
    let Some(host) = &state.plugins else {
        return Ok(None);
    };
    let load_id = hook_id(file, url);
    if !host.hook_wants_load(&load_id) {
        if plugins::hook_gate_debug() {
            eprintln!("oj: hook gate skipped load for {load_id}");
        }
        return Ok(None);
    }
    // A throwing `load` fails the module like Vite (500 + overlay),
    // rather than silently reading the disk file it meant to replace.
    host.load(&load_id)
        .await
        .map_err(|e| format!("plugin load error for {url}:\n{e}"))
}

fn record_glob_importer(state: &ServerState, file: &Path, url: &str, source: &str) {
    if !source.contains("import.meta.glob") {
        return;
    }
    let patterns: Vec<glob::Pattern> = oj_compiler::glob::glob_patterns(source, file)
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    let clean = url.split('?').next().unwrap_or(url).to_string();
    let mut globs = state.glob_importers.lock().unwrap();
    if patterns.is_empty() {
        globs.remove(&clean);
    } else {
        globs.insert(clean, patterns);
    }
}

struct CompileKey {
    key: String,
    /// tsconfig class-field semantics, decided ONCE from the real path: the
    /// compile may run on a synthetic one (`x.svg` -> `x.svg.tsx`).
    class_field_semantics: bool,
}

/// The content cache key, also recorded against the file's stamp for the warm path.
fn compile_key(
    state: &ServerState,
    file: &Path,
    url: &str,
    source: &str,
    is_server: bool,
    stamp: Option<FileStamp>,
) -> CompileKey {
    let mode = if is_server { "server" } else { "dev" };
    // Fold the newest HMR stamp among imports into the key: an unchanged importer
    // must recompile after a dep update so its import carries the new `?t=`.
    let imports_stamp = state
        .graph
        .lock()
        .unwrap()
        .imports_timestamp(Path::new(url));
    let mut mode_key = if imports_stamp > 0 {
        format!("{mode}@{imports_stamp}")
    } else {
        mode.to_string()
    };
    // tsconfig class-field semantics change the output, so they are part of the key.
    let class_field_semantics = oj_compiler::tsconfig::class_field_set_semantics(file);
    if class_field_semantics {
        mode_key.push_str("+setcf");
    }
    let key = state.cache.key(source.as_bytes(), url, &mode_key);
    if let Some((mtime, size)) = stamp {
        state
            .mtime_keys
            .lock()
            .unwrap()
            .insert(url.to_string(), (mtime, size, key.clone()));
    }
    CompileKey {
        key,
        class_field_semantics,
    }
}

// Cached post-transform code may import a plugin-served virtual whose content
// lives only in plugin memory (wyw-in-js cssLookup) and 404s on a warm start:
// re-run the transform for cached modules whose imports name a filesystem path
// with no file on disk; all-real-file modules keep the fast persistent cache.
fn persistent_hit(state: &ServerState, key: &str, is_dep: bool) -> Option<Arc<CachedModule>> {
    let module = Arc::new(
        state
            .persistent_cache
            .then(|| state.cache.get(key))
            .flatten()?,
    );
    let needs_retransform = state.plugins_have_transform
        && !is_dep
        && imports_a_plugin_virtual(&module.imports, &state.root, &state.dir_cache);
    (!needs_retransform).then_some(module)
}

struct Transformed {
    code: String,
    watch_files: Vec<String>,
    maps: Vec<String>,
}

async fn plugin_transform(
    state: &ServerState,
    file: &Path,
    url: &str,
    source: String,
    is_dep: bool,
) -> Result<Transformed, String> {
    let untouched = |code| Transformed {
        code,
        watch_files: Vec::new(),
        maps: Vec::new(),
    };
    let dep_wants_transform = is_dep
        && (state.optimize_view.is_excluded(file)
            || state
                .dep_transform_res
                .iter()
                .any(|re| re.is_match(&source)));
    let host = match &state.plugins {
        Some(host) if state.plugins_have_transform && (!is_dep || dep_wants_transform) => host,
        _ => return Ok(untouched(source)),
    };
    let transform_id = hook_id(file, url);
    if !host.hook_wants_transform(&transform_id, &source) {
        if plugins::hook_gate_debug() {
            eprintln!("oj: hook gate skipped transform for {transform_id}");
        }
        return Ok(untouched(source));
    }
    let resolved = resolved_imports_json(&state.resolver, &state.fs_allow, &source, file);
    match host.transform(&source, &transform_id, &resolved).await {
        Ok((code, watch_files, maps, _)) => Ok(Transformed {
            code,
            watch_files,
            maps,
        }),
        // Vite fails the request with the plugin's error (code frame in the
        // overlay); serving the untransformed source would ship wrong code.
        Err(e) => Err(format!(
            "plugin transform error for {}:\n{e}",
            file.display()
        )),
    }
}

/// css.preprocessorOptions.<less|stylus>: `additionalData` is prepended,
/// everything else goes to the preprocessor as its options (Vite parity).
async fn preprocess_less_stylus(
    state: &Arc<ServerState>,
    url: &str,
    source: String,
) -> Result<String, String> {
    let lang = if sidecar::is_less(url) {
        "less"
    } else {
        "stylus"
    };
    let cfg = state.css_config.clone().map(|c| oj_config::OjConfig {
        css: Some(c),
        ..Default::default()
    });
    let (data, opts) = match &cfg {
        Some(c) => (
            oj_config::css_additional_data(c, lang),
            oj_config::css_preprocessor_json(c, lang),
        ),
        None => (None, serde_json::Value::Null),
    };
    let with_data = match data {
        Some(d) if !d.is_empty() => format!("{d}\n{source}"),
        _ => source,
    };
    run_preprocess_engine(state, url, &with_data, opts)
        .await
        .map_err(|e| format!("css preprocess error for {url}: {e}"))
}

#[derive(Default)]
struct CssState {
    /// Pulled-in files (@import targets, sass loads), collected for the module graph.
    deps: Vec<PathBuf>,
    sass_precompiled: bool,
    imports_inlined: bool,
}

/// Vite orders Sass before PostCSS, so compile Sass here first when a PostCSS
/// config applies; the compile step then skips Sass.
async fn css_prepass(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    source: String,
    css: &mut CssState,
) -> Result<String, String> {
    let source = if state.has_postcss && oj_css::is_sass(url) {
        let data = sass_additional_data_for(state, url);
        let load_paths = sass_load_paths_for(state, url);
        let dir = file.parent().map(Path::to_path_buf);
        let css_resolve = state.css_resolve.clone();
        let (compiled, deps) = tokio::task::spawn_blocking(move || {
            let mut deps = Vec::new();
            let out = oj_css::compile_sass_collecting(
                &source,
                &oj_css::SassOptions {
                    load_dir: dir.as_deref(),
                    additional_data: data.as_deref(),
                    load_paths: &load_paths,
                    resolve: css_resolve.as_ref(),
                },
                &mut deps,
            );
            out.map(|css| (css, deps))
        })
        .await
        .map_err(|e| format!("sass compile task failed for {url}: {e}"))??;
        css.deps.extend(deps);
        css.sass_precompiled = true;
        compiled
    } else {
        source
    };
    let css_like = css.sass_precompiled
        || is_preprocessor(url)
        || file.extension().and_then(|e| e.to_str()) == Some("css");
    if !(state.has_postcss && css_like) {
        return Ok(source);
    }
    // postcss-import is first in Vite's PostCSS chain: inline @imports before
    // the PostCSS pass so imported rules go through the user's plugins too.
    let source = oj_css::inline_imports_collecting(
        &source,
        file,
        &state.css_resolve.as_ref(),
        &mut css.deps,
    )?;
    css.imports_inlined = true;
    Ok(match run_css_engine(state, url, &source).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("oj: postcss failed for {url}: {e}");
            source
        }
    })
}

#[derive(Clone, Copy)]
enum CompileKind {
    Json,
    Css,
    Script,
}

/// The owned inputs of the blocking compile stage.
struct CompileJob {
    state: Arc<ServerState>,
    kind: CompileKind,
    /// The path the compiler sees; synthetic for svg (`.svg.tsx`) and svelte.
    file: PathBuf,
    /// The real file, as the `importer` of `/@id/` urls.
    importer_abs: String,
    dir: PathBuf,
    url: String,
    source: String,
    is_dep: bool,
    is_server: bool,
    is_svelte: bool,
    class_field_semantics: bool,
    css: CssState,
    plugin_maps: Vec<String>,
    dep_map: Arc<optimize::DepMap>,
}

impl CompileJob {
    fn run(self) -> Result<CachedModule, String> {
        match self.kind {
            CompileKind::Json => {
                let code = oj_compiler::json::to_esm(&self.source, &self.url)
                    .map_err(|err| format!("compile error:\n{err}"))?;
                Ok(code_module(code))
            }
            CompileKind::Css => self.compile_css(),
            CompileKind::Script => self.compile_script(),
        }
    }

    fn compile_css(self) -> Result<CachedModule, String> {
        let state = &self.state;
        let mut css_deps = self.css.deps;
        let resolve = state.css_resolve.as_ref();
        let css_src = if oj_css::is_sass(&self.url) && !self.css.sass_precompiled {
            let sass_data = sass_additional_data_for(state, &self.url);
            let sass_load_paths = sass_load_paths_for(state, &self.url);
            oj_css::compile_sass_collecting(
                &self.source,
                &oj_css::SassOptions {
                    load_dir: Some(&self.dir),
                    additional_data: sass_data.as_deref(),
                    load_paths: &sass_load_paths,
                    resolve,
                },
                &mut css_deps,
            )?
        } else {
            self.source
        };
        // Plain `@import`s are inlined (postcss-import parity) so the injected
        // stylesheet does not @import a bare specifier or a wrong-relative url.
        let css_src = if self.css.imports_inlined {
            css_src
        } else {
            oj_css::inline_imports_collecting(&css_src, &self.file, &resolve, &mut css_deps)?
        };
        let dev_sourcemap = state
            .css_config
            .as_ref()
            .and_then(|c| c.dev_sourcemap)
            .unwrap_or(false);
        let output = oj_css::compile_css_dev(&self.url, &css_src, dev_sourcemap, &resolve)?;
        // Pulled-in files enter the graph as this sheet's imports so a dep edit
        // hot-updates it; filter the sheet itself (a self-edge is a bogus cycle).
        let root = &state.root;
        let mut dep_imports: Vec<String> = css_deps
            .iter()
            .filter(|p| p.starts_with(root) && **p != self.file)
            .map(|p| url_of(root, p))
            .collect();
        dep_imports.sort();
        dep_imports.dedup();
        // A CSS module's class map changes on edit, so it cannot self-accept
        // (Vite's css-analysis): the update climbs to the importing component.
        let is_css_module = output.exports.is_some();
        Ok(CachedModule {
            is_boundary: !is_css_module,
            kind: "css".into(),
            imports: dep_imports,
            css_exports: output.exports.unwrap_or_default(),
            ..code_module(output.css)
        })
    }

    fn compile_script(self) -> Result<CachedModule, String> {
        let rw = ImportRewrite {
            job: &self,
            unresolved: RefCell::new(None),
        };
        let output = if self.is_dep {
            self.compile_dep(&rw)
        } else {
            self.compile_app(&rw)
        }
        .map_err(|err| format!("compile error:\n{err}"))?;
        if let Some(spec) = rw.unresolved.borrow().as_ref() {
            return Err(unresolved_import_error(
                &self.state.root,
                &self.file,
                &self.source,
                spec,
            ));
        }
        Ok(CachedModule {
            is_boundary: self.is_svelte || (!self.is_dep && output.has_refresh_registrations()),
            hot: output.hot_accept.map(|h| oj_cache::HotMeta {
                self_accept: h.self_accepting,
                deps: h.deps,
                accepted_exports: h.accepted_exports,
            }),
            code: output.code,
            map_json: output.map_json,
            fs_allow: fs_allow_from(&output.imports),
            watch_files: Vec::new(),
            import_bindings: output.import_bindings,
            imports: output.imports,
            kind: if self.is_svelte {
                "svelte".into()
            } else {
                String::new()
            },
            require_map: Vec::new(),
            css_exports: Vec::new(),
        })
    }

    fn compile_dep(
        &self,
        rw: &ImportRewrite<'_>,
    ) -> Result<oj_compiler::CompileOutput, oj_compiler::CompileError> {
        let (file, source) = (&self.file, &self.source);
        if !oj_compiler::cjs::has_module_syntax_pub(file, source) {
            let dep_interop = interop_node_builtins(source, file);
            let dep_src = dep_interop.as_deref().unwrap_or(source);
            // A CJS dep's `require()`s resolve with the `require` condition (Vite's
            // getConditions), so a dual package hands it its CJS build, not the ESM one.
            return oj_compiler::cjs::compile_dep(file, &self.url, dep_src, &mut |spec: &str| {
                rw.rewrite(spec, &self.state.require_resolver)
            });
        }
        // Gated on a cheap bare-import scan: the interop rewrite (a full
        // parse) runs only when a bare specifier maps to an interop URL.
        let interop = |spec: &str| rw.cjs_interop_url(spec);
        let dep_interop = if oj_compiler::interop::bare_import_specifiers(source)
            .iter()
            .any(|spec| interop(spec).is_some())
        {
            oj_compiler::interop::rewrite_cjs_interop_logged(
                source,
                file,
                &interop,
                &mut warn_interop_once,
            )
        } else {
            None
        };
        let dep_src = dep_interop.as_deref().unwrap_or(source);
        oj_compiler::cjs::compile_dep(file, &self.url, dep_src, &mut |spec: &str| {
            rw.rewrite(spec, &self.state.resolver)
        })
    }

    fn compile_app(
        &self,
        rw: &ImportRewrite<'_>,
    ) -> Result<oj_compiler::CompileOutput, oj_compiler::CompileError> {
        let interopped = oj_compiler::interop::rewrite_cjs_interop_logged(
            &self.source,
            &self.file,
            &|spec: &str| rw.cjs_interop_url(spec),
            &mut warn_interop_once,
        );
        let mut opts = dev_compile_opts(&self.state);
        if self.is_svelte {
            opts.refresh = false;
        }
        // The key already folded this decision (from the original path); the
        // compile must never re-derive it from a synthetic one.
        opts.class_field_set_semantics = Some(self.class_field_semantics);
        oj_compiler::compile_module_with_maps(
            &self.file,
            interopped.as_deref().unwrap_or(&self.source),
            &opts,
            Some(&mut |spec: &str| rw.rewrite(spec, &self.state.resolver)),
            &self.plugin_maps,
        )
    }
}

/// Import specifier rewriting for one compile.
struct ImportRewrite<'a> {
    job: &'a CompileJob,
    /// First unresolvable relative import: Vite's import analysis fails the
    /// transform for it; shipping it unchanged would only 404 with no overlay.
    unresolved: RefCell<Option<String>>,
}

impl ImportRewrite<'_> {
    fn id_url(&self, spec: &str) -> String {
        format!(
            "/@id/{}?importer={}",
            hex_encode(spec),
            hex_encode(&self.job.importer_abs)
        )
    }

    fn rewrite(&self, spec: &str, resolver: &OjResolver) -> Option<String> {
        let job = self.job;
        let state = &*job.state;
        let plugin_fallback = state.plugins.is_some();
        if spec == "virtual:oj-routes" {
            return Some("/@oj/routes.js".to_string());
        }
        if state.virtual_ids.contains(spec) {
            return Some(format!("/@virtual/{spec}"));
        }
        // Vite runs plugins' resolveId before its own resolver: a non-bare import
        // a filter claims goes to /@id/, which falls back to disk on decline.
        if plugin_fallback
            && !is_bare_specifier(spec)
            && state.resolve_id_res.iter().any(|re| re.is_match(spec))
        {
            return Some(self.id_url(spec));
        }
        if let Some(id) = state.jsx_overrides.get(spec) {
            return Some(format!("/@presolve/{}", hex_encode(id)));
        }
        if let Some(meta) = job.dep_map.get(spec) {
            if !meta.needs_interop {
                return Some(meta.url.clone());
            }
        }
        if let Some(url) = rewrite_specifier(
            &state.root,
            &job.dir,
            resolver,
            &state.fs_allow,
            &state.dir_cache,
            spec,
            true,
        ) {
            // With a transform plugin active, leave `.svg` unmarked (not `?url`) so
            // it routes through compile and svgr can componentize it (Vite parity).
            if state.plugins_have_transform {
                if let Some(base) = url.strip_suffix(".svg?url") {
                    return Some(format!("{base}.svg"));
                }
            }
            // Vite's importAnalysis appends `?t=<lastHMRTimestamp>` to imports of
            // HMR-invalidated modules so the importer loads the new version.
            let stamp = state
                .graph
                .lock()
                .unwrap()
                .hmr_timestamp(Path::new(url.split('?').next().unwrap_or(&url)));
            return Some(stamp_import_url(&url, stamp));
        }
        if plugin_fallback && is_bare_specifier(spec) {
            return Some(self.id_url(spec));
        }
        // An unclaimable bare specifier fails like Vite's importAnalysis instead
        // of shipping the bare name; SSR keeps `if (ssr) return [url, null]`.
        if !job.is_dep
            && !job.is_server
            && (relative_import_missing(&job.dir, resolver, spec)
                || bare_import_unresolved(&job.dir, resolver, spec))
        {
            self.unresolved
                .borrow_mut()
                .get_or_insert_with(|| spec.to_string());
        }
        None
    }

    /// Specifiers whose named imports must read off the CJS value (module.exports)
    /// instead of linking as ESM bindings; shared by app source and served ESM deps.
    fn cjs_interop_url(&self, spec: &str) -> Option<String> {
        let job = self.job;
        // Builtins are browser-externalized to a stub with no named exports;
        // interop so named imports read undefined instead of failing to link.
        if is_node_builtin(spec) {
            return Some(format!("/@id/{}", hex_encode(spec)));
        }
        // lingui macro entrypoints go to the shim (which has real
        // named exports), never through default-access interop.
        if is_lingui_macro_specifier(spec) {
            return None;
        }
        if let Some(m) = job.dep_map.get(spec).filter(|m| m.needs_interop) {
            return Some(m.url.clone());
        }
        // Directly-served bare CJS dep: interop at the importer (Vite pre-bundles
        // these). node_modules-only so aliased app source is never treated as a dep.
        if !is_bare_specifier(spec) || job.dep_map.get(spec).is_some() {
            return None;
        }
        let resolved = job.state.resolver.resolve(&job.dir, spec).ok()?;
        let in_node_modules = resolved
            .components()
            .any(|c| c.as_os_str() == "node_modules");
        // optimizeDeps.needsInterop forces the interop rewrite even
        // when static analysis reads the dep as ESM.
        if in_node_modules
            && (is_cjs_dep_file(&resolved)
                || job.state.optimize_view.needs_forced_interop(&resolved))
        {
            allow_root(&job.state.fs_allow, package_root(&resolved));
            return Some(url_of(&job.state.root, &resolved));
        }
        None
    }
}

pub(crate) async fn replay_module_parsed(
    state: &Arc<ServerState>,
    file: &Path,
    key: &str,
    is_dep: bool,
    is_server: bool,
) {
    if !state.plugins_use_module_parsed || is_dep || is_server {
        return;
    }
    if !state.parsed_fired.lock().unwrap().insert(key.to_string()) {
        return;
    }
    if let Some(host) = &state.plugins {
        let _ = host.module_parsed(&file.to_string_lossy()).await;
    }
}

pub(crate) fn package_root(path: &Path) -> PathBuf {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("package.json").is_file() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    path.parent().unwrap_or(path).to_path_buf()
}

pub(crate) fn fs_allow_from(imports: &[String]) -> Vec<String> {
    imports
        .iter()
        .filter_map(|i| i.split('?').next().unwrap_or(i).strip_prefix("/@fs"))
        .map(|p| package_root(Path::new(p)).display().to_string())
        .collect()
}

pub(crate) fn register_in_graph(state: &ServerState, url: &str, module: &CachedModule) {
    if !module.fs_allow.is_empty() {
        let mut allow = state.fs_allow.lock().unwrap();
        for p in &module.fs_allow {
            allow.insert(PathBuf::from(p));
        }
    }
    if !module.watch_files.is_empty() {
        let mut watched = state.plugin_watched.lock().unwrap();
        for p in &module.watch_files {
            let p = PathBuf::from(p);
            // Vite's addWatchFile goes through ensureWatchedFile too: an
            // outside-root watch file otherwise never produces an event.
            ensure_watched_file(state, &p);
            watched.insert(p);
        }
    }
    let mut graph = state.graph.lock().unwrap();
    let local_imports: Vec<&Path> = module
        .imports
        .iter()
        .filter(|s| s.starts_with('/') && !s.starts_with("/@oj/") && !is_worker_query(s))
        .map(|s| Path::new(s.split('?').next().unwrap_or(s)))
        .collect();
    let pruned = graph.set_imports(Path::new(url), &local_imports);
    if !pruned.is_empty() {
        // Dropped deps nothing imports any more: the client runs their `hot.prune`
        // callbacks, stamped so a re-import re-runs them (Vite's handlePrunedModules).
        graph.stamp_pruned(&pruned, now_millis() as u64);
        let paths: Vec<String> = pruned.iter().map(|p| p.display().to_string()).collect();
        println!("oj: prune {paths:?}");
        let _ = state
            .reload_tx
            .send(serde_json::json!({ "type": "prune", "paths": paths }).to_string());
    }
    let hot = module.hot.as_ref();
    graph.set_self_accepting(
        Path::new(url),
        module.is_boundary || hot.is_some_and(|h| h.self_accept),
    );
    let accepted: Vec<PathBuf> = hot
        .map(|h| {
            h.deps
                .iter()
                .map(|d| PathBuf::from(d.split('?').next().unwrap_or(d)))
                .collect()
        })
        .unwrap_or_default();
    graph.set_accepted_deps(Path::new(url), &accepted);
    graph.set_accepted_exports(
        Path::new(url),
        hot.and_then(|h| h.accepted_exports.as_deref()),
    );
    // Borrowed all the way down: the graph compares in place and only the
    // changed case materializes, keeping the warm re-register heap-free.
    let bindings = module
        .import_bindings
        .iter()
        .filter(|(s, _)| s.starts_with('/') && !s.starts_with("/@oj/") && !is_worker_query(s))
        .map(|(s, names)| {
            (
                Path::new(s.split('?').next().unwrap_or(s)),
                names.as_slice(),
            )
        });
    graph.set_imported_bindings(Path::new(url), bindings);
}

pub(crate) fn compile_fs_deny(user: &[String]) -> Vec<(glob::Pattern, bool)> {
    const DEFAULTS: &[&str] = &[".env", ".env.*", "*.crt", "*.pem", "**/.git/**"];
    DEFAULTS
        .iter()
        .map(|s| s.to_string())
        .chain(user.iter().cloned())
        .flat_map(|p| expand_braces(&p))
        .filter_map(|p| {
            let base_only = !p.contains('/');
            glob::Pattern::new(&p).ok().map(|pat| (pat, base_only))
        })
        .collect()
}

/// Expands `{a,b}` groups (picomatch/Vite semantics); the `glob` crate has no
/// brace support, so Vite's default deny shape would match nothing.
pub(crate) fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        return vec![pattern.to_string()];
    };
    let (head, rest) = (&pattern[..open], &pattern[close + 1..]);
    pattern[open + 1..close]
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{rest}")))
        .collect()
}

pub(crate) fn path_is_denied(file: &Path, root: &Path, deny: &[(glob::Pattern, bool)]) -> bool {
    if deny.is_empty() {
        return false;
    }
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let abs_str = file.to_string_lossy().replace('\\', "/");
    let base = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // The deny list is a security control: match case-insensitively so `.ENV` on a
    // case-insensitive filesystem cannot leak; a superset deny elsewhere is safe.
    let opts = glob::MatchOptions {
        case_sensitive: false,
        ..glob::MatchOptions::new()
    };
    for (pat, base_only) in deny {
        let hit = if *base_only {
            pat.matches_with(&base, opts)
        } else {
            pat.matches_with(&rel_str, opts) || pat.matches_with(&abs_str, opts)
        };
        if hit {
            return true;
        }
    }
    false
}

/// A url requested by a module import (Vite's `?import` marker, or the browser's
/// `sec-fetch-dest: script`) gets the JS-module form; anything else gets raw bytes.
pub(crate) fn wants_module_import(headers: &HeaderMap, query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|kv| kv == "import"))
        || headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("script")
}

pub(crate) fn wants_raw_resource(headers: &HeaderMap) -> bool {
    matches!(
        headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()),
        Some("style" | "image" | "font" | "audio" | "video" | "track" | "object" | "embed")
    )
}

// Assets that resolve to a URL-exporting module when imported from JS. svg is
// excluded: it routes through compile so svgr can componentize it.
pub fn is_importable_asset_ext(ext: &str) -> bool {
    oj_compiler::assets::is_asset_ext(ext) && !ext.eq_ignore_ascii_case("svg")
}

/// `module.builtinModules` without the `node:` scheme, sorted for binary search.
const NODE_BUILTINS: &[&str] = &[
    "assert",
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
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

// Node core modules get a browser-externalized stub (Vite parity). Pub: the Start
// host checks builtins before resolving on the SSR side, as Node itself does.
pub fn is_node_builtin(spec: &str) -> bool {
    // Vite's isNodeBuiltin: anything under the `node:` scheme is a builtin, plus
    // the scheme-less `module.builtinModules` names.
    if spec.starts_with("node:") {
        return true;
    }
    let base = spec.split('/').next().unwrap_or(spec);
    base.starts_with("_http_")
        || base.starts_with("_stream_")
        || base.starts_with("_tls_")
        || NODE_BUILTINS.binary_search(&base).is_ok()
}

// Vite's searchForWorkspaceRoot: first workspace marker walking up, else NEAREST
// ancestor package.json. `.git` is deliberately not a marker (would expose the
// whole repository over /@fs). Seeds server.fs.allow, matching Vite's default.
pub(crate) fn workspace_root(root: &Path) -> PathBuf {
    let mut pkg_root: Option<PathBuf> = None;
    let mut dir = root;
    loop {
        if dir.join("pnpm-workspace.yaml").exists() || dir.join("lerna.json").exists() {
            return dir.to_path_buf();
        }
        if dir.join("package.json").exists() {
            if json_field_truthy(&dir.join("package.json"), "workspaces") {
                return dir.to_path_buf();
            }
            if pkg_root.is_none() {
                pkg_root = Some(dir.to_path_buf());
            }
        }
        if json_field_truthy(&dir.join("deno.json"), "workspace")
            || json_field_truthy(&dir.join("deno.jsonc"), "workspace")
        {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return pkg_root.unwrap_or_else(|| root.to_path_buf()),
        }
    }
}

// Vite parity: the PARSED field, truthy by JS rules; a dep literally named
// "workspaces" must not widen the root, and deno.jsonc must be valid JSON.
fn json_field_truthy(file: &Path, field: &str) -> bool {
    let Ok(txt) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return false;
    };
    match v.get(field) {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

// Pre-resolve a module's static imports into a {spec: id|null} JSON map for the
// plugin transform, so per-import `this.resolve` is a local lookup, not an IPC
// round-trip (thousands per page for import-protection otherwise).
pub(crate) fn resolved_imports_json(
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    source: &str,
    file: &Path,
) -> String {
    let dir = file.parent().unwrap_or(file);
    let mut map = serde_json::Map::new();
    for spec in oj_compiler::imports(source, file) {
        // Builtins never resolve to a file; skip to avoid a resolver warning per
        // module. A plugin's this.resolve falls back to the host for these.
        if is_node_builtin(&spec) {
            continue;
        }
        let val = match resolver.resolve(dir, &spec) {
            Ok(p) => {
                // If the transform keeps the import, the browser fetches it from
                // /@fs, so allow-list its package root now.
                if p.components().any(|c| c.as_os_str() == "node_modules") || !p.starts_with(dir) {
                    allow_root(fs_allow, package_root(&p));
                }
                serde_json::Value::String(p.display().to_string())
            }
            Err(_) => serde_json::Value::Null,
        };
        map.insert(spec, val);
    }
    serde_json::Value::Object(map).to_string()
}

/// Warn once per distinct message (Vite's deduping logger); a stable per-file
/// condition would otherwise re-warn on every recompile.
pub(crate) fn warn_interop_once(msg: String) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if seen.lock().unwrap().insert(msg.clone()) {
        eprintln!("oj: warning: {msg}");
    }
}

pub(crate) fn interop_node_builtins(source: &str, file: &Path) -> Option<String> {
    if !source.contains("node:") {
        return None;
    }
    oj_compiler::interop::rewrite_cjs_interop(source, file, &|spec| {
        is_node_builtin(spec).then(|| format!("/@id/{}", hex_encode(spec)))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn node_builtins_sorted_for_binary_search() {
        assert!(super::NODE_BUILTINS.windows(2).all(|w| w[0] < w[1]));
        assert!(super::is_node_builtin("fs/promises"));
        assert!(super::is_node_builtin("_stream_readable"));
        assert!(!super::is_node_builtin("react"));
    }
}
