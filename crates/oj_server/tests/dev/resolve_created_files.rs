// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite resolves through rolldown's native `vite_resolve` plugin, whose
//! `watchChange` clears its resolver cache on a create or delete, so a created
//! file resolves at once and a deleted one stops resolving, whatever HMR does.
//! oj's resolvers cache lookups, misses included; dropping them must not
//! depend on HMR, which `server.hmr: false` skips (as do a held editor gate
//! and its full-reload flush). Checked
//! through the SSR resolver, where a missed alias (`@/lib/x`, tsconfig
//! `paths`) falls back to a bare package the runtime then cannot find.

use std::time::{Duration, Instant};

use crate::App;

fn app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"resolve-created","version":"1.0.0","type":"module"}"#,
        ),
        (
            "vite.config.mjs",
            "export default { server: { hmr: false } };\n",
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["./src/*"]}}}"#,
        ),
        ("index.html", crate::INDEX_HTML),
        ("src/main.js", "export {};\n"),
        ("src/server/lab.ts", "export const lab = 1;\n"),
    ])
}

/// `/@ssr-resolve` for `spec` from `src/server/lab.ts`.
async fn ssr_resolve(server: &crate::Server, app: &App, spec: &str) -> serde_json::Value {
    let importer = app.root.join("src/server/lab.ts");
    let url = format!(
        "/@ssr-resolve?importer={}&spec={}",
        urlencoding(&importer.to_string_lossy()),
        urlencoding(spec)
    );
    let text = server.text(&url).await;
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Polls the resolution until `done` holds, or the deadline passes.
async fn resolution_until(
    server: &crate::Server,
    app: &App,
    spec: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let r = ssr_resolve(server, app, spec).await;
        if done(&r) || Instant::now() > deadline {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn resolved_to(r: &serde_json::Value, suffix: &str) -> bool {
    r["id"].as_str().is_some_and(|id| id.ends_with(suffix))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_created_file_resolves_with_hmr_off() {
    let app = app();
    let server = app.serve().await;
    // A miss first (the resolver caches it): the alias is a bare package.
    let before = ssr_resolve(&server, &app, "@/lib/auth/helpers").await;
    assert_eq!(before["external"], true, "{before}");

    // Created in a directory that did not exist either.
    app.write(&[("src/lib/auth/helpers.ts", "export const h = 1;\n")]);
    let after = resolution_until(&server, &app, "@/lib/auth/helpers", |r| {
        resolved_to(r, "src/lib/auth/helpers.ts")
    })
    .await;
    assert!(
        resolved_to(&after, "src/lib/auth/helpers.ts"),
        "the created file never resolved: {after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_file_stops_resolving_with_hmr_off() {
    let app = app();
    app.write(&[("src/lib/gone.ts", "export const g = 1;\n")]);
    let server = app.serve().await;
    let before = ssr_resolve(&server, &app, "@/lib/gone").await;
    assert!(resolved_to(&before, "src/lib/gone.ts"), "{before}");

    std::fs::remove_file(app.root.join("src/lib/gone.ts")).unwrap();
    let after = resolution_until(&server, &app, "@/lib/gone", |r| {
        !resolved_to(r, "src/lib/gone.ts")
    })
    .await;
    assert!(
        !resolved_to(&after, "src/lib/gone.ts"),
        "the deleted file still resolved: {after}"
    );
}

/// Created, deleted, then created again: each step is a create or delete to
/// Vite's resolver cache, so the second create resolves again. A path the
/// watcher had seen before counted as an edit and left the miss cached.
#[tokio::test(flavor = "multi_thread")]
async fn a_recreated_file_resolves_again_with_hmr_off() {
    let app = app();
    let server = app.serve().await;
    let spec = "@/lib/again";
    app.write(&[("src/lib/again.ts", "export const a = 1;\n")]);
    let first = resolution_until(&server, &app, spec, |r| resolved_to(r, "src/lib/again.ts")).await;
    assert!(
        resolved_to(&first, "src/lib/again.ts"),
        "never resolved: {first}"
    );

    std::fs::remove_file(app.root.join("src/lib/again.ts")).unwrap();
    let gone = resolution_until(&server, &app, spec, |r| !resolved_to(r, "src/lib/again.ts")).await;
    assert!(
        !resolved_to(&gone, "src/lib/again.ts"),
        "still resolved: {gone}"
    );

    app.write(&[("src/lib/again.ts", "export const a = 2;\n")]);
    let again = resolution_until(&server, &app, spec, |r| resolved_to(r, "src/lib/again.ts")).await;
    assert!(
        resolved_to(&again, "src/lib/again.ts"),
        "the recreated file never resolved: {again}"
    );
}
