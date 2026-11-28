// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A CSS module's class map in dev is the one Vite serves: postcss-modules'
//! default `_${local}_${hash}_${line}`, the hash over the sheet Vite hands
//! PostCSS (a Sass module's over the Sass output), the line of the first
//! `.${local}` in it. The names were recorded from Vite 8.3.5; the build half
//! and a live-Vite check live in e2e/css-modules-vite-names.mjs.

use crate::{App, INDEX_HTML};

/// A module file, its sheet, and the class map Vite serves for it.
type Sheet = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const SHEETS: &[Sheet] = &[
    (
        "counter.module.css",
        ".button { color: red; }\n",
        &[("button", "_button_hwshl_1")],
    ),
    (
        "multi.module.css",
        ".a { color: red; }\n\n.b { color: blue; }\n.c { composes: a; }\n#main { color: green; }\n@keyframes spin { to { opacity: 1 } }\n.d { animation: spin 1s; }\n",
        &[
            ("a", "_a_hn2ce_1"),
            ("b", "_b_hn2ce_3"),
            ("c", "_c_hn2ce_4 _a_hn2ce_1"),
            ("d", "_d_hn2ce_7"),
            ("main", "_main_hn2ce_1"),
            ("spin", "_spin_hn2ce_1"),
        ],
    ),
    (
        "crlf.module.css",
        ".x {\r\n  color: red;\r\n}\r\n.y { color: blue; }\r\n",
        &[("x", "_x_1kb7v_1"), ("y", "_y_1kb7v_7")],
    ),
    (
        "prefix.module.css",
        ".btn-primary { color: red; }\n.btn { color: blue; }\n",
        &[("btn", "_btn_n9bj6_1"), ("btn-primary", "_btn-primary_n9bj6_1")],
    ),
    (
        "sass.module.scss",
        "$c: red;\n.s { color: $c; }\n",
        &[("s", "_s_76bxl_1")],
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn css_module_class_maps_are_vites() {
    let files: Vec<(String, &str)> = SHEETS
        .iter()
        .map(|(file, css, _)| (format!("src/{file}"), *css))
        .collect();
    let mut all: Vec<(&str, &str)> = files.iter().map(|(f, c)| (f.as_str(), *c)).collect();
    all.extend([
        (
            "package.json",
            r#"{"name":"css-module-names","version":"1.0.0"}"#,
        ),
        ("src/main.js", "window.__OK = true;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let app = App::new(&all);
    let server = app.serve().await;

    for (file, _, expected) in SHEETS {
        let module = server.text(&format!("/src/{file}")).await;
        // The map is Vite's dataToEsm text: `export const k = "v";` per legal
        // key, then `export default {\n\tk: k,\n\t"other-key": "v"\n};`.
        let consts: std::collections::HashMap<&str, &str> = module
            .lines()
            .filter_map(|line| line.strip_prefix("export const "))
            .filter_map(|rest| rest.split_once(" = "))
            .map(|(name, value)| (name, value.trim_end_matches(';').trim_matches('"')))
            .collect();
        let map = module
            .split_once("export default {\n")
            .and_then(|(_, rest)| rest.split_once("\n};").map(|(rows, _)| rows))
            .unwrap_or_else(|| panic!("{file}: no default export:\n{module}"));
        let mut got: Vec<(String, String)> = map
            .lines()
            .map(|row| {
                let (key, value) = row
                    .trim_start_matches('\t')
                    .trim_end_matches(',')
                    .split_once(": ")
                    .unwrap_or_else(|| panic!("{file}: a map row without a value: {row}"));
                let value = match value.strip_prefix('"') {
                    Some(quoted) => quoted.trim_end_matches('"').to_string(),
                    None => consts
                        .get(value)
                        .unwrap_or_else(|| panic!("{file}: {value} is no const:\n{module}"))
                        .to_string(),
                };
                (key.trim_matches('"').to_string(), value)
            })
            .collect();
        got.sort();
        let want: Vec<(String, String)> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(got, want, "{file}:\n{module}");
    }
}
