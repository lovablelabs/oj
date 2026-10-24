// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, files_under, scratch, write_files};

/// A plugin `transform` runs on CSS/preprocessor sources before oj compiles
/// them, as in Vite where vite:css's transform sits after user transforms in
/// the pipeline, so directive transformers (UnoCSS `@apply` and the like)
/// resolve. A plugin rewrites a marker inside nested SCSS; the rewrite must
/// reach the output CSS and the surrounding SCSS must still compile.
#[tokio::test]
async fn a_plugin_transform_runs_on_scss_before_it_is_compiled() {
    let root = scratch("css-plugin-transform");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"csstx-app","version":"1.0.0"}"#),
            (
                "src/styles.scss",
                ".card { __BRAND__ .inner { font-weight: bold; } }\n",
            ),
            (
                "src/entry.js",
                "import \"./styles.scss\";\nconsole.log(\"app\");\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"export default [{
  name: "css-directive",
  transform(code, id) {
    if (id.endsWith(".scss") || id.endsWith(".css")) {
      return { code: code.replace("__BRAND__", "color: rebeccapurple;"), map: null };
    }
    return null;
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let css = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".css"))
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n");
    // lightningcss minifies `rebeccapurple` to `#639`.
    assert!(
        css.contains("rebeccapurple") || css.contains("#639"),
        "the plugin transform ran on the CSS source before compilation:\n{css}"
    );
    assert!(
        !css.contains("__BRAND__"),
        "the source marker was resolved, not left raw:\n{css}"
    );
    assert!(
        css.contains(".inner"),
        "the SCSS around the injected rule still compiled:\n{css}"
    );
}

/// A stylesheet goes through the plugin transforms once, as in Vite. The
/// plugin returns a stylesheet of its own rather than editing its input, as
/// `@tailwindcss/vite`'s `generate:build` does. A second pass over the JS
/// module oj bundles in the stylesheet's place handed that CSS to rolldown as
/// JavaScript, and the build failed with a parse error.
#[tokio::test]
async fn a_css_transform_that_returns_its_own_stylesheet_runs_once() {
    let root = scratch("css-plugin-transform-once");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"csstx-once-app","version":"1.0.0"}"#),
            ("src/styles.css", ".from-source { color: red; }\n"),
            ("src/card.css", ".from-source { color: red; }\n"),
            (
                "src/entry.js",
                "import \"./styles.css\";\nimport card from \"./card.css?inline\";\nconsole.log(card);\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"import { appendFileSync } from "node:fs";
export default [{
  name: "css-generator",
  enforce: "pre",
  transform(code, id) {
    if (!/\.css(\?.*)?$/.test(id)) return null;
    appendFileSync(new URL("./transform-calls.txt", import.meta.url), id.slice(id.lastIndexOf("/") + 1) + "\n");
    return { code: ".generated{color:rebeccapurple}\n", map: null };
  },
}];
"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the app builds with a transform that returns CSS");

    let calls = std::fs::read_to_string(root.join("transform-calls.txt")).unwrap_or_default();
    let mut calls: Vec<&str> = calls.lines().collect();
    calls.sort_unstable();
    assert_eq!(
        calls,
        ["card.css?inline", "styles.css"],
        "each stylesheet went through the transform once"
    );
    let assets = files_under(&root.join("dist/assets"));
    let joined = |ext: &str| {
        assets
            .iter()
            .filter(|(rel, _)| rel.ends_with(ext))
            .map(|(_, code)| code.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let css = joined(".css");
    assert!(
        css.contains(".generated") && !css.contains(".from-source"),
        "the plugin's stylesheet shipped as CSS:\n{css}"
    );
    let js = joined(".js");
    assert!(
        js.contains(".generated"),
        "the `?inline` import is the plugin's stylesheet as text:\n{js}"
    );
}
