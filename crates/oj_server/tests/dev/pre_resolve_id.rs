// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's resolve order for an import: the `pre` plugins' resolveId
//! (`enforce: "pre"`), then vite:resolve, then every other plugin. A `pre`
//! plugin can answer a bare import a package also provides; a normal plugin
//! only sees what vite:resolve could not resolve. A plugin's `this.resolve`
//! takes the same order, and a client bare import already pre-bundled
//! resolves to its bundle (tryOptimizedResolve). Before, oj never offered a
//! resolvable bare import to any plugin, and `this.resolve` ran the whole
//! chain ahead of oj's resolver.

use regex::Regex;

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"const alt = (name) => new URL(`./src/alt/${name}.js`, import.meta.url).pathname;
export default [
  {
    name: "redirect",
    enforce: "pre",
    resolveId: {
      filter: { id: /^(shared-lib|linked-lib)$/ },
      handler(id) { return { id: alt(id), external: false }; },
    },
  },
  {
    name: "passthrough",
    enforce: "pre",
    resolveId: {
      filter: { id: /^pass-lib$/ },
      handler(id, importer) { return this.resolve(id, importer, { skipSelf: true }); },
    },
  },
  {
    name: "virtual",
    enforce: "pre",
    resolveId: {
      filter: { id: /^virt-lib$/ },
      handler() { return "\0virt-lib"; },
    },
    load(id) { return id === "\0virt-lib" ? "export default 'from-virtual';" : null; },
  },
  {
    name: "cdn",
    enforce: "pre",
    resolveId: {
      filter: { id: /^cdn-lib$/ },
      handler() { return { id: "https://cdn.example/lib.js", external: true }; },
    },
  },
  {
    name: "late",
    resolveId(id) { return id === "late-lib" || id === "only-late" ? "\0" + id : null; },
    load(id) { return id === "\0only-late" ? "export default 'late';" : null; },
  },
  {
    name: "reports-resolve",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const tail = async (spec) => {
        const r = await this.resolve(spec, id);
        return r ? r.id.replace(/\0/g, "\\0").split("/").slice(-2).join("/") : null;
      };
      const seen = {
        shared: await tail("shared-lib"),
        late: await tail("late-lib"),
        onlyLate: await tail("only-late"),
        plain: await tail("plain-lib"),
      };
      return code + "\nexport const seen = " + JSON.stringify(seen) + ";\n";
    },
  },
];
"#;

const MAIN: &str = r#"import shared from "shared-lib";
import pass from "pass-lib";
import virt from "virt-lib";
import cdn from "cdn-lib";
import late from "late-lib";
import plain from "plain-lib";
export const all = [shared, pass, virt, cdn, late, plain];
"#;

fn package(name: &str) -> [(String, String); 2] {
    [
        (
            format!("node_modules/{name}/package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","type":"module","main":"index.js"}}"#),
        ),
        (
            format!("node_modules/{name}/index.js"),
            format!("export default 'installed {name}';\n"),
        ),
    ]
}

fn app() -> App {
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"pre-resolve","private":true,"type":"module"}"#.into(),
        ),
        ("index.html".into(), INDEX_HTML.into()),
        ("oj.plugins.mjs".into(), PLUGINS.into()),
        ("src/main.js".into(), MAIN.into()),
        (
            "src/alt/shared-lib.js".into(),
            "export default 'alt';\n".into(),
        ),
        (
            "src/alt/linked-lib.js".into(),
            "export default 'alt';\n".into(),
        ),
        (
            "packages/linked-lib/package.json".into(),
            r#"{"name":"linked-lib","version":"1.0.0","type":"module","main":"index.js"}"#.into(),
        ),
        (
            "packages/linked-lib/index.js".into(),
            "export default 'linked';\n".into(),
        ),
    ];
    for name in [
        "shared-lib",
        "pass-lib",
        "virt-lib",
        "late-lib",
        "plain-lib",
    ] {
        files.extend(package(name));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let app = App::new(&refs);
    // A workspace member: linked into node_modules, resolved to its source.
    std::os::unix::fs::symlink(
        app.root.join("packages/linked-lib"),
        app.root.join("node_modules/linked-lib"),
    )
    .unwrap();
    app
}

/// The `this.resolve` answers the transform reported.
fn seen(main: &str) -> serde_json::Value {
    let compact: String = main.split_whitespace().collect();
    let seen = Regex::new(r#"exportconstseen=(\{[^}]*\})"#)
        .unwrap()
        .captures(&compact)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| panic!("no report:\n{main}"));
    serde_json::from_str(&seen).unwrap()
}

/// The URL `main` imports `spec`'s default from.
fn import_url(main: &str, binding: &str) -> String {
    let re = Regex::new(&format!(r#"import {binding} from "([^"]+)""#)).unwrap();
    match re.captures(main) {
        Some(c) => c[1].to_string(),
        None => panic!("no import of {binding}:\n{main}"),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn pre_plugins_resolve_bare_imports_before_oj_in_the_browser() {
    let app = app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;

    let shared = import_url(&main, "shared");
    assert!(
        shared.starts_with("/src/alt/shared-lib.js"),
        "a pre plugin wins over the installed package: {shared}\n{main}"
    );
    let pass = import_url(&main, "pass");
    assert!(
        pass.starts_with("/@oj-deps/"),
        "a passthrough keeps the pre-bundle: {pass}"
    );
    let virt = import_url(&main, "virt");
    let body = server.text(&virt).await;
    assert!(
        body.contains("from-virtual"),
        "the pre plugin's virtual module, not the package: {virt}\n{body}"
    );
    assert_eq!(
        import_url(&main, "cdn"),
        "https://cdn.example/lib.js",
        "a remote URL a plugin answers stays as is"
    );
    let late = import_url(&main, "late");
    assert!(
        late.starts_with("/@oj-deps/"),
        "a normal plugin never sees what oj's resolver resolves: {late}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_runs_pre_plugins_then_oj_then_the_rest() {
    let app = app();
    let server = app.serve().await;
    let seen = seen(&server.text("/src/main.js").await);
    assert_eq!(seen["shared"], "alt/shared-lib.js", "{seen}");
    let late = seen["late"].as_str().unwrap_or_default();
    assert!(
        !late.contains("\\0"),
        "oj's resolver answers before the normal plugin: {seen}"
    );
    assert_eq!(seen["onlyLate"], "\\0only-late", "{seen}");
}

// tryOptimizedResolve: once the dep is pre-bundled, a client `this.resolve`
// of it answers with the bundle, as the import itself is rewritten.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_of_a_pre_bundled_dep_answers_with_the_bundle() {
    let app = app();
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut edit = 0;
    let plain = loop {
        edit += 1;
        app.write(&[("src/main.js", &format!("{MAIN}// edit {edit}\n"))]);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let plain = seen(&server.text("/src/main.js").await)["plain"].clone();
        if plain
            .as_str()
            .is_some_and(|p| p.starts_with("deps/plain-lib"))
        {
            break plain;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never the bundle: {plain}"
        );
    };
    assert!(
        plain.as_str().unwrap().contains("?v="),
        "versioned like Vite's: {plain}"
    );
}

// tryOptimizedResolve awaits `scanProcessing`: a transform that runs while
// the dependency scan is still going waits for it, and its `this.resolve`
// of a dep the scan finds answers with the bundle on the first transform.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_during_the_dep_scan_waits_for_it() {
    let app = app();
    // Holds the scan (its resolves carry `scan: true`) well past the first
    // request, which no other plugin delays.
    let slow = r#"
  {
    name: "slow-scan",
    async resolveId(id, importer, options) {
      if (options?.scan && id === "plain-lib") await new Promise((r) => setTimeout(r, 2000));
      return null;
    },
  },
];
"#;
    let plugins = PLUGINS.replacen("\n];\n", slow, 1);
    assert_ne!(plugins, PLUGINS);
    app.write(&[("oj.plugins.mjs", &plugins)]);
    let server = app.serve().await;
    let started = std::time::Instant::now();
    let plain = seen(&server.text("/src/main.js").await)["plain"].clone();
    assert!(
        plain
            .as_str()
            .is_some_and(|p| p.starts_with("deps/plain-lib") && p.contains("?v=")),
        "the first transform waited for the scan: {plain}"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(500),
        "the scan was over before the transform, so nothing waited"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn pre_plugins_resolve_bare_imports_in_ssr() {
    let app = app();
    let server = app.serve().await;
    let importer = app.root.join("src/main.js");
    let resolve = |spec: &'static str| {
        let url = format!("/@ssr-resolve?importer={}&spec={spec}", importer.display());
        let server = &server;
        async move { server.text(&url).await }
    };

    let linked = resolve("linked-lib").await;
    assert!(
        linked.contains("/src/alt/linked-lib.js\""),
        "a pre plugin answers a workspace member's import: {linked}"
    );
    // Vite externalizes an installed dependency before any plugin resolves it.
    let shared = resolve("shared-lib").await;
    assert!(
        shared.contains(r#""external":true"#) && shared.contains(r#""spec":"shared-lib""#),
        "{shared}"
    );
    let cdn = resolve("cdn-lib").await;
    assert!(
        cdn.contains(r#""external":true"#) && cdn.contains("https://cdn.example/lib.js"),
        "{cdn}"
    );
}
