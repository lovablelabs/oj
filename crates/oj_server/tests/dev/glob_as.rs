// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The deprecated `as` option of `import.meta.glob`: `as: "raw"` and
//! `as: "url"` give the default export of each file (its text, its URL), not
//! the module namespace, as Vite's parseGlobOptions sets `import: "default"`
//! for them (`forceDefaultAs`). An explicit `import: "*"` keeps the namespace.

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn as_raw_and_as_url_globs_give_the_default_export() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"glob-as","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX),
        (
            "main.js",
            "export const eagerRaw = import.meta.glob(\"./notes/*.md\", { as: \"raw\", eager: true });\n\
             export const lazyRaw = import.meta.glob(\"./notes/*.md\", { as: \"raw\" });\n\
             export const eagerUrl = import.meta.glob(\"./notes/*.md\", { as: \"url\", eager: true });\n\
             export const lazyUrl = import.meta.glob(\"./notes/*.md\", { as: \"url\" });\n\
             export const star = import.meta.glob(\"./notes/*.md\", { as: \"raw\", eager: true, import: \"*\" });\n",
        ),
        ("notes/a.md", "# a\n"),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;

    let js = server.text("/main.js").await;
    for expected in [
        r#"import { default as __oj_glob_0 } from "/notes/a.md?raw";"#,
        r#""./notes/a.md": () => import("/notes/a.md?raw").then((m) => m["default"])"#,
        r#"import { default as __oj_glob_1 } from "/notes/a.md?url";"#,
        r#""./notes/a.md": () => import("/notes/a.md?url").then((m) => m["default"])"#,
        r#"import * as __oj_glob_2 from "/notes/a.md?raw";"#,
    ] {
        assert!(js.contains(expected), "missing {expected}\n{js}");
    }

    // The module the eager import names has the text as its default export.
    let raw = server.text("/notes/a.md?raw").await;
    assert!(raw.contains("export default \"# a\\n\""), "{raw}");
}
