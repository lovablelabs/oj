// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Tailwind v4 sheet with no PostCSS config keeps its compiler and scanner
//! between compiles, as @tailwindcss/vite and @tailwindcss/postcss do: a
//! source edit rescans incrementally, and candidates accumulate for the
//! session (a class removed from the source stays); an edit to a build
//! dependency (an `@plugin` file) or to the sheet rebuilds the compiler.

use std::time::Duration;

use crate::{require_fixture, App};

const SHEET: &str = "/src/index.css?direct";

fn component(class: &str) -> String {
    format!("export const A = () => <h1 className=\"{class}\">hi</h1>;\n")
}

fn plugin(marker: &str) -> String {
    format!("export default function ({{ addBase }}) {{ addBase({{ \".{marker}\": {{ color: \"red\" }} }}); }}\n")
}

async fn until(server: &crate::Server, what: &str, ok: impl Fn(&str) -> bool) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let css = server.text(SHEET).await;
        if ok(&css) {
            return css;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}:\n{css}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v4_sheet_rescans_incrementally_and_rebuilds_on_build_deps() {
    require_fixture!("tailwindcss", "@tailwindcss");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"tw4-persist","version":"1.0.0","type":"module"}"#,
        ),
        ("vite.config.mjs", "export default {};\n"),
        (
            "src/index.css",
            "@import \"tailwindcss\";\n@plugin \"./plug.js\";\n",
        ),
        ("src/plug.js", &plugin("from-plugin-v1")),
        ("src/App.tsx", &component("underline")),
        ("index.html", crate::INDEX_HTML),
    ]);
    app.link_fixture("tailwindcss");
    app.link_fixture("@tailwindcss");
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(
        first.contains(".underline") && first.contains(".from-plugin-v1"),
        "{first}"
    );

    app.write(&[("src/App.tsx", &component("italic"))]);
    let css = until(&server, "the new class reaches the sheet", |c| {
        c.contains(".italic")
    })
    .await;
    assert!(
        css.contains(".underline"),
        "a removed class stays, as Tailwind's scanner accumulates candidates:\n{css}"
    );

    app.write(&[("src/plug.js", &plugin("from-plugin-v2"))]);
    let css = until(&server, "a plugin edit rebuilds the compiler", |c| {
        c.contains(".from-plugin-v2")
    })
    .await;
    assert!(!css.contains(".from-plugin-v1"), "{css}");
    assert!(css.contains(".italic"), "{css}");

    app.write(&[(
        "src/index.css",
        "@import \"tailwindcss\";\n@plugin \"./plug.js\";\n.from-sheet { color: blue; }\n",
    )]);
    until(&server, "a sheet edit rebuilds the compiler", |c| {
        c.contains(".from-sheet")
    })
    .await;
}
