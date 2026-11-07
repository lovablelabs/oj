// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The plugin context exposes `this.parse` (Rollup/Vite parseAst) and
//! `this.meta`. Without them any AST-aware Vite plugin throws ("this.parse is
//! not a function") inside transform. A probe plugin parses its input and
//! reads this.meta; the served module carries the results.

use crate::{require_fixture, App};

const VITE_CONFIG: &str = r#"export default {
      plugins: [{
        name: "ast-probe",
        transform(code, id) {
          if (!id.endsWith("main.js")) return null;
          const ast = this.parse(code);
          const parsedOk = !!ast && ast.type === "Program" && Array.isArray(ast.body);
          const metaOk = !!this.meta && typeof this.meta.rollupVersion === "string";
          return code + "\nexport const __parsedOk = " + parsedOk + ";\nexport const __metaOk = " + metaOk + ";\n";
        },
      }],
    };
"#;

#[tokio::test(flavor = "multi_thread")]
async fn transform_context_has_parse_and_meta() {
    require_fixture!("vite");
    let app = App::new(&[
        ("package.json", r#"{"name":"parseapp","private":true}"#),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        ("src/main.js", "export const v = 1;\n"),
        ("vite.config.mjs", VITE_CONFIG),
    ]);
    // A real vite so the host can resolve parseAst + loadConfigFromFile.
    app.link_fixture("vite");
    let server = app.serve().await;

    let module = server.text("/src/main.js").await;
    assert!(
        module.contains("__parsedOk = true"),
        "this.parse did not return a Program AST:\n{module}"
    );
    assert!(
        module.contains("__metaOk = true"),
        "this.meta.rollupVersion missing:\n{module}"
    );
}
