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

// Vite dispatches watchChange to every environment's container and hotUpdate
// to each environment over its own plugins, the ssr environment existing from
// boot: an ssr-only plugin's hooks fire on an edit made before any SSR request,
// and a client-only plugin's watchChange fires once per change.
#[tokio::test(flavor = "multi_thread")]
async fn an_ssr_only_plugin_gets_watch_and_hot_updates_from_boot() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({literal}, line + "\n");
export default [
  {{
    name: "ssr-only",
    applyToEnvironment: (environment) => environment.name === "ssr",
    watchChange(id) {{
      if (id.endsWith("/src/main.js")) log("ssr-only watchChange " + this.environment.name);
    }},
    hotUpdate({{ file }}) {{
      if (file.endsWith("/src/main.js")) log("ssr-only hotUpdate " + this.environment.name);
    }},
  }},
  {{
    name: "client-only",
    applyToEnvironment: (environment) => environment.name === "client",
    watchChange(id) {{
      if (id.endsWith("/src/main.js")) log("client-only watchChange " + this.environment.name);
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
        "client-only watchChange client",
        "ssr-only hotUpdate ssr",
        "ssr-only watchChange ssr",
    ];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let lines = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines.sort();
        if expected.iter().all(|e| lines.iter().any(|l| l == e)) {
            break lines;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ssr environment's hooks never ran: {lines:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    // Any duplicate would have landed with the first dispatch.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut lines_now: Vec<&str> = text.lines().collect();
    lines_now.sort();
    assert_eq!(
        lines_now, expected,
        "each hook once, in its own environment (was {lines:?})"
    );
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
