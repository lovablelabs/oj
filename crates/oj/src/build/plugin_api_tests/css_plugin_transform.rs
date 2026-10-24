// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, files_under, scratch, write_files};

/// An `enforce: "pre"` plugin `transform` runs on CSS/preprocessor sources
/// before oj compiles them, as it runs before vite:css in Vite, so directive
/// transformers (UnoCSS's, which are `pre`) resolve. A plugin rewrites a
/// marker inside nested SCSS; the rewrite must reach the output CSS and the
/// surrounding SCSS must still compile.
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
  enforce: "pre",
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

/// Each plugin sees a stylesheet once, in its slot of Vite's order around
/// vite:css and vite:css-post: "pre" plugins the source, "normal" ones the
/// compiled CSS, "post" ones the JS module that stands in for it. A `?url`
/// import is JS to every plugin, and its sheet compiles as `?transform-only`,
/// an empty module to "post" plugins. A hook's own `order` moves it between
/// slots, `?raw` is JS throughout, and a CSS module's "post" plugins see its
/// class map. The expected lines are what Vite 8.3.5 records for this app.
#[tokio::test]
async fn stylesheet_transforms_run_in_vites_slots() {
    let root = scratch("css-plugin-transform-slots");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"csstx-slots","version":"1.0.0"}"#),
            ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
            ("src/b.scss", "$c: blue;\n.b { color: $c; }\n"),
            ("src/u.scss", "$c: green;\n.u { color: $c; }\n"),
            ("src/r.scss", "$c: navy;\n.r { color: $c; }\n"),
            ("src/m.module.css", ".m { color: teal; }\n"),
            (
                "src/entry.js",
                "import \"./a.scss\";\nimport b from \"./b.scss?inline\";\nimport u from \"./u.scss?url\";\nimport r from \"./r.scss?raw\";\nimport m from \"./m.module.css\";\nconsole.log(b, u, r, m);\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"import { appendFileSync } from "node:fs";
const record = (name) => function (code, id) {
  if (!/\.(scss|css)(\?|$)/.test(id)) return null;
  const kind = code.includes("$c") ? "source" : /export (default|const)|^$/.test(code) || code.startsWith("import") ? "js" : "css";
  appendFileSync(new URL("./seen.txt", import.meta.url), `${name} ${id.slice(id.lastIndexOf("/") + 1)} ${kind}\n`);
  return null;
};
const spy = (name, enforce) => ({ name, enforce, transform: record(name) });
const ordered = (name, enforce, order) => ({ name, enforce, transform: { order, handler: record(name) } });
export default [
  spy("post", "post"),
  spy("normal"),
  spy("pre", "pre"),
  ordered("normal-order-pre", undefined, "pre"),
  ordered("pre-order-post", "pre", "post"),
  ordered("post-order-pre", "post", "pre"),
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let seen = std::fs::read_to_string(root.join("seen.txt")).expect("the plugins ran");
    let mut seen: Vec<&str> = seen.lines().collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "normal a.scss css",
            "normal b.scss?inline css",
            "normal m.module.css css",
            "normal r.scss?raw source",
            "normal u.scss?transform-only css",
            "normal u.scss?url js",
            "normal-order-pre a.scss source",
            "normal-order-pre b.scss?inline source",
            "normal-order-pre m.module.css css",
            "normal-order-pre r.scss?raw source",
            "normal-order-pre u.scss?transform-only source",
            "normal-order-pre u.scss?url js",
            "post a.scss js",
            "post b.scss?inline js",
            "post m.module.css js",
            "post r.scss?raw source",
            "post u.scss?transform-only js",
            "post u.scss?url js",
            "post-order-pre a.scss source",
            "post-order-pre b.scss?inline source",
            "post-order-pre m.module.css css",
            "post-order-pre r.scss?raw source",
            "post-order-pre u.scss?transform-only source",
            "post-order-pre u.scss?url js",
            "pre a.scss source",
            "pre b.scss?inline source",
            "pre m.module.css css",
            "pre r.scss?raw source",
            "pre u.scss?transform-only source",
            "pre u.scss?url js",
            "pre-order-post a.scss js",
            "pre-order-post b.scss?inline js",
            "pre-order-post m.module.css js",
            "pre-order-post r.scss?raw source",
            "pre-order-post u.scss?transform-only js",
            "pre-order-post u.scss?url js",
        ]
    );
}

/// A "pre" transform that returns a stylesheet of its own rather than editing
/// its input, as `@tailwindcss/vite`'s `generate:build` does, ships that
/// stylesheet for a plain, an `?inline` and a `?url` import. Run again over
/// the JS module standing in for the sheet, it made that module CSS, which
/// failed the build with a parse error.
#[tokio::test]
async fn a_pre_transform_that_returns_its_own_stylesheet_ships_it() {
    let root = scratch("css-plugin-transform-generate");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"csstx-generate","version":"1.0.0"}"#),
            ("src/styles.css", ".from-source { color: red; }\n"),
            ("src/card.css", ".from-source { color: red; }\n"),
            ("src/link.css", ".from-source { color: red; }\n"),
            (
                "src/entry.js",
                "import \"./styles.css\";\nimport card from \"./card.css?inline\";\nimport link from \"./link.css?url\";\nconsole.log(card, link);\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"export default [{
  name: "css-generator",
  enforce: "pre",
  transform: {
    filter: { id: { include: [/\.css(?:\?.*)?$/], exclude: [/[?&](?:worker|sharedworker|raw|url)\b/] } },
    handler: () => ({ code: ".generated{color:rebeccapurple}\n", map: null }),
  },
}];
"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the app builds with a transform that returns CSS");

    let assets = files_under(&root.join("dist/assets"));
    let sheets: Vec<&str> = assets
        .iter()
        .filter(|(rel, _)| rel.ends_with(".css"))
        .map(|(_, code)| code.as_str())
        .collect();
    assert_eq!(sheets.len(), 2, "the linked sheet and the `?url` one");
    for css in &sheets {
        assert!(
            css.contains(".generated") && !css.contains(".from-source"),
            "the plugin's stylesheet shipped as CSS:\n{css}"
        );
    }
    let js: String = assets
        .iter()
        .filter(|(rel, _)| rel.ends_with(".js"))
        .map(|(_, code)| code.as_str())
        .collect();
    assert!(
        js.contains(".generated"),
        "the `?inline` import is the plugin's stylesheet as text:\n{js}"
    );
}
