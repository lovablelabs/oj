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

// Vite's dev fileToDevUrl, on the server render and the client alike: every
// URL takes `server.origin` and the base; a public file is public only when
// its request path names it under the public dir (checkPublicFile); an svg
// under assetsInlineLimit inlines (svgToDataURL) unless `?no-inline`, which
// stays in the URL with every other parameter but `url`; `?url&inline`
// inlines any file; `?inline` is a data URL only for an asset; the path is
// encodeURIPath'd; worker and wasm URLs and a stylesheet's `url()` take the
// same rules (worker.ts, wasm.ts, css.ts all name files with fileToUrl).
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
        ("src/my pic.png", "png"),
        ("src/icon.svg", "<svg></svg>\n"),
        ("src/big.svg", &big),
        ("src/data.json", "{\"k\":42}\n"),
        ("src/w.js", "self.onmessage = () => {};\n"),
        ("src/add.wasm", "\0asm"),
        (
            "src/style.css",
            ".a { background: url(./icon.svg) } .b { background: url(./pic.png) } .c { background: url(/logo.png) }\n",
        ),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let module = |id: String, query: &str| {
        let url = format!("/@ssr-module?id={id}%3F{}", query.replace('&', "%26"));
        let server = &server;
        async move { server.text(&url).await }
    };
    let fs = |rel: &str| app.root.join(rel).display().to_string().replace(' ', "%20");
    let png64 = "data:image/png;base64,cG5n";
    // (ssr id, client path, query, expected default export)
    let cases = [
        (
            "/logo.png".to_string(),
            "logo.png",
            "url",
            "https://cdn.example/app/logo.png",
        ),
        (
            fs("public/logo.png"),
            "public/logo.png",
            "url",
            "https://cdn.example/app/public/logo.png",
        ),
        (
            fs("src/pic.png"),
            "src/pic.png",
            "url",
            "https://cdn.example/app/src/pic.png",
        ),
        (
            fs("src/pic.png"),
            "src/pic.png",
            "url&v=3",
            "https://cdn.example/app/src/pic.png?v=3",
        ),
        (fs("src/pic.png"), "src/pic.png", "url&inline", png64),
        (fs("src/pic.png"), "src/pic.png", "inline", png64),
        (
            fs("src/my pic.png"),
            "src/my%20pic.png",
            "url",
            "https://cdn.example/app/src/my%20pic.png",
        ),
        (
            fs("src/icon.svg"),
            "src/icon.svg",
            "url",
            "data:image/svg+xml,%3csvg%3e%3c/svg%3e",
        ),
        (
            fs("src/icon.svg"),
            "src/icon.svg",
            "no-inline",
            "https://cdn.example/app/src/icon.svg?no-inline",
        ),
        (
            fs("src/big.svg"),
            "src/big.svg",
            "url",
            "https://cdn.example/app/src/big.svg",
        ),
        (
            fs("src/icon.svg"),
            "src/icon.svg",
            "inline",
            "data:image/svg+xml,%3csvg%3e%3c/svg%3e",
        ),
    ];
    for (id, path, query, want) in &cases {
        let want = format!("export default \"{want}\"");
        let ssr = module(id.clone(), query).await;
        assert!(ssr.contains(&want), "ssr {id}?{query}: {ssr}");
        let client = server.text(&format!("/app/{path}?{query}")).await;
        assert!(client.contains(&want), "client {path}?{query}: {client}");
    }

    // `?inline` on a file vite:asset does not load is the plain module.
    let json_ssr = module(fs("src/data.json"), "inline").await;
    assert!(
        !json_ssr.contains("data:") && json_ssr.contains("42"),
        "{json_ssr}"
    );
    let json = server.text("/app/src/data.json?inline").await;
    assert!(!json.contains("data:") && json.contains("42"), "{json}");

    // The module runner gets an asset module in its transformed form.
    let runner = server
        .text(&format!(
            "/@ssr-module?id={}%3Furl&runner=1",
            fs("src/pic.png")
        ))
        .await;
    assert!(
        runner.contains("https://cdn.example/app/src/pic.png")
            && !runner.contains("export default"),
        "{runner}"
    );

    let worker = server.text("/app/src/w.js?worker").await;
    assert!(
        worker.contains(r#""https://cdn.example/app/src/w.js?worker_file&type=module""#),
        "{worker}"
    );
    let init = server.text("/app/src/add.wasm?init").await;
    assert!(
        init.contains(r#""https://cdn.example/app/src/add.wasm""#),
        "{init}"
    );

    let css = server.text("/app/src/style.css?direct").await;
    for want in [
        r#"url("data:image/svg+xml,%3csvg%3e%3c/svg%3e")"#,
        r#"url("https://cdn.example/app/src/pic.png")"#,
        r#"url("https://cdn.example/app/logo.png")"#,
    ] {
        assert!(css.contains(want), "css {want}:\n{css}");
    }
}

// Vite's createIsExternal and importAnalysis: a builtin is Node's before any
// `ssr.noExternal` (an installed `events` polyfill never wins), an aliased
// specifier never is, and a `noExternal` package's own bare import is
// decided like any other.
#[tokio::test(flavor = "multi_thread")]
async fn builtins_aliases_and_a_no_external_packages_imports_decide_as_in_vite() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrbuiltin","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "resolve": { "alias": { "aliased": "real" } }, "ssr": { "noExternal": ["events", "nox"] } }"#,
        ),
        (
            "node_modules/events/package.json",
            r#"{"name":"events","version":"3.3.0","main":"events.js"}"#,
        ),
        ("node_modules/events/events.js", "module.exports = {};\n"),
        (
            "node_modules/real/package.json",
            r#"{"name":"real","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/real/index.js", "export default 'real';\n"),
        (
            "node_modules/nox/package.json",
            r#"{"name":"nox","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/nox/index.js",
            "export { default } from 'real';\n",
        ),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = app.root.join("src/main.js");

    for spec in ["events", "node:events", "fs/promises"] {
        let r = ssr_resolve(&server, &main, spec).await;
        assert!(
            r.contains(r#""external":true"#),
            "builtin {spec} is Node's: {r}"
        );
    }
    let aliased = ssr_resolve(&server, &main, "aliased").await;
    assert!(
        !aliased.contains(r#""external""#) && aliased.contains("/node_modules/real/index.js\""),
        "an aliased specifier goes through the pipeline: {aliased}"
    );
    let plain = ssr_resolve(&server, &main, "real").await;
    assert!(
        plain.contains(r#""external":true"#),
        "unaliased stays Node's: {plain}"
    );
    let dep = app.root.join("node_modules/nox/index.js");
    let inner = ssr_resolve(&server, &dep, "real").await;
    assert!(
        inner.contains(r#""external":true"#),
        "a noExternal package's bare import is decided on its own: {inner}"
    );
}

// Vite's checkPublicFile only names files inside the public dir: a
// root-relative id that climbs out of it, or any queried id outside the
// allowed roots, is never read.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_asset_ids_never_read_outside_the_allowed_roots() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrtraverse","private":true,"type":"module"}"#,
        ),
        ("public/logo.png", "png"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let outside = app.root.parent().unwrap().join("secret-ssr-outside.txt");
    std::fs::write(&outside, "TOP-SECRET").unwrap();
    let outside_js = app.root.parent().unwrap().join("secret-ssr-outside.js");
    std::fs::write(&outside_js, "export default \"TOP-SECRET\";\n").unwrap();
    let server = app.serve().await;
    for id in [
        "/../../secret-ssr-outside.txt%3Fraw".to_string(),
        "/../secret-ssr-outside.txt%3Finline".to_string(),
        format!("{}%3Fraw", outside.display()),
        // A query names no file on disk, so it must not pass as a virtual id.
        format!("{}%3Fv%3D1", outside_js.display()),
        format!("{}%3Fimport", outside_js.display()),
    ] {
        let r = server.get(&format!("/@ssr-module?id={id}")).await;
        assert!(
            r.status != 200 && !r.body.contains("TOP-SECRET") && !r.body.contains("VE9QLVNFQ1JFVA"),
            "{id}: {} {}",
            r.status,
            r.body
        );
    }
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_file(&outside_js);
}

// importAnalysis gates the builtin check on `!matchAlias` too: an alias of a
// builtin name loads its target through the pipeline.
#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_builtin_name_goes_through_the_pipeline() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssraliasbuiltin","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "resolve": { "alias": { "events": "events-polyfill" } } }"#,
        ),
        (
            "node_modules/events-polyfill/package.json",
            r#"{"name":"events-polyfill","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/events-polyfill/index.js",
            "export default 'polyfill';\n",
        ),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = app.root.join("src/main.js");
    let r = ssr_resolve(&server, &main, "events").await;
    assert!(
        !r.contains(r#""external""#) && r.contains("/node_modules/events-polyfill/index.js\""),
        "{r}"
    );
    let fs = ssr_resolve(&server, &main, "fs").await;
    assert!(fs.contains(r#""external":true"#), "{fs}");
}

// A plain import of a public asset in the server render is its URL module,
// as vite:asset's load names it; a stylesheet's inlined svg follows the svg
// as it is now (the svg is a dep of the sheet's compile).
#[tokio::test(flavor = "multi_thread")]
async fn public_imports_and_inlined_svgs_in_sheets_stay_current() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrpublicplain","private":true,"type":"module"}"#,
        ),
        ("public/logo.png", "png"),
        ("src/i.svg", "<svg></svg>\n"),
        ("src/style.css", ".a { background: url(./i.svg) }\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let logo = server.text("/@ssr-module?id=/logo.png").await;
    assert!(logo.contains(r#"export default "/logo.png""#), "{logo}");

    let before = server.text("/src/style.css?direct").await;
    assert!(before.contains("%3csvg%3e%3c/svg%3e"), "{before}");
    app.write(&[("src/i.svg", "<svg><g/></svg>\n")]);
    // The sheet recompiles once the watcher reports the svg's edit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let after = loop {
        let after = server.text("/src/style.css?direct").await;
        if after.contains("%3csvg%3e%3cg/%3e%3c/svg%3e") || std::time::Instant::now() > deadline {
            break after;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(after.contains("%3csvg%3e%3cg/%3e%3c/svg%3e"), "{after}");
}

// `/@ssr-module` is an HTTP route, so Vite's isFileLoadingAllowed holds
// there: `server.fs.deny` refuses `.env`, keys and `.git` however they are
// asked for, a public file included.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_module_reads_honor_server_fs_deny() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrdeny","private":true,"type":"module"}"#,
        ),
        (".env", "SECRET=hunter2\n"),
        ("key.pem", "SECRET-KEY\n"),
        (".git/config", "SECRET-GIT\n"),
        ("public/.env.local", "SECRET=public\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let root = app.root.display();
    for id in [
        format!("{root}/.env%3Fraw"),
        format!("{root}/key.pem%3Fraw"),
        format!("{root}/.git/config%3Fraw"),
        format!("{root}/.env%3Furl%26inline"),
        format!("{root}/.env"),
        "/.env.local%3Fraw".to_string(),
    ] {
        let r = server.get(&format!("/@ssr-module?id={id}")).await;
        assert!(
            r.status != 200 && !r.body.contains("SECRET") && !r.body.contains("U0VDUkVU"),
            "{id}: {} {}",
            r.status,
            r.body
        );
    }
}

// Vite's isFileLoadingAllowed: `server.fs.strict: false` lets a load read
// outside the allowed roots, and a file a package import resolved to is a
// safe module path wherever it is linked.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_loads_follow_fs_strict_and_resolved_package_files() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrstrict","private":true,"type":"module"}"#,
        ),
        ("src/main.js", "export {};\n"),
        // Imported by the server render only, so the client's crawl never
        // allows its imports.
        (
            "src/server.js",
            "import { v } from \"outlinked\";\nexport { v };\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let outside = app.root.parent().unwrap().join("ssr-linked-outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("package.json"),
        r#"{"name":"outlinked","version":"1.0.0","type":"module","main":"index.ts"}"#,
    )
    .unwrap();
    std::fs::write(
        outside.join("index.ts"),
        "export const v: string = \"outlinked\";\n",
    )
    .unwrap();
    std::fs::create_dir_all(app.root.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&outside, app.root.join("node_modules/outlinked")).unwrap();
    let server = app.serve().await;
    let file = std::fs::canonicalize(outside.join("index.ts")).unwrap();
    let load = || {
        let url = format!("/@ssr-module?id={}", file.display());
        let server = &server;
        async move { server.get(&url).await }
    };
    assert_ne!(load().await.status, 200, "not resolved yet");
    let resolve = |importer: &std::path::Path, spec: &str| {
        let url = format!("/@ssr-resolve?importer={}&spec={spec}", importer.display());
        let server = &server;
        async move { server.text(&url).await }
    };
    // Resolving it for an importer the pipeline never served, or one whose
    // code does not import it, makes nothing loadable.
    let nowhere = app
        .root
        .parent()
        .unwrap()
        .join("ssr-strict-nowhere/nope.js");
    resolve(&nowhere, "outlinked").await;
    resolve(&app.root.join("src/server.js"), "outlinked").await;
    assert_ne!(
        load().await.status,
        200,
        "resolved for code oj never served"
    );
    server
        .text(&format!(
            "/@ssr-module?id={}",
            app.root.join("src/server.js").display()
        ))
        .await;
    let resolved = server
        .text(&format!(
            "/@ssr-resolve?importer={}&spec=outlinked",
            app.root.join("src/server.js").display()
        ))
        .await;
    assert!(resolved.contains("index.ts"), "{resolved}");
    let r = load().await;
    assert!(
        r.status == 200 && r.body.contains("outlinked"),
        "{} {}",
        r.status,
        r.body
    );
    // A specifier the served code does not import makes nothing loadable,
    // however it is spelled or wherever its importer claims to be.
    let secret = app.root.parent().unwrap().join("ssr-strict-secret.txt");
    std::fs::write(&secret, "TOP-SECRET").unwrap();
    let other = app.root.parent().unwrap().join("ssr-strict-other");
    std::fs::create_dir_all(other.join("node_modules/victim")).unwrap();
    std::fs::write(other.join("node_modules/victim/secret.txt"), "TOP-SECRET").unwrap();
    let main = app.root.join("src/server.js");
    resolve(&main, "../../ssr-strict-secret.txt").await;
    resolve(&main, "victim/secret.txt").await;
    resolve(&other.join("nope.js"), "victim/secret.txt").await;
    for file in [secret.clone(), other.join("node_modules/victim/secret.txt")] {
        for url in [
            format!("/@ssr-module?id={}%3Fraw", file.display()),
            format!("/@fs{}", file.display()),
        ] {
            let r = server.get(&url).await;
            assert!(
                r.status != 200 && !r.body.contains("TOP-SECRET"),
                "{url}: {} {}",
                r.status,
                r.body
            );
        }
    }
    drop(server);

    app.write(&[(
        "oj.config.json",
        r#"{ "server": { "fs": { "strict": false } } }"#,
    )]);
    let server = app.serve().await;
    let r = server
        .get(&format!("/@ssr-module?id={}%3Fraw", secret.display()))
        .await;
    assert!(
        r.status == 200 && r.body.contains("TOP-SECRET"),
        "{} {}",
        r.status,
        r.body
    );
    let _ = std::fs::remove_file(&secret);
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_dir_all(&other);
}

// vite:asset loads any asset id in every environment, and vite:css-post
// exports a sheet's `?inline` as its compiled text.
#[tokio::test(flavor = "multi_thread")]
async fn plain_asset_and_inline_sheet_imports_load_in_ssr() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrassets","private":true,"type":"module"}"#,
        ),
        ("src/dot.svg", "<svg></svg>"),
        ("src/img.png", "png"),
        (
            "src/c.css",
            ".c { color: red; background: url(./img.png) }\n",
        ),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let module = |rel: &str| {
        let url = format!("/@ssr-module?id={}", app.root.join(rel).display());
        let server = &server;
        async move { server.text(&url).await }
    };
    let svg = module("src/dot.svg").await;
    assert!(
        svg.contains("export default \"data:image/svg+xml,%3csvg%3e%3c/svg%3e\""),
        "{svg}"
    );
    let png = module("src/img.png").await;
    assert!(png.contains("export default \"/src/img.png\""), "{png}");
    // The same text the client's `?inline` compiles, urls rewritten.
    let inline = module("src/c.css%3Finline").await;
    assert!(
        inline.contains("export default \"")
            && inline.contains(".c")
            && inline.contains("/src/img.png"),
        "{inline}"
    );
    // `inline=1` is `inline` too, and a non-asset's plain module, once.
    let main = module("src/main.js%3Finline%3D1").await;
    assert!(main.contains("export"), "{main}");
    assert!(module("src/main.js").await.contains("export"));
}

// vite:asset's load result takes the environment's transforms: a plugin that
// makes a plain svg a component (svgr) does so in the server render too.
#[tokio::test(flavor = "multi_thread")]
async fn a_transform_plugin_componentizes_a_plain_svg_in_ssr() {
    let plugins = r#"export default [{
  name: "svg-component",
  transform(code, id) {
    // Componentizes the markup, as svgr does (the client hands it the file).
    if (!id.split("?")[0].endsWith(".svg") || !code.startsWith("<svg")) return;
    return `export default function Icon() { return ${JSON.stringify(code)}; }`;
  },
}];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrsvgplugin","private":true,"type":"module"}"#,
        ),
        ("src/icon.svg", "<svg></svg>"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", plugins),
    ]);
    let server = app.serve().await;
    let client = server.text("/src/icon.svg?import").await;
    assert!(client.contains("function Icon"), "{client}");
    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}",
            app.root.join("src/icon.svg").display()
        ))
        .await;
    assert!(ssr.contains("function Icon"), "{ssr}");
}
