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
    let plain = seen(&server.text("/src/main.js").await)["plain"].clone();
    assert!(
        plain
            .as_str()
            .is_some_and(|p| p.starts_with("deps/plain-lib") && p.contains("?v=")),
        "the first transform waited for the scan: {plain}"
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

/// Plugins for the parity cases: an `order: "pre"` hook and an
/// `enforce: "pre"` plugin that record the bare ids they are offered, and
/// answers of each shape Vite's import analysis handles.
const PARITY_PLUGINS: &str = r#"const alt = (name) => new URL(`./src/alt/${name}.js`, import.meta.url).pathname;
const nm = (pkg) => new URL(`./node_modules/${pkg}/index.js`, import.meta.url).pathname;
const bare = (id) => !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !id.startsWith("virtual:");
const seen = { ordered: [], enforced: [] };
export default [
  {
    name: "ordered",
    resolveId: { order: "pre", handler(id) { if (bare(id)) seen.ordered.push(id); return null; } },
  },
  {
    name: "enforced",
    enforce: "pre",
    resolveId(id) {
      if (bare(id)) seen.enforced.push(id);
      if (id === "aliased-target") return alt("aliased-target");
      if (id === "self-id") return "self-id";
      if (id === "root-rel") return "/src/alt/root-rel.js";
      if (id === "cdn-flag-off") return { id: "https://cdn.example/off.js", external: false };
      if (id === "claimed-nm") return nm("real-pkg");
      if (id === "ext-bare") return { id: "some-shim", external: true };
      if (id === "queried") return alt("queried") + "?raw";
      return null;
    },
    load(id) { return id === "self-id" ? "export default 'self';" : null; },
  },
  {
    name: "pass",
    enforce: "pre",
    resolveId: {
      filter: { id: /^pass2$/ },
      async handler(id, importer) {
        const r = await this.resolve("plain-lib", importer, { skipSelf: true });
        seen.pass = r ? r.id.split("/").slice(-2).join("/") : null;
        return r;
      },
    },
  },
  {
    name: "virtuals",
    resolveId(id) { return ["virtual:seen", "virtual:v", "virtual:emit"].includes(id) ? "\0" + id : null; },
    async load(id) {
      if (id === "\0virtual:seen") return `export default ${JSON.stringify(seen)};`;
      if (id === "\0virtual:v") return `import p from "pass2"; export default p;`;
      if (id === "\0virtual:emit") {
        const main = new URL("./src/main.js", import.meta.url).pathname;
        const r = await this.resolve("plain-lib", main);
        return `export { default } from ${JSON.stringify(r.id)};`;
      }
      return null;
    },
  },
  {
    name: "reports-missed",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const r = await this.resolve("unused-lib", id);
      const missed = r ? r.id.split("/").slice(-2).join("/") : null;
      return code + "\nexport const seen = " + JSON.stringify({ missed }) + ";\n";
    },
  },
];
"#;

const PARITY_MAIN: &str = r#"import aliased from "alias-key";
import self from "self-id";
import rootRel from "root-rel";
import cdn from "cdn-flag-off";
import plain from "plain-lib";
import emit from "virtual:emit";
import v from "virtual:v";
export const all = [aliased, self, rootRel, cdn, plain, emit, v];
"#;

fn parity_app() -> App {
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"pre-resolve-parity","private":true,"type":"module"}"#.into(),
        ),
        ("index.html".into(), INDEX_HTML.into()),
        ("oj.plugins.mjs".into(), PARITY_PLUGINS.into()),
        (
            "oj.config.json".into(),
            // `queried` stays in the ssr pipeline, so a plugin's answer counts.
            r#"{ "resolve": { "alias": { "alias-key": "aliased-target" } }, "ssr": { "noExternal": ["queried"] } }"#.into(),
        ),
        ("src/main.js".into(), PARITY_MAIN.into()),
        ("src/alt/aliased-target.js".into(), "export default 'alt';\n".into()),
        ("src/alt/root-rel.js".into(), "export default 'rootrel';\n".into()),
        ("src/alt/queried.js".into(), "export default 'q';\n".into()),
    ];
    for name in [
        "plain-lib",
        "unused-lib",
        "real-pkg",
        "queried",
        "alias-key",
    ] {
        files.extend(package(name));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    App::new(&refs)
}

// Vite's chain for a bare import: the `order: "pre"` hooks on the specifier as
// written, the alias plugin (the aliased id resolved again), then the
// `enforce: "pre"` plugins, which never see an aliased specifier itself.
// Import analysis keeps a remote URL as is whatever its `external` flag, and
// serves the specifier itself or a root-relative id a plugin answers.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn pre_plugins_see_imports_as_vites_chain_orders_them() {
    let app = parity_app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;

    assert!(
        import_url(&main, "aliased").starts_with("/src/alt/aliased-target.js"),
        "a pre plugin claims the alias target: {main}"
    );
    assert!(
        import_url(&main, "rootRel").starts_with("/src/alt/root-rel.js"),
        "a root-relative answer is the file's URL: {main}"
    );
    assert_eq!(import_url(&main, "cdn"), "https://cdn.example/off.js");
    let self_url = import_url(&main, "self");
    let body = server.text(&self_url).await;
    assert!(
        body.contains("\"self\"") || body.contains("'self'"),
        "the plugin's own id loads: {self_url}\n{body}"
    );

    let seen_url = format!("/@id/{}", "virtual:seen");
    let seen_mod = server.text(&seen_url).await;
    let compact: String = seen_mod.split_whitespace().collect();
    let list = |name: &str| {
        let re = Regex::new(&format!(r#""{name}":\[([^\]]*)\]"#)).unwrap();
        re.captures(&compact).expect("a recorded list")[1].to_string()
    };
    let ordered = list("ordered");
    assert!(
        ordered.contains("\"alias-key\"") && ordered.contains("aliased-target"),
        "the ordered hook sees the specifier and the alias: {ordered}"
    );
    let enforced = list("enforced");
    assert!(
        enforced.contains("aliased-target") && !enforced.contains("alias-key"),
        "enforce: pre plugins see only the aliased id: {enforced}"
    );
}

// tryOptimizedResolve and finalizeBareSpecifier: a pre-bundle's own path a
// plugin emits is served at the bundle's URL (one module instance), a dep the
// scan missed answers with its future bundle, and `this.resolve` from a
// virtual module resolves from the root.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_bundle_ids_map_to_one_module() {
    let app = parity_app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let plain = import_url(&main, "plain");
    assert!(plain.starts_with("/@oj-deps/plain-lib"), "{main}");

    let emit = server.text(&import_url(&main, "emit")).await;
    assert!(
        emit.contains(&format!("\"{plain}\"")),
        "an emitted bundle path is the bundle's URL ({plain}):\n{emit}"
    );
    let missed = seen(&main)["missed"].clone();
    assert!(
        missed
            .as_str()
            .is_some_and(|m| m.starts_with("deps/unused-lib") && m.contains("?v=")),
        "a dep the scan missed answers with its future bundle: {missed}"
    );
    let v = server.text(&import_url(&main, "v")).await;
    assert!(!v.contains("Failed to resolve"), "{v}");
    let seen_mod = server.text("/@id/virtual:seen").await;
    assert!(
        seen_mod.contains("plain-lib"),
        "this.resolve from a virtual module resolves from the root: {seen_mod}"
    );
}

// Vite decides SSR externalization on the specifier before any plugin
// resolves: a plugin's node_modules file is a module, a non-URL `external`
// answer is a module id too, and a queried answer stands.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ssr_pre_answers_are_modules_as_vite_fetches_them() {
    let app = parity_app();
    let server = app.serve().await;
    let main = app.root.join("src/main.js");
    let resolve = |spec: &'static str| {
        let url = format!("/@ssr-resolve?importer={}&spec={spec}", main.display());
        let server = &server;
        async move { server.text(&url).await }
    };
    let claimed = resolve("claimed-nm").await;
    assert!(
        !claimed.contains(r#""external""#) && claimed.contains("real-pkg/index.js"),
        "{claimed}"
    );
    let ext = resolve("ext-bare").await;
    assert!(!ext.contains(r#""external":true"#), "{ext}");
    let queried = resolve("queried").await;
    assert!(queried.contains("alt/queried.js?raw"), "{queried}");
}

// The same chain for a plugin's `this.resolve`, through Vite's own resolved
// `resolve.alias`: an `enforce: "pre"` plugin is offered the aliased id only.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_aliases_before_the_enforce_pre_plugins() {
    crate::require_fixture!("vite");
    let config = r#"const seen = [];
export default {
  resolve: { alias: { "alias-key": "aliased-target" } },
  plugins: [
    {
      name: "enforced",
      enforce: "pre",
      resolveId(id) {
        if (id === "alias-key" || id === "aliased-target") seen.push(id);
        return id === "aliased-target" ? "virtual-aliased" : null;
      },
    },
    {
      name: "reports",
      async transform(code, id) {
        if (!id.split("?")[0].endsWith("/src/main.js")) return null;
        const r = await this.resolve("alias-key", id);
        return code + "\nexport const seen = " + JSON.stringify({ id: r && r.id, offered: seen.join(",") }) + ";\n";
      },
    },
  ],
};
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ctx-alias","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "export const value = 1;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    let seen = seen(&server.text("/src/main.js").await);
    assert_eq!(seen["id"], "virtual-aliased", "{seen}");
    assert_eq!(seen["offered"], "aliased-target", "{seen}");
}
