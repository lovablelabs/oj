//! `url()` / `@import` rewriting: lightningcss placeholders for a compile,
//! Vite's text rewrite for inlined imports.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use lightningcss::dependencies::Dependency;
use regex::Regex;

use crate::path::{is_external, join_relative, posix_join, relative_path, split_query};
use crate::resolve::CssResolve;

/// Rewrite the relative urls of a stylesheet in `from_dir` so they are
/// correct from `to_dir`, where it is being inlined. Text-based like Vite's
/// `rebaseUrls`: `url()`, `@import "x.css"` and `image-set()` strings.
pub(crate) fn rebase_to_dir(
    css: &str,
    from_dir: &Path,
    to_dir: &Path,
    resolve: &CssResolve<'_>,
) -> String {
    if from_dir == to_dir {
        return css.to_string();
    }
    let base = relative_path(to_dir, from_dir);
    let rebase = |url: &str| -> Option<String> {
        // Aliased specs keep their spelling for the entry's own resolution.
        if rebase_relative(url, "/x").is_none() || resolve.alias_spec(url).is_some() {
            return None;
        }
        let (path, suffix) = split_query(url);
        Some(format!("{}{suffix}", join_relative(&base, path)))
    };
    let mut out = css.to_string();
    if out.contains("@import") {
        out = rewrite_import_css(&out, &rebase);
    }
    if out.contains("url(") {
        out = rewrite_css_urls(&out, &rebase);
    }
    if out.contains("image-set(") {
        out = rewrite_image_set_strings(&out, &rebase);
    }
    out
}

type Rebase<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Vite's `cssUrlRE` minus its lookbehinds, which are checked by hand.
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"url\((\s*('[^']+'|"[^"]+")\s*|(?:\\.|[^'")\\])+)\)"#).unwrap());
/// Vite's `importCssRE`.
static IMPORT_CSS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"@import\s+(?:url\()?('[^']+\.css'|"[^"]+\.css"|[^'"\s)]+\.css)"#).unwrap()
});
/// Vite's `cssImageSetRE` (its `{1,256}` guards JS backtracking; this engine is linear).
static IMAGE_SET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"image-set\(((?:[\w-]+\([^)]*\)|[^)])*)\)"#).unwrap());

/// `css` with each match of `re` replaced by `f(match, match start)` (None
/// keeps it). Plain matches, no capture groups: those need the slower engine.
fn replace_matches(
    css: &str,
    re: &Regex,
    mut f: impl FnMut(&str, usize) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(css.len());
    let mut copied = 0;
    for m in re.find_iter(css) {
        if let Some(replacement) = f(m.as_str(), m.start()) {
            out.push_str(&css[copied..m.start()]);
            out.push_str(&replacement);
            copied = m.end();
        }
    }
    out.push_str(&css[copied..]);
    out
}

fn rewrite_css_urls(css: &str, rebase: &Rebase<'_>) -> String {
    replace_matches(css, &URL_RE, |m, start| {
        let before = &css[..start];
        // `(?<=^|[^\w\-\u0080-\uffff])`: not the tail of a longer name.
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii();
        if before.chars().next_back().is_some_and(ident) {
            return None;
        }
        // `(?<!@import\s+)`: an import's url is the import rewrite's business.
        let trimmed = before.trim_end();
        if trimmed.len() < before.len() && trimmed.ends_with("@import") {
            return None;
        }
        let inner = &m["url(".len()..m.len() - 1];
        url_replace(inner.trim(), rebase).map(|(wrap, url)| format!("url({wrap}{url}{wrap})"))
    })
}

fn rewrite_import_css(css: &str, rebase: &Rebase<'_>) -> String {
    replace_matches(css, &IMPORT_CSS_RE, |m, _| {
        let spec = m["@import".len()..].trim_start();
        let (prefix, spec) = match spec.strip_prefix("url(") {
            Some(rest) => ("url(", rest),
            None => ("", spec),
        };
        let (wrap, unquoted) = unquote(spec);
        if skip_url(unquoted) {
            return None;
        }
        let url = rebase(unquoted)?;
        Some(format!("@import {prefix}{wrap}{url}{wrap}"))
    })
}

/// The bare `"x" 1x` candidates of `image-set()`; its `url()` ones were
/// already rewritten.
fn rewrite_image_set_strings(css: &str, rebase: &Rebase<'_>) -> String {
    replace_matches(css, &IMAGE_SET_RE, |m, _| {
        let body = &m["image-set(".len()..m.len() - 1];
        let mut out = String::with_capacity(body.len());
        let mut depth = 0usize;
        let mut chars = body.char_indices();
        let mut changed = false;
        while let Some((i, c)) = chars.next() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                '"' | '\'' if depth == 0 => {
                    if let Some(close) = body[i + 1..].find(c) {
                        let raw = &body[i..i + close + 2];
                        if let Some((wrap, url)) = url_replace(raw, rebase) {
                            let wrap = if wrap.is_empty() { "\"" } else { wrap };
                            out.push_str(&format!("{wrap}{url}{wrap}"));
                            changed = true;
                        } else {
                            out.push_str(raw);
                        }
                        for _ in 0..raw.chars().count() - 1 {
                            chars.next();
                        }
                        continue;
                    }
                }
                _ => {}
            }
            out.push(c);
        }
        changed.then(|| format!("image-set({out})"))
    })
}

/// Vite's `doUrlReplace`: the new url and the quote to wrap it in, or None to
/// keep the original.
fn url_replace(raw: &str, rebase: &Rebase<'_>) -> Option<(&'static str, String)> {
    let (wrap, unquoted) = unquote(raw);
    if skip_url(unquoted) {
        return None;
    }
    let mut url = rebase(&unescape(unquoted))?;
    let mut wrap = wrap;
    if wrap.is_empty() && (needs_uri_encoding(&url) || url.contains(')')) {
        wrap = "\"";
    }
    if wrap == "'" && url.contains('\'') {
        wrap = "\"";
    }
    if wrap == "\"" && url.contains('"') {
        url = escape_double_quotes(&url);
    }
    Some((wrap, url))
}

fn unquote(raw: &str) -> (&'static str, &str) {
    match raw.as_bytes() {
        [b'"', .., b'"'] if raw.len() >= 2 => ("\"", &raw[1..raw.len() - 1]),
        [b'\'', .., b'\''] if raw.len() >= 2 => ("'", &raw[1..raw.len() - 1]),
        _ => ("", raw),
    }
}

/// Vite's `skipUrlReplacer`: external, data, `#fragment` and `fn(...)` urls.
fn skip_url(url: &str) -> bool {
    let function_call = url.find('(').is_some_and(|i| {
        let name = &url[..i];
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
    });
    is_external(url)
        || url.trim_start().starts_with("data:")
        || url.starts_with('#')
        || function_call
}

/// `\(\W)` -> `$1`.
fn unescape(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut chars = url.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(&n) if c == '\\' && !(n.is_alphanumeric() || n == '_') => {
                out.push(n);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether `encodeURI` would change `url`.
fn needs_uri_encoding(url: &str) -> bool {
    !url.chars()
        .all(|c| c.is_ascii_alphanumeric() || ";,/?:@&=+$-_.!~*'()#".contains(c))
}

/// `"` not already escaped, escaped.
fn escape_double_quotes(url: &str) -> String {
    let mut out = String::with_capacity(url.len() + 2);
    let mut prev = '\0';
    for c in url.chars() {
        if c == '"' && prev != '\\' {
            out.push('\\');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// Swap every printed placeholder for `rewrite(original url)`.
pub(crate) fn rewrite_dependencies(
    code: String,
    deps: Vec<Dependency>,
    mut rewrite: impl FnMut(String) -> String,
) -> String {
    let pairs: Vec<(String, String)> = deps
        .into_iter()
        .map(|dep| {
            let (placeholder, url) = match dep {
                Dependency::Url(u) => (u.placeholder, u.url),
                Dependency::Import(i) => (i.placeholder, i.url),
            };
            (placeholder, rewrite(url))
        })
        .collect();
    substitute_placeholders(code, &pairs)
}

/// The url directory a server url lives in (`/src/app.css` -> `/src`).
pub(crate) fn css_base_dir(url: &str) -> Option<String> {
    let path = split_query(url).0;
    if !path.starts_with('/') {
        return None;
    }
    let i = path.rfind('/')?;
    Some(if i == 0 { "/".into() } else { path[..i].into() })
}

/// The server url a spec in a stylesheet served from `base_dir` stands for:
/// an alias becomes the file's url, a relative path joins `base_dir`, the
/// rest (root-absolute, external, data:) is kept.
pub(crate) fn dev_url_of(spec: &str, base_dir: &str, resolve: &CssResolve<'_>) -> String {
    let (path, suffix) = split_query(spec);
    if let Some(file) = resolve.alias_path(path) {
        return format!("{}{suffix}", resolve.dev_url(&file));
    }
    rebase_relative(spec, base_dir).unwrap_or_else(|| spec.to_string())
}

/// A relative spec joined onto `base_dir`; None for anything not relative.
fn rebase_relative(spec: &str, base_dir: &str) -> Option<String> {
    if spec.is_empty() || spec.starts_with(['/', '#']) || is_external(spec) {
        return None;
    }
    let (path, suffix) = split_query(spec);
    Some(format!("{}{suffix}", posix_join(base_dir, path)))
}

/// Replace dependency placeholders in one left-to-right pass. The printer
/// always emits a placeholder quoted (`url("PH")`, `@import "PH"`, `"PH" 1x`
/// in `image-set()`), so only text between a `"` and the next is looked up.
/// The first replacement for a repeated placeholder wins; replacements are
/// not re-scanned.
pub(crate) fn substitute_placeholders(code: String, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return code;
    }
    let mut map: HashMap<&str, &str> = HashMap::with_capacity(pairs.len());
    for (p, r) in pairs {
        if !p.is_empty() {
            map.entry(p.as_str()).or_insert(r.as_str());
        }
    }
    let extra: usize = pairs
        .iter()
        .map(|(p, r)| r.len().saturating_sub(p.len()))
        .sum();
    let mut out = String::with_capacity(code.len() + extra);
    let mut copied = 0; // start of the text not yet appended to `out`
    let mut scan = 0; // where the next `"` search begins
    while let Some(q) = code[scan..].find('"') {
        let start = scan + q + 1;
        let Some(n) = code[start..].find('"') else {
            break;
        };
        let end = start + n;
        if let Some(r) = map.get(&code[start..end]) {
            out.push_str(&code[copied..start]);
            out.push_str(r);
            copied = end;
            scan = end + 1;
        } else {
            // The closing quote may open the next token.
            scan = start;
        }
    }
    out.push_str(&code[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use lightningcss::dependencies::DependencyOptions;
    use lightningcss::printer::PrinterOptions;
    use lightningcss::stylesheet::{ParserOptions, StyleSheet};

    /// The sequential `str::replace` loop `substitute_placeholders` replaced.
    fn sequential_replace(code: &str, pairs: &[(String, String)]) -> String {
        pairs
            .iter()
            .fold(code.to_string(), |c, (p, r)| c.replace(p, r))
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .collect()
    }

    /// Every `"` followed by exactly six placeholder-alphabet bytes and a closing
    /// `"`: the shape of an unsubstituted lightningcss dependency placeholder
    /// (and of a six-character user string, which callers exclude by value).
    fn quoted_six_char_tokens(css: &str) -> Vec<&str> {
        let b = css.as_bytes();
        (0..b.len())
            .filter(|&i| {
                b[i] == b'"'
                    && i + 7 < b.len()
                    && b[i + 7] == b'"'
                    && b[i + 1..i + 7]
                        .iter()
                        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            })
            .map(|i| &css[i + 1..i + 7])
            .collect()
    }

    #[test]
    fn substitute_placeholders_handles_edge_shapes() {
        // Empty pairs: the input comes back as-is, without a copy.
        let input = ".a{content:\"abcdef\"}".to_string();
        let (ptr, cap) = (input.as_ptr(), input.capacity());
        let same = substitute_placeholders(input, &[]);
        assert_eq!((same.as_ptr(), same.capacity()), (ptr, cap));
        assert_eq!(same, ".a{content:\"abcdef\"}");

        let cases = vec![
            ("", pairs(&[("abcdef", "/a.png")]), ""),
            (
                ".a{color:red}",
                pairs(&[("abcdef", "/a.png")]),
                ".a{color:red}",
            ),
            // Placeholder at byte 0 and as the final quoted token.
            (
                "\"abcdef\" .a{x:1}\"abcdef\"",
                pairs(&[("abcdef", "/a.png")]),
                "\"/a.png\" .a{x:1}\"/a.png\"",
            ),
            // Duplicate placeholder with two replacements: first wins, both occurrences replaced.
            (
                ".a{background:url(\"abcdef\")}.b{background:url(\"abcdef\")}",
                pairs(&[("abcdef", "/first.png"), ("abcdef", "/second.png")]),
                ".a{background:url(\"/first.png\")}.b{background:url(\"/first.png\")}",
            ),
            // Zero-occurrence placeholder and a same-length user string.
            (
                ".a{content:\"qqqqqq\";background:url(\"abcdef\")}",
                pairs(&[("zzzzzz", "/z.png"), ("abcdef", "/a.png")]),
                ".a{content:\"qqqqqq\";background:url(\"/a.png\")}",
            ),
            // Escaped quote inside a user string before a placeholder.
            (
                ".a{content:\"a\\\"b\";background:url(\"Ab-_09\")}",
                pairs(&[("Ab-_09", "/x/y.svg")]),
                ".a{content:\"a\\\"b\";background:url(\"/x/y.svg\")}",
            ),
            // Adjacent quoted tokens.
            (
                "\"P1P1P1\"\"P2P2P2\"",
                pairs(&[("P1P1P1", "/1"), ("P2P2P2", "/2")]),
                "\"/1\"\"/2\"",
            ),
            // Multibyte neighbours and a multibyte replacement.
            (
                ".a::before{content:\"héllo\";background:url(\"Ab-_09\")}",
                pairs(&[("Ab-_09", "/ünï/y.svg")]),
                ".a::before{content:\"héllo\";background:url(\"/ünï/y.svg\")}",
            ),
            // Empty replacement.
            (
                ".a{x:url(\"abcdef\")}",
                pairs(&[("abcdef", "")]),
                ".a{x:url(\"\")}",
            ),
        ];
        for (code, pairs, want) in cases {
            let got = substitute_placeholders(code.to_string(), &pairs);
            assert_eq!(got, want, "input {code:?}");
            assert_eq!(
                got,
                sequential_replace(code, &pairs),
                "diverges from the sequential loop on {code:?}"
            );
        }

        // An unterminated `"PH` at the end of the input is left as written.
        let code = ".a{background:url(\"abcdef\")}\"abcdef";
        let got = substitute_placeholders(code.to_string(), &pairs(&[("abcdef", "/a.png")]));
        assert_eq!(got, ".a{background:url(\"/a.png\")}\"abcdef");
    }

    #[test]
    fn substitute_placeholders_matches_sequential_replace_on_printed_css() {
        let src = "@import \"./x.css\";\n\
                   @font-face { font-family: F; src: url(\"./f.woff2\") format(\"woff2\"); }\n\
                   .a { background: url(./a.png); }\n\
                   .b { background: url(\"./b.png\"); }\n\
                   .c { background: url(./a.png); }\n\
                   .d { background: image-set(\"./a.png\" 1x, \"./b.png\" 2x); }\n\
                   .e { background: -webkit-image-set(url(\"./c.png\") 1x, url(\"./d.png\") 2x); }\n\
                   .f::before { content: \"abcdef\"; }\n\
                   .g::before { content: \"a\\\"b\"; }\n\
                   .h { background: url(\"data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg'/%3e\"); }\n";
        let stylesheet = StyleSheet::parse(
            src,
            ParserOptions {
                filename: "/src/x.css".into(),
                ..ParserOptions::default()
            },
        )
        .unwrap();
        let result = stylesheet
            .to_css(PrinterOptions {
                analyze_dependencies: Some(DependencyOptions::default()),
                ..PrinterOptions::default()
            })
            .unwrap();
        let deps = result.dependencies.unwrap();
        let pairs: Vec<(String, String)> = deps
            .into_iter()
            .map(|dep| match dep {
                Dependency::Url(u) => (u.placeholder, format!("/src/{}", u.url)),
                Dependency::Import(i) => (i.placeholder, format!("/src/{}", i.url)),
            })
            .collect();
        assert!(
            pairs.len() >= 9,
            "one Dependency per printed url/import: {pairs:?}"
        );
        assert!(pairs.iter().all(|(p, _)| p.len() == 6), "{pairs:?}");
        // One quoted placeholder per Dependency, plus the user string.
        assert_eq!(
            quoted_six_char_tokens(&result.code).len(),
            pairs.len() + 1,
            "{}",
            result.code
        );

        let got = substitute_placeholders(result.code.clone(), &pairs);
        assert_eq!(got, sequential_replace(&result.code, &pairs));
        // Only the same-width user string survives as a six-char quoted token.
        assert_eq!(quoted_six_char_tokens(&got), vec!["abcdef"], "{got}");
        assert!(
            got.contains("/src/./a.png")
                && got.contains("/src/./x.css")
                && got.contains("/src/./f.woff2"),
            "{got}"
        );
        assert!(
            got.contains("/src/data:image/svg+xml"),
            "the test maps every dependency, data: included: {got}"
        );
    }

    #[test]
    fn dev_compile_rewrites_a_url_used_twice_and_an_import() {
        // Distinct declarations keep the two rules from being merged by minify.
        let src = "@import \"./b.css\";\n\
                   .a { background: url(./a.png); color: red; }\n\
                   .b { background: url(\"./a.png\"); color: blue; }\n";
        let out = compile_css_dev("/src/x.css", src, false, &CssResolve::default())
            .unwrap()
            .css;
        assert_eq!(
            out.matches("url(\"/src/a.png\")").count(),
            2,
            "both occurrences rewritten: {out}"
        );
        assert!(out.contains("@import \"/src/b.css\""), "{out}");
        assert!(
            quoted_six_char_tokens(&out).is_empty(),
            "a placeholder survived substitution: {out}"
        );
    }

    #[test]
    fn rebase_to_dir_rewrites_like_vite_rebase_urls() {
        let resolve = CssResolveConfig {
            alias: vec![("@".into(), "/abs/src".into())],
            ..Default::default()
        };
        let rebase = |css: &str| {
            rebase_to_dir(
                css,
                Path::new("/p/src/base"),
                Path::new("/p/src"),
                &resolve.as_ref(),
            )
        };
        assert_eq!(
            rebase(".a{background:url(./x.png)}"),
            ".a{background:url(./base/x.png)}"
        );
        assert_eq!(
            rebase(".a{background:url( 'x.png?v=1#h' )}"),
            ".a{background:url('./base/x.png?v=1#h')}"
        );
        assert_eq!(
            rebase(r#".a{background:url("../up.png")}"#),
            r#".a{background:url("./up.png")}"#
        );
        // A space in the new url needs quotes.
        assert_eq!(
            rebase(r".a{background:url(my\ file.png)}"),
            r#".a{background:url("./base/my file.png")}"#
        );
        // Not a `url(` token, external, data, fragments, functions, aliases, root-absolute.
        for kept in [
            ".a{b:myurl(x.png)}",
            ".a{b:url(https://cdn.test/x.png)}",
            ".a{b:url(//cdn.test/x.png)}",
            ".a{b:url(data:image/png;base64,AAAA)}",
            ".a{b:url(#grad)}",
            ".a{b:url(var(--x))}",
            ".a{b:url(@/x.png)}",
            ".a{b:url(/x.png)}",
        ] {
            assert_eq!(rebase(kept), kept);
        }
        // `@import` urls go through the import rewrite only (`.css` specs).
        assert_eq!(
            rebase("@import url(./a.css) print;"),
            "@import url(./base/a.css) print;"
        );
        assert_eq!(rebase("@import \"./a.css\";"), "@import \"./base/a.css\";");
        assert_eq!(
            rebase("@import url(https://x.test/a.css);"),
            "@import url(https://x.test/a.css);"
        );
        // image-set: bare strings and url() candidates, each rewritten once.
        assert_eq!(
            rebase(r#".a{b:image-set("x.png" 1x, url(y.png) 2x, "z.png" type("image/png"))}"#),
            r#".a{b:image-set("./base/x.png" 1x, url(./base/y.png) 2x, "./base/z.png" type("image/png"))}"#
        );
        assert_eq!(
            rebase(".a{b:url(x.png)}").len(),
            ".a{b:url(./base/x.png)}".len()
        );
        // Same directory: untouched.
        let same = rebase_to_dir(
            ".a{b:url(x.png)}",
            Path::new("/p"),
            Path::new("/p"),
            &resolve.as_ref(),
        );
        assert_eq!(same, ".a{b:url(x.png)}");
    }
}
