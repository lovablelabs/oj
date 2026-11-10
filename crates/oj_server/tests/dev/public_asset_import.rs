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
