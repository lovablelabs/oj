use super::*;

pub(crate) async fn ensure_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<(String, Arc<CachedModule>), String> {
    let react_svg = file.extension().and_then(|e| e.to_str()) == Some("svg")
        && url
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv == "react"));
    let is_svelte = file.extension().and_then(|e| e.to_str()) == Some("svelte");
    // A plain `.svg` (no explicit `?url`/`?raw`) reaches here when a transform
    // plugin might componentize it (vite-plugin-svgr). Route it through the transform
    // pipeline instead of short-circuiting to a URL asset; the svgr transform decides
    // per its own include filter, and an svg it does not match falls back to a URL
    // asset after the transform runs.
    let svgr_candidate = !react_svg
        && state.plugins_have_transform
        && file.extension().and_then(|e| e.to_str()) == Some("svg")
        && query_asset_kind(url.split_once('?').map(|(_, q)| q)).is_none();

    if !react_svg && !svgr_candidate && is_asset_path(file) {
        let clean = url.split('?').next().unwrap_or(url);
        let default = format!(
            "export default {};\n",
            serde_json::Value::String(clean.to_string())
        );
        let module = Arc::new(CachedModule {
            is_boundary: false,
            hot: None,
            kind: String::new(),
            code: default,
            map_json: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
            import_bindings: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let stamp = match tokio::fs::metadata(file).await {
        Ok(meta) => meta.modified().ok().map(|mtime| (mtime, meta.len())),
        Err(_) => None,
    };
    if let Some((mtime, size)) = stamp {
        let cached_key = state
            .mtime_keys
            .lock()
            .unwrap()
            .get(url)
            .filter(|(t, s, _)| *t == mtime && *s == size)
            .map(|(_, _, k)| k.clone());
        if let Some(key) = cached_key {
            if let Some(module) = memory_get(state, url, &key) {
                register_in_graph(state, url, &module);
                return Ok((key, module));
            }
        }
    }

    let is_dep_early = is_dep_module(url, file);

    // Vite runs plugin `load` hooks before the filesystem read (its fs read is the
    // last-resort `vite:load-fallback` plugin), so a plugin can replace an on-disk
    // file's contents. The i18n-dev plugin relies on this: its `load` collapses the
    // generated 8k-line message barrel (`_index.js`) into a handful of grouped
    // virtual modules, so the browser fetches a few groups instead of thousands of
    // individual re-exported files. oj mirrors the ordering here: give a matching
    // plugin `load` the first say, fall back to the disk read when none loads. It is
    // gated to app source (deps in node_modules never need it) and reached only on a
    // cold module (the mtime cache above short-circuits warm ones), so it adds no
    // per-request RPC on the warm path, and nothing at all when no plugin has `load`.
    // An `optimizeDeps.exclude`d package is one Vite never pre-bundles, so its
    // files go through every plugin's `load`/`transform` there like app source;
    // the other deps stand in for Vite's pre-bundled ones, which no plugin sees.
    let dep_wants_load = is_dep_early
        && (pkg_bundle::is_excluded(file) || {
            let path = file.to_string_lossy();
            state.dep_load_res.iter().any(|re| re.is_match(&path))
        });
    let plugin_loaded = if state.plugins_have_load && (!is_dep_early || dep_wants_load) {
        match &state.plugins {
            Some(host) => {
                let load_id = match url.split_once('?') {
                    Some((_, q)) => format!("{}?{}", file.display(), q),
                    None => file.to_string_lossy().into_owned(),
                };
                if !host.hook_wants_load(&load_id) {
                    if plugins::hook_gate_debug() {
                        eprintln!("oj: hook gate skipped load for {load_id}");
                    }
                    None
                } else {
                    // A throwing `load` fails the module like Vite (500 + overlay),
                    // rather than silently reading the disk file it meant to replace.
                    host.load(&load_id)
                        .await
                        .map_err(|e| format!("plugin load error for {url}:\n{e}"))?
                }
            }
            None => None,
        }
    } else {
        None
    };
    let source = match plugin_loaded {
        Some(code) => code,
        None => bytes_to_string(
            tokio::fs::read(file)
                .await
                .map_err(|err| format!("read error for {url}: {err}"))?,
        )
        .map_err(|err| format!("read error for {url}: {err}"))?,
    };
    if source.contains("import.meta.glob") {
        let patterns: Vec<glob::Pattern> = oj_compiler::glob::glob_patterns(&source, file)
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
    if file.extension().and_then(|e| e.to_str()) == Some("css") && is_tailwind_css(&source) {
        let css = compile_tailwind(state, url, &source).await?;
        let module = Arc::new(CachedModule {
            is_boundary: true,
            hot: None,
            kind: "css".into(),
            code: css,
            map_json: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
            import_bindings: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let is_server = is_server_module(file) && !is_dep_early;

    let mode = if is_server { "server" } else { "dev" };
    // Fold the newest HMR stamp among this module's imports into the key: after a
    // dependency updates, the (unchanged) importer must recompile so its import of
    // that dependency carries the new `?t=`, or the browser keeps the stale one.
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
    // The tsconfig's class-field semantics change the transform output for the
    // same source, so they are part of the key: a tsconfig edit (which clears
    // the discovery cache) then misses both the memory and persistent caches
    // instead of resurrecting stale code. Decided ONCE here and handed to the
    // compile, which may run on a synthetic path (`x.svg` -> `x.svg.tsx`).
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

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(url.to_string()).or_default())
    };
    let _guard = lock.lock().await;

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }
    // The persistent (cross-restart) cache holds post-plugin-transform code. A
    // transform can append an import to a plugin-served *virtual* whose content
    // lives only in the plugin's in-memory state: wyw-in-js records each module's
    // extracted CSS in a `cssLookup` its `load` hook serves, and the cached code
    // still `import`s that `.wyw-in-js.css` id. On a warm start the transform
    // never re-runs, so that map is empty and the import 404s. Detect it
    // precisely — a cached module whose imports include a filesystem path with no
    // file on disk depends on such a virtual — and re-run the transform for just
    // those. Modules whose imports are all real files (svgr on disk, plain
    // source, deps) keep the fast persistent cache (Vite has no cross-restart
    // transform cache at all; this preserves oj's where it is sound).
    if let Some(module) = state
        .persistent_cache
        .then(|| state.cache.get(&key))
        .flatten()
    {
        let module = Arc::new(module);
        let needs_retransform = state.plugins_have_transform
            && !is_dep_early
            && imports_a_plugin_virtual(&module.imports, &state.root, &state.dir_cache);
        if !needs_retransform {
            memory_put(state, url, &key, &module);
            register_in_graph(state, url, &module);
            replay_module_parsed(state, file, &key, is_dep_early, is_server).await;
            return Ok((key, module));
        }
    }

    if is_server {
        let code = server_fn_stub(&oj_compiler::exports(&source, file), url);
        let module = Arc::new(CachedModule {
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
        });
        if state.persistent_cache {
            let _ = state
                .cache_writes
                .try_send((key.clone(), Arc::clone(&module)));
        }
        memory_put(state, url, &key, &module);
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let is_dep = is_dep_module(url, file);
    let mut plugin_watch_files: Vec<String> = Vec::new();
    let mut plugin_maps: Vec<String> = Vec::new();
    let dep_wants_transform = is_dep
        && (pkg_bundle::is_excluded(file)
            || state
                .dep_transform_res
                .iter()
                .any(|re| re.is_match(&source)));
    let source = match &state.plugins {
        Some(host) if state.plugins_have_transform && (!is_dep || dep_wants_transform) => {
            // Pass the id WITH its query (e.g. `?tsr-shared=1`), like Vite: the router
            // code-splitter emits a different variant per query, keyed off the id.
            let transform_id = match url.split_once('?') {
                Some((_, q)) => format!("{}?{}", file.display(), q),
                None => file.to_string_lossy().into_owned(),
            };
            if !host.hook_wants_transform(&transform_id, &source) {
                if plugins::hook_gate_debug() {
                    eprintln!("oj: hook gate skipped transform for {transform_id}");
                }
                source
            } else {
                let resolved =
                    resolved_imports_json(&state.resolver, &state.fs_allow, &source, file);
                match host.transform(&source, &transform_id, &resolved).await {
                    Ok((code, watches, maps, _)) => {
                        plugin_watch_files = watches;
                        plugin_maps = maps;
                        code
                    }
                    // Vite fails the request with the plugin's error (code frame in the
                    // overlay); serving the untransformed source would ship wrong code.
                    Err(e) => {
                        return Err(format!(
                            "plugin transform error for {}:\n{e}",
                            file.display()
                        ));
                    }
                }
            }
        }
        _ => source,
    };

    let source = if is_preprocessor(url) {
        // css.preprocessorOptions.<less|stylus>: `additionalData` is prepended,
        // everything else goes to the preprocessor as its options (Vite parity).
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
            .map_err(|e| format!("css preprocess error for {url}: {e}"))?
    } else {
        source
    };

    // PostCSS runs on the preprocessor OUTPUT (Vite orders Sass before PostCSS),
    // so a Sass file is compiled here first when a PostCSS config applies; the
    // compile step below then skips Sass for it.
    // Every file this stylesheet pulls in (@import targets, sass loads): Vite
    // records them in the module graph so a dependency edit hot-updates the
    // importing sheet (vite:css addWatchFile -> css-post file-only entries).
    let mut css_deps: Vec<PathBuf> = Vec::new();
    let mut sass_precompiled = false;
    let source = if state.has_postcss && oj_css::is_sass(url) {
        let data = sass_additional_data_for(state, url);
        let load_paths = sass_load_paths_for(state, url);
        let dir = file.parent().map(Path::to_path_buf);
        let src = source.clone();
        let css_resolve = state.css_resolve.clone();
        let (compiled, deps) = tokio::task::spawn_blocking(move || {
            let mut deps = Vec::new();
            let out = oj_css::compile_sass_collecting(
                &src,
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
        css_deps.extend(deps);
        sass_precompiled = true;
        compiled
    } else {
        source
    };
    let css_like = sass_precompiled
        || is_preprocessor(url)
        || file.extension().and_then(|e| e.to_str()) == Some("css");
    let mut imports_inlined = false;
    let source = if state.has_postcss && css_like {
        // postcss-import is the first plugin of Vite's PostCSS chain, so the
        // rules of an @imported stylesheet go through the user's plugins too:
        // inline before the PostCSS pass, not after it.
        let source = oj_css::inline_imports_collecting(
            &source,
            file,
            &state.css_resolve.as_ref(),
            &mut css_deps,
        )?;
        imports_inlined = true;
        match run_css_engine(state, url, &source).await {
            Ok(out) => out,
            Err(e) => {
                eprintln!("oj: postcss failed for {url}: {e}");
                source
            }
        }
    } else {
        source
    };

    let source = if react_svg {
        svgr::svg_to_component(&source)
    } else {
        source
    };
    // The svgr plugin transform (if any) has run by now. If a plain `.svg` candidate
    // is still raw markup, the plugin did not match it (not in its include list), so
    // serve it as a URL asset like Vite; otherwise it is now component code compiled
    // as `.svg.tsx` below.
    let svgr_componentized = svgr_candidate && !source.trim_start().starts_with('<');
    if svgr_candidate && !svgr_componentized {
        let clean = url.split('?').next().unwrap_or(url);
        let module = Arc::new(CachedModule {
            is_boundary: false,
            hot: None,
            kind: String::new(),
            code: format!(
                "export default {};\n",
                serde_json::Value::String(clean.to_string())
            ),
            map_json: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
            import_bindings: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }
    let source = if is_svelte {
        run_svelte_engine(state, url, &source)
            .await
            .map_err(|e| format!("svelte compile error for {url}: {e}"))?
    } else {
        source
    };

    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let require_resolver = Arc::clone(&state.require_resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
    let jsx_overrides = state.jsx_overrides.clone();
    let jsx_config = state.jsx.clone();
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let file_owned = if react_svg || svgr_componentized {
        file.with_extension("svg.tsx")
    } else if is_svelte {
        file.with_extension("svelte.js")
    } else {
        file.to_path_buf()
    };
    let url_owned = url.to_string();
    let sass_data = sass_additional_data_for(state, &url_owned);
    let sass_load_paths = sass_load_paths_for(state, &url_owned);
    let css_resolve = state.css_resolve.clone();
    let css_dev_sourcemap = state
        .css_config
        .as_ref()
        .and_then(|c| c.dev_sourcemap)
        .unwrap_or(false);
    let hmr_state = Arc::clone(state);
    let plugin_fallback = state.plugins.is_some();
    let svgr_active = state.plugins_have_transform;
    let resolve_id_res = if plugin_fallback {
        state.resolve_id_res.clone()
    } else {
        Vec::new()
    };
    let importer_abs = file.to_string_lossy().into_owned();
    let ext = file.extension().and_then(|e| e.to_str());
    let is_css = ext.is_some_and(is_style_ext);
    let is_json = ext == Some("json");
    let dep_map = if is_css || is_json {
        Arc::new(optimize::DepMap::new())
    } else {
        state.optimized.ready().await
    };
    let compiled = tokio::task::spawn_blocking(move || -> Result<CachedModule, String> {
        if is_json {
            let code = oj_compiler::json::to_esm(&source, &url_owned)
                .map_err(|err| format!("compile error:\n{err}"))?;
            return Ok(CachedModule {
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
            });
        }
        if is_css {
            let mut css_deps = css_deps;
            let resolve = css_resolve.as_ref();
            let css_src = if oj_css::is_sass(&url_owned) && !sass_precompiled {
                oj_css::compile_sass_collecting(
                    &source,
                    &oj_css::SassOptions {
                        load_dir: Some(&dir),
                        additional_data: sass_data.as_deref(),
                        load_paths: &sass_load_paths,
                        resolve,
                    },
                    &mut css_deps,
                )?
            } else {
                source.clone()
            };
            // Plain `@import`s are inlined (postcss-import parity) so the injected
            // stylesheet does not @import a bare specifier or a wrong-relative url.
            let css_src = if imports_inlined {
                css_src
            } else {
                oj_css::inline_imports_collecting(&css_src, &file_owned, &resolve, &mut css_deps)?
            };
            let output =
                oj_css::compile_css_dev(&url_owned, &css_src, css_dev_sourcemap, &resolve)?;
            // The pulled-in files enter the graph as this sheet's imports (Vite's
            // file-only entries via addWatchFile): an edit to one hot-updates
            // this sheet, and its stamp folds into the compile key so the sheet
            // recompiles instead of serving the cached css.
            // Sass sometimes registers the sheet itself as a dep (Vite filters
            // it too, css.ts); a self-edge would be a bogus import cycle.
            let mut dep_imports: Vec<String> = css_deps
                .iter()
                .filter(|p| p.starts_with(&root) && **p != file_owned)
                .map(|p| url_of(&root, p))
                .collect();
            dep_imports.sort();
            dep_imports.dedup();
            // A CSS module exports its class map, which changes on edit, so it
            // cannot self-accept (Vite's css-analysis): the update climbs to the
            // importing component, whose re-import fetches the new exports.
            let is_css_module = output.exports.is_some();
            return Ok(CachedModule {
                is_boundary: !is_css_module,
                hot: None,
                kind: "css".into(),
                code: output.css,
                map_json: None,
                imports: dep_imports,
                require_map: Vec::new(),
                css_exports: output.exports.unwrap_or_default(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
                import_bindings: Vec::new(),
            });
        }
        // The first relative import nothing on disk satisfies. Vite's import
        // analysis fails the transform for it ("Failed to resolve import ...");
        // shipping the specifier unchanged would only surface as a 404 in the
        // browser, with no overlay and no recovery when the file is created.
        let unresolved: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let rewrite_with = |spec: &str, resolver: &OjResolver| {
            if spec == "virtual:oj-routes" {
                return Some("/@oj/routes.js".to_string());
            }
            if virtual_ids.contains(spec) {
                return Some(format!("/@virtual/{spec}"));
            }
            // Vite runs the plugins' resolveId before its own resolver for every
            // import. A relative / absolute import a plugin's resolveId filter
            // claims goes to the plugins first (`./icon.svg?react` remaps); the
            // /@id/ route falls back to the disk resolver when they decline.
            if !is_bare_specifier(spec) && resolve_id_res.iter().any(|re| re.is_match(spec)) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            if let Some(id) = jsx_overrides.get(spec) {
                return Some(format!("/@presolve/{}", hex_encode(id)));
            }
            if let Some(meta) = dep_map.get(spec) {
                if !meta.needs_interop {
                    return Some(meta.url.clone());
                }
            }
            if let Some(url) =
                rewrite_specifier(&root, &dir, resolver, &fs_allow, &dir_cache, spec, true)
            {
                // `.svg` resolves to `<url>?url` (asset). When a transform plugin is
                // active (vite-plugin-svgr), leave the svg unmarked instead so it
                // routes through the compile path and svgr can componentize it, as
                // Vite does (it marks svg imports `?import`, not `?url`); an svg svgr
                // does not match falls back to a URL asset there.
                if svgr_active {
                    if let Some(base) = url.strip_suffix(".svg?url") {
                        return Some(format!("{base}.svg"));
                    }
                }
                // Vite's importAnalysis appends `?t=<lastHMRTimestamp>` to an import
                // of a module an HMR update invalidated, so the re-fetched importer
                // loads the dependency's new version instead of the browser's cached
                // instance (only boundaries are named in the update itself).
                let stamp = hmr_state
                    .graph
                    .lock()
                    .unwrap()
                    .hmr_timestamp(Path::new(url.split('?').next().unwrap_or(&url)));
                return Some(stamp_import_url(&url, stamp));
            }
            if plugin_fallback && is_bare_specifier(spec) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            // A bare specifier no plugin can claim (there is no plugin fallback
            // here) fails the same way: Vite's importAnalysis errors for it
            // instead of shipping the bare name for the browser to reject. SSR
            // keeps Vite's `if (ssr) return [url, null]`: Node reports it.
            if !is_dep
                && !is_server
                && (relative_import_missing(&dir, resolver, spec)
                    || bare_import_unresolved(&dir, resolver, spec))
            {
                unresolved
                    .borrow_mut()
                    .get_or_insert_with(|| spec.to_string());
            }
            None
        };
        let mut rewrite = |spec: &str| rewrite_with(spec, &resolver);
        // The import specifiers whose named imports must read off the CJS
        // value (module.exports) instead of linking as ESM bindings, shared
        // by app source and served ESM deps (an ESM dep importing an
        // unbundled UMD sibling, the proj4 -> geographiclib-geodesic shape,
        // strict-links the same way app source does).
        let cjs_interop_url = |spec: &str| {
            // node builtins are browser-externalized to a stub with no
            // named exports; interop so `import { X } from "node:..."`
            // reads X off it (undefined) instead of failing to link.
            if is_node_builtin(spec) {
                return Some(format!("/@id/{}", hex_encode(spec)));
            }
            // lingui macro entrypoints go to the shim (which has real
            // named exports), never through default-access interop.
            if is_lingui_macro_specifier(spec) {
                return None;
            }
            if let Some(m) = dep_map.get(spec).filter(|m| m.needs_interop) {
                return Some(m.url.clone());
            }
            // A directly-served bare CJS dep (not pre-bundled): rewrite
            // `import { x } from "dep"` to read x off the default export,
            // so runtime-assigned CJS exports resolve. Vite pre-bundles
            // these; oj interops at the importer instead. Restricted to
            // node_modules so aliased app source (`~/x`, `@/x`, which
            // is_bare_specifier also matches) is never treated as a dep.
            if is_bare_specifier(spec) && dep_map.get(spec).is_none() {
                if let Ok(resolved) = resolver.resolve(&dir, spec) {
                    let in_node_modules = resolved
                        .components()
                        .any(|c| c.as_os_str() == "node_modules");
                    // optimizeDeps.needsInterop forces the interop
                    // rewrite even when static analysis reads the dep
                    // as ESM (its real exports only appear at runtime).
                    if in_node_modules
                        && (is_cjs_dep_file(&resolved)
                            || pkg_bundle::needs_forced_interop(&resolved))
                    {
                        fs_allow.lock().unwrap().insert(package_root(&resolved));
                        // With partial bundling on this is the /@oj-pkg
                        // bundle URL, which exports __cjs_exports too, so
                        // the destructured interop still reads names off it.
                        return Some(dep_serve_url(&resolved, &root));
                    }
                }
            }
            None
        };
        let output = if is_dep {
            if oj_compiler::cjs::has_module_syntax_pub(&file_owned, &source) {
                // Gated on a cheap bare-import scan: deps overwhelmingly
                // import their own relative files, and the rewrite would
                // otherwise add a second full parse to every served ESM dep.
                // The scan finds the bare specifiers without a parse; the
                // rewrite (a full parse) runs only when one of them actually
                // maps to an interop URL, so a dep file whose bare imports
                // are all ESM peers (react, tslib) skips it entirely.
                let dep_interop = if oj_compiler::interop::bare_import_specifiers(&source)
                    .iter()
                    .any(|spec| cjs_interop_url(spec).is_some())
                {
                    oj_compiler::interop::rewrite_cjs_interop_logged(
                        &source,
                        &file_owned,
                        &cjs_interop_url,
                        &mut warn_interop_once,
                    )
                } else {
                    None
                };
                let dep_src = dep_interop.as_deref().unwrap_or(&source);
                oj_compiler::cjs::compile_dep(&file_owned, &url_owned, dep_src, &mut rewrite)
            } else {
                let dep_interop = interop_node_builtins(&source, &file_owned);
                let dep_src = dep_interop.as_deref().unwrap_or(&source);
                // A CommonJS dep's `require()`s resolve with the `require`
                // condition (Vite's getConditions for a requirer), so a dual
                // package hands it its CJS build (`module.exports = fn`), not
                // the ESM one the interop would wrap as `{ default: fn }`.
                oj_compiler::cjs::compile_dep(
                    &file_owned,
                    &url_owned,
                    dep_src,
                    &mut |spec: &str| rewrite_with(spec, &require_resolver),
                )
            }
        } else {
            let interopped = oj_compiler::interop::rewrite_cjs_interop_logged(
                &source,
                &file_owned,
                &cjs_interop_url,
                &mut warn_interop_once,
            );
            let mut opts = if is_svelte {
                oj_compiler::CompileOptions {
                    refresh: false,
                    ..oj_compiler::CompileOptions::dev()
                }
            } else {
                oj_compiler::CompileOptions::dev()
            };
            opts.jsx = jsx_config;
            // The cache key already folded this decision (from the original
            // path); the compile must never re-derive it from a synthetic one.
            opts.class_field_set_semantics = Some(class_field_semantics);
            oj_compiler::compile_module_with_maps(
                &file_owned,
                interopped.as_deref().unwrap_or(&source),
                &opts,
                Some(&mut rewrite),
                &plugin_maps,
            )
        }
        .map_err(|err| format!("compile error:\n{err}"))?;
        if let Some(spec) = unresolved.borrow().as_ref() {
            return Err(unresolved_import_error(&root, &file_owned, &source, spec));
        }
        Ok(CachedModule {
            is_boundary: is_svelte || (!is_dep && output.has_refresh_registrations()),
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
            kind: if is_svelte {
                "svelte".into()
            } else {
                String::new()
            },
            require_map: Vec::new(),
            css_exports: Vec::new(),
        })
    })
    .await;

    let module = match compiled {
        Ok(Ok(mut module)) => {
            module.watch_files = plugin_watch_files;
            Arc::new(module)
        }
        Ok(Err(err)) => {
            if is_unresolved_import_error(&err) {
                let clean = url.split('?').next().unwrap_or(url).to_string();
                state.resolve_failed.lock().unwrap().insert(clean);
                // Vite clears the importer's isSelfAccepting here (#9534) so the
                // update a later `create` triggers climbs to a boundary the page
                // did load rather than stopping at a module it never evaluated.
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
    if state.persistent_cache {
        let _ = state
            .cache_writes
            .try_send((key.clone(), Arc::clone(&module)));
    }
    memory_put(state, url, &key, &module);
    register_in_graph(state, url, &module);
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
            watched.insert(PathBuf::from(p));
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
        // Dependencies this module dropped that nothing imports any more: the
        // client runs their `hot.prune` callbacks (a stylesheet removes its
        // <style>), and they are stamped so a later re-import re-runs them, as
        // Vite's handlePrunedModules does after importAnalysis.
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

/// Expands `{a,b}` groups the way picomatch/Vite treat them; the `glob` crate
/// has no brace support, so `*.{key,pem}` (Vite's own default deny shape)
/// would otherwise compile to a literal that matches nothing.
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
    // Match case-insensitively: a deny list is a security control, and on a
    // case-insensitive filesystem (macOS, Windows) `.ENV` opens the same bytes
    // as `.env`, so a case-sensitive glob would leak the file. Denying a
    // superset on case-sensitive filesystems is the safe direction. Other
    // options stay at glob's defaults, so only case behavior changes.
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

// The browser stamps every request with Sec-Fetch-Dest describing what it will
// do with the bytes: `style` (<link rel=stylesheet>), `image` (<img>), `font`
// (@font-face), media. Those want the raw resource. A JS `import` fetches the
// module with dest `script`/`empty`/worker, which wants the JS-module form: a
// style-injecting module for CSS, a URL-exporting module for an asset. Vite draws
// the same line; a `.css` reached from JS is served as JS, not text/css.
/// An asset url requested by a module import: Vite marks those `?import`, and a
/// browser sets `sec-fetch-dest: script` for an `import` of the url. Anything
/// else (a fetch(), curl, the Start proxy, an <img>) gets the file's bytes, as
/// Vite's static middleware serves them.
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

// Assets that, when imported from JS, resolve to a URL-exporting module (Vite's
// default asset handling, case-insensitive). svg is excluded here: it is routed
// through the compile path so vite-plugin-svgr can componentize it, falling back
// to a URL module there.
pub fn is_importable_asset_ext(ext: &str) -> bool {
    oj_compiler::assets::is_asset_ext(ext) && !ext.eq_ignore_ascii_case("svg")
}

// Node core modules. When one reaches the browser graph (usually via config-time
// tooling a dep drags along), Vite serves a browser-externalized stub rather than
// 404ing the whole module chain; oj does the same so the app still mounts. Pub:
// the Start module host also consults it, because on the SSR side a builtin
// outranks an installed polyfill package of the same name (Vite's fetchModule
// checks isBuiltin before resolving), exactly as Node itself behaves.
pub fn is_node_builtin(spec: &str) -> bool {
    // Vite's isNodeBuiltin: anything under the `node:` scheme is a builtin (this
    // covers node:sqlite, node:sea, node:test and whatever Node adds next); the
    // list below is `module.builtinModules` for the bare (scheme-less) names.
    if spec.starts_with("node:") {
        return true;
    }
    let base = spec.split('/').next().unwrap_or(spec);
    if base.starts_with("_http_") || base.starts_with("_stream_") || base.starts_with("_tls_") {
        return true;
    }
    matches!(
        base,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "domain"
            | "events"
            | "fs"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "repl"
            | "stream"
            | "string_decoder"
            | "sys"
            | "timers"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
}

// Vite's searchForWorkspaceRoot: walk up from the app root and stop at the first
// workspace marker (pnpm-workspace.yaml / lerna.json, or a package.json with a
// `workspaces` field); otherwise searchForPackageRoot, the NEAREST ancestor with
// a package.json (the root itself, normally). `.git` is deliberately not a
// marker (Vite comments it out): a project nested somewhere inside a repository
// must not expose the whole repository over /@fs by default. oj seeds
// server.fs.allow with this, matching Vite's default.
pub(crate) fn workspace_root(root: &Path) -> PathBuf {
    // Vite's hasWorkspacePackageJSON / hasWorkspaceDenoJSON: the PARSED field,
    // truthy by JS rules — a dependency literally named "workspaces" must not
    // widen the served root, and a deno.jsonc counts only while it is also
    // valid JSON (Vite skips full JSONC parsing).
    let field_truthy = |file: &Path, field: &str| -> bool {
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
    };
    let mut pkg_root: Option<PathBuf> = None;
    let mut dir = root;
    loop {
        if dir.join("pnpm-workspace.yaml").exists() || dir.join("lerna.json").exists() {
            return dir.to_path_buf();
        }
        if dir.join("package.json").exists() {
            if field_truthy(&dir.join("package.json"), "workspaces") {
                return dir.to_path_buf();
            }
            if pkg_root.is_none() {
                pkg_root = Some(dir.to_path_buf());
            }
        }
        if field_truthy(&dir.join("deno.json"), "workspace")
            || field_truthy(&dir.join("deno.jsonc"), "workspace")
        {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return pkg_root.unwrap_or_else(|| root.to_path_buf()),
        }
    }
}

// Rewrite `import { X } from "node:builtin"` to read X off the browser-externalized
// stub (undefined) instead of a native named import that fails to link, matching
// Vite's importAnalysis interop for browser-external modules. Returns None when the
// source imports no node builtins. Applied on every compile path so deps and app
// source interop consistently.
// Pre-resolve a module's static imports (same resolver ctx.resolve uses) into a
// {spec: id|null} JSON map, handed to the plugin transform so a plugin's per-import
// `this.resolve` is a local lookup instead of a host round-trip. This is what keeps
// import-protection's transform (a resolve per import) from being thousands of IPC
// round-trips per page.
pub(crate) fn resolved_imports_json(
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    source: &str,
    file: &Path,
) -> String {
    let dir = file.parent().unwrap_or(file);
    let mut map = serde_json::Map::new();
    for spec in oj_compiler::imports(source, file) {
        // Node builtins never resolve to a file (they're browser-externalized via
        // interop); skip them so the resolver doesn't log a "cannot resolve" warning
        // per app module. A plugin's this.resolve falls back to the host for these.
        if is_node_builtin(&spec) {
            continue;
        }
        let val = match resolver.resolve(dir, &spec) {
            Ok(p) => {
                // The map hands these ids to the plugin transform; if the transform
                // keeps the import, the browser fetches it from /@fs, so allow-list
                // its package root now (rewrite_specifier does the same on its path).
                if p.components().any(|c| c.as_os_str() == "node_modules") || !p.starts_with(dir) {
                    fs_allow.lock().unwrap().insert(package_root(&p));
                }
                serde_json::Value::String(p.display().to_string())
            }
            Err(_) => serde_json::Value::Null,
        };
        map.insert(spec, val);
    }
    serde_json::Value::Object(map).to_string()
}

/// A stable, per-file condition (the bare star re-export of a CJS dep) would
/// otherwise re-warn on every recompile: HMR invalidations, cache misses,
/// restarts. Once per distinct message, like Vite's deduping logger.
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
