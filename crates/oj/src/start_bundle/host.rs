// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's Vite plugins inside a Start bundle: one plugin host per Vite
//! environment (client, ssr, or the Cloudflare Worker's), driven from
//! rolldown the way Vite's plugin container is driven from its bundler.
//!
//! resolveId keeps Vite's order around its core resolver: the hooks that run
//! before it (`enforce: "pre"`, `order: "pre"`) see every specifier first,
//! rolldown's own resolution comes next, and the remaining hooks see only
//! what it left, each answer kept whole (`external` included). A plugin's
//! `this.resolve` that no plugin answers ends in this bundle's resolution
//! (the host's `bundlerResolve` reverse RPC), as it ends in Rollup's.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oj_server::plugins::{PluginHost, ResolvedId};
use rolldown_common::{ModuleType, ResolvedExternal};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookRenderChunkArgs, HookRenderChunkOutput,
    HookRenderChunkReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookTransformArgs, HookTransformOutput, HookTransformReturn, Plugin, PluginContext,
    PluginContextResolveOptions, SharedLoadPluginContext, SharedTransformPluginContext,
};

use super::facts::Facts;
use super::Command;
use crate::build::EmitState;

/// The rolldown context a host's `this.resolve` fallback resolves through:
/// the bundle the host currently serves (set at its buildStart).
#[derive(Clone, Default)]
pub(crate) struct BundlerCtx(Arc<Mutex<Option<PluginContext>>>);

impl BundlerCtx {
    fn set(&self, ctx: &PluginContext) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
    }

    fn get(&self) -> Option<PluginContext> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Answers `host`'s reverse RPCs for as long as it lives: `bundlerResolve`
/// through the bundle in `ctx` (`{ fallback: true }` while no bundle runs,
/// so oj's resolver answers instead).
pub(crate) fn serve_bundler_resolve(host: &Arc<PluginHost>) -> BundlerCtx {
    let ctx = BundlerCtx::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<oj_server::plugins::HostRpc>();
    host.set_rpc_request_sender(tx);
    let weak = Arc::downgrade(host);
    let slot = ctx.clone();
    tokio::spawn(async move {
        while let Some(rpc) = rx.recv().await {
            let Some(host) = weak.upgrade() else {
                return;
            };
            let slot = slot.clone();
            tokio::spawn(async move {
                let result = answer(&rpc, slot.get()).await;
                host.rpc_reply(rpc.id, result);
            });
        }
    });
    ctx
}

async fn answer(
    rpc: &oj_server::plugins::HostRpc,
    ctx: Option<PluginContext>,
) -> Result<serde_json::Value, String> {
    if rpc.method != "bundlerResolve" {
        return Err(format!("unknown host rpc: {}", rpc.method));
    }
    let Some(ctx) = ctx else {
        return Ok(serde_json::json!({ "fallback": true }));
    };
    let arg = |i: usize| rpc.args.get(i).and_then(|v| v.as_str()).unwrap_or("");
    let (source, importer) = (arg(0), arg(1));
    let is_entry = rpc.args.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
    let resolved = ctx
        .resolve(
            source,
            (!importer.is_empty()).then_some(importer),
            Some(PluginContextResolveOptions {
                is_entry,
                skip_self: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(match resolved {
        Ok(r) => serde_json::json!({
            "id": r.id.as_str(),
            "external": !matches!(r.external, ResolvedExternal::Bool(false)),
        }),
        Err(_) => serde_json::Value::Null,
    })
}

/// A Start bundle's view of one plugin host.
pub(crate) struct StartHostPlugin {
    host: Arc<PluginHost>,
    /// The server build's client host: a load the server host leaves goes to
    /// it, as some ssr plugins expect state Vite shares between environments.
    fallback: Option<Arc<PluginHost>>,
    bundler_ctx: BundlerCtx,
    emit: Arc<EmitState>,
    command: Command,
    root: PathBuf,
    pre: bool,
    post: bool,
    /// False for a bundle nested in another (a worker entry): the host's
    /// lifecycle hooks belong to the bundle that owns it.
    lifecycle: bool,
    render_chunk_enabled: Arc<tokio::sync::OnceCell<bool>>,
    warned_virtual: Mutex<HashSet<String>>,
    map_warnings: Arc<crate::build::MapWarnings>,
}

impl std::fmt::Debug for StartHostPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartHostPlugin")
    }
}

impl StartHostPlugin {
    pub fn new(
        host: Arc<PluginHost>,
        bundler_ctx: BundlerCtx,
        facts: &Facts,
        command: Command,
        root: &Path,
        emit: Arc<EmitState>,
    ) -> Self {
        Self {
            host,
            fallback: None,
            bundler_ctx,
            emit,
            command,
            root: root.to_path_buf(),
            pre: facts.resolve_id_pre > 0,
            post: facts.resolve_id_post > 0,
            lifecycle: true,
            render_chunk_enabled: Arc::new(tokio::sync::OnceCell::new()),
            warned_virtual: Mutex::new(HashSet::new()),
            map_warnings: Arc::default(),
        }
    }

    pub fn with_fallback(mut self, fallback: Arc<PluginHost>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// A plugin's buildStart threw. `oj build` fails, as `vite build` does.
    /// The dev server logs it once per host and bundles on: the host settles
    /// buildStart once per session, so failing here would fail every later
    /// rebundle too, with nothing the user could fix short of a restart.
    fn build_start_failed(&self, host: &Arc<PluginHost>, e: &str) -> anyhow::Result<()> {
        if self.command == Command::Build {
            anyhow::bail!("plugin buildStart failed:\n{e}");
        }
        static LOGGED: std::sync::LazyLock<Mutex<HashSet<usize>>> =
            std::sync::LazyLock::new(Default::default);
        let key = Arc::as_ptr(host) as usize;
        if LOGGED.lock().unwrap_or_else(|p| p.into_inner()).insert(key) {
            eprintln!(
                "{}: plugin buildStart failed (skipped): {e}",
                oj_server::oj_brand()
            );
        }
        Ok(())
    }

    pub fn nested(mut self) -> Self {
        self.lifecycle = false;
        self
    }
}

/// A plugin resolveId answer as rolldown's.
fn hook_output(r: ResolvedId) -> HookResolveIdOutput {
    let external = match r.external.as_ref() {
        Some(serde_json::Value::String(s)) if s == "absolute" => Some(ResolvedExternal::Absolute),
        Some(serde_json::Value::String(s)) if s == "relative" => Some(ResolvedExternal::Relative),
        _ if r.is_external() => Some(ResolvedExternal::Bool(true)),
        _ => None,
    };
    HookResolveIdOutput {
        external,
        side_effects: r.module_side_effects.map(|s| {
            if s {
                rolldown_common::side_effects::HookSideEffects::True
            } else {
                rolldown_common::side_effects::HookSideEffects::False
            }
        }),
        ..HookResolveIdOutput::from_id(r.id)
    }
}

/// Specifiers oj's own plugins own: asset and stylesheet imports, which
/// Vite's core resolver answers before a normal plugin's resolveId sees them.
fn asset_owned(spec: &str) -> bool {
    super::assets::asset_tag(spec, false).is_some()
}

/// The language of a file read for an id, by the file's extension: `.mts` and
/// `.cts` are TypeScript, never JSX.
fn file_module_type(path: &str) -> ModuleType {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("ts" | "mts" | "cts") => ModuleType::Ts,
        Some("tsx") => ModuleType::Tsx,
        Some("jsx") => ModuleType::Jsx,
        _ => ModuleType::Js,
    }
}

/// The language of plugin-loaded code: a tolerant superset, since a load may
/// return JSX for any id: TSX for a TypeScript-family id (types and JSX both
/// parse), JSX otherwise.
fn plugin_module_type(id: &str) -> ModuleType {
    let path = id.split(['?', '#']).next().unwrap_or(id);
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("ts" | "mts" | "cts" | "tsx") => ModuleType::Tsx,
        Some("json") => ModuleType::Json,
        Some("css") => ModuleType::Js,
        _ => ModuleType::Jsx,
    }
}

fn is_script(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

fn is_app_source(path: &str) -> bool {
    !path.contains("/node_modules/")
        && matches!(
            Path::new(path).extension().and_then(|e| e.to_str()),
            Some("js" | "jsx" | "mjs" | "ts" | "tsx")
        )
}

impl Plugin for StartHostPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-plugins")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        use rolldown_plugin::HookUsage;
        let mut usage = HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform;
        if self.lifecycle {
            usage |= HookUsage::BuildStart | HookUsage::RenderStart;
            if self.command == Command::Build {
                usage |= HookUsage::BuildEnd
                    | HookUsage::RenderChunk
                    | HookUsage::GenerateBundle
                    | HookUsage::WriteBundle;
            }
        }
        usage
    }

    async fn build_start(
        &self,
        ctx: &PluginContext,
        _args: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.bundler_ctx.set(ctx);
        // Before any module loads: a plugin compiling on startup (i18n
        // catalogs) has its state ready for the loads it serves. The host runs
        // buildStart once per session, as Vite's container does.
        match self.host.build_start().await {
            Ok(chunks) => crate::build::forward_emitted_chunks(ctx, &chunks, &self.emit),
            Err(e) => self.build_start_failed(&self.host, &e)?,
        }
        if let Some(fallback) = &self.fallback {
            if let Err(e) = fallback.build_start().await {
                self.build_start_failed(fallback, &e)?;
            }
        }
        Ok(())
    }

    fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let spec = args.specifier.to_string();
        let importer = args.importer.map(str::to_string);
        let is_entry = args.is_entry;
        let kind = args.kind;
        // oj's own module ids are answered by the plugins that minted them.
        let ours = spec.starts_with("\0oj-");
        let wants = !ours && self.host.hook_wants_resolve_id(&spec);
        let (pre, post) = (self.pre && wants, self.post && wants && !asset_owned(&spec));
        let host = Arc::clone(&self.host);
        let ctx = ctx.clone();
        async move {
            if !pre && !post {
                return Ok(None);
            }
            let importer_str = importer.as_deref().unwrap_or("");
            let fail = |e: String| anyhow::anyhow!("plugin resolveId failed for {spec}:\n{e}");
            if pre {
                if let Some(r) = host
                    .resolve_id_phase(&spec, importer_str, "pre", is_entry)
                    .await
                    .map_err(fail)?
                {
                    return Ok(Some(hook_output(r)));
                }
            }
            if !post {
                return Ok(None);
            }
            // The core resolver (everything after this plugin) first; the
            // remaining hooks only see what it left.
            let core = ctx
                .resolve(
                    &spec,
                    importer.as_deref(),
                    Some(PluginContextResolveOptions {
                        import_kind: kind,
                        is_entry,
                        skip_self: true,
                        ..Default::default()
                    }),
                )
                .await?;
            if let Ok(found) = core {
                return Ok(Some(HookResolveIdOutput::from_resolved_id(found)));
            }
            Ok(host
                .resolve_id_phase(&spec, importer_str, "post", is_entry)
                .await
                .map_err(fail)?
                .map(hook_output))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = args.id.to_string();
        let host = Arc::clone(&self.host);
        let fallback = self.fallback.clone();
        let wants = !id.starts_with("\0oj-") && self.host.hook_wants_load(&id);
        let dev = self.command == Command::Serve;
        let root = self.root.clone();
        let warned = !self
            .warned_virtual
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone());
        async move {
            if id.starts_with("\0oj-") {
                return Ok(None);
            }
            let clean = id.split(['?', '#']).next().unwrap_or(&id).to_string();
            let on_disk = !id.starts_with('\0')
                && Path::new(&clean).is_absolute()
                && Path::new(&clean).is_file();
            // `?raw`, `?worker`, a stylesheet: oj's asset plugin loads those.
            if on_disk && super::assets::asset_tag(&id, false).is_some() {
                return Ok(None);
            }
            let fail = |e: String| anyhow::anyhow!("plugin load failed for {id}:\n{e}");
            let mut code = None;
            if wants {
                for h in std::iter::once(&host).chain(fallback.iter()) {
                    // The FULL id first: a plugin's load keys on its own query
                    // (`?tsr-split=...`), Vite's contract; then the file.
                    code = h.load(&id).await.map_err(fail)?;
                    if code.is_none() && on_disk && id != clean && is_app_source(&clean) {
                        code = h.load(&clean).await.map_err(fail)?;
                    }
                    if code.is_some() {
                        break;
                    }
                }
            }
            if let Some(code) = code {
                if clean.ends_with(".css") {
                    // A plugin-served virtual stylesheet (UnoCSS's layer
                    // placeholder): a side-effect stub keeps it in the graph.
                    return Ok(Some(HookLoadOutput {
                        code: arcstr::ArcStr::from("export {};\n"),
                        module_type: Some(ModuleType::Js),
                        side_effects: Some(
                            rolldown_common::side_effects::HookSideEffects::NoTreeshake,
                        ),
                        ..Default::default()
                    }));
                }
                let module_type = if on_disk && is_app_source(&clean) {
                    file_module_type(&clean)
                } else {
                    plugin_module_type(&id)
                };
                return Ok(Some(HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(module_type),
                    ..Default::default()
                }));
            }
            // Rollup's contract: a resolved id no load claims is read from
            // disk, the file behind its query (`/abs/route.ts?tsr-split=x`).
            if on_disk && id != clean && is_script(&clean) {
                let source =
                    std::fs::read_to_string(&clean).map_err(|e| anyhow::anyhow!("{clean}: {e}"))?;
                return Ok(Some(HookLoadOutput {
                    code: arcstr::ArcStr::from(source),
                    module_type: Some(file_module_type(&clean)),
                    ..Default::default()
                }));
            }
            // A plugin virtual nothing loaded. The dev bundle stubs it with
            // an empty module so the page still boots (a virtual that needs
            // the build's full graph); a build fails on it, as Vite's does.
            if dev && !on_disk && !Path::new(&clean).is_absolute() {
                if !warned {
                    eprintln!(
                        "oj: plugin virtual \"{id}\" produced no content in the dev client bundle; emitting an empty module. This virtual likely needs the full build graph oj does not run in dev."
                    );
                }
                return Ok(Some(HookLoadOutput {
                    code: arcstr::ArcStr::from(empty_virtual_stub(&root, &id)),
                    module_type: Some(ModuleType::Js),
                    ..Default::default()
                }));
            }
            Ok(None)
        }
    }

    fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let id: &str = args.id;
        let code: &str = args.code;
        let pass = !id.starts_with("\0oj-")
            // A `.svg` was componentized (or left an asset) by its load, where
            // the plugins already saw the markup.
            && !id.split('?').next().unwrap_or(id).ends_with(".svg")
            && self.host.hook_wants_transform(id, code);
        let host = Arc::clone(&self.host);
        let emit = Arc::clone(&self.emit);
        let sourcemap = ctx.inner.options().is_sourcemap_enabled();
        let warnings = (self.command == Command::Build).then(|| Arc::clone(&self.map_warnings));
        async move {
            if !pass {
                return Ok(None);
            }
            let t = host
                .transform(code, id, "{}")
                .await
                .map_err(|e| anyhow::anyhow!("plugin transform failed for {id}:\n{e}"))?;
            crate::build::forward_emitted_chunks(&ctx.inner, &t.chunks, &emit);
            Ok((t.code != code).then(|| HookTransformOutput {
                map: crate::build::host_chain_map(
                    sourcemap,
                    &t,
                    crate::build::ChainHook::Transform,
                    warnings.as_deref(),
                ),
                code: Some(t.code),
                ..Default::default()
            }))
        }
    }

    async fn build_end(
        &self,
        _ctx: &PluginContext,
        args: Option<&rolldown_plugin::HookBuildEndArgs<'_>>,
    ) -> rolldown_plugin::HookNoopReturn {
        let error = args.map(|a| {
            a.errors
                .iter()
                .map(|e| e.to_diagnostic().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        });
        self.host
            .build_end(error.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("plugin buildEnd failed:\n{e}"))
    }

    async fn render_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookRenderStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.host
            .render_start()
            .await
            .map_err(|e| anyhow::anyhow!("plugin renderStart failed:\n{e}"))
    }

    fn render_chunk(
        &self,
        _ctx: &PluginContext,
        args: &HookRenderChunkArgs<'_>,
    ) -> impl std::future::Future<Output = HookRenderChunkReturn> + Send {
        let host = Arc::clone(&self.host);
        let enabled = Arc::clone(&self.render_chunk_enabled);
        let code = Arc::clone(&args.code);
        let sourcemap = args.options.is_sourcemap_enabled();
        let warnings = (self.command == Command::Build).then(|| Arc::clone(&self.map_warnings));
        let chunk_json = if enabled.get() == Some(&false) {
            String::new()
        } else {
            crate::build::serialize_rendered_chunk(&args.chunk)
        };
        async move {
            let on = *enabled
                .get_or_init(|| async { host.has_render_chunk().await })
                .await;
            if !on {
                return Ok(None);
            }
            match host
                .render_chunk(&code, &chunk_json)
                .await
                .map_err(|e| anyhow::anyhow!("plugin renderChunk failed:\n{e}"))?
            {
                t if t.code != *code => Ok(Some(HookRenderChunkOutput {
                    map: crate::build::host_chain_map(
                        sourcemap,
                        &t,
                        crate::build::ChainHook::RenderChunk,
                        warnings.as_deref(),
                    ),
                    code: t.code,
                })),
                _ => Ok(None),
            }
        }
    }

    async fn generate_bundle(
        &self,
        ctx: &PluginContext,
        args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        // `this.getFileName(ref)` of a chunk a plugin emitted reads the hashed
        // name rolldown gave it.
        let refs: Vec<(String, String)> = self
            .emit
            .chunk_refs
            .lock()
            .unwrap()
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        if !refs.is_empty() {
            let mut map = serde_json::Map::new();
            for (oj_ref, rd_ref) in refs {
                if let Ok(name) = ctx.get_file_name(&rd_ref) {
                    map.insert(oj_ref, serde_json::Value::String(name.to_string()));
                }
            }
            if !map.is_empty() {
                let _ = self
                    .host
                    .seed_chunk_names(&serde_json::Value::Object(map).to_string())
                    .await;
            }
        }
        if !self.host.has_generate_bundle().await {
            return Ok(());
        }
        let bundle_json =
            crate::build::serialize_bundle_with_vite_manifest(args.bundle, &self.root);
        let mutated = self
            .host
            .generate_bundle(&bundle_json, args.is_write)
            .await
            .map_err(|e| anyhow::anyhow!("plugin generateBundle failed:\n{e}"))?;
        if let Some(mutated) = mutated {
            crate::build::apply_bundle_mutations(args.bundle, &mutated);
        }
        Ok(())
    }

    async fn write_bundle(
        &self,
        _ctx: &PluginContext,
        args: &mut rolldown_plugin::HookWriteBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        if !self.host.has_write_bundle().await {
            return Ok(());
        }
        let bundle_json = crate::build::serialize_bundle(args.bundle);
        self.host
            .write_bundle(&bundle_json, true)
            .await
            .map_err(|e| anyhow::anyhow!("plugin writeBundle failed:\n{e}"))
    }
}

/// A module for a plugin virtual nothing loaded in dev: every name the app
/// imports from it, as `undefined`, so the importers still link.
pub(crate) fn empty_virtual_stub(root: &Path, id: &str) -> String {
    let original = id.trim_start_matches('\0');
    let pattern = format!(
        r#"import\s+(?:type\s+)?[^;{{]*\{{([^}}]*)\}}[^;]*?from\s*["']{}["']"#,
        regex::escape(original)
    );
    let mut names: Vec<String> = Vec::new();
    if let Ok(re) = regex::Regex::new(&pattern) {
        let mut stack = vec![root.join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "node_modules" || name.starts_with('.') {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if !matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts")
                ) {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if !src.contains(original) {
                    continue;
                }
                for caps in re.captures_iter(&src) {
                    for spec in caps[1].split(',') {
                        let spec = spec.trim();
                        if spec.is_empty() || spec.starts_with("type ") {
                            continue;
                        }
                        let name = spec.split(" as ").next().unwrap_or("").trim();
                        let ident = !name.is_empty()
                            && name
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
                            && name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                        if ident && name != "default" && !names.iter().any(|n| n == name) {
                            names.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut out = String::from("export default undefined;\n");
    for n in names {
        out.push_str(&format!("export const {n} = undefined;\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_loads_parse_tolerantly_and_files_as_themselves() {
        assert_eq!(plugin_module_type("virtual:routes"), ModuleType::Jsx);
        assert_eq!(plugin_module_type("/a/b.ts?x"), ModuleType::Tsx);
        assert_eq!(plugin_module_type("/a/b.mts"), ModuleType::Tsx);
        assert_eq!(plugin_module_type("\0data.json"), ModuleType::Json);
        assert_eq!(file_module_type("/a/b.mts"), ModuleType::Ts);
        assert_eq!(file_module_type("/a/b.jsx"), ModuleType::Jsx);
        assert_eq!(file_module_type("/a/b.mjs"), ModuleType::Js);
    }

    #[test]
    fn an_empty_virtual_exports_every_name_the_app_imports() {
        let dir = std::env::temp_dir().join(format!("oj-start-stub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(
            dir.join("src/a.ts"),
            "import { one, two as t, type Skip } from \"virtual:thing\";\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/deep/b.tsx"),
            "import def, { three } from 'virtual:thing';\nimport { other } from 'virtual:else';\n",
        )
        .unwrap();
        let stub = empty_virtual_stub(&dir, "\0virtual:thing");
        assert!(stub.starts_with("export default undefined;\n"), "{stub}");
        for name in ["one", "two", "three"] {
            assert!(
                stub.contains(&format!("export const {name} = undefined;")),
                "{stub}"
            );
        }
        assert!(!stub.contains("Skip") && !stub.contains("other"), "{stub}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
