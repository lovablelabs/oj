// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! With the on-disk module cache on (`--enable-cache`), every importer of a
//! module an HMR update touched names it the same way: Vite's importAnalysis
//! appends `?t=<lastHMRTimestamp>` to each import of a stamped module, so the
//! browser holds one instance of it. A cached importer compiled under other
//! stamps (an earlier session, before the update) must not be served as is:
//! its import would name the bare url while another importer names the
//! stamped one, two urls for one module, which gives a React context two
//! identities ("must be used within a provider") until the cache is cleared.
//! The restarts are further `build_app`s over the kept cache.

use std::path::Path;
use std::time::Duration;

use regex::Regex;

use crate::{App, Server};

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.ts"></script></body></html>"#;

/// `importer` imports `./ctx` and accepts its own updates, so an update to
/// ctx stops at it (as a component module does under React Refresh). The
/// string literal marks its cache entry.
fn importer(name: &str) -> String {
    format!(
        "import {{ value }} from \"./ctx\";\nexport const {name} = () => value + \"marker-{name}\";\n\
         if (import.meta.hot) import.meta.hot.accept();\n"
    )
}

async fn serve_cached(app: &App) -> Server {
    let mut dev = app.dev();
    dev.enable_cache = true;
    crate::serve(dev).await
}

/// The `?t=` stamp `code` imports `/src/ctx.ts` with, or None when the import
/// is bare; panics when the import is missing altogether.
fn ctx_stamp(code: &str) -> Option<String> {
    let import = Regex::new(r#""/src/ctx\.ts(\?t=(\d+))?""#).unwrap();
    let found = import
        .captures(code)
        .unwrap_or_else(|| panic!("no import of /src/ctx.ts in:\n{code}"));
    found.get(2).map(|m| m.as_str().to_string())
}

/// Polls `path` until its import of ctx carries a stamp (the server has
/// processed the edit), within `within`.
async fn stamped(server: &Server, path: &str, within: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let code = server.text(path).await;
        if let Some(stamp) = ctx_stamp(&code) {
            return Some(stamp);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a file named `name` exists anywhere under `dir`.
fn file_under(dir: &Path, name: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let path = e.path();
            if path.is_dir() {
                file_under(&path, name)
            } else {
                e.file_name() == name
            }
        })
}

/// Polls until `pred` holds, failing with `what` after `within`.
async fn wait_until(what: &str, within: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !pred() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_importer_names_an_updated_module_as_the_others_do() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"stamp-cache","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX),
        // A dependency, so the optimizer commits a pre-bundle and writes its
        // manifest: the next session's dep version (part of every compile
        // key) is then the manifest's, and the cold session's entries are
        // found. Without one the version is per session and nothing is.
        (
            "node_modules/served-dep/package.json",
            r#"{"name":"served-dep","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/served-dep/index.js", "export const dep = 1;\n"),
        (
            "src/main.ts",
            // b is a dynamic import: the eager crawl walks static imports
            // only, so b is not served (nor in the graph) until asked for.
            "import { dep } from \"served-dep\";\nimport { a } from \"./a\";\nexport const loadB = () => import(\"./b\");\nconsole.log(dep, a(), loadB);\n",
        ),
        ("src/a.ts", &importer("a")),
        ("src/b.ts", &importer("b")),
        ("src/ctx.ts", "export const value = \"v1\";\n"),
    ]);
    let cache = oj_cache::cache_root(&app.root);

    // Cold: every module compiles, with bare imports (no update yet), and
    // lands in the cache under the committed pre-bundle's version.
    {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        crate::crawled(&app.root).await;
        wait_until(
            "the optimizer never wrote its manifest",
            Duration::from_secs(20),
            || file_under(&cache, "manifest.json"),
        )
        .await;
        for path in ["/src/main.ts", "/src/a.ts", "/src/b.ts", "/src/ctx.ts"] {
            server.text(path).await;
        }
        assert_eq!(ctx_stamp(&server.text("/src/b.ts").await), None);
        wait_until(
            "the cold run never persisted b.ts",
            Duration::from_secs(10),
            || crate::warm_transform_virtual::cached_module_contains(&cache, "marker-b"),
        )
        .await;
    }

    // Warm: a.ts is loaded (from the cache); ctx is edited; b.ts, which this
    // session has not served yet (a dynamic import, nothing crawls it), is
    // then requested for the first time.
    let stamp = {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        server.text("/src/a.ts").await;
        app.write(&[("src/ctx.ts", "export const value = \"v2\";\n")]);
        let stamp = stamped(&server, "/src/a.ts", Duration::from_secs(20))
            .await
            .expect("a.ts, a loaded importer, re-stamps its import after the edit");
        let b = server.text("/src/b.ts").await;
        assert_eq!(
            ctx_stamp(&b).as_deref(),
            Some(stamp.as_str()),
            "b.ts, served from the cache after the update, names ctx as a.ts does:\n{b}"
        );
        // And stays so on a repeat request (the memory and mtime fast paths).
        assert_eq!(
            ctx_stamp(&server.text("/src/b.ts").await),
            Some(stamp.clone())
        );
        wait_until(
            "the warm run never persisted the stamped importers",
            Duration::from_secs(10),
            || {
                crate::warm_transform_virtual::cached_module_contains(
                    &cache,
                    &format!("ctx.ts?t={stamp}"),
                )
            },
        )
        .await;
        stamp
    };

    // A later session starts with no stamps (Vite's are per server too): the
    // stamped entries the last one wrote must not come back for one importer
    // while another is served bare.
    {
        let server = serve_cached(&app).await;
        let a = server.text("/src/a.ts").await;
        let b = server.text("/src/b.ts").await;
        assert_eq!(
            ctx_stamp(&a),
            ctx_stamp(&b),
            "after a restart both importers name ctx the same way:\n{a}\n{b}"
        );
        assert_ne!(
            ctx_stamp(&a).as_deref(),
            Some(stamp.as_str()),
            "a stamp from an earlier session is never served again:\n{a}"
        );
    }
}

/// A plugin whose `transform` records each run over `/src/b.ts`: a
/// persistent-cache hit skips the transform (Vite's cached transform result
/// is served as is), a compile runs it.
const COUNTING_PLUGIN: &str = r#"import fs from "node:fs";
export default {
  plugins: [{
    name: "count-b-transforms",
    transform(code, id) {
      if (id.split("?")[0].endsWith("/src/b.ts")) {
        fs.appendFileSync(new URL("./transforms.log", import.meta.url), id + "\n");
      }
      return null;
    },
  }],
};
"#;

/// How many times the plugin transformed `/src/b.ts` so far.
fn b_transforms(root: &Path) -> usize {
    std::fs::read_to_string(root.join("transforms.log"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The rule from both sides, as Vite's per-server transform cache behaves:
/// a cached compile is served (no transform runs) while the stamps of its
/// imports still hold, both in a later session with no update and on a
/// repeat request; it is recompiled only once an import of its was updated;
/// and the bare compile stays valid for a session that starts with no stamps.
#[tokio::test(flavor = "multi_thread")]
async fn a_cached_compile_is_served_under_its_stamps_and_recompiled_otherwise() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"stamp-cache-hits","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", COUNTING_PLUGIN),
        ("index.html", INDEX),
        (
            "node_modules/served-dep/package.json",
            r#"{"name":"served-dep","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/served-dep/index.js", "export const dep = 1;\n"),
        (
            "src/main.ts",
            "import { dep } from \"served-dep\";\nimport { a } from \"./a\";\nexport const loadB = () => import(\"./b\");\nconsole.log(dep, a(), loadB);\n",
        ),
        ("src/a.ts", &importer("a")),
        ("src/b.ts", &importer("b")),
        ("src/ctx.ts", "export const value = \"v1\";\n"),
    ]);
    let root = app.root.clone();
    let cache = oj_cache::cache_root(&root);

    // Cold: b compiles once (the transform runs) and lands in the cache.
    {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        crate::crawled(&root).await;
        wait_until(
            "the optimizer never wrote its manifest",
            Duration::from_secs(20),
            || file_under(&cache, "manifest.json"),
        )
        .await;
        assert_eq!(ctx_stamp(&server.text("/src/b.ts").await), None);
        assert_eq!(b_transforms(&root), 1, "the cold compile ran the transform");
        wait_until(
            "the cold run never persisted b.ts",
            Duration::from_secs(10),
            || crate::warm_transform_virtual::cached_module_contains(&cache, "marker-b"),
        )
        .await;
    }

    // Warm, nothing updated: the cached compile is served, no transform.
    {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        assert_eq!(ctx_stamp(&server.text("/src/b.ts").await), None);
        assert_eq!(
            b_transforms(&root),
            1,
            "a cached compile whose import stamps hold is served without a transform"
        );
    }

    // Warm, ctx updated before b is first requested: the cached compile
    // (bare import) is rejected, b compiles (one transform) with the stamp
    // a.ts carries, and the repeat request is a memory hit.
    {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        server.text("/src/a.ts").await;
        app.write(&[("src/ctx.ts", "export const value = \"v2\";\n")]);
        let stamp = stamped(&server, "/src/a.ts", Duration::from_secs(20))
            .await
            .expect("a.ts re-stamps its import after the edit");
        assert_eq!(
            ctx_stamp(&server.text("/src/b.ts").await).as_deref(),
            Some(stamp.as_str())
        );
        assert_eq!(
            b_transforms(&root),
            2,
            "the cached compile under other stamps was recompiled"
        );
        assert_eq!(
            ctx_stamp(&server.text("/src/b.ts").await).as_deref(),
            Some(stamp.as_str())
        );
        assert_eq!(b_transforms(&root), 2, "the repeat request is a cache hit");
        wait_until(
            "the warm run never persisted the stamped compile",
            Duration::from_secs(10),
            || {
                crate::warm_transform_virtual::cached_module_contains(
                    &cache,
                    &format!("ctx.ts?t={stamp}"),
                )
            },
        )
        .await;
    }

    // A later session starts with no stamps: the bare compile from the cold
    // run is the one that holds, served without a transform; the stamped
    // one is never found under this session's key.
    {
        let server = serve_cached(&app).await;
        server.text("/src/main.ts").await;
        let a = server.text("/src/a.ts").await;
        let b = server.text("/src/b.ts").await;
        assert_eq!(ctx_stamp(&a), None, "{a}");
        assert_eq!(ctx_stamp(&b), None, "{b}");
        assert_eq!(
            b_transforms(&root),
            2,
            "the bare compile is served from the cache in a session with no stamps"
        );
    }
}
