// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `@stylexjs/unplugin` in `oj build`, run natively: modules compile with the
//! config's options in production mode, and the collected sheet joins the
//! first stylesheet the build emits (or `assets/stylex.css` with none).

use super::{build_app, files_under, link_fixtures, scratch, write_files};

/// The StyleX packages as an app installs them, minus their code: oj serves
/// its stand-in for the plugin, and `@stylexjs/stylex` is compiled away.
const STYLEX_PACKAGES: &[(&str, &str)] = &[
    (
        "node_modules/@stylexjs/unplugin/package.json",
        r#"{"name":"@stylexjs/unplugin","version":"0.19.0","exports":{".":{"import":"./lib/es/index.mjs"}}}"#,
    ),
    (
        "node_modules/@stylexjs/unplugin/lib/es/index.mjs",
        "throw new Error(\"the real @stylexjs/unplugin ran\");\n",
    ),
    (
        "node_modules/@stylexjs/stylex/package.json",
        r#"{"name":"@stylexjs/stylex","version":"0.19.0","type":"module","main":"index.js"}"#,
    ),
    (
        "node_modules/@stylexjs/stylex/index.js",
        "export const create = () => { throw new Error(\"uncompiled StyleX\"); };\n",
    ),
];

const CONFIG: &str = "import stylex from \"@stylexjs/unplugin\";\n\
export default { plugins: [stylex.vite({ useCSSLayers: true })] };\n";

const MAIN: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
const styles = stylex.create({ title: { color: \"olive\" } });\n\
document.body.className = stylex.props(styles.title).className;\n";

fn app(name: &str, files: &[(&str, &str)]) -> Option<std::path::PathBuf> {
    let root = scratch(name);
    if !link_fixtures(&root, &["vite"]) {
        return None;
    }
    write_files(&root, STYLEX_PACKAGES);
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"sx","version":"1.0.0","type":"module"}"#,
            ),
            ("vite.config.js", CONFIG),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.ts"></script></body></html>"#,
            ),
        ],
    );
    write_files(&root, files);
    Some(root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sheet_joins_the_first_stylesheet() {
    let Some(root) = app(
        "stylex-sheet",
        &[
            ("src/main.ts", &format!("import \"./app.css\";\n{MAIN}")),
            ("src/app.css", "body { margin: 0 }\n"),
        ],
    ) else {
        return;
    };
    build_app(&root).await.expect("the StyleX app builds");
    let files = files_under(&root.join("dist"));
    let css: Vec<_> = files.iter().filter(|(p, _)| p.ends_with(".css")).collect();
    assert_eq!(css.len(), 1, "one stylesheet: {css:?}");
    let sheet = &css[0].1;
    let (app_css, stylex_css) = sheet.split_once("@layer").expect("the StyleX sheet");
    assert!(
        app_css.contains("margin:0"),
        "the app's own rules first:\n{sheet}"
    );
    assert!(stylex_css.contains("color: olive"), "{sheet}");

    let js: String = files
        .iter()
        .filter(|(p, _)| p.ends_with(".js"))
        .map(|(_, c)| c.as_str())
        .collect();
    assert!(!js.contains("stylex.create"), "compiled away");
    assert!(
        !js.contains("main__styles"),
        "production: no debug class names"
    );
    let class = stylex_css
        .split('.')
        .nth(1)
        .and_then(|rest| rest.split([' ', '{']).next())
        .expect("a class rule");
    assert!(
        js.contains(class),
        "the bundle uses the sheet's class {class}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_with_no_stylesheet_gets_the_plugins_fallback_file() {
    let Some(root) = app("stylex-fallback", &[("src/main.ts", MAIN)]) else {
        return;
    };
    build_app(&root).await.expect("the StyleX app builds");
    let sheet = std::fs::read_to_string(root.join("dist/assets/stylex.css"))
        .expect("assets/stylex.css written");
    assert!(sheet.contains("color: olive"), "{sheet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dependencies_on_stylex_compile_in_the_bundle() {
    let deps = "import { esmClass } from \"sx-esm\";\nimport { cjsClass } from \"sx-cjs\";\n\
                document.body.className = esmClass + cjsClass;\n";
    let Some(root) = app("stylex-deps", &[("src/main.ts", deps)]) else {
        return;
    };
    write_files(
        &root,
        &[
            (
                "node_modules/sx-esm/package.json",
                r#"{"name":"sx-esm","version":"1.0.0","type":"module","main":"index.js"}"#,
            ),
            (
                "node_modules/sx-esm/index.js",
                "import * as stylex from \"@stylexjs/stylex\";\n\
                 export const esmClass = stylex.props(stylex.create({ c: { color: \"navy\" } }).c).className;\n",
            ),
            (
                "node_modules/sx-cjs/package.json",
                r#"{"name":"sx-cjs","version":"1.0.0","main":"index.js"}"#,
            ),
            (
                "node_modules/sx-cjs/index.js",
                "const stylex = require(\"@stylexjs/stylex\");\n\
                 exports.cjsClass = stylex.props(stylex.create({ c: { color: \"maroon\" } }).c).className;\n",
            ),
        ],
    );
    build_app(&root).await.expect("the StyleX app builds");
    let sheet = std::fs::read_to_string(root.join("dist/assets/stylex.css")).unwrap();
    assert!(
        sheet.contains("color: navy") && sheet.contains("color: maroon"),
        "{sheet}"
    );
    let js: String = files_under(&root.join("dist"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".js"))
        .map(|(_, c)| c)
        .collect();
    assert!(!js.contains("stylex.create"), "dependencies compiled");
}

const LIB_CONFIG: &str = "import stylex from \"@stylexjs/unplugin\";\n\
export default {\n\
  plugins: [stylex.vite({ useCSSLayers: true })],\n\
  build: { lib: { entry: \"src/index.ts\", formats: [\"es\", \"cjs\"] } },\n\
};\n";

const LIB: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
const styles = stylex.create({ button: { color: \"olive\" } });\n\
export const buttonClass = stylex.props(styles.button).className;\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_library_build_appends_the_sheet_to_its_stylesheet() {
    let Some(root) = app(
        "stylex-lib",
        &[
            ("vite.config.js", LIB_CONFIG),
            ("src/index.ts", &format!("import \"./lib.css\";\n{LIB}")),
            ("src/lib.css", ".lib { margin: 0 }\n"),
        ],
    ) else {
        return;
    };
    build_app(&root).await.expect("the StyleX library builds");
    // The package is named `sx`: Vite names the library's files after it.
    let sheet = std::fs::read_to_string(root.join("dist/sx.css")).expect("dist/sx.css");
    let (lib_css, stylex_css) = sheet.split_once("@layer").expect("the StyleX sheet");
    assert!(
        lib_css.contains(".lib"),
        "the library's own rules first:\n{sheet}"
    );
    assert!(stylex_css.contains("color: olive"), "{sheet}");
    let class = stylex_css
        .split('.')
        .nth(1)
        .and_then(|rest| rest.split([' ', '{']).next())
        .expect("a class rule");
    for out in ["dist/sx.js", "dist/sx.cjs"] {
        let js = std::fs::read_to_string(root.join(out)).unwrap();
        assert!(!js.contains("stylex.create"), "{out} compiled:\n{js}");
        assert!(js.contains(class), "{out} uses {class}:\n{js}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_library_without_a_stylesheet_gets_the_fallback_file() {
    let Some(root) = app(
        "stylex-lib-fallback",
        &[("vite.config.js", LIB_CONFIG), ("src/index.ts", LIB)],
    ) else {
        return;
    };
    build_app(&root).await.expect("the StyleX library builds");
    let sheet = std::fs::read_to_string(root.join("dist/assets/stylex.css"))
        .expect("assets/stylex.css written");
    assert!(sheet.contains("color: olive"), "{sheet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sheet_joins_the_entrys_stylesheet_not_a_lazy_ones() {
    let Some(root) = app(
        "stylex-split",
        &[
            (
                "src/main.ts",
                &format!("import \"./app.css\";\nimport(\"./lazy\");\n{MAIN}"),
            ),
            ("src/app.css", ".from-entry { margin: 0 }\n"),
            (
                "src/lazy.ts",
                "import \"./lazy.css\";\nexport const lazy = 1;\n",
            ),
            ("src/lazy.css", ".from-lazy { padding: 0 }\n"),
        ],
    ) else {
        return;
    };
    build_app(&root).await.expect("the StyleX app builds");
    let files = files_under(&root.join("dist"));
    let sheets: Vec<&String> = files
        .iter()
        .filter(|(p, _)| p.ends_with(".css"))
        .map(|(_, c)| c)
        .collect();
    assert_eq!(sheets.len(), 2, "{sheets:?}");
    let with_stylex: Vec<&&String> = sheets
        .iter()
        .filter(|c| c.contains("color: olive"))
        .collect();
    assert_eq!(with_stylex.len(), 1, "{sheets:?}");
    assert!(
        with_stylex[0].contains("from-entry"),
        "the entry's sheet: {sheets:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ssr_build_compiles_and_writes_the_fallback_sheet() {
    let Some(root) = app(
        "stylex-ssr",
        &[(
            "src/entry-server.ts",
            "import * as stylex from \"@stylexjs/stylex\";\n\
             const styles = stylex.create({ title: { color: \"olive\" } });\n\
             export const render = () => `<h1 class=\"${stylex.props(styles.title).className}\">x</h1>`;\n",
        )],
    ) else {
        return;
    };
    let cli = super::CliOptions {
        ssr: Some("src/entry-server.ts".into()),
        ..Default::default()
    };
    super::build(root.clone(), None, &tokio::runtime::Handle::current(), cli)
        .await
        .expect("the StyleX SSR build");
    let js: String = files_under(&root.join("dist"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".js") || p.ends_with(".mjs"))
        .map(|(_, c)| c)
        .collect();
    assert!(!js.contains("stylex.create"), "compiled: {js}");
    let sheet =
        std::fs::read_to_string(root.join("dist/assets/stylex.css")).expect("the fallback sheet");
    assert!(sheet.contains("color: olive"), "{sheet}");
}
