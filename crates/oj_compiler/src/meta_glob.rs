// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's `import.meta.glob` expansion and `new URL(..., import.meta.url)`
//! rewrites as source-text edits, for modules on their way through a bundler's
//! transform hooks (Vite's vite:import-glob, vite:worker-import-meta-url and
//! vite:asset-import-meta-url). Every edit keeps the module's line count: the
//! imports they hoist share the first line and a call that spanned several
//! lines keeps its line breaks, so the source map the bundler builds for the
//! module still points at the right lines.
//!
//! [`crate::glob`] holds the matching rules (Vite's picomatch dialect and its
//! tinyglobby walk), shared with oj's dev server.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{ArrayExpressionElement, Expression, ObjectPropertyKind, PropertyKey};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::glob::{relative_import, to_absolute_glob, GlobAlias, GlobCall};

/// A glob call that cannot be expanded as written, reported as Vite reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobError(pub String);

impl std::fmt::Display for GlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GlobError {}

/// Expands every `import.meta.glob(...)` call of the module at `file` into
/// the object Vite writes for it. `root` is the project root (`/` patterns
/// and `/`-led keys), `alias` the app's `resolve.alias`. Each call's resolved patterns are pushed to `scopes`,
/// what a dev rebundle gate matches created files against. Calls whose
/// arguments are not static values are left as written.
pub fn transform_glob(
    code: &str,
    file: &Path,
    root: &Path,
    alias: GlobAlias,
    scopes: &mut Vec<GlobCall>,
) -> Result<String, GlobError> {
    const HEAD: &str = "import.meta.glob";
    if !code.contains(HEAD) {
        return Ok(code.to_string());
    }
    // Call sites are found on the literal-stripped code, so a call spelled
    // inside a string or a comment is none (Vite finds them in the AST).
    let clean = strip_literals(code);
    let file_dir = file.parent().unwrap_or(file);
    let mut prelude: Vec<String> = Vec::new();
    let mut out = String::with_capacity(code.len());
    let mut last = 0;
    let mut calls = 0usize;
    let mut from = 0;
    while let Some(found) = code[from..].find(HEAD) {
        let head = from + found;
        let after = head + HEAD.len();
        from = after;
        if clean.as_bytes()[head..after] != *HEAD.as_bytes() {
            continue;
        }
        // `\b`: `import.meta.globEager` is no call of this.
        if code[after..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            continue;
        }
        let Some(args_start) = call_args_start(code, after) else {
            continue;
        };
        let Some(args_end) = matching_paren(code, args_start) else {
            continue;
        };
        let Some(args) = static_args(&code[args_start..args_end]) else {
            continue;
        };
        from = args_end + 1;
        let call_end = args_end + 1;

        let patterns: Vec<String> = match args.first() {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            Some(Value::Str(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        let opts = match args.get(1) {
            Some(Value::Object(o)) => o.clone(),
            _ => Vec::new(),
        };
        let opts = GlobOpts::normalize(&opts)
            .map_err(|e| GlobError(format!("{e} (import.meta.glob in {})", file.display())))?;

        let is_relative = patterns
            .iter()
            .all(|p| p.starts_with('.') || p.starts_with('!'));
        let resolved: Vec<String> = patterns
            .iter()
            .map(|p| to_absolute_glob(p, file_dir, root, opts.base.as_deref(), alias))
            .collect();
        let call = GlobCall {
            patterns: resolved,
            exhaustive: opts.exhaustive,
            case_sensitive: opts.case_sensitive,
        };
        let files: Vec<PathBuf> = call.files().into_iter().filter(|f| f != file).collect();
        scopes.push(call);

        let import_name = opts.import.as_deref().filter(|i| *i != "*");
        let want_default = import_name == Some("default");
        let mut entries: Vec<String> = Vec::with_capacity(files.len());
        for (idx, f) in files.iter().enumerate() {
            let spec = format!("{}{}", relative_import(file_dir, f), opts.query);
            let key = json(&key_of(
                f,
                file_dir,
                root,
                opts.base.as_deref(),
                is_relative,
            ));
            let spec_json = json(&spec);
            if opts.eager {
                let id = format!("__oj_glob{calls}_{idx}");
                prelude.push(match import_name {
                    _ if want_default => format!("import {id} from {spec_json};"),
                    Some(name) => format!("import {{ {name} as {id} }} from {spec_json};"),
                    None => format!("import * as {id} from {spec_json};"),
                });
                entries.push(format!("{key}: {id}"));
            } else {
                entries.push(match import_name {
                    _ if want_default => {
                        format!("{key}: () => import({spec_json}).then((m) => m.default)")
                    }
                    Some(name) => format!(
                        "{key}: () => import({spec_json}).then((m) => m[{}])",
                        json(name)
                    ),
                    None => format!("{key}: () => import({spec_json})"),
                });
            }
        }
        let spanned = code[head..call_end].matches('\n').count();
        out.push_str(&code[last..head]);
        out.push('{');
        out.push_str(&entries.join(", "));
        out.push_str(&"\n".repeat(spanned));
        out.push('}');
        last = call_end;
        calls += 1;
    }
    out.push_str(&code[last..]);
    Ok(if prelude.is_empty() {
        out
    } else {
        format!("{} {out}", prelude.join(" "))
    })
}

/// A call's options after Vite's normalization: the deprecated `as` is
/// `query` under another name, and `as: 'raw' | 'url'` forces the default
/// import.
#[derive(Default)]
struct GlobOpts {
    eager: bool,
    import: Option<String>,
    /// `?`-led, or empty.
    query: String,
    exhaustive: bool,
    case_sensitive: bool,
    base: Option<String>,
}

impl GlobOpts {
    fn normalize(opts: &[(String, Value)]) -> Result<GlobOpts, String> {
        let get = |k: &str| opts.iter().find(|(key, _)| key == k).map(|(_, v)| v);
        let mut import = match get("import") {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let mut query = match get("query") {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Object(pairs)) => {
                let mut ser = url::form_urlencoded::Serializer::new(String::new());
                for (k, v) in pairs {
                    ser.append_pair(k, &v.as_query_value());
                }
                format!("?{}", ser.finish())
            }
            _ => String::new(),
        };
        if let Some(Value::Str(as_)) = get("as") {
            if matches!(get("query"), Some(Value::Str(_)) | Some(Value::Object(_))) {
                return Err("Options \"as\" and \"query\" cannot be used together".into());
            }
            if as_ == "raw" || as_ == "url" {
                if let Some(i) = import.as_deref().filter(|i| *i != "default" && *i != "*") {
                    return Err(format!(
                        "Option \"import\" can only be \"default\" or \"*\" when \"as\" is \"{as_}\", but got \"{i}\""
                    ));
                }
                import = Some("default".into());
            }
            query = as_.clone();
        }
        if !query.is_empty() && !query.starts_with('?') {
            query.insert(0, '?');
        }
        let base = match get("base") {
            Some(Value::Str(b)) => {
                if !(b.starts_with('/') || b.starts_with("./") || b.starts_with("../")) {
                    return Err(format!(
                        "Option \"base\" must start with '/', './' or '../', but got \"{b}\""
                    ));
                }
                Some(b.clone())
            }
            _ => None,
        };
        Ok(GlobOpts {
            eager: matches!(get("eager"), Some(Value::Bool(true))),
            import,
            query,
            exhaustive: matches!(get("exhaustive"), Some(Value::Bool(true))),
            case_sensitive: !matches!(get("caseSensitive"), Some(Value::Bool(false))),
            base,
        })
    }
}

/// The key of a matched file, as Vite writes it: relative to `base` when
/// set, to the importer for relative patterns, and root-relative (`/src/...`)
/// otherwise.
fn key_of(file: &Path, file_dir: &Path, root: &Path, base: Option<&str>, relative: bool) -> String {
    if let Some(base) = base {
        let base_dir = match base.strip_prefix('/') {
            Some(rest) => root.join(rest),
            None => file_dir.join(base),
        };
        return relative_import(&base_dir, file);
    }
    if relative {
        return relative_import(file_dir, file);
    }
    let rel = relative_import(root, file);
    match rel.strip_prefix("./") {
        Some(inside) => format!("/{inside}"),
        None => rel,
    }
}

/// Past whitespace and an optional balanced TypeScript type argument
/// (`<Record<string, unknown>>`, strings inside skipped), the offset just
/// past the call's `(`.
fn call_args_start(code: &str, from: usize) -> Option<usize> {
    let b = code.as_bytes();
    let mut i = from;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) == Some(&b'<') {
        let mut depth = 0i32;
        let mut quote: Option<u8> = None;
        while i < b.len() {
            let c = b[i];
            if let Some(q) = quote {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' | b'\'' | b'`' => quote = Some(c),
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    (b.get(i) == Some(&b'(')).then_some(i + 1)
}

/// From just past a `(`, the offset of its matching `)`, strings skipped.
fn matching_paren(code: &str, from: usize) -> Option<usize> {
    let b = code.as_bytes();
    let mut depth = 1i32;
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' | b'`' => quote = Some(c),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A literal argument value: what `new Function("return [" + args + "]")()`
/// evaluates to for the static forms Vite accepts.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The value as URLSearchParams stringifies it.
    fn as_query_value(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{}", *n as i64),
            Value::Num(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            Value::Array(items) => items
                .iter()
                .map(Value::as_query_value)
                .collect::<Vec<_>>()
                .join(","),
            Value::Object(_) => "[object Object]".into(),
        }
    }
}

/// The call's arguments as static values; `None` when any argument is not one
/// (a variable, a call), which Vite rejects and oj leaves untouched.
fn static_args(src: &str) -> Option<Vec<Value>> {
    let wrapped = format!("[{src}]");
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &wrapped, SourceType::mjs()).parse_expression();
    let expr = parsed.ok()?;
    match static_value(&expr)? {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

fn static_value(expr: &Expression) -> Option<Value> {
    Some(match expr {
        Expression::StringLiteral(s) => Value::Str(s.value.to_string()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() => Value::Str(
            t.quasis
                .iter()
                .map(|q| {
                    q.value
                        .cooked
                        .as_ref()
                        .map_or(q.value.raw.as_str(), |c| c.as_str())
                })
                .collect(),
        ),
        Expression::NumericLiteral(n) => Value::Num(n.value),
        Expression::BooleanLiteral(b) => Value::Bool(b.value),
        Expression::NullLiteral(_) => Value::Null,
        Expression::ParenthesizedExpression(p) => static_value(&p.expression)?,
        Expression::TSAsExpression(a) => static_value(&a.expression)?,
        Expression::TSSatisfiesExpression(a) => static_value(&a.expression)?,
        Expression::ArrayExpression(a) => {
            let mut items = Vec::with_capacity(a.elements.len());
            for el in &a.elements {
                match el {
                    ArrayExpressionElement::SpreadElement(_)
                    | ArrayExpressionElement::Elision(_) => return None,
                    other => items.push(static_value(other.as_expression()?)?),
                }
            }
            Value::Array(items)
        }
        Expression::ObjectExpression(o) => {
            let mut pairs = Vec::with_capacity(o.properties.len());
            for prop in &o.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    return None;
                };
                if p.computed {
                    return None;
                }
                let key = match &p.key {
                    PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                    PropertyKey::StringLiteral(s) => s.value.to_string(),
                    PropertyKey::NumericLiteral(n) => n.value.to_string(),
                    _ => return None,
                };
                pairs.push((key, static_value(&p.value)?));
            }
            Value::Object(pairs)
        }
        _ => return None,
    })
}

fn json(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// Vite's stripLiteral (strip-literal) on `code`: every comment, string body,
/// template text and regex literal body blanked to spaces, offsets and line
/// breaks kept (a multi-byte character becomes as many spaces as it has
/// bytes). A template's `${...}` code stays, as does every other piece of
/// code.
pub fn strip_literals(code: &str) -> String {
    const REGEX_AFTER_WORD: &[&str] = &[
        "return",
        "typeof",
        "instanceof",
        "in",
        "of",
        "new",
        "delete",
        "void",
        "throw",
        "case",
        "do",
        "else",
        "yield",
        "await",
    ];
    let src = code.as_bytes();
    let mut out = src.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for b in &mut out[from..to] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    };
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    // Open `${` substitutions, each with the `{` depth inside it.
    let mut templates: Vec<i32> = Vec::new();
    // A template's text from `from` to its closing backtick or next `${`.
    let quasi = |out: &mut Vec<u8>, templates: &mut Vec<i32>, from: usize| -> usize {
        let mut j = from;
        while j < src.len() {
            match src[j] {
                b'\\' => j += 2,
                b'`' => {
                    blank(out, from, j);
                    return j + 1;
                }
                b'$' if src.get(j + 1) == Some(&b'{') => {
                    blank(out, from, j);
                    templates.push(0);
                    return j + 2;
                }
                _ => j += 1,
            }
        }
        blank(out, from, src.len());
        src.len()
    };
    // js-tokens' regex-or-division call from the last significant token.
    let regex_allowed = |out: &[u8], at: usize| -> bool {
        let mut k = at;
        while k > 0 && out[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        if k == 0 {
            return true;
        }
        let ch = out[k - 1];
        if is_word(ch) {
            let mut w = k - 1;
            while w > 0 && is_word(out[w - 1]) {
                w -= 1;
            }
            let word = std::str::from_utf8(&out[w..k]).unwrap_or("");
            return REGEX_AFTER_WORD.contains(&word);
        }
        !matches!(ch, b')' | b']' | b'}' | b'"' | b'\'' | b'`')
    };
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == b'/' && src.get(i + 1) == Some(&b'/') {
            let stop = memchr::memchr(b'\n', &src[i..]).map_or(src.len(), |n| i + n);
            blank(&mut out, i, stop);
            i = stop;
        } else if c == b'/' && src.get(i + 1) == Some(&b'*') {
            let stop =
                memchr::memmem::find(&src[i + 2..], b"*/").map_or(src.len(), |n| i + 2 + n + 2);
            blank(&mut out, i, stop);
            i = stop;
        } else if c == b'"' || c == b'\'' {
            let mut j = i + 1;
            while j < src.len() && src[j] != c && src[j] != b'\n' {
                j += if src[j] == b'\\' { 2 } else { 1 };
            }
            // strip-literal keeps an unclosed string as written.
            if src.get(j) == Some(&c) {
                blank(&mut out, i + 1, j);
            }
            i = j + 1;
        } else if c == b'`' {
            i = quasi(&mut out, &mut templates, i + 1);
        } else if c == b'}' && templates.last() == Some(&0) {
            templates.pop();
            i = quasi(&mut out, &mut templates, i + 1);
        } else if c == b'/' && regex_allowed(&out, i) {
            let mut j = i + 1;
            let mut class = false;
            while j < src.len() && src[j] != b'\n' && (class || src[j] != b'/') {
                match src[j] {
                    b'\\' => j += 1,
                    b'[' => class = true,
                    b']' => class = false,
                    _ => {}
                }
                j += 1;
            }
            if src.get(j) == Some(&b'/') {
                blank(&mut out, i + 1, j);
            }
            i = j + 1;
        } else {
            if let Some(depth) = templates.last_mut() {
                match c {
                    b'{' => *depth += 1,
                    b'}' => *depth -= 1,
                    _ => {}
                }
            }
            i += 1;
        }
    }
    // Only ASCII spaces replaced whole UTF-8 sequences' bytes, and every
    // blanked range starts and ends on a character boundary of `code`.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// One `new URL(<literal>, import.meta.url)` site (or the URL of a
/// `new Worker(new URL(...))`), found on the stripped code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaUrl {
    /// The span the rewrite replaces: the `new URL(...)` expression.
    pub start: usize,
    pub end: usize,
    pub kind: MetaUrlKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaUrlKind {
    /// `new Worker(new URL(file, ...))`: `shared` for a SharedWorker.
    Worker {
        file: String,
        postfix: String,
        shared: bool,
    },
    /// `new URL(file, import.meta.url)` of an asset.
    Asset { file: String, postfix: String },
    /// A template URL with substitutions: Vite's eager `?url` glob lookup.
    Template { replacement: String },
}

impl MetaUrlKind {
    /// The file part to resolve, when the site names one.
    pub fn file(&self) -> Option<&str> {
        match self {
            MetaUrlKind::Worker { file, .. } | MetaUrlKind::Asset { file, .. } => Some(file),
            MetaUrlKind::Template { .. } => None,
        }
    }
}

/// The `new URL(..., import.meta.url)` sites of a client module, in order,
/// as Vite's worker and asset import-meta-url plugins match them: on the
/// code with literals stripped (a commented-out call or one spelled inside a
/// string is none), skipping `/* @vite-ignore */` and `data:` URLs. A
/// template worker URL with substitutions is an error, as in Vite.
pub fn scan_import_meta_urls(code: &str, id: &str) -> Result<Vec<MetaUrl>, GlobError> {
    if !code.contains("import.meta.url") {
        return Ok(Vec::new());
    }
    static WORKER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static ASSET: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let worker_re = WORKER.get_or_init(|| {
        regex::Regex::new(
            r#"\bnew\s+(Worker|SharedWorker)\s*\(\s*(new\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\))"#,
        )
        .expect("worker url regex")
    });
    let asset_re = ASSET.get_or_init(|| {
        regex::Regex::new(
            r#"\bnew\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\)"#,
        )
        .expect("asset url regex")
    });
    let clean = strip_literals(code);
    let mut sites = Vec::new();
    for caps in worker_re.captures_iter(&clean) {
        let exp = caps.get(2).expect("expression group");
        let url = caps.get(3).expect("url group");
        let raw = &code[url.start()..url.end()];
        if raw.starts_with('`') && raw.contains("${") {
            return Err(GlobError(format!(
                "`new URL(url, import.meta.url)` is not supported in dynamic template string. ({id})"
            )));
        }
        let (file, postfix) = split_postfix(&raw[1..raw.len() - 1]);
        sites.push(MetaUrl {
            start: exp.start(),
            end: exp.end(),
            kind: MetaUrlKind::Worker {
                file: file.to_string(),
                postfix: postfix.to_string(),
                shared: &caps[1] == "SharedWorker",
            },
        });
    }
    static IGNORE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let ignore = IGNORE.get_or_init(|| regex::Regex::new(r"/\*\s*@vite-ignore\s*\*/").unwrap());
    let mut assets = Vec::new();
    for caps in asset_re.captures_iter(&clean) {
        let whole = caps.get(0).expect("match");
        let url = caps.get(1).expect("url group");
        // A worker's URL, taken above.
        if sites
            .iter()
            .any(|s: &MetaUrl| whole.start() >= s.start && whole.end() <= s.end)
        {
            continue;
        }
        if ignore.is_match(&code[whole.start()..url.start()]) {
            continue;
        }
        let raw = &code[url.start()..url.end()];
        if raw.starts_with('`') && raw.contains("${") {
            let q = query_delimiter(raw);
            let (pure, query_string) = match q {
                Some(q) => (format!("{}`", &raw[..q]), &raw[q..raw.len() - 1]),
                None => (raw.to_string(), ""),
            };
            let pattern = template_glob(&pure);
            // Vite leaves a pattern that starts with a wildcard alone.
            if pattern.starts_with('*') {
                continue;
            }
            let query = if query_string.is_empty() {
                "?url".to_string()
            } else {
                format!("?url&{}", &query_string[1..])
            };
            let opts = format!(
                "{{\"eager\":true,\"import\":\"default\",\"query\":{}}}",
                json(&query)
            );
            assets.push(MetaUrl {
                start: whole.start(),
                end: whole.end(),
                kind: MetaUrlKind::Template {
                    replacement: format!(
                        "new URL((import.meta.glob({}, {opts}))[{pure}], import.meta.url)",
                        json(&pattern)
                    ),
                },
            });
            continue;
        }
        let url_text = &raw[1..raw.len() - 1];
        if url_text
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("data:")
        {
            continue;
        }
        let (file, postfix) = split_postfix(url_text);
        assets.push(MetaUrl {
            start: whole.start(),
            end: whole.end(),
            kind: MetaUrlKind::Asset {
                file: file.to_string(),
                postfix: postfix.to_string(),
            },
        });
    }
    sites.extend(assets);
    Ok(sites)
}

/// Whether a URL names no file at all (`https:`, `blob:`).
pub fn has_scheme(file: &str) -> bool {
    let mut chars = file.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && file.find(':').is_some_and(|i| {
            file[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        })
}

/// How a site resolved: the file it names, the worker entry itself, or
/// nothing (the site stays as written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaUrlTarget {
    File(PathBuf),
    /// A worker starting its own entry: `self.location.href`.
    SelfWorker,
    Unresolved,
}

/// Applies the rewrites for `sites` (as [`scan_import_meta_urls`] returned
/// them) with their resolved `targets`: a worker URL becomes its file's
/// `?worker&url` import, an asset URL its `?url` import (Vite's injectQuery:
/// the new query first, then the URL's own; a hash stays on the URL), a
/// template URL the eager glob lookup [`transform_glob`] expands next.
pub fn apply_import_meta_urls(code: &str, sites: &[MetaUrl], targets: &[MetaUrlTarget]) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut hoisted: Vec<String> = Vec::new();
    let hoist = |prefix: &str, spec: String, hoisted: &mut Vec<String>| {
        let ident = format!("__oj_{prefix}{}", hoisted.len());
        hoisted.push(format!("import {ident} from {};", json(&spec)));
        ident
    };
    for (site, target) in sites.iter().zip(targets) {
        match (&site.kind, target) {
            (MetaUrlKind::Template { replacement }, _) => {
                edits.push((site.start, site.end, replacement.clone()))
            }
            (_, MetaUrlTarget::Unresolved) => {}
            (MetaUrlKind::Worker { .. }, MetaUrlTarget::SelfWorker) => {
                edits.push((site.start, site.end, "self.location.href".into()))
            }
            (
                MetaUrlKind::Worker {
                    postfix, shared, ..
                },
                MetaUrlTarget::File(abs),
            ) => {
                let kind = if *shared { "sharedworker" } else { "worker" };
                let ident = hoist(
                    "worker_url",
                    inject_query(&abs.to_string_lossy(), postfix, &format!("{kind}&url")),
                    &mut hoisted,
                );
                edits.push((
                    site.start,
                    site.end,
                    format!("new URL(/* @vite-ignore */ {ident}, '' + import.meta.url)"),
                ));
            }
            (MetaUrlKind::Asset { postfix, .. }, MetaUrlTarget::File(abs)) => {
                let hash = postfix.starts_with('#');
                let query_postfix = if hash { "" } else { postfix.as_str() };
                let ident = hoist(
                    "asset_url",
                    inject_query(&abs.to_string_lossy(), query_postfix, "url"),
                    &mut hoisted,
                );
                let href = if hash {
                    format!("{ident} + {}", json(postfix))
                } else {
                    ident
                };
                edits.push((
                    site.start,
                    site.end,
                    format!("new URL({href}, '' + import.meta.url)"),
                ));
            }
            (MetaUrlKind::Asset { .. }, MetaUrlTarget::SelfWorker) => {}
        }
    }
    if edits.is_empty() {
        return code.to_string();
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(code.len());
    let mut last = 0;
    for (start, end, text) in edits {
        out.push_str(&code[last..start]);
        out.push_str(&text);
        out.push_str(&"\n".repeat(code[start..end].matches('\n').count()));
        last = end;
    }
    out.push_str(&code[last..]);
    if hoisted.is_empty() {
        out
    } else {
        format!("{} {out}", hoisted.join(" "))
    }
}

fn split_postfix(url: &str) -> (&str, &str) {
    match url.find(['?', '#']) {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    }
}

/// Vite's injectQuery: the new query first, then the URL's own.
fn inject_query(file: &str, postfix: &str, query: &str) -> String {
    match postfix.strip_prefix('?') {
        Some(own) => format!("{file}?{query}&{own}"),
        None => format!("{file}?{query}{postfix}"),
    }
}

/// Vite's getQueryDelimiterIndex: the first `?` outside a `${...}`.
fn query_delimiter(raw: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in raw.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            '?' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Vite's buildGlobPattern: a template's text with each `${...}` run as one
/// `*`.
fn template_glob(raw: &str) -> String {
    let body = &raw[1..raw.len() - 1];
    let b = body.as_bytes();
    let mut pattern = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                match b[i] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            if !pattern.ends_with('*') {
                pattern.push('*');
            }
        } else {
            let ch = body[i..].chars().next().expect("in bounds");
            pattern.push(ch);
            i += ch.len_utf8();
        }
    }
    pattern
}

#[cfg(test)]
#[path = "meta_glob_tests.rs"]
mod tests;
