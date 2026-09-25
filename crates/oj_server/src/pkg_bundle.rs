// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Server side of oj-native partial bundling: discover one node_modules package's
// internal CommonJS file graph, compile each file to a factory body, and hand it
// to `oj_compiler::pkgbundle::emit_package_bundle` for a single served ESM file.
// Cross-package `require`s become native ESM imports to *their* `/@oj-pkg/...`
// bundles (or a node-builtin stub), so each package collapses to one request
// while keeping oj's per-module CommonJS runtime (robust UMD/dynamic interop).
//
// v1 is deliberately conservative: a package whose graph contains an ES module,
// an unsupported file type, or an escaping relative import bails to `Fallback`,
// and the caller serves that package the normal (per-file) way.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use oj_compiler::pkgbundle::{emit_package_bundle, DepTarget, ModuleKind, PkgModule};
use oj_resolver::OjResolver;

use crate::{hex_encode, is_node_builtin, normalize, package_root, url_of};

/// Where oj serves package bundles. `<hex>` encodes the entry file's abs path.
pub const PKG_PREFIX: &str = "/@oj-pkg/";

pub enum BundleOutcome {
    /// The emitted self-contained ESM bundle source.
    Bundle(String),
    /// This package can't be safely bundled in v1; serve it the normal way.
    Fallback,
}

const INTERNAL_EXTS: &[&str] = &["js", "cjs", "mjs", "jsx", "json"];

pub fn bundle_url_for(entry_abs: &Path) -> String {
    let url = format!("{PKG_PREFIX}{}", hex_encode(&entry_abs.to_string_lossy()));
    // Same `?v=<prebundle hash>` stamp as /@oj-deps (Vite's ensureVersionQuery)
    // so the bundle is served immutable and every importer, app source or a
    // sibling bundle, names the one URL (one module instance).
    match version_cell().get().filter(|v| !v.is_empty()) {
        Some(v) => format!("{url}?v={v}"),
        None => url,
    }
}

fn version_cell() -> &'static std::sync::OnceLock<String> {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    &V
}

/// Install the dep version stamped onto bundle URLs. Set once at startup (the
/// optimizer's short hash); later calls are ignored like `configure`.
pub fn set_version(version: &str) {
    let _ = version_cell().set(version.to_string());
}

/// Decode a `/@oj-pkg/<hex>` request path back to the entry file.
pub fn entry_from_url(path: &str) -> Option<PathBuf> {
    let hex = path.strip_prefix(PKG_PREFIX)?;
    crate::hex_decode(hex).map(PathBuf::from)
}

// In-memory cache of built bundles, keyed by the `/@oj-pkg/<hex>` URL. A
// package's files don't change during a dev session, so no invalidation.
fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<String>>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<String>>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
pub fn cached(url: &str) -> Option<std::sync::Arc<String>> {
    cache().lock().unwrap().get(url).cloned()
}
pub fn store(url: &str, code: std::sync::Arc<String>) {
    cache().lock().unwrap().insert(url.to_string(), code);
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn pb_debug() -> bool {
    std::env::var("OJ_PB_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Runtime view of the user's `optimizeDeps` config, set once at server startup.
#[derive(Default)]
pub struct OptimizeConfig {
    /// `optimizeDeps.include`: force these packages to be pre-bundled (rolldown).
    pub include: std::collections::HashSet<String>,
    /// `optimizeDeps.exclude`: never partial-bundle these; serve per-file.
    pub exclude: std::collections::HashSet<String>,
    /// `optimizeDeps.needsInterop`: force CJS->ESM interop for these.
    pub needs_interop: std::collections::HashSet<String>,
}

fn opt_config() -> &'static std::sync::OnceLock<OptimizeConfig> {
    static C: std::sync::OnceLock<OptimizeConfig> = std::sync::OnceLock::new();
    &C
}

/// Install the resolved `optimizeDeps` config. Called once at startup; later
/// calls are ignored (the first wins), matching the OnceLock semantics.
pub fn configure(include: Vec<String>, exclude: Vec<String>, needs_interop: Vec<String>) {
    let cfg = OptimizeConfig {
        include: include.into_iter().collect(),
        exclude: exclude.into_iter().collect(),
        needs_interop: needs_interop.into_iter().collect(),
    };
    let _ = opt_config().set(cfg);
}

fn config() -> Option<&'static OptimizeConfig> {
    opt_config().get()
}

/// The npm package name a node_modules path belongs to (`@scope/name` or `name`).
pub fn package_name(entry: &Path) -> Option<String> {
    let comps: Vec<&std::ffi::OsStr> = entry.components().map(|c| c.as_os_str()).collect();
    let idx = comps.iter().rposition(|c| *c == "node_modules")?;
    let first = comps.get(idx + 1)?.to_str()?;
    if first.starts_with('@') {
        Some(format!("{first}/{}", comps.get(idx + 2)?.to_str()?))
    } else {
        Some(first.to_string())
    }
}

fn pkg_in(path: &Path, set: impl Fn(&OptimizeConfig) -> &std::collections::HashSet<String>) -> bool {
    match (config(), package_name(path)) {
        (Some(c), Some(name)) => set(c).contains(&name),
        _ => false,
    }
}

/// `optimizeDeps.include` names this package (force it through pre-bundling).
pub fn is_include_forced(path: &Path) -> bool {
    pkg_in(path, |c| &c.include)
}

/// `optimizeDeps.exclude` names this package (skip partial bundling, serve per-file).
pub fn is_excluded(path: &Path) -> bool {
    pkg_in(path, |c| &c.exclude)
}

/// `optimizeDeps.needsInterop` names this package (force CJS->ESM interop).
pub fn needs_forced_interop(path: &Path) -> bool {
    pkg_in(path, |c| &c.needs_interop)
}

/// Return `Fallback`, logging why (and for which file) when `OJ_PB_DEBUG` is set.
fn bail(reason: &str, file: &Path) -> BundleOutcome {
    if pb_debug() {
        eprintln!("oj[pb] fallback: {reason} @ {}", file.display());
    }
    BundleOutcome::Fallback
}

fn is_esm(path: &Path, src: &str) -> bool {
    oj_compiler::cjs::has_module_syntax_pub(path, src)
}

/// Resolve a relative specifier to an actual file, with extension / index probing.
fn resolve_relative(from_dir: &Path, spec: &str) -> Option<PathBuf> {
    let base = normalize(&from_dir.join(spec));
    if base.is_file() {
        return Some(base);
    }
    for ext in INTERNAL_EXTS {
        let cand = PathBuf::from(format!("{}.{ext}", base.display()));
        if cand.is_file() {
            return Some(cand);
        }
    }
    if base.is_dir() {
        for ext in INTERNAL_EXTS {
            let cand = base.join(format!("index.{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// A bare specifier (e.g. `jotai/react`) that resolves to a file *inside the
/// same package* is internal, not a cross-package edge: bundling it keeps its
/// exports concrete (so `export * from 'jotai/react'` becomes real static
/// re-exports, not runtime-only properties an `import { x }` can't see).
fn resolve_same_package(
    pkg_root: &Path,
    from_dir: &Path,
    spec: &str,
    resolver: &OjResolver,
) -> Option<PathBuf> {
    let resolved = normalize(&resolver.resolve(from_dir, spec).ok()?);
    // Compare via canonical paths so a symlinked layout (pnpm, or macOS
    // /var -> /private/var) doesn't hide that the file is inside this package.
    let root_c = std::fs::canonicalize(pkg_root).ok()?;
    let resolved_c = std::fs::canonicalize(&resolved).ok()?;
    let rel = resolved_c.strip_prefix(&root_c).ok()?;
    // Inside this package, and not down a nested node_modules of it.
    if rel.components().any(|c| c.as_os_str() == "node_modules") {
        return None;
    }
    let ext = rel.extension().and_then(|e| e.to_str()).unwrap_or("");
    // Return the target under the *original* pkg_root so `id_of` strips cleanly.
    INTERNAL_EXTS.contains(&ext).then(|| pkg_root.join(rel))
}

/// Build the bundle for a package `entry`. Returns `Fallback` if the
/// package isn't safely bundleable in v1.
pub fn build(entry: &Path, resolver: &OjResolver, root: &Path) -> BundleOutcome {
    if read(entry).is_none() {
        return BundleOutcome::Fallback;
    }
    let pkg_root = package_root(entry);

    let mut modules: Vec<PkgModule> = Vec::new();
    let mut externals: Vec<String> = Vec::new();
    let mut ext_seen: HashSet<String> = HashSet::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(entry.to_path_buf());

    let entry_id = match id_of(&pkg_root, entry) {
        Some(id) => id,
        None => return BundleOutcome::Fallback,
    };
    // id -> (direct named exports, internal re-export target ids), so the entry's
    // full ESM export set can follow `__exportStar` barrels (e.g. @sniptt/guards,
    // whose entry re-exports its submodules and has no direct names of its own).
    let mut export_info: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();

    while let Some(file) = queue.pop_front() {
        if !visited.insert(file.clone()) {
            continue;
        }
        let Some(id) = id_of(&pkg_root, &file) else {
            return bail("path escaped the package", &file);
        };
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(src) = read(&file) else {
            return bail("could not read file", &file);
        };

        // JSON internal module: expose the parsed value as module.exports.
        if ext == "json" {
            export_info.insert(id.clone(), (Vec::new(), Vec::new()));
            modules.push(PkgModule {
                id,
                kind: ModuleKind::Cjs,
                body: format!("module.exports = {};", src.trim()),
                deps: Vec::new(),
            });
            continue;
        }

        // ES module inside the package: compile it to an ESM factory and
        // register it alongside the CJS ones. The
        // resolve callback rewrites each import to a bundle-internal ("#id") or
        // cross-package ("@url") target that the runtime interprets.
        if ext == "mjs" || is_esm(&file, &src) {
            // import.meta.url / .resolve can't be honored inside a factory function.
            if src.contains("import.meta.url") || src.contains("import.meta.resolve") {
                return bail("uses import.meta.url/resolve", &file);
            }
            let file_url = url_of(root, &file);
            let dir = file.parent().unwrap_or(&pkg_root).to_path_buf();
            let mut bail_spec: Option<String> = None;
            let mut discovered: Vec<PathBuf> = Vec::new();
            let factory = {
                let mut resolve = |spec: &str| -> Option<String> {
                    if spec.starts_with('.') {
                        let Some(target) = resolve_relative(&dir, spec) else {
                            bail_spec = Some(format!("unresolved import {spec:?}"));
                            return None;
                        };
                        let Some(tid) = id_of(&pkg_root, &target) else {
                            bail_spec = Some(format!("import {spec:?} escapes package"));
                            return None;
                        };
                        let t_ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
                        if !INTERNAL_EXTS.contains(&t_ext) {
                            bail_spec = Some(format!("import {spec:?} -> unsupported .{t_ext}"));
                            return None;
                        }
                        discovered.push(target);
                        Some(format!("#{tid}"))
                    } else if let Some(target) = resolve_same_package(&pkg_root, &dir, spec, resolver)
                    {
                        // Same-package subpath (`jotai/react`): treat as internal.
                        match id_of(&pkg_root, &target) {
                            Some(tid) => {
                                discovered.push(target);
                                Some(format!("#{tid}"))
                            }
                            None => {
                                bail_spec = Some(format!("same-package import {spec:?} has no id"));
                                None
                            }
                        }
                    } else {
                        match external_url(spec, &dir, resolver, root) {
                            Some(u) => {
                                if ext_seen.insert(u.clone()) {
                                    externals.push(u.clone());
                                }
                                Some(format!("@{u}"))
                            }
                            None => {
                                bail_spec = Some(format!("unresolvable import {spec:?}"));
                                None
                            }
                        }
                    }
                };
                oj_compiler::bundle::compile_factory(&file, &file_url, &src, &mut resolve)
            };
            if let Some(reason) = bail_spec {
                return bail(&reason, &file);
            }
            let factory = match factory {
                Ok(f) => f,
                Err(e) => {
                    if pb_debug() {
                        eprintln!("oj[pb] fallback: esm compile error ({e}) @ {}", file.display());
                    }
                    return BundleOutcome::Fallback;
                }
            };
            // Dynamic imports are fine: compile_esm_factory lowered them to
            // `__oj_import_lazy("#id"|"@url")`, which the emitted bundle runtime
            // resolves (internal -> resolved namespace, external -> native import).
            if factory.kind != oj_compiler::bundle::FactoryKind::Esm {
                return bail("compiled as CJS unexpectedly", &file);
            }
            for target in discovered {
                queue.push_back(target);
            }
            let reexport_ids: Vec<String> = factory
                .esm_star_targets
                .iter()
                .filter_map(|t| t.strip_prefix('#').map(|s| s.to_string()))
                .collect();
            export_info.insert(id.clone(), (factory.esm_named.clone(), reexport_ids));
            modules.push(PkgModule {
                id,
                kind: ModuleKind::Esm,
                body: factory.code,
                deps: Vec::new(),
            });
            continue;
        }

        let analysis = match oj_compiler::cjs::analyze_for_factory(&file, &src) {
            Ok(a) => a,
            Err(e) => {
                if pb_debug() {
                    eprintln!("oj[pb] fallback: cjs analyze error ({e}) @ {}", file.display());
                }
                return BundleOutcome::Fallback;
            }
        };

        let dir = file.parent().unwrap_or(&pkg_root).to_path_buf();
        let mut deps: Vec<(String, DepTarget)> = Vec::new();
        for spec in &analysis.requires {
            if spec.starts_with('.') {
                // Relative: stay inside the package or bail.
                let Some(target) = resolve_relative(&dir, spec) else {
                    return bail(&format!("unresolved relative require({spec:?})"), &file);
                };
                let Some(tid) = id_of(&pkg_root, &target) else {
                    return bail(&format!("relative require({spec:?}) escapes package"), &file);
                };
                let t_ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !INTERNAL_EXTS.contains(&t_ext) {
                    return bail(&format!("relative require({spec:?}) -> unsupported .{t_ext}"), &file);
                }
                deps.push((spec.clone(), DepTarget::Internal(tid)));
                queue.push_back(target);
            } else if let Some(target) = resolve_same_package(&pkg_root, &dir, spec, resolver) {
                // Same-package subpath require: internal.
                let Some(tid) = id_of(&pkg_root, &target) else {
                    return bail(&format!("same-package require({spec:?}) has no id"), &file);
                };
                deps.push((spec.clone(), DepTarget::Internal(tid)));
                queue.push_back(target);
            } else {
                // Bare: another package (its own bundle) or a node builtin (stub).
                let url = external_url(spec, &dir, resolver, root);
                let Some(url) = url else {
                    return bail(&format!("unresolvable bare require({spec:?})"), &file);
                };
                if ext_seen.insert(url.clone()) {
                    externals.push(url.clone());
                }
                deps.push((spec.clone(), DepTarget::External(url)));
            }
        }

        // Internal re-export targets (for the transitive export-name walk).
        let reexport_ids: Vec<String> = analysis
            .reexport_requires
            .iter()
            .filter_map(|spec| {
                deps.iter().find(|(s, _)| s == spec).and_then(|(_, t)| match t {
                    DepTarget::Internal(id) => Some(id.clone()),
                    DepTarget::External(_) => None,
                })
            })
            .collect();
        export_info.insert(id.clone(), (analysis.named_exports.clone(), reexport_ids));
        modules.push(PkgModule { id, kind: ModuleKind::Cjs, body: analysis.body, deps });
    }

    let entry_named = collect_exports(&entry_id, &export_info);
    externals.sort();
    BundleOutcome::Bundle(emit_package_bundle(&modules, &entry_id, &externals, &entry_named))
}

/// The entry's full ESM export names: its own plus everything re-exported
/// (transitively) from internal modules via `__exportStar` / `module.exports =
/// require("./x")`.
fn collect_exports(entry: &str, info: &HashMap<String, (Vec<String>, Vec<String>)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack = vec![entry.to_string()];
    while let Some(id) = stack.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some((named, reexports)) = info.get(&id) {
            for n in named {
                if seen_names.insert(n.clone()) {
                    out.push(n.clone());
                }
            }
            for r in reexports {
                stack.push(r.clone());
            }
        }
    }
    out
}

fn id_of(pkg_root: &Path, file: &Path) -> Option<String> {
    file.strip_prefix(pkg_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// Resolve a bare specifier a package `require`s to the URL its bundle imports.
fn external_url(spec: &str, from_dir: &Path, resolver: &OjResolver, root: &Path) -> Option<String> {
    match resolver.resolve(from_dir, spec) {
        Ok(resolved) => {
            // Another node_modules package -> its own /@oj-pkg bundle (singletons
            // preserved: the app's top-level import resolves to the same URL).
            if resolved
                .components()
                .any(|c| c.as_os_str() == "node_modules")
            {
                Some(bundle_url_for(&resolved))
            } else {
                // Workspace/source dep: serve normally.
                Some(url_of(root, &resolved))
            }
        }
        Err(e) if e.ignored => Some("/@oj-empty".to_string()),
        Err(_) if is_node_builtin(spec) => Some(format!("/@id/{}", hex_encode(spec))),
        // A missing optional peer of this package: Vite's lazy-error stub.
        Err(_) => crate::optional_peer_dep_url(root, from_dir, spec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_from_node_modules_path() {
        assert_eq!(
            package_name(Path::new("/a/node_modules/object-inspect/index.js")).as_deref(),
            Some("object-inspect")
        );
        assert_eq!(
            package_name(Path::new("/a/node_modules/@apollo/client/core/index.js")).as_deref(),
            Some("@apollo/client")
        );
        // Nested dependency: the deepest node_modules wins.
        assert_eq!(
            package_name(Path::new("/a/node_modules/x/node_modules/y/i.js")).as_deref(),
            Some("y")
        );
        assert_eq!(package_name(Path::new("/a/src/main.tsx")), None);
    }

    #[test]
    fn optimize_config_drives_include_exclude_interop() {
        // OnceLock is process-global and set-once; this is the only test that
        // configures it, so first-wins is deterministic here.
        configure(
            vec!["object-inspect".into()],
            vec!["big-esm".into()],
            vec!["@scope/legacy".into()],
        );
        assert!(is_include_forced(Path::new("/a/node_modules/object-inspect/index.js")));
        assert!(!is_include_forced(Path::new("/a/node_modules/react/index.js")));
        assert!(is_excluded(Path::new("/a/node_modules/big-esm/dist/x.js")));
        assert!(!is_excluded(Path::new("/a/node_modules/object-inspect/index.js")));
        assert!(needs_forced_interop(Path::new("/a/node_modules/@scope/legacy/index.js")));
        assert!(!needs_forced_interop(Path::new("/a/node_modules/big-esm/x.js")));
    }

    // Build a synthetic package on disk, bundle it, node-eval the result.
    fn eval_bundle(src: &str) -> serde_json::Value {
        let dir = std::env::temp_dir().join(format!("oj-pkgbuild-{}", fnv(src)));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bundle.mjs");
        std::fs::write(&f, src).unwrap();
        let probe = dir.join("probe.mjs");
        std::fs::write(
            &probe,
            format!(
                "import def, * as ns from {:?};\nprocess.stdout.write(JSON.stringify({{ def, keys: Object.keys(ns) }}));\n",
                f.to_string_lossy()
            ),
        )
        .unwrap();
        let out = std::process::Command::new("node").arg(&probe).output().unwrap();
        assert!(out.status.success(), "node: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn fnv(s: &str) -> u64 {
        let mut h: u64 = 1469598103934665603;
        for b in s.bytes() { h ^= b as u64; h = h.wrapping_mul(1099511628211); }
        h
    }

    fn mkpkg() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("oj-pkg-{}-{}", std::process::id(), fnv("x")));
        let nm = root.join("node_modules").join("acme");
        std::fs::create_dir_all(nm.join("lib")).unwrap();
        std::fs::write(nm.join("package.json"), r#"{"name":"acme","version":"1.0.0","main":"index.js"}"#).unwrap();
        // entry -> internal ./lib/impl + ./data.json; re-exports a name.
        std::fs::write(nm.join("index.js"), "const impl = require('./lib/impl');\nconst data = require('./data.json');\nmodule.exports.greet = (n) => impl.hi + n + data.mark;\nmodule.exports.value = 42;\n").unwrap();
        std::fs::write(nm.join("lib").join("impl.js"), "module.exports.hi = 'hi ';\n").unwrap();
        std::fs::write(nm.join("data.json"), r#"{"mark":"!"}"#).unwrap();
        (root, nm.join("index.js"))
    }

    #[test]
    fn bundles_multifile_cjs_package_with_json_and_subdir() {
        let (root, entry) = mkpkg();
        let resolver = OjResolver::new(&root);
        let outcome = build(&entry, &resolver, &root);
        let src = match outcome {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("expected a bundle, got fallback"),
        };
        // one bundle, no externals, internal graph + json included.
        assert!(src.contains(r#""index.js""#), "entry registered");
        assert!(src.contains(r#""lib/impl.js""#), "subdir file registered: {src}");
        assert!(src.contains(r#""data.json""#) && src.contains("module.exports = {"), "json inlined");
        assert!(!src.contains("import * as __oj_extns"), "no externals expected");

        // Runtime: named exports resolve across files + json.
        let probe = format!(
            "import {{ greet, value }} from {:?};\nprocess.stdout.write(JSON.stringify({{ g: greet('x'), v: value }}));\n",
            {
                let d = std::env::temp_dir().join(format!("oj-pkgrun-{}", fnv(&src)));
                std::fs::create_dir_all(&d).unwrap();
                let f = d.join("b.mjs");
                std::fs::write(&f, &src).unwrap();
                f.to_string_lossy().into_owned()
            }
        );
        let pf = std::env::temp_dir().join(format!("oj-pkgrun-probe-{}.mjs", fnv(&src)));
        std::fs::write(&pf, probe).unwrap();
        let out = std::process::Command::new("node").arg(&pf).output().unwrap();
        assert!(out.status.success(), "node: {}", String::from_utf8_lossy(&out.stderr));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["g"], serde_json::json!("hi x!"), "cross-file + json require works: {v}");
        assert_eq!(v["v"], serde_json::json!(42));
        std::fs::remove_dir_all(&root).ok();
    }

    // Node-eval a built bundle: write it to disk, import it, print a JSON probe
    // of the given expression list.
    fn eval_named(root: &Path, src: &str, probe: &str) -> serde_json::Value {
        let f = root.join(format!("bundle-{}.mjs", fnv(src)));
        std::fs::write(&f, src).unwrap();
        let pf = root.join(format!("probe-{}.mjs", fnv(&(src.to_string() + probe))));
        std::fs::write(&pf, probe.replace("BUNDLE", &format!("{:?}", f.to_string_lossy()))).unwrap();
        let out = std::process::Command::new("node").arg(&pf).output().unwrap();
        assert!(out.status.success(), "node: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    #[test]
    fn bundles_esm_package_with_internal_graph() {
        // Pure-ESM package: entry re-exports a submodule (star barrel) and has a
        // direct named + default export. All of it must survive one bundled file.
        let root = std::env::temp_dir().join(format!("oj-pkg-esm-{}", std::process::id()));
        let nm = root.join("node_modules").join("esmpkg");
        std::fs::create_dir_all(nm.join("lib")).unwrap();
        std::fs::write(nm.join("package.json"), r#"{"name":"esmpkg","type":"module","main":"index.js"}"#).unwrap();
        std::fs::write(
            nm.join("index.js"),
            "export * from './lib/util.js';\nexport const version = '1.0';\nexport default 7;\n",
        )
        .unwrap();
        std::fs::write(nm.join("lib").join("util.js"), "export const add = (a, b) => a + b;\n").unwrap();
        let resolver = OjResolver::new(&root);
        let src = match build(&nm.join("index.js"), &resolver, &root) {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("expected esm bundle, got fallback"),
        };
        let v = eval_named(
            &root,
            &src,
            "import def, { version, add } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ def, version, sum: add(2,3) }));\n",
        );
        assert_eq!(v["def"], serde_json::json!(7), "esm default: {v}");
        assert_eq!(v["version"], serde_json::json!("1.0"), "esm direct named: {v}");
        assert_eq!(v["sum"], serde_json::json!(5), "star-barrel re-export callable: {v}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn same_package_subpath_star_export_is_internal_and_named() {
        // jotai shape: the entry `export * from 'pkg/react'` (a bare same-package
        // subpath). It must bundle react.mjs internally and expose its names as
        // real static ESM exports, not runtime-only properties.
        let root = std::env::temp_dir().join(format!("oj-pkg-subpath-{}", std::process::id()));
        let nm = root.join("node_modules").join("jotaiish");
        std::fs::create_dir_all(nm.join("esm")).unwrap();
        std::fs::write(
            nm.join("package.json"),
            r#"{"name":"jotaiish","type":"module","exports":{".":"./esm/index.mjs","./react":"./esm/react.mjs"}}"#,
        )
        .unwrap();
        std::fs::write(nm.join("esm").join("index.mjs"), "export * from 'jotaiish/react';\n").unwrap();
        std::fs::write(
            nm.join("esm").join("react.mjs"),
            "function useAtomValue(a) { return a; }\nexport { useAtomValue };\n",
        )
        .unwrap();
        let resolver = OjResolver::new(&root);
        let src = match build(&nm.join("esm").join("index.mjs"), &resolver, &root) {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("expected bundle, got fallback"),
        };
        // react.mjs bundled internally (no external import for the subpath).
        assert!(!src.contains("import * as __oj_extns"), "subpath must be internal: {src}");
        let v = eval_named(
            &root,
            &src,
            "import { useAtomValue } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ ok: typeof useAtomValue }));\n",
        );
        assert_eq!(v["ok"], serde_json::json!("function"), "subpath name statically re-exported: {v}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bundles_mixed_esm_entry_over_cjs_internal() {
        // ESM entry that imports an internal CJS helper (the common real-world
        // shape: an ESM index over a CJS impl). Interop must resolve.
        let root = std::env::temp_dir().join(format!("oj-pkg-mixed-{}", std::process::id()));
        let nm = root.join("node_modules").join("mixed");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("package.json"), r#"{"name":"mixed","main":"index.js"}"#).unwrap();
        std::fs::write(
            nm.join("index.js"),
            "import impl from './impl.cjs';\nexport const shout = (s) => impl(s).toUpperCase();\n",
        )
        .unwrap();
        std::fs::write(nm.join("impl.cjs"), "module.exports = (s) => 'hi ' + s;\n").unwrap();
        let resolver = OjResolver::new(&root);
        let src = match build(&nm.join("index.js"), &resolver, &root) {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("expected mixed bundle, got fallback"),
        };
        let v = eval_named(
            &root,
            &src,
            "import { shout } from BUNDLE;\nprocess.stdout.write(JSON.stringify({ out: shout('ada') }));\n",
        );
        assert_eq!(v["out"], serde_json::json!("HI ADA"), "esm-over-cjs interop: {v}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn barrel_reexports_become_static_named_exports() {
        // A @sniptt/guards-style barrel: the entry has no direct names, it
        // __exportStar's a submodule. The bundle must still expose those names as
        // ESM exports (ESM-dep importers can't be interop-rewritten).
        let root = std::env::temp_dir().join(format!("oj-pkg-barrel-{}", std::process::id()));
        let nm = root.join("node_modules").join("guards");
        std::fs::create_dir_all(nm.join("g")).unwrap();
        std::fs::write(nm.join("package.json"), r#"{"name":"guards","main":"index.js"}"#).unwrap();
        std::fs::write(
            nm.join("index.js"),
            "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nvar tslib = require(\"tslib\");\n__exportStar(require(\"./g/prims\"), exports);\n".to_string(),
        )
        .unwrap();
        std::fs::write(nm.join("g").join("prims.js"), "exports.isNonEmptyString = (x) => typeof x === 'string' && x.length > 0;\nexports.isUndefined = (x) => x === undefined;\n").unwrap();
        // tslib external (provides __exportStar at runtime; here it just needs to resolve)
        let tslib = root.join("node_modules").join("tslib");
        std::fs::create_dir_all(&tslib).unwrap();
        std::fs::write(tslib.join("package.json"), r#"{"name":"tslib","main":"index.js"}"#).unwrap();
        std::fs::write(tslib.join("index.js"), "exports.__exportStar = function(m, e){ for (var k in m) if (k!=='default') e[k]=m[k]; };\n").unwrap();

        let resolver = OjResolver::new(&root);
        let src = match build(&nm.join("index.js"), &resolver, &root) {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("fallback"),
        };
        assert!(src.contains("as isNonEmptyString"), "barrel re-export name missing: {src}");
        assert!(src.contains("as isUndefined"), "barrel re-export name missing");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn external_require_becomes_pkg_import() {
        let root = std::env::temp_dir().join(format!("oj-pkg-ext-{}", std::process::id()));
        let a = root.join("node_modules").join("a");
        let b = root.join("node_modules").join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("package.json"), r#"{"name":"a","main":"index.js"}"#).unwrap();
        std::fs::write(a.join("index.js"), "const b = require('b');\nmodule.exports = b;\n").unwrap();
        std::fs::write(b.join("package.json"), r#"{"name":"b","main":"index.js"}"#).unwrap();
        std::fs::write(b.join("index.js"), "module.exports = 5;\n").unwrap();
        let resolver = OjResolver::new(&root);
        let src = match build(&a.join("index.js"), &resolver, &root) {
            BundleOutcome::Bundle(s) => s,
            BundleOutcome::Fallback => panic!("fallback"),
        };
        assert!(src.contains(PKG_PREFIX), "external routed to a /@oj-pkg bundle: {src}");
        assert!(src.contains("import * as __oj_extns0"), "external ESM import emitted");
        std::fs::remove_dir_all(&root).ok();
    }
}
