//! A function `oj.config.js` is called with Vite's `{ command, mode }`: under
//! `oj dev` that is `serve` + `development`, so `base` and `define` follow it.
//! The build half is crates/oj/src/build/plugin_api_tests/config_function_build.rs.

use crate::{App, INDEX_HTML};

const CONFIG: &str = r#"export default ({ command, mode }) => ({
  base: command === "build" ? "/prod/" : "/dev/",
  define: { __MODE__: JSON.stringify(mode) },
});
"#;

#[tokio::test(flavor = "multi_thread")]
async fn function_config_sees_serve_and_development() {
    let app = App::new(&[
        ("package.json", r#"{"name":"fncfg-app","version":"1.0.0"}"#),
        ("oj.config.js", CONFIG),
        ("src/main.js", "window.__M = __MODE__;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;

    // Readiness: the page answers under the serve-time base.
    server.text("/dev/").await;
    let main = server.text("/dev/src/main.js").await;
    assert!(
        main.contains("development"),
        "dev config define did not apply mode=development:\n{main}"
    );
}
