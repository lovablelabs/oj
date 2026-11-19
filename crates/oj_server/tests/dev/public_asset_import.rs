// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file under `publicDir` imported by its url (`import logo from
//! "/assets/logo.png"`) is a module exporting that url, as Vite serves it:
//! importAnalysis marks the import as a module request (its `?import`,
//! `isExplicitImportRequired`), servePublicMiddleware steps aside for such a
//! request (`isImportRequest`) and vite:asset answers it with the url
//! (`checkPublicFile`, `fileToDevUrl`). A plain request for the url is still
//! the file.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::assets::PNG_1X1;
use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

fn app() -> App {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"public-import","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "import logo from \"/assets/logo.png\";\nwindow.__LOGO = logo;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::create_dir_all(app.root.join("public/assets")).unwrap();
    std::fs::write(app.root.join("public/assets/logo.png"), PNG_1X1).unwrap();
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn public_file_imported_by_its_url_is_a_url_module() {
    let app = app();
    let server = app.serve().await;

    // The importer asks for the module, not the bytes.
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"/assets/logo.png?url\""),
        "the public asset's import is marked as a module request:\n{main}"
    );

    // oj's mark, Vite's mark, and the browser's own word for a module import.
    let module_requests = [
        ("/assets/logo.png?url", None),
        ("/assets/logo.png?import", None),
        ("/assets/logo.png", Some("script")),
    ];
    for (url, dest) in module_requests {
        let mut req = Request::get(url);
        if let Some(dest) = dest {
            req = req.header("sec-fetch-dest", dest);
        }
        let res = server.request(req.body(Body::empty()).unwrap()).await;
        assert_eq!(res.status, 200, "{url} {dest:?}: {}", res.body);
        assert_eq!(
            res.headers["content-type"], "text/javascript",
            "{url} {dest:?} is a module"
        );
        assert!(
            res.body.contains("export default \"/assets/logo.png\""),
            "{url} {dest:?} exports the public url:\n{}",
            res.body
        );
    }

    // The url itself (an `<img src>`, a fetch) is still the file.
    for dest in [None, Some("image")] {
        let mut req = Request::get("/assets/logo.png");
        if let Some(dest) = dest {
            req = req.header("sec-fetch-dest", dest);
        }
        let res = server.request(req.body(Body::empty()).unwrap()).await;
        assert_eq!(res.status, 200, "{dest:?}: {}", res.body);
        assert_eq!(res.headers["content-type"], "image/png", "{dest:?}");
        assert_eq!(
            res.headers["content-length"],
            PNG_1X1.len().to_string().as_str(),
            "{dest:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_importing_a_public_file_by_its_url_gets_the_url() {
    if crate::browser::skip() {
        return;
    }
    let app = app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__LOGO !== undefined", WAIT).await;
    let logo: String = page.eval("window.__LOGO").await;
    assert_eq!(logo, "/assets/logo.png", "the import is the public url");
    let status: u16 = page
        .eval("fetch(window.__LOGO).then((r) => r.status)")
        .await;
    assert_eq!(status, 200, "the url fetches");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// A project whose `src/main.js` is `main`, with a JSON file, a script and a
/// stylesheet in `public/`.
fn non_asset_app(main: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"public-non-asset","version":"1.0.0"}"#,
        ),
        ("src/main.js", main),
        ("public/data.json", "{\"a\":1}\n"),
        ("public/lib.js", "window.__LIB = 1;\n"),
        ("public/theme.css", "body { color: red }\n"),
        ("index.html", INDEX_HTML),
    ])
}

// Vite's importAnalysis refuses an import of a file in the public dir that
// is not an asset, before resolving it: such a file is copied as it is by
// the build and has no module. oj left the import as written, and the
// browser failed on the file's MIME type with nothing saying why.
#[tokio::test(flavor = "multi_thread")]
async fn importing_a_non_asset_public_file_fails_with_vites_error() {
    for (spec, with_url) in [
        ("/data.json", "/data.json?url"),
        ("/lib.js", "/lib.js?url"),
        ("/theme.css", "/theme.css?url"),
        ("/data.json?raw", "/data.json?raw&url"),
    ] {
        let app = non_asset_app(&format!(
            "import \"./ok.js\";\nimport thing from \"{spec}\";\nwindow.__THING = thing;\n"
        ));
        app.write(&[("src/ok.js", "export {};\n")]);
        let server = app.serve().await;
        let res = server.get("/src/main.js").await;
        assert_ne!(res.status, 200, "{spec} was served:\n{}", res.body);
        let said = format!(
            "Cannot import non-asset file {spec} which is inside /public. JS/CSS files inside \
             /public are copied as-is on build and can only be referenced via <script src> or \
             <link href> in html. If you want to get the URL of that file, use {with_url} instead."
        );
        assert!(
            res.body.contains(&said),
            "{spec}: Vite's message, word for word:\n{}",
            res.body
        );
        assert!(
            res.body.contains("src/main.js:2:20"),
            "{spec}: the error points at the specifier:\n{}",
            res.body
        );
    }
}

// What the error tells the user to write works: `?url` is the file's url.
// A dynamic import the analysis cannot read is still a request the server
// answers, and the file itself (a `<script src>`, a `<link href>`) is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_asset_public_file_is_importable_by_its_url_and_served_as_a_file() {
    let app = non_asset_app(
        "import data from \"/data.json?url\";\nimport lib from \"/lib.js?url\";\n\
         window.__URLS = [data, lib];\n",
    );
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"/data.json?url\"") && main.contains("\"/lib.js?url\""),
        "the `?url` imports are left as written:\n{main}"
    );
    for (url, exported) in [("/data.json?url", "/data.json"), ("/lib.js?url", "/lib.js")] {
        let body = server.text(url).await;
        assert!(
            body.contains(&format!("export default \"{exported}\"")),
            "{url}:\n{body}"
        );
    }
    assert_eq!(server.text("/lib.js").await, "window.__LIB = 1;\n");
    assert_eq!(server.text("/theme.css").await, "body { color: red }\n");
}

// A file of the same name under the root is not a public file: the check is
// Vite's checkPublicFile, on the public dir alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_root_file_that_is_not_in_public_is_imported_as_ever() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"public-non-asset","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "import { x } from \"/src/x.js\";\nwindow.__X = x;\n",
        ),
        ("src/x.js", "export const x = 1;\n"),
        ("public/other.js", "window.__OTHER = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(main.contains("\"/src/x.js\""), "{main}");
}

// Vite's servePublicMiddleware runs ahead of its transform and static
// middlewares: a path naming a public file is that file, as it is, beside a
// file of the same name under the root too. Only a module request
// (`?import`, `?url`) and `/` go past the public file.
#[tokio::test(flavor = "multi_thread")]
async fn a_public_file_is_served_ahead_of_a_root_file_of_the_same_name() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"public-shadow","version":"1.0.0"}"#,
        ),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("index.html", INDEX_HTML),
        ("data.json", "{\"root\":1}\n"),
        ("locales/en.json", "{\"root\":2}\n"),
        ("lib.js", "window.__ROOT = 1;\n"),
        ("public/data.json", "{\"public\":1}\n"),
        ("public/other.json", "{\"other\":1}\n"),
        ("public/locales/en.json", "{\"public\":2}\n"),
        ("public/lib.js", "window.__PUBLIC = 1;\n"),
        ("public/index.html", "<p>public page</p>\n"),
    ]);
    let server = app.serve().await;

    let plain = [
        ("/data.json", "{\"public\":1}\n", "application/json"),
        ("/other.json", "{\"other\":1}\n", "application/json"),
        ("/locales/en.json", "{\"public\":2}\n", "application/json"),
        ("/lib.js", "window.__PUBLIC = 1;\n", "text/javascript"),
        ("/data.json?t=1", "{\"public\":1}\n", "application/json"),
        ("/index.html", "<p>public page</p>\n", "text/html"),
    ];
    let shapes: [&[(&str, &str)]; 3] = [
        &[],
        &[("accept", "application/json")],
        &[
            ("accept", "*/*"),
            ("sec-fetch-dest", "empty"),
            ("referer", "/src/main.js"),
        ],
    ];
    for (url, body, content_type) in plain {
        for shape in shapes {
            let mut req = Request::get(url);
            for (name, value) in shape {
                req = req.header(*name, *value);
            }
            let res = server.request(req.body(Body::empty()).unwrap()).await;
            assert_eq!(res.status, 200, "{url} {shape:?}: {}", res.body);
            assert_eq!(res.body, body, "{url} {shape:?} is the public file");
            assert!(
                res.headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with(content_type),
                "{url} {shape:?}: {:?}",
                res.headers["content-type"]
            );
        }
    }

    // A module request is the transform pipeline's, on the root file.
    let module = server.get("/data.json?import").await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert_eq!(module.headers["content-type"], "text/javascript");
    assert!(module.body.contains("\"root\":1"), "{}", module.body);
    let url = server.text("/data.json?url").await;
    assert!(url.contains("export default \"/data.json\""), "{url}");

    // `/` is the root page, never `public/index.html`.
    let page = server.text("/").await;
    assert!(page.contains("/src/main.js"), "{page}");
}

/// A module importing a root JSON file that has a public twin, as a module
/// and as `?raw`, and fetching the public file.
fn twin_app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"public-twin","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "import data from \"../data.json\";\nimport raw from \"../data.json?raw\";\n\
             window.__DATA = data;\nwindow.__RAW = raw;\n\
             fetch(\"/data.json\").then((r) => r.text()).then((t) => { window.__FETCHED = t; });\n",
        ),
        ("index.html", INDEX_HTML),
        ("data.json", "{\"root\":1}\n"),
        ("public/data.json", "{\"public\":1}\n"),
    ])
}

// The import of a root file that is neither a script nor a stylesheet is
// named with `?import`, as Vite's importAnalysis names it
// (`isExplicitImportRequired`), so it is the root file's module whether or
// not the request carries `Sec-Fetch-Dest` (plain http to a LAN address, a
// module worker, a proxy that drops the header). A url without the mark is
// the public file whatever the headers, as Vite's servePublicMiddleware,
// which reads none, serves it.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_of_a_root_file_with_a_public_twin_is_the_root_module() {
    let app = twin_app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"/data.json?import\"") && main.contains("\"/data.json?import&raw\""),
        "the imports carry Vite's mark:\n{main}"
    );
    let request = |url: &'static str, dest: Option<&'static str>| {
        let mut req = Request::get(url).header("accept", "*/*");
        if let Some(dest) = dest {
            req = req.header("sec-fetch-dest", dest);
        }
        req.body(Body::empty()).unwrap()
    };
    for (url, dest) in [
        ("/data.json?import", None),
        ("/data.json?import", Some("script")),
        ("/data.json?import", Some("worker")),
    ] {
        let data = server.request(request(url, dest)).await;
        assert_eq!(data.status, 200, "{url} {dest:?}: {}", data.body);
        assert_eq!(
            data.headers["content-type"], "text/javascript",
            "{url} {dest:?}"
        );
        assert!(
            data.body.contains("\"root\":1"),
            "{url} {dest:?}: the root module:\n{}",
            data.body
        );
    }
    let raw = server.request(request("/data.json?import&raw", None)).await;
    assert_eq!(raw.status, 200, "{}", raw.body);
    assert_eq!(raw.headers["content-type"], "text/javascript");
    assert!(
        raw.body.contains(r#"export default "{\"root\":1}\n""#),
        "the root file's text:\n{}",
        raw.body
    );
    for (url, dest) in [("/data.json", None), ("/data.json", Some("script"))] {
        let public = server.request(request(url, dest)).await;
        assert_eq!(
            public.body, "{\"public\":1}\n",
            "{url} {dest:?} is the public file"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_importing_a_root_file_with_a_public_twin_gets_the_root_file() {
    if crate::browser::skip() {
        return;
    }
    let app = twin_app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__FETCHED !== undefined", WAIT).await;
    let data: String = page.eval("JSON.stringify(window.__DATA)").await;
    assert_eq!(data, "{\"root\":1}", "the import is the root module");
    let raw: String = page.eval("window.__RAW").await;
    assert_eq!(raw, "{\"root\":1}\n", "the `?raw` import is the root text");
    let fetched: String = page.eval("window.__FETCHED").await;
    assert_eq!(fetched, "{\"public\":1}\n", "the fetch is the public file");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// `server.get` sends no `Sec-Fetch-Dest`: what a browser sends to a
/// non-trustworthy origin (`oj dev --host` from another machine), an older
/// Safari, or a request through a proxy that drops the header.
async fn headerless(server: &crate::Server, url: &str) -> crate::Response {
    server
        .request(
            Request::get(url)
                .header("accept", "*/*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

/// The url `importer` names a module by: the quoted import that starts with
/// `prefix`.
fn imported_url(importer: &str, prefix: &str) -> String {
    let start = importer
        .find(&format!("\"{prefix}"))
        .unwrap_or_else(|| panic!("no import of {prefix}:\n{importer}"))
        + 1;
    let rest = &importer[start..];
    rest[..rest.find('"').unwrap()].to_string()
}

/// A JSON module of `src/data.json` (`"root"`), never the public file's bytes.
fn assert_root_module(res: &crate::Response, what: &str) {
    assert_eq!(res.status, 200, "{what}: {}", res.body);
    assert_eq!(res.headers["content-type"], "text/javascript", "{what}");
    assert!(
        res.body.contains("\"root\""),
        "{what}: the root module:\n{}",
        res.body
    );
}

// A public twin created while the server runs: the importer, compiled
// before it existed, is not recompiled, so its import must not depend on the
// public dir: Vite names it `/src/data.json?import` either way. Asserted:
// that url, asked for without `Sec-Fetch-Dest`, is the root module.
#[tokio::test(flavor = "multi_thread")]
async fn a_public_twin_created_while_dev_runs_does_not_shadow_an_import_without_fetch_metadata() {
    let app = App::new(&[
        ("package.json", r#"{"name":"late-twin","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import data from \"./data.json\";\nwindow.__DATA = data;\n",
        ),
        ("src/data.json", "{\"from\":\"root\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::create_dir_all(app.root.join("public")).unwrap();
    let server = app.serve().await;
    let before = server.text("/src/main.js").await;
    app.write(&[("public/src/data.json", "{\"from\":\"public\"}\n")]);
    let importer = server.text("/src/main.js").await;
    assert_eq!(
        imported_url(&before, "/src/data.json"),
        imported_url(&importer, "/src/data.json"),
        "the importer is the one compiled before the twin existed"
    );
    let url = imported_url(&importer, "/src/data.json");
    assert_root_module(&headerless(&server, &url).await, &url);
    // The public file is still what a plain fetch of the url gets.
    let fetched = headerless(&server, "/src/data.json").await;
    assert_eq!(fetched.body, "{\"from\":\"public\"}\n");
}

// A dynamic import whose specifier is computed: Vite wraps it in
// `__vite__injectQuery(x, 'import')`, so the browser asks for
// `/src/dyn.json?import`, the root module, while a plain fetch of
// `/src/dyn.json` is the public twin.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_literal_dynamic_import_of_a_root_file_with_a_public_twin_is_the_module_without_fetch_metadata(
) {
    let app = App::new(&[
        ("package.json", r#"{"name":"dyn-twin","version":"1.0.0"}"#),
        (
            "src/main.js",
            "const name = \"./dyn.json\";\nimport(name).then((m) => { window.__DYN = m.default; });\n",
        ),
        ("src/dyn.json", "{\"from\":\"root\"}\n"),
        ("public/src/dyn.json", "{\"from\":\"public\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("import { injectQuery as __vite__injectQuery } from \"/@oj/client.js\";")
            && main.contains("import(__vite__injectQuery(name, \"import\"))"),
        "the computed import asks for the module, as Vite's importAnalysis writes it:\n{main}"
    );
    let client = server.text("/@oj/client.js").await;
    assert!(
        client.contains("export function injectQuery("),
        "the client exports the helper"
    );
    // `injectQuery("./dyn.json", "import")`, resolved against the importer.
    assert_root_module(
        &headerless(&server, "/src/dyn.json?import").await,
        "/src/dyn.json?import",
    );
    let fetched = headerless(&server, "/src/dyn.json").await;
    assert_eq!(
        fetched.body, "{\"from\":\"public\"}\n",
        "a plain fetch is the public file"
    );
}

// The hot update of an accepted JSON module: Vite's update carries
// `explicitImportRequired` and its client re-imports `?import&t=`. Asserted:
// the flag, and that url without `Sec-Fetch-Dest` is the new root module.
#[tokio::test(flavor = "multi_thread")]
async fn the_hot_update_of_an_accepted_json_module_with_a_public_twin_is_the_module_without_fetch_metadata(
) {
    let app = App::new(&[
        ("package.json", r#"{"name":"hot-twin","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import hot from \"./hot.json\";\nwindow.__HOT = hot;\n\
             if (import.meta.hot) import.meta.hot.accept(\"./hot.json\", (m) => { window.__HOT = m.default; });\n",
        ),
        ("src/hot.json", "{\"from\":\"root\",\"n\":1}\n"),
        ("public/src/hot.json", "{\"from\":\"public\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let url = imported_url(&main, "/src/hot.json");
    assert_root_module(&headerless(&server, &url).await, &url);

    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    app.write(&[("src/hot.json", "{\"from\":\"root\",\"n\":2}\n")]);
    let frame =
        crate::listen::frame_matching(&mut socket, "the json's update", |f| f["type"] == "update")
            .await;
    crate::listen::close(socket).await;
    let update = &frame["updates"][0];
    assert_eq!(update["acceptedPath"], "/src/hot.json", "{frame}");
    assert_eq!(
        update["explicitImportRequired"], true,
        "the update says the module needs `import`, as Vite's does: {frame}"
    );
    let client = server.text("/@oj/client.js").await;
    assert!(
        client.contains("explicitImportRequired") && client.contains("?import&t="),
        "the client re-imports with `import&`, as Vite's importUpdatedModule does"
    );
    // The url the client asks for: `${acceptedPath}?import&t=${timestamp}`.
    let next = format!("/src/hot.json?import&t={}", update["timestamp"]);
    let res = headerless(&server, &next).await;
    assert_root_module(&res, &next);
    assert!(
        res.body.contains("\"n\": 2") || res.body.contains("\"n\":2"),
        "{}",
        res.body
    );
}

// The url of a public file with a root twin, asked for by a plain fetch, is
// the public file's bytes, headers or not (Vite's servePublicMiddleware).
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_fetch_of_a_public_file_with_a_root_twin_is_the_file_without_fetch_metadata() {
    let app = twin_app();
    let server = app.serve().await;
    for url in ["/data.json", "/data.json?t=1"] {
        let res = headerless(&server, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(res.body, "{\"public\":1}\n", "{url} is the public file");
        assert!(
            res.headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{url}: {:?}",
            res.headers["content-type"]
        );
    }
}

// `?import` is the transform pipeline's flag, not part of the module's id
// (Vite's removeImportQuery): a root file asked for with and without it is
// one module, under one key.
#[tokio::test(flavor = "multi_thread")]
async fn a_marked_and_an_unmarked_request_for_a_root_file_are_one_module() {
    let app = App::new(&[
        ("package.json", r#"{"name":"one-module","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import data from \"./data.json\";\nwindow.__DATA = data;\n",
        ),
        ("src/data.json", "{\"from\":\"root\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"/src/data.json?import\""),
        "the import carries Vite's mark:\n{main}"
    );
    let marked = headerless(&server, "/src/data.json?import").await;
    let unmarked = headerless(&server, "/src/data.json").await;
    assert_root_module(&marked, "?import");
    assert_root_module(&unmarked, "bare");
    assert_eq!(marked.body, unmarked.body, "one module's code");
    assert_eq!(
        marked.headers.get("etag"),
        unmarked.headers.get("etag"),
        "one module's key"
    );
    assert!(
        !marked.body.contains("?import\""),
        "the module names itself by its bare url:\n{}",
        marked.body
    );
}

/// A worker whose computed `import(n)` names a JSON file.
fn worker_app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"worker-dyn","version":"1.0.0"}"#),
        ("src/main.js", "window.__MAIN = 1;\n"),
        (
            "src/w.js",
            "const n = \"./data.json\";\nimport(n).then((m) => postMessage(m.default));\n",
        ),
        ("src/data.json", "{\"from\":\"root\"}\n"),
        ("index.html", INDEX_HTML),
    ])
}

// A classic worker cannot hold a static import: Vite appends its helper to
// a classic worker only (importAnalysis.ts `isClassicWorker`), and a module
// worker imports `injectQuery` from the dev client. oj differs: it writes the
// helper into both. Asserted: each worker file holds it and no client import.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_with_a_computed_import_gets_the_helper_not_an_import_of_the_client() {
    let app = worker_app();
    let server = app.serve().await;
    for url in [
        "/src/w.js?worker_file&type=classic",
        "/src/w.js?worker_file&type=module",
    ] {
        let body = server.text(url).await;
        assert!(
            body.contains("function __vite__injectQuery(url, queryToInject)")
                && body.contains("import(__vite__injectQuery(n, \"import\"))"),
            "{url}: the computed import calls the helper written in:\n{body}"
        );
        assert!(
            !body.contains("/@oj/client.js") && !body.contains("import {"),
            "{url}: no import of the dev client:\n{body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_classic_worker_with_a_computed_import_runs() {
    if crate::browser::skip() {
        return;
    }
    let app = worker_app();
    app.write(&[(
        "src/main.js",
        "const w = new Worker(\"/src/w.js?worker_file&type=classic\");\n\
         w.onmessage = (e) => { window.__FROM = e.data.from; };\n\
         w.onerror = (e) => { window.__FROM = \"error: \" + e.message; };\n",
    )]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__FROM !== undefined", WAIT).await;
    let from: String = page.eval("window.__FROM").await;
    assert_eq!(from, "root", "the worker ran and imported the module");
    page.close().await;
}

// Vite wraps a template literal only when its text needs `?import`: a
// script's (`/src/pages/${n}`, no extension) is left as written, so it is
// the same browser module as the page's static import of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_computed_import_of_a_script_is_left_as_written() {
    let app = App::new(&[
        ("package.json", r#"{"name":"tpl-script","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import { a } from \"/src/pages/a.js\";\nconst n = \"a.js\";\nimport(`/src/pages/${n}`);\nwindow.__A = a;\n",
        ),
        ("src/pages/a.js", "export const a = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("import(`/src/pages/${n}`)") && !main.contains("__vite__injectQuery"),
        "the script's computed import is left as written:\n{main}"
    );
    assert!(
        main.contains("from \"/src/pages/a.js\""),
        "the static import names the script unmarked:\n{main}"
    );
}

// A root-absolute import with an asset query is marked as a relative one is:
// Vite writes `/src/abs.json?import&raw`, which a public twin created later
// does not shadow.
#[tokio::test(flavor = "multi_thread")]
async fn a_root_absolute_import_with_a_query_carries_the_mark() {
    let app = App::new(&[
        ("package.json", r#"{"name":"abs-raw","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import raw from \"/src/abs.json?raw\";\nimport data from \"/src/abs.json\";\nwindow.__ABS = [raw, data];\n",
        ),
        ("src/abs.json", "{\"from\":\"root\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"/src/abs.json?import&raw\"") && main.contains("\"/src/abs.json?import\""),
        "{main}"
    );
    let raw = headerless(&server, "/src/abs.json?import&raw").await;
    assert!(
        raw.body
            .contains(r#"export default "{\"from\":\"root\"}\n""#),
        "{}",
        raw.body
    );
}

// The page's hot update of an accepted JSON module with a public twin: the
// client re-imports `?import&t=`, which the public dir steps aside for, and
// the page shows the new value without a reload.
#[tokio::test(flavor = "multi_thread")]
async fn page_hot_updates_an_accepted_json_module_with_a_public_twin() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"hot-twin-page","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import hot from \"./hot.json\";\nwindow.__HOT = hot.n;\n\
             if (import.meta.hot) import.meta.hot.accept(\"./hot.json\", (m) => { window.__HOT = m.default.n; });\n",
        ),
        ("src/hot.json", "{\"n\":1}\n"),
        ("public/src/hot.json", "{\"n\":\"public\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__HOT === 1", WAIT).await;
    assert!(page.eval::<bool>("window.__KEPT = true").await);
    app.write(&[("src/hot.json", "{\"n\":2}\n")]);
    page.wait_for("window.__HOT === 2", WAIT).await;
    assert!(
        page.eval::<bool>("window.__KEPT === true").await,
        "the update reloaded the page"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

// A module worker whose module makes a computed `import()` loads the dev
// client for the helper. Vite's client keeps the page-only work behind
// `hasDocument`: a worker shows no overlay and reloads nothing. Asserted: the
// worker posts `root`, then `after-error` from its `vite:error` listener, and
// no error between them.
#[tokio::test(flavor = "multi_thread")]
async fn page_module_worker_importing_the_client_survives_a_server_error() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"worker-client","version":"1.0.0"}"#),
        (
            "src/main.js",
            "const w = new Worker(\"/src/w.js?worker_file&type=module\", { type: \"module\" });\n\
             window.__MSGS = [];\n\
             w.onmessage = (e) => { window.__MSGS.push(e.data); };\n\
             w.onerror = (e) => { window.__MSGS.push(\"onerror:\" + (e.message || e)); };\n",
        ),
        (
            "src/w.js",
            "self.addEventListener(\"error\", (e) => postMessage(\"error:\" + e.message));\n\
             import { load } from \"./lib.js\";\nload().then((v) => postMessage(v));\n",
        ),
        (
            "src/lib.js",
            "export const load = () => { const n = \"./data.json\"; return import(n).then((m) => m.default.from); };\n\
             import.meta.hot.on(\"vite:error\", () => setTimeout(() => postMessage(\"after-error\")));\n",
        ),
        ("src/data.json", "{\"from\":\"root\"}\n"),
        ("src/broken.js", "export const x = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let lib = server.text("/src/lib.js").await;
    assert!(
        lib.contains("from \"/@oj/client.js\""),
        "the worker's module imports the client for the helper:\n{lib}"
    );
    let listening = server.listen().await;
    let before = server.built.reload_tx.receiver_count();
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__MSGS.length > 0", WAIT).await;
    let first: Vec<String> = page.eval("window.__MSGS").await;
    assert_eq!(first, ["root"], "the worker ran and imported the module");
    // The page's socket and the worker's.
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.built.reload_tx.receiver_count() < before + 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the worker's client never connected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    app.write(&[("src/broken.js", "export const = ;\n")]);
    let broken = server.get("/src/broken.js").await;
    assert_ne!(
        broken.status, 200,
        "the compile error is sent as an error frame"
    );
    page.wait_for("window.__MSGS.includes(\"after-error\")", WAIT)
        .await;
    let msgs: Vec<String> = page.eval("window.__MSGS").await;
    assert_eq!(
        msgs,
        ["root", "after-error"],
        "the worker's client threw on the error frame"
    );
    page.close().await;
}

// Vite's importQueryRE (`(\?|&)import=?(?:&|$)`) takes `import=` as the flag
// too: `/src/logo.png?import=` is the asset's url module, as `?import` is.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_flag_with_an_equals_sign_is_a_module_request() {
    let app = App::new(&[
        ("package.json", r#"{"name":"import-eq","version":"1.0.0"}"#),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/logo.png"), PNG_1X1).unwrap();
    let server = app.serve().await;
    for url in ["/src/logo.png?import", "/src/logo.png?import="] {
        let res = headerless(&server, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(res.headers["content-type"], "text/javascript", "{url}");
        assert!(
            res.body.contains("export default \"/src/logo.png\""),
            "{url}:\n{}",
            res.body
        );
    }
}

/// Every quoted url in `importer` that starts with `prefix`, in order.
fn imported_urls(importer: &str, prefix: &str) -> Vec<String> {
    importer
        .match_indices(&format!("\"{prefix}"))
        .map(|(at, _)| {
            let rest = &importer[at + 1..];
            rest[..rest.find('"').unwrap()].to_string()
        })
        .collect()
}

// Vite marks an import whatever its query (`injectQuery(url, 'import')`):
// `import` goes first in the query and the hash stays last, so the url the
// browser sends is a module request the public twin steps aside for. The
// root-absolute imports name a file with no twin: Vite refuses one that has.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_with_any_query_or_a_hash_beside_a_public_twin_is_the_root_module_without_fetch_metadata(
) {
    let app = App::new(&[
        ("package.json", r#"{"name":"twin-query","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import a from \"./data.json?raw&inline\";\nimport b from \"./data.json?x=1\";\n\
             import c from \"./data.json#x\";\nimport d from \"/src/abs.json?x=2\";\n\
             import e from \"/src/abs.json#y\";\nimport u from \"./util.js?x=1\";\n\
             window.__D = [a, b, c, d, e, u];\n",
        ),
        ("src/util.js", "export default 1;\n"),
        ("src/abs.json", "{\"v\":\"root\"}\n"),
        ("src/data.json", "{\"v\":\"root\"}\n"),
        ("public/src/data.json", "{\"v\":\"public\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let mut urls = imported_urls(&main, "/src/data.json");
    urls.extend(imported_urls(&main, "/src/abs.json"));
    assert_eq!(urls.len(), 5, "{main}");
    for url in &urls {
        let sent = url.split('#').next().unwrap();
        let res = headerless(&server, sent).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(
            res.headers["content-type"], "text/javascript",
            "{url}: {}",
            res.body
        );
        assert!(
            res.body.contains("root") && !res.body.contains("public"),
            "{url}: the root module, not the public file:\n{}",
            res.body
        );
    }
    assert_eq!(
        urls,
        [
            "/src/data.json?import&raw&inline",
            "/src/data.json?import&x=1",
            "/src/data.json?import#x",
            "/src/abs.json?import&x=2",
            "/src/abs.json?import#y",
        ],
        "{main}"
    );
    assert_eq!(
        imported_urls(&main, "/src/util.js"),
        ["/src/util.js?x=1"],
        "{main}"
    );
}

// The mark changes only which file answers, never which loader runs: a
// marked url gets what its unmarked spelling gets where no public twin is.
#[tokio::test(flavor = "multi_thread")]
async fn a_marked_query_import_runs_the_loader_its_unmarked_spelling_runs() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"mark-loader","version":"1.0.0"}"#,
        ),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("src/solo.json", "{\"v\":\"solo\"}\n"),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/logo.png"), PNG_1X1).unwrap();
    let server = app.serve().await;
    for (file, query) in [
        ("solo.json", "raw"),
        ("solo.json", "raw&inline"),
        ("solo.json", "inline"),
        ("solo.json", "x=1"),
        ("logo.png", "inline"),
        ("logo.png", "x=1&url"),
    ] {
        let unmarked = headerless(&server, &format!("/src/{file}?{query}")).await;
        let marked = headerless(&server, &format!("/src/{file}?import&{query}")).await;
        assert_eq!(unmarked.status, 200, "{file}?{query}: {}", unmarked.body);
        assert_eq!(
            (marked.status, &marked.headers["content-type"], &marked.body),
            (
                unmarked.status,
                &unmarked.headers["content-type"],
                &unmarked.body
            ),
            "{file}?import&{query} against {file}?{query}"
        );
    }
}

const REWRITE_TWIN: &str = r#"
const rewriteTwin = {
  name: "rewrite-twin",
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      const [path, query] = req.url.split("?");
      if (path === "/twin") req.url = "/data.json" + (query ? "?" + query : "");
      next();
    });
  },
};
export default { plugins: [rewriteTwin] };
"#;

// A plugin middleware runs ahead of Vite's servePublicMiddleware, so its rewrite is served as asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_url_a_plugin_rewrites_to_a_public_file_with_a_root_twin_is_the_public_file() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"rewrite-twin","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", REWRITE_TWIN),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("index.html", INDEX_HTML),
        ("data.json", "{\"root\":1}\n"),
        ("public/data.json", "{\"public\":1}\n"),
    ]);
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let res = headerless(&server, "/twin").await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body, "{\"public\":1}\n", "/twin is the public file");
    assert!(
        res.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "{:?}",
        res.headers["content-type"]
    );
    assert_root_module(&headerless(&server, "/twin?import").await, "/twin?import");
}

// Vite's baseMiddleware strips the base before servePublicMiddleware runs.
#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_a_public_file_is_served_ahead_of_its_root_twin() {
    let app = App::new(&[
        ("package.json", r#"{"name":"base-twin","version":"1.0.0"}"#),
        ("vite.config.js", "export default { base: \"/app/\" };\n"),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("index.html", INDEX_HTML),
        ("data.json", "{\"root\":1}\n"),
        ("public/data.json", "{\"public\":1}\n"),
    ]);
    let server = app.serve().await;
    for url in ["/app/data.json", "/app/data.json?t=1"] {
        let res = headerless(&server, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(res.body, "{\"public\":1}\n", "{url} is the public file");
    }
    assert_root_module(
        &headerless(&server, "/app/data.json?import").await,
        "/app/data.json?import",
    );
}

// Vite's importQueryRE takes `import=` as the flag on a stylesheet too.
#[tokio::test(flavor = "multi_thread")]
async fn a_stylesheet_import_flag_with_an_equals_sign_is_a_module_request() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-import-eq","version":"1.0.0"}"#,
        ),
        ("src/main.js", "window.__MAIN = 1;\n"),
        ("src/a.css", ".a { color: red; }\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    for url in ["/src/a.css?import", "/src/a.css?import="] {
        let res = headerless(&server, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(res.headers["content-type"], "text/javascript", "{url}");
        assert!(res.body.contains("red"), "{url}:\n{}", res.body);
    }
    for url in ["/src/a.css?import&url", "/src/a.css?import=&url"] {
        let res = headerless(&server, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        // oj's `?direct` on a stylesheet's url module is an older difference from Vite.
        let body = res.body.replace("?direct", "");
        assert_eq!(body.trim_end(), "export default \"/src/a.css\";", "{url}");
    }
}
