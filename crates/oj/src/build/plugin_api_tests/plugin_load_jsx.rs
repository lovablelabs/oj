// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, js_under, scratch, write_files};

/// A plugin-served virtual module whose id ends in `.jsx` is parsed as JSX,
/// not plain JS: like Rolldown, the module type follows the id's extension
/// when `load` returns bare code. Regression for unplugin-icons'
/// `~icons/*.jsx`, where oj hardcoded module type Js for every plugin load and
/// oxc rejected the markup.
#[tokio::test]
async fn a_plugin_loaded_jsx_virtual_module_is_transformed_as_jsx() {
    let root = scratch("plugin-load-jsx");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"lj-app","version":"1.0.0"}"#),
            (
                "src/entry.js",
                "import { Icon } from \"virtual:icon.jsx\";\nwindow.__icon = Icon();\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r##"export default [{
  name: "virtual-jsx",
  resolveId(id) {
    if (id === "virtual:icon.jsx") return "\0virtual:icon.jsx";
    if (id === "react/jsx-runtime") return "\0jsx-runtime";
    return null;
  },
  load(id) {
    if (id === "\0virtual:icon.jsx") return "export const Icon = () => <svg width=\"1em\" data-mark=\"icon-ok\" />;";
    if (id === "\0jsx-runtime") return "export const jsx = (t, p) => ({ t, p }); export const jsxs = jsx; export const Fragment = 'F';";
    return null;
  },
}];
"##,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the app builds (the .jsx virtual module parses as JSX)");

    let js = js_under(&root.join("dist/assets"));
    assert!(
        !js.contains("<svg"),
        "raw JSX must not survive into the bundle:\n{js}"
    );
    assert!(
        js.contains("icon-ok"),
        "the component's props made it through the JSX transform:\n{js}"
    );
}
