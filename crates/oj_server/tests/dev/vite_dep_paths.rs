// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's URL for a pre-bundled dependency (`/node_modules/.vite/deps/react.js`,
//! `react-dom_client.js` for `react-dom/client`: the optimizer's `flattenId`)
//! is served under oj too, as a re-export of the pre-bundle the app's own
//! rewritten imports load, so a script or a retained test that was written
//! against Vite's cache paths shares the page's one copy of the dep instead
//! of 404ing (or, pointed at an `/@oj-deps/` URL by hand, loading a second
//! copy). A flat id of an installed dependency nothing has imported yet
//! registers it with the optimizer, as a bare import would (Vite's
//! registerMissingImport); one that is not a dependency is a 404, and one
//! whose re-optimization fails is Vite's processing error, a 504. The URL
//! follows the config's `cacheDir` as Vite resolves it: a configured
//! directory, else `node_modules/.vite` under the nearest package.json at or
//! above the root.

use std::time::Duration;

use regex::Regex;

use crate::browser::Page;
use crate::{require_fixture, App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(30);

fn link_react(app: &App) {
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
}

/// The pre-bundle URL a served module's import of the dep whose oj file stem
/// is `stem` (with or without the named-export facade) was rewritten to.
fn dep_url_in(module: &str, stem: &str) -> String {
    let re = Regex::new(&format!(
        r#"from "(/@oj-deps/{stem}(?:__oj_named)?\.mjs[^"]*)""#
    ))
    .unwrap();
    re.captures(module)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| panic!("no /@oj-deps/{stem} import in:\n{module}"))
}

/// Every `/@oj-deps/` URL a served module imports.
fn dep_urls(module: &str) -> Vec<String> {
    Regex::new(r#"from "(/@oj-deps/[^"]*)""#)
        .unwrap()
        .captures_iter(module)
        .map(|c| c[1].to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_vite_dep_url_is_the_apps_own_pre_bundle() {
    require_fixture!("react", "react-dom", "scheduler");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-dep-paths","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX_HTML),
        // The app imports its deps bare; a module written against Vite's
        // cache paths (a retained test, say) imports the same deps that way.
        (
            "src/main.js",
            "import { createRoot } from \"react-dom/client\";\n\
             import { useState as bareUseState } from \"react\";\n\
             import { jsx as bareJsx } from \"react/jsx-runtime\";\n\
             import { useState } from \"/node_modules/.vite/deps/react.js\";\n\
             import { jsx } from \"/node_modules/.vite/deps/react_jsx-runtime.js?v=0123abcd\";\n\
             window.__t = [createRoot, bareUseState === useState, bareJsx === jsx];\n",
        ),
    ]);
    link_react(&app);
    let server = app.serve().await;

    // Each dep request waits for the pre-bundle's commit (optimize_plan.rs);
    // the module is read once the URLs are final.
    for url in dep_urls(&server.text("/src/main.js").await) {
        server.text(&url).await;
    }
    let main = server.text("/src/main.js").await;

    // A Vite path import gets the URL the bare import of the same dep got,
    // so the browser has one module record for react however the import
    // was spelled; Vite's own `?v=` hash on the path gives way to it.
    assert!(
        !main.contains("/node_modules/.vite/deps/"),
        "the Vite dep paths were rewritten:\n{main}"
    );
    let react_url = dep_url_in(&main, "react");
    assert_eq!(
        main.matches(&format!("from \"{react_url}\"")).count(),
        2,
        "the bare and the Vite path import of react share {react_url}:\n{main}"
    );
    let runtime_url = dep_url_in(&main, "react_jsx-runtime");
    assert_eq!(
        main.matches(&format!("from \"{runtime_url}\"")).count(),
        2,
        "the bare and the Vite path import of react/jsx-runtime share {runtime_url}:\n{main}"
    );
    assert!(
        runtime_url.contains("?v=") && !runtime_url.contains("0123abcd"),
        "Vite's own ?v= hash gives way to oj's version: {runtime_url}"
    );

    // Requested directly (a dynamic import from a devtools script, say), the
    // Vite URL re-exports the app's pre-bundle instead of carrying its own
    // copy of the code: one instance per page.
    let res = server.get("/node_modules/.vite/deps/react.js").await;
    assert_eq!(
        res.status, 200,
        "GET /node_modules/.vite/deps/react.js:\n{}",
        res.body
    );
    assert_eq!(
        res.headers.get("content-type").unwrap(),
        "text/javascript",
        "served as a module"
    );
    assert_eq!(
        res.headers.get("cache-control").unwrap(),
        "no-cache",
        "the re-export moves with the pre-bundle's version, so it is not immutable"
    );
    assert!(
        res.body.contains(&format!("export * from \"{react_url}\""))
            && res
                .body
                .contains(&format!("export {{ default }} from \"{react_url}\"")),
        "re-exports {react_url}:\n{}",
        res.body
    );
    // Vite's hash on the URL (copied from a Vite session) is not oj's and is
    // ignored; Vite's optimizedDeps plugin would 504 the stale hash, but no
    // hash of Vite's could ever be current here.
    let versioned = server
        .get("/node_modules/.vite/deps/react.js?v=0123abcd")
        .await;
    assert_eq!(versioned.status, 200);
    assert_eq!(versioned.body, res.body);

    // A CJS dep's named exports: `react-dom/client` under Vite's flat name.
    let client_url = dep_url_in(&main, "react-dom_client");
    let client = server
        .text("/node_modules/.vite/deps/react-dom_client.js")
        .await;
    assert!(
        client.contains(&format!("export * from \"{client_url}\"")),
        "react-dom_client.js re-exports the app's react-dom/client pre-bundle {client_url}:\n{client}"
    );
    let target = server.text(&client_url).await;
    assert!(
        target.contains("createRoot") && target.contains("hydrateRoot"),
        "the re-exported module names createRoot and hydrateRoot:\n{}",
        &target[..target.len().min(600)]
    );

    // Not a dependency: Vite's optimizedDeps load finds no such file (404).
    let missing = server.get("/node_modules/.vite/deps/not-a-dep.js").await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    let escaped = server.get("/node_modules/.vite/deps/../../x.js").await;
    assert_ne!(escaped.status, 200, "{}", escaped.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dep_nothing_imported_yet_registers_through_its_vite_url() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-dep-late","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__main = true;\n"),
        (
            "node_modules/late-cjs/package.json",
            r#"{"name":"late-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/late-cjs/index.js",
            "module.exports = { hi: function () { return \"late-cjs-hi\"; } };\n",
        ),
        (
            "node_modules/lodash_like/package.json",
            r#"{"name":"lodash_like","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/lodash_like/index.js",
            "export const under = () => \"underscore-name\";\n",
        ),
    ]);
    let server = app.serve().await;
    crate::crawled(&app.root).await;

    // Vite's registerMissingImport: the request waits for the re-optimization
    // and then re-exports the new entry, which is served too.
    let res = server.get("/node_modules/.vite/deps/late-cjs.js").await;
    assert_eq!(
        res.status, 200,
        "GET /node_modules/.vite/deps/late-cjs.js:\n{}",
        res.body
    );
    let url = dep_url_in(&res.body, "late-cjs");
    assert!(url.contains("?v="), "a registered dep has a version: {url}");
    let bundle = server.text(&url).await;
    assert!(
        bundle.contains("late-cjs-hi"),
        "the pre-bundle was written:\n{bundle}"
    );
    assert!(
        res.body.contains("export { default }"),
        "a CJS pre-bundle has a default export to re-export:\n{}",
        res.body
    );

    // flattenId writes a literal underscore as three (`_` plus `__`), and an
    // ESM dep's re-export carries no default the dep does not have.
    let res = server
        .get("/node_modules/.vite/deps/lodash___like.js")
        .await;
    assert_eq!(res.status, 200, "GET lodash___like.js:\n{}", res.body);
    let url = dep_url_in(&res.body, "lodash_like");
    assert!(server.text(&url).await.contains("underscore-name"));
    assert!(
        !res.body.contains("default"),
        "no default re-export for an ESM dep without one:\n{}",
        res.body
    );

    // A dep whose bundle fails: the rerun commits without it (oj parks the
    // batch and its importers recompile onto the per-file path), and the
    // request is Vite's processing error, a 504, never a re-export of a URL
    // nothing serves. The dep registers no more after that, so the next
    // request is the 404 of a file Vite's cache dir lacks.
    app.write(&[
        (
            "node_modules/broken-dep/package.json",
            r#"{"name":"broken-dep","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/broken-dep/index.js", "export const x = ;\n"),
    ]);
    let res = server.get("/node_modules/.vite/deps/broken-dep.js").await;
    assert_eq!(
        res.status, 504,
        "a dep whose re-optimization failed:\n{}",
        res.body
    );
    let again = server.get("/node_modules/.vite/deps/broken-dep.js").await;
    assert_eq!(again.status, 404, "{}", again.body);
}

const TINY: [(&str, &str); 2] = [
    (
        "node_modules/tiny/package.json",
        r#"{"name":"tiny","version":"1.0.0","type":"module","main":"index.js"}"#,
    ),
    (
        "node_modules/tiny/index.js",
        "export const hi = () => \"tiny-hi\";\n",
    ),
];

/// The re-export a Vite dep URL answers for `tiny`, checked against the URL
/// the app's own bare import was rewritten to.
async fn re_exports_tiny(server: &crate::Server, vite_url: &str) {
    let main = server.text("/src/main.js").await;
    let url = dep_url_in(&main, "tiny");
    let res = server.get(vite_url).await;
    assert_eq!(res.status, 200, "GET {vite_url}:\n{}", res.body);
    assert!(
        res.body.contains(&format!("export * from \"{url}\"")),
        "{vite_url} re-exports the app's pre-bundle {url}:\n{}",
        res.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_vite_url_follows_the_configs_cache_dir() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-dep-cache-dir","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX_HTML),
        (
            "src/main.js",
            "import { hi } from \"tiny\";\nwindow.__hi = hi();\n",
        ),
        (
            "vite.config.mjs",
            "export default { cacheDir: \".cache/vite\" };\n",
        ),
    ]);
    app.write(&TINY);
    app.link_fixture("vite");
    let mut dev = app.dev();
    dev.config = Some(app.root.join("vite.config.mjs"));
    let server = crate::serve(dev).await;

    // Vite's cache dir is the config's, so its dep URLs sit under it...
    re_exports_tiny(&server, "/.cache/vite/deps/tiny.js").await;
    // ...and the default prefix is an ordinary path with no such file.
    let default = server.get("/node_modules/.vite/deps/tiny.js").await;
    assert_ne!(default.status, 200, "{}", default.body);
    // A module's import of the configured path is the dep's own import.
    app.write(&[(
        "src/via.js",
        "import { hi } from \"/.cache/vite/deps/tiny.js\";\nexport const v = hi();\n",
    )]);
    let via = server.text("/src/via.js").await;
    let url = dep_url_in(&server.text("/src/main.js").await, "tiny");
    assert!(
        via.contains(&format!("from \"{url}\"")) && !via.contains("/.cache/"),
        "the configured path is rewritten to {url}:\n{via}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_root_inside_a_package_has_its_cache_dir_above_it() {
    // Vite places the cache under the nearest package.json at or above the
    // root (findNearestPackageData), so a root with no manifest of its own
    // inside a package caches in that package's node_modules, outside the
    // root, and the dep URLs take Vite's `/@fs/` form of the absolute path.
    let pkg = App::new(&[
        (
            "package.json",
            r#"{"name":"workspace","version":"1.0.0","private":true}"#,
        ),
        ("app/index.html", INDEX_HTML),
        (
            "app/src/main.js",
            "import { hi } from \"tiny\";\nwindow.__hi = hi();\n",
        ),
    ]);
    pkg.write(&TINY);
    let mut dev = pkg.dev();
    dev.root = pkg.root.join("app");
    let server = crate::serve(dev).await;
    let root = std::fs::canonicalize(&pkg.root).unwrap();

    re_exports_tiny(
        &server,
        &format!("/@fs{}/node_modules/.vite/deps/tiny.js", root.display()),
    )
    .await;
    let under_root = server.get("/node_modules/.vite/deps/tiny.js").await;
    assert_ne!(under_root.status, 200, "{}", under_root.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vite_path_import_and_an_oj_path_import_share_one_react() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"vite-dep-page","version":"1.0.0"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.jsx"></script></body></html>"#,
        ),
        // The root comes from the dep as oj names it (a bare import)...
        (
            "src/main.jsx",
            "import { createRoot } from \"react-dom/client\";\nimport { Counter } from \"./Counter.js\";\n\
             createRoot(document.getElementById(\"root\")).render(<Counter />);\n",
        ),
        // ...and the component's hook from the dep as Vite's cache names it. A
        // second React instance would fail the hook ("Invalid hook call").
        (
            "src/Counter.js",
            "import { useState } from \"/node_modules/.vite/deps/react.js\";\n\
             import { jsx } from \"/node_modules/.vite/deps/react_jsx-runtime.js\";\n\
             window.__useState = useState;\n\
             export function Counter() {\n  const [n] = useState(\"one-react\");\n  return jsx(\"p\", { id: \"out\", children: n });\n}\n",
        ),
    ]);
    link_react(&app);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r##"document.querySelector("#out")?.textContent === "one-react""##,
        WAIT,
    )
    .await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());

    // A hand-written import of the Vite URL, outside oj's rewrite (a devtools
    // or test-runner script), resolves to the same instance the page runs on.
    let same: bool = page
        .eval(r#"import("/node_modules/.vite/deps/react.js").then((m) => m.useState === window.__useState)"#)
        .await;
    assert!(same, "the Vite URL's useState is the page's useState");
    // And the CJS dep's names are there, as the app's own import sees them.
    let shape: Vec<String> = page
        .eval(
            r#"import("/node_modules/.vite/deps/react-dom_client.js").then((m) => [typeof m.createRoot, typeof m.hydrateRoot, typeof m.default])"#,
        )
        .await;
    assert_eq!(
        shape,
        ["function", "function", "object"],
        "react-dom/client's export shape"
    );
    page.close().await;
}
