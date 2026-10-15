// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The dep optimizer skips linked / workspace packages (symlinked into
//! node_modules from outside it), as Vite does: pre-bundling them would freeze
//! their source so edits stop HMR-ing. A normal node_modules dep IS
//! pre-bundled (in the manifest, served via /@oj-deps); the linked one is not
//! (served on demand).

use std::time::{Duration, Instant};

use regex::Regex;

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn linked_workspace_dep_is_not_pre_bundled() {
    crate::require_fixture!("vite");
    let ws = App::new(&[
        (
            "app/node_modules/norm-lib/package.json",
            r#"{"name":"norm-lib","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "app/node_modules/norm-lib/index.js",
            "module.exports = { hi: function () { return \"norm\"; } };\n",
        ),
        // Workspace source, OUTSIDE node_modules.
        (
            "packages/link-lib/package.json",
            r#"{"name":"link-lib","version":"1.0.0","module":"index.js"}"#,
        ),
        ("packages/link-lib/index.js", "export const hi = () => \"link\";\n"),
        ("app/package.json", r#"{"name":"linkedapp","private":true}"#),
        (
            "app/index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#,
        ),
        (
            "app/main.js",
            "import { hi as h1 } from \"norm-lib\";\nimport { hi as h2 } from \"link-lib\";\nwindow.__R = h1() + h2();\n",
        ),
    ]);
    let app = ws.root.join("app");
    let nm = app.join("node_modules");
    // link_fixture targets the App root; this app sits one level down.
    std::os::unix::fs::symlink(crate::fixture("vite").unwrap(), nm.join("vite")).unwrap();
    std::os::unix::fs::symlink(ws.root.join("packages/link-lib"), nm.join("link-lib")).unwrap();

    let mut dev = ws.dev();
    dev.root = app.clone();
    let server = crate::serve(dev).await;

    // Wait until main.js imports norm-lib from the pre-bundle, then fetch that
    // bundle so it has committed.
    let imports = Regex::new(r#"(?:from|import)\s*"([^"]+)""#).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (norm_url, link_url) = loop {
        let main = server.text("/main.js").await;
        let find = |name: &str| {
            imports
                .captures_iter(&main)
                .map(|c| c[1].to_string())
                .find(|u| u.contains(name))
        };
        if let (Some(norm), Some(link)) = (find("norm-lib"), find("link-lib")) {
            if norm.starts_with("/@oj-deps/") {
                break (norm, link);
            }
        }
        assert!(
            Instant::now() < deadline,
            "norm-lib never served from /@oj-deps:\n{main}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    server.text(&norm_url).await;
    // Checked on the import itself, not just the manifest.
    assert!(
        !link_url.starts_with("/@oj-deps/"),
        "linked/workspace dep imported from the pre-bundle: {link_url}"
    );

    let manifest_path = oj_cache::cache_root(&app).join("deps/manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "no deps manifest emitted at {}: {e}",
            manifest_path.display()
        )
    });
    let manifest: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let meta = manifest.get("metadata").cloned().unwrap_or_default();
    assert!(
        meta.get("norm-lib").is_some(),
        "normal dep should be pre-bundled; manifest: {meta}"
    );
    assert!(
        meta.get("link-lib").is_none(),
        "linked/workspace dep must NOT be pre-bundled; manifest: {meta}"
    );
}
