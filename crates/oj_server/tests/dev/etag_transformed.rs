// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module's ETag is of its transformed output, as under Vite (the
//! transform result's `etag` is a hash of the code it serves), not of the
//! compile's inputs. The output can change with the file unchanged: an
//! import that resolves elsewhere once a dependency's `exports` moved a
//! target (a workspace package's package.json, whose change restarts the
//! dev server), or a glob whose matches changed. The ETag used to be the
//! compile key, which is of the source, the url and the compile settings:
//! after the restart the importer compiled to new code under the same key,
//! the browser's `If-None-Match` was answered 304, and the page ran its old
//! copy, which imported a path the server no longer had (404).

use axum::body::Body;
use axum::http::Request;

use crate::{App, INDEX_HTML};

/// The dependency's package.json, exporting `./x` from `file`.
fn exports(file: &str) -> String {
    format!(r#"{{"name":"dep","version":"1.0.0","type":"module","exports":{{"./x":"./{file}"}}}}"#)
}

/// An app importing `dep/x` from a workspace package linked into
/// node_modules, as a monorepo's are.
fn app() -> App {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"etag-app","version":"1.0.0","dependencies":{"dep":"1.0.0"}}"#,
        ),
        ("packages/dep/package.json", &exports("a.js")),
        ("packages/dep/a.js", "export const X = \"from a\";\n"),
        ("packages/dep/b.js", "export const X = \"from b\";\n"),
        (
            "src/main.js",
            "import { X } from \"dep/x\";\nwindow.__X = X;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let link = app.root.join("node_modules/dep");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../packages/dep", link).unwrap();
    app
}

fn with_etag(path: &str, etag: &str) -> Request<Body> {
    Request::get(path)
        .header("if-none-match", etag)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_whose_import_resolves_elsewhere_gets_a_new_etag_with_its_file_unchanged() {
    let app = app();
    let (first_etag, first_body) = {
        let server = app.serve().await;
        let first = server.get("/src/main.js").await;
        assert_eq!(first.status, 200, "{}", first.body);
        let etag = first.headers["etag"].to_str().unwrap().to_string();
        assert!(
            first.body.contains("a.js"),
            "the import does not resolve to the package's export:\n{}",
            first.body
        );
        // The same output again: the browser's copy is current.
        let again = server.request(with_etag("/src/main.js", &etag)).await;
        assert_eq!(again.status, 304, "{}", again.body);
        (etag, first.body)
    };

    // The package moves the export; its package.json change restarts the
    // dev server, which compiles the unchanged importer anew.
    app.write(&[("packages/dep/package.json", &exports("b.js"))]);
    let server = app.serve().await;
    let moved = server.request(with_etag("/src/main.js", &first_etag)).await;
    assert_eq!(
        moved.status, 200,
        "the importer's output changed, and its old ETag was taken for current:\n{}",
        moved.body
    );
    assert!(
        moved.body.contains("b.js") && !moved.body.contains("a.js"),
        "the importer does not import the moved export:\n{}",
        moved.body
    );
    assert_ne!(first_body, moved.body);
    let moved_etag = moved.headers["etag"].to_str().unwrap().to_string();
    assert_ne!(moved_etag, first_etag, "the ETag follows the output");
    // Its new copy is current, and stays so across a server that compiles
    // the same output.
    let current = server.request(with_etag("/src/main.js", &moved_etag)).await;
    assert_eq!(current.status, 304, "{}", current.body);
    drop(server);
    let server = app.serve().await;
    let same = server.request(with_etag("/src/main.js", &moved_etag)).await;
    assert_eq!(same.status, 304, "{}", same.body);
}
