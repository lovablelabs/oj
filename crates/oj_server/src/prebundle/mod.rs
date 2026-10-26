// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite 8's dependency optimizer on oj's own rolldown: the scan
//! (optimizer/scan.ts) crawls the app from its entries and records every bare
//! import that resolves into node_modules, and the pre-bundle
//! (prepareRolldownOptimizerRun) bundles those deps into one ESM entry each
//! plus shared chunks. [`crate::optimize`] owns when this runs and what the
//! dev server does with the result.

mod bundle;
mod options;
mod scan;
mod tsconfig;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use oj_compiler::glob::GlobCall;

use crate::optimize::OptimizeInput;

pub(crate) use options::DepOptions;
pub(crate) use scan::ScanPluginHooks;

/// What the scanner follows (Vite's JS_TYPES_RE).
fn is_js_type(file: &str) -> bool {
    let file = clean_url(file);
    [".js", ".jsx", ".ts", ".tsx", ".mjs"]
        .iter()
        .any(|ext| file.ends_with(ext))
}

/// What the optimizer takes as a bundle entry (Vite's OPTIMIZABLE_ENTRY_RE).
fn is_optimizable_entry(file: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts"]
        .iter()
        .any(|ext| file.ends_with(ext))
}

fn clean_url(id: &str) -> &str {
    id.split(['?', '#']).next().unwrap_or(id)
}

/// A bare specifier (Vite's bareImportRE).
fn is_bare(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '@')
        && chars.next().is_some_and(|c| c != ':')
}

fn in_node_modules(id: &str) -> bool {
    id.split(['/', '\\']).any(|part| part == "node_modules")
}

/// `id` is `m` or a subpath of it, for some `m` in `list`.
fn module_list_contains(list: &[String], id: &str) -> bool {
    list.iter().any(|m| {
        id == m
            || id
                .strip_prefix(m.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The package a bare specifier names: `@scope/pkg` or `pkg`.
pub(crate) fn npm_package_name(id: &str) -> Option<&str> {
    if id.starts_with('@') {
        let mut slashes = id.match_indices('/');
        slashes.next()?;
        return Some(slashes.next().map_or(id, |(i, _)| &id[..i]));
    }
    id.split('/').next().filter(|s| !s.is_empty())
}

/// The name a dep bundles under: Vite's flattenId (`@scope/pkg/sub` is
/// `scope_pkg_sub`), so the future file is known before any bundling.
pub(crate) fn entry_name(dep: &str) -> String {
    dep.trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Vite's isDynamicPattern for include and entry lists.
fn is_dynamic_pattern(id: &str) -> bool {
    id.contains(['*', '?', '[', ']', '{', '}'])
}

/// The React family, plus `resolve.dedupe`: a CommonJS one gets a
/// named-export facade over its bundle, so modules that link it as ESM (no
/// interop rewrite) still find its names.
const FACADE_DEPS: &[&str] = &[
    "react",
    "react-dom",
    "react-dom/client",
    "react/jsx-runtime",
    "react/jsx-dev-runtime",
];

/// One pre-bundled dep, as the dev server serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prebuilt {
    /// The file under the deps dir.
    pub file: String,
    pub needs_interop: bool,
    /// Vite's fileHash: a rerun whose served entries all hash the same
    /// commits without a reload. Empty for a planned, not yet bundled, dep.
    pub file_hash: String,
}

/// The scanned deps: each bare id with the file the bundle takes as its
/// entry, in discovery order.
pub(crate) type Scanned = Vec<(String, PathBuf)>;

/// Which scanned deps the pre-bundle takes: entry name to file, and each
/// dep's entry name.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub entries: BTreeMap<String, PathBuf>,
    pub names: BTreeMap<String, String>,
}

/// One optimizer run's settings, read once from the config.
pub(crate) struct Prebundle {
    root: PathBuf,
    /// `root` with symlinks resolved, for the URLs of files under it.
    real_root: PathBuf,
    out_dir: PathBuf,
    auto_discover: bool,
    /// The scan's entries: JS and html files, absolute.
    entries: Vec<PathBuf>,
    /// `optimizeDeps.include` (globs expanded), then oj's implicit ones.
    include: Vec<String>,
    /// The user's includes, which warn when they do not resolve.
    warn_includes: HashSet<String>,
    exclude: Vec<String>,
    /// `resolve.dedupe` package names: resolved from the root.
    dedupe: HashSet<String>,
    facade_deps: HashSet<String>,
    needs_interop: HashSet<String>,
    alias: tsconfig::Aliases,
    options: DepOptions,
    keep_existing: bool,
    /// Where the scan and the bundle run (see `crate::rolldown_runtime`).
    rolldown: tokio::runtime::Handle,
}

impl std::fmt::Debug for Prebundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prebundle")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Prebundle {
    /// `implicit`: deps oj adds itself (the injected JSX runtime, deps the
    /// dev server discovered), bundled when they resolve, never warned about.
    pub fn new(
        root: &Path,
        out_dir: &Path,
        input: &OptimizeInput,
        implicit: impl IntoIterator<Item = String>,
        keep_existing: bool,
        rolldown: &tokio::runtime::Handle,
    ) -> Prebundle {
        let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut include: Vec<String> = Vec::new();
        for inc in &input.include {
            let ids = if is_dynamic_pattern(inc) {
                expand_glob_ids(root, inc)
            } else {
                vec![inc.clone()]
            };
            for id in ids {
                if !include.contains(&id) {
                    include.push(id);
                }
            }
        }
        let warn_includes = include.iter().cloned().collect();
        for inc in implicit {
            if !include.contains(&inc) {
                include.push(inc);
            }
        }
        let entries = scan_entries(root, input);
        let dedupe = input
            .dedupe
            .iter()
            .filter_map(|d| npm_package_name(d).map(str::to_string))
            .collect();
        let facade_deps = FACADE_DEPS
            .iter()
            .map(|s| s.to_string())
            .chain(input.dedupe.iter().cloned())
            .collect();
        Prebundle {
            root: root.to_path_buf(),
            real_root,
            out_dir: out_dir.to_path_buf(),
            auto_discover: !input.no_discovery.unwrap_or(false),
            entries,
            include,
            warn_includes,
            exclude: input.exclude.clone(),
            dedupe,
            facade_deps,
            needs_interop: input.needs_interop.iter().cloned().collect(),
            alias: tsconfig::Aliases::load(root, &input.alias),
            options: DepOptions::new(input),
            keep_existing,
            rolldown: rolldown.clone(),
        }
    }

    /// The URL oj's dev server serves a file at (rewrite.rs `url_of`):
    /// root-relative inside the root, `/@fs` outside it. Bundles are served
    /// verbatim, so every import they keep must already be a URL the browser
    /// can fetch (Vite runs importAnalysis over them instead).
    fn url_of(&self, file: &Path) -> String {
        for base in [&self.root, &self.real_root] {
            if let Ok(rel) = file.strip_prefix(base) {
                if !rel.as_os_str().is_empty() {
                    return format!("/{}", slash(rel));
                }
            }
        }
        format!("/@fs{}", slash(file))
    }

    /// Which scanned deps the pre-bundle takes, under which entry names.
    /// `warn`: report the includes that did not resolve (off for the
    /// provisional plan, which the bundle repeats).
    pub fn plan(&self, scanned: &Scanned, warn: bool) -> Plan {
        let mut candidates: Vec<(String, Option<PathBuf>)> = scanned
            .iter()
            .map(|(dep, file)| (dep.clone(), Some(file.clone())))
            .collect();
        // "a > b" names a nested copy the scan cannot resolve (it resolves
        // below), and a plain include the scan could not resolve must reach
        // the loop too, so its warning fires instead of a silent drop.
        for inc in &self.include {
            if !candidates.iter().any(|(dep, _)| dep == inc) {
                candidates.push((inc.clone(), None));
            }
        }
        let unable = |dep: &str| {
            if warn && (self.warn_includes.contains(dep) || dep.contains('>')) {
                eprintln!(
                    "oj: failed to resolve dependency \"{dep}\", present in 'optimizeDeps.include'"
                );
            }
        };
        let mut plan = Plan::default();
        for (dep, entry) in candidates {
            if self.exclude.contains(&dep) {
                continue;
            }
            // Vite's nested syntax: "a > b" bundles the copy of `b` nested in
            // `a`, each segment resolved from the previous one's directory,
            // registered under the last segment (what the app imports).
            if dep.contains('>') {
                let parts: Vec<&str> = dep
                    .split('>')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                let resolver = oj_resolver::OjResolver::new(&self.root).require_variant();
                let mut dir = self.root.clone();
                let mut resolved = None;
                for part in &parts {
                    match resolver.resolve(&dir, part) {
                        Ok(file) => {
                            dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
                            resolved = Some(file);
                        }
                        Err(_) => {
                            resolved = None;
                            break;
                        }
                    }
                }
                let (Some(file), Some(last)) = (resolved, parts.last()) else {
                    unable(&dep);
                    continue;
                };
                let name = entry_name(last);
                plan.entries.insert(name.clone(), file);
                plan.names.insert(last.to_string(), name);
                continue;
            }
            // A package.json `#imports` subpath is the app's own source.
            if dep.starts_with('#') {
                continue;
            }
            let Some(entry) = entry else {
                unable(&dep);
                continue;
            };
            // Linked / workspace packages are crawled, not frozen into a
            // bundle (their edits must keep hot-updating), unless included.
            if !self.include.contains(&dep) {
                let real = std::fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
                if !in_node_modules(&real.to_string_lossy()) {
                    continue;
                }
            }
            // The lingui macro entrypoints are served by oj's runtime shim.
            if matches!(
                dep.as_str(),
                "@lingui/macro" | "@lingui/core/macro" | "@lingui/react/macro"
            ) {
                continue;
            }
            // Only JavaScript deps bundle; a CSS-only dep or an expanded
            // `pkg/*` include's package.json is served directly.
            if !is_optimizable_entry(&entry.to_string_lossy()) {
                continue;
            }
            let name = entry_name(&dep);
            plan.entries.insert(name.clone(), entry);
            plan.names.insert(dep, name);
        }
        plan
    }

    /// The metadata the bundle is going to commit, guessed before bundling
    /// (Vite's discovered-deps optimization): file names are known up front,
    /// interop is read off each entry's source. The dev server compiles
    /// against it while the bundle runs; a commit that disagrees reloads.
    pub fn provisional(&self, plan: &Plan) -> BTreeMap<String, Prebuilt> {
        let mut out = BTreeMap::new();
        for (dep, name) in &plan.names {
            let cjs = plan
                .entries
                .get(name)
                .is_some_and(|f| bundle::is_common_js(f));
            let forced = self.needs_interop.contains(dep);
            let meta = if cjs && self.facade_deps.contains(dep) {
                Prebuilt {
                    file: format!("{name}__oj_named.mjs"),
                    needs_interop: forced,
                    file_hash: String::new(),
                }
            } else {
                Prebuilt {
                    file: format!("{name}.mjs"),
                    needs_interop: cjs || forced,
                    file_hash: String::new(),
                }
            };
            out.insert(dep.clone(), meta);
        }
        out
    }
}

/// The scan's entries, Vite's computeEntries: `optimizeDeps.entries` (globs
/// from the root, a literal path as given), else the build inputs, else every
/// html file under the root. Only existing JS and html files feed the scan.
fn scan_entries(root: &Path, input: &OptimizeInput) -> Vec<PathBuf> {
    let absolute = |e: &str| {
        let p = Path::new(e);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(e)
        }
    };
    let listed = !input.entries.is_empty();
    let found: Vec<PathBuf> = if listed {
        input
            .entries
            .iter()
            .flat_map(|e| {
                if is_dynamic_pattern(e) {
                    glob_entries(root, &[e.as_str()], &input.build_out_dir, true)
                } else {
                    vec![absolute(e)]
                }
            })
            .collect()
    } else if !input.build_inputs.is_empty() {
        input.build_inputs.iter().map(|e| absolute(e)).collect()
    } else {
        glob_entries(root, &["**/*.html"], &input.build_out_dir, false)
    };
    found
        .into_iter()
        .filter(|f| {
            let s = f.to_string_lossy();
            (is_js_type(&s) || s.ends_with(".html")) && f.is_file()
        })
        .collect()
}

/// Vite's globEntries: the patterns from the root, absolute, ignoring the
/// build outDir (and tests and coverage unless the entries were listed), and
/// `node_modules` for any pattern that does not name it.
fn glob_entries(root: &Path, patterns: &[&str], out_dir: &str, listed: bool) -> Vec<PathBuf> {
    let base = oj_compiler::glob::escape_glob_path(root);
    let out_dir = if out_dir.is_empty() { "dist" } else { out_dir };
    let mut ignore = vec![format!("!{base}/**/{out_dir}/**")];
    if !listed {
        ignore.push(format!("!{base}/**/__tests__/**"));
        ignore.push(format!("!{base}/**/coverage/**"));
    }
    let absolute = |p: &str| match p.strip_prefix('!') {
        Some(neg) => format!("!{base}/{}", neg.trim_start_matches("./")),
        None => format!("{base}/{}", p.trim_start_matches("./")),
    };
    let (in_modules, regular): (Vec<&str>, Vec<&str>) =
        patterns.iter().partition(|p| p.contains("node_modules"));
    let mut files = Vec::new();
    for (group, exhaustive) in [(in_modules, true), (regular, false)] {
        if group.is_empty() {
            continue;
        }
        let call = GlobCall {
            patterns: group
                .iter()
                .map(|p| absolute(p))
                .chain(ignore.iter().cloned())
                .collect(),
            exhaustive,
            case_sensitive: true,
        };
        files.extend(call.files());
    }
    files
}

/// Vite's expandGlobIds (optimizer/resolve.ts): an include with a glob in its
/// subpath (`pkg/*`, `@scope/pkg/dist/**/*.js`) is the package plus every
/// subpath the glob matches: the `exports` keys when the package has an
/// exports map (subpath patterns listed through their first target), else
/// the files under the package directory.
fn expand_glob_ids(root: &Path, id: &str) -> Vec<String> {
    let Some(pkg_name) = npm_package_name(id) else {
        return Vec::new();
    };
    let Some(pkg_dir) = find_package_dir(root, pkg_name) else {
        return Vec::new();
    };
    let Some(pkg_json) = std::fs::read_to_string(pkg_dir.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return Vec::new();
    };
    let pattern = id[pkg_name.len()..].trim_start_matches('/');
    let dir = slash(&pkg_dir);
    let dir_glob = oj_compiler::glob::escape_glob_path(&pkg_dir);
    let files_under = |pattern: &str| -> Vec<String> {
        GlobCall {
            patterns: vec![format!("{dir_glob}/{pattern}")],
            exhaustive: false,
            case_sensitive: true,
        }
        .files()
        .iter()
        .filter_map(|f| f.strip_prefix(&pkg_dir).ok().map(slash))
        .collect()
    };
    let join = |sub: &str| format!("{pkg_name}/{sub}");
    let mut out = vec![pkg_name.to_string()];
    match pkg_json.get("exports") {
        Some(serde_json::Value::String(_) | serde_json::Value::Array(_)) => {}
        Some(serde_json::Value::Object(exports)) => {
            let mut possible = Vec::new();
            for (key, value) in exports {
                let Some(key) = key.strip_prefix("./") else {
                    continue;
                };
                if !key.contains('*') {
                    if !value.is_null() {
                        possible.push(key.to_string());
                    }
                    continue;
                }
                let Some(target) = first_export_string(value) else {
                    continue;
                };
                let target = target.trim_start_matches("./");
                let Ok(target_re) = regex::Regex::new(&format!(
                    "^{}$",
                    target
                        .split('*')
                        .map(regex::escape)
                        .collect::<Vec<_>>()
                        .join("(.*)")
                )) else {
                    continue;
                };
                for file in files_under(&target.replace('*', "**/*")) {
                    let Some(caps) = target_re.captures(&file) else {
                        continue;
                    };
                    // Every `*` must stand for the same text.
                    let parts: Vec<&str> =
                        caps.iter().skip(1).flatten().map(|m| m.as_str()).collect();
                    if parts.windows(2).all(|w| w[0] == w[1]) {
                        if let Some(first) = parts.first() {
                            possible.push(key.replacen('*', first, 1));
                        }
                    }
                }
            }
            let matcher = GlobCall {
                patterns: vec![format!("{dir_glob}/{pattern}")],
                exhaustive: false,
                case_sensitive: true,
            }
            .matcher();
            out.extend(
                possible
                    .iter()
                    .filter(|p| matcher.is_match(&format!("{dir}/{p}")))
                    .map(|p| join(p)),
            );
        }
        _ => out.extend(files_under(pattern).iter().map(|f| join(f))),
    }
    out
}

fn first_export_string(v: &serde_json::Value) -> Option<&str> {
    match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Array(a) => a.first().and_then(first_export_string),
        serde_json::Value::Object(o) => o.values().next().and_then(first_export_string),
        _ => None,
    }
}

/// The installed copy of `pkg_name`, walking up from the root.
fn find_package_dir(root: &Path, pkg_name: &str) -> Option<PathBuf> {
    root.ancestors()
        .map(|dir| dir.join("node_modules").join(pkg_name))
        .find(|dir| dir.join("package.json").is_file())
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `.` and `..` folded away, as Node's path.resolve does.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
