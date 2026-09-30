//! `url()` / `@import` rewriting through lightningcss dependency placeholders.

use std::collections::HashMap;
use std::path::Path;

use lightningcss::dependencies::{Dependency, DependencyOptions};
use lightningcss::printer::PrinterOptions;

use crate::compile::parse_stylesheet;
use crate::path::{is_external, posix_join, relative_path, split_query};
use crate::resolve::CssResolve;

/// Rewrite the relative urls of a stylesheet in `from_dir` so they are
/// correct from `to_dir`, where it is being inlined.
pub(crate) fn rebase_to_dir(
    css: &str,
    file: &Path,
    from_dir: &Path,
    to_dir: &Path,
    resolve: &CssResolve<'_>,
) -> Result<String, String> {
    if !(css.contains("url(") || css.contains("@import")) || from_dir == to_dir {
        return Ok(css.to_string());
    }
    let name = file.to_string_lossy();
    let result = parse_stylesheet(css, &name, None)?
        .to_css(PrinterOptions {
            analyze_dependencies: Some(DependencyOptions::default()),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {name}: {err}"))?;
    let deps = result.dependencies.unwrap_or_default();
    Ok(rewrite_dependencies(result.code, deps, |url| {
        // Aliased specs keep their spelling for the entry's own resolution.
        if rebase_relative(&url, "/x").is_none() || resolve.alias_spec(&url).is_some() {
            return url;
        }
        let (path, suffix) = split_query(&url);
        format!("{}{suffix}", relative_path(to_dir, &from_dir.join(path)))
    }))
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
}
