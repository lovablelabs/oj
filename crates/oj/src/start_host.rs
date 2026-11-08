// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The in-process TanStack Start runner: an embedded JS engine whose module
//! loading reproduces the semantics of the retired Node loader
//! (`registerHooks` + `loader.mjs`) on top of the dev server's SSR pipeline.
//!
//! Resolution and loading decisions live here, in the [`ModuleHost`]; what
//! genuinely stays JS (the `fetch` handler protocol) lives in the bootstrap
//! module. Compared to the node loader:
//! - TS/JSX stripping, `define`/`import.meta.env`, `import.meta.glob` and the
//!   plugin transform chain come from the same Rust pipeline the SSR runner
//!   uses (`SsrBridge::transform_module`), replacing the node loader's
//!   rolldown `transformSync`, glob transform and FIFO plugin bridge.
//! - CSS-module class names come from the Rust side (`oj_css`), replacing the
//!   loader's SipHash mirror.
//! - CJS facades, directory-entry recovery and extension probing inside
//!   node_modules are the engine's byonm loading plus the Vite-style resolver
//!   (externals travel as the RESOLVED file, not the bare specifier).
//! - The on-disk transform cache keyed on the Node version is gone: the
//!   pipeline's own caches and the isolate's module map cover it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use oj_js::EngineConfig;
use oj_js::HostModule;
use oj_js::HostModuleType;
use oj_js::HostResolved;
use oj_js::JsEngine;
use oj_js::ModuleHost;
use oj_server::SsrBridge;
use oj_server::StartFramework;
use oj_server::StartResolution;

use crate::ssr_dev::normalize_path;
use crate::ssr_host::{importer_id, versioned_id, VersionGraph};

const START_BOOTSTRAP_JS: &str = include_str!("assets/start-bootstrap.mjs");

/// A module id the resolver rejected with a scheme the runtime cannot load
/// (an un-aliased `cloudflare:*`): served as an empty module instead of
/// crashing the render, like the node loader's scheme net.
const STUB_PREFIX: &str = "oj-start-stub:";

/// Extensions probed for the Start-alias and root-relative ladders (Vite's
/// DEFAULT_EXTENSIONS as files and directory indexes, like the loader's EXTS).
const PROBE_EXTS: [&str; 7] = [".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs", ".json"];

fn probe(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    let s = base.to_string_lossy();
    for ext in PROBE_EXTS {
        let p = PathBuf::from(format!("{s}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    for ext in PROBE_EXTS {
        let p = base.join(format!("index{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Vite's `ssr.noExternal` matcher (names, `@scope/*` globs, RegExp sources,
/// `true`), applied to package names; `external` names always win.
pub(crate) struct NoExternal {
    rules: oj_config::SsrExternals,
    regexes: Vec<regex::Regex>,
}

impl NoExternal {
    fn new(rules: oj_config::SsrExternals) -> NoExternal {
        let regexes = rules
            .no_external_regex
            .iter()
            .filter_map(|src| regex::Regex::new(src).ok())
            .collect();
        NoExternal { rules, regexes }
    }

    fn matches_pkg(&self, pkg: &str) -> bool {
        if self.rules.external.iter().any(|e| e == pkg) || self.rules.external_all {
            return false;
        }
        if self.rules.no_external_all {
            return true;
        }
        if self.rules.no_external.iter().any(|p| glob_match(p, pkg)) {
            return true;
        }
        self.regexes.iter().any(|re| re.is_match(pkg))
    }

    pub(crate) fn matches_path(&self, path: &str) -> bool {
        match pkg_name_of_path(path) {
            Some(pkg) => self.matches_pkg(&pkg),
            None => false,
        }
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let re = format!(
        "^{}$",
        pattern
            .split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*")
    );
    regex::Regex::new(&re)
        .map(|r| r.is_match(value))
        .unwrap_or(false)
}

/// Package name of a node_modules path (`.../node_modules/@scope/pkg/x` ->
/// `@scope/pkg`).
fn pkg_name_of_path(path: &str) -> Option<String> {
    let i = path.rfind("/node_modules/")?;
    let rest = &path[i + "/node_modules/".len()..];
    let mut parts = rest.split('/');
    let first = parts.next().filter(|p| !p.is_empty())?;
    if let Some(scope) = first.strip_prefix('@').map(|_| first) {
        let second = parts.next().filter(|p| !p.is_empty())?;
        return Some(format!("{scope}/{second}"));
    }
    Some(first.to_string())
}

/// Decodes the bootstrap's standard base64 (Node `Buffer.toString("base64")`,
/// padded); oj owns both ends, so malformed input decodes to empty.
fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The server-side `createServerFn` rewrite the node loader applied
/// (loader-util.mjs `rewriteServerFns`): every
/// `const NAME = createServerFn(...).handler(` grows a sibling
/// `NAME_createServerFn_handler = createServerRpc(meta, ...)` export and the
/// handler's first argument, without adding lines (stack traces keep their
/// positions).
///
/// The `.handler(` must belong to the SAME expression as the
/// `createServerFn` call: the chain is followed with a bracket-depth scan
/// (strings, template literals, comments and regex literals skipped) instead
/// of a lazy `.*?`, which used to span across statements and anchor a
/// split-statement factory (`const factory = createServerFn(...); export
/// const doIt = factory.handler(...)`) to the WRONG variable. That split
/// shape is left untouched: TanStack's own compiler resolves it by following
/// variable bindings in the AST, which a textual rewrite cannot do safely.
pub(crate) fn rewrite_server_fns(code: &str, rel: &str, framework: StartFramework) -> String {
    if !code.contains("createServerFn") {
        return code.to_string();
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:^|[\n;])[ \t]*((?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*createServerFn)\b",
        )
        .expect("server-fn rewrite regex")
    });
    // (byte offset, text to insert there), collected per anchored chain.
    let mut edits: Vec<(usize, String)> = Vec::new();
    for caps in re.captures_iter(code) {
        let decl = caps.get(1).expect("decl group");
        let name = &caps[2];
        let Some(handler_args_at) = scan_chain_to_handler(code, decl.end()) else {
            continue; // no same-expression .handler( — leave the statement alone
        };
        let id = base64url(format!("{rel}#{name}").as_bytes());
        let meta = format!("{{ id: {id:?}, name: {name:?}, filename: {rel:?} }}");
        edits.push((
            decl.start(),
            format!(
                "export const {name}_createServerFn_handler = createServerRpc({meta}, (opts) => {name}.__executeServer(opts)); "
            ),
        ));
        edits.push((handler_args_at, format!("{name}_createServerFn_handler, ")));
    }
    if edits.is_empty() {
        return code.to_string();
    }
    edits.sort_by_key(|(at, _)| *at);
    let mut out = String::from(format!(
        "import {{ createServerRpc }} from \"{}/server-rpc\"; ",
        framework.start_package()
    ));
    let mut last = 0;
    for (at, text) in edits {
        out.push_str(&code[last..at]);
        out.push_str(&text);
        last = at;
    }
    out.push_str(&code[last..]);
    out
}

/// Follows a `createServerFn` method chain from `from` (the byte right after
/// the identifier) to the `.handler(` of the same expression, returning the
/// offset just past its opening paren. The chain grammar is strict — a call's
/// balanced arguments, then `.name` and the next call — so the scan ends
/// (returning `None`) at anything else: a `;`, a new statement on the next
/// line, an unexpected token. Bracket depth is tracked with strings, template
/// literals, comments and regex literals skipped, so a `)` inside a validator
/// string can not end the argument list early.
fn scan_chain_to_handler(code: &str, from: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut i = from;
    loop {
        // The chain's next call: balanced `( ... )`.
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'(' {
            return None;
        }
        i = skip_balanced_parens(bytes, i)?;
        // After the call only `.name` continues the chain.
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'.' {
            return None;
        }
        i = skip_ws_and_comments(bytes, i + 1)?;
        let name_start = i;
        while i < bytes.len()
            && (bytes[i] == b'$' || bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric())
        {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        let method = &code[name_start..i];
        i = skip_ws_and_comments(bytes, i)?;
        if bytes[i] != b'(' {
            return None;
        }
        if method == "handler" {
            return Some(i + 1);
        }
        // A non-handler link (.validator, .middleware, …): consume its call
        // on the next loop turn.
    }
}

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return None;
            }
            i += 2;
            continue;
        }
        return if i < bytes.len() { Some(i) } else { None };
    }
}

/// From an opening `(` at `open`, returns the index just past its matching
/// `)`. Tracks every bracket kind, skips strings, template literals (with
/// nested `${}` code), comments and regex literals (the `/`-after-operator
/// heuristic), and gives up (`None`) on EOF or mismatched brackets.
fn skip_balanced_parens(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'(');
    // Bracket stack; b'T' marks a template-literal `${` substitution, b'`'
    // template text itself.
    let mut stack: Vec<u8> = vec![b'('];
    // The previous significant code byte, for the regex-vs-division call.
    let mut prev: Option<u8> = Some(b'(');
    let mut i = open + 1;
    while i < bytes.len() {
        if stack.last() == Some(&b'`') {
            match bytes[i] {
                b'\\' => i += 2,
                b'`' => {
                    stack.pop();
                    i += 1;
                }
                b'$' if bytes.get(i + 1) == Some(&b'{') => {
                    stack.push(b'T');
                    prev = Some(b'{');
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                i = skip_string(bytes, i)?;
                prev = Some(b'"');
            }
            b'`' => {
                stack.push(b'`');
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return None;
                }
                i += 2;
            }
            b'/' if regex_can_follow(prev) => {
                i = skip_regex_literal(bytes, i)?;
                prev = Some(b'/');
            }
            b'(' | b'[' | b'{' => {
                stack.push(c);
                prev = Some(c);
                i += 1;
            }
            b')' | b']' => {
                let want = if c == b')' { b'(' } else { b'[' };
                if stack.pop() != Some(want) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(i + 1);
                }
                prev = Some(c);
                i += 1;
            }
            b'}' => {
                match stack.pop() {
                    Some(b'{') | Some(b'T') => {}
                    _ => return None,
                }
                prev = Some(c);
                i += 1;
            }
            _ => {
                if !c.is_ascii_whitespace() {
                    prev = Some(c);
                }
                i += 1;
            }
        }
    }
    None
}

/// Whether a `/` after this significant byte starts a regex literal rather
/// than a division: after an operator, an opener or a separator it can only
/// be a regex (`.validator((s) => /x\(/.test(s))`), after a value it is a
/// division. Keyword-preceded regexes (`return /x/`) read as
/// identifier-preceded here and mis-classify as division — acceptable inside
/// call arguments, where a bare `return` at bracket depth is already rare.
fn regex_can_follow(prev: Option<u8>) -> bool {
    match prev {
        None => true,
        Some(c) => matches!(
            c,
            b'(' | b'['
                | b'{'
                | b','
                | b';'
                | b':'
                | b'!'
                | b'&'
                | b'|'
                | b'?'
                | b'='
                | b'+'
                | b'-'
                | b'*'
                | b'%'
                | b'<'
                | b'>'
                | b'~'
                | b'^'
        ),
    }
}

fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == quote => return Some(i + 1),
            b'\n' => return Some(i + 1), // unterminated: fail soft at the line end
            _ => i += 1,
        }
    }
    None
}

fn skip_regex_literal(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' if in_class => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => return Some(i + 1),
            b'\n' => return None, // not a regex after all
            _ => i += 1,
        }
    }
    None
}

fn ext_of(path: &str) -> &str {
    let path = path.split_once('?').map_or(path, |(clean, _)| clean);
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}

/// Whether a resolved dependency file may stay external to an SSR bundle
/// (a stylesheet, JSON or raw TypeScript bundles through the plugins instead).
pub(crate) use oj_server::can_externalize_file;

/// A source file the runtime cannot load natively from node_modules: Node and
/// byonm refuse to strip types there, and nothing executes raw JSX.
pub(crate) fn ts_source_ext(path: &str) -> bool {
    matches!(
        ext_of(path).to_ascii_lowercase().as_str(),
        "ts" | "mts" | "cts" | "tsx" | "jsx"
    )
}

fn src_lang(path: &str) -> Option<&'static str> {
    // Case-insensitive, like `ts_source_ext`: a claimed `Badge.TSX` must also
    // compile, not fall back to the runtime it was claimed away from.
    match ext_of(path).to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => Some("ts"),
        "tsx" => Some("tsx"),
        "jsx" => Some("jsx"),
        "js" | "mjs" => Some("js"),
        _ => None,
    }
}

/// The loader's `isCjsFile` (`.cjs` always, `.js` when the nearest
/// package.json is not `"type": "module"` and the file shows no ESM syntax),
/// memoized like the node loader's `pkgTypeCache`: this runs per resolution
/// on the shared runtime, and uncached it walked up to 40 directories and
/// read the whole module on every hit.
///
/// The package type is cached per starting directory for the host's lifetime
/// — a package.json `type` flip means a reinstall, which needs a dev-server
/// restart anyway (the engine's own module map is just as stale then), so
/// in-session invalidation buys nothing. The per-file ESM-syntax verdict is
/// keyed on the file's mtime, so an edited linked `file:` dependency is
/// re-read.
#[derive(Default)]
pub(crate) struct CjsCache {
    /// dir → nearest package.json has `"type": "module"`.
    pkg_is_module: Mutex<std::collections::HashMap<PathBuf, bool>>,
    /// file → (mtime at sniff time, is-CJS verdict).
    verdicts: Mutex<std::collections::HashMap<PathBuf, (std::time::SystemTime, bool)>>,
    /// file → (mtime at scan time, calls an undeclared `require`).
    requires: Mutex<std::collections::HashMap<PathBuf, (std::time::SystemTime, bool)>>,
}

impl CjsCache {
    fn nearest_pkg_is_module(&self, start: &Path) -> bool {
        if let Some(hit) = self.pkg_is_module.lock().unwrap().get(start) {
            return *hit;
        }
        let mut dir = Some(start);
        let mut is_module = false;
        for _ in 0..40 {
            let Some(d) = dir else { break };
            let pj = d.join("package.json");
            if pj.is_file() {
                is_module = std::fs::read_to_string(&pj)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| {
                        v.get("type")
                            .and_then(|t| t.as_str())
                            .map(|t| t == "module")
                    })
                    .unwrap_or(false);
                break;
            }
            dir = d.parent();
        }
        self.pkg_is_module
            .lock()
            .unwrap()
            .insert(start.to_path_buf(), is_module);
        is_module
    }

    fn is_cjs_file(&self, path: &str) -> bool {
        if path.ends_with(".cjs") {
            return true;
        }
        if !path.ends_with(".js") {
            return false;
        }
        let file = Path::new(path);
        if let Some(dir) = file.parent() {
            if self.nearest_pkg_is_module(dir) {
                return false;
            }
        }
        // The walk was inconclusive (commonjs or no package.json): the
        // full-file ESM-syntax sniff decides, cached per path + mtime.
        let mtime = std::fs::metadata(file).ok().and_then(|m| m.modified().ok());
        if let Some(mtime) = mtime {
            if let Some((seen, verdict)) = self.verdicts.lock().unwrap().get(file) {
                if *seen == mtime {
                    return *verdict;
                }
            }
        }
        let verdict = match std::fs::read_to_string(file) {
            Ok(src) => !has_esm_syntax(&src),
            Err(_) => false,
        };
        if let Some(mtime) = mtime {
            self.verdicts
                .lock()
                .unwrap()
                .insert(file.to_path_buf(), (mtime, verdict));
        }
        verdict
    }
}

impl CjsCache {
    /// Whether `path` is an ES module that calls a `require` it never
    /// declares: a bundler-only build no ESM runtime can evaluate (turndown's
    /// `module` entry requires its Node DOM parser on first use), cached per
    /// path + mtime.
    fn esm_calls_require(&self, path: &str) -> bool {
        if !(path.ends_with(".js") || path.ends_with(".mjs")) || self.is_cjs_file(path) {
            return false;
        }
        let file = Path::new(path);
        let mtime = std::fs::metadata(file).ok().and_then(|m| m.modified().ok());
        if let Some(mtime) = mtime {
            if let Some((seen, verdict)) = self.requires.lock().unwrap().get(file) {
                if *seen == mtime {
                    return *verdict;
                }
            }
        }
        let verdict = std::fs::read_to_string(file)
            .map(|src| calls_undeclared_require(&src))
            .unwrap_or(false);
        if let Some(mtime) = mtime {
            self.requires
                .lock()
                .unwrap()
                .insert(file.to_path_buf(), (mtime, verdict));
        }
        verdict
    }
}

/// Whether module code calls a `require` no scope of its own declares (a
/// `createRequire` result or a wrapper's parameter is declared): directly,
/// through a member (`require.resolve(id)`, esbuild's `__require` shim
/// calling `require.apply`) or as `(0, require)(id)`. A call guarded by
/// `typeof require` counts too: Node's entry, which it then gets, is Vite's
/// pick anyway.
fn calls_undeclared_require(src: &str) -> bool {
    use oxc_ast::ast::{CallExpression, Expression};
    use oxc_ast_visit::{walk, Visit};

    struct Scan<'s> {
        scoping: &'s oxc_semantic::Scoping,
        calls: bool,
    }
    impl Scan<'_> {
        fn global_require(&self, expr: &Expression) -> bool {
            match expr.without_parentheses() {
                Expression::Identifier(id) => {
                    id.name == "require"
                        && id
                            .reference_id
                            .get()
                            .is_some_and(|r| self.scoping.get_reference(r).symbol_id().is_none())
                }
                Expression::SequenceExpression(seq) => seq
                    .expressions
                    .last()
                    .is_some_and(|e| self.global_require(e)),
                _ => false,
            }
        }
    }
    impl<'a> Visit<'a> for Scan<'_> {
        fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
            let callee = it.callee.without_parentheses();
            let object = match callee {
                Expression::StaticMemberExpression(m) => Some(&m.object),
                Expression::ComputedMemberExpression(m) => Some(&m.object),
                _ => None,
            };
            if self.global_require(callee) || object.is_some_and(|o| self.global_require(o)) {
                self.calls = true;
            }
            walk::walk_call_expression(self, it);
        }
    }

    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let word = WORD.get_or_init(|| regex::Regex::new(r"\brequire\b").expect("require regex"));
    if !word.is_match(src) {
        return false;
    }
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, src, oxc_span::SourceType::mjs()).parse();
    if !parsed.diagnostics.is_empty() {
        return false;
    }
    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parsed.program)
        .semantic;
    let mut scan = Scan {
        scoping: semantic.scoping(),
        calls: false,
    };
    scan.visit_program(&parsed.program);
    scan.calls
}

/// The package a bare specifier names (`@scope/name` or `name`), or None for
/// a relative, absolute, `#` or builtin one.
fn bare_package(spec: &str) -> Option<&str> {
    if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with('#') {
        return None;
    }
    if oj_server::is_node_builtin(spec) || spec.contains(':') {
        return None;
    }
    let segments = if spec.starts_with('@') { 2 } else { 1 };
    let end = spec
        .match_indices('/')
        .nth(segments - 1)
        .map_or(spec.len(), |(i, _)| i);
    Some(&spec[..end])
}

fn has_esm_syntax(src: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?:^|[\n;])\s*export\s|(?:^|[\n;])\s*import\s[^(]")
            .expect("esm syntax regex")
    });
    re.is_match(src)
}

fn mime_of(path: &str) -> &'static str {
    match ext_of(path).to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "mov" => "video/quicktime",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "css" => "text/css",
        "json" => "application/json",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Classify a resolved filesystem path into the module id the host serves: a
/// plain path (source modules, svg, json, mdx), or a path with an `?ojasset` /
/// `?react` tag for asset intents, styles and importable assets (resolve
/// first, classify second — Vite's ordering).
fn classify_id(abs: &str, intent: Option<&'static str>, svg_react: bool) -> String {
    let clean = abs.split_once('?').map_or(abs, |(clean, _)| clean);
    if svg_react {
        return format!("{clean}?react");
    }
    if let Some(kind) = intent {
        return format!("{clean}?ojasset={kind}");
    }
    let ext = ext_of(abs).to_ascii_lowercase();
    if oj_server::is_style_ext(&ext) {
        return format!("{clean}?ojasset=css");
    }
    // wasm is asset-classified like the loader (URL export only). A plain
    // import is `asset`: served like `?url`, but its id has no query.
    if oj_server::is_importable_asset_ext(&ext) || ext == "wasm" {
        return format!("{abs}?ojasset=asset");
    }
    abs.to_string()
}

/// The intent query the loader recognized on a specifier, parsed like the
/// build (Vite's query grammar: `?worker&url`, `?url&no-inline`, ...).
fn split_intent(spec: &str) -> (String, Option<&'static str>, bool) {
    let Some((clean, query)) = crate::build::split_asset_query(spec) else {
        return (spec.to_string(), None, false);
    };
    let tag: &'static str = match query.as_str() {
        "react" if clean.ends_with(".svg") => return (clean, None, true),
        "raw" => "raw",
        "url" | "no-inline" => "url",
        "inline" => "inline",
        // Unbundled dev ignores `&inline` on a worker (Vite's worker load only
        // inlines when the environment is bundled), so `&url` wins whenever it
        // appears -- the build's precedence is the reverse (inline first).
        "worker" | "worker&inline" => "worker",
        "worker&url" | "worker&inline&url" => match worker_type_of(spec) {
            "classic" => "worker-url:classic",
            "ignore" => "worker-url:ignore",
            _ => "worker-url",
        },
        "sharedworker" | "sharedworker&inline" => "sharedworker",
        "sharedworker&url" | "sharedworker&inline&url" => match worker_type_of(spec) {
            "classic" => "sharedworker-url:classic",
            "ignore" => "sharedworker-url:ignore",
            _ => "sharedworker-url",
        },
        _ => return (spec.to_string(), None, false),
    };
    (clean, Some(tag), false)
}

/// The worker type oj's rewrite of `new Worker(new URL(...), opts)` carries
/// (`worker_type=`, see oj_compiler's glob/meta_glob), else `module`: what an
/// import of `./w.ts?worker` always is in Vite's dev.
fn worker_type_of(spec: &str) -> &'static str {
    let query = spec.split_once('?').map_or("", |(_, q)| q);
    match query
        .split('&')
        .find_map(|kv| kv.strip_prefix("worker_type="))
    {
        Some("classic") => "classic",
        Some("ignore") => "ignore",
        _ => "module",
    }
}

/// The `#tanstack-start-entry` target: the app's `src/start.*` when one
/// exists, else the bundled stub. Upstream resolves this entry the same way
/// (`startFilePath ?? defaultEntryPaths.start` in start-plugin-core's
/// planning.ts), and its default entry is exactly the stub's content
/// (`export const startInstance = undefined`). Resolving to the stub while a
/// real `src/start.ts` exists silently drops the app's `createStart` options:
/// request middleware server-side and function middleware on every server-fn
/// call. The app's package.json `imports` still wins over both in
/// `resolve_inner`.
fn start_entry_target(root: &Path, cache: &Path) -> PathBuf {
    start_file(root).unwrap_or_else(|| cache.join("start-entry.ts"))
}

/// Upstream's `src/start` lookup (start-plugin-core's resolveStartEntryPlan):
/// exsolve with these extensions in this order, no directory index.
const START_ENTRY_EXTS: [&str; 6] = [".ts", ".js", ".mts", ".mjs", ".tsx", ".jsx"];

fn start_file(root: &Path) -> Option<PathBuf> {
    START_ENTRY_EXTS
        .iter()
        .map(|ext| root.join(format!("src/start{ext}")))
        .find(|p| p.is_file())
}

/// The app's own start entry when one exists: the `package.json`
/// `imports["#tanstack-start-entry"]` target first (it wins over the
/// convention in `resolve_inner` and in the client bundle's probe), then the
/// `src/start.*` ladder. `None` means the framework-default stub serves.
/// Presence is a config-time input (upstream resolves it once in its plugin's
/// `config` hook), so the dev watcher restarts on a flip of this answer.
pub(crate) fn app_start_entry(root: &Path) -> Option<PathBuf> {
    let declared = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("imports")?
                .get("#tanstack-start-entry")?
                .as_str()
                .map(String::from)
        });
    if let Some(target) = declared {
        let p = root.join(target);
        if p.is_file() {
            return Some(p);
        }
    }
    start_file(root)
}

/// The Start framework-seam aliases (the retired node loader's ALIASES map):
/// resolved targets under the app or the start cache dir. start-server-core
/// imports these by bare specifier and expects the bundler to answer; one the
/// host does not map fails every document request.
fn framework_aliases(
    root: &Path,
    cache: &Path,
    framework: StartFramework,
) -> Vec<(String, PathBuf)> {
    vec![
        ("#tanstack-router-entry".into(), root.join("src/router")),
        (
            "#tanstack-start-entry".into(),
            start_entry_target(root, cache),
        ),
        (
            "#tanstack-start-plugin-adapters".into(),
            cache.join("plugin-adapters.ts"),
        ),
        (
            "#tanstack-start-server-fn-resolver".into(),
            cache.join("server-fn-resolver.mjs"),
        ),
        (
            "tanstack-start-manifest:v".into(),
            cache.join("manifest-dev.ts"),
        ),
        (
            "tanstack-start-injected-head-scripts:v".into(),
            cache.join("injected-head-scripts.ts"),
        ),
        (
            "@cloudflare/vite-plugin/server".into(),
            cache.join("cf-server.mjs"),
        ),
        // Vite externalizes `cloudflare:*` into workerd; here there is no
        // workerd, so the one module server fns actually use resolves to a
        // dev stub whose `env` comes from the wrangler vars. Other
        // cloudflare:* ids fall to the empty-module net in resolve.
        ("cloudflare:workers".into(), cache.join("cf-workers.mjs")),
        // Start's default server entry is oj's runner entry: an app
        // `server.entry` that wraps it composes the same way as in Vite.
        (
            format!("{}/server-entry", framework.start_package()),
            cache.join(framework.server_entry_file()),
        ),
    ]
}

pub struct StartHost {
    bridge: SsrBridge,
    root: PathBuf,
    /// `publicDir` (adopted from the config); `None` when disabled.
    public_dir: Option<PathBuf>,
    /// The Start framework-seam aliases (the loader's ALIASES map): resolved
    /// targets under the app or the start cache dir.
    aliases: Vec<(String, PathBuf)>,
    framework: StartFramework,
    no_external: NoExternal,
    graph: VersionGraph,
    /// buildStart for the ssr plugin container ran (once per session), so
    /// compile-on-startup plugins have their state before any load().
    plugins_started: tokio::sync::OnceCell<()>,
    warned_schemes: Mutex<HashSet<String>>,
    /// Memoized CJS detection (package type per dir, ESM sniff per file).
    cjs: CjsCache,
}

impl StartHost {
    pub fn new(
        root: PathBuf,
        cache: &Path,
        bridge: SsrBridge,
        no_external: oj_config::SsrExternals,
        public_dir: Option<PathBuf>,
        framework: StartFramework,
    ) -> Arc<StartHost> {
        let aliases = framework_aliases(&root, cache, framework);
        // The framework's own dev runner plan (`resolve.noExternal`) runs the
        // Start runtime inside the module graph; claim the same seam here so
        // an app edit re-evaluates the runtime's cached entries with the app
        // (see `claims_dependency`). An explicit user `ssr.external` still
        // wins, as in `matches_pkg`.
        let mut no_external = no_external;
        for pat in framework.start_runtime_no_external() {
            if !no_external.no_external.iter().any(|p| p == pat) {
                no_external.no_external.push(pat.to_string());
            }
        }
        Arc::new(StartHost {
            bridge,
            root,
            public_dir,
            aliases,
            framework,
            no_external: NoExternal::new(no_external),
            graph: VersionGraph::default(),
            plugins_started: tokio::sync::OnceCell::new(),
            warned_schemes: Mutex::new(HashSet::new()),
            cjs: CjsCache::default(),
        })
    }

    pub(crate) fn graph(&self) -> &VersionGraph {
        &self.graph
    }

    /// buildStart on the ssr plugin container, once, before its first use:
    /// Vite runs it before any module loads, and compile-on-startup plugins
    /// (i18n barrels) serve their state from load(). A throwing plugin is
    /// logged with attribution and skipped, never taking the dev server down.
    async fn ensure_plugins_started(&self) {
        self.plugins_started
            .get_or_init(|| async {
                let Some(host) = self.bridge.plugin_host().await else {
                    return;
                };
                if let Err(e) = host.build_start().await {
                    eprintln!("{}", crate::start_bundle::host::build_start_skipped(&e));
                }
            })
            .await;
    }

    fn alias_target(&self, spec: &str) -> Option<&Path> {
        self.aliases
            .iter()
            .find(|(find, _)| find == spec)
            .map(|(_, target)| target.as_path())
    }

    fn stub_specifier(&self, spec: &str) -> String {
        let scheme = spec.split(':').next().unwrap_or("").to_string();
        if self.warned_schemes.lock().unwrap().insert(scheme.clone()) {
            eprintln!(
                "oj: no SSR module for the '{scheme}:' scheme (e.g. {spec}); serving an empty module"
            );
        }
        self.graph.specifier_for(&format!("{STUB_PREFIX}{spec}"))
    }

    fn module_url(&self, id: &str, importer: String, generation: u64) -> HostResolved {
        HostResolved::Url(
            self.graph
                .edge_specifier_in_generation(id, importer, generation),
        )
    }

    async fn resolve_inner(
        &self,
        importer: &str,
        spec: &str,
        generation: u64,
    ) -> Result<Option<HostResolved>, String> {
        let importer_id = importer_id(importer);

        // The framework-seam aliases first, from every importer (tanstack's
        // own dist imports `tanstack-start-manifest:v` and friends). For
        // `#`-prefixed keys the app's own package.json `imports` wins over the
        // convention, so the resolver gets the first try.
        if let Some(target) = self.alias_target(spec) {
            let bridge_first = spec.starts_with('#');
            if !bridge_first {
                if let Some(hit) = probe(target) {
                    return Ok(Some(self.module_url(
                        &hit.to_string_lossy(),
                        importer_id,
                        generation,
                    )));
                }
            } else {
                if let Ok(StartResolution::Module(id)) =
                    self.bridge.resolve_start(&importer_id, spec).await
                {
                    return Ok(Some(self.module_url(&id, importer_id, generation)));
                }
                if let Some(hit) = probe(target) {
                    return Ok(Some(self.module_url(
                        &hit.to_string_lossy(),
                        importer_id,
                        generation,
                    )));
                }
            }
        }

        let is_plugin_shaped = spec.starts_with("virtual:") || spec.starts_with('\0');

        // node_modules internals resolve with plain Node semantics, as they
        // did under the node runner (only aliases and plugin ids cross) —
        // except bare specifiers. This importer was entered through the entry
        // the Vite-style resolver picked (mainFields: often an ESM `module`
        // entry plain Node would never choose), so its bare imports must
        // follow the same rule, or an ESM dependency file gets strict-linked
        // against a sibling dependency's CJS `main` — a UMD wrapper there
        // defeats the CJS named-export lexer (drei's ESM
        // `import { getGPUTier } from "detect-gpu"` resolved to the UMD and
        // died at link). `#` imports and Node builtins keep Node semantics —
        // a builtin outranks an installed polyfill package of the same name
        // here, as under Node and Vite SSR — and an unresolvable bare name
        // falls back to them too. A relative path keeps Node semantics while
        // its exact target exists; when it does not, the importer is a
        // bundler-only ESM build the Vite-style resolver picked
        // (functions-js's `module` entry re-exports from the extensionless
        // './FunctionsClient'), so the same resolver finishes the edge —
        // extension inference, and a directory's package.json entry under
        // the server mainFields — where plain Node 500s with
        // ERR_MODULE_NOT_FOUND on every request before first render.
        // A dependency module the host claimed for the pipeline is past this:
        // Vite never externalizes a relative or absolute specifier
        // (createIsExternal), so the imports of a module it transformed go
        // through the plugins too, and a component library's
        // `import "./select.css"` reaches the css pipeline instead of the
        // runtime loading the stylesheet as JavaScript. A builtin still
        // short-circuits first, as in Vite, so an installed polyfill package
        // of the same name never shadows it.
        if importer_id.contains("/node_modules/")
            && !is_plugin_shaped
            && (!self.claims_dependency(&importer_id) || oj_server::is_node_builtin(spec))
        {
            let bare = !spec.starts_with('.')
                && !spec.starts_with('/')
                && !spec.starts_with('#')
                && !oj_server::is_node_builtin(spec);
            if bare {
                if let Ok(StartResolution::Dependency(p)) =
                    self.bridge.resolve_start(&importer_id, spec).await
                {
                    if let Some(node) =
                        self.vite_external_instead(&importer_id, spec, &p.to_string_lossy())
                    {
                        return Ok(Some(node));
                    }
                    if let Ok(u) = url::Url::from_file_path(&p) {
                        return Ok(Some(HostResolved::External(u.to_string())));
                    }
                }
            }
            if spec.starts_with("./") || spec.starts_with("../") {
                if let Some(dir) = Path::new(&importer_id).parent() {
                    if !normalize_path(&dir.join(spec)).is_file() {
                        if let Ok(StartResolution::Dependency(p)) =
                            self.bridge.resolve_start(&importer_id, spec).await
                        {
                            if let Ok(u) = url::Url::from_file_path(&p) {
                                return Ok(Some(HostResolved::External(u.to_string())));
                            }
                        }
                    }
                }
            }
            return Ok(Some(HostResolved::External(spec.to_string())));
        }

        let (clean, intent, svg_react) = split_intent(spec);

        // Vite resolves an import once, as written: every resolveId hook sees
        // its query (`x.css?url`), and the intent is the answer's own query.
        // A /-prefixed id first goes to the `pre` hooks, then resolves against
        // the project root (Vite's asSrc) and publicDir; a real absolute fs
        // path falls through.
        if clean.starts_with('/') {
            if clean != spec {
                if let Some(hit) = self.bridge.resolve_start_pre(&importer_id, spec).await? {
                    return self.resolved(hit, spec, &importer_id, generation);
                }
            }
            let rel = clean.trim_start_matches('/');
            let public_hit = || {
                self.public_dir
                    .as_ref()
                    .and_then(|public| probe(&public.join(rel)))
            };
            if let Some(hit) = probe(&self.root.join(rel)).or_else(public_hit) {
                let id = classify_id(&hit.to_string_lossy(), intent, svg_react);
                return Ok(Some(self.module_url(&id, importer_id, generation)));
            }
        }

        let resolution = self
            .bridge
            .resolve_start(&importer_id, spec)
            .await
            .map_err(|e| format!("{e} (imported from {importer_id})"))?;
        self.resolved(resolution, spec, &importer_id, generation)
    }

    /// The file Vite's module runner would load for the bare `spec`, in
    /// place of the ESM build `file` the resolver picked, when that build
    /// calls an undeclared `require`. Vite loads every external as fetchModule
    /// resolves it (tryNodeResolve: mainFields ["main"], the external
    /// conditions), so turndown runs as its CommonJS `main` there; the
    /// `module` entry oj prefers for externals throws "require is not
    /// defined" on import. None (the picked file stands) for an aliased
    /// import, which Vite never externalizes, or another package's file.
    fn vite_external_instead(
        &self,
        importer: &str,
        spec: &str,
        file: &str,
    ) -> Option<HostResolved> {
        let package = bare_package(spec)?;
        if pkg_name_of_path(file).as_deref() != Some(package) || !self.cjs.esm_calls_require(file) {
            return None;
        }
        let vite = self.bridge.resolve_external(importer, spec)?;
        let url = url::Url::from_file_path(&vite).ok()?;
        Some(HostResolved::External(url.to_string()))
    }

    /// The host's answer for `spec`, resolved to `resolution`: the intent and
    /// svg `?react` read off the resolved id's own query, which a plugin may
    /// have rewritten (Vite's import analysis uses the id it got).
    fn resolved(
        &self,
        resolution: StartResolution,
        spec: &str,
        importer_id: &str,
        generation: u64,
    ) -> Result<Option<HostResolved>, String> {
        let importer_id = importer_id.to_string();
        match resolution {
            StartResolution::Module(id) if Path::new(&id).is_absolute() => {
                let (file, intent, svg_react) = split_intent(&id);
                let tagged = classify_id(&file, intent, svg_react);
                // A CommonJS file (a linked `file:` dependency, a .cjs
                // module) is Node's to load: byonm gives it require
                // semantics and lexed named exports, like the node
                // loader's fall-through did.
                if tagged == file && file == id && self.cjs.is_cjs_file(&file) {
                    if let Ok(u) = url::Url::from_file_path(&file) {
                        return Ok(Some(HostResolved::External(u.to_string())));
                    }
                    return Ok(Some(self.module_url(&tagged, importer_id, generation)));
                }
                // Classify second, as the dependency branch does: a style or
                // asset file (or an intent-carrying id) must reach the load
                // path tagged, or it falls into the JS compile and dies with
                // "unsupported file type". Plain sources classify to
                // themselves, so their graph identity is unchanged.
                Ok(Some(self.module_url(&tagged, importer_id, generation)))
            }
            // A plugin's module id, used as is.
            StartResolution::Module(id) => Ok(Some(self.module_url(&id, importer_id, generation))),
            StartResolution::Dependency(p) => {
                let (abs, intent, svg_react) = split_intent(&p.to_string_lossy());
                let ext = ext_of(&abs).to_ascii_lowercase();
                if svg_react
                    || intent.is_some()
                    || abs.contains('?')
                    || oj_server::is_style_ext(&ext)
                    || oj_server::is_importable_asset_ext(&ext)
                {
                    let id = classify_id(&abs, intent, svg_react);
                    return Ok(Some(self.module_url(&id, importer_id, generation)));
                }
                // A dependency the pipeline claims (`ssr.noExternal`, raw TS)
                // is a module of the graph like an app module: Vite's module
                // runner evaluates noExternal dependencies itself and clears
                // them with the app on every SSR reload (hmrHandler
                // full-reload: evaluatedModules.clear()). Versioned, with its
                // importer edges, an app edit re-evaluates the dependency
                // modules above it; TanStack's start-server-core caches the
                // app's router entry in module state (`entriesPromise`) and
                // would otherwise keep serving the first one.
                if self.claims_dependency(&abs) {
                    return Ok(Some(self.module_url(&abs, importer_id, generation)));
                }
                if let Some(node) = self.vite_external_instead(&importer_id, spec, &abs) {
                    return Ok(Some(node));
                }
                // Hand the engine the exact file the Vite-style resolver
                // picked (mainFields, extension probing, exports conditions,
                // legacy directory entries); byonm loads it with Node
                // semantics (CJS translation included).
                match url::Url::from_file_path(&abs) {
                    Ok(u) => Ok(Some(HostResolved::External(u.to_string()))),
                    Err(_) => Ok(Some(HostResolved::External(abs))),
                }
            }
            StartResolution::Bare(bare) => {
                // Unresolved, the name is Node's without the intent query;
                // a plugin's own answer (a remote URL) stands as it is.
                let bare = if bare == spec {
                    split_intent(spec).0
                } else {
                    bare
                };
                // A scheme Node's loader would reject (an un-aliased
                // `cloudflare:*`) becomes an empty module instead of taking
                // the server down; `node:` and bare npm names stay external.
                if let Ok(u) = url::Url::parse(&bare) {
                    if u.scheme().len() > 1 && u.scheme() != "node" {
                        return Ok(Some(HostResolved::Url(self.stub_specifier(&bare))));
                    }
                }
                Ok(Some(HostResolved::External(bare)))
            }
        }
    }

    async fn load_inner(
        &self,
        specifier: &str,
        generation: u64,
    ) -> Result<Option<HostModule>, String> {
        // Plain file loads (byonm-resolved) the host still claims: json as
        // ESM with named exports, and noExternal dependencies through the
        // transform pipeline (define/env/glob, plugin transforms).
        let Some(id) = versioned_id(specifier) else {
            return self.load_unversioned(specifier).await;
        };
        self.ensure_plugins_started().await;

        let module = self.load_id(&id).await?;
        self.graph.record_loaded_in_generation(&id, generation);
        Ok(Some(module))
    }

    /// A node_modules file the host loads through the transform pipeline
    /// instead of leaving it to the runtime.
    fn claims_dependency(&self, path: &str) -> bool {
        path.contains("/node_modules/")
            && (ts_source_ext(path) || self.no_external.matches_path(path))
            && src_lang(path).is_some()
            && !self.cjs.is_cjs_file(path)
    }

    async fn load_unversioned(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let Ok(u) = url::Url::parse(specifier) else {
            return Ok(None);
        };
        if u.scheme() != "file" {
            return Ok(None);
        }
        let Ok(path) = u.to_file_path() else {
            return Ok(None);
        };
        let path_str = path.to_string_lossy().into_owned();
        if !path_str.contains("/node_modules/") {
            return Ok(None);
        }
        if path_str.ends_with(".json") {
            // Vite's json plugin (namedExports) for dependency json too.
            let code = self
                .bridge
                .load_module(&path_str)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(Some(HostModule {
                code,
                module_type: HostModuleType::JavaScript,
            }));
        }
        // `ssr.noExternal` dependencies run through the pipeline (Vite
        // bundles them through its plugins); everything else is byonm's —
        // except TypeScript/JSX sources, which the runtime refuses to
        // type-strip under node_modules. Vite never externalizes those either
        // (canExternalizeFile allows only js/mjs/cjs), so they take the
        // pipeline unconditionally. A claimed file reached here was imported
        // natively by an unclaimed dependency (resolve_inner versions every
        // other claimed import), so it shares that importer's lifetime, as a
        // native import of a noExternal package does under Vite.
        if !self.claims_dependency(&path_str) {
            return Ok(None);
        }
        self.ensure_plugins_started().await;
        let source = std::fs::read_to_string(&path).map_err(|e| format!("read {path_str}: {e}"))?;
        let code = self
            .bridge
            .transform_module(&path_str, source, false, true)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        }))
    }

    async fn load_id(&self, id: &str) -> Result<HostModule, String> {
        let js = |code: String| HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        };
        if let Some(spec) = id.strip_prefix(STUB_PREFIX) {
            let _ = spec;
            return Ok(js("export default {};".into()));
        }

        let (clean, query) = match id.split_once('?') {
            Some((c, q)) => (c.to_string(), Some(q.to_string())),
            None => (id.to_string(), None),
        };
        let is_path = Path::new(&clean).is_absolute();

        // svgr: the plugin owns .svg modules (bare and ?react); without a
        // claiming plugin the module is its served URL.
        if is_path && clean.ends_with(".svg") {
            let loaded = match self.bridge.plugin_host().await {
                Some(host) if host.hook_wants_load(id) => host.load(id).await.ok().flatten(),
                _ => None,
            };
            let code = loaded.unwrap_or_else(|| {
                format!(
                    "export default {};",
                    json_str(&format!(
                        "{}/@oj-start/fs{clean}",
                        self.bridge.start_url_base()
                    ))
                )
            });
            return Ok(js(code));
        }

        if let Some(kind) = query.as_deref().and_then(|q| q.strip_prefix("ojasset=")) {
            let code = self.asset_module(&clean, kind).await?;
            return Ok(js(self.asset_transform(&clean, kind, code).await?));
        }

        // An asset under a query no intent names (vite-imagetools'
        // `x.webp?w=768`): the load hooks Vite runs before vite:asset see the
        // full id; unclaimed, it is the file's served URL, as vite:asset's.
        if is_path
            && query.is_some()
            && oj_server::is_importable_asset_ext(&ext_of(&clean).to_ascii_lowercase())
        {
            let loaded = match self.bridge.plugin_host().await {
                Some(host) if host.hook_wants_load(id) => {
                    host.load_pre(id).await.map_err(|e| format!("{id}: {e}"))?
                }
                _ => None,
            };
            return match loaded {
                Some(code) => self
                    .bridge
                    .transform_module(id, code, true, true)
                    .await
                    .map(js)
                    .map_err(|e| e.to_string()),
                None => {
                    // A named intent inlines/embeds per vite:asset (?raw the
                    // text, ?inline a data URI); only an unnamed query
                    // (imagetools `?w=...`) names the served URL. Fork parity:
                    // the old classifier named the kind, the new pipeline's
                    // resolve path keeps the written query.
                    let kind = match query.as_deref() {
                        Some("raw") => "raw",
                        Some("inline") => "inline",
                        _ => "url",
                    };
                    Ok(js(self.asset_module(&clean, kind).await?))
                }
            };
        }

        if !is_path {
            // A plugin virtual id: the pipeline's load() consults the plugin
            // host and compiles the result.
            return self
                .bridge
                .load_module(id)
                .await
                .map(js)
                .map_err(|e| e.to_string());
        }

        if clean.ends_with(".json") {
            return self
                .bridge
                .load_module(id)
                .await
                .map(js)
                .map_err(|e| e.to_string());
        }

        if clean.ends_with(".mdx") {
            let raw = std::fs::read_to_string(&clean).map_err(|e| format!("read {clean}: {e}"))?;
            let compiled = match self.bridge.plugin_host().await {
                Some(host) => host
                    .transform(&raw, id, "{}")
                    .await
                    .map(|t| t.code)
                    .map_err(|e| e.to_string())?,
                None => return Err(format!("no plugin compiles {clean}")),
            };
            // Compile the mdx output (jsx) with the pipeline's dev-ssr
            // options; `from_plugin` keeps the compile off the .mdx path.
            let code = self
                .bridge
                .transform_module(id, compiled, true, false)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(js(code));
        }

        // Source modules: a plugin load() overrides the on-disk file (Vite
        // runs load before the fs read); app sources get the server-fn
        // rewrite; the pipeline does plugin transforms + compile.
        let in_deps = clean.contains("/node_modules/");
        // Vite runs load before the fs read, but only plugins whose filter can
        // claim the id are consulted; the gate skips the per-module isolate RPC
        // for everything else (the whole server graph on the first render).
        // The full id first, as Vite's load sees it: a plugin's load may key
        // on its own query (`?tsr-split=...`); then the file.
        let plugin_loaded = match self.bridge.plugin_host().await {
            Some(host) if query.is_some() && host.hook_wants_load(id) => {
                match host.load(id).await.ok().flatten() {
                    Some(code) => Some(code),
                    None if host.hook_wants_load(&clean) => host.load(&clean).await.ok().flatten(),
                    None => None,
                }
            }
            Some(host) if host.hook_wants_load(&clean) => host.load(&clean).await.ok().flatten(),
            Some(_) => {
                if oj_server::plugins::hook_gate_debug() {
                    eprintln!("oj: hook gate skipped start load for {id}");
                }
                None
            }
            None => None,
        };
        let from_plugin = plugin_loaded.is_some();
        let raw = match plugin_loaded {
            Some(code) => code,
            None => match std::fs::read_to_string(&clean) {
                Ok(s) => s,
                Err(_) => {
                    // Not readable and no plugin claim: the pipeline's own
                    // fallback (virtual content, precise errors).
                    return self
                        .bridge
                        .load_module(id)
                        .await
                        .map(js)
                        .map_err(|e| e.to_string());
                }
            },
        };
        let raw = if !in_deps {
            let rel = clean
                .strip_prefix(&format!("{}/", self.root.to_string_lossy()))
                .unwrap_or(&clean)
                .to_string();
            // Fork: the Solid flavor keeps its own server-fn rewrite (the rpc
            // import follows the Solid Start package); React follows the
            // pipeline's rewrites.
            if self.framework == StartFramework::Solid {
                rewrite_server_fns(&raw, &rel, self.framework)
            } else {
                let raw = oj_compiler::env_fns::rewrite_env_fns(
                    &raw,
                    &clean,
                    oj_compiler::env_fns::Env::Server,
                )
                .map_err(|e| format!("{clean}: {e}"))?
                .unwrap_or(raw);
                oj_compiler::server_fns::rewrite_server(&raw, &rel).unwrap_or(raw)
            }
        } else {
            raw
        };
        // The plugins see the id with its query, as Vite's transforms do: a
        // route's `?tsr-split=component` is what TanStack's code splitter
        // compiles into the split-off component, from the route file's code.
        let code = self
            .bridge
            .transform_module(id, raw, from_plugin, true)
            .await
            .map_err(|e| e.to_string())?;
        Ok(js(code))
    }

    /// A file's URL on the dev pipeline, as Vite's `fileToUrl` in dev.
    fn dev_url(&self, path: &str) -> String {
        let base = self.bridge.start_url_base();
        match Path::new(path).strip_prefix(&self.root) {
            Ok(rel) => format!("{base}/{}", rel.to_string_lossy()),
            Err(_) => format!("{base}/@fs{path}"),
        }
    }

    /// The SSR environment's plugin transforms over an asset's module, under
    /// Vite's id for it (`/abs/x.css?url`): vite:asset's JS goes through the
    /// whole chain, as any module. A stylesheet's `?inline` and a CSS import
    /// ran their own slots already.
    async fn asset_transform(
        &self,
        path: &str,
        kind: &str,
        code: String,
    ) -> Result<String, String> {
        let style = oj_server::is_style_ext(&ext_of(path).to_ascii_lowercase());
        if !matches!(kind, "raw" | "url" | "asset") && !(kind == "inline" && !style) {
            return Ok(code);
        }
        // A plain asset import's id is the file's, as Vite's.
        let id = match kind {
            "asset" => path.to_string(),
            _ => format!("{path}?{kind}"),
        };
        match self.bridge.plugin_host().await {
            Some(host) if host.hook_wants_transform(&id, &code) => host
                .transform(&code, &id, "{}")
                .await
                .map(|t| t.code)
                .map_err(|e| format!("plugin transform error for {id}:\n{e}")),
            _ => Ok(code),
        }
    }

    async fn asset_module(&self, path: &str, kind: &str) -> Result<String, String> {
        match kind {
            "raw" => {
                let text =
                    std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
                Ok(format!("export default {};", json_str(&text)))
            }
            "inline" => {
                let ext = ext_of(path).to_ascii_lowercase();
                if oj_server::is_style_ext(&ext) {
                    // Vite's `?inline` sheet: compiled, as the client bundle
                    // inlines it.
                    self.bridge.start_dev_inline_style(Path::new(path)).await
                } else {
                    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
                    let url = format!(
                        "data:{};base64,{}",
                        mime_of(path),
                        crate::ssr_host::base64(&bytes)
                    );
                    Ok(format!("export default {};", json_str(&url)))
                }
            }
            "css" => {
                // The pipeline serves css modules with the client's exact
                // scoped names (oj_css) and plain css as an empty module.
                self.bridge
                    .load_module(path)
                    .await
                    .map_err(|e| e.to_string())
            }
            // Vite's unbundled worker load: the worker file's dev URL with
            // `worker_file&type=<type>` (its type `module`, or the one the
            // `new Worker` options gave), bare for `&url`, else wrapped in a
            // constructor. It is the URL the client pipeline serves the
            // compiled worker at, its env prelude included, so a rendered
            // value hydrates as is.
            k if k.starts_with("worker-url") || k.starts_with("sharedworker-url") => {
                let worker_type = k.split_once(':').map_or("module", |(_, t)| t);
                let url = format!("{}?worker_file&type={worker_type}", self.dev_url(path));
                Ok(format!("export default {};", json_str(&url)))
            }
            "worker" | "sharedworker" => {
                let ctor = if kind == "sharedworker" {
                    "SharedWorker"
                } else {
                    "Worker"
                };
                let url = format!("{}?worker_file&type=module", self.dev_url(path));
                Ok(format!(
                    "export default function WorkerWrapper(options) {{ return new {ctor}({}, {{ type: \"module\", name: options?.name }}); }}",
                    json_str(&url)
                ))
            }
            // "url" and anything unrecognized: the served-URL module.
            _ => Ok(format!(
                "export default {};",
                json_str(&format!(
                    "{}/@oj-start/fs{path}",
                    self.bridge.start_url_base()
                ))
            )),
        }
    }
}

/// Serves the engine's module-host requests by calling this host. One task
/// owns the recv loop; each request gets a task of its own because deno_core
/// polls module loads CONCURRENTLY while it builds a graph — handling them
/// inline would serialize every fetch behind the slowest transform. The loop
/// ends when the engine (the sender) is gone.
fn serve_module_host(host: Arc<StartHost>, mut requests: oj_js::HostRequests, generation: u64) {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let host = Arc::clone(&host);
            tokio::spawn(async move {
                match request {
                    oj_js::HostRequest::Resolve {
                        importer,
                        specifier,
                        reply,
                    } => {
                        let _ =
                            reply.send(host.resolve_inner(&importer, &specifier, generation).await);
                    }
                    oj_js::HostRequest::Load { specifier, reply } => {
                        let _ = reply.send(host.load_inner(&specifier, generation).await);
                    }
                }
            });
        }
    });
}

/// One request into the app's fetch handler, as the bootstrap consumes it.
#[derive(Debug)]
pub struct StartRequest {
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// The handler's reply: status/headers/set-cookies plus the whole body (the
/// stream is read to completion in the bootstrap, so deferred TanStack
/// content is present).
#[derive(Debug)]
pub struct StartResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub set_cookies: Vec<String>,
    pub body: Vec<u8>,
}

/// The Start runner handle `start_dev` calls per request. Wraps the engine so
/// a respawn (fresh framework caches after a reload) is transparent:
/// requests share it behind a read lock, the respawn takes the write lock.
pub struct StartEngine {
    host: Arc<StartHost>,
    root: PathBuf,
    entry_id: String,
    bootstrap: String,
    init_env: Vec<(String, String)>,
    engine: tokio::sync::RwLock<EngineSlot>,
}

/// A spawned engine plus its one-time bootstrap init (the env priming runs
/// once per isolate; a respawn gets a fresh slot and re-primes).
struct EngineSlot {
    engine: JsEngine,
    inited: tokio::sync::OnceCell<()>,
    reset: EngineReset,
}

/// Invalidation removes records even if spawning fails. Keep reset intent and
/// its error with the slot until a replacement actually succeeds.
#[derive(Default)]
struct EngineReset {
    pending: bool,
    error: Option<String>,
}

impl EngineReset {
    fn require(&mut self) {
        self.pending = true;
    }

    fn check_ready(&self) -> Result<(), String> {
        if self.pending {
            return Err(self
                .error
                .clone()
                .unwrap_or_else(|| "oj start: engine replacement pending".into()));
        }
        Ok(())
    }

    fn attempt<T>(&mut self, spawn: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        match spawn() {
            Ok(fresh) => {
                *self = Self::default();
                Ok(fresh)
            }
            Err(error) => {
                self.pending = true;
                let error = format!("oj start: engine respawn failed: {error}");
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }
}

impl EngineSlot {
    fn new(engine: JsEngine) -> EngineSlot {
        EngineSlot {
            engine,
            inited: tokio::sync::OnceCell::new(),
            reset: EngineReset::default(),
        }
    }
}

impl StartEngine {
    pub fn new(
        root: PathBuf,
        cache: &Path,
        bridge: SsrBridge,
        entry_abs: PathBuf,
        no_external: oj_config::SsrExternals,
        public_dir: Option<PathBuf>,
        init_env: Vec<(String, String)>,
        framework: StartFramework,
    ) -> anyhow::Result<StartEngine> {
        let bootstrap = cache.join("start-bootstrap.mjs");
        std::fs::write(&bootstrap, START_BOOTSTRAP_JS)?;
        let host = StartHost::new(
            root.clone(),
            cache,
            bridge,
            no_external,
            public_dir,
            framework,
        );
        let engine = spawn_engine(&root, &host, host.graph().generation())?;
        Ok(StartEngine {
            host,
            root,
            entry_id: entry_abs.to_string_lossy().into_owned(),
            bootstrap: bootstrap.to_string_lossy().into_owned(),
            init_env,
            engine: tokio::sync::RwLock::new(EngineSlot::new(engine)),
        })
    }

    async fn init(&self, engine: &JsEngine) -> Result<(), String> {
        let env: serde_json::Map<String, serde_json::Value> = self
            .init_env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        engine
            .call(
                self.bootstrap.clone(),
                "init",
                vec![serde_json::Value::Object(env)],
                None,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// One request through the handler; concurrent calls interleave on the
    /// isolate (the engine's call scheduler).
    pub async fn handle(&self, req: StartRequest) -> Result<StartResponse, String> {
        let slot = self.engine.read().await;
        slot.reset.check_ready()?;
        slot.inited
            .get_or_try_init(|| self.init(&slot.engine))
            .await?;
        let engine = &slot.engine;
        let entry = self.host.graph().specifier_for(&self.entry_id);
        let payload = serde_json::json!({
            "method": req.method,
            "url": req.url,
            "host": req.host,
            "headers": req.headers,
            "bodyBase64": req.body.map(|b| crate::ssr_host::base64(&b)),
        });
        let value = engine
            .call(
                self.bootstrap.clone(),
                "handle",
                vec![entry.into(), payload],
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let status = value.get("status").and_then(|s| s.as_u64()).unwrap_or(500) as u16;
        let pairs = |key: &str| -> Vec<(String, String)> {
            value
                .get(key)
                .and_then(|h| h.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            let k = p.get(0)?.as_str()?.to_string();
                            let v = p.get(1)?.as_str()?.to_string();
                            Some((k, v))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let set_cookies = value
            .get("setCookies")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let body = value
            .get("bodyBase64")
            .and_then(|b| b.as_str())
            .map(base64_decode)
            .unwrap_or_default();
        Ok(StartResponse {
            status,
            headers: pairs("headers"),
            set_cookies,
            body,
        })
    }

    /// The watcher's reload: force a fresh entry AND isolate. Start's external
    /// runtime caches its router entries promise, so versioning only the app
    /// modules cannot make it load a new router graph.
    pub async fn reload(&self) {
        let mut engine = self.engine.write().await;
        self.host.graph().invalidate();
        self.host.graph().bump(&self.entry_id);
        engine.reset.require();
        self.respawn(&mut engine);
    }

    /// The boot revalidation: re-check the world; reports whether anything
    /// changed (the caller reloads the browser then).
    pub async fn revalidate(&self) -> bool {
        let mut engine = self.engine.write().await;
        let dropped = self.host.graph().invalidate();
        if dropped > 0 {
            self.host.graph().bump(&self.entry_id);
            engine.reset.require();
        }
        let pending = engine.reset.pending;
        if pending {
            self.respawn(&mut engine);
        }
        dropped > 0 || pending
    }

    /// The write lock excludes handle calls. The graph's generation gate also
    /// excludes detached workers of the retired isolate after the reset.
    /// Fork: kept for the explicit respawn path.
    fn respawn(&self, engine: &mut EngineSlot) {
        let generation = self.host.graph().generation() + 1;
        match engine
            .reset
            .attempt(|| spawn_engine(&self.root, &self.host, generation).map_err(|e| e.to_string()))
        {
            Ok(fresh) => {
                *engine = EngineSlot::new(fresh);
                self.host.graph().reset_after_respawn();
                eprintln!("oj start: engine respawned for fresh server modules");
            }
            Err(e) => eprintln!("{e}"),
        }
    }

    /// A watch batch's revalidation: drops every stale record, as
    /// [`revalidate`](Self::revalidate) does, but reports only whether a
    /// changed file of the batch was one the engine served (the caller
    /// reloads the browser then). A record that went stale behind the
    /// watcher's back is still re-imported.
    pub async fn revalidate_for(&self, changed: &[PathBuf]) -> bool {
        let (dropped, hit) = self.host.graph().invalidate_for(changed);
        if dropped > 0 {
            self.host.graph().bump(&self.entry_id);
        }
        self.maybe_respawn().await;
        hit
    }

    async fn maybe_respawn(&self) {
        if !self.host.graph().should_respawn() {
            return;
        }
        let mut engine = self.engine.write().await;
        if !self.host.graph().should_respawn() {
            return; // another reload already respawned
        }
        // Fork: spawn_engine threads the module generation (3-arg kept).
        match spawn_engine(&self.root, &self.host, self.host.graph().generation()) {
            Ok(fresh) => {
                *engine = EngineSlot::new(fresh);
                self.host.graph().reset_after_respawn();
                eprintln!("oj start: engine respawned for fresh server modules");
            }
            Err(e) => eprintln!("{e}"),
        }
    }
}

/// The engine Start's codegen scripts (route-tree generation) and the build's
/// prerender run on: the app's packages load with plain byonm semantics, as
/// under node. Each script exports `run(env)`; the env values that used to be
/// spawn env travel as the argument.
pub struct ScriptEngine {
    engine: JsEngine,
    /// The env shadow ran on the isolate (once, before the first script).
    env_shadowed: tokio::sync::OnceCell<()>,
}

/// Shadows `process.env` with a private copy on the script isolate before any
/// script runs — the same isolation block plugin-host.mjs and the config
/// extractor install. The scripts' `run(env)` merges its values (route-tree
/// codegen gets NODE_ENV=development even under `oj build`) into
/// `process.env`, and Deno.env writes are PROCESS-REAL: unshadowed, that
/// NODE_ENV leaked into oj's own environment and could steer the vite-config
/// extraction that reads it afterwards (or races it, in dev).
const SCRIPT_ENV_SHADOW_JS: &str = r#"
Object.defineProperty(process, "env", {
  value: { ...process.env },
  configurable: true,
  writable: true,
});
"#;

impl ScriptEngine {
    pub fn new(root: &Path) -> anyhow::Result<ScriptEngine> {
        let mut config = EngineConfig::new(root);
        config.code_cache_dir = Some(oj_server::engine_code_cache_dir(root));
        Ok(ScriptEngine {
            engine: JsEngine::spawn(config, None, None).map_err(|e| anyhow::anyhow!("{e}"))?,
            env_shadowed: tokio::sync::OnceCell::new(),
        })
    }

    /// Runs `export async function run(env)` of `script` to completion.
    pub async fn run_async(
        &self,
        script: &Path,
        env: &[(String, String)],
        what: &str,
    ) -> anyhow::Result<()> {
        self.call(script, env)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{what} failed: {e}"))
    }

    async fn call(
        &self,
        script: &Path,
        env: &[(String, String)],
    ) -> Result<serde_json::Value, oj_js::EngineError> {
        self.env_shadowed
            .get_or_try_init(|| async {
                self.engine
                    .eval(
                        oj_js::EvalInput::Source(SCRIPT_ENV_SHADOW_JS.to_string()),
                        None,
                    )
                    .await
                    .map(|_| ())
            })
            .await?;
        let env_obj: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        self.engine
            .call(
                script.to_string_lossy().into_owned(),
                "run",
                vec![serde_json::Value::Object(env_obj)],
                None,
            )
            .await
    }
}

fn spawn_engine(
    root: &Path,
    host: &Arc<StartHost>,
    generation: u64,
) -> Result<JsEngine, oj_js::EngineError> {
    let mut config = EngineConfig::new(root);
    config.code_cache_dir = Some(oj_server::engine_code_cache_dir(root));
    config.registry = Some(host.bridge.engine_registry());
    let (link, requests) = ModuleHost::channel();
    serve_module_host(Arc::clone(host), requests, generation);
    JsEngine::spawn(config, Some(link), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_reset_failure_stays_pending_and_retries_without_new_invalidation() {
        let graph = VersionGraph::default();
        graph.record_loaded("virtual:entry");
        graph.bump("virtual:entry");
        let mut reset = EngineReset::default();
        reset.require();
        let failed = reset.attempt::<()>(|| Err("injected spawn failure".into()));
        assert!(failed.unwrap_err().contains("injected spawn failure"));
        assert!(reset.pending);
        assert!(reset
            .check_ready()
            .unwrap_err()
            .contains("injected spawn failure"));
        assert_eq!(graph.invalidate(), 0, "changed records are already gone");

        // Revalidation retries because intent survives, not because invalidate
        // found another change. No old-isolate successful response is allowed.
        if reset.pending {
            assert_eq!(
                reset.attempt(|| Ok("fresh isolate")).unwrap(),
                "fresh isolate"
            );
        }
        assert!(!reset.pending);
        assert!(reset.check_ready().is_ok());
    }

    #[test]
    fn start_retired_generation_cannot_record_importer_edges() {
        let graph = VersionGraph::default();
        let file =
            std::env::temp_dir().join(format!("oj-start-generation-{}.ts", std::process::id()));
        std::fs::write(&file, "export {};").unwrap();
        let id = file.to_string_lossy();
        let old = graph.generation();
        graph.reset_after_respawn();
        graph.record_loaded("virtual:old-importer");
        graph.record_loaded("virtual:new-importer");
        graph.edge_specifier_in_generation(&id, "virtual:old-importer".into(), old);
        graph.edge_specifier_in_generation(&id, "virtual:new-importer".into(), graph.generation());
        graph.record_loaded_in_generation(&id, graph.generation());
        std::fs::remove_file(&file).unwrap();
        assert_eq!(graph.invalidate(), 2);
        assert_eq!(graph.version_of("virtual:old-importer"), 0);
        assert_eq!(graph.version_of("virtual:new-importer"), 1);
    }

    // A custom server entry wrapping the framework default through the named
    // export (`import handler, { createServerEntry } from
    // "@tanstack/solid-start/server-entry"`, as a real app's src/server.ts
    // does): the Start host shadows that subpath with its runner, so the
    // shadow must provide the named export too. Before it did, every page
    // 500d with "does not provide an export named 'createServerEntry'".
    #[tokio::test(flavor = "multi_thread")]
    async fn custom_server_entry_may_wrap_the_default_through_create_server_entry() {
        let root =
            std::env::temp_dir().join(format!("oj-start-serverentry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join("start-cache");
        // The real runner stubs, as the dev server writes them.
        oj_server::write_start_assets(&cache).unwrap();
        // A minimal `@tanstack/solid-start` shaped like 1.169: the default
        // entry re-exports `createServerEntry` from the `server` subpath.
        let package = root.join("node_modules/@tanstack/solid-start");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@tanstack/solid-start","type":"module","exports":{".":"./index.js","./server":{"import":"./server.js"},"./server-entry":{"import":"./server-entry.js"}}}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("server.js"),
            "export function createStartHandler() {\n  return async () => new Response(\"<p>stub-marker</p>\");\n}\nexport function defaultStreamHandler() {}\nexport function createServerEntry(entry) {\n  return { async fetch(...args) {\n    const response = await entry.fetch(...args);\n    const headers = new Headers(response.headers);\n    headers.set(\"x-server-entry\", \"wrapped\");\n    return new Response(response.body, { status: response.status, statusText: response.statusText, headers });\n  } };\n}\n",
        )
        .unwrap();
        std::fs::write(
            package.join("server-entry.js"),
            "export { createServerEntry } from \"./server.js\";\n",
        )
        .unwrap();
        let entry = root.join("src/server.ts");
        std::fs::write(
            &entry,
            "import handler, { createServerEntry } from \"@tanstack/solid-start/server-entry\";\nexport default createServerEntry({\n  fetch(request) {\n    return handler.fetch(request);\n  },\n});\n",
        )
        .unwrap();
        let built = oj_server::DevServer {
            root: root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: true,
            lazy: true,
            typecheck: false,
            mode: None,
            rolldown: tokio::runtime::Handle::current(),
        }
        .build_app()
        .await
        .unwrap();
        let engine = StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            Default::default(),
            None,
            Vec::new(),
            StartFramework::Solid,
        )
        .unwrap();
        let response = engine
            .handle(StartRequest {
                method: "GET".into(),
                url: "/".into(),
                host: "localhost".into(),
                headers: Vec::new(),
                body: None,
            })
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(
            response
                .headers
                .contains(&("x-server-entry".into(), "wrapped".into())),
            "the named export did not wrap the response: {response:?}"
        );
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "<p>stub-marker</p>"
        );
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    // Stylesheets in the Start SSR graph are modules, not JS: a plain import
    // links the css pipeline's module, `?url` the served URL. Before the
    // resolver classified them, both fell into the JS compile and every page
    // 500d with "unsupported file type".
    #[tokio::test(flavor = "multi_thread")]
    async fn start_engine_serves_stylesheet_imports_as_modules() {
        let root = std::env::temp_dir().join(format!("oj-start-styles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join("start-cache");
        oj_server::write_start_assets(&cache).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        std::fs::write(root.join("src/empty.css"), ".x { color: red; }\n").unwrap();
        std::fs::write(root.join("src/url.css"), ".y { color: blue; }\n").unwrap();
        let entry = root.join("src/server.ts");
        std::fs::write(
            &entry,
            "import \"./empty.css\";\nimport urlCss from \"./url.css?url\";\nexport default {\n  async fetch(request: Request) {\n    if (new URL(request.url).pathname === \"/css-probe\") {\n      return new Response(urlCss);\n    }\n    return new Response(\"<p>css-marker</p>\");\n  },\n};\n",
        )
        .unwrap();
        let built = oj_server::DevServer {
            root: root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: true,
            lazy: true,
            typecheck: false,
            mode: None,
            rolldown: tokio::runtime::Handle::current(),
        }
        .build_app()
        .await
        .unwrap();
        let engine = StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            Default::default(),
            None,
            Vec::new(),
            StartFramework::Solid,
        )
        .unwrap();
        let request = |url: &str| StartRequest {
            method: "GET".into(),
            url: url.into(),
            host: "localhost".into(),
            headers: Vec::new(),
            body: None,
        };
        let response = engine.handle(request("/")).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "<p>css-marker</p>"
        );
        let probe = engine.handle(request("/css-probe")).await.unwrap();
        assert_eq!(probe.status, 200);
        assert!(
            String::from_utf8(probe.body).unwrap().contains("url.css"),
            "the ?url import did not serve the stylesheet URL"
        );
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    // Reproduce the external Start runtime's module-scope entriesPromise:
    // bumping the custom server entry alone must not leave its router cached.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_engine_refreshes_cached_external_router_on_reload_and_revalidate() {
        let root = std::env::temp_dir().join(format!("oj-start-freshness-{}", std::process::id()));
        let cache = root.join("start-cache");
        let package = root.join("node_modules/@tanstack/start-server-core");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@tanstack/start-server-core","type":"module","exports":{"./createStartHandler":"./handler.mjs"}}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("handler.mjs"),
            r#"let entriesPromise;
let requests = 0;
export function createStartHandler() {
  return async () => {
    const entries = await (entriesPromise ||= import('#tanstack-router-entry'));
    return new Response(entries.render(), { headers: { 'x-requests': String(++requests) } });
  };
}
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("src/router.ts"),
            "export { render } from './component.ts';",
        )
        .unwrap();
        let component = root.join("src/component.ts");
        let edit = |marker: &str, seconds: u64| {
            std::fs::write(
                &component,
                format!("export function render() {{ return '<p>{marker}</p>'; }}"),
            )
            .unwrap();
            std::fs::File::options()
                .write(true)
                .open(&component)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::now() + std::time::Duration::from_secs(seconds),
                )
                .unwrap();
        };
        edit("initial-marker", 0);
        let entry = root.join("src/server.ts");
        std::fs::write(
            &entry,
            r#"import { createStartHandler } from '@tanstack/start-server-core/createStartHandler';
const handler = createStartHandler();
export default { async fetch(req) {
  if (new URL(req.url).pathname === '/throw') throw new Error('uncaught-handler-marker');
  const response = await handler(req);
  return new Response('<main data-wrapper="custom">' + await response.text() + '</main>', {
    status: 201, headers: response.headers,
  });
} };
"#,
        )
        .unwrap();
        let built = oj_server::DevServer {
            root: root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: true,
            lazy: true,
            typecheck: false,
            mode: None,
            rolldown: tokio::runtime::Handle::current(),
        }
        .build_app()
        .await
        .unwrap();
        let engine = StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            Default::default(),
            None,
            Vec::new(),
            StartFramework::Solid,
        )
        .unwrap();
        // Hold startup so an actual old-isolate module load is in flight when
        // reload drops that engine. It must finish without restoring a record.
        let background = root.join("background.ts");
        std::fs::write(&background, "export const value = 1;").unwrap();
        let generation = engine.host.graph().generation();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let startup_host = Arc::clone(&engine.host);
        let startup = tokio::spawn(async move {
            startup_host
                .plugins_started
                .get_or_init(|| async {
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                })
                .await;
        });
        started_rx.await.unwrap();
        let load_host = Arc::clone(&engine.host);
        let background_specifier = engine
            .host
            .graph()
            .specifier_for(&background.to_string_lossy());
        let (loading_tx, loading_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let load = load_host.load_inner(&background_specifier, generation);
            tokio::pin!(load);
            assert!(futures_util::poll!(&mut load).is_pending());
            loading_tx.send(()).unwrap();
            load.await.unwrap().unwrap()
        });
        loading_rx.await.unwrap();
        engine.reload().await;
        release_tx.send(()).unwrap();
        startup.await.unwrap();
        worker.await.unwrap();
        std::fs::remove_file(&background).unwrap();
        assert_eq!(
            engine.host.graph().invalidate(),
            0,
            "retired background load repopulated graph"
        );
        let request = |url: &str| StartRequest {
            method: "GET".into(),
            url: url.into(),
            host: "localhost".into(),
            headers: Vec::new(),
            body: None,
        };
        let assert_response = |response: StartResponse, marker: &str, count: &str| {
            assert_eq!(response.status, 201);
            assert_eq!(
                String::from_utf8(response.body).unwrap(),
                format!("<main data-wrapper=\"custom\"><p>{marker}</p></main>")
            );
            assert!(response
                .headers
                .contains(&("x-requests".into(), count.into())));
        };
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "initial-marker",
            "1",
        );
        assert!(!engine.revalidate().await);
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "initial-marker",
            "2",
        );

        // Extra unchanged records must also be cleared when the isolate dies.
        let orphan = root.join("orphan.ts");
        std::fs::write(&orphan, "export {};").unwrap();
        engine.host.graph().record_loaded(&orphan.to_string_lossy());
        edit("reload-marker", 5);
        engine.reload().await;
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "reload-marker",
            "1",
        );
        assert!(engine.host.graph().version_of(&component.to_string_lossy()) > 0);
        std::fs::remove_file(&orphan).unwrap();
        assert_eq!(
            engine.host.graph().invalidate(),
            0,
            "old graph records survived"
        );
        assert!(!engine.revalidate().await);
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "reload-marker",
            "2",
        );

        edit("revalidated-marker", 10);
        assert!(engine.revalidate().await);
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "revalidated-marker",
            "1",
        );
        assert!(!engine.revalidate().await);
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "revalidated-marker",
            "2",
        );

        // Explicit reload also refreshes virtual/generated entries whose files
        // need not have changed; unchanged boot revalidation above does not.
        engine.reload().await;
        assert_response(
            engine.handle(request("/")).await.unwrap(),
            "revalidated-marker",
            "1",
        );
        assert!(engine
            .handle(request("/throw"))
            .await
            .unwrap_err()
            .contains("uncaught-handler-marker"));
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    // A plain module edit must reach the page WITHOUT a fresh isolate, while
    // the batches the watcher still fully reloads keep their contract. The
    // fake reproduces the external Start runtime's module-scope entries
    // cache; with the runtime seam versioned, the dropped chain reaches that
    // holder, the entry bump re-links it in place, and the generation — the
    // respawn counter — does not move until the explicit reload.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_engine_revalidates_plain_edits_without_respawn_and_reloads_on_demand() {
        let root = std::env::temp_dir().join(format!("oj-start-revalidate-{}", std::process::id()));
        let cache = root.join("start-cache");
        let package = root.join("node_modules/@tanstack/start-server-core");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@tanstack/start-server-core","type":"module","exports":{"./createStartHandler":"./handler.mjs"}}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("handler.mjs"),
            r#"let entriesPromise;
export function createStartHandler() {
  return async () => {
    const entries = await (entriesPromise ||= import('#tanstack-router-entry'));
    return new Response(entries.render());
  };
}
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("src/router.ts"),
            "export { render } from './component.ts';",
        )
        .unwrap();
        let component = root.join("src/component.ts");
        let edit = |marker: &str, seconds: u64| {
            std::fs::write(
                &component,
                format!("export function render() {{ return '<p>{marker}</p>'; }}"),
            )
            .unwrap();
            // Coarse filesystem clocks need a shove past the load-time mtime,
            // and later edits need strictly increasing offsets to seed.
            std::fs::File::options()
                .write(true)
                .open(&component)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::now() + std::time::Duration::from_secs(seconds),
                )
                .unwrap();
        };
        edit("initial-marker", 5);
        let entry = root.join("src/server.ts");
        std::fs::write(
            &entry,
            r#"import { createStartHandler } from '@tanstack/start-server-core/createStartHandler';
const handler = createStartHandler();
export default { async fetch(req) { return handler(req); } };
"#,
        )
        .unwrap();
        let built = oj_server::DevServer {
            root: root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: true,
            lazy: true,
            typecheck: false,
            mode: None,
            rolldown: tokio::runtime::Handle::current(),
        }
        .build_app()
        .await
        .unwrap();
        let engine = StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            Default::default(),
            None,
            Vec::new(),
            StartFramework::Solid,
        )
        .unwrap();
        let request = |url: &str| StartRequest {
            method: "GET".into(),
            url: url.into(),
            host: "localhost".into(),
            headers: Vec::new(),
            body: None,
        };
        let body = |response: StartResponse| String::from_utf8(response.body).unwrap();
        assert_eq!(
            body(engine.handle(request("/")).await.unwrap()),
            "<p>initial-marker</p>"
        );
        let generation = engine.host.graph().generation();
        let holder = package.join("handler.mjs").to_string_lossy().into_owned();

        // (a) The plain edit is picked up with the isolate untouched.
        edit("revalidated-marker", 60);
        assert!(
            engine
                .revalidate_for(std::slice::from_ref(&component))
                .await
        );
        assert_eq!(
            engine.host.graph().generation(),
            generation,
            "targeted revalidation must not respawn the isolate"
        );
        assert!(
            engine.host.graph().version_of(&holder) > 0,
            "the dropped chain reaches the runtime's cached entries holder"
        );
        assert_eq!(
            body(engine.handle(request("/")).await.unwrap()),
            "<p>revalidated-marker</p>"
        );

        // (b) The kept full-reload path still resets onto the new modules.
        edit("reload-marker", 120);
        engine.reload().await;
        assert!(
            engine.host.graph().generation() > generation,
            "an explicit reload still respawns the isolate"
        );
        assert_eq!(
            body(engine.handle(request("/")).await.unwrap()),
            "<p>reload-marker</p>"
        );
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn server_fn_rewrite_matches_the_node_loader() {
        let code = "import { createServerFn } from \"@tanstack/react-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"hi\");\n";
        let out = rewrite_server_fns(code, "src/server/data.ts", StartFramework::React);
        assert!(out
            .starts_with("import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; "));
        let id = base64url(b"src/server/data.ts#getGreeting");
        assert!(
            out.contains(&format!(
                "export const getGreeting_createServerFn_handler = createServerRpc({{ id: \"{id}\", name: \"getGreeting\", filename: \"src/server/data.ts\" }}, (opts) => getGreeting.__executeServer(opts)); "
            )),
            "{out}"
        );
        assert!(out.contains(".handler(getGreeting_createServerFn_handler, async () => \"hi\")"));
        // Line-preserving: the rewrite adds no lines.
        assert_eq!(code.lines().count(), out.lines().count());

        // Untouched code passes through byte-identical.
        let plain = "export const x = 1;\n";
        assert_eq!(
            rewrite_server_fns(plain, "src/x.ts", StartFramework::React),
            plain
        );
    }

    #[test]
    fn server_fn_rewrite_uses_the_solid_start_package_for_solid() {
        let code = "import { createServerFn } from \"@tanstack/solid-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"hi\");\n";
        let out = rewrite_server_fns(code, "src/server/data.ts", StartFramework::Solid);
        assert!(
            out.starts_with(
                "import { createServerRpc } from \"@tanstack/solid-start/server-rpc\"; "
            ),
            "{out}"
        );
        assert!(out.contains("getGreeting_createServerFn_handler"), "{out}");
        assert_eq!(code.lines().count(), out.lines().count());
    }

    // The mis-rewrite the lazy regex allowed: a factory assigned in one
    // statement and consumed by `.handler(...)` in another must not anchor
    // the rewrite to the factory. TanStack's compiler resolves this shape by
    // following variable bindings in the AST; the textual rewrite leaves it
    // byte-identical instead of guessing.
    #[test]
    fn split_statement_factories_are_left_untouched() {
        let split = "import { createServerFn } from \"@tanstack/react-start\";\nconst factory = createServerFn({ method: \"POST\" });\nexport const doIt = factory.handler(async () => 1);\n";
        assert_eq!(
            rewrite_server_fns(split, "src/split.ts", StartFramework::React),
            split
        );

        // Same, single line with a `;` between the statements.
        let inline = "const factory = createServerFn({ method: \"POST\" }); export const doIt = factory.handler(async () => 1);\n";
        assert_eq!(
            rewrite_server_fns(inline, "src/split.ts", StartFramework::React),
            inline
        );

        // A factory NEXT TO a chained server fn: the factory stays untouched,
        // the chained one still rewrites under its own name.
        let mixed = "const factory = createServerFn({ method: \"POST\" });\nexport const real = createServerFn({ method: \"GET\" }).handler(async () => 2);\nexport const viaFactory = factory.handler(async () => 3);\n";
        let out = rewrite_server_fns(mixed, "src/mixed.ts", StartFramework::React);
        assert!(out.contains("export const real_createServerFn_handler = createServerRpc("));
        assert!(out.contains(".handler(real_createServerFn_handler, async () => 2)"));
        assert!(!out.contains("factory_createServerFn_handler"), "{out}");
        assert!(
            out.contains(".handler(async () => 3)"),
            "the factory's handler call keeps its own arguments: {out}"
        );
    }

    // Two adjacent server fns each anchor to their own declaration.
    #[test]
    fn adjacent_server_fns_rewrite_independently() {
        let code = "export const a = createServerFn({ method: \"GET\" }).handler(async () => 1);\nexport const b = createServerFn({ method: \"POST\" }).handler(async () => 2);\n";
        let out = rewrite_server_fns(code, "src/two.ts", StartFramework::React);
        for name in ["a", "b"] {
            let id = base64url(format!("src/two.ts#{name}").as_bytes());
            assert!(
                out.contains(&format!(
                    "export const {name}_createServerFn_handler = createServerRpc({{ id: \"{id}\""
                )),
                "{out}"
            );
        }
        assert!(out.contains(".handler(a_createServerFn_handler, async () => 1)"));
        assert!(out.contains(".handler(b_createServerFn_handler, async () => 2)"));
        assert_eq!(code.lines().count(), out.lines().count());
    }

    // A multi-line .validator()/.middleware() chain is one expression: the
    // scan follows it across lines, comments and tricky argument content
    // (strings and regex literals holding unbalanced brackets).
    #[test]
    fn multiline_chains_keep_rewriting() {
        let code = r#"export const getUser = createServerFn({
  method: "GET",
})
  .middleware([authMiddleware]) // trailing comment
  .validator((s) => {
    if (!/^[(]/.test(s.name) && s.label !== ")((") return s;
    throw new Error(`bad ${s.name}; (unbalanced`);
  })
  .handler(async ({ data }) => data);
"#;
        let out = rewrite_server_fns(code, "src/user.ts", StartFramework::React);
        assert!(
            out.contains("export const getUser_createServerFn_handler = createServerRpc("),
            "{out}"
        );
        assert!(
            out.contains(".handler(getUser_createServerFn_handler, async ({ data }) => data)"),
            "{out}"
        );
        assert_eq!(code.lines().count(), out.lines().count());
    }

    // ASI variant of the split factory: no semicolon, the next statement
    // starts on its own line. The chain scan must not walk into it.
    #[test]
    fn asi_split_factories_are_left_untouched() {
        let code = "const factory = createServerFn({ method: \"POST\" })\nexport const doIt = factory.handler(async () => 1)\n";
        assert_eq!(
            rewrite_server_fns(code, "src/asi.ts", StartFramework::React),
            code
        );
    }

    #[test]
    fn base64url_matches_node_buffer() {
        assert_eq!(base64url(b"src/a.ts#fn"), "c3JjL2EudHMjZm4");
        assert_eq!(base64url(&[0xff, 0xef, 0x01]), "_-8B");
    }

    #[test]
    fn intent_queries_split_like_the_loader() {
        assert_eq!(
            split_intent("./logo.svg?react"),
            ("./logo.svg".into(), None, true)
        );
        assert_eq!(
            split_intent("../notes.txt?raw"),
            ("../notes.txt".into(), Some("raw"), false)
        );
        assert_eq!(
            split_intent("./hero.png?url"),
            ("./hero.png".into(), Some("url"), false)
        );
        assert_eq!(
            split_intent("./x.css?inline"),
            ("./x.css".into(), Some("inline"), false)
        );
        assert_eq!(split_intent("./x.tsx"), ("./x.tsx".into(), None, false));
        // ?react is only the svg component intent.
        assert_eq!(
            split_intent("./x.ts?react"),
            ("./x.ts?react".into(), None, false)
        );
    }

    // Vite's worker queries in any order (workerOrSharedWorkerRE + urlRE):
    // `&url` is the bare URL, `&inline` is ignored unbundled, and `?url` takes
    // `&no-inline` too. Before this, `?worker&url` loaded the worker as a plain
    // module and its importer failed with "does not provide an export named
    // 'default'".
    #[test]
    fn worker_queries_split_like_vite() {
        for (spec, tag) in [
            ("./w.ts?worker&url", "worker-url"),
            ("./w.ts?url&worker", "worker-url"),
            ("./w.ts?worker", "worker"),
            ("./w.ts?worker&inline", "worker"),
            ("./w.ts?sharedworker", "sharedworker"),
            ("./w.ts?sharedworker&url", "sharedworker-url"),
            // oj's rewrite of `new Worker(new URL(...), opts)` carries the type.
            ("./w.ts?worker&url&worker_type=module", "worker-url"),
            (
                "./w.ts?worker&url&worker_type=classic",
                "worker-url:classic",
            ),
            (
                "./w.ts?sharedworker&url&worker_type=ignore",
                "sharedworker-url:ignore",
            ),
            // A user `type=` is not the worker's type.
            ("./w.ts?worker&url&type=classic", "worker-url"),
        ] {
            assert_eq!(
                split_intent(spec),
                ("./w.ts".into(), Some(tag), false),
                "{spec}"
            );
        }
        assert_eq!(
            split_intent("./hero.png?no-inline"),
            ("./hero.png".into(), Some("url"), false)
        );
        // Vite's combinable asset grammar: no-inline always wins, inline
        // beats url, and dev serves `worker&inline&url` as the URL export
        // (inline is a bundled-only concern).
        assert_eq!(
            split_intent("./hero.png?url&no-inline"),
            ("./hero.png".into(), Some("url"), false)
        );
        assert_eq!(
            split_intent("./hero.png?inline&no-inline"),
            ("./hero.png".into(), Some("url"), false)
        );
        assert_eq!(
            split_intent("./hero.png?url&inline"),
            ("./hero.png".into(), Some("inline"), false)
        );
        assert_eq!(
            split_intent("./w.ts?worker&inline&url"),
            ("./w.ts".into(), Some("worker-url"), false)
        );
        // A query oj does not own passes through untouched.
        assert_eq!(
            split_intent("./r.tsx?tsr-split=component"),
            ("./r.tsx?tsr-split=component".into(), None, false)
        );
    }

    #[test]
    fn only_javascript_dependency_files_stay_external() {
        for js in [
            "/n/node_modules/p/i.js",
            "/n/node_modules/p/i.mjs",
            "/n/node_modules/p/i.cjs",
            "/n/node_modules/p/bin",
        ] {
            assert!(can_externalize_file(js), "{js}");
        }
        for other in [
            "/n/node_modules/p/select.css",
            "/n/node_modules/p/x.module.scss",
            "/n/node_modules/p/data.json",
            "/n/node_modules/p/index.ts",
            "/n/node_modules/p/icon.tsx",
            "/n/node_modules/p/x.css?inline",
        ] {
            assert!(!can_externalize_file(other), "{other}");
        }
    }

    #[test]
    fn classification_tags_assets_styles_and_sources() {
        assert_eq!(classify_id("/a/b.css", None, false), "/a/b.css?ojasset=css");
        assert_eq!(
            classify_id("/a/b.module.scss", None, false),
            "/a/b.module.scss?ojasset=css"
        );
        assert_eq!(
            classify_id("/a/hero.png", None, false),
            "/a/hero.png?ojasset=asset"
        );
        assert_eq!(
            classify_id("/a/n.txt", Some("raw"), false),
            "/a/n.txt?ojasset=raw"
        );
        assert_eq!(classify_id("/a/l.svg", None, true), "/a/l.svg?react");
        assert_eq!(classify_id("/a/l.svg", None, false), "/a/l.svg");
        assert_eq!(classify_id("/a/x.tsx", None, false), "/a/x.tsx");
        assert_eq!(
            classify_id("/a/x.tsx?tsr-split=component", None, false),
            "/a/x.tsx?tsr-split=component"
        );
        assert_eq!(src_lang("/a/x.tsx?tsr-split=component"), Some("tsx"));
        assert_eq!(
            classify_id("/a/x.css?plugin-query", None, false),
            "/a/x.css?ojasset=css"
        );
        assert_eq!(
            classify_id("/a/m.wasm", None, false),
            "/a/m.wasm?ojasset=asset"
        );
    }

    // A split route is a distinct plugin module, not the reference file on
    // disk. Both hook filters and handlers must see the entire queried id.
    #[tokio::test(flavor = "multi_thread")]
    async fn queried_sources_keep_plugin_identity_and_clean_compile_paths() {
        let root = std::env::temp_dir().join(format!("oj-start-query-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("oj.plugins.mjs"),
            r#"export default [{
  name: "queried-source",
  load: {
    filter: { id: /\.tsx\?tsr-split=component&opaque=%2F$/ },
    handler(id) {
      return 'export const component: string = "split-component"; export const loadedId = ' + JSON.stringify(id) + ';';
    },
  },
  transform: {
    filter: { id: /\.(tsx|ts)\?tsr-split=component&opaque=%2F$/ },
    handler(code, id) {
      return code + '\nexport const transformedId: string = ' + JSON.stringify(id) + ';';
    },
  },
}];
"#,
        )
        .unwrap();
        std::fs::write(root.join("route.tsx"), "export const Route = 'reference';").unwrap();
        std::fs::write(
            root.join("ordinary.ts"),
            "export const ordinary: number = 42;",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("node_modules/query-source")).unwrap();
        std::fs::write(
            root.join("node_modules/query-source/index.ts"),
            "export const dependency: number = 43;",
        )
        .unwrap();
        let built = oj_server::DevServer {
            root: root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: true,
            lazy: true,
            typecheck: false,
            mode: None,
            rolldown: tokio::runtime::Handle::current(),
        }
        .build_app()
        .await
        .unwrap();
        let host = StartHost::new(
            root.clone(),
            &root.join("start-cache"),
            built.ssr.clone(),
            Default::default(),
            None,
            StartFramework::Solid,
        );
        for (name, marker) in [
            ("route.tsx", "split-component"),
            ("ordinary.ts", "ordinary"),
            ("node_modules/query-source/index.ts", "dependency"),
        ] {
            let id = format!("{}/{name}?tsr-split=component&opaque=%2F", root.display());
            let spec = format!("./{name}?tsr-split=component&opaque=%2F");
            let Some(HostResolved::Url(specifier)) = host
                .resolve_inner(
                    &root.join("entry.ts").to_string_lossy(),
                    &spec,
                    host.graph().generation(),
                )
                .await
                .unwrap()
            else {
                panic!("queried source must stay in the plugin module graph");
            };
            assert_eq!(versioned_id(&specifier).as_deref(), Some(id.as_str()));
            let module = host
                .load_inner(&specifier, host.graph().generation())
                .await
                .unwrap()
                .unwrap();
            let code = module.code.split("//# sourceMappingURL=").next().unwrap();
            assert!(code.contains(marker), "{code}");
            assert!(code.contains("transformedId"), "{code}");
            assert!(code.contains(&id), "opaque query changed: {code}");
            assert!(
                !code.contains(": string") && !code.contains(": number"),
                "{code}"
            );
            if name == "route.tsx" {
                assert!(code.contains("loadedId"), "{code}");
                assert!(
                    !code.contains("reference") && !code.contains("Route"),
                    "{code}"
                );
            }
        }
        let reference = host
            .load_id(&root.join("route.tsx").to_string_lossy())
            .await
            .unwrap();
        assert!(reference.code.contains("Route") && reference.code.contains("reference"));
        assert!(!reference.code.contains("loadedId") && !reference.code.contains("transformedId"));
        host.bridge.plugin_host().await.unwrap().shutdown();
        if let Some(client) = built.plugin_host {
            client.shutdown();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_external_matches_names_globs_and_regexes() {
        let rules = oj_config::SsrExternals {
            no_external: vec!["left-pad".into(), "@acme/*".into()],
            no_external_regex: vec!["^weird-".into()],
            external: vec!["@acme/keep-out".into()],
            ..Default::default()
        };
        let m = NoExternal::new(rules);
        assert!(m.matches_path("/x/node_modules/left-pad/index.js"));
        assert!(m.matches_path("/x/node_modules/@acme/ui/dist/i.js"));
        assert!(m.matches_path("/x/node_modules/weird-lib/i.js"));
        assert!(!m.matches_path("/x/node_modules/@acme/keep-out/i.js"));
        assert!(!m.matches_path("/x/node_modules/react/index.js"));
        assert!(!m.matches_path("/x/src/app.tsx"));
    }

    #[test]
    fn cjs_detection_matches_the_loader() {
        let dir = std::env::temp_dir().join(format!("oj-cjsdet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("esm")).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("esm/package.json"), r#"{"type":"module"}"#).unwrap();
        let cache = CjsCache::default();
        let cjs = dir.join("plain.js");
        std::fs::write(&cjs, "module.exports = { a: 1 };\n").unwrap();
        let esmish = dir.join("esmish.js");
        std::fs::write(&esmish, "export const a = 1;\n").unwrap();
        let typed = dir.join("esm/x.js");
        std::fs::write(&typed, "module.exports = 1;\n").unwrap();
        assert!(cache.is_cjs_file(&cjs.to_string_lossy()));
        assert!(!cache.is_cjs_file(&esmish.to_string_lossy()));
        assert!(
            !cache.is_cjs_file(&typed.to_string_lossy()),
            "type:module wins"
        );
        assert!(cache.is_cjs_file("/whatever/x.cjs"));
        assert!(!cache.is_cjs_file("/whatever/x.mjs"));
        // Memoized: a second ask answers from the caches.
        assert!(cache.is_cjs_file(&cjs.to_string_lossy()));
        assert!(!cache.is_cjs_file(&typed.to_string_lossy()));
    }

    // The ESM-syntax verdict is keyed on the file's mtime: an edited file
    // (a linked `file:` dependency rewritten mid-session) is re-read, while
    // an untouched file answers from the cache.
    #[test]
    fn cjs_verdict_cache_follows_the_file_mtime() {
        let dir = std::env::temp_dir().join(format!("oj-cjsmtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let cache = CjsCache::default();
        let file = dir.join("mut.js");
        std::fs::write(&file, "module.exports = 1;\n").unwrap();
        assert!(cache.is_cjs_file(&file.to_string_lossy()));
        // Rewrite as ESM and push the mtime forward explicitly (same-second
        // writes can share an mtime on coarse filesystems).
        std::fs::write(&file, "export const a = 1;\n").unwrap();
        let f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            !cache.is_cjs_file(&file.to_string_lossy()),
            "an mtime move re-reads the file"
        );
    }

    #[test]
    fn a_module_build_needs_a_bundler_when_it_calls_an_undeclared_require() {
        // turndown's `module` build, and a call at the top level.
        assert!(calls_undeclared_require(
            "function f() {\n  var d = require('domino');\n  return d;\n}\nexport default f();\n"
        ));
        assert!(calls_undeclared_require(
            "const x = require('./x.cjs');\nexport { x };\n"
        ));
        // A guarded call still needs one; Node's entry is Vite's pick anyway.
        assert!(calls_undeclared_require(
            "export const d = typeof require === 'function' ? require('d') : null;\n"
        ));
        // Its own require: createRequire, a parameter, a function.
        assert!(!calls_undeclared_require(
            "import { createRequire } from 'node:module';\nconst require = createRequire(import.meta.url);\nexport const x = require('./x.cjs');\n"
        ));
        assert!(!calls_undeclared_require(
            "export function load(require) { return require('x'); }\n"
        ));
        assert!(!calls_undeclared_require(
            "function require(id) { return id; }\nexport const x = require('x');\n"
        ));
        // A declaration in one scope leaves another's call undeclared: a
        // wrapper's `require` parameter beside a top-level call.
        assert!(calls_undeclared_require(
            "function wrap(exports, require) { return require('a'); }\nconst d = require('d');\nexport { d, wrap };\n"
        ));
        // esbuild's shim, through `require.apply`; a member call; a sequence.
        assert!(calls_undeclared_require(
            "var __require = (x => typeof require !== 'undefined' ? require : x)(function (x) {\n  if (typeof require !== 'undefined') return require.apply(this, arguments);\n  throw Error('Dynamic require of \"' + x + '\" is not supported');\n});\nvar d = __require('d');\nexport { d };\n"
        ));
        assert!(calls_undeclared_require(
            "export const p = require.resolve('x');\n"
        ));
        assert!(calls_undeclared_require(
            "export const x = (0, require)('x');\n"
        ));
        assert!(calls_undeclared_require(
            "export const x = require?.('x');\n"
        ));
        // No call: a mention, a member, a property, a string.
        assert!(!calls_undeclared_require("export const x = 1;\n"));
        assert!(!calls_undeclared_require(
            "export const required = true;\nexport function f() { return required; }\n"
        ));
        assert!(!calls_undeclared_require(
            "export const r = typeof require;\nexport const y = module.require('y');\nexport const s = 'require(x)';\nexport const o = { require: 1 };\n"
        ));
        // Unparseable code is left as it was.
        assert!(!calls_undeclared_require("export const = require('x'"));
    }

    #[test]
    fn a_bare_specifier_names_its_package() {
        assert_eq!(bare_package("turndown"), Some("turndown"));
        assert_eq!(bare_package("turndown/lib/x.js"), Some("turndown"));
        assert_eq!(bare_package("@scope/pkg"), Some("@scope/pkg"));
        assert_eq!(bare_package("@scope/pkg/sub"), Some("@scope/pkg"));
        for spec in [
            "./x",
            "../x",
            "/abs/x",
            "#lib/x",
            "node:fs",
            "fs",
            "virtual:x",
        ] {
            assert_eq!(bare_package(spec), None, "{spec}");
        }
    }

    #[test]
    fn only_an_es_module_is_scanned_for_require() {
        let dir = std::env::temp_dir().join(format!("oj-esmreq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let cache = CjsCache::default();
        let esm = dir.join("index.es.js");
        std::fs::write(&esm, "var d = require('d');\nexport default d;\n").unwrap();
        let cjs = dir.join("index.cjs.js");
        std::fs::write(&cjs, "module.exports = require('d');\n").unwrap();
        let mjs = dir.join("index.mjs");
        std::fs::write(&mjs, "export const d = require('d');\n").unwrap();
        assert!(cache.esm_calls_require(&esm.to_string_lossy()));
        assert!(cache.esm_calls_require(&mjs.to_string_lossy()));
        assert!(
            !cache.esm_calls_require(&cjs.to_string_lossy()),
            "CommonJS has require"
        );
        assert!(!cache.esm_calls_require("/whatever/x.cjs"));
        // Keyed on the mtime: a reinstalled build is scanned again.
        std::fs::write(&esm, "export default 1;\n").unwrap();
        let f = std::fs::OpenOptions::new().append(true).open(&esm).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert!(!cache.esm_calls_require(&esm.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The framework seam, from the consumer's side: start-server-core imports
    // these by bare specifier and expects the bundler to answer; one the host
    // does not map fails every document request. (The old node loader had the
    // same table; write_start_assets covers the targets existing.)
    #[test]
    fn the_start_host_maps_every_framework_virtual_module() {
        let root = Path::new("/app");
        let cache = Path::new("/app/.oj-cache/v1/start");
        let aliases = framework_aliases(root, cache, StartFramework::React);
        for spec in [
            "tanstack-start-manifest:v",
            "tanstack-start-injected-head-scripts:v",
            "#tanstack-router-entry",
            "#tanstack-start-entry",
            "#tanstack-start-plugin-adapters",
            "#tanstack-start-server-fn-resolver",
            "@cloudflare/vite-plugin/server",
            "cloudflare:workers",
            "@tanstack/react-start/server-entry",
        ] {
            assert!(
                aliases.iter().any(|(find, _)| find == spec),
                "the start host has no alias for {spec}"
            );
        }
    }

    #[test]
    fn the_start_host_aliases_the_solid_server_entry_for_solid() {
        let root = Path::new("/app");
        let cache = Path::new("/app/.oj-cache/v1/start");
        let aliases = framework_aliases(root, cache, StartFramework::Solid);
        assert!(
            aliases
                .iter()
                .any(|(find, _)| find == "@tanstack/solid-start/server-entry"),
            "no solid server-entry alias"
        );
        assert!(
            !aliases
                .iter()
                .any(|(find, _)| find == "@tanstack/react-start/server-entry"),
            "the react entry must not leak into solid apps"
        );
        assert_eq!(
            aliases
                .iter()
                .find(|(find, _)| find == "@tanstack/solid-start/server-entry")
                .map(|(_, target)| target.as_path()),
            Some(cache.join("server-entry-solid.tsx").as_path())
        );
    }

    // Upstream resolves `#tanstack-start-entry` to the app's `src/start.*`
    // before its bundled default (planning.ts: `startFilePath ??
    // defaultEntryPaths.start`); the stub stands in only for the default.
    // Falling to the stub while a real src/start.ts exists silently drops the
    // app's createStart middleware: request middleware server-side, and
    // function middleware on every server-fn call (an app attaching auth
    // there gets Unauthorized from every authed server fn).
    #[test]
    fn the_start_entry_alias_prefers_the_apps_src_start() {
        let root = std::env::temp_dir().join(format!("oj-startentry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let cache = root.join(".oj-cache/v1/start");

        let stub = start_entry_target(&root, &cache);
        assert_eq!(
            stub,
            cache.join("start-entry.ts"),
            "no src/start.* means the framework-default stub"
        );

        std::fs::write(
            root.join("src/start.ts"),
            "export const startInstance = 1;\n",
        )
        .unwrap();
        assert_eq!(
            start_entry_target(&root, &cache),
            root.join("src/start.ts"),
            "an existing src/start.* wins"
        );
    }

    // Upstream's ladder, not probe(): .js outranks .tsx, and a src/start/
    // directory index is not a start entry (upstream serves its default).
    #[test]
    fn start_file_follows_the_framework_ladder() {
        let root = std::env::temp_dir().join(format!("oj-startladder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/start")).unwrap();
        std::fs::write(root.join("src/start/index.ts"), "").unwrap();
        assert_eq!(start_file(&root), None, "a directory index is not an entry");

        std::fs::write(root.join("src/start.tsx"), "").unwrap();
        assert_eq!(start_file(&root), Some(root.join("src/start.tsx")));
        std::fs::write(root.join("src/start.js"), "").unwrap();
        assert_eq!(
            start_file(&root),
            Some(root.join("src/start.js")),
            ".js outranks .tsx"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // app_start_entry is the presence probe the dev watcher restarts on: it
    // answers which file the start entry resolves to right now, `None` for
    // the stub. A package.json `imports` target outranks the src/start
    // convention, matching resolve_inner and the client bundle's probe.
    #[test]
    fn app_start_entry_tracks_presence_and_imports() {
        let root = std::env::temp_dir().join(format!("oj-startpresence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();

        assert_eq!(app_start_entry(&root), None, "no entry means the stub");

        std::fs::write(
            root.join("src/start.ts"),
            "export const startInstance = 1;\n",
        )
        .unwrap();
        assert_eq!(
            app_start_entry(&root),
            Some(root.join("src/start.ts")),
            "a created src/start.ts flips the answer"
        );

        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("app/entry.ts"),
            "export const startInstance = 2;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r##"{ "imports": { "#tanstack-start-entry": "./app/entry.ts" } }"##,
        )
        .unwrap();
        assert_eq!(
            app_start_entry(&root),
            Some(root.join("./app/entry.ts")),
            "a declared imports target outranks the convention"
        );

        std::fs::remove_file(root.join("package.json")).unwrap();
        std::fs::remove_file(root.join("src/start.ts")).unwrap();
        assert_eq!(
            app_start_entry(&root),
            None,
            "a delete flips back to the stub"
        );
    }

    // The ScriptEngine's process.env shadow: a script's env writes — the
    // run(env) merge and its own assignments (route-tree codegen's
    // NODE_ENV=development) — stay on the isolate's private copy and never
    // reach oj's real environment, exactly like the plugin host's and config
    // extractor's isolation (plugins::config_env_writes_do_not_leak...).
    #[tokio::test(flavor = "multi_thread")]
    async fn script_engine_env_writes_do_not_leak_into_the_oj_process() {
        let dir = std::env::temp_dir().join(format!("oj-scriptenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("probe.mjs");
        std::fs::write(
            &script,
            r#"export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  // The shadow must still BEHAVE like the env: the merged value is visible.
  if (process.env.OJ_SCRIPT_MERGE_PROBE !== "merged") {
    throw new Error("merged env not visible on the shadow");
  }
  process.env.NODE_ENV = "development";
  process.env.OJ_SCRIPT_WRITE_PROBE = "leaked";
  if (process.env.OJ_SCRIPT_WRITE_PROBE !== "leaked") {
    throw new Error("own write not visible on the shadow");
  }
  return null;
}
"#,
        )
        .unwrap();
        let scripts = ScriptEngine::new(&dir).unwrap();
        let node_env_before = std::env::var("NODE_ENV").ok();
        scripts
            .run_async(
                &script,
                &[("OJ_SCRIPT_MERGE_PROBE".to_string(), "merged".to_string())],
                "env shadow probe",
            )
            .await
            .expect("the probe script runs");
        assert!(
            std::env::var("OJ_SCRIPT_MERGE_PROBE").is_err(),
            "the run(env) merge must stay on the isolate's shadow"
        );
        assert!(
            std::env::var("OJ_SCRIPT_WRITE_PROBE").is_err(),
            "a script's own env write must stay on the isolate's shadow"
        );
        assert_eq!(
            std::env::var("NODE_ENV").ok(),
            node_env_before,
            "the script's NODE_ENV write must not alter oj's real env"
        );
    }

    // The Start runner's bootstrap primes process.env (init's env, the TSS
    // defaults) on its own shadow too: a leak would ride a restart's exec into
    // the next boot as shell env, re-keying the client bundle cache and
    // outranking edited .env files.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_runner_env_priming_does_not_leak_into_the_oj_process() {
        let dir = std::env::temp_dir().join(format!("oj-runnerenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bootstrap = dir.join("start-bootstrap.mjs");
        std::fs::write(&bootstrap, START_BOOTSTRAP_JS).unwrap();
        let probe = dir.join("probe.mjs");
        std::fs::write(
            &probe,
            "export function read() { return process.env.OJ_RUNNER_ENV_PROBE ?? null; }\n",
        )
        .unwrap();
        let engine = JsEngine::spawn(EngineConfig::new(&dir), None, None).unwrap();
        let fn_base_before = std::env::var("TSS_SERVER_FN_BASE").ok();
        engine
            .call(
                bootstrap.to_string_lossy().into_owned(),
                "init",
                vec![serde_json::json!({ "OJ_RUNNER_ENV_PROBE": "primed" })],
                None,
            )
            .await
            .expect("the bootstrap init runs");
        let seen = engine
            .call(probe.to_string_lossy().into_owned(), "read", vec![], None)
            .await
            .unwrap();
        assert_eq!(
            seen,
            serde_json::json!("primed"),
            "the runner sees its primed env"
        );
        assert!(
            std::env::var("OJ_RUNNER_ENV_PROBE").is_err(),
            "init's priming must stay on the isolate's shadow"
        );
        assert_eq!(
            std::env::var("TSS_SERVER_FN_BASE").ok(),
            fn_base_before,
            "the TSS defaults must not alter oj's real env"
        );
    }

    #[test]
    fn pkg_names_parse_scoped_and_plain() {
        assert_eq!(
            pkg_name_of_path("/a/node_modules/react/index.js").as_deref(),
            Some("react")
        );
        assert_eq!(
            pkg_name_of_path("/a/node_modules/@acme/ui/dist/x.js").as_deref(),
            Some("@acme/ui")
        );
        assert_eq!(pkg_name_of_path("/a/src/x.ts"), None);
    }
}
