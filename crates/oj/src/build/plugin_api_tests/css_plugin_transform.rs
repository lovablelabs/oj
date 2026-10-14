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
