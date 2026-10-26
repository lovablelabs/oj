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
