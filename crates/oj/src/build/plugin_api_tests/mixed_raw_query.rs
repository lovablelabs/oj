// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, files_under, scratch, write_files};

/// `raw` anywhere in an import's query is the file's text, as Vite's rawRE
/// (`(\?|&)raw(?:&|$)`) reads it: a stylesheet is neither compiled nor
/// emitted, and TypeScript stays as written. The expected array is what Vite
/// 8.3.5 builds for this app.
#[tokio::test]
async fn raw_anywhere_in_the_query_is_the_files_text() {
    let root = scratch("mixed-raw-query");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"mixed-raw","version":"1.0.0"}"#),
            ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
            ("src/b.css", ".b { color: blue; }\n"),
            ("src/util.ts", "export const n: number = 1;\n"),
            (
                "src/main.js",
                "import a from \"./a.scss?v=1&raw\";\nimport b from \"./b.css?raw&v=2\";\nimport u from \"./util.ts?v=1&raw\";\nwindow.__R = [a, b, u];\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let files = files_under(&root.join("dist/assets"));
    assert!(
        files.iter().all(|(rel, _)| !rel.ends_with(".css")),
        "no stylesheet emitted: {:?}",
        files.iter().map(|(r, _)| r).collect::<Vec<_>>()
    );
    let js: String = files
        .iter()
        .filter(|(rel, _)| rel.ends_with(".js"))
        .map(|(_, code)| code.as_str())
        .collect();
    let lit = |s: &str| serde_json::to_string(s).unwrap();
    for text in [
        "$c: red;\n.a { color: $c; }\n",
        ".b { color: blue; }\n",
        "export const n: number = 1;\n",
    ] {
        let template = format!("`{text}`");
        assert!(
            js.contains(&lit(text)) || js.contains(&template),
            "{text:?} as written:\n{js}"
        );
    }
}
