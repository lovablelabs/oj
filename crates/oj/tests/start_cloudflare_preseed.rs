// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The boot-time deps optimization of a runner-backed config runs in one-shot
//! children, never in the dev server: rolldown keeps the native memory of
//! every build until its process exits. Here on the Start fixture under
//! @cloudflare/vite-plugin (see `start_app::cloudflare_app`):
//!
//! - the deps a plugin hands the optimizer while the server sets up (the
//!   Cloudflare plugin's nodejs_compat polyfills, registered from its
//!   configureServer) are optimized by a child;
//! - the dev server hashes the config the child hashed (Vite's configHash),
//!   so it loads the child's cache instead of optimizing again in process;
//! - the plugin host starts while the child runs, and builds its
//!   environments only once the child is done;
//! - the next session's replay names only the deps Vite's include resolver
//!   reaches from the app's root, and a dep only a workspace package's own
//!   node_modules holds is committed by the child through the file it
//!   resolved to, never again by the dev server.

#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use start_app::{cloudflare_app, Server};

/// The workspace package's dependency, installed under the package alone.
const INNER_DEP: &str = "oj-e2e-inner-dep";

/// The fixture as a Cloudflare app: the plugin rendering `/` from workerd,
/// a deps cache of its own (cold, and no other run's), and an import of a
/// workspace package whose dependency only the package's own node_modules
/// holds (a strict install's shape): the session optimizes it through the
/// package's import, but its bare id does not resolve from the app's root.
fn prepare(root: &Path) {
    std::fs::write(
        root.join("wrangler.jsonc"),
        r#"{"name":"oj-start-fixture","main":"@tanstack/react-start/server-entry","compatibility_date":"2025-09-01","compatibility_flags":["nodejs_compat"]}"#,
    )
    .unwrap();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let cache_dir = root.join(".vite-deps");
    let config = config
        .replacen(
            "import { defineConfig } from \"vite\";",
            "import { defineConfig } from \"vite\";\nimport { cloudflare } from \"@cloudflare/vite-plugin\";",
            1,
        )
        .replacen(
            "    tanstackStart(",
            "    cloudflare({ viteEnvironment: { name: \"ssr\" } }),\n    tanstackStart(",
            1,
        )
        // The child's ssr optimizer build, slowed (OJ_PRESEED_REPORT is the
        // child's alone, so the dev server's own optimizer runs at speed):
        // the host reaches its environments while the seed still runs.
        .replacen(
            "    ssr: { define:",
            "    ssr: { optimizeDeps: { rolldownOptions: { plugins: [{ name: \"slow-seed\", buildStart: () => process.env.OJ_PRESEED_REPORT && new Promise((r) => setTimeout(r, 2000)) }] } }, define:",
            1,
        )
        .replacen(
            "export default defineConfig({",
            &format!(
                "export default defineConfig({{\n  cacheDir: {},",
                serde_json::json!(cache_dir.to_string_lossy())
            ),
            1,
        );
    assert!(
        config.contains("cloudflare(")
            && config.contains("cacheDir:")
            && config.contains("slow-seed"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();

    let inner = root.join("packages/inner");
    let dep = inner.join("node_modules").join(INNER_DEP);
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        inner.join("package.json"),
        r#"{"name":"oj-e2e-inner","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        inner.join("index.js"),
        format!("export {{ innerValue }} from \"{INNER_DEP}\";\n"),
    )
    .unwrap();
    std::fs::write(
        dep.join("package.json"),
        format!(r#"{{"name":"{INNER_DEP}","type":"module","exports":"./index.js"}}"#),
    )
    .unwrap();
    std::fs::write(
        dep.join("index.js"),
        "export const innerValue = \"inner-dep-ok\";\n",
    )
    .unwrap();
    let route = root.join("src/routes/index.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    std::fs::write(
        &route,
        format!("import \"../../packages/inner/index.js\";\n{source}"),
    )
    .unwrap();
}

/// The ids Vite committed for the ssr environment.
fn optimized(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".vite-deps/deps_ssr/_metadata.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|meta| {
            meta["optimized"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
        })
        .unwrap_or_default()
}

fn polyfill(id: &str) -> bool {
    id.starts_with("@cloudflare/unenv-preset/") || id.starts_with("unenv/")
}

/// Whether `cond` holds within `wait`.
fn settles(wait: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + wait;
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn lines<'a>(log: &'a str, needle: &str) -> Vec<&'a str> {
    log.lines().filter(|l| l.contains(needle)).collect()
}

/// What the dev server logged after the boot-time seed. The first seed line:
/// a hash mismatch can start a setup-registration child that logs another.
fn after_seed(log: &str) -> &str {
    let at = log
        .find("deps pre-optimized for environment")
        .unwrap_or_else(|| panic!("the child seeded nothing:\n{log}"));
    &log[at..]
}

/// When oj (or its plugin host) logged boot phase `label`, in epoch ms.
fn phase(log: &str, label: &str) -> u128 {
    log.lines()
        .filter_map(|l| l.strip_prefix("[oj-phase] "))
        .find_map(|l| {
            let (ms, rest) = l.split_once(' ')?;
            (rest == label).then(|| ms.parse().ok()).flatten()
        })
        .unwrap_or_else(|| panic!("no boot phase {label:?}:\n{log}"))
}

/// One `oj dev` session serving `/`, with oj's cache under `cache`: the test
/// reads the stamp and the include snapshot there.
fn boot(root: &Path, cache: &Path) -> Server {
    let server = Server::oj_dev_with(
        root,
        &[
            ("OJ_CACHE_DIR", cache.to_str().unwrap()),
            ("WRANGLER_SEND_METRICS", "false"),
            ("CI", "1"),
            ("NO_COLOR", "1"),
            ("DEBUG", "oj:boot"),
        ],
    );
    // Any answer: the render discovers the workspace package's import.
    server.request("GET", "/", &[]);
    assert!(
        server.logs("a custom dev environment is declared"),
        "no runner environment:\n{}",
        server.output()
    );
    server
}

#[test]
fn the_dev_server_never_optimizes_what_the_preseed_child_seeded() {
    let Some(cf) = cloudflare_app("preseed") else {
        return;
    };
    let root = cf.app.0.clone();
    prepare(&root);
    let cache: PathBuf = root.join(".oj-cache-test");

    // A cold first session.
    {
        let server = boot(&root, &cache);
        assert!(
            settles(Duration::from_secs(30), || optimized(&root)
                .iter()
                .any(|id| polyfill(id))),
            "the setup's polyfills were never optimized: {:?}\n{}",
            optimized(&root),
            server.output()
        );
        assert!(
            settles(Duration::from_secs(30), || optimized(&root)
                .iter()
                .any(|id| id == INNER_DEP)),
            "the workspace package's dep was never optimized: {:?}",
            optimized(&root)
        );
        // Long enough for a rerun of the dev server's own optimizer to log.
        std::thread::sleep(Duration::from_secs(3));
        let log = server.output();
        // The session finds the workspace package's dep at runtime, as Vite's
        // own session would; nothing else may run the optimizer in process.
        let in_process: Vec<_> = lines(&log, "optimized: ")
            .into_iter()
            .filter(|l| l.contains("dependenc") && !l.ends_with(&format!("optimized: {INNER_DEP}")))
            .collect();
        assert!(
            in_process.is_empty(),
            "the dev server's optimizer ran in process:\n{}",
            in_process.join("\n")
        );
        let rehashed = lines(&log, "Re-optimizing dependencies");
        assert!(
            rehashed.is_empty(),
            "the dev server rejected the child's deps cache:\n{}",
            rehashed.join("\n")
        );
        // The host starts alongside the seed and reaches its environments
        // while the seed still runs; only building them waits.
        let seeded = phase(&log, "deps preseed done");
        assert!(
            phase(&log, "plugin host spawning") < seeded,
            "the plugin host waited for the whole seed to start:\n{log}"
        );
        assert!(
            phase(&log, "waiting for the deps pre-seed") < seeded,
            "the host reached its environments only after the seed was over:\n{log}"
        );
        assert!(
            phase(&log, "deps pre-seed over") >= seeded,
            "the plugin host built its environments before the seed was over:\n{log}"
        );
    }

    // A second cold boot replays the first session's deps: by name through
    // the include snapshot, only ids Vite's include resolver reaches from the
    // root; the workspace package's dep through the file it resolved to,
    // registered on the child's optimizer.
    std::fs::remove_file(cache.join("v1/optimize-env-stamp.json")).unwrap();
    let server = boot(&root, &cache);
    std::thread::sleep(Duration::from_secs(3));
    let log = server.output();
    assert!(
        log.contains("deps: pre-optimizing"),
        "the second boot never ran the child:\n{log}"
    );
    let unresolved = lines(&log, "Failed to resolve dependency");
    assert!(
        unresolved.is_empty(),
        "the replay named unresolvable deps:\n{}",
        unresolved.join("\n")
    );
    let rehashed = lines(after_seed(&log), "Re-optimizing dependencies");
    assert!(
        rehashed.is_empty(),
        "the dev server rejected the child's deps cache:\n{}",
        rehashed.join("\n")
    );
    let in_process: Vec<_> = lines(after_seed(&log), "optimized: ")
        .into_iter()
        .filter(|l| l.contains("dependenc"))
        .collect();
    assert!(
        in_process.is_empty(),
        "the dev server's optimizer ran in process:\n{}",
        in_process.join("\n")
    );
    assert!(
        optimized(&root).iter().any(|id| id == INNER_DEP),
        "the child did not commit the workspace package's dep: {:?}",
        optimized(&root)
    );
    // The replay itself still carries every dep the root resolves.
    let snapshot: Value = serde_json::from_str(
        &std::fs::read_to_string(cache.join("v1/preseed-include.json")).unwrap(),
    )
    .unwrap();
    let replayed: Vec<&str> = snapshot["environments"]["ssr"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        replayed.contains(&"react")
            && replayed.iter().any(|id| polyfill(id))
            && !replayed.contains(&INNER_DEP),
        "unexpected replay set: {replayed:?}"
    );
}
