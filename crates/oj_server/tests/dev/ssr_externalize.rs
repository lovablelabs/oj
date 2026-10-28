// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Which resolved dependency files the ssr environment leaves to Node. Vite's
//! canExternalizeFile hands Node only `.js` / `.mjs` / `.cjs` or an
//! extensionless file, reading the extension with its query, so
//! `pkg/styles.css?url` and a raw `.ts` source stay in the pipeline. Before,
//! oj externalized every dependency file but a stylesheet or JSON, and read
//! `styles.css?url` as extension `css?url`: Node then loaded the sheet.

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn ssr_externalizes_only_what_vite_can_externalize() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrext","private":true,"type":"module"}"#,
        ),
        (
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/pkg/index.js", "export default 'pkg';\n"),
        ("node_modules/pkg/lib.cjs", "module.exports = 1;\n"),
        ("node_modules/pkg/bin", "export default 'bin';\n"),
        ("node_modules/pkg/styles.css", ".pkg { color: red }\n"),
        ("node_modules/pkg/util.ts", "export const n: number = 1;\n"),
        ("node_modules/pkg/data.json", "{\"a\":1}\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let importer = app.root.join("src/main.js");
    let resolve = |spec: &'static str| {
        let url = format!(
            "/@ssr-resolve?importer={}&spec={}",
            importer.display(),
            spec.replace('?', "%3F")
        );
        let server = &server;
        async move { server.text(&url).await }
    };

    for spec in ["pkg", "pkg/lib.cjs", "pkg/bin"] {
        let r = resolve(spec).await;
        assert!(r.contains(r#""external":true"#), "{spec} stays Node's: {r}");
    }
    for (spec, file) in [
        ("pkg/styles.css?url", "/node_modules/pkg/styles.css?url"),
        ("pkg/styles.css", "/node_modules/pkg/styles.css"),
        ("pkg/util.ts", "/node_modules/pkg/util.ts"),
        ("pkg/data.json", "/node_modules/pkg/data.json"),
        ("pkg/index.js?raw", "/node_modules/pkg/index.js?raw"),
    ] {
        let r = resolve(spec).await;
        assert!(
            !r.contains(r#""external""#) && r.contains(&format!("{file}\"")),
            "{spec} goes through the pipeline as {file}: {r}"
        );
    }

    // The sheet's URL module, not the sheet run as JavaScript.
    let sheet = app.root.join("node_modules/pkg/styles.css");
    let module = server
        .get(&format!("/@ssr-module?id={}%3Furl", sheet.display()))
        .await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.body.contains("export default") && module.body.contains("styles.css"),
        "{}",
        module.body
    );
    // An app file's `?raw`, its text.
    let raw = server
        .text(&format!("/@ssr-module?id={}%3Fraw", importer.display()))
        .await;
    assert!(raw.contains(r#"export default "export {};\n""#), "{raw}");
}

/// `/@ssr-resolve` for `spec` imported from `importer`.
async fn ssr_resolve(server: &crate::Server, importer: &std::path::Path, spec: &str) -> String {
    let url = format!(
        "/@ssr-resolve?importer={}&spec={}",
        importer.display(),
        spec.replace('?', "%3F")
    );
    server.text(&url).await
}

// Vite's createIsConfiguredAsExternal: `ssr.noExternal` keeps a package (and
// so its own relative imports) in the pipeline, an exact `ssr.external` id is
// always Node's, and a package `ssr.external` names is Node's even when linked
// outside node_modules. A relative or absolute specifier never is.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_external_and_no_external_decide_as_in_vite() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrcfg","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "ssr": { "noExternal": ["nox"], "external": ["linked", "nox/raw.css"] } }"#,
        ),
        (
            "node_modules/nox/package.json",
            r#"{"name":"nox","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/nox/index.js", "export { x } from './x.js';\n"),
        ("node_modules/nox/x.js", "export const x = 1;\n"),
        ("node_modules/nox/raw.css", ".nox {}\n"),
        (
            "packages/linked/package.json",
            r#"{"name":"linked","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("packages/linked/index.js", "export default 'linked';\n"),
        (
            "packages/plain/package.json",
            r#"{"name":"plain","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("packages/plain/index.js", "export default 'plain';\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    for name in ["linked", "plain"] {
        std::os::unix::fs::symlink(
            app.root.join("packages").join(name),
            app.root.join("node_modules").join(name),
        )
        .unwrap();
    }
    let server = app.serve().await;
    let main = app.root.join("src/main.js");

    let nox = ssr_resolve(&server, &main, "nox").await;
    assert!(
        !nox.contains(r#""external""#) && nox.contains("/node_modules/nox/index.js\""),
        "a noExternal package goes through the pipeline: {nox}"
    );
    let dep = app.root.join("node_modules/nox/index.js");
    let inner = ssr_resolve(&server, &dep, "./x.js").await;
    assert!(
        !inner.contains(r#""external""#) && inner.contains("/node_modules/nox/x.js\""),
        "its relative import stays in the pipeline: {inner}"
    );
    let exact = ssr_resolve(&server, &main, "nox/raw.css").await;
    assert!(
        exact.contains(r#""external":true"#),
        "an exact ssr.external id is Node's: {exact}"
    );
    let linked = ssr_resolve(&server, &main, "linked").await;
    assert!(
        linked.contains(r#""external":true"#),
        "a linked package ssr.external names is Node's: {linked}"
    );
    let plain = ssr_resolve(&server, &main, "plain").await;
    assert!(
        !plain.contains(r#""external""#),
        "an unlisted linked package goes through the pipeline: {plain}"
    );
}

// Vite's dev fileToDevUrl, on the server render and the client alike: a
// public file's URL is its path under the public dir, every URL takes
// `server.origin` and the base, and an svg under assetsInlineLimit inlines
// (svgToDataURL) unless `?no-inline`.
#[tokio::test(flavor = "multi_thread")]
async fn asset_urls_take_the_origin_and_base_and_small_svgs_inline() {
    let big = format!("<svg>{}</svg>\n", "<g/>".repeat(1100));
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrpub","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "base": "/app/", "server": { "origin": "https://cdn.example" } }"#,
        ),
        ("public/logo.png", "png"),
        ("src/pic.png", "png"),
        ("src/icon.svg", "<svg></svg>\n"),
        ("src/big.svg", &big),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let ssr = |rel: &str, query: &str| {
        let url = format!("/@ssr-module?id={}%3F{query}", app.root.join(rel).display());
        let server = &server;
        async move { server.text(&url).await }
    };
    let cases = [
        ("public/logo.png", "url", "https://cdn.example/app/logo.png"),
        ("src/pic.png", "url", "https://cdn.example/app/src/pic.png"),
        (
            "src/icon.svg",
            "url",
            "data:image/svg+xml,%3csvg%3e%3c/svg%3e",
        ),
        (
            "src/icon.svg",
            "no-inline",
            "https://cdn.example/app/src/icon.svg",
        ),
        ("src/big.svg", "url", "https://cdn.example/app/src/big.svg"),
        (
            "src/icon.svg",
            "inline",
            "data:image/svg+xml,%3csvg%3e%3c/svg%3e",
        ),
    ];
    for (rel, query, want) in cases {
        let module = ssr(rel, query).await;
        assert!(
            module.contains(&format!("export default \"{want}\"")),
            "ssr {rel}?{query}: {module}"
        );
    }
    for (rel, query, want) in &cases[1..] {
        let module = server.text(&format!("/app/{rel}?{query}")).await;
        assert!(
            module.contains(&format!("export default \"{want}\"")),
            "client {rel}?{query}: {module}"
        );
    }
}
