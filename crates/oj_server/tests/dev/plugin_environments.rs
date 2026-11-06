// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Every environment runs the same plugin instances, as Vite's createServer
//! gives each environment a container over one resolved plugin array: the
//! ssr environment sees what the client's hooks and configureServer left
//! behind, the plugins file is evaluated once, and the dev server starts the
//! plugins on the client environment only (Vite's
//! perEnvironmentStartEndDuringDev opts a plugin into the others).

use crate::App;

const INDEX_HTML: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_ssr_environment_shares_the_clients_plugin_instances() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
log("eval");
let server = null;
let clientTransformed = false;
export default [
  {{
    name: "shared",
    configureServer(s) {{
      server = s;
      log("configureServer");
    }},
    buildStart() {{
      log("buildStart " + this.environment.name);
    }},
    transform(code, id) {{
      if (!id.endsWith("/src/main.js")) return;
      const env = this.environment.name;
      if (env === "client") clientTransformed = true;
      return code.replace("__STATE__", JSON.stringify({{ env, clientTransformed, server: !!(server && server.ws) }}));
    }},
  }},
  {{
    name: "per-environment",
    perEnvironmentStartEndDuringDev: true,
    buildStart() {{
      log("per-environment buildStart " + this.environment.name);
    }},
  }},
];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "export const state = __STATE__;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    let compact = |s: String| s.split_whitespace().collect::<String>();
    let client = compact(server.text("/src/main.js").await);
    assert!(
        client.contains(r#""env":"client""#),
        "the client transform ran:\n{client}"
    );
    let main = app.root.join("src/main.js");
    let ssr = compact(
        server
            .text(&format!("/@ssr-module?id={}", main.display()))
            .await,
    );
    assert!(
        ssr.contains(r#"{"env":"ssr","clientTransformed":true,"server":true}"#),
        "the ssr transform runs on the client's instance, configureServer's server in hand:\n{ssr}"
    );

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let count = |line: &str| text.lines().filter(|l| *l == line).count();
    assert_eq!(
        count("eval"),
        1,
        "the plugins file is evaluated once:\n{text}"
    );
    assert_eq!(count("configureServer"), 1, "{text}");
    assert_eq!(count("buildStart client"), 1, "{text}");
    assert_eq!(
        count("buildStart ssr"),
        0,
        "the dev server starts plugins on the client only:\n{text}"
    );
    assert_eq!(count("per-environment buildStart client"), 1, "{text}");
    assert_eq!(
        count("per-environment buildStart ssr"),
        1,
        "perEnvironmentStartEndDuringDev starts it per environment:\n{text}"
    );
}

// Vite's dev server runs watchChange on the client environment and, in
// another environment, only for a plugin that asks for it per environment
// (EnvironmentPluginContainer.watchChange); hotUpdate runs in each
// environment over its own plugins, the legacy handleHotUpdate once, in the
// client's pass. The ssr environment exists from boot: an edit made before
// any SSR request reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn an_ssr_only_plugin_gets_watch_and_hot_updates_from_boot() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
const main = (id) => id.endsWith("/src/main.js");
export default [
  {{
    name: "ssr-per-environment",
    applyToEnvironment: (environment) => environment.name === "ssr",
    perEnvironmentWatchChangeDuringDev: true,
    watchChange(id) {{
      if (main(id)) log("ssr-per-environment watchChange " + this.environment.name);
    }},
    hotUpdate({{ file }}) {{
      if (main(file)) log("ssr-per-environment hotUpdate " + this.environment.name);
    }},
  }},
  {{
    name: "ssr-plain",
    applyToEnvironment: (environment) => environment.name === "ssr",
    watchChange(id) {{
      if (main(id)) log("ssr-plain watchChange " + this.environment.name);
    }},
  }},
  {{
    name: "shared",
    watchChange(id) {{
      if (main(id)) log("shared watchChange " + this.environment.name);
    }},
    handleHotUpdate({{ file }}) {{
      if (main(file)) log("shared handleHotUpdate");
    }},
  }},
];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-hmr","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    assert!(server.text("/src/main.js").await.contains("__ok"));
    app.write(&[("src/main.js", "window.__ok = 2;\n")]);
    let expected = [
        "shared handleHotUpdate",
        "shared watchChange client",
        "ssr-per-environment hotUpdate ssr",
        "ssr-per-environment watchChange ssr",
    ];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if expected.iter().all(|e| text.lines().any(|l| l == *e)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hooks never all ran: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Any duplicate would have landed with the first dispatch.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, expected, "each hook once, where Vite runs it");
}

// Each Vite DevEnvironment carries its own module graph and hot channel; the
// ssr environment's are not the client's.
#[tokio::test(flavor = "multi_thread")]
async fn the_ssr_environment_has_its_own_module_graph_and_hot_channel() {
    let plugins = r#"let client = null;
export default [{
  name: "graph-probe",
  transform(code, id) {
    if (!id.endsWith("/src/main.js")) return;
    const env = this.environment;
    if (env.name === "client") client = env;
    const graph = env.moduleGraph;
    const seen = {
      env: env.name,
      graph: typeof graph?.getModuleById === "function" && typeof graph?.invalidateModule === "function",
      hot: typeof env.hot?.send === "function" && typeof env.hot?.on === "function",
      own: env.name === "client" || (!!client && graph !== client.moduleGraph && env.hot !== client.hot),
      known: !!graph?.getModuleById(id),
    };
    return code.replace("__STATE__", JSON.stringify(seen));
  },
}];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-graph","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "export const state = __STATE__;\n"),
        ("oj.plugins.mjs", plugins),
    ]);
    let server = app.serve().await;
    let compact = |s: String| s.split_whitespace().collect::<String>();
    let client = compact(server.text("/src/main.js").await);
    assert!(
        client.contains(r#"{"env":"client","graph":true,"hot":true,"own":true,"known":true}"#),
        "{client}"
    );
    let main = app.root.join("src/main.js");
    let ssr = compact(
        server
            .text(&format!("/@ssr-module?id={}", main.display()))
            .await,
    );
    assert!(
        ssr.contains(r#"{"env":"ssr","graph":true,"hot":true,"own":true,"known":true}"#),
        "the ssr environment's own graph and hot channel:\n{ssr}"
    );
}

// The deps a runner environment's optimizer is handed while the server sets
// up are optimized in batches, each after the last: a batch whose work fails
// (the pre-seed child and the in-process fallback alike) never stops the
// batches after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_boot_dep_batch_leaves_the_next_one_running() {
    crate::require_fixture!("vite");
    let config = r#"import { DevEnvironment } from "vite";
const registered = [];
const optimizer = {
  metadata: { optimized: {}, chunks: {}, discovered: {}, depInfoList: [], browserHash: "h" },
  registerMissingImport(id) {
    if (id === "dep-a") throw new Error("this process's optimizer failed too");
    registered.push(id);
    return { id, processing: Promise.resolve() };
  },
};
const runner = {
  name: "runner-env",
  config() {
    return {
      environments: {
        worker: {
          consumer: "server",
          dev: {
            createEnvironment(name, config) {
              const environment = new DevEnvironment(name, config, { hot: true });
              environment.depsOptimizer = optimizer;
              return environment;
            },
          },
        },
      },
    };
  },
  async configureServer(server) {
    server.middlewares.use((req, res, next) => {
      if (req.url !== "/__registered") return next();
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(registered));
    });
    const deps = server.environments.worker.depsOptimizer;
    deps.registerMissingImport("dep-a", "/a.js");
    await new Promise((resolve) => setTimeout(resolve, 200));
    deps.registerMissingImport("dep-b", "/b.js");
  },
};
export default { plugins: [runner] };
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"boot-dep-batches","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let registered = server.text("/__registered").await;
        if registered.contains("dep-b") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the batch after a failed one never ran: {registered}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// A setup-time dep's stand-in is what Vite's addMissingDep makes: a browser
// hash of its own discovery (getDiscoveredBrowserHash, never the metadata's),
// and a `processing` that settles even when no optimizer took the dep over.
#[tokio::test(flavor = "multi_thread")]
async fn a_boot_dep_stand_in_has_its_own_hash_and_always_settles() {
    crate::require_fixture!("vite");
    let config = r#"import { DevEnvironment } from "vite";
let seen = null;
const optimizer = {
  metadata: { hash: "m", optimized: {}, chunks: {}, discovered: {}, depInfoList: [], browserHash: "h" },
  registerMissingImport(id) {
    return { id, processing: Promise.resolve() };
  },
};
const runner = {
  name: "runner-env",
  config() {
    return {
      environments: {
        worker: {
          consumer: "server",
          dev: {
            createEnvironment(name, config) {
              const environment = new DevEnvironment(name, config, { hot: true });
              environment.depsOptimizer = optimizer;
              return environment;
            },
          },
        },
      },
    };
  },
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      if (req.url !== "/__seen") return next();
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(seen));
    });
    const environment = server.environments.worker;
    const info = environment.depsOptimizer.registerMissingImport("dep-a", "/a.js");
    // The environment drops its optimizer before the batch runs: nothing
    // takes the stand-in over.
    environment.depsOptimizer = undefined;
    info.processing.then(
      () => (seen = { hash: info.browserHash, processing: "settled" }),
      (e) => (seen = { hash: info.browserHash, processing: String(e && e.message) }),
    );
  },
};
export default { plugins: [runner] };
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"boot-dep-stand-in","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let seen = loop {
        let seen: serde_json::Value = serde_json::from_str(&server.text("/__seen").await).unwrap();
        if !seen.is_null() {
            break seen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stand-in's processing never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    assert_eq!(seen["processing"], "settled", "{seen}");
    let hash = seen["hash"].as_str().unwrap();
    assert!(hash != "h" && hash.len() == 8, "{seen}");
}

// Vite's createServer inits every environment before configureServer runs:
// `server.environments.ssr` is the environment the ssr hooks see as
// `this.environment`, whose module graph knows what they transformed.
#[tokio::test(flavor = "multi_thread")]
async fn configure_server_sees_the_ssr_environment_itself() {
    let plugins = r#"let captured = null;
export default [{
  name: "captures-ssr",
  configureServer(server) {
    captured = server.environments.ssr;
  },
  transform(code, id) {
    if (!id.endsWith("/src/main.js") || this.environment.name !== "ssr") return;
    const seen = {
      same: this.environment === captured,
      known: !!captured?.moduleGraph?.getModuleById(id),
    };
    return code.replace("__STATE__", JSON.stringify(seen));
  },
}];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-identity","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "export const state = __STATE__;\n"),
        ("oj.plugins.mjs", plugins),
    ]);
    let server = app.serve().await;
    let main = app.root.join("src/main.js");
    let ssr: String = server
        .text(&format!("/@ssr-module?id={}", main.display()))
        .await
        .split_whitespace()
        .collect();
    assert!(
        ssr.contains(r#"{"same":true,"known":true}"#),
        "server.environments.ssr is the ssr environment:\n{ssr}"
    );
}

// A plugin whose applyToEnvironment throws fails the dev server's start, as it
// rejects Vite's createServer (resolveEnvironmentPlugins): never a server
// whose ssr environment silently runs without the app's plugins.
#[tokio::test(flavor = "multi_thread")]
async fn a_throwing_apply_to_environment_fails_the_boot() {
    let plugins = r#"export default [{
  name: "refuses-ssr",
  applyToEnvironment(environment) {
    if (environment.name === "ssr") throw new Error("refuses-ssr-environment");
    return true;
  },
  transform(code) { return code; },
}];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-refuses","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", plugins),
    ]);
    let built = app.dev().build_app().await;
    assert!(built.is_err(), "the boot must fail");
}

// The ssr environment's module graph is its own: invalidating it (Vite: the
// ssr environment's moduleGraph) never reaches the browser.
#[tokio::test(flavor = "multi_thread")]
async fn an_ssr_graph_invalidation_never_reloads_the_browser() {
    let plugins = r#"export default [{
  name: "invalidates-ssr",
  applyToEnvironment: (environment) => environment.name === "ssr",
  transform(code, id) {
    if (!id.endsWith("/src/main.js")) return;
    this.environment.moduleGraph.invalidateAll();
    const mod = this.environment.moduleGraph.getModuleById(id);
    if (mod) this.environment.moduleGraph.invalidateModule(mod);
    return code;
  },
}];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-ssr-graph","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", plugins),
    ]);
    let server = app.serve().await;
    let mut frames = server.built.reload_tx.subscribe();
    let main = app.root.join("src/main.js");
    server
        .text(&format!("/@ssr-module?id={}", main.display()))
        .await;
    let reload = regex::Regex::new(r#""type":"(update|full-reload)""#).unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match frames.recv().await {
                Ok(f) if reload.is_match(&f) => return f,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("ws broadcast closed: {e}"),
            }
        }
    })
    .await;
    assert!(got.is_err(), "the browser got {:?}", got.ok());
}

// Vite closes every environment's container on server close: buildEnd by
// buildStart's rule (in ssr only for a plugin that starts per environment),
// closeBundle for every plugin.
#[tokio::test(flavor = "multi_thread")]
async fn close_ends_every_environment() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
export default [
  {{
    name: "per-environment",
    perEnvironmentStartEndDuringDev: true,
    buildEnd() {{ log("per-environment buildEnd " + this.environment.name); }},
  }},
  {{
    name: "plain",
    buildEnd() {{ log("plain buildEnd " + this.environment.name); }},
    closeBundle() {{ log("plain closeBundle " + this.environment.name); }},
  }},
];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-close","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    let host = server.built.plugin_host.clone().expect("a plugin host");
    oj_server::close_plugins(&host).await;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "per-environment buildEnd client",
            "per-environment buildEnd ssr",
            "plain buildEnd client",
            "plain closeBundle client",
            "plain closeBundle ssr",
        ]
    );
}

// Vite closes the environments at once (allSettled), and each environment's
// closeBundle gets what its buildEnd threw.
#[tokio::test(flavor = "multi_thread")]
async fn close_runs_the_environments_at_once_and_hands_close_bundle_the_error() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
let ssrClosing;
const ssrClosed = new Promise((resolve) => (ssrClosing = resolve));
export default [{{
  name: "fails-to-end",
  buildEnd() {{ throw new Error("end-failed"); }},
  async closeBundle(error) {{
    log("closeBundle " + this.environment.name + " " + (error ? error.message : "none"));
    // The client's close waits on the ssr one's: closing one after the
    // other never gets here.
    if (this.environment.name === "ssr") ssrClosing();
    else await ssrClosed;
  }},
}}];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-close-error","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    let host = server.built.plugin_host.clone().expect("a plugin host");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        oj_server::close_plugins(&host),
    )
    .await
    .expect("the environments close at once");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    // oj names the plugin in the message, as its other hook errors.
    assert_eq!(
        lines,
        [
            "closeBundle client [plugin:fails-to-end] end-failed",
            "closeBundle ssr none"
        ]
    );
}

// Vite's hotUpdate gets the environment's own graph's modules for the file:
// a file only the ssr environment loaded is in its modules and not the
// client's.
#[tokio::test(flavor = "multi_thread")]
async fn each_environments_hot_update_gets_its_own_graphs_modules() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
const base = (id) => id.split("/").pop();
export default [{{
  name: "modules",
  hotUpdate({{ file, modules }}) {{
    if (file.endsWith("/src/server.js")) {{
      log(this.environment.name + " [" + modules.map((m) => base(m.id ?? m.url)).join(",") + "]");
    }}
  }},
}}];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-environments-hot-modules","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("src/server.js", "export const s = 1;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    let file = app.root.join("src/server.js");
    server
        .text(&format!("/@ssr-module?id={}", file.display()))
        .await;
    app.write(&[("src/server.js", "export const s = 2;\n")]);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let text = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().count() >= 2 {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hooks never both ran: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, ["client []", "ssr [server.js]"]);
}

// A runner-backed ssr environment (a plugin's own dev.createEnvironment, the
// Cloudflare shape) is the app's one ssr environment: a save runs each
// environment's watchChange and hotUpdate once, as Vite's single pass does.
#[tokio::test(flavor = "multi_thread")]
async fn a_runner_backed_ssr_environment_gets_one_pass_per_save() {
    crate::require_fixture!("vite");
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let config = format!(
        r#"import fs from "node:fs";
import {{ DevEnvironment }} from "vite";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
const main = (id) => id.endsWith("/src/main.js");
export default {{
  plugins: [{{
    name: "runner-ssr",
    perEnvironmentWatchChangeDuringDev: true,
    config: () => ({{
      environments: {{
        ssr: {{ dev: {{ createEnvironment: (name, config) => new DevEnvironment(name, config, {{ hot: true }}) }} }},
      }},
    }}),
    configureServer(server) {{
      server.middlewares.use((req, res, next) => next());
    }},
    watchChange(id) {{
      if (main(id)) log("watchChange " + this.environment.name);
    }},
    hotUpdate({{ file }}) {{
      if (main(file)) log("hotUpdate " + this.environment.name);
    }},
  }}],
}};
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runner-ssr-one-pass","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", &config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    assert!(
        server.built.plugin_serve.runner_environments(),
        "the runner-backed ssr environment never came up"
    );
    assert!(server.text("/src/main.js").await.contains("__ok"));
    app.write(&[("src/main.js", "window.__ok = 2;\n")]);
    // Start's dev server hands the runner-backed environments each saved
    // batch, beside the dev server's own watcher.
    let port = server.built.plugin_serve.mw_port().unwrap();
    let main = app.root.join("src/main.js").display().to_string();
    oj_server::notify_plugin_mw_invalidate(port, &[(main, "update")]).await;
    let expected = [
        "hotUpdate client",
        "hotUpdate ssr",
        "watchChange client",
        "watchChange ssr",
    ];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if expected.iter().all(|e| text.lines().any(|l| l == *e)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hooks never all ran: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, expected, "each environment's hooks once per save");
}

// Vite resolves the environments after configResolved (config.ts), so what a
// configResolved hook changes reaches the runner-backed environments' config.
#[tokio::test(flavor = "multi_thread")]
async fn configresolved_edits_reach_the_runner_environments() {
    crate::require_fixture!("vite");
    let config = r#"import { DevEnvironment } from "vite";
let seen = null;
export default {
  plugins: [{
    name: "runner-env",
    config: () => ({
      environments: {
        worker: {
          consumer: "server",
          dev: { createEnvironment: (name, config) => new DevEnvironment(name, config, { hot: true }) },
        },
      },
    }),
    configResolved(config) {
      config.environments.worker.optimizeDeps.include ??= [];
      config.environments.worker.optimizeDeps.include.push("from-config-resolved");
      config.server.fs.allow.push("/from-config-resolved");
    },
    configureServer(server) {
      const worker = server.environments.worker.config;
      seen = [...(worker.optimizeDeps.include ?? []), ...worker.server.fs.allow];
      server.middlewares.use((req, res, next) => {
        if (req.url !== "/__seen") return next();
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(seen));
      });
    },
  }],
};
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"config-resolved-runner","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let seen: Vec<String> = serde_json::from_str(&server.text("/__seen").await).unwrap();
    for want in ["from-config-resolved", "/from-config-resolved"] {
        assert!(
            seen.iter().any(|s| s == want),
            "{want} missing from the worker environment's config: {seen:?}"
        );
    }
}

// A runner environment's config lists the app's plugins, as the app's own Vite
// resolve does: never the ones oj's recording resolve adds for itself. Vite
// hashes these names into the environment's deps optimizer configHash, so one
// left in made the dev server throw the preseed child's deps cache away and
// optimize again in process.
#[tokio::test(flavor = "multi_thread")]
async fn a_runner_environment_sees_only_the_apps_plugins() {
    crate::require_fixture!("vite");
    let config = r#"import { DevEnvironment } from "vite";
let seen = null;
const runner = {
  name: "runner-env",
  config() {
    return {
      environments: {
        worker: {
          consumer: "server",
          dev: {
            createEnvironment(name, config) {
              const environment = new DevEnvironment(name, config, { hot: true });
              // What the optimizer's configHash reads.
              seen = environment.config.plugins.map((p) => p.name);
              return environment;
            },
          },
        },
      },
    };
  },
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      if (req.url !== "/__plugins") return next();
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(seen));
    });
  },
};
export default { plugins: [runner] };
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runner-env-plugins","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let seen: Vec<String> = serde_json::from_str(&server.text("/__plugins").await).unwrap();
    assert!(
        seen.iter().any(|p| p == "runner-env"),
        "the worker environment's config lists the app's plugins: {seen:?}"
    );
    assert!(
        !seen.iter().any(|p| p == "oj:config-hooks-done"),
        "oj's marker is in the worker environment's plugins: {seen:?}"
    );
}
