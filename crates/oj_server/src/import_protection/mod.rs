// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's import protection for the dev server, in Rust.
//!
//! Start's Vite plugin (`tanstack-start-core:import-protection`) keeps every
//! module's transformed code and Babel AST, per environment, for the
//! diagnostics of a violation that rarely comes. This keeps the import graph
//! and the marker each file set, and reads the offending file again when a
//! violation is reported.
//!
//! It stands in for the plugin only where [`ImportProtection::native`] says
//! so: a dev server whose `importProtection.behavior` is `"error"` in dev,
//! with no `onViolation` callback and only patterns Rust reads the way
//! picomatch and JS regular expressions do. Anywhere else (a build, the
//! `"mock"` behavior) the plugin runs in the plugin host as before.

mod mock;
mod report;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use oj_compiler::glob::GlobMatcher;
use serde_json::Value;

pub use mock::{load_virtual, resolve_virtual};

const MARKER_PREFIX: &str = "tanstack-start-import-protection:marker:";
const FRAMEWORKS: [&str; 3] = ["react", "solid", "vue"];

/// The boundary an environment is on: Vite's `client` environment, or a
/// server one (`ssr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
        }
    }

    fn index(self) -> usize {
        match self {
            Side::Client => 0,
            Side::Server => 1,
        }
    }
}

/// One deny pattern, matched as Start's `compileMatcher` matches it.
#[derive(Debug)]
enum Pattern {
    /// A string with no glob syntax: equality.
    Literal(String),
    /// picomatch with `dot: true`.
    Glob {
        text: String,
        glob: GlobMatcher,
        /// For `x/**`, `x` itself, which picomatch matches too.
        prefix: Option<GlobMatcher>,
    },
    Regex {
        text: String,
        regex: regex::Regex,
    },
}

impl Pattern {
    fn from_json(v: &Value) -> Option<Pattern> {
        if let Some(text) = v.get("text").and_then(Value::as_str) {
            return Pattern::from_text(text);
        }
        let source = v.get("source")?.as_str()?;
        let flags = v.get("flags").and_then(Value::as_str).unwrap_or("");
        // A sticky pattern matches at the string's start only.
        if flags.contains('y') {
            return None;
        }
        let rust = crate::plugins::rust_regex_from_js(source, flags)?;
        Some(Pattern::Regex {
            text: format!("/{source}/{flags}"),
            regex: regex::Regex::new(&rust).ok()?,
        })
    }

    fn from_text(text: &str) -> Option<Pattern> {
        if !text.contains(['*', '?', '!', '[', ']', '{', '}', '(', ')']) {
            return Some(Pattern::Literal(text.to_string()));
        }
        if !reads_as_picomatch(text) {
            return None;
        }
        let glob = GlobMatcher::plugin_id_filter(text)?;
        // A matcher of a glob that did not build matches everything.
        glob.bases().next()?;
        let prefix = match text.strip_suffix("/**") {
            Some(prefix) if !prefix.is_empty() => {
                let glob = GlobMatcher::plugin_id_filter(prefix)?;
                glob.bases().next()?;
                Some(glob)
            }
            _ => None,
        };
        Some(Pattern::Glob {
            text: text.to_string(),
            glob,
            prefix,
        })
    }

    fn is_match(&self, value: &str) -> bool {
        match self {
            Pattern::Literal(text) => text == value,
            Pattern::Glob { glob, prefix, .. } => {
                glob.is_match(value) || prefix.as_ref().is_some_and(|p| p.is_match(value))
            }
            Pattern::Regex { regex, .. } => regex.is_match(value),
        }
    }

    /// The pattern as Start's messages print it (`String(pattern)`).
    fn text(&self) -> &str {
        match self {
            Pattern::Literal(text) | Pattern::Glob { text, .. } | Pattern::Regex { text, .. } => {
                text
            }
        }
    }
}

/// Whether oj's glob matcher reads `glob` as picomatch does. It does not
/// for a leading `./` (picomatch strips it), parentheses (a regex group to
/// picomatch), brackets and escapes (a literal to picomatch where they do
/// not parse), and braces without a comma (a literal to picomatch).
fn reads_as_picomatch(glob: &str) -> bool {
    if glob.starts_with("./") || glob.contains(['(', ')', '[', ']', '\\']) {
        return false;
    }
    let mut rest = glob;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return false;
        }
        let Some(len) = rest[open..].find('}') else {
            return false;
        };
        let body = &rest[open + 1..open + len];
        if body.contains('{') || !body.contains(',') {
            return false;
        }
        rest = &rest[open + len + 1..];
    }
    true
}

/// A JSON list of patterns, or None when one of them cannot be carried.
fn patterns(v: Option<&Value>) -> Option<Vec<Pattern>> {
    let mut out: Vec<Pattern> = Vec::new();
    for item in v.and_then(Value::as_array).into_iter().flatten() {
        let pattern = Pattern::from_json(item)?;
        // Start's dedupePatterns.
        if !out.iter().any(|p| p.text() == pattern.text()) {
            out.push(pattern);
        }
    }
    Some(out)
}

fn texts(list: &[&str]) -> Vec<Pattern> {
    list.iter().filter_map(|t| Pattern::from_text(t)).collect()
}

#[derive(Debug)]
struct Rules {
    specifiers: Vec<Pattern>,
    files: Vec<Pattern>,
    exclude_files: Vec<Pattern>,
}

impl Rules {
    fn denied_file(&self, relative: &str) -> Option<&Pattern> {
        if self.exclude_files.iter().any(|p| p.is_match(relative)) {
            return None;
        }
        self.files.iter().find(|p| p.is_match(relative))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockAccess {
    Error,
    Warn,
    Off,
}

/// What [`ImportProtection::before_resolve`] says about an import.
#[derive(Debug, PartialEq, Eq)]
pub enum Before {
    /// Outside import protection: resolve as usual.
    Pass,
    /// Resolve as usual, then hand the answer to
    /// [`ImportProtection::after_resolve`].
    Check,
    /// A marker import: this module id, which [`load_virtual`] serves.
    Virtual(String),
    /// A violation, as the message Start's plugin throws.
    Denied(String),
}

#[derive(Debug, Default)]
struct Graph {
    /// Resolved module to its importers, each with the specifier it wrote,
    /// in the order the edges were first seen.
    reverse: HashMap<String, Vec<(String, String)>>,
    forward: HashMap<String, HashSet<String>>,
    entries: HashSet<String>,
}

impl Graph {
    fn add_edge(&mut self, resolved: &str, importer: &str, specifier: &str) {
        let importers = self.reverse.entry(resolved.to_string()).or_default();
        match importers.iter_mut().find(|(file, _)| file == importer) {
            Some(edge) => edge.1 = specifier.to_string(),
            None => importers.push((importer.to_string(), specifier.to_string())),
        }
        self.forward
            .entry(importer.to_string())
            .or_default()
            .insert(resolved.to_string());
    }

    /// Drops the imports `file` made. Its importers did not change, and
    /// stay.
    fn invalidate(&mut self, file: &str) {
        for resolved in self.forward.remove(file).into_iter().flatten() {
            if let Some(importers) = self.reverse.get_mut(&resolved) {
                importers.retain(|(importer, _)| importer != file);
            }
        }
    }
}

#[derive(Debug, Default)]
struct State {
    /// The marker each file imported, shared by both sides.
    markers: HashMap<String, Side>,
    graphs: [Graph; 2],
}

#[derive(Debug)]
pub struct ImportProtection {
    root: String,
    src_dir: String,
    rules: [Rules; 2],
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    ignore_importers: Vec<Pattern>,
    max_trace_depth: usize,
    mock_access: MockAccess,
    state: Mutex<State>,
}

impl ImportProtection {
    /// Import protection for the app at `root`, from the options extraction
    /// recorded (`vite_start_import_protection`), when this module can stand
    /// in for Start's plugin. None leaves the plugin in the plugin host.
    pub fn native(root: &Path, options: Option<&Value>) -> Option<Arc<ImportProtection>> {
        Self::from_options(root, options?)
    }

    /// [`native`](Self::native) for a dev server over `config`. A server
    /// whose documents a custom dev environment renders (a worker runtime)
    /// keeps Start's plugin: nothing of oj's resolves that environment's
    /// imports.
    pub fn for_dev(root: &Path, config: &oj_config::OjConfig) -> Option<Arc<ImportProtection>> {
        if oj_config::ssr_runner_backed(config) {
            return None;
        }
        Self::native(root, config.vite_start_import_protection.as_ref())
    }

    fn from_options(root: &Path, o: &Value) -> Option<Arc<ImportProtection>> {
        if !o
            .get("version")
            .and_then(Value::as_str)
            .is_some_and(supported_version)
        {
            return None;
        }
        let flag = |key: &str| o.get(key).and_then(Value::as_bool);
        if flag("enabled") != Some(true) || flag("onViolation") != Some(false) {
            return None;
        }
        if o.get("devBehavior").and_then(Value::as_str) != Some("error") {
            return None;
        }
        let mock_access = match o.get("mockAccess").and_then(Value::as_str)? {
            "error" => MockAccess::Error,
            "warn" => MockAccess::Warn,
            "off" => MockAccess::Off,
            _ => return None,
        };
        let root = normalize(&root.to_string_lossy());
        let src_dir = normalize(&format!(
            "{root}/{}",
            o.get("srcDirectory").and_then(Value::as_str)?
        ));
        // A list the app set replaces the default one, except the client's
        // denied specifiers, which add to Start's own.
        let list = |side: &str, key: &str, default: &[&str]| -> Option<Vec<Pattern>> {
            match o.get(side).and_then(|rules| rules.get(key)) {
                Some(Value::Null) | None => Some(texts(default)),
                set => patterns(set),
            }
        };
        let mut client_specifiers: Vec<Pattern> = FRAMEWORKS
            .iter()
            .filter_map(|fw| Pattern::from_text(&format!("@tanstack/{fw}-start/server")))
            .collect();
        for pattern in list("client", "specifiers", &[])? {
            if !client_specifiers.iter().any(|p| p.text() == pattern.text()) {
                client_specifiers.push(pattern);
            }
        }
        let client = Rules {
            specifiers: client_specifiers,
            files: list("client", "files", &["**/*.server.*"])?,
            exclude_files: list("client", "excludeFiles", &["**/node_modules/**"])?,
        };
        let server = Rules {
            specifiers: list("server", "specifiers", &[])?,
            files: list("server", "files", &["**/*.client.*"])?,
            exclude_files: list("server", "excludeFiles", &["**/node_modules/**"])?,
        };
        // Start's registerEntries: the router and the start file root a
        // trace on both sides, whatever imports them.
        let mut state = State::default();
        for (key, default) in [("routerEntry", "router"), ("startEntry", "start")] {
            let name = o.get(key).and_then(Value::as_str).unwrap_or(default);
            if let Some(file) = entry_file(&src_dir, name) {
                for graph in &mut state.graphs {
                    graph.entries.insert(file.clone());
                }
            }
        }
        Some(Arc::new(ImportProtection {
            root,
            src_dir,
            rules: [client, server],
            include: patterns(o.get("include"))?,
            exclude: patterns(o.get("exclude"))?,
            ignore_importers: patterns(o.get("ignoreImporters"))?,
            max_trace_depth: o
                .get("maxTraceDepth")
                .and_then(Value::as_u64)
                .map_or(20, |n| n as usize),
            mock_access,
            state: Mutex::new(state),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn relative<'a>(&self, path: &'a str) -> &'a str {
        relativize(path, &self.root)
    }

    /// Start's `shouldCheckImporter`.
    fn in_scope(&self, file: &str) -> bool {
        let relative = self.relative(file);
        let any = |list: &[Pattern]| list.iter().any(|p| p.is_match(relative));
        if any(&self.exclude) || any(&self.ignore_importers) {
            return false;
        }
        if !self.include.is_empty() {
            return any(&self.include);
        }
        self.src_dir.is_empty() || is_inside(file, &self.src_dir)
    }

    /// A module a bundle or a render starts from: where a trace ends.
    pub fn add_entry(&self, side: Side, id: &str) {
        self.state().graphs[side.index()]
            .entries
            .insert(id.to_string());
    }

    /// Start's resolveId up to its own `this.resolve`: marker imports and
    /// denied specifiers, for `source` as `importer` wrote it.
    pub fn before_resolve(&self, side: Side, importer: &str, source: &str) -> Before {
        if source.starts_with('\0') || source.starts_with("virtual:") {
            return Before::Pass;
        }
        let importer = normalize_id(importer);
        if let Some(marker) = marker_kind(source) {
            let mut state = self.state();
            if state.markers.get(&importer).is_some_and(|m| *m != marker) {
                return Before::Denied(format!(
                    "[import-protection] File \"{}\" has both server-only and client-only markers. This is not allowed.",
                    self.relative(&importer)
                ));
            }
            state.markers.insert(importer.clone(), marker);
            if marker != side {
                let violation = report::Violation {
                    side,
                    kind: report::Kind::Marker,
                    importer: &importer,
                    specifier: source,
                    resolved: None,
                };
                return Before::Denied(report::message(
                    self,
                    &state.graphs[side.index()],
                    &violation,
                ));
            }
            return Before::Virtual(format!("\0{MARKER_PREFIX}{}-only", marker.name()));
        }
        if !self.in_scope(&importer) {
            return Before::Pass;
        }
        if let Some(pattern) = self.rules[side.index()]
            .specifiers
            .iter()
            .find(|p| p.is_match(source))
        {
            let mut state = self.state();
            let graph = &mut state.graphs[side.index()];
            graph.add_edge(source, &importer, source);
            let violation = report::Violation {
                side,
                kind: report::Kind::Specifier(pattern.text()),
                importer: &importer,
                specifier: source,
                resolved: None,
            };
            return Before::Denied(report::message(self, graph, &violation));
        }
        Before::Check
    }

    /// The rest of Start's resolveId, for an import [`Before::Check`] was
    /// answered for and `resolved` is the module of: denied files, and
    /// modules a marker restricts to the other side. The violation is the
    /// message Start's plugin throws.
    pub fn after_resolve(
        &self,
        side: Side,
        importer: &str,
        source: &str,
        resolved: &str,
    ) -> Result<(), String> {
        let importer = normalize_id(importer);
        let resolved = self.canonical(resolved);
        let mut state = self.state();
        state.graphs[side.index()].add_edge(&resolved, &importer, source);
        let rules = &self.rules[side.index()];
        let relative = self.relative(&resolved);
        if rules.exclude_files.iter().any(|p| p.is_match(relative)) {
            return Ok(());
        }
        let kind = match rules.files.iter().find(|p| p.is_match(relative)) {
            Some(pattern) => report::Kind::File(pattern.text()),
            None if state.markers.get(&resolved).is_some_and(|m| *m != side) => {
                report::Kind::Marker
            }
            None => return Ok(()),
        };
        let violation = report::Violation {
            side,
            kind,
            importer: &importer,
            specifier: source,
            resolved: Some(&resolved),
        };
        Err(report::message(
            self,
            &state.graphs[side.index()],
            &violation,
        ))
    }

    /// Whether `id` is a file the rules deny to `side` and import
    /// protection covers, so [`self_denial`](Self::self_denial) mocks it.
    pub fn denies(&self, side: Side, id: &str) -> bool {
        if id.starts_with('\0') {
            return false;
        }
        let file = normalize_id(id);
        is_script(&file)
            && self.in_scope(&file)
            && self.rules[side.index()]
                .denied_file(self.relative(&file))
                .is_some()
    }

    /// Start's self-denial: the mock module that stands in for a file denied
    /// on `side`, whatever imported it (an importer outside import
    /// protection's scope reaches it with no violation). `code` is the file's
    /// own, read for the names it exports.
    pub fn self_denial(&self, side: Side, id: &str, code: &str) -> Option<String> {
        if !self.denies(side, id) {
            return None;
        }
        let file = normalize_id(id);
        let names = report::named_exports(code, Path::new(&file));
        let runtime = mock::runtime_id(side, &file, self.relative(&file), self.mock_access);
        Some(mock::self_denial_module(&names, &runtime))
    }

    /// Forgets what `side` learned about `file`, which changed and `side`
    /// reads again.
    pub fn changed(&self, side: Side, file: &str) {
        let file = normalize_id(file);
        let mut state = self.state();
        state.markers.remove(&file);
        state.graphs[side.index()].invalidate(&file);
    }

    /// Start's `canonicalizeResolvedId`.
    fn canonical(&self, id: &str) -> String {
        let id = normalize_id(id);
        let has_scheme = id.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic())
        });
        if Path::new(&id).is_absolute() || id.starts_with('.') || id.starts_with('\0') || has_scheme
        {
            return id;
        }
        normalize(&format!("{}/{id}", self.root))
    }
}

/// Start's `resolveEntry`: the file `name` is in `src_dir`, as written or
/// with one of the extensions Start tries.
fn entry_file(src_dir: &str, name: &str) -> Option<String> {
    let base = normalize(&Path::new(src_dir).join(name).to_string_lossy());
    std::iter::once(base.clone())
        .chain(
            [".ts", ".js", ".mts", ".mjs", ".tsx", ".jsx"]
                .iter()
                .map(|ext| format!("{base}{ext}")),
        )
        .find(|file| Path::new(file).is_file())
}

/// Whether `file` is JavaScript or TypeScript: what Start's self-denial
/// transform runs on.
fn is_script(file: &str) -> bool {
    Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e,
                "js" | "jsx"
                    | "ts"
                    | "tsx"
                    | "mjs"
                    | "mjsx"
                    | "mts"
                    | "mtsx"
                    | "cjs"
                    | "cjsx"
                    | "cts"
                    | "ctsx"
            )
        })
}

/// Whether `version` is a `@tanstack/start-plugin-core` whose import
/// protection this module was checked against: the plugin's rules and
/// messages are its own to change, so any other version runs the plugin.
fn supported_version(version: &str) -> bool {
    let mut parts = version.split(['.', '-']).map(|p| p.parse::<u32>().ok());
    let (Some(Some(major)), Some(Some(minor)), Some(Some(patch))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (1, 171, 34) <= (major, minor, patch) && (major, minor) <= (1, 172)
}

fn marker_kind(source: &str) -> Option<Side> {
    let rest = source.strip_prefix("@tanstack/")?;
    let (framework, marker) = rest.split_once("-start/")?;
    if !FRAMEWORKS.contains(&framework) {
        return None;
    }
    match marker {
        "server-only" => Some(Side::Server),
        "client-only" => Some(Side::Client),
        _ => None,
    }
}

/// Start's `normalizeFilePath`: the id without its query and hash, as a
/// `/`-separated path with no `.` or `..` parts.
fn normalize_id(id: &str) -> String {
    normalize(id.split(['?', '#']).next().unwrap_or(id))
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in Path::new(&path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out.to_string_lossy().replace('\\', "/")
}

/// Start's `relativizePath`: `path` under `root` without it, else as it is.
fn relativize<'a>(path: &'a str, root: &str) -> &'a str {
    match path.strip_prefix(root) {
        Some("") => "",
        Some(rest) => rest.strip_prefix('/').unwrap_or(path),
        None => path,
    }
}

fn is_inside(file: &str, directory: &str) -> bool {
    file.strip_prefix(directory)
        .is_some_and(|rest| rest.len() > 1 && rest.starts_with('/'))
}

#[cfg(test)]
mod tests;
