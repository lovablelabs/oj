// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's `assetsInclude`: a file the config names, of a type oj does not
//! know as an asset, is a URL module when a script imports it and the file
//! itself when fetched, as `vite:asset` admits it through
//! `config.assetsInclude(file)` (a glob, or a RegExp) beside the known types.
//! A file of an unknown type the config does not name stays what it was.

use axum::body::Body;
use axum::http::Request;

use crate::{App, Server, INDEX_HTML};

const SCENE: &str = r#"{"asset":{"version":"2.0"}}"#;

/// `path` as a module import fetches it: the browser's `sec-fetch-dest: script`.
async fn imported(server: &Server, path: &str) -> crate::Response {
    server
        .request(
            Request::get(path)
                .header("sec-fetch-dest", "script")
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_assets_include_names_imports_as_its_url() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"assets-include","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            "export default { assetsInclude: [\"**/*.gltf\", /\\.bin$/] };\n",
        ),
        ("src/scene.gltf", SCENE),
        ("src/data.bin", "bin-bytes"),
        ("src/notes.xyz", "export const notes = 1;\n"),
        (
            "src/main.js",
            "import scene from \"./scene.gltf\";\nimport data from \"./data.bin\";\nwindow.__SCENE = scene;\nwindow.__DATA = data;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;

    // The importer names the files by their served urls.
    let main = server.text("/src/main.js").await;
    assert!(main.contains("/src/scene.gltf"), "{main}");
    assert!(main.contains("/src/data.bin"), "{main}");

    // Imported, each is a module whose default export is its url: the glob's
    // match and the RegExp's.
    for (path, what) in [
        ("/src/scene.gltf", "the glob"),
        ("/src/data.bin", "the RegExp"),
    ] {
        let module = imported(&server, path).await;
        assert_eq!(module.status, 200, "{what}: {}", module.body);
        assert!(
            module.body.contains("export default") && module.body.contains(path),
            "{what} did not make {path} a URL module:\n{}",
            module.body
        );
    }

    // Fetched as a resource, the file is its own bytes.
    let raw = server.get("/src/scene.gltf").await;
    assert_eq!(raw.status, 200, "{}", raw.body);
    assert_eq!(raw.body, SCENE);

    // A type the config does not name is no asset: imported, it is served as
    // the script it is.
    let notes = imported(&server, "/src/notes.xyz").await;
    assert_eq!(notes.status, 200, "{}", notes.body);
    assert!(
        notes.body.contains("notes = 1")
            && !notes.body.contains("export default \"/src/notes.xyz\""),
        "an unnamed type became a URL module:\n{}",
        notes.body
    );
}

fn typed_app(name: &str, assets_include: &str) -> App {
    App::new(&[
        (
            "package.json",
            &format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        ),
        (
            "vite.config.mjs",
            &format!("export default {{ assetsInclude: {assets_include} }};\n"),
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "console.log(1);\n"),
        ("src/scene.gltf", SCENE),
        ("src/mesh.obj", "o mesh\n"),
    ])
}

/// What `path` is when a script imports it from `server`: a URL module, or
/// the file's own text served as a script.
async fn import_kind(server: &Server, path: &str) -> &'static str {
    let module = imported(server, path).await;
    assert_eq!(module.status, 200, "{path}: {}", module.body);
    if module.body.contains("export default") && module.body.contains(path) {
        "url module"
    } else {
        "script"
    }
}

/// Each server judges `assetsInclude` by its own config, as Vite's resolved
/// config carries the filter per server. Three servers in one process, each
/// configured after the last: a filter is neither overwritten by the next
/// server's nor dropped by one that names no pattern.
#[tokio::test(flavor = "multi_thread")]
async fn each_server_keeps_its_own_assets_include() {
    let gltf = typed_app("include-gltf", r#"["**/*.gltf"]"#).serve().await;
    let obj = typed_app("include-obj", r#"["**/*.obj"]"#).serve().await;
    let none = App::new(&[
        (
            "package.json",
            r#"{"name":"include-none","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "console.log(1);\n"),
        ("src/scene.gltf", SCENE),
    ])
    .serve()
    .await;

    assert_eq!(import_kind(&gltf, "/src/scene.gltf").await, "url module");
    assert_eq!(import_kind(&gltf, "/src/mesh.obj").await, "script");
    assert_eq!(import_kind(&obj, "/src/mesh.obj").await, "url module");
    assert_eq!(import_kind(&obj, "/src/scene.gltf").await, "script");
    assert_eq!(import_kind(&none, "/src/scene.gltf").await, "script");
    // The first server's filter survives the two configured after it.
    assert_eq!(import_kind(&gltf, "/src/scene.gltf").await, "url module");
}
