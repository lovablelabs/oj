// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `this.resolve` inside a Start bundle takes Vite's order (its
//! pluginContainer resolve): the hooks before Vite's resolver, the resolver,
//! then the normal ones. An `enforce: "pre"` plugin resolving a relative
//! import gets the file, never a normal plugin's answer for that id, and an id
//! the resolver cannot find still reaches the normal plugin that serves it.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

const PLUGINS: &str = r#"function resolveOrderPlugins() {
  return [
    {
      name: "fixture-pre-delegates",
      enforce: "pre",
      async resolveId(source, importer) {
        if (source !== "./order-probe" && source !== "order-fallback") return;
        return this.resolve(source, importer, { skipSelf: true });
      },
    },
    {
      name: "fixture-normal-claims",
      resolveId(id) {
        if (id === "./order-probe") return "\0order-hijacked";
        if (id === "order-fallback") return "\0order-fallback";
      },
      load(id) {
        if (id === "\0order-hijacked") return 'export const probe = "order-hijacked";';
        if (id === "\0order-fallback") return 'export const fallback = "order-fallback-ok";';
      },
    },
  ];
}
"#;

fn write_app(root: &Path) {
    std::fs::write(
        root.join("src/routes/order-probe.ts"),
        "export const probe = \"order-file-ok\";\n",
    )
    .unwrap();
    let about = root.join("src/routes/about.tsx");
    let source = std::fs::read_to_string(&about).unwrap();
    let import = "import { rootRoute } from \"./__root\";";
    let heading = "<h1 className=\"fixture-heading\">about-page-marker</h1>";
    assert!(
        source.contains(import) && source.contains(heading),
        "{source}"
    );
    let source = source
        .replacen(
            import,
            &format!(
                "{import}\nimport {{ probe }} from \"./order-probe\";\nimport {{ fallback }} from \"order-fallback\";"
            ),
            1,
        )
        .replacen(heading, &format!("{heading}\n      <p id=\"order\">{{probe}}|{{fallback}}</p>"), 1);
    std::fs::write(&about, source).unwrap();
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(source.contains("  plugins: [\n"), "{source}");
    let source = source
        .replacen(
            "  plugins: [\n",
            "  plugins: [\n    ...resolveOrderPlugins(),\n",
            1,
        )
        .replacen(
            "export default defineConfig(",
            &format!("{PLUGINS}\nexport default defineConfig("),
            1,
        );
    std::fs::write(&config, source).unwrap();
}

fn client_js(root: &Path) -> String {
    let mut out = String::new();
    for entry in std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
    {
        if entry.path().extension().is_some_and(|e| e == "js") {
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    out
}

#[test]
fn a_pre_plugins_resolve_takes_vites_order_in_a_start_bundle() {
    let Some(app) = start_app::app("resolve-order") else {
        return;
    };
    write_app(&app.0);
    start_app::build(&app.0);
    let js = client_js(&app.0);
    assert!(
        js.contains("order-file-ok"),
        "the resolver's file is not in the client"
    );
    assert!(
        !js.contains("order-hijacked"),
        "a normal plugin answered an id the resolver resolves"
    );
    assert!(
        js.contains("order-fallback-ok"),
        "the id the resolver cannot find never reached the normal plugin"
    );
}
