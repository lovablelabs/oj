// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A config's `define` of `import.meta.env.VITE_X` reaches the
//! `import.meta.env` object a served module gets, not only the dotted access:
//! `import.meta.env["VITE_X"]`, `const env = import.meta.env; env.VITE_X` and
//! `{ ...import.meta.env }` all see the value, as under Vite, whose import
//! analysis folds the user's env defines into the object it injects
//! (plugins/importAnalysis.ts `getEnv`).

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
