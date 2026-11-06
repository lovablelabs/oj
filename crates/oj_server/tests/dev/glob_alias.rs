// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An `import.meta.glob` whose pattern starts with a `resolve.alias` find
//! (`@/assets/*.png`) matches the aliased directory, as Vite's toAbsoluteGlob
//! hands such a pattern to resolveId: lazy and eager calls expand to the
//! files with root-absolute keys, and a file created there later joins the
//! glob.

use std::time::{Duration, Instant};

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn aliased_globs_expand_through_resolve_alias() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"glob-alias","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.js",
            "import path from \"node:path\";\n\
             const dir = path.dirname(new URL(import.meta.url).pathname);\n\
             export default { resolve: { alias: { \"@\": path.resolve(dir, \"src\") } } };\n",
        ),
        ("index.html", INDEX),
        (
            "main.js",
            "export const lazy = import.meta.glob(\"@/assets/*.png\");\n\
             export const eager = import.meta.glob(\"@/assets/*.png\", { eager: true, import: \"default\" });\n",
        ),
        ("src/assets/hero.png", "png"),
        ("src/assets/notes.txt", "txt"),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;

    let js = server.text("/main.js").await;
    assert!(
        js.contains(r#""/src/assets/hero.png": () => import("/src/assets/hero.png?url")"#),
        "lazy glob:\n{js}"
    );
    assert!(
        js.contains(r#"import { default as __oj_glob_0 } from "/src/assets/hero.png?url";"#),
        "eager glob:\n{js}"
    );
    assert!(
        js.contains(r#""/src/assets/hero.png": __oj_glob_0"#),
        "{js}"
    );
    assert!(!js.contains("notes.txt"), "{js}");

    // A generated image landing in the aliased directory joins the glob.
    app.write(&[("src/assets/gen.png", "png")]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let js = server.text("/main.js").await;
        if js.contains(r#""/src/assets/gen.png": () => import("/src/assets/gen.png?url")"#) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "created file never joined:\n{js}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
