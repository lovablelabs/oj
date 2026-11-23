//! oj runs `@stylexjs/unplugin` natively: the options the config passes to
//! `stylex.vite(...)` drive the compile, every served module's rules feed the
//! plugin's `/virtual:stylex.css`, and a changed sheet sends its update event.

use std::time::Duration;

use crate::{require_fixture, App};

const PAGE: &str = "<!doctype html><html><head><title>t</title></head><body>\
<script type=\"module\" src=\"/src/main.ts\"></script></body></html>";

const TOKENS: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
export const colors = stylex.defineVars({ accent: \"rebeccapurple\" });\n";

fn main_ts(color: &str) -> String {
    format!(
        "import * as stylex from \"@stylexjs/stylex\";\n\
         import {{ colors }} from \"./tokens.stylex\";\n\
         const styles = stylex.create({{ title: {{ color: \"{color}\", borderColor: colors.accent }} }});\n\
         export const title = stylex.props(styles.title).className;\n"
    )
}

fn app(options: &str) -> App {
    let config = format!(
        "import stylex from \"@stylexjs/unplugin\";\n\
         export default {{ plugins: [stylex.vite({options})] }};\n"
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"sx","version":"1.0.0","type":"module"}"#,
        ),
        ("vite.config.js", &config),
        ("index.html", PAGE),
        ("src/tokens.stylex.ts", TOKENS),
        ("src/main.ts", &main_ts("red")),
    ]);
    app.link_fixture("vite");
    app.write(STYLEX_PACKAGES);
    app
}

/// The StyleX packages as an app installs them, minus their code: the
/// plugin's modules throw if anything runs them (oj serves its stand-in), and
/// `@stylexjs/stylex` is only ever compiled away.
pub(crate) const STYLEX_PACKAGES: &[(&str, &str)] = &[
    (
        "node_modules/@stylexjs/unplugin/package.json",
        r#"{"name":"@stylexjs/unplugin","version":"0.19.0","exports":{".":{"import":"./lib/es/index.mjs","require":"./lib/index.js"},"./vite":{"import":"./lib/es/vite.mjs","require":"./lib/vite.js"}}}"#,
    ),
    (
        "node_modules/@stylexjs/unplugin/lib/es/index.mjs",
        "throw new Error(\"the real @stylexjs/unplugin ran\");\n",
    ),
    (
        "node_modules/@stylexjs/unplugin/lib/es/vite.mjs",
        "throw new Error(\"the real @stylexjs/unplugin ran\");\n",
    ),
    (
        "node_modules/@stylexjs/unplugin/lib/index.js",
        "throw new Error(\"the real @stylexjs/unplugin ran\");\n",
    ),
    (
        "node_modules/@stylexjs/unplugin/lib/vite.js",
        "throw new Error(\"the real @stylexjs/unplugin ran\");\n",
    ),
    (
        "node_modules/@stylexjs/stylex/package.json",
        r#"{"name":"@stylexjs/stylex","version":"0.19.0","type":"module","main":"index.js"}"#,
    ),
    (
        "node_modules/@stylexjs/stylex/index.js",
        "export const create = () => { throw new Error(\"uncompiled StyleX\"); };\nexport const props = create;\nexport const defineVars = create;\n",
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn config_options_drive_the_compile_and_the_sheet() {
    require_fixture!("vite");
    let app = app("{ useCSSLayers: true, classNamePrefix: \"q\" }");
    let server = app.serve().await;

    let html = server.text("/").await;
    assert!(
        html.contains(r#"<script type="module" src="/@id/virtual:stylex:runtime"></script>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<link rel="stylesheet" href="/virtual:stylex.css">"#),
        "{html}"
    );

    let main = server.text("/src/main.ts").await;
    assert!(!main.contains("stylex.create"), "compiled away:\n{main}");
    // `dev` follows NODE_ENV (development): the debug class name leads.
    assert!(
        main.contains("main__styles.title q"),
        "classNamePrefix applied:\n{main}"
    );
    server.text("/src/tokens.stylex.ts").await;

    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("@layer priority1"), "useCSSLayers:\n{css}");
    assert!(css.contains("color: red"), "{css}");
    assert!(
        css.contains("rebeccapurple"),
        "the theme module's rules:\n{css}"
    );

    // The plain runtime script and the hot-wired virtual module.
    let runtime = server.text("/@id/virtual:stylex:runtime").await;
    assert!(runtime.contains("__oj_createHotContext"), "{runtime}");
    assert!(runtime.contains("stylex:css-update"), "{runtime}");
    assert!(server
        .text("/virtual:stylex.js")
        .await
        .contains("POLL_INTERVAL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_replaces_the_modules_rules_and_notifies() {
    require_fixture!("vite");
    let app = app("{}");
    let server = app.serve().await;
    server.text("/src/main.ts").await;
    assert!(server
        .text("/virtual:stylex.css")
        .await
        .contains("color: red"));
    // The boot crawl already compiled the module; listen from here on.
    let mut frames = server.built.reload_tx.subscribe();

    // A file's newest compile replaces its rules, whatever it was before.
    std::thread::sleep(Duration::from_millis(20));
    app.write(&[("src/main.ts", &main_ts("olive"))]);
    let main = server.text("/src/main.ts").await;
    assert!(!main.contains("stylex.create"), "{main}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: olive"), "{css}");
    assert!(!css.contains("color: red"), "the old rule is gone:\n{css}");

    let update = async {
        loop {
            let frame = frames.recv().await.unwrap();
            if frame.contains("stylex:css-update") {
                return frame;
            }
        }
    };
    let frame = tokio::time::timeout(Duration::from_secs(5), update)
        .await
        .expect("the changed sheet sends the update event");
    assert!(frame.contains(r#""type":"custom""#), "{frame}");
}

// StyleX 0.19.2: the plugin's transform clears the rules of a module it
// passes over, so a file that drops its StyleX imports drops its styles,
// whichever environment compiled it.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_drops_its_stylex_imports_leaves_the_sheet() {
    require_fixture!("vite");
    let app = app("{}");
    let server_only = |color: &str| {
        format!(
            "import * as stylex from \"@stylexjs/stylex\";\n\
             export const s = stylex.props(stylex.create({{ x: {{ color: \"{color}\" }} }}).x);\n"
        )
    };
    app.write(&[("src/server-only.ts", &server_only("teal"))]);
    let server = app.serve().await;
    let ssr_module = format!(
        "/@ssr-module?id={}",
        app.root.join("src/server-only.ts").display()
    );
    server.text("/src/main.ts").await;
    server.text(&ssr_module).await;
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        css.contains("color: red") && css.contains("color: teal"),
        "{css}"
    );
    let mut frames = server.built.reload_tx.subscribe();

    std::thread::sleep(Duration::from_millis(20));
    app.write(&[("src/main.ts", "export const title = \"plain\";\n")]);
    let main = server.text("/src/main.ts").await;
    assert!(main.contains("plain"), "{main}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        !css.contains("color: red"),
        "the client module's rule is gone:\n{css}"
    );
    assert!(css.contains("color: teal"), "{css}");
    let update = async {
        loop {
            if frames.recv().await.unwrap().contains("stylex:css-update") {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), update)
        .await
        .expect("the emptied module sends the update event");

    app.write(&[("src/server-only.ts", "export const s = \"plain\";\n")]);
    let module = server.text(&ssr_module).await;
    assert!(module.contains("plain"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        !css.contains("color: teal"),
        "the server module's rule is gone:\n{css}"
    );
}

// StyleX 0.19.2: `importSources` adds to the built-in sources at the
// plugin's gate, so modules on `@stylexjs/stylex` still compile.
#[tokio::test(flavor = "multi_thread")]
async fn custom_import_sources_keep_the_built_in_ones() {
    require_fixture!("vite");
    let app = app("{ importSources: [{ from: \"my-sx\", as: \"css\" }] }");
    app.write(&[
        (
            "node_modules/my-sx/package.json",
            r#"{"name":"my-sx","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/my-sx/index.js",
            "export const css = { props: () => ({ className: \"\" }) };\n",
        ),
        (
            "src/custom.ts",
            "import { css } from \"my-sx\";\n\
             export const c = css.props(css.create({ x: { color: \"teal\" } }).x).className;\n",
        ),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    assert!(
        !main.contains("stylex.create"),
        "the built-in source compiles:\n{main}"
    );
    server.text("/src/tokens.stylex.ts").await;
    let custom = server.text("/src/custom.ts").await;
    assert!(
        !custom.contains("css.create"),
        "the listed source compiles:\n{custom}"
    );
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        css.contains("color: red") && css.contains("rebeccapurple") && css.contains("color: teal"),
        "{css}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn css_only_mode_serves_its_shim_and_no_page_tags() {
    require_fixture!("vite");
    let app = app("{ devMode: \"css-only\" }");
    let server = app.serve().await;
    let html = server.text("/").await;
    assert!(!html.contains("virtual:stylex"), "{html}");
    let shim = server.text("/@id/virtual:stylex:css-only").await;
    assert!(shim.contains("function bust()"), "{shim}");
    // Imported from app code, the shim id rewrites to its served url.
    app.write(&[("src/entry.ts", "import \"virtual:stylex:css-only\";\n")]);
    let entry = server.text("/src/entry.ts").await;
    assert!(
        entry.contains("\"/@id/virtual:stylex:css-only\""),
        "{entry}"
    );
    server.text("/src/main.ts").await;
    assert!(server
        .text("/virtual:stylex.css")
        .await
        .contains("color: red"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_mode_off_does_not_apply_under_serve() {
    require_fixture!("vite");
    let app = app("{ devMode: \"off\" }");
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    assert!(
        main.contains("stylex.create"),
        "the plugin's apply skips serve:\n{main}"
    );
    let css = server.get("/virtual:stylex.css").await;
    assert_ne!(
        css.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/css"),
        "no stylesheet endpoint"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ssr_compiles_feed_the_same_sheet() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[(
        "src/server-only.ts",
        "import * as stylex from \"@stylexjs/stylex\";\n\
         export const s = stylex.props(stylex.create({ x: { color: \"teal\" } }).x);\n",
    )]);
    let server = app.serve().await;
    let id = app.root.join("src/server-only.ts");
    let module = server
        .text(&format!("/@ssr-module?id={}", id.display()))
        .await;
    assert!(!module.contains("stylex.create"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: teal"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_restyles_the_page_on_an_edit() {
    if crate::browser::skip() {
        return;
    }
    require_fixture!("vite");
    let page_main = |color: &str| {
        format!(
            "import * as stylex from \"@stylexjs/stylex\";\n\
             const styles = stylex.create({{ title: {{ color: \"{color}\" }} }});\n\
             document.getElementById(\"t\")!.className = stylex.props(styles.title).className ?? \"\";\n\
             if (import.meta.hot) import.meta.hot.accept();\n"
        )
    };
    let app = app("{}");
    app.write(&[
        (
            "index.html",
            "<!doctype html><html><head><title>t</title></head><body><h1 id=\"t\">x</h1>\
             <script type=\"module\" src=\"/src/main.ts\"></script></body></html>",
        ),
        ("src/main.ts", &page_main("olive")),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page =
        crate::browser::Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    let color =
        |rgb: &str| format!(r#"getComputedStyle(document.getElementById("t")).color === "{rgb}""#);
    page.wait_for(&color("rgb(128, 128, 0)"), Duration::from_secs(20))
        .await;
    page.eval::<bool>("window.__kept = true").await;

    // The module hot-updates; its new rule reaches the page through the
    // runtime's refetch on the update event, with no reload.
    app.write(&[("src/main.ts", &page_main("teal"))]);
    page.wait_for(&color("rgb(0, 128, 128)"), Duration::from_secs(20))
        .await;
    assert!(
        page.eval::<bool>("window.__kept === true").await,
        "the page reloaded"
    );
}

/// Packages that depend on `@stylexjs/stylex`, one per module format: the
/// plugin keeps them out of the pre-bundle so its transform reaches them.
pub(crate) const STYLEX_DEPS: &[(&str, &str)] = &[
    (
        "node_modules/sx-esm/package.json",
        r#"{"name":"sx-esm","version":"1.0.0","type":"module","main":"index.js","dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
    ),
    (
        "node_modules/sx-esm/index.js",
        "import * as stylex from \"@stylexjs/stylex\";\n\
         const s = stylex.create({ c: { color: \"navy\" } });\n\
         export const esmClass = stylex.props(s.c).className;\n",
    ),
    (
        "node_modules/sx-cjs/package.json",
        r#"{"name":"sx-cjs","version":"1.0.0","main":"index.js","dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
    ),
    (
        "node_modules/sx-cjs/index.js",
        "const stylex = require(\"@stylexjs/stylex\");\n\
         const s = stylex.create({ c: { color: \"maroon\" } });\n\
         exports.cjsClass = stylex.props(s.c).className;\n",
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn dependencies_on_stylex_compile_like_app_modules() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(STYLEX_DEPS);
    app.write(&[
        (
            "package.json",
            r#"{"name":"sx","version":"1.0.0","type":"module","dependencies":{"sx-esm":"1.0.0","sx-cjs":"1.0.0"}}"#,
        ),
        (
            "src/main.ts",
            "import { esmClass } from \"sx-esm\";\nimport { cjsClass } from \"sx-cjs\";\n\
             export const both = [esmClass, cjsClass];\n",
        ),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    for dep in ["sx-esm", "sx-cjs"] {
        let url = main
            .split('"')
            .find(|s| s.contains(&format!("/{dep}/")))
            .unwrap_or_else(|| panic!("{dep} imported:\n{main}"));
        assert!(
            !url.contains("/@oj-deps/"),
            "{dep} kept out of the pre-bundle: {url}"
        );
        let module = server.text(url).await;
        assert!(
            !module.contains("stylex.create"),
            "{dep} compiled:\n{module}"
        );
    }
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        css.contains("color: navy"),
        "the ESM dependency's rules:\n{css}"
    );
    assert!(
        css.contains("color: maroon"),
        "the CJS dependency's rules:\n{css}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_import_form_hands_its_options_over() {
    require_fixture!("vite");
    for config in [
        "import { vite } from \"@stylexjs/unplugin\";\nexport default { plugins: [vite({ classNamePrefix: \"q\" })] };\n",
        "import stylexPlugin from \"@stylexjs/unplugin/vite\";\nexport default { plugins: [stylexPlugin({ classNamePrefix: \"q\" })] };\n",
    ] {
        let app = app("{}");
        app.write(&[("vite.config.js", config)]);
        let server = app.serve().await;
        let main = server.text("/src/main.ts").await;
        assert!(main.contains("main__styles.title q"), "{config}\n{main}");
    }
    // A CommonJS config `require()`s the package's CommonJS build.
    for config in [
        "const stylex = require(\"@stylexjs/unplugin\");\nmodule.exports = { plugins: [stylex.default.vite({ classNamePrefix: \"q\" })] };\n",
        "const { vite } = require(\"@stylexjs/unplugin\");\nmodule.exports = { plugins: [vite({ classNamePrefix: \"q\" })] };\n",
        "const stylexPlugin = require(\"@stylexjs/unplugin/vite\").default;\nmodule.exports = { plugins: [stylexPlugin({ classNamePrefix: \"q\" })] };\n",
    ] {
        let app = app("{}");
        std::fs::remove_file(app.root.join("vite.config.js")).unwrap();
        app.write(&[("vite.config.cjs", config)]);
        let server = app.serve().await;
        let main = server.text("/src/main.ts").await;
        assert!(main.contains("main__styles.title q"), "{config}\n{main}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ssr_modules_a_plugin_loads_feed_the_sheet() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[(
        "vite.config.js",
        "import stylex from \"@stylexjs/unplugin\";\n\
         const gen = { name: \"gen\", load(id) { if (id.endsWith(\"/src/gen.ts\")) return \"import * as stylex from '@stylexjs/stylex';\\nexport const s = stylex.props(stylex.create({ x: { color: 'maroon' } }).x);\\n\"; } };\n\
         export default { plugins: [stylex.vite({}), gen] };\n",
    )]);
    let server = app.serve().await;
    let id = app.root.join("src/gen.ts");
    let module = server
        .text(&format!("/@ssr-module?id={}", id.display()))
        .await;
    assert!(!module.contains("stylex.create"), "{module}");
    // Compiled under its own file name, as the client's compile of it is.
    assert!(module.contains("gen__"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: maroon"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stylex_runs_before_the_apps_transforms() {
    require_fixture!("vite");
    let app = app("{}");
    // Rewrites the StyleX import away: StyleX must have compiled first.
    app.write(&[(
        "vite.config.js",
        "import stylex from \"@stylexjs/unplugin\";\n\
         const late = { name: \"late\", transform(code, id) { if (id.endsWith(\"/src/main.ts\")) return code.replaceAll(\"@stylexjs/stylex\", \"not-stylex\"); } };\n\
         export default { plugins: [late, stylex.vite({})] };\n",
    )]);
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    assert!(
        !main.contains("stylex.create"),
        "StyleX compiled first:\n{main}"
    );
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: red"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_query_variant_keeps_its_own_rules() {
    require_fixture!("vite");
    let app = app("{}");
    let alt = main_ts("olive").replace('"', "\\\"").replace('\n', "\\n");
    app.write(&[(
        "vite.config.js",
        &format!(
            "import stylex from \"@stylexjs/unplugin\";\n\
             const alt = {{ name: \"alt\", load(id) {{ if (id.endsWith(\"/src/main.ts?alt\")) return \"{alt}\"; }} }};\n\
             export default {{ plugins: [stylex.vite({{}}), alt] }};\n"
        ),
    )]);
    let server = app.serve().await;
    server.text("/src/main.ts").await;
    let variant = server.text("/src/main.ts?alt").await;
    assert!(!variant.contains("stylex.create"), "{variant}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        css.contains("color: red") && css.contains("color: olive"),
        "both variants' rules:\n{css}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_prefixes_the_page_tags_that_then_load() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[(
        "vite.config.js",
        "import stylex from \"@stylexjs/unplugin\";\nexport default { base: \"/app/\", plugins: [stylex.vite({})] };\n",
    )]);
    let server = app.serve().await;
    let html = server.text("/app/").await;
    assert!(
        html.contains(r#"src="/app/@id/virtual:stylex:runtime""#),
        "{html}"
    );
    assert!(html.contains(r#"href="/app/virtual:stylex.css""#), "{html}");
    let runtime = server.text("/app/@id/virtual:stylex:runtime").await;
    assert!(runtime.contains("__stylex_virtual__"), "{runtime}");
    server.text("/app/src/main.ts").await;
    // The page's link, and the unprefixed path the runtime fetches.
    for sheet in ["/app/virtual:stylex.css", "/virtual:stylex.css"] {
        let css = server.text(sheet).await;
        assert!(css.contains("color: red"), "{sheet}: {css}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_the_plugin_host_transforms_get_the_tags_once() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[(
        "vite.config.js",
        "import stylex from \"@stylexjs/unplugin\";\n\
         const custom = {\n\
           name: \"custom\",\n\
           transformIndexHtml(html) { return html; },\n\
           configureServer(server) {\n\
             server.middlewares.use(async (req, res, next) => {\n\
               if (req.url !== \"/custom\") return next();\n\
               const html = await server.transformIndexHtml(req.url, \"<html><head><title>c</title></head><body></body></html>\");\n\
               res.setHeader(\"content-type\", \"text/html\");\n\
               res.end(html);\n\
             });\n\
           },\n\
         };\n\
         export default { plugins: [stylex.vite({}), custom] };\n",
    )]);
    let server = app.serve().await;
    let runtime = r#"src="/@id/virtual:stylex:runtime""#;
    let page = server.text("/").await;
    assert_eq!(page.matches(runtime).count(), 1, "{page}");
    assert_eq!(
        page.matches(r#"href="/virtual:stylex.css""#).count(),
        1,
        "{page}"
    );
    let custom = server.text("/custom").await;
    assert!(
        custom.contains(runtime),
        "a custom server's page:\n{custom}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stylex_sees_the_import_before_the_cjs_interop_rewrite() {
    require_fixture!("vite");
    let app = app("{}");
    // A CommonJS build of the runtime package: the pre-bundle gives it
    // interop, which rewrites the module's import before oj compiles it.
    app.write(&[
        (
            "node_modules/@stylexjs/stylex/package.json",
            r#"{"name":"@stylexjs/stylex","version":"0.19.0","main":"index.js"}"#,
        ),
        (
            "node_modules/@stylexjs/stylex/index.js",
            "exports.create = () => { throw new Error(\"uncompiled StyleX\"); };\nexports.props = exports.create;\nexports.defineVars = exports.create;\n",
        ),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    assert!(!main.contains("stylex.create"), "compiled:\n{main}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: red"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_loaded_client_module_feeds_the_sheet() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[
        (
            "vite.config.js",
            "import fs from \"node:fs\";\n\
             import stylex from \"@stylexjs/unplugin\";\n\
             const source = new URL(\"./sx-source.txt\", import.meta.url);\n\
             const sx = {\n\
               name: \"sx\",\n\
               resolveId(id) { if (id === \"virtual:sx\") return \"\\0virtual-sx.ts\"; },\n\
               load(id) { if (id === \"\\0virtual-sx.ts\") return fs.readFileSync(source, \"utf8\"); },\n\
             };\n\
             export default { plugins: [stylex.vite({}), sx] };\n",
        ),
        (
            "sx-source.txt",
            "import * as stylex from '@stylexjs/stylex';\n\
             export const c = stylex.props(stylex.create({ x: { color: 'maroon' } }).x).className;\n",
        ),
        ("src/main.ts", "import { c } from \"virtual:sx\";\nexport const both = c;\n"),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.ts").await;
    let url = main
        .split('"')
        .find(|s| s.starts_with("/@id/"))
        .unwrap_or_else(|| panic!("the virtual import:\n{main}"))
        .to_string();
    let module = server.text(&url).await;
    assert!(!module.contains("stylex.create"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: maroon"), "{css}");

    // Loaded again without its StyleX import, its rule leaves the sheet.
    app.write(&[("sx-source.txt", "export const c = \"plain\";\n")]);
    let module = server.text(&url).await;
    assert!(module.contains("plain"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(!css.contains("color: maroon"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_function_stub_keeps_its_modules_rules() {
    require_fixture!("vite");
    let app = app("{}");
    app.write(&[(
        "src/data.server.ts",
        "import * as stylex from \"@stylexjs/stylex\";\n\
         export const cls = () => stylex.props(stylex.create({ x: { color: \"maroon\" } }).x).className;\n",
    )]);
    let server = app.serve().await;
    let id = app.root.join("src/data.server.ts");
    server
        .text(&format!("/@ssr-module?id={}", id.display()))
        .await;
    // The page loads the client stub of the same file afterwards.
    server.text("/src/data.server.ts").await;
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: maroon"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ssr_resolves_and_loads_the_virtual_runtime() {
    require_fixture!("vite");
    let app = app("{}");
    let server = app.serve().await;
    let importer = app.root.join("src/main.ts");
    let resolved = server
        .text(&format!(
            "/@ssr-resolve?importer={}&spec=virtual:stylex:runtime",
            importer.display()
        ))
        .await;
    assert!(
        resolved.contains(r#""id":"virtual:stylex:runtime""#),
        "{resolved}"
    );
    let module = server.get("/@ssr-module?id=virtual:stylex:runtime").await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.body.contains("__stylex_virtual__"),
        "{}",
        module.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relative_base_or_unknown_dev_mode_still_boots() {
    require_fixture!("vite");
    // Vite serves a relative base from the root in dev.
    let app = app("{}");
    app.write(&[(
        "vite.config.js",
        "import stylex from \"@stylexjs/unplugin\";\nexport default { base: \"./\", plugins: [stylex.vite({})] };\n",
    )]);
    let server = app.serve().await;
    let html = server.text("/").await;
    assert!(
        html.contains(r#"src="/@id/virtual:stylex:runtime""#),
        "{html}"
    );
    assert!(html.contains(r#"href="/virtual:stylex.css""#), "{html}");
    drop(server);

    // Any devMode string: no tags, the sheet still served.
    let other = self::app("{ devMode: \"bogus\" }");
    let server = other.serve().await;
    assert!(!server.text("/").await.contains("virtual:stylex"));
    server.text("/src/main.ts").await;
    assert!(server
        .text("/virtual:stylex.css")
        .await
        .contains("color: red"));
}

// A runner environment (a plugin's dev.createEnvironment, the Cloudflare
// shape) runs through the app's own Vite in the plugin host: its modules get
// the plugin's transform there, as every environment's do under Vite, from
// its boot-time imports on (Cloudflare's configureServer loads the worker),
// and their rules feed the same sheet, leaving it with the module's.
const WORKER: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
export const s = stylex.props(stylex.create({ x: { color: \"teal\" } }).x);\n";

/// An app whose config adds a `worker` runner environment, loaded at boot
/// (`/__boot`) and again, afresh, on every `/__worker` request.
fn runner_app(options: &str) -> App {
    let app = app("{}");
    app.write(&[
        ("src/worker.ts", WORKER),
        (
            "vite.config.js",
            &format!(
                "import stylex from \"@stylexjs/unplugin\";\n\
             import {{ DevEnvironment }} from \"vite\";\n\
             let boot = null;\n\
             const runner = {{\n\
               name: \"runner-env\",\n\
               config: () => ({{ environments: {{ worker: {{ consumer: \"server\", dev: {{\n\
                 createEnvironment: (name, config) => new DevEnvironment(name, config, {{ hot: true }}),\n\
               }} }} }} }}),\n\
               async configureServer(server) {{\n\
                 const worker = server.environments.worker;\n\
                 boot = (await worker.transformRequest(\"/src/worker.ts\"))?.code ?? \"\";\n\
                 server.middlewares.use(async (req, res, next) => {{\n\
                   if (req.url === \"/__boot\") return res.end(boot);\n\
                   if (req.url !== \"/__worker\") return next();\n\
                   worker.moduleGraph.invalidateAll();\n\
                   res.end((await worker.transformRequest(\"/src/worker.ts\"))?.code ?? \"\");\n\
                 }});\n\
               }},\n\
             }};\n\
             export default {{ plugins: [stylex.vite({options}), runner] }};\n"
            ),
        ),
    ]);
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runner_environments_modules_compile_and_feed_the_sheet() {
    require_fixture!("vite");
    let app = runner_app("{}");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    assert!(
        server.built.plugin_serve.runner_environments(),
        "the worker environment never came up"
    );
    let boot = server.text("/__boot").await;
    assert!(boot.contains("worker__"), "{boot}");
    assert!(!boot.contains("stylex.create"), "{boot}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: teal"), "{css}");

    // Still importing StyleX, compiling nothing: its rule leaves the sheet.
    app.write(&[(
        "src/worker.ts",
        "import * as stylex from \"@stylexjs/stylex\";\nexport const s = typeof stylex;\n",
    )]);
    let module = server.text("/__worker").await;
    assert!(
        module.contains("typeof ") && !module.contains("worker__"),
        "{module}"
    );
    let css = server.text("/virtual:stylex.css").await;
    assert!(!css.contains("color: teal"), "{css}");
}

// StyleX 0.19.2 in a runner environment: the built-in sources pass the gate
// under a custom `importSources`, and a module that drops its StyleX imports
// drops its rules.
#[tokio::test(flavor = "multi_thread")]
async fn a_runner_environment_follows_the_plugins_gate() {
    require_fixture!("vite");
    let app = runner_app("{ importSources: [{ from: \"my-sx\", as: \"css\" }] }");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    assert!(
        server.built.plugin_serve.runner_environments(),
        "the worker environment never came up"
    );
    let boot = server.text("/__boot").await;
    assert!(
        boot.contains("worker__") && !boot.contains("stylex.create"),
        "the built-in source compiles:\n{boot}"
    );
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: teal"), "{css}");

    app.write(&[("src/worker.ts", "export const s = \"plain\";\n")]);
    let module = server.text("/__worker").await;
    assert!(module.contains("plain"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(
        !css.contains("color: teal"),
        "the module's rule is gone:\n{css}"
    );

    // And back: the module compiles again.
    app.write(&[("src/worker.ts", WORKER)]);
    let module = server.text("/__worker").await;
    assert!(module.contains("worker__"), "{module}");
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: teal"), "{css}");
}
