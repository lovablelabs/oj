// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use regex::Regex;

use super::{build_app, scratch, write_files};

/// The entry `<script type=module src>` and `<link rel=stylesheet href>` are
/// found and rewritten whatever the attribute quoting (single, spaced,
/// unquoted) and without matching `data-src` / `data-href` lookalikes. The
/// stylesheet is bundled into the entry's own (hashed, minified) instead of
/// being copied verbatim, like Vite's HTML parser-based rewrite.
#[tokio::test]
async fn entry_script_and_stylesheet_are_rewritten_whatever_the_attribute_quoting() {
    let fixtures = [
        (
            "single-quoted",
            "<link rel='stylesheet' href='/site.css'>",
            "<script type='module' src='/main.js'></script>",
        ),
        (
            "spaced",
            r#"<link rel = "stylesheet" href = "/site.css">"#,
            r#"<script type = "module" src = "/main.js"></script>"#,
        ),
        (
            "unquoted",
            "<link rel=stylesheet href=/site.css>",
            "<script type=module src=/main.js></script>",
        ),
        (
            "name-boundaries",
            "<link rel='stylesheet' data-href='/ignored.css' href='/site.css'>",
            "<script data-type='module' data-src='/ignored.js' type='module' src='/main.js'></script>",
        ),
    ];
    let entry_ref = Regex::new(r#"/assets/index-[^"'\s>]+\.js"#).unwrap();
    let css_ref = Regex::new(r#"/assets/index-[^"'\s>]+\.css"#).unwrap();
    let red = Regex::new(r"color:\s*red").unwrap();
    for (name, stylesheet, entry) in fixtures {
        let root = scratch(&format!("html-attr-{name}"));
        let html = format!("<html><head>{stylesheet}</head><body>{entry}</body></html>");
        write_files(
            &root,
            &[
                ("package.json", r#"{"type":"module"}"#),
                ("index.html", &html),
                ("main.js", r#"document.body.textContent = "ready";"#),
                ("site.css", "body { color: red; }"),
            ],
        );
        if let Err(err) = build_app(&root).await {
            panic!("{name} must build successfully:\n{err:#}");
        }

        let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
        assert!(
            entry_ref.is_match(&html),
            "{name} must rewrite the module entry:\n{html}"
        );
        let css_url = css_ref
            .find(&html)
            .unwrap_or_else(|| panic!("{name} must rewrite the stylesheet link:\n{html}"))
            .as_str();
        let css = fs::read_to_string(root.join("dist").join(css_url.trim_start_matches('/')))
            .unwrap_or_else(|_| panic!("{name}: the linked stylesheet {css_url} exists"));
        assert!(
            red.is_match(&css),
            "{name} must emit the stylesheet:\n{css}"
        );
        assert!(
            !root.join("dist/site.css").exists(),
            "{name}: stylesheet is not copied verbatim"
        );
    }
}

/// Inline module scripts are externalized (Vite's html-proxy) whatever the
/// quoting of `type`: the body is bundled and transformed (its TypeScript
/// import compiled), never shipped raw in the HTML.
#[tokio::test]
async fn inline_module_scripts_are_bundled_whatever_the_type_quoting() {
    let fixtures = [
        ("single-quoted", "<script type='module'>"),
        ("unquoted", "<script type=module>"),
        ("spaced", r#"<script type = "module">"#),
    ];
    let entry_ref = Regex::new(r#"/assets/[^"'\s>]+\.js"#).unwrap();
    for (name, tag) in fixtures {
        let root = scratch(&format!("html-inline-{name}"));
        let html = format!(
            r#"<html><body>{tag}import {{ greet }} from "./util.ts"; document.body.textContent = greet("inline");</script></body></html>"#
        );
        write_files(
            &root,
            &[
                ("package.json", r#"{"type":"module"}"#),
                ("index.html", &html),
                (
                    "util.ts",
                    "export function greet(who: string): string { return `hi ${who}`; }",
                ),
            ],
        );
        if let Err(err) = build_app(&root).await {
            panic!("{name} inline module must build successfully:\n{err:#}");
        }

        let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
        assert!(
            !html.contains("./util.ts"),
            "{name}: inline body must not ship raw:\n{html}"
        );
        let entry = entry_ref
            .find(&html)
            .unwrap_or_else(|| panic!("{name} must reference a bundled entry chunk:\n{html}"))
            .as_str();
        let js = fs::read_to_string(root.join("dist").join(entry.trim_start_matches('/')))
            .unwrap_or_else(|_| panic!("{name}: the entry chunk {entry} exists"));
        assert!(
            js.contains("hi "),
            "{name}: the bundled chunk carries the inline body and its import:\n{js}"
        );
        assert!(
            !js.contains(": string"),
            "{name}: TypeScript was compiled:\n{js}"
        );
    }
}
