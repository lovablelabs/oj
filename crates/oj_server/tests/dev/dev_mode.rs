//! `oj dev --mode <mode>` selects `.env.<mode>`, sets `import.meta.env.MODE`
//! and hands the mode to a function config, like `vite dev --mode`; a
//! non-production mode is still `DEV`. Without the flag the mode is
//! `development`.

use crate::{serve, App, INDEX_HTML};

const MAIN: &str =
    "window.__MODE = import.meta.env.MODE; window.__FLAVOR = import.meta.env.VITE_FLAVOR;\n\
window.__DEV = import.meta.env.DEV; window.__CFG = __CFG_MODE__;\n";

fn app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"devmode-app","version":"1.0.0"}"#,
        ),
        (".env.development", "VITE_FLAVOR=dev-flavor\n"),
        (".env.staging", "VITE_FLAVOR=staging-flavor\n"),
        (
            "oj.config.js",
            "export default ({ mode }) => ({ define: { __CFG_MODE__: JSON.stringify(mode) } });\n",
        ),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn default_mode_is_development() {
    let app = app();
    let server = app.serve().await;
    server.text("/").await;
    let dev = server.text("/src/main.js").await;
    assert!(
        dev.contains(r#"__MODE = "development""#),
        "default mode is development:\n{dev}"
    );
    assert!(
        dev.contains(r#""dev-flavor""#),
        "default loads .env.development:\n{dev}"
    );
    assert!(
        dev.contains(r#"__CFG = "development""#),
        "config fn sees mode=development:\n{dev}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mode_flag_selects_env_file_and_config_mode() {
    let app = app();
    let mut dev = app.dev();
    dev.mode = Some("staging".into());
    let server = serve(dev).await;
    server.text("/").await;
    let staging = server.text("/src/main.js").await;
    assert!(
        staging.contains(r#"__MODE = "staging""#),
        "--mode sets MODE:\n{staging}"
    );
    assert!(
        staging.contains(r#""staging-flavor""#),
        "--mode staging loads .env.staging:\n{staging}"
    );
    assert!(
        !staging.contains("dev-flavor"),
        ".env.development is not loaded for --mode staging:\n{staging}"
    );
    assert!(
        staging.contains("__DEV = true"),
        "a non-production dev mode is still DEV:\n{staging}"
    );
    assert!(
        staging.contains(r#"__CFG = "staging""#),
        "config fn sees mode=staging:\n{staging}"
    );
}
