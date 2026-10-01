// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Rolldown fallback for partial bundling: when pkg_bundle bails, bundle that one
// package with the embedded rolldown, externalizing cross-package imports to their
// served URLs (singletons preserved). Prototype, gated behind OJ_PB_ROLLDOWN.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat};
use rolldown_common::{Output, Platform, ResolvedExternal};
use rolldown_plugin::__inner::SharedPluginable;
use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, HookUsage, Plugin, PluginContext,
};

use oj_resolver::OjResolver;

use crate::{dep_serve_url, hex_encode, is_node_builtin, normalize, package_root};

/// Is the rolldown fallback enabled? Requires both partial bundling and the
/// prototype opt-in, so the concatenator stays the default path.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::partial_bundle_enabled()
            && std::env::var("OJ_PB_ROLLDOWN").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

// Packages the concatenator bundles but breaks at runtime (it never bails, so the
// fallback never fires): curated force-through-rolldown list; extend with OJ_PB_ROLLDOWN_FORCE=a,b.
const BUILTIN_FORCE: &[&str] = &["object-inspect"];

fn force_set() -> &'static HashSet<String> {
    static S: OnceLock<HashSet<String>> = OnceLock::new();
    S.get_or_init(|| {
        let mut set: HashSet<String> = BUILTIN_FORCE.iter().map(|s| s.to_string()).collect();
        if let Ok(v) = std::env::var("OJ_PB_ROLLDOWN_FORCE") {
            for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                set.insert(name.to_string());
            }
        }
        set
    })
}

/// Skip the concatenator: the curated known-broken list, `OJ_PB_ROLLDOWN_FORCE`
/// additions, and packages named in `optimizeDeps.include`.
pub fn is_forced(entry: &Path) -> bool {
    crate::pkg_bundle::package_name(entry).is_some_and(|n| force_set().contains(&n))
        || crate::pkg_bundle::is_include_forced(entry)
}

// Cache of emitted chunks keyed by their served path (`/@oj-pkg/<filename>`), so
// a package's sibling chunks are already present when the browser requests them.
fn chunk_cache() -> &'static Mutex<HashMap<String, Bytes>> {
    static C: OnceLock<Mutex<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A chunk previously emitted by a rolldown bundle, if any.
pub fn cached_chunk(path: &str) -> Option<Bytes> {
    chunk_cache().lock().unwrap().get(path).cloned()
}

/// (entries, bytes) resident in the emitted-chunk map, for /@oj/debug/mem.
pub fn debug_stats() -> (usize, usize) {
    let m = chunk_cache().lock().unwrap();
    (m.len(), m.values().map(Bytes::len).sum())
}

fn store_chunk(path: String, code: Bytes) {
    chunk_cache().lock().unwrap().insert(path, code);
}

/// Bundle one package with rolldown, caching every emitted chunk under its served
/// path. Returns the entry chunk's code; None = caller keeps the per-file fallback.
pub async fn build(entry: &Path, root: &Path, resolver: Arc<OjResolver>) -> Option<Bytes> {
    let hex = hex_encode(&entry.to_string_lossy());
    let pkg_root = package_root(entry);

    let plugin = ExternalizePlugin {
        pkg_root,
        root: root.to_path_buf(),
        resolver,
    };
    let plugins: Vec<SharedPluginable> = vec![Arc::new(plugin)];

    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                // `[name]` -> the entry's hex, so chunk filenames are unique per
                // package and never collide across concurrently-bundled deps.
                name: Some(hex.clone()),
                import: entry.to_string_lossy().into_owned(),
            }]),
            cwd: Some(root.to_path_buf()),
            platform: Some(Platform::Browser),
            format: Some(OutputFormat::Esm),
            entry_filenames: Some("[name].js".to_string().into()),
            chunk_filenames: Some("[name]-[hash].js".to_string().into()),
            define: Some(
                [
                    ("process.env.NODE_ENV", "\"development\""),
                    ("import.meta.env.DEV", "true"),
                    ("import.meta.env.PROD", "false"),
                    ("import.meta.env.SSR", "false"),
                    ("import.meta.env.MODE", "\"development\""),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ),
            ..Default::default()
        })
        .build()
        .ok()?;

    let outcome = bundler.generate().await;
    let _ = bundler.close().await;
    let output = outcome.ok()?;

    let prefix = crate::pkg_bundle::PKG_PREFIX;
    let mut entry_code: Option<Bytes> = None;
    for asset in &output.assets {
        if let Output::Chunk(c) = asset {
            let code = Bytes::from(c.code.clone());
            let served = format!("{prefix}{}", c.filename);
            store_chunk(served, code.clone());
            if c.is_entry {
                entry_code = Some(code);
            }
        }
    }
    let entry_code = entry_code?;
    // The app requests the entry at `/@oj-pkg/<hex>` (no extension); serve the
    // entry chunk's code there too.
    store_chunk(format!("{prefix}{hex}"), entry_code.clone());
    Some(entry_code)
}

/// Rewrites cross-package imports to their own served URLs and leaves everything
/// inside the package for rolldown to bundle.
struct ExternalizePlugin {
    pkg_root: PathBuf,
    root: PathBuf,
    resolver: Arc<OjResolver>,
}

impl std::fmt::Debug for ExternalizePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalizePlugin")
            .field("pkg_root", &self.pkg_root)
            .finish()
    }
}

/// Is `resolved` a file inside `pkg_root` (and not down a nested node_modules)?
fn inside_package(pkg_root: &Path, resolved: &Path) -> bool {
    let root_c = std::fs::canonicalize(pkg_root).unwrap_or_else(|_| pkg_root.to_path_buf());
    let res_c = std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf());
    match res_c.strip_prefix(&root_c) {
        Ok(rel) => !rel.components().any(|c| c.as_os_str() == "node_modules"),
        Err(_) => false,
    }
}

fn external(id: String) -> HookResolveIdOutput {
    HookResolveIdOutput {
        id: id.into(),
        external: Some(ResolvedExternal::Bool(true)),
        ..Default::default()
    }
}

impl Plugin for ExternalizePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:pkg-externalize")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let spec = args.specifier.to_string();
        let importer = args.importer.map(str::to_string);
        let pkg_root = self.pkg_root.clone();
        let root = self.root.clone();
        let resolver = Arc::clone(&self.resolver);
        async move {
            // Relative / absolute: rolldown resolves and bundles it (inside the pkg).
            if spec.starts_with('.') || spec.starts_with('/') {
                return Ok(None);
            }
            // Node builtins: an empty external stub the browser can import.
            if is_node_builtin(&spec) {
                return Ok(Some(external(format!("/@id/{}", hex_encode(&spec)))));
            }
            let dir = importer
                .as_deref()
                .map(Path::new)
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.clone());
            match resolver.resolve(&dir, &spec) {
                Ok(resolved) => {
                    let resolved = normalize(&resolved);
                    if inside_package(&pkg_root, &resolved) {
                        // Same package: let rolldown bundle it into this output.
                        Ok(None)
                    } else {
                        // Another package: import from its own served URL (its own
                        // bundle), preserving singletons across bundles.
                        Ok(Some(external(dep_serve_url(&resolved, &root))))
                    }
                }
                // `browser: false` stub, like Vite's empty module.
                Err(e) if e.ignored => Ok(Some(external("/@oj-empty".to_string()))),
                // Let rolldown surface a real resolution error.
                Err(_) => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_forced;
    use std::path::Path;

    #[test]
    fn builtin_hard_packages_are_forced() {
        // The curated known-broken package is forced without any env override or
        // config; a benign package is not (config include is unset in tests).
        assert!(is_forced(Path::new(
            "/x/node_modules/object-inspect/index.js"
        )));
        assert!(!is_forced(Path::new("/x/node_modules/lodash-es/index.js")));
    }
}
