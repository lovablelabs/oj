// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Where Start's compiler runs among the app's plugins in a Start bundle, as
//! TanStack's `enforce: "pre"` compiler plugin (no hook `order`) sits in Vite:
//! after every `order: "pre"` hook and the "pre" plugins listed before
//! `tanstackStart()`, before the "pre" plugins listed after it and every
//! "normal" one. The ones before it see a server-fn module as written (its
//! handler and the `getRequest` import), the ones after it see it compiled.
//! A `.svg` svgr componentized at load takes the transforms once, as Vite
//! runs them on what a load returned.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

const PLUGINS: &str = r#"function slotPlugins() {
  const slot = (id) => id.split("?")[0].endsWith("/lib/slot.functions.ts");
  const saw = (code, who) => `${who}-saw-${code.includes("getRequest") ? "raw" : "compiled"}`;
  return [
    {
      name: "fixture-slot-pre",
      enforce: "pre",
      transform(code, id) {
        if (slot(id)) return code + `\nexport const preSaw = ${JSON.stringify(saw(code, "pre"))};\n`;
      },
    },
  ];
}
// Listed after tanstackStart(): an unordered "pre" plugin there runs after
// Start's compiler, a hook with `order: "pre"` before it.
function slotAfterPlugins() {
  const slot = (id) => id.split("?")[0].endsWith("/lib/slot.functions.ts");
  const saw = (code, who) => `${who}-saw-${code.includes("getRequest") ? "raw" : "compiled"}`;
  return [
    {
      name: "fixture-slot-late-pre",
      enforce: "pre",
      transform(code, id) {
        if (slot(id)) return code + `\nexport const latePreSaw = ${JSON.stringify(saw(code, "late-pre"))};\n`;
      },
    },
    {
      name: "fixture-slot-late-ordered",
      transform: {
        order: "pre",
        handler(code, id) {
          if (slot(id)) return code + `\nexport const lateOrderedSaw = ${JSON.stringify(saw(code, "late-ordered"))};\n`;
        },
      },
    },
    {
      name: "fixture-slot-normal",
      transform(code, id) {
        if (slot(id)) return code + `\nexport const normalSaw = ${JSON.stringify(saw(code, "normal"))};\n`;
        if (id.split("?")[0].endsWith(".svg")) return code + `\n;(globalThis.__slotSvg ??= []).push("svg-normal-ran");\n`;
      },
    },
  ];
}
"#;

const SLOT_FNS: &str = r#"import { createServerFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";

export const slotFn = createServerFn({ method: "POST" }).handler(async () => {
  return getRequest()?.method ?? "none";
});
"#;

fn write_app(root: &Path) {
    std::fs::write(root.join("src/lib/slot.functions.ts"), SLOT_FNS).unwrap();
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
                "{import}\nimport {{ slotFn, preSaw, normalSaw, latePreSaw, lateOrderedSaw }} from \"../lib/slot.functions\";"
            ),
            1,
        )
        .replacen(
            heading,
            &format!(
                "{heading}\n      <p id=\"slot\" onClick={{() => slotFn()}}>{{preSaw}}|{{normalSaw}}|{{latePreSaw}}|{{lateOrderedSaw}}</p>"
            ),
            1,
        );
    std::fs::write(&about, source).unwrap();
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(source.contains("  plugins: [\n"), "{source}");
    assert!(source.contains("    react(),\n  ],"), "{source}");
    let source = source
        .replacen("  plugins: [\n", "  plugins: [\n    ...slotPlugins(),\n", 1)
        .replacen(
            "    react(),\n  ],",
            "    react(),\n    ...slotAfterPlugins(),\n  ],",
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
fn the_compiler_runs_after_the_pre_transforms_and_before_the_normal_ones() {
    let Some(app) = start_app::app("compile-slot") else {
        return;
    };
    write_app(&app.0);
    start_app::build(&app.0);
    let js = client_js(&app.0);
    assert!(
        js.contains("pre-saw-raw"),
        "the pre plugin did not see the module as written"
    );
    assert!(
        js.contains("normal-saw-compiled"),
        "the normal plugin did not see the compiled module"
    );
    assert!(
        js.contains("late-pre-saw-compiled"),
        "a pre plugin listed after tanstackStart() ran before the compiler"
    );
    assert!(
        js.contains("late-ordered-saw-raw"),
        "an order: \"pre\" hook listed after tanstackStart() ran after the compiler"
    );
    assert!(
        !js.contains("getRequest"),
        "the handler's import reached the client"
    );
}

#[test]
fn an_svg_takes_a_normal_transform_once() {
    let Some(app) = start_app::app("compile-slot-svg") else {
        return;
    };
    write_app(&app.0);
    start_app::build(&app.0);
    let js = client_js(&app.0);
    // The fixture's index route imports two svgr components.
    let runs = js.matches("svg-normal-ran").count();
    assert_eq!(
        runs, 2,
        "a .svg's normal transforms ran {runs} times for two svgs"
    );
}
