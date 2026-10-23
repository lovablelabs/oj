// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's finalizeCss for a CSS asset: a chunk's module stylesheets,
//! concatenated, with `@import` and `@charset` hoisted to the top, minified
//! as one sheet under `build.cssMinify`, and ending in a linebreak (Vite adds
//! the one lightningcss leaves out, so every emitted asset ends with one).

use crate::{minify_css, CssResolve};

/// The finished asset text for a chunk's concatenated CSS.
pub fn finalize_chunk_css(css: &str, resolve: &CssResolve<'_>) -> Result<String, String> {
    let css = if css.contains("@import") || css.contains("@charset") {
        hoist_at_rules(css)
    } else {
        css.to_string()
    };
    if !resolve.minify {
        return Ok(css);
    }
    let mut out = minify_css("style.css", &css, resolve.targets, resolve.lightning)?;
    out.push('\n');
    Ok(out)
}

/// Vite's hoistAtRules: every `@import` moves to the top in its original
/// order (CSS takes them only before other rules, and a concatenation has
/// them mid-sheet), then the first `@charset` goes first and the others go.
pub fn hoist_at_rules(css: &str) -> String {
    let clean = blank_comments(css);
    let imports = statements(&clean, "@import");
    let charsets = statements(&clean, "@charset");
    let mut cut: Vec<(usize, usize)> = imports.iter().chain(&charsets).copied().collect();
    cut.sort_unstable();
    let mut out = String::with_capacity(css.len());
    if let Some(&(start, end)) = charsets.first() {
        out.push_str(&css[start..end]);
    }
    for &(start, end) in &imports {
        out.push_str(&css[start..end]);
    }
    let mut last = 0;
    for (start, end) in cut {
        out.push_str(&css[last..start]);
        last = end;
    }
    out.push_str(&css[last..]);
    out
}

/// `css` with each `/* */` comment blanked to spaces of the same length
/// (Vite's emptyCssComments), so offsets into it are offsets into `css`.
fn blank_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let body = &rest[start..];
        let end = body[2..].find("*/").map_or(body.len(), |e| e + 4);
        for c in body[..end].chars() {
            match c {
                '\n' => out.push('\n'),
                c => out.extend(std::iter::repeat_n(' ', c.len_utf8())),
            }
        }
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}

/// The byte spans of each `keyword` statement in `clean`, through its `;`:
/// one inside quotes or `url(...)` does not end it (Vite's atImportRE).
fn statements(clean: &str, keyword: &str) -> Vec<(usize, usize)> {
    let bytes = clean.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(found) = clean[from..].find(keyword) {
        let start = from + found;
        let mut i = start + keyword.len();
        let mut quote: Option<u8> = None;
        let mut parens = 0usize;
        let mut end = None;
        while i < bytes.len() {
            let b = bytes[i];
            match quote {
                Some(_) if b == b'\\' => i += 1,
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None => match b {
                    b'"' | b'\'' => quote = Some(b),
                    b'(' => parens += 1,
                    b')' => parens = parens.saturating_sub(1),
                    b';' if parens == 0 => {
                        end = Some(i + 1);
                        break;
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        match end {
            Some(end) => {
                spans.push((start, end));
                from = end;
            }
            None => break,
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CssModulesOptions, LightningOptions};

    fn resolve<'a>(
        minify: bool,
        modules: &'a CssModulesOptions,
        lightning: &'a LightningOptions,
    ) -> CssResolve<'a> {
        CssResolve {
            root: None,
            public_dir: None,
            alias: &[],
            targets: &[],
            minify,
            production: true,
            modules,
            lightning,
        }
    }

    #[test]
    fn a_minified_chunk_is_one_sheet_ending_in_a_linebreak() {
        let (m, l) = (CssModulesOptions::default(), LightningOptions::default());
        // Two modules' sheets: minified together, the repeated rule merges.
        let css = "body{margin:0}.a{color:red}.b{color:#00f}.a{color:red}";
        assert_eq!(
            finalize_chunk_css(css, &resolve(true, &m, &l)).unwrap(),
            "body{margin:0}.b{color:#00f}.a{color:red}\n"
        );
        assert_eq!(
            finalize_chunk_css(css, &resolve(false, &m, &l)).unwrap(),
            css,
            "no cssMinify, no rewrite"
        );
    }

    #[test]
    fn imports_and_the_first_charset_hoist_like_vite() {
        let css =
            ".a{}\n@charset \"utf-8\";\n@import url(\"x;y.css\");\n/* @import \"no.css\"; ü */\
                   .b{}\n@import 'z.css';\n@charset \"latin1\";\n";
        assert_eq!(
            hoist_at_rules(css),
            "@charset \"utf-8\";@import url(\"x;y.css\");@import 'z.css';.a{}\n\n\n\
             /* @import \"no.css\"; ü */.b{}\n\n\n"
        );
    }
}
