// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A JS import that starts with `/` is a file under the root (`/src/x.js`) or,
//! failing that, a public-dir file (`/vite.svg`, as create-vite's template
//! imports its logo). The public file's module exports its URL under `base`,
//! and the file itself is never hashed into assets/.

use super::{build_app, files_under, js_under, scratch, write_files};

fn app(name: &str, base: &str) -> std::path::PathBuf {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"rootabs","private":true}"#),
            ("oj.config.json", &format!(r#"{{"base":"{base}"}}"#)),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("public/vite.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n"),
            (
                "src/main.js",
                "import logo from '/vite.svg';\nimport { x } from '/src/x.js';\nconsole.log(logo, x);\n",
            ),
            ("src/x.js", "export const x = 'from-src-x';\n"),
        ],
    );
    root
}

#[tokio::test]
async fn root_absolute_and_public_imports_resolve_under_an_absolute_base() {
    let root = app("root-abs-imports", "/app/");
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("from-src-x"),
        "/src/x.js was not bundled:\n{js}"
    );
    assert!(
        js.contains("\"/app/vite.svg\"") || js.contains("`/app/vite.svg`"),
        "public URL not under the base:\n{js}"
    );
    assert!(
        root.join("dist/vite.svg").is_file(),
        "public file not copied"
    );
    let hashed = files_under(&root.join("dist/assets"))
        .into_iter()
        .any(|(p, _)| p.ends_with(".svg"));
    assert!(!hashed, "public file was emitted as a hashed asset");
}

#[tokio::test]
async fn public_import_resolves_against_the_chunk_under_a_relative_base() {
    let root = app("root-abs-imports-relative", "./");
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("../vite.svg"),
        "public URL not relative to the chunk:\n{js}"
    );
    assert!(
        root.join("dist/vite.svg").is_file(),
        "public file not written at its own path"
    );
    assert!(!js.contains("__OJ_PUBLIC_ASSET__"), "marker left:\n{js}");
}

/// Vite never emits a public file: with `copyPublicDir: false` the URL still
/// points at it, and nothing writes it.
#[tokio::test]
async fn relative_base_public_import_honors_copy_public_dir_false() {
    let root = app("root-abs-imports-nocopy", "./");
    write_files(
        &root,
        &[(
            "oj.config.json",
            r#"{"base":"./","build":{"copyPublicDir":false}}"#,
        )],
    );
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(js.contains("../vite.svg"), "URL not chunk-relative:\n{js}");
    assert!(
        !root.join("dist/vite.svg").exists(),
        "public file written despite copyPublicDir: false"
    );
}

/// The public URL is encoded as Vite's encodeURIPath does.
#[tokio::test]
async fn public_import_url_is_uri_encoded() {
    let root = app("root-abs-imports-encoded", "/app/");
    write_files(
        &root,
        &[
            (
                "public/my logo.svg",
                "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
            ),
            (
                "src/main.js",
                "import logo from '/my logo.svg';\nconsole.log(logo);\n",
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(js.contains("/app/my%20logo.svg"), "URL not encoded:\n{js}");
}

#[tokio::test]
async fn queried_public_imports_resolve_like_vite_asset() {
    let root = app("root-abs-imports-queried", "/app/");
    write_files(
        &root,
        &[(
            "src/main.js",
            "import url from '/vite.svg?url';\nimport raw from '/vite.svg?raw';\nimport inline from '/vite.svg?inline';\nconsole.log(url, raw, inline);\n",
        )],
    );
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("/app/vite.svg"),
        "?url not the public URL:\n{js}"
    );
    assert!(
        js.contains("http://www.w3.org/2000/svg"),
        "?raw not the public file's text:\n{js}"
    );
    assert!(
        js.contains("data:image/svg+xml"),
        "?inline not a data URL:\n{js}"
    );
}

/// Vite resolves a relative base to `/` for an SSR build: the server's public
/// URL is root-absolute, never a `file://` URL off the server chunk.
#[tokio::test]
async fn ssr_public_import_is_root_absolute_under_a_relative_base() {
    let root = app("root-abs-imports-ssr", "./");
    write_files(
        &root,
        &[
            (
                "oj.config.json",
                r#"{"base":"./","build":{"ssr":true,"rollupOptions":{"input":"src/entry-server.js"}}}"#,
            ),
            (
                "src/entry-server.js",
                "import logo from '/vite.svg';\nexport const url = logo;\n",
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let server = std::fs::read_to_string(root.join("dist/entry-server.mjs")).unwrap();
    assert!(
        server.contains("\"/vite.svg\"") || server.contains("`/vite.svg`"),
        "server public URL not root-absolute:\n{server}"
    );
    assert!(
        !server.contains("import.meta.url"),
        "chunk-relative:\n{server}"
    );
}

/// vite:asset claims a public file only as an asset type or a `?url` import:
/// a public `/data.json` fails to resolve, its `?url` form is the URL.
#[tokio::test]
async fn public_non_asset_import_is_unresolved_like_vite() {
    let root = app("root-abs-imports-non-asset", "/app/");
    write_files(
        &root,
        &[
            ("public/data.json", "{\"a\":1}\n"),
            (
                "src/main.js",
                "import d from '/data.json';\nconsole.log(d);\n",
            ),
        ],
    );
    let err = build_app(&root)
        .await
        .expect_err("a public json import fails");
    assert!(
        format!("{err:#}").contains("/data.json"),
        "error does not name the import: {err:#}"
    );
    write_files(
        &root,
        &[(
            "src/main.js",
            "import d from '/data.json?url';\nconsole.log(d);\n",
        )],
    );
    build_app(&root).await.expect("the ?url form builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("/app/data.json"),
        "?url not the public URL:\n{js}"
    );
}
