// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Regression: a plugin `transform` can append an import of a virtual it
//! serves from in-memory state filled during that transform (wyw-in-js keeps
//! each module's extracted CSS in a `cssLookup` its `load` serves). The
//! persistent cache keeps the transformed code across restarts; serving it on
//! a warm start without re-running the transform leaves the plugin's map
//! empty and the virtual import 404s. oj re-transforms a cached module that
//! imports such a virtual. The restart is a second `build_app` in the same
//! process over the kept cache, with a fresh plugin host (fresh map).

use std::path::Path;
use std::time::Duration;

use crate::{App, Server};

const VITE_CONFIG: &str = r#"const cssStore = {};
export default {
  plugins: [
    {
      name: "stateful-virtual-css",
      enforce: "pre",
      resolveId(id) {
        const clean = id.split("?", 1)[0];
        return clean in cssStore ? clean : null;
      },
      load(id) {
        const clean = id.split("?", 1)[0];
        return clean in cssStore ? cssStore[clean] : null;
      },
      transform(code, id) {
        const clean = id.split("?", 1)[0];
        if (!clean.endsWith("/main.tsx")) return null;
        const virt = clean.replace(/\.tsx$/, ".stateful.css");
        cssStore[virt] = ".injected { color: rebeccapurple }";
        return { code: code + "\nimport " + JSON.stringify(virt) + ";\n", map: null };
      },
    },
  ],
};
"#;

fn virt_url_from(main: &str) -> String {
    regex::Regex::new(r#"import\s+"([^"]*\.stateful\.css)""#)
        .unwrap()
        .captures(main)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| panic!("main.tsx must import the injected virtual css:\n{main}"))
}

/// Harness addition: whether a persisted module entry
/// (`<cache>/<key[..2]>/<key>.json`) holds `needle`. Other cache files (the
/// graph snapshot) name the virtual's url too, so only entries count. Cache
/// writes go through a background thread, so the warm run waits for the cold
/// run's entry instead of racing it, and fails rather than passing vacuously
/// when the cache is off.
fn cached_module_contains(cache: &Path, needle: &str) -> bool {
    let is_hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let Ok(shards) = std::fs::read_dir(cache) else {
        return false;
    };
    shards
        .flatten()
        .filter(|d| d.file_name().to_str().is_some_and(|n| is_hex(n, 2)))
        .filter_map(|d| std::fs::read_dir(d.path()).ok())
        .flat_map(|entries| entries.flatten())
        .any(|e| {
            let path = e.path();
            path.extension().is_some_and(|x| x == "json")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| is_hex(s, 64))
                && std::fs::read_to_string(&path).is_ok_and(|s| s.contains(needle))
        })
}

async fn serve_cached(app: &App) -> Server {
    let mut dev = app.dev();
    dev.enable_cache = true;
    crate::serve(dev).await
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_virtual_still_serves_after_a_warm_start() {
    let app = App::new(&[
        ("package.json", r#"{"name":"warmvirt","version":"1.0.0"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/main.tsx"></script></body></html>"#,
        ),
        ("main.tsx", "export const app = \"hi\";\n"),
        ("vite.config.js", VITE_CONFIG),
    ]);
    let cache = oj_cache::cache_root(&app.root);

    // Cold: the transform runs, the virtual is populated and served.
    {
        let server = serve_cached(&app).await;
        let main = server.text("/main.tsx").await;
        let virt = virt_url_from(&main);
        let css = server.get(&virt).await;
        assert_eq!(css.status, 200, "cold: virtual css serves:\n{}", css.body);
        let ctype = css
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            ctype.contains("javascript"),
            "cold: virtual css wrapped as a JS module: {ctype}"
        );
        assert!(
            css.body.contains("rebeccapurple"),
            "cold: extracted CSS present:\n{}",
            css.body
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cached_module_contains(&cache, ".stateful.css") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the cold run never persisted main.tsx under {}",
                cache.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Some(host) = &server.built.plugin_host {
            host.shutdown();
        }
    }

    // Warm: cache kept, fresh plugin host (empty map). The re-transform must
    // repopulate the virtual so it serves instead of 404ing.
    assert!(cache.exists(), "cache persisted for the warm run");
    let server = serve_cached(&app).await;
    let main = server.text("/main.tsx").await;
    let virt = virt_url_from(&main);
    let css = server.get(&virt).await;
    assert_eq!(
        css.status, 200,
        "warm: virtual css must still serve (was 404 before the fix):\n{}",
        css.body
    );
    assert!(
        css.body.contains("rebeccapurple"),
        "warm: extracted CSS present:\n{}",
        css.body
    );
}
