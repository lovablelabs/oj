//! Vite's dev stylesheet url rewrite (css.ts UrlRewritePostcssPlugin:
//! rewriteCssUrls, rewriteCssImageSet, doUrlReplace and the urlResolver) on a
//! sheet oj compiled with its urls already rebased to served paths. It runs
//! when the sheet is served, never in the cached compile: an inlined svg or
//! an HMR `t=` stamp then always reflects the asset as it is now.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::assets_serve::{decode_uri, DevAssetUrls};

/// What the url resolver needs: where files live, how they are named, and
/// each asset's last HMR timestamp (Vite's `lastHMRTimestamp`, 0 for none).
pub(crate) struct CssUrls<'a> {
    pub(crate) root: &'a Path,
    pub(crate) public_dir: Option<&'a Path>,
    pub(crate) urls: &'a DevAssetUrls,
    pub(crate) env: &'a str,
    pub(crate) stamp: &'a dyn Fn(&Path) -> u64,
}

/// What a resolved url names.
enum Target {
    Public,
    File(PathBuf),
}

impl CssUrls<'_> {
    /// The sheet with its `url()`s and `image-set()` candidates rewritten.
    pub(crate) fn rewrite(&self, css: &str) -> Result<String, String> {
        let css = rewrite_css_urls(css, &mut |url| self.replace(url))?;
        rewrite_css_image_set(&css, &mut |url| self.replace(url))
    }

    /// The files under the root a sheet's urls name: its asset deps (Vite
    /// adds them as watch files), for the module graph.
    pub(crate) fn assets(&self, css: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut collect = |url: &str| {
            if let Some(Target::File(file)) = self.target(url) {
                out.push(file);
            }
            Ok(None)
        };
        let _ = rewrite_css_urls(css, &mut collect);
        let _ = rewrite_css_image_set(css, &mut collect);
        out.sort();
        out.dedup();
        out
    }

    fn target(&self, url: &str) -> Option<Target> {
        let decoded = decode_uri(url);
        let id = decoded.split('#').next().unwrap_or(&decoded);
        let path = id.split('?').next().unwrap_or(id);
        let rel = path.strip_prefix('/').filter(|r| !r.starts_with('/'))?;
        // Vite's checkPublicFile, before any resolution.
        if let Some(public) = self.public_dir {
            let file = crate::rewrite::normalize(&public.join(rel));
            if file.starts_with(public) && file.is_file() {
                return Some(Target::Public);
            }
        }
        let file = match path.strip_prefix("/@fs") {
            Some(abs) if abs.starts_with('/') => PathBuf::from(abs),
            _ => self.root.join(rel),
        };
        file.is_file().then_some(Target::File(file))
    }

    /// Vite's urlResolver: a public file is the base plus its decoded path;
    /// a file the url names is fileToUrl's URL of it, its fragment kept and
    /// stamped `t=` once it hot-updated; anything else is left as written.
    fn replace(&self, url: &str) -> Result<Option<String>, String> {
        let decoded = decode_uri(url);
        match self.target(url) {
            None => Ok(None),
            Some(Target::Public) => Ok(Some(format!("{}{decoded}", self.urls.raw_prefix))),
            Some(Target::File(file)) => {
                let mut named = self.urls.file_url(self.env, &file, &decoded)?;
                let ts = (self.stamp)(&file);
                if ts > 0 && !named.starts_with("data:") {
                    named = inject_query(&named, &format!("t={ts}"));
                }
                Ok(Some(named))
            }
        }
    }
}

/// Vite's injectQuery: `query` first in the url's query, before the hash.
fn inject_query(url: &str, query: &str) -> String {
    let (rest, hash) = match url.find('#') {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    match rest.split_once('?') {
        Some((path, q)) if !q.is_empty() => format!("{path}?{query}&{q}{hash}"),
        Some((path, _)) => format!("{path}?{query}{hash}"),
        None => format!("{rest}?{query}{hash}"),
    }
}

/// A replacer: Some new url, None to keep the url as written.
type Replace<'r> = dyn FnMut(&str) -> Result<Option<String>, String> + 'r;

/// Vite's rewriteCssUrls over cssUrlRE:
/// `(?<!@import\s+)(?<=^|[^\w\-\u0080-\uffff])url\((\s*('[^']+'|"[^"]+")\s*|(?:\\.|[^'")\\])+)\)`,
/// its lookbehinds checked by hand.
fn rewrite_css_urls(css: &str, replace: &mut Replace<'_>) -> Result<String, String> {
    static BODY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^(\s*('[^']+'|"[^"]+")\s*|(?:\\.|[^'")\\])+)\)"#).unwrap());
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(found) = css[from..].find("url(") {
        let at = from + found;
        from = at + 4;
        let before = &css[..at];
        let word_before = before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-' || c as u32 >= 0x80);
        let after_import = {
            let trimmed = before.trim_end();
            trimmed.len() < before.len() && trimmed.ends_with("@import")
        };
        if word_before || after_import {
            continue;
        }
        let Some(body) = BODY.captures(&css[at + 4..]) else {
            continue;
        };
        let end = at + 4 + body.get(0).unwrap().end();
        let matched = &css[at..end];
        let replaced = do_url_replace(body[1].trim(), matched, replace, "url")?;
        out.push_str(&css[last..at]);
        out.push_str(&replaced);
        last = end;
        from = end;
    }
    out.push_str(&css[last..]);
    Ok(out)
}

/// Vite's rewriteCssImageSet (its `[\w-]{1,256}` only caps the scan, and
/// is `[\w-]+` here): each candidate of an `image-set(...)` that is
/// not a `url()` (rewritten above) or another image function is replaced as
/// a url, the candidates joined back as joinSrcset joins them.
fn rewrite_css_image_set(css: &str, replace: &mut Replace<'_>) -> Result<String, String> {
    static IMAGE_SET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"image-set\(((?:[\w-]+\([^)]*\)|[^)])*)\)"#).unwrap());
    static NOT_PROCESSED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:gradient|element|cross-fade|image)\(").unwrap());
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    for caps in IMAGE_SET.captures_iter(css) {
        let inner = caps.get(1).unwrap();
        let mut candidates = Vec::new();
        for (url, descriptor) in parse_srcset(inner.as_str()) {
            let url = if url.starts_with("url(") || NOT_PROCESSED.is_match(&url) {
                url
            } else {
                do_url_replace(&url, &url, replace, "url")?
            };
            candidates.push(match descriptor.is_empty() {
                true => url,
                false => format!("{url} {descriptor}"),
            });
        }
        out.push_str(&css[last..inner.start()]);
        out.push_str(&candidates.join(", "));
        last = inner.end();
    }
    out.push_str(&css[last..]);
    Ok(out)
}

/// Vite's parseSrcset, as an image-set needs it: comma-separated candidates
/// (commas inside a function or a string don't split), each a url (a
/// function, a string, or a bare token) and an optional descriptor.
fn parse_srcset(srcs: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0usize, None::<char>, 0usize);
    for (i, c) in srcs.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                parts.push(&srcs[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&srcs[start..]);
    parts
        .into_iter()
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let end = match part.chars().next()? {
                q @ ('"' | '\'') => part[1..].find(q).map_or(part.len(), |i| i + 2),
                _ if part.contains('(')
                    && !part[..part.find('(').unwrap()].contains(char::is_whitespace) =>
                {
                    part.find(')').map_or(part.len(), |i| i + 1)
                }
                _ => part.find(char::is_whitespace).unwrap_or(part.len()),
            };
            let words = part[end..].split_whitespace().collect::<Vec<_>>().join(" ");
            Some((part[..end].to_string(), words))
        })
        .collect()
}

/// Vite's doUrlReplace: the url unquoted and unescaped, skipped as
/// skipUrlReplacer skips it, then wrapped as the original was, in double
/// quotes when the new one needs them.
fn do_url_replace(
    raw_url: &str,
    matched: &str,
    replace: &mut Replace<'_>,
    func: &str,
) -> Result<String, String> {
    static UNESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\(\W)").unwrap());
    static FUNCTION_CALL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^[A-Z_][.\w-]*\(").unwrap());
    static EXTERNAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-z]+:)?//").unwrap());
    static DATA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*data:").unwrap());
    let mut wrap = "";
    let mut unquoted = raw_url;
    if let Some(first @ ('"' | '\'')) = raw_url.chars().next() {
        wrap = if first == '"' { "\"" } else { "'" };
        unquoted = &raw_url[1..raw_url.len().max(2) - 1];
    }
    if EXTERNAL.is_match(unquoted)
        || DATA.is_match(unquoted)
        || unquoted.starts_with('#')
        || FUNCTION_CALL.is_match(unquoted)
        || unquoted.starts_with("__VITE_ASSET__")
        || unquoted.starts_with("__VITE_PUBLIC_ASSET__")
    {
        return Ok(matched.to_string());
    }
    let unescaped = UNESCAPE.replace_all(unquoted, "$1");
    let Some(mut new_url) = replace(&unescaped)? else {
        return Ok(matched.to_string());
    };
    if wrap.is_empty() && (new_url != encode_uri(&new_url) || new_url.contains(')')) {
        wrap = "\"";
    }
    if wrap == "'" && new_url.contains('\'') {
        wrap = "\"";
    }
    if wrap == "\"" && new_url.contains('"') {
        new_url = escape_double_quotes(&new_url);
    }
    Ok(format!("{func}({wrap}{new_url}{wrap})"))
}

/// JS `encodeURI`.
fn encode_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b";,/?:@&=+$-_.!~*'()#".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Vite's nonEscapedDoubleQuoteRe replace: `"` not after a backslash as `\"`.
fn escape_double_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = '\0';
    for c in s.chars() {
        if c == '"' && prev != '\\' {
            out.push('\\');
        }
        out.push(c);
        prev = c;
    }
    out
}
