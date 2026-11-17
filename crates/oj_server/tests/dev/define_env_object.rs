// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A config's `define` of `import.meta.env.VITE_X` reaches the
//! `import.meta.env` object a served module gets, not only the dotted access:
//! `import.meta.env["VITE_X"]`, `const env = import.meta.env; env.VITE_X` and
//! `{ ...import.meta.env }` all see the value, as under Vite, whose import
//! analysis folds the top-level `define`'s env keys into the object it
//! injects (plugins/importAnalysis.ts `getEnv`).

use crate::{App, INDEX_HTML};

const CONFIG: &str = "export default { define: { \"import.meta.env.VITE_API_URL\": JSON.stringify(\"https://api.example\") } };\n";

const MAIN: &str = "const env = import.meta.env;\n\
export const url = env.VITE_API_URL;\n\
export const copy = { ...import.meta.env };\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_defined_env_key_is_in_the_import_meta_env_object() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"define-env","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("vite.config.mjs", CONFIG),
        ("src/main.js", MAIN),
    ]);
    let server = app.serve().await;
    server.text("/").await;
    let module = server.text("/src/main.js").await;
    assert!(
        !module.contains("import.meta.env"),
        "import.meta.env left raw:\n{module}"
    );
    // The served text is printed with a space after each key.
    let compact: String = module.split_whitespace().collect();
    assert!(
        compact.contains(r#""VITE_API_URL":"https://api.example""#),
        "the defined key is missing from the import.meta.env object:\n{module}"
    );
}

const RUNTIME_CONFIG: &str = "export default { define: {\n\
  \"import.meta.env.VITE_API_URL\": JSON.stringify(\"https://api.example\"),\n\
  \"import.meta.env.VITE_BACKEND_URL\": JSON.stringify(\"https://defined.test\"),\n\
} };\n";

/// With runtime env keys (`OJ_RUNTIME_ENV_KEYS`) the object a module reads
/// is the one `/@oj/env.js` installs, and a key the config's `define` adds
/// is in it too: a key outside the runtime list as the define wrote it, a
/// runtime key as the define's value until it is set and as the set value
/// after.
#[tokio::test(flavor = "multi_thread")]
async fn a_defined_env_key_is_in_the_runtime_env_object() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"define-env-runtime","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("vite.config.mjs", RUNTIME_CONFIG),
        ("src/main.js", MAIN),
    ]);
    let mut dev = app.dev();
    dev.runtime_env = Some(oj_server::RuntimeEnvConfig {
        keys: vec!["VITE_BACKEND_URL".into()],
    });
    let server = crate::serve(dev).await;
    server.text("/").await;

    let module = server.text("/src/main.js").await;
    assert!(
        module.contains("/@oj/env.js") && module.contains("globalThis.__oj_env"),
        "the module reads the object the env module installs:\n{module}"
    );
    assert!(
        !module.contains("api.example") && !module.contains("defined.test"),
        "no value is baked into the module:\n{module}"
    );

    let env = server.text("/@oj/env.js").await;
    assert!(
        env.contains(r#""VITE_API_URL": "https://api.example""#),
        "the defined key outside the runtime list:\n{env}"
    );
    assert!(
        env.contains(r#""VITE_BACKEND_URL": "https://defined.test""#),
        "the runtime key reads the define until it is set:\n{env}"
    );

    let res = server
        .request(
            axum::http::Request::post("/@oj/env")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"VITE_BACKEND_URL":"/_port_8001"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.body);
    let env = server.text("/@oj/env.js").await;
    assert!(
        env.contains(r#""VITE_BACKEND_URL": "/_port_8001""#) && !env.contains("defined.test"),
        "the runtime key reads the set value:\n{env}"
    );
    assert!(
        env.contains(r#""VITE_API_URL": "https://api.example""#),
        "the defined key stays:\n{env}"
    );
}
