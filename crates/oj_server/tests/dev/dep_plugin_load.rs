// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Plugins and node_modules: an object-form `load` whose `filter.id` names a
//! dependency is offered that dependency (oj gates deps on the plugin's own
//! filter so unfiltered plugins cost no RPC); a linked workspace package
//! (realpath outside node_modules) is source, so a plugin transform applies to
//! it; and a transform emitting an import of an absolute path outside the app
//! root is served through /@fs instead of left as a 404 url.

use crate::{App, INDEX_HTML};

// The plugin replaces the quoted `"virtual:outside"` specifier with the quoted
// absolute path, so both sides are JS string literals of JSON strings.
const PLUGINS: &str = r#"export default [{
     name: "dep-aware",
     load: {
       filter: { id: /node_modules\/fake-dep\// },
       handler(id) { return id.includes("fake-dep") ? "export const WHO = \"FROM_PLUGIN\";\n" : null; },
     },
     transform(code, id) {
       if (code.includes("__UI_MARKER__")) return code.replace("__UI_MARKER__", "TRANSFORMED_LINKED");
       if (code.includes("virtual:outside")) return code.replace(__FROM__, __TO__);
       return null;
     },
   }];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn dep_load_filter_linked_package_transform_and_outside_root_fs_import() {
    // The app sits one level down so the linked package and the outside file
    // are siblings of it, outside both the root and node_modules.
    let tmp = App::new(&[]);
    let outside = tmp.root.join("shared/util.js");
    let quoted = |s: &str| serde_json::to_string(&serde_json::to_string(s).unwrap()).unwrap();
    let plugins = PLUGINS
        .replace("__FROM__", &quoted("virtual:outside"))
        .replace("__TO__", &quoted(&outside.display().to_string()));
    tmp.write(&[
        (
            "app/package.json",
            r#"{"name":"dep-load-app","version":"1.0.0","dependencies":{"fake-dep":"1.0.0","@ws/ui":"1.0.0"}}"#,
        ),
        (
            "app/node_modules/fake-dep/package.json",
            r#"{"name":"fake-dep","version":"1.0.0","main":"index.js","type":"module"}"#,
        ),
        (
            "app/node_modules/fake-dep/index.js",
            "export const WHO = \"FROM_DISK\";\n",
        ),
        (
            "packages/ui/package.json",
            r#"{"name":"@ws/ui","version":"1.0.0","main":"index.js","type":"module"}"#,
        ),
        ("packages/ui/index.js", "export const UI = \"__UI_MARKER__\";\n"),
        (
            "shared/package.json",
            r#"{"name":"shared","version":"1.0.0","type":"module"}"#,
        ),
        ("shared/util.js", "export const OUTSIDE = \"outside-root\";\n"),
        (
            "app/src/main.js",
            "import { WHO } from \"fake-dep\";\nimport { UI } from \"@ws/ui\";\nimport { OUTSIDE } from \"virtual:outside\";\nwindow.__R = [WHO, UI, OUTSIDE];\n",
        ),
        ("app/index.html", INDEX_HTML),
        // Like Vite, plugin hooks apply to deps that are not pre-bundled.
        (
            "app/oj.config.json",
            r#"{"optimizeDeps":{"exclude":["fake-dep"]}}"#,
        ),
        ("app/oj.plugins.mjs", &plugins),
    ]);
    std::fs::create_dir_all(tmp.root.join("app/node_modules/@ws")).unwrap();
    std::os::unix::fs::symlink(
        tmp.root.join("packages/ui"),
        tmp.root.join("app/node_modules/@ws/ui"),
    )
    .unwrap();

    let mut dev = tmp.dev();
    dev.root = tmp.root.join("app");
    let server = crate::serve(dev).await;

    let main = server.text("/src/main.js").await;
    let urls: Vec<String> = regex::Regex::new(r#"from\s+"([^"]+)""#)
        .unwrap()
        .captures_iter(&main)
        .map(|c| c[1].to_string())
        .collect();
    assert_eq!(urls.len(), 3, "three rewritten imports:\n{main}");
    let (dep_url, linked_url, outside_url) = (&urls[0], &urls[1], &urls[2]);

    let dep = server.get(dep_url).await.body;
    assert!(
        dep.contains("FROM_PLUGIN"),
        "filtered plugin load did not run for the dep ({dep_url}):\n{dep}"
    );

    let ui = server.get(linked_url).await.body;
    assert!(
        ui.contains("TRANSFORMED_LINKED"),
        "plugin transform skipped the linked package ({linked_url}):\n{ui}"
    );

    assert!(
        outside_url.starts_with("/@fs/"),
        "outside-root path not served via /@fs: {outside_url}"
    );
    let res = server.get(outside_url).await;
    assert_eq!(
        res.status, 200,
        "outside-root module not served:\n{}",
        res.body
    );
    assert!(res.body.contains("outside-root"), "{}", res.body);
}
