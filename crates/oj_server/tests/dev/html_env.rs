//! The dev server substitutes `%VITE_*%` / `%MODE%` in index.html (Vite's
//! htmlEnvHook): known keys become their env value, unknown placeholders stay
//! as written. The build half is
//! crates/oj/src/build/plugin_api_tests/html_env_build.rs.

use crate::App;

const INDEX: &str = concat!(
    "<!doctype html><html><head><title>%VITE_APP_TITLE%</title>",
    r#"<meta name="mode" content="%MODE%"><meta name="missing" content="%VITE_MISSING%">"#,
    r#"</head><body><script type="module" src="/src/main.js"></script></body></html>"#,
);

#[tokio::test(flavor = "multi_thread")]
async fn index_html_env_placeholders_substituted_in_dev() {
    let app = App::new(&[
        ("package.json", r#"{"name":"htmlenv","private":true}"#),
        (".env", "VITE_APP_TITLE=Hello OJ\n"),
        ("index.html", INDEX),
        ("src/main.js", "export const v = 1;\n"),
    ]);
    let server = app.serve().await;
    let html = server.text("/").await;
    assert!(
        html.contains("<title>Hello OJ</title>"),
        "%VITE_APP_TITLE% not substituted:\n{html}"
    );
    assert!(
        html.contains(r#"content="development""#),
        "%MODE% not substituted to development:\n{html}"
    );
    assert!(
        html.contains(r#"content="%VITE_MISSING%""#),
        "unknown %VITE_MISSING% should be left as-is:\n{html}"
    );
}
