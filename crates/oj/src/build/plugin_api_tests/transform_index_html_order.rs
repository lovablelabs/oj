// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, files_under, scratch, write_files};

/// Vite's vite:build-html runs the `order: "pre"` transformIndexHtml hooks on
/// the page source in its transform: after buildStart, before the page's
/// scripts are read and before any chunk renders, with `{ path, filename }`.
/// The normal and post hooks run once the bundle exists, with `bundle` and
/// `chunk`. UnoCSS collects a page's classes in a pre hook, resets in
/// buildStart and generates in renderChunk, so all three orders matter.
#[tokio::test]
async fn pre_html_hooks_run_on_the_source_after_build_start() {
    let root = scratch("html-pre-hooks");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"html-pre","version":"1.0.0"}"#),
            ("src/entry.js", "console.log(\"entry\");\n"),
            (
                "src/injected.js",
                "console.log(\"injected-by-pre-hook\");\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><div class="from-page">hi</div><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"let log;
export default [{
  name: "html-order",
  buildStart() { log = ["buildStart"]; },
  transformIndexHtml: {
    order: "pre",
    handler(html, ctx) {
      log.push(`pre ${ctx.path} ${ctx.filename.endsWith("/index.html")} ${"bundle" in ctx} ${/class="from-page"/.test(html)}`);
      return html.replace("</body>", '<script type="module" src="/src/injected.js"></script></body>');
    },
  },
  renderChunk() { log.push("renderChunk"); return null; },
  generateBundle() {
    this.emitFile({ type: "asset", fileName: "log.json", source: JSON.stringify(log) });
  },
}, {
  name: "html-normal",
  transformIndexHtml(html, ctx) {
    log.push(`normal ${ctx.path} ${typeof ctx.bundle} ${ctx.chunk ? "chunk" : "none"}`);
    return html.replace("<title>t</title>", "<title>normal</title>");
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let log: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(root.join("dist/log.json")).expect("generateBundle ran"),
    )
    .expect("log is JSON");
    let renders = log.iter().filter(|l| *l == "renderChunk").count();
    let pre = log
        .iter()
        .position(|l| l.starts_with("pre "))
        .expect("the pre hook ran");
    assert_eq!(log[0], "buildStart", "{log:?}");
    assert_eq!(log[pre], "pre /index.html true false true", "{log:?}");
    assert!(
        renders > 0 && log.iter().position(|l| l == "renderChunk").unwrap() > pre,
        "the pre hook ran before any chunk rendered: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|l| l.starts_with("pre ")).count(),
        1,
        "{log:?}"
    );
    // The normal hook runs after generateBundle wrote the log, on the page
    // the pre hook returned.
    let html = std::fs::read_to_string(root.join("dist/index.html")).expect("page written");
    assert!(html.contains("<title>normal</title>"), "{html}");
    let js: String = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".js"))
        .map(|(_, code)| code)
        .collect();
    assert!(
        js.contains("injected-by-pre-hook") && (js.contains("\"entry\"") || js.contains("`entry`")),
        "the script the pre hook added is bundled with the page's own:\n{js}"
    );
}

/// Vite's html playground (`pre-transform`, `append-external-path-by-rollup-options`,
/// `transform-inline-js`): a pre hook's tag descriptors and its rewrite of an
/// inline module script both land before the page's scripts are read, so the
/// injected script is bundled and the inline one carries the new text.
#[tokio::test]
async fn pre_html_hooks_add_tags_and_rewrite_inline_scripts_before_bundling() {
    let root = scratch("html-pre-tags-inline");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"html-pre-tags","version":"1.0.0"}"#,
            ),
            ("src/tag.js", "console.log(\"tag-script-bundled\");\n"),
            ("src/main.js", "console.log(\"entry\");\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module">window.__ID = "{{ id }}";</script><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"export default [
  { name: "tags", transformIndexHtml: { order: "pre", handler: () => [{ tag: "script", attrs: { type: "module", src: "/src/tag.js" }, injectTo: "body" }] } },
  { name: "inline", transformIndexHtml: { order: "pre", handler: (html) => html.replaceAll("{{ id }}", "replaced-by-pre") } },
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let js: String = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".js"))
        .map(|(_, code)| code)
        .collect();
    assert!(
        js.contains("tag-script-bundled"),
        "the injected script is bundled:\n{js}"
    );
    assert!(
        js.contains("replaced-by-pre") && !js.contains("{{ id }}"),
        "the inline script carries the pre hook's text:\n{js}"
    );
    let html = std::fs::read_to_string(root.join("dist/index.html")).expect("page written");
    assert_eq!(
        html.matches("<script").count(),
        1,
        "one entry script, as Vite writes:\n{html}"
    );
}
