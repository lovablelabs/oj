//! The dev server defines `process.env.NODE_ENV` (Vite parity: nodeEnv =
//! NODE_ENV || mode). Without it, library code reading it throws a
//! ReferenceError in dev while working in build. Assumes the test process
//! runs without NODE_ENV set.

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn node_env_is_development_in_dev() {
    let app = App::new(&[
        ("package.json", r#"{"name":"nodeenv","private":true}"#),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        ("src/main.js", "export const mode = process.env.NODE_ENV;\n"),
    ]);
    let server = app.serve().await;
    server.text("/").await;
    let module = server.text("/src/main.js").await;
    assert!(
        !module.contains("process.env.NODE_ENV"),
        "process.env.NODE_ENV left raw (not defined in dev):\n{module}"
    );
    assert!(
        module.contains(r#""development""#),
        "process.env.NODE_ENV not replaced with \"development\":\n{module}"
    );
}
