// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's build-html page output (html.ts): the bundled module scripts and
//! stylesheet links leave the page, and the entry `<script>`, its
//! modulepreload and stylesheet `<link>`s go back in through `injectToHead`,
//! serialized as Vite writes them.

use std::sync::LazyLock;

use oj_compiler::html::html_attr;
use regex::Regex;

/// A tag Vite injects (`HtmlTagDescriptor`); an attribute without a value is
/// a `true` boolean and is written bare.
pub(super) struct Tag {
    name: &'static str,
    attrs: Vec<(&'static str, Option<String>)>,
}

impl Tag {
    /// `toScriptTag`.
    pub(super) fn script(src: String, is_async: bool) -> Self {
        let mut attrs = Vec::with_capacity(4);
        if is_async {
            attrs.push(("async", None));
        }
        attrs.extend([
            ("type", Some("module".to_string())),
            ("crossorigin", None),
            ("src", Some(src)),
        ]);
        Tag {
            name: "script",
            attrs,
        }
    }

    /// `toPreloadTag`.
    pub(super) fn modulepreload(href: String) -> Self {
        Self::link("modulepreload", href)
    }

    /// `toStyleSheetLinkTag`.
    pub(super) fn stylesheet(href: String) -> Self {
        Self::link("stylesheet", href)
    }

    fn link(rel: &str, href: String) -> Self {
        Tag {
            name: "link",
            attrs: vec![
                ("rel", Some(rel.to_string())),
                ("crossorigin", None),
                ("href", Some(href)),
            ],
        }
    }
}

/// `serializeTags`: one tag per line, each after `indent`.
fn serialize_tags(tags: &[Tag], indent: &str) -> String {
    let mut out = String::new();
    for tag in tags {
        out.push_str(indent);
        out.push('<');
        out.push_str(tag.name);
        for (key, value) in &tag.attrs {
            out.push(' ');
            out.push_str(key);
            if let Some(value) = value {
                out.push_str("=\"");
                out.push_str(&escape_html(value));
                out.push('"');
            }
        }
        out.push('>');
        // `link`, `meta` and `base` are Vite's unary tags.
        if tag.name != "link" {
            out.push_str("</");
            out.push_str(tag.name);
            out.push('>');
        }
        out.push('\n');
    }
    out
}

/// The `escape-html` package.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// `incrementIndent`.
fn increment_indent(indent: &str) -> String {
    let step = if indent.starts_with('\t') { "\t" } else { "  " };
    format!("{indent}{step}")
}

static HEAD_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([ \t]*)</head>").unwrap());
static BODY_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)<body[^>]*>").unwrap());
static HTML_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([ \t]*)<html[^>]*>").unwrap());
static DOCTYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<!doctype html>").unwrap());

/// `injectToHead` (not prepending): before `</head>`, else before `<body>`,
/// else after `<html>` or the doctype, else at the very start.
pub(super) fn inject_to_head(html: &str, tags: &[Tag]) -> String {
    if tags.is_empty() {
        return html.to_string();
    }
    let splice = |range: std::ops::Range<usize>, with: String| {
        format!("{}{with}{}", &html[..range.start], &html[range.end..])
    };
    if let Some(m) = HEAD_END.captures(html) {
        let (all, indent) = (m.get(0).unwrap(), &m[1]);
        let tags = serialize_tags(tags, &increment_indent(indent));
        return splice(all.range(), format!("{tags}{}", all.as_str()));
    }
    if let Some(m) = BODY_START.captures(html) {
        let (all, indent) = (m.get(0).unwrap(), &m[1]);
        let tags = serialize_tags(tags, indent);
        return splice(all.range(), format!("{tags}\n{}", all.as_str()));
    }
    for re in [&*HTML_START, &*DOCTYPE] {
        if let Some(all) = re.find(html) {
            let tags = serialize_tags(tags, "");
            return splice(all.range(), format!("{}\n{tags}", all.as_str()));
        }
    }
    format!("{}{html}", serialize_tags(tags, ""))
}

/// A page's module script (its `src`) or bundled stylesheet link (its `href`).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PageRef {
    Script { src: String, is_async: bool },
    Style(String),
}

/// The module scripts with a `src` and the local stylesheet links Vite
/// bundles (`rel="stylesheet"`, a CSS request, no `media` or `disabled`), in
/// document order: the page's entry imports them in this order.
pub(super) fn page_refs(html: &str, is_css_request: impl Fn(&str) -> bool) -> Vec<PageRef> {
    let mut out = Vec::new();
    for (_, tag, name) in tags(html) {
        if name.eq_ignore_ascii_case("script") {
            if !html_attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("module"))
                || has_attr(tag, "vite-ignore")
            {
                continue;
            }
            if let Some(src) = html_attr(tag, "src").filter(|s| !is_excluded_url(s)) {
                out.push(PageRef::Script {
                    src: src.to_string(),
                    is_async: has_attr(tag, "async"),
                });
            }
        } else if let Some(href) = bundled_stylesheet(tag, &is_css_request) {
            out.push(PageRef::Style(href.to_string()));
        }
    }
    out
}

/// `html` without the elements of `refs` (Vite's transform removes them, with
/// the whitespace before them when it is all that precedes them on the line,
/// the line break included).
pub(super) fn strip_page_refs(html: &str, refs: &[PageRef]) -> String {
    let scripts: Vec<&str> = refs
        .iter()
        .filter_map(|r| match r {
            PageRef::Script { src, .. } => Some(src.as_str()),
            PageRef::Style(_) => None,
        })
        .collect();
    let styles: Vec<&str> = refs
        .iter()
        .filter_map(|r| match r {
            PageRef::Style(href) => Some(href.as_str()),
            PageRef::Script { .. } => None,
        })
        .collect();
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    for (start, tag, name) in tags(html) {
        if start < last {
            continue;
        }
        let tag_end = start + tag.len() + 1;
        let end = if name.eq_ignore_ascii_case("script") {
            let module = html_attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("module"));
            if !module || !html_attr(tag, "src").is_some_and(|s| scripts.contains(&s)) {
                continue;
            }
            find_ci(&html[tag_end..], "</script>").map_or(tag_end, |e| tag_end + e + 9)
        } else if bundled_stylesheet(tag, &|_: &str| true).is_some_and(|h| styles.contains(&h)) {
            tag_end
        } else {
            continue;
        };
        out.push_str(&html[last..line_start_if_blank(html, start)]);
        last = end;
    }
    out.push_str(&html[last..]);
    out
}

/// Vite's `nodeStartWithLeadingWhitespace`: the element's start, or the line
/// break before it when only whitespace sits between the two.
fn line_start_if_blank(html: &str, start: usize) -> usize {
    let line = html[..start].rfind('\n').unwrap_or(0);
    if html[line..start].trim().is_empty() {
        line
    } else {
        start
    }
}

/// `<script` and `<link` tags: their offset, the text between `<` and `>`,
/// and the tag name.
fn tags(html: &str) -> impl Iterator<Item = (usize, &str, &str)> {
    html.match_indices('<').filter_map(move |(start, _)| {
        let rest = &html[start + 1..];
        let name_len = rest
            .bytes()
            .position(|b| !b.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let name = &rest[..name_len];
        if !name.eq_ignore_ascii_case("script") && !name.eq_ignore_ascii_case("link") {
            return None;
        }
        let end = rest.find('>')?;
        Some((start, &html[start..start + 1 + end], name))
    })
}

fn bundled_stylesheet<'a>(tag: &'a str, is_css_request: &dyn Fn(&str) -> bool) -> Option<&'a str> {
    if !html_attr(tag, "rel").is_some_and(|r| r.eq_ignore_ascii_case("stylesheet"))
        || has_attr(tag, "media")
        || has_attr(tag, "disabled")
    {
        return None;
    }
    html_attr(tag, "href").filter(|h| !is_excluded_url(h) && is_css_request(h))
}

/// Vite's `isExcludedUrl`: a fragment, an external or a data URL.
fn is_excluded_url(url: &str) -> bool {
    url.starts_with('#')
        || url.starts_with("//")
        || url.starts_with("data:")
        || url
            .split_once("://")
            .is_some_and(|(scheme, _)| !scheme.is_empty() && !scheme.contains('/'))
}

/// An attribute present with or without a value.
fn has_attr(tag: &str, name: &str) -> bool {
    tag.split(|c: char| c.is_ascii_whitespace() || c == '/')
        .skip(1)
        .any(|tok| {
            let key = tok.split('=').next().unwrap_or("");
            key.eq_ignore_ascii_case(name)
        })
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> Vec<Tag> {
        vec![
            Tag::script("/assets/index-a.js".into(), false),
            Tag::modulepreload("/assets/dep-b.js".into()),
            Tag::stylesheet("/assets/index-c.css".into()),
        ]
    }

    #[test]
    fn tags_go_before_head_one_per_line_indented_one_step_deeper() {
        let page = "<html>\n  <head>\n    <title>t</title>\n  </head>\n</html>\n";
        assert_eq!(
            inject_to_head(page, &tags()),
            "<html>\n  <head>\n    <title>t</title>\n    \
             <script type=\"module\" crossorigin src=\"/assets/index-a.js\"></script>\n    \
             <link rel=\"modulepreload\" crossorigin href=\"/assets/dep-b.js\">\n    \
             <link rel=\"stylesheet\" crossorigin href=\"/assets/index-c.css\">\n  </head>\n</html>\n"
        );
    }

    #[test]
    fn a_one_line_head_gets_a_two_space_indent() {
        let page = "<head><title>r</title></head><body></body>";
        assert_eq!(
            inject_to_head(page, &[Tag::script("/a.js".into(), true)]),
            "<head><title>r</title>  <script async type=\"module\" crossorigin src=\"/a.js\"></script>\n</head><body></body>"
        );
    }

    #[test]
    fn without_a_head_tags_go_before_body_then_after_html() {
        let one = [Tag::stylesheet("/a&b.css".into())];
        assert_eq!(
            inject_to_head("<body></body>", &one),
            "<link rel=\"stylesheet\" crossorigin href=\"/a&amp;b.css\">\n\n<body></body>"
        );
        assert_eq!(
            inject_to_head("<!DOCTYPE html><HTML lang=en><p>x", &one),
            "<!DOCTYPE html><HTML lang=en>\n<link rel=\"stylesheet\" crossorigin href=\"/a&amp;b.css\">\n<p>x"
        );
        assert_eq!(
            inject_to_head("<p>x", &one),
            "<link rel=\"stylesheet\" crossorigin href=\"/a&amp;b.css\">\n<p>x"
        );
        assert_eq!(inject_to_head("<p>x", &[]), "<p>x");
    }

    #[test]
    fn page_refs_are_module_scripts_and_bundled_stylesheets_in_document_order() {
        let page = r#"<link rel="stylesheet" href="/a.css"><link rel="stylesheet" href="/p.css" media="print">
<link rel="icon" href="/i.png"><link rel=stylesheet href="https://x/y.css">
<script src="/classic.js"></script><script type="module" src="/m.js" async></script>
<script type="module" vite-ignore src="/skip.js"></script><link rel="stylesheet" href="/b.scss">"#;
        assert_eq!(
            page_refs(page, |u| u.ends_with(".css") || u.ends_with(".scss")),
            [
                PageRef::Style("/a.css".into()),
                PageRef::Script {
                    src: "/m.js".into(),
                    is_async: true
                },
                PageRef::Style("/b.scss".into()),
            ]
        );
    }

    #[test]
    fn stripping_takes_the_blank_line_lead_with_the_element() {
        let page = "<head>\n    <link rel=\"stylesheet\" href=\"/a.css\" />\n    <title>t</title>\n  </head>\n  <body>\n    <script type=\"module\" src=\"/m.js\"></script>\n  <p>x</p><script type=\"module\" src=\"/m2.js\"></SCRIPT></body>";
        let refs = page_refs(page, |_| true);
        assert_eq!(
            strip_page_refs(page, &refs),
            "<head>\n    <title>t</title>\n  </head>\n  <body>\n  <p>x</p></body>"
        );
    }
}
