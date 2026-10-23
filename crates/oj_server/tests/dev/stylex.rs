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
    for pkg in ["vite", "@stylexjs/unplugin", "@stylexjs/stylex"] {
        app.link_fixture(pkg);
    }
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn config_options_drive_the_compile_and_the_sheet() {
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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

#[tokio::test(flavor = "multi_thread")]
async fn css_only_mode_serves_its_shim_and_no_page_tags() {
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
async fn the_disk_cache_carries_each_modules_rules() {
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
    let app = app("{}");
    let dev = || oj_server::DevServer {
        enable_cache: true,
        ..app.dev()
    };
    {
        let server = crate::serve(dev()).await;
        server.text("/src/main.ts").await;
        // A warm start serves this entry without compiling, so the rules must
        // travel in it. Cache writes are asynchronous.
        let cache = oj_cache::cache_root(&app.root);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !persisted(&cache, "\"stylex_rules\"", "color:red") {
            assert!(
                std::time::Instant::now() < deadline,
                "main.ts never persisted its rules"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let server = crate::serve(dev()).await;
    server.text("/src/main.ts").await;
    let css = server.text("/virtual:stylex.css").await;
    assert!(css.contains("color: red"), "{css}");
}

/// Whether a persisted module under `dir` mentions both needles.
fn persisted(dir: &std::path::Path, a: &str, b: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let path = e.path();
            if path.is_dir() {
                persisted(&path, a, b)
            } else {
                path.extension().is_some_and(|x| x == "json")
                    && std::fs::read_to_string(&path).is_ok_and(|s| s.contains(a) && s.contains(b))
            }
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_restyles_the_page_on_an_edit() {
    if crate::browser::skip() {
        return;
    }
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
    require_fixture!("vite", "@stylexjs/unplugin", "@stylexjs/stylex");
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
