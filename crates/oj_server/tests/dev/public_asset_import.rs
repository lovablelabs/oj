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
