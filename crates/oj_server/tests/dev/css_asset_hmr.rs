// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An asset a `<link>`-loaded sheet names is one of the sheet's deps (Vite's
//! watch files): editing it sends a `css-update` for the sheet, whose
//! recompile names the asset with its new `t=`, and a sheet naming assets is
//! never served from the persistent cache with an asset as it was.

use axum::body::Body;
use axum::http::Request;

use crate::listen::{close, frame_matching};
use crate::{App, Server};

const INDEX: &str = r#"<!doctype html><html><head><link rel="stylesheet" href="/src/style.css"></head><body></body></html>"#;

async fn linked_sheet(server: &Server) -> String {
    let res = server
        .request(
            Request::get("/src/style.css")
                .header("sec-fetch-dest", "style")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.body);
    res.body
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linked_sheets_asset_edit_sends_a_css_update_for_the_sheet() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-asset-hmr","version":"1.0.0"}"#,
        ),
        (
            "src/style.css",
            ".a { background: url(./bg.png) } .b { background: url(./dot.svg) }\n",
        ),
        ("src/bg.png", "png-v1"),
        ("src/dot.svg", "<svg><g/></svg>"),
        ("index.html", INDEX),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    let first = linked_sheet(&server).await;
    assert!(first.contains(r#"url("/src/bg.png")"#), "{first}");
    app.write(&[("src/bg.png", "png-v2")]);
    let frame = frame_matching(&mut socket, "css-update for the sheet", |f| {
        f["type"] == "update"
            && f["updates"].as_array().is_some_and(|u| {
                u.iter()
                    .any(|u| u["type"] == "css-update" && u["path"] == "/src/style.css")
            })
    })
    .await;
    assert!(
        !frame["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["type"] == "js-update"),
        "{frame}"
    );
    let second = linked_sheet(&server).await;
    assert!(second.contains("/src/bg.png?t="), "{second}");
    close(socket).await;
}

/// Waits until the persistent cache holds a module whose code has `marker`:
/// writes are queued in order, so every earlier write has landed too.
async fn wait_cached(root: &std::path::Path, marker: &str) {
    let dir = oj_cache::cache_root(root);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let found = walk(&dir)
            .iter()
            .any(|f| std::fs::read_to_string(f).is_ok_and(|text| text.contains(marker)));
        if found {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{marker} never cached"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|e| match e.path() {
            p if p.is_dir() => walk(&p),
            p => vec![p],
        })
        .collect()
}

// A sheet that inlined a file it names (a small svg, a JSON file under
// `?inline`) is never served from the persistent cache with the file as it
// was; a sheet compiled after it is cached, so its write had its turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_sheet_naming_assets_is_not_served_stale_from_the_persistent_cache() {
    let cases = [
        (
            "url(./dot.svg)",
            "src/dot.svg",
            "<svg><g/></svg>",
            "<svg><rect/></svg>",
            "%3cg/%3e",
            "%3crect/%3e",
        ),
        // base64 of `{"v":1}` and `{"v":2}`.
        (
            "url(./d.json?inline)",
            "src/d.json",
            r#"{"v":1}"#,
            r#"{"v":2}"#,
            "eyJ2IjoxfQ==",
            "eyJ2IjoyfQ==",
        ),
    ];
    for (url, file, before, after, old, new) in cases {
        let sheet = format!(".b {{ background: {url} }}\n");
        let app = App::new(&[
            (
                "package.json",
                r#"{"name":"css-asset-cache","version":"1.0.0"}"#,
            ),
            ("src/style.css", &sheet),
            ("src/plain.css", ".cache-marker { color: red }\n"),
            (file, before),
            ("index.html", INDEX),
        ]);
        let serve = || {
            let mut dev = app.dev();
            dev.enable_cache = true;
            crate::serve(dev)
        };
        let server = serve().await;
        let first = linked_sheet(&server).await;
        assert!(first.contains(old), "{first}");
        server.text("/src/plain.css?direct").await;
        wait_cached(&app.root, "cache-marker").await;
        drop(server);

        app.write(&[(file, after)]);
        let server = serve().await;
        let second = linked_sheet(&server).await;
        assert!(
            second.contains(new) && !second.contains(old),
            "{url}: {second}"
        );
    }
}

// A page's `<style>` resolves its relative urls against the page's directory,
// as Vite's html-proxy sheet does: under the base, a small svg inlined, and
// an edit of what it names reaches the page.
#[tokio::test(flavor = "multi_thread")]
async fn a_pages_style_urls_resolve_beside_the_page() {
    let app = App::new(&[
        ("package.json", r#"{"name":"page-style-urls","version":"1.0.0"}"#),
        // Beside the root too, so a url resolved against the root shows.
        ("dot.svg", "<svg><rect/></svg>"),
        ("nested/dot.svg", "<svg><g/></svg>"),
        ("nested/img.png", "png-v1"),
        (
            "nested/index.html",
            "<!doctype html><html><head><style>.h1{background:url(./dot.svg)} .h2{background:url(./img.png)}</style></head><body></body></html>",
        ),
        ("index.html", "<!doctype html><html><head></head><body></body></html>"),
        ("vite.config.js", "export default { base: \"/app/\" };\n"),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/app/__ws", None).await;
    let html = server.text("/app/nested/").await;
    assert!(
        html.contains("data:image/svg+xml,%3csvg%3e%3cg/%3e%3c/svg%3e"),
        "{html}"
    );
    assert!(html.contains("/app/nested/img.png"), "{html}");
    app.write(&[("nested/img.png", "png-v2")]);
    frame_matching(&mut socket, "an update for the page's sheet", |f| {
        f["type"] == "update" || f["type"] == "full-reload"
    })
    .await;
    close(socket).await;
}

// The css url resolver resolves a package's file as the resolver does, its
// `exports` honored: a mapped subpath names its target, a hidden one stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_url_follows_the_packages_exports() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-pkg-exports","version":"1.0.0"}"#,
        ),
        (
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","exports":{"./assets/*":"./assets/sub/*"}}"#,
        ),
        ("node_modules/pkg/assets/sub/p.png", "png"),
        ("node_modules/pkg/hidden.png", "png"),
        (
            "src/style.css",
            ".a { background: url(pkg/assets/p.png) } .b { background: url(pkg/hidden.png) }\n",
        ),
        ("index.html", INDEX),
    ]);
    let server = app.serve().await;
    let css = linked_sheet(&server).await;
    assert!(css.contains("/node_modules/pkg/assets/sub/p.png"), "{css}");
    assert!(
        css.contains("url(\"pkg/hidden.png\")") || css.contains("url(pkg/hidden.png)"),
        "{css}"
    );
}
