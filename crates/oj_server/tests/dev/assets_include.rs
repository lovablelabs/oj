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

// The filter is the app's own, as each Vite server consults its own resolved
// config's `assetsInclude(file)`: a second server booting in the same process
// from a config without `assetsInclude` leaves the first one's as it is, and
// no app is judged by another's patterns, whether its config names other
// types, names none, or does not exist. The process used to hold one filter,
// which a config without `assetsInclude` cleared (the first server then
// served its named files as the files, not as URL modules, whenever another
// test's server booted while this file's test ran) and which every app
// without one was judged by.
#[tokio::test(flavor = "multi_thread")]
async fn another_apps_config_leaves_this_apps_assets_include() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"assets-include-first","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            "export default { assetsInclude: [\"**/*.gltf\", /\\.bin$/] };\n",
        ),
        ("src/scene.gltf", SCENE),
        ("src/data.bin", "bin-bytes"),
        (
            "src/main.js",
            "import scene from \"./scene.gltf\";\nwindow.__SCENE = scene;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let module = imported(&server, "/src/scene.gltf").await;
    assert!(module.body.contains("export default"), "{}", module.body);
    // What an app that names neither type serves for the same two urls:
    // the files, as what they are, never a URL module.
    async fn serves_the_files(server: &Server, which: &str) {
        for path in ["/src/scene.gltf", "/src/data.bin"] {
            let module = imported(server, path).await;
            assert!(
                !module.body.contains(&format!("export default \"{path}\"")),
                "{which}: another app's assetsInclude made {path} a URL module:\n{}",
                module.body
            );
        }
    }

    // Another app, whose config names no extra asset types, and one whose
    // config names its own.
    let plain = App::new(&[
        (
            "package.json",
            r#"{"name":"assets-include-second","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            "export default { define: { __X__: \"1\" } };\n",
        ),
        ("src/scene.gltf", SCENE),
        ("src/data.bin", "bin-bytes"),
        (
            "src/main.js",
            "import scene from \"./scene.gltf\";\nwindow.__SCENE = scene;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let plain_server = plain.serve().await;
    serves_the_files(&plain_server, "a config without assetsInclude").await;
    // An app with no vite.config at all installs no filter of its own.
    let bare = App::new(&[
        (
            "package.json",
            r#"{"name":"assets-include-bare","version":"1.0.0"}"#,
        ),
        ("src/scene.gltf", SCENE),
        ("src/data.bin", "bin-bytes"),
        (
            "src/main.js",
            "import scene from \"./scene.gltf\";\nwindow.__SCENE = scene;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let bare_server = bare.serve().await;
    serves_the_files(&bare_server, "no vite.config").await;
    let other = App::new(&[
        (
            "package.json",
            r#"{"name":"assets-include-third","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            "export default { assetsInclude: [\"**/*.voxels\"] };\n",
        ),
        ("src/a.voxels", "voxels"),
        ("src/scene.gltf", SCENE),
        ("src/data.bin", "bin-bytes"),
        (
            "src/main.js",
            "import v from \"./a.voxels\";\nwindow.__V = v;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let other_server = other.serve().await;
    serves_the_files(&other_server, "a config naming another type").await;

    // The first server's file is still a URL module, after a fresh request
    // for its importer too.
    let main = server.text("/src/main.js").await;
    assert!(main.contains("/src/scene.gltf"), "{main}");
    let module = imported(&server, "/src/scene.gltf?after").await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.body.contains("export default") && module.body.contains("/src/scene.gltf"),
        "another app's config cleared this app's assetsInclude:\n{}",
        module.body
    );
    // And the third's own type is its own.
    let voxels = imported(&other_server, "/src/a.voxels").await;
    assert!(
        voxels.body.contains("export default") && voxels.body.contains("/src/a.voxels"),
        "{}",
        voxels.body
    );
    // And the first app does not take the third's.
    let voxels = imported(&server, "/src/a.voxels").await;
    assert!(
        !voxels.body.contains("export default \"/src/a.voxels\""),
        "{}",
        voxels.body
    );
    // The others answer the same with all three filters installed.
    serves_the_files(&plain_server, "a config without assetsInclude, later").await;
    serves_the_files(&bare_server, "no vite.config, later").await;
}
