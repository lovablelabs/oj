// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's dev HTML transform for a page no plugin hooks (no
//! `transformIndexHtml`), natively: the hooks createDevHtmlTransformFn runs
//! (server/middlewares/indexHtml.ts) and applyHtmlTransforms' tag injection
//! (plugins/html.ts), translated from the plugin host's port
//! (assets/vite-html.mjs) so both give the same bytes; tests/fixtures/dev-html
//! holds pages with Vite's own output for them. An inline
//! `<script type="module">` becomes an html-proxy module kept in
//! [`HtmlProxyCache`]; inline styles compile through `compile_style`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

const FS_PREFIX: &str = "/@fs/";
const VALID_ID_PREFIX: &str = "/@id/";
const NULL_BYTE_PLACEHOLDER: &str = "__x00__";

/// What a page's transform needs to know about the app.
pub(crate) struct DevHtmlConfig<'a> {
    pub root: &'a Path,
    /// `base`, `/` when unset.
    pub base: &'a str,
    /// Vite's `{ ...config.env, ...import.meta.env defines }`.
    pub env: &'a std::collections::BTreeMap<String, String>,
    pub env_prefixes: &'a [String],
    pub nonce: Option<&'a str>,
    /// The dev client's module scripts, injected head-prepend.
    pub client_srcs: &'a [String],
}

/// The request: `path` (ctx.path), `filename` (the html on disk, or where it
/// would be), `original_url` (what the browser asked for).
pub(crate) struct DevHtmlRequest<'a> {
    pub path: &'a str,
    pub filename: &'a Path,
    pub original_url: &'a str,
}

/// The inline modules lifted out of served pages, by page, as Vite's
/// html-inline-proxy plugin keeps them.
#[derive(Default)]
pub(crate) struct HtmlProxyCache {
    pages: Mutex<HashMap<String, Vec<Option<String>>>>,
}

impl HtmlProxyCache {
    fn add(&self, page: &str, index: usize, code: String) {
        let mut pages = self.pages.lock().unwrap();
        let entries = pages.entry(page.to_string()).or_default();
        if entries.len() <= index {
            entries.resize(index + 1, None);
        }
        entries[index] = Some(code);
    }

    /// The code an html-proxy `id` names (`/index.html?html-proxy&index=0.js`),
    /// as html-inline-proxy's load finds it: by the id's page without the root.
    pub(crate) fn load(&self, id: &str, root: &Path) -> Option<String> {
        let index: usize = HTML_PROXY_RE.captures(id)?[1].parse().ok()?;
        let root = normalize_path(&root.to_string_lossy());
        let page = clean_url(id).replacen(&root, "", 1);
        self.pages.lock().unwrap().get(&page)?.get(index)?.clone()
    }
}

static HTML_PROXY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[?&]html-proxy=?(?:&inline-css)?(?:&style-attr)?&index=(\d+)\.(?:js|css)$")
        .unwrap()
});
static IMPORT_MAP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)[ \t]*<script[^>]*type\s*=\s*(?:"importmap"|'importmap'|importmap)[^>]*>.*?</script>"#)
        .unwrap()
});
static IMPORT_MAP_APPEND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)[ \t]*<script[^>]*type\s*=\s*(?:"module"|'module'|module)[^>]*>"#,
        r#"|[ \t]*<link[^>]*rel\s*=\s*(?:"modulepreload"|'modulepreload'|modulepreload)[\s\S]*?>"#
    ))
    .unwrap()
});
static ENV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"%(\S+?)%").unwrap());
static HEAD_INJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)</head>").unwrap());
static HEAD_PREPEND_INJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)<head[^>]*>").unwrap());
static HTML_PREPEND_INJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)<html[^>]*>").unwrap());
static BODY_PREPEND_INJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)<body[^>]*>").unwrap());
static DOCTYPE_PREPEND_INJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<!doctype html>").unwrap());

// ---- url helpers (Vite's utils) ---------------------------------------------

fn clean_url(url: &str) -> &str {
    url.find(['?', '#']).map_or(url, |i| &url[..i])
}

fn slash(p: &str) -> String {
    p.replace('\\', "/")
}

/// Node's `path.posix.normalize`.
fn posix_normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let absolute = p.starts_with('/');
    let trailing = p.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() && !absolute {
        out.push('.');
    }
    if trailing && !out.is_empty() && out != "." {
        out.push('/');
    } else if trailing && out == "." {
        out = "./".to_string();
    }
    if absolute {
        out.insert(0, '/');
        if out == "//" {
            out = "/".to_string();
        }
    }
    out
}

/// Node's `path.posix.join`.
pub(crate) fn posix_join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    posix_normalize(&joined.join("/"))
}

fn normalize_path(id: &str) -> String {
    posix_normalize(&slash(id))
}

fn wrap_id(id: &str) -> String {
    if id.starts_with(VALID_ID_PREFIX) {
        id.to_string()
    } else {
        format!(
            "{VALID_ID_PREFIX}{}",
            id.replacen('\0', NULL_BYTE_PLACEHOLDER, 1)
        )
    }
}

fn join_url_segments(a: &str, b: &str) -> String {
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() {
            b.to_string()
        } else {
            a.to_string()
        };
    }
    let a = a.strip_suffix('/').unwrap_or(a);
    if b.starts_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `decodeURI`: escapes outside the reserved set decode; a malformed one
/// keeps the input as is (the page then just misses the cache).
fn decode_uri(s: &str) -> String {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                if RESERVED.contains(&v) {
                    out.extend_from_slice(&bytes[i..i + 3]);
                } else {
                    out.push(v);
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// `wordCharRE.test(url[0]) && !url.includes(":")`; `url[0]` of `""` reads as
/// the string "undefined", a word char.
fn is_bare_relative(url: &str) -> bool {
    url.chars()
        .next()
        .is_none_or(|c| c.is_ascii_alphanumeric() || c == '_')
        && !url.contains(':')
}

// ---- HTML scanning ----------------------------------------------------------
// The slice of parse5 the hooks read: elements in document order with their
// attributes and source offsets, and the raw text of raw-text elements.

const RAW_TEXT: &[&str] = &[
    "script", "style", "xmp", "iframe", "noembed", "noframes", "textarea", "title",
];

#[derive(Debug, Clone, Copy)]
struct Loc {
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct Attr {
    name: String,
    value: String,
    loc: Loc,
}

#[derive(Debug)]
struct Node {
    name: String,
    attrs: Vec<Attr>,
    /// The whole element (start tag through end tag for raw text ones).
    loc: Loc,
    start_tag: Loc,
    /// A raw-text element's text, when not empty.
    text: Option<(String, Loc)>,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

fn decode_refs(value: &str) -> String {
    static REF_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)&(#x[0-9a-f]+|#\d+|[a-z]+);").unwrap());
    if !value.contains('&') {
        return value.to_string();
    }
    REF_RE
        .replace_all(value, |caps: &regex::Captures<'_>| {
            let r = &caps[1];
            if let Some(num) = r.strip_prefix('#') {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse::<u32>().ok(),
                };
                return code
                    .filter(|c| *c <= 0x10ffff)
                    .and_then(char::from_u32)
                    .map_or_else(|| caps[0].to_string(), |c| c.to_string());
            }
            match r.to_ascii_lowercase().as_str() {
                "amp" => "&".into(),
                "lt" => "<".into(),
                "gt" => ">".into(),
                "quot" => "\"".into(),
                "apos" => "'".into(),
                "nbsp" => "\u{a0}".into(),
                _ => caps[0].to_string(),
            }
        })
        .into_owned()
}

fn scan_start_tag(html: &str, lt: usize) -> Option<(String, Vec<Attr>, Loc)> {
    let b = html.as_bytes();
    let mut i = lt + 1;
    let name_start = i;
    while i < b.len() && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' {
        i += 1;
    }
    let name = html[name_start..i].to_ascii_lowercase();
    let mut attrs: Vec<Attr> = Vec::new();
    loop {
        while i < b.len() && (is_ws(b[i]) || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'>' {
            break;
        }
        let start = i;
        i += 1;
        while i < b.len() && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' && b[i] != b'=' {
            i += 1;
        }
        let attr_name = html[start..i].to_ascii_lowercase();
        let mut j = i;
        while j < b.len() && is_ws(b[j]) {
            j += 1;
        }
        let mut value = "";
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && is_ws(b[j]) {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j] as char;
                let close = html[j + 1..].find(q)? + j + 1;
                value = &html[j + 1..close];
                i = close + 1;
            } else {
                let v_start = j;
                while j < b.len() && !is_ws(b[j]) && b[j] != b'>' {
                    j += 1;
                }
                value = &html[v_start..j];
                i = j;
            }
        }
        if !attrs.iter().any(|a| a.name == attr_name) {
            attrs.push(Attr {
                name: attr_name,
                value: decode_refs(value),
                loc: Loc { start, end: i },
            });
        }
    }
    Some((
        name,
        attrs,
        Loc {
            start: lt,
            end: i + 1,
        },
    ))
}

fn scan_html(html: &str) -> Vec<Node> {
    let b = html.as_bytes();
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(lt) = html[i..].find('<').map(|p| p + i) else {
            break;
        };
        let next = b.get(lt + 1).copied();
        if html[lt..].starts_with("<!--") {
            i = html[lt + 4..]
                .find("-->")
                .map_or(b.len(), |c| lt + 4 + c + 3);
            continue;
        }
        if matches!(next, Some(b'!' | b'?' | b'/')) {
            i = html[lt + 2.min(b.len() - lt)..]
                .find('>')
                .map_or(b.len(), |c| lt + 2 + c + 1);
            continue;
        }
        if !next.is_some_and(|c| c.is_ascii_alphabetic()) {
            i = lt + 1;
            continue;
        }
        let Some((name, attrs, start_tag)) = scan_start_tag(html, lt) else {
            break;
        };
        let mut node = Node {
            name,
            attrs,
            loc: Loc {
                start: lt,
                end: start_tag.end,
            },
            start_tag,
            text: None,
        };
        i = start_tag.end;
        if RAW_TEXT.contains(&node.name.as_str()) {
            let close = find_end_tag(html, i, &node.name);
            let content_end = close.unwrap_or(b.len());
            if content_end > i {
                node.text = Some((
                    html[i..content_end].to_string(),
                    Loc {
                        start: i,
                        end: content_end,
                    },
                ));
            }
            let gt = close.and_then(|c| html[c..].find('>').map(|g| c + g));
            i = gt.map_or(b.len(), |g| g + 1);
            node.loc.end = i;
        }
        nodes.push(node);
    }
    nodes
}

/// `</name` followed by whitespace, `/` or `>`, ASCII case-insensitively.
fn find_end_tag(html: &str, from: usize, name: &str) -> Option<usize> {
    let b = html.as_bytes();
    let mut at = from;
    while let Some(p) = html[at..].find("</").map(|p| p + at) {
        let n = p + 2;
        if html
            .get(n..n + name.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
            && b.get(n + name.len())
                .is_some_and(|&c| is_ws(c) || c == b'/' || c == b'>')
        {
            return Some(p);
        }
        at = p + 2;
    }
    None
}

// ---- MagicString, the slice the hooks use -------------------------------------

struct Edits<'a> {
    original: &'a str,
    edits: Vec<(usize, usize, String)>,
}

impl<'a> Edits<'a> {
    fn new(original: &'a str) -> Self {
        Edits {
            original,
            edits: Vec::new(),
        }
    }

    fn update(&mut self, start: usize, end: usize, content: impl Into<String>) {
        self.edits.push((start, end, content.into()));
    }

    fn remove(&mut self, start: usize, end: usize) {
        self.update(start, end, "");
    }

    fn append_right(&mut self, at: usize, content: impl Into<String>) {
        self.update(at, at, content);
    }

    fn finish(mut self) -> String {
        // A stable sort keeps insertion order among equal starts.
        self.edits.sort_by_key(|e| e.0);
        let mut out = String::with_capacity(self.original.len());
        let mut at = 0;
        for (start, end, content) in &self.edits {
            if *start < at {
                continue;
            }
            out.push_str(&self.original[at..*start]);
            out.push_str(content);
            at = *end;
        }
        out.push_str(&self.original[at..]);
        out
    }
}

fn overwrite_attr_value(s: &mut Edits<'_>, loc: Loc, new_value: &str) {
    static VALUE_START_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"=\s*(.)").unwrap());
    let src = &s.original[loc.start..loc.end];
    let Some(caps) = VALUE_START_RE.captures(src) else {
        return;
    };
    let whole = caps.get(0).unwrap();
    let quote = &caps[1];
    let wrap = usize::from(quote == "\"" || quote == "'");
    let value_offset = whole.start() + whole.as_str().len() - caps[1].len();
    s.update(
        loc.start + value_offset + wrap,
        loc.end - wrap,
        new_value.to_string(),
    );
}

// ---- asset attributes ----------------------------------------------------------

const ALLOWED_META_NAME: &[&str] = &[
    "msapplication-tileimage",
    "msapplication-square70x70logo",
    "msapplication-square150x150logo",
    "msapplication-wide310x150logo",
    "msapplication-square310x310logo",
    "msapplication-config",
    "twitter:image",
];
const ALLOWED_META_PROPERTY: &[&str] = &[
    "og:image",
    "og:image:url",
    "og:image:secure_url",
    "og:audio",
    "og:audio:secure_url",
    "og:video",
    "og:video:secure_url",
];

/// DEFAULT_HTML_ASSET_SOURCES: (src attributes, srcset attributes).
fn asset_sources(tag: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match tag {
        "audio" | "embed" | "input" | "track" => (&["src"], &[]),
        "img" | "source" => (&["src"], &["srcset"]),
        "image" | "use" => (&["href", "xlink:href"], &[]),
        "link" => (&["href"], &["imagesrcset"]),
        "object" => (&["data"], &[]),
        "video" => (&["src", "poster"], &[]),
        "meta" => (&["content"], &[]),
        _ => return None,
    })
}

enum AssetAction<'n> {
    Remove(Loc),
    Url {
        value: &'n str,
        srcset: bool,
        loc: Loc,
    },
}

fn node_asset_attributes(node: &Node) -> Vec<AssetAction<'_>> {
    let Some((srcs, srcsets)) = asset_sources(&node.name) else {
        return Vec::new();
    };
    if let Some(ignore) = node.attr("vite-ignore") {
        return vec![AssetAction::Remove(ignore.loc)];
    }
    if node.name == "meta" {
        let named = node
            .attr("name")
            .is_some_and(|a| ALLOWED_META_NAME.contains(&a.value.trim().to_lowercase().as_str()));
        let property = node.attr("property").is_some_and(|a| {
            ALLOWED_META_PROPERTY.contains(&a.value.trim().to_lowercase().as_str())
        });
        if !named && !property {
            return Vec::new();
        }
    }
    let mut out = Vec::new();
    for (keys, srcset) in [(srcs, false), (srcsets, true)] {
        for key in keys {
            if let Some(attr) = node.attr(key).filter(|a| !a.value.is_empty()) {
                out.push(AssetAction::Url {
                    value: &attr.value,
                    srcset,
                    loc: attr.loc,
                });
            }
        }
    }
    out
}

/// Vite's parseSrcset + joinSrcset around `replace`.
fn process_srcset(srcs: &str, replace: impl Fn(&str) -> String) -> String {
    static ESCAPED_SPACES_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?: |\\t|\\n|\\f|\\r)+").unwrap());
    static NEWLINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n").unwrap());
    static COMMA_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r",\s+").unwrap());
    static SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static CANDIDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?P<url>[A-Za-z0-9_-]+\([^)]*\)|"[^"]*"|'[^']*'|[^,]\S*[^,])\s*(?:\s(?P<descriptor>[A-Za-z0-9_][^,]+))?(?:,|$)"#,
        )
        .unwrap()
    });
    let s = ESCAPED_SPACES_RE.replace_all(srcs.trim(), " ");
    let s = NEWLINE_RE.replacen(&s, 1, "");
    let s = COMMA_SPACE_RE.replacen(&s, 1, ", ");
    let s = SPACES_RE.replace_all(&s, " ").into_owned();
    // `(?:^|\s|(?<=,))` before each candidate, checked by hand.
    let mut parts = Vec::new();
    let mut at = 0;
    while at <= s.len() {
        let Some(caps) = CANDIDATE_RE.captures_at(&s, at) else {
            break;
        };
        let whole = caps.get(0).unwrap();
        let start = whole.start();
        let lead_ok = start == 0
            || s[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c == ',' || c.is_whitespace());
        if !lead_ok {
            at = start + s[start..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let url = caps.name("url").map_or("", |m| m.as_str()).trim();
        let descriptor = caps.name("descriptor").map_or("", |m| m.as_str()).trim();
        if !url.is_empty() {
            let url = replace(url);
            parts.push(if descriptor.is_empty() {
                url
            } else {
                format!("{url} {descriptor}")
            });
        }
        at = if whole.end() > start {
            whole.end()
        } else {
            start + 1
        };
    }
    parts.join(", ")
}

// ---- classic script import() ----------------------------------------------------

/// strip-literal: comments and string or template contents blanked to spaces
/// at the same offsets, quotes kept.
fn strip_literal(code: &str) -> String {
    let b = code.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let blank = |out: &mut Vec<u8>, s: &[u8]| {
        for &c in s {
            out.push(if c == b'\n' { b'\n' } else { b' ' });
        }
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let stop = code[i..].find('\n').map_or(b.len(), |e| i + e);
            blank(&mut out, &b[i..stop]);
            i = stop;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let stop = code[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            blank(&mut out, &b[i..stop]);
            i = stop;
        } else if c == b'"' || c == b'\'' || c == b'`' {
            let mut j = i + 1;
            while j < b.len() && b[j] != c {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            out.push(c);
            blank(&mut out, &b[i + 1..j.min(b.len())]);
            if j < b.len() {
                out.push(c);
            }
            i = j + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    // Multi-byte chars blanked byte by byte keep their offsets.
    String::from_utf8(out).unwrap_or_default()
}

/// `inlineImportRE` over the stripped code: `import("...")` / `import('...')`
/// not after a single `.`; the url spans of the original text.
fn classic_script_imports(text: &str, offset: usize) -> Vec<(usize, usize, String)> {
    let clean = strip_literal(text);
    let b = clean.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(p) = clean[at..].find("import").map(|p| p + at) {
        at = p + 1;
        if p > 0 && word(b[p - 1]) {
            continue;
        }
        if p > 0 && b[p - 1] == b'.' && !(p >= 3 && b[p - 2] == b'.' && b[p - 3] == b'.') {
            continue;
        }
        let mut i = p + "import".len();
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if b.get(i) != Some(&b'(') {
            continue;
        }
        let q = i + 1;
        let Some(&quote) = b.get(q).filter(|c| **c == b'"' || **c == b'\'') else {
            continue;
        };
        let Some(close) = clean[q + 1..].find(quote as char).map(|c| q + 1 + c) else {
            continue;
        };
        if b.get(close + 1) != Some(&b')') {
            continue;
        }
        let (start, end) = (q + 1, close);
        out.push((start + offset, end + offset, text[start..end].to_string()));
        at = close + 2;
    }
    out
}

// ---- tag injection (applyHtmlTransforms) ---------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum InjectTo {
    Head,
    HeadPrepend,
}

struct Tag {
    tag: &'static str,
    attrs: Vec<(&'static str, String)>,
    inject_to: InjectTo,
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn serialize_tag(t: &Tag) -> String {
    let attrs: String = t
        .attrs
        .iter()
        .map(|(k, v)| format!(" {k}=\"{}\"", escape_html(v)))
        .collect();
    if matches!(t.tag, "link" | "meta" | "base") {
        format!("<{}{attrs}>", t.tag)
    } else {
        format!("<{0}{attrs}></{0}>", t.tag)
    }
}

fn serialize_tags(tags: &[&Tag], indent: &str) -> String {
    tags.iter()
        .map(|t| format!("{indent}{}\n", serialize_tag(t)))
        .collect()
}

fn increment_indent(indent: &str) -> String {
    format!(
        "{indent}{}",
        if indent.starts_with('\t') { "\t" } else { "  " }
    )
}

/// `html.replace(re, fn)`: the first match only, as a non-global JS replace.
fn replace_first(html: &str, re: &Regex, f: impl FnOnce(&str, &str) -> String) -> Option<String> {
    let caps = re.captures(html)?;
    let whole = caps.get(0).unwrap();
    let p1 = caps.get(1).map_or("", |m| m.as_str());
    Some(format!(
        "{}{}{}",
        &html[..whole.start()],
        f(whole.as_str(), p1),
        &html[whole.end()..]
    ))
}

fn prepend_inject_fallback(html: &str, tags: &[&Tag]) -> String {
    if let Some(out) = replace_first(html, &HTML_PREPEND_INJECT_RE, |m, _| {
        format!("{m}\n{}", serialize_tags(tags, ""))
    }) {
        return out;
    }
    if let Some(out) = replace_first(html, &DOCTYPE_PREPEND_INJECT_RE, |m, _| {
        format!("{m}\n{}", serialize_tags(tags, ""))
    }) {
        return out;
    }
    format!("{}{html}", serialize_tags(tags, ""))
}

fn inject_to_head(html: &str, tags: &[&Tag], prepend: bool) -> String {
    if tags.is_empty() {
        return html.to_string();
    }
    if prepend {
        if let Some(out) = replace_first(html, &HEAD_PREPEND_INJECT_RE, |m, p1| {
            format!("{m}\n{}", serialize_tags(tags, &increment_indent(p1)))
        }) {
            return out;
        }
    } else {
        if let Some(out) = replace_first(html, &HEAD_INJECT_RE, |m, p1| {
            format!("{}{m}", serialize_tags(tags, &increment_indent(p1)))
        }) {
            return out;
        }
        if let Some(out) = replace_first(html, &BODY_PREPEND_INJECT_RE, |m, p1| {
            format!("{}\n{m}", serialize_tags(tags, p1))
        }) {
            return out;
        }
    }
    prepend_inject_fallback(html, tags)
}

/// Tags a natively run plugin's `transformIndexHtml` returned, `injectTo:
/// 'head'`, applied to an already transformed page.
pub(crate) fn inject_head_tags(
    html: String,
    tags: &[(&'static str, Vec<(&'static str, String)>)],
) -> String {
    let tags: Vec<Tag> = tags
        .iter()
        .map(|(tag, attrs)| Tag {
            tag,
            attrs: attrs.clone(),
            inject_to: InjectTo::Head,
        })
        .collect();
    inject_tags(html, &tags)
}

/// One hook result's tags, injected as applyHtmlTransforms does (only the
/// head positions occur natively).
fn inject_tags(html: String, tags: &[Tag]) -> String {
    let head: Vec<&Tag> = tags
        .iter()
        .filter(|t| t.inject_to == InjectTo::Head)
        .collect();
    let head_prepend: Vec<&Tag> = tags
        .iter()
        .filter(|t| t.inject_to == InjectTo::HeadPrepend)
        .collect();
    let mut html = html;
    if !head_prepend.is_empty() {
        html = inject_to_head(&html, &head_prepend, true);
    }
    if !head.is_empty() {
        html = inject_to_head(&html, &head, false);
    }
    html
}

// ---- the hooks --------------------------------------------------------------------

fn relative_html(root: &Path, filename: &Path) -> String {
    let rel = pathdiff::diff_paths(filename, root).unwrap_or_else(|| filename.to_path_buf());
    normalize_path(&rel.to_string_lossy())
}

fn warn_once(msg: String) {
    static WARNED: LazyLock<Mutex<std::collections::HashSet<String>>> =
        LazyLock::new(Default::default);
    if WARNED.lock().unwrap().insert(msg.clone()) {
        eprintln!("{msg}");
    }
}

fn pre_import_map_hook(html: &str, root: &Path, filename: &Path) {
    let Some(map) = IMPORT_MAP_RE.find(html) else {
        return;
    };
    let Some(append) = IMPORT_MAP_APPEND_RE.find(html) else {
        return;
    };
    if append.start() < map.start() {
        warn_once(format!(
            "(!) <script type=\"importmap\"> should come before <script type=\"module\"> and <link rel=\"modulepreload\"> in /{}",
            relative_html(root, filename)
        ));
    }
}

fn post_import_map_hook(html: String) -> String {
    if !IMPORT_MAP_APPEND_RE.is_match(&html) {
        return html;
    }
    let Some(map) = IMPORT_MAP_RE.find(&html) else {
        return html;
    };
    let import_map = map.as_str().to_string();
    let html = format!("{}{}", &html[..map.start()], &html[map.end()..]);
    replace_first(&html, &IMPORT_MAP_APPEND_RE, |m, _| {
        format!("{import_map}\n{m}")
    })
    .unwrap_or(html)
}

fn html_env_hook(html: &str, cfg: &DevHtmlConfig<'_>, filename: &Path) -> String {
    ENV_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let key = &caps[1];
            if let Some(v) = cfg.env.get(key) {
                return v.clone();
            }
            if cfg.env_prefixes.iter().any(|p| key.starts_with(p.as_str())) {
                eprintln!(
                    "(!) {} is not defined in env variables found in /{}. Is the variable mistyped?",
                    &caps[0],
                    relative_html(cfg.root, filename)
                );
            }
            caps[0].to_string()
        })
        .into_owned()
}

fn nonce_attribute_hook(html: String, nonce: &str) -> String {
    let nodes = scan_html(&html);
    let mut s = Edits::new(&html);
    for node in &nodes {
        let wants = match node.name.as_str() {
            "script" | "style" => true,
            "link" => node.attrs.iter().any(|a| {
                a.name == "rel"
                    && a.value.split(['\t', '\n', '\x0c', '\r', ' ']).any(|v| {
                        matches!(
                            v.to_lowercase().as_str(),
                            "stylesheet" | "modulepreload" | "preload"
                        )
                    })
            }),
            _ => false,
        };
        if !wants || node.attr("nonce").is_some() {
            continue;
        }
        let end = node.start_tag.end;
        let back = if html.as_bytes().get(end.wrapping_sub(2)) == Some(&b'/') {
            2
        } else {
            1
        };
        s.append_right(end - back, format!(" nonce=\"{nonce}\""));
    }
    s.finish()
}

/// What devHtmlHook needs from the server: compile an inline style under its
/// html-proxy id, and keep a lifted module.
pub(crate) trait DevHtmlHost {
    fn compile_style(
        &self,
        id: &str,
        code: &str,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send;
    fn proxy(&self) -> &HtmlProxyCache;
}

fn process_node_url(
    url: &str,
    srcset: bool,
    cfg: &DevHtmlConfig<'_>,
    req: &DevHtmlRequest<'_>,
) -> String {
    let replacer = |url: &str| -> String {
        let b = url.as_bytes();
        if (b.first() == Some(&b'/') && b.get(1) != Some(&b'/'))
            || ((b.first() == Some(&b'.') || is_bare_relative(url))
                && !req.original_url.is_empty()
                && req.original_url != "/"
                && req.path == "/index.html")
        {
            return posix_join(&[cfg.base, url]);
        }
        url.to_string()
    };
    if srcset {
        process_srcset(url, replacer)
    } else {
        replacer(url)
    }
}

async fn dev_html_hook<H: DevHtmlHost>(
    html: &str,
    cfg: &DevHtmlConfig<'_>,
    req: &DevHtmlRequest<'_>,
    host: &H,
) -> Result<String, String> {
    let decoded_base = decode_uri(cfg.base);
    let trailing = req.path.ends_with('/');
    let (proxy_module_path, proxy_module_url) = if !trailing && req.filename.exists() {
        let path = if req.path.starts_with(FS_PREFIX) {
            req.filename.to_string_lossy().into_owned()
        } else {
            req.path.to_string()
        };
        (path, req.path.to_string())
    } else {
        let path = format!("\0{}{}", req.path, if trailing { "index.html" } else { "" });
        let url = wrap_id(&path);
        (path, url)
    };
    let proxy_module_url = join_url_segments(&decoded_base, &proxy_module_url);
    let root = normalize_path(&cfg.root.to_string_lossy());
    let proxy_cache_url = decode_uri(&clean_url(&proxy_module_path).replacen(&root, "", 1));

    let nodes = scan_html(html);
    let mut s = Edits::new(html);
    let mut inline_index: isize = -1;
    let mut styles: Vec<(Loc, &str)> = Vec::new();
    let mut style_attrs: Vec<(usize, Loc, &str)> = Vec::new();
    for node in &nodes {
        if node.name == "script" {
            let src = node.attr("src");
            let module = node.attr("type").is_some_and(|t| t.value == "module");
            if let Some(ignore) = node.attr("vite-ignore") {
                s.remove(ignore.loc.start, ignore.loc.end);
            } else if let Some(src) = src {
                let processed = process_node_url(&src.value, false, cfg, req);
                if processed != src.value {
                    overwrite_attr_value(&mut s, src.loc, &processed);
                }
            } else if let (true, Some((code, _))) = (module, &node.text) {
                inline_index += 1;
                host.proxy()
                    .add(&proxy_cache_url, inline_index as usize, code.clone());
                s.update(
                    node.loc.start,
                    node.loc.end,
                    format!(
                        "<script type=\"module\" src=\"{proxy_module_url}?html-proxy&index={inline_index}.js\"></script>"
                    ),
                );
            } else if let Some((code, loc)) = &node.text {
                for (start, end, url) in classic_script_imports(code, loc.start) {
                    let processed = process_node_url(&url, false, cfg, req);
                    if processed != url {
                        s.update(start, end, processed);
                    }
                }
            }
        }
        if let Some(style) = node
            .attr("style")
            .filter(|a| a.value.contains("url(") || a.value.contains("image-set("))
        {
            inline_index += 1;
            style_attrs.push((inline_index as usize, style.loc, &style.value));
        }
        if node.name == "style" {
            if let Some((code, loc)) = &node.text {
                styles.push((*loc, code));
            }
        }
        for action in node_asset_attributes(node) {
            match action {
                AssetAction::Remove(loc) => s.remove(loc.start, loc.end),
                AssetAction::Url { value, srcset, loc } => {
                    let processed = process_node_url(value, srcset, cfg, req);
                    if processed != value {
                        overwrite_attr_value(&mut s, loc, &processed);
                    }
                }
            }
        }
    }
    let sheets =
        futures_util::future::try_join_all(styles.iter().enumerate().map(|(index, (_, code))| {
            let id = format!("{proxy_module_path}?html-proxy&direct&index={index}.css");
            async move { host.compile_style(&id, code).await }
        }));
    let attrs = futures_util::future::try_join_all(style_attrs.iter().map(|(index, _, code)| {
        let id = format!("{proxy_module_path}?html-proxy&inline-css&style-attr&index={index}.css");
        async move { host.compile_style(&id, code).await }
    }));
    let (sheets, attrs) = futures_util::try_join!(sheets, attrs)?;
    for ((loc, _), css) in styles.iter().zip(sheets) {
        s.update(loc.start, loc.end, css);
    }
    for ((_, loc, _), css) in style_attrs.iter().zip(attrs) {
        overwrite_attr_value(&mut s, *loc, &css);
    }
    Ok(s.finish())
}

/// The dev client's scripts under `base` (`path.posix.join(base, src)`).
pub(crate) fn client_srcs(base: &str, srcs: &[&str]) -> Vec<String> {
    srcs.iter().map(|src| posix_join(&[base, src])).collect()
}

/// The page through createDevHtmlTransformFn's hooks, none of them a plugin's.
pub(crate) async fn transform_dev_html<H: DevHtmlHost>(
    html: &str,
    cfg: &DevHtmlConfig<'_>,
    req: &DevHtmlRequest<'_>,
    host: &H,
) -> Result<String, String> {
    pre_import_map_hook(html, cfg.root, req.filename);
    let mut html = html.to_string();
    if let Some(nonce) = cfg.nonce {
        html = inject_tags(
            html,
            &[Tag {
                tag: "meta",
                attrs: vec![("property", "csp-nonce".into()), ("nonce", nonce.into())],
                inject_to: InjectTo::Head,
            }],
        );
    }
    html = html_env_hook(&html, cfg, req.filename);
    html = dev_html_hook(&html, cfg, req, host).await?;
    let client: Vec<Tag> = cfg
        .client_srcs
        .iter()
        .map(|src| Tag {
            tag: "script",
            attrs: vec![("type", "module".into()), ("src", src.clone())],
            inject_to: InjectTo::HeadPrepend,
        })
        .collect();
    html = inject_tags(html, &client);
    if let Some(nonce) = cfg.nonce {
        html = nonce_attribute_hook(html, nonce);
    }
    Ok(post_import_map_hook(html))
}

#[cfg(test)]
#[path = "dev_html_tests.rs"]
mod tests;
