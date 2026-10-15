// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use serde_json::Value;

use super::{build_app, scratch, write_files};

/// `.vite/manifest.json` from a real build lists every chunk, not just the
/// entries (Vite's manifest plugin walks the whole bundle): a `manualChunks`
/// vendor chunk the entry statically imports gets a row keyed `_<file name>`
/// with no `src` and no `isEntry`, and the entry's `imports` name that row by
/// its manifest KEY, never by output file name. Every `imports` /
/// `dynamicImports` reference resolves to a key in the same manifest.
/// (The row serializer itself is pinned on hand-made rows by
/// `manifest_matches_vite_shape`; this pins the rows a build derives.)
#[tokio::test]
async fn manifest_lists_non_entry_chunks_and_references_them_by_key() {
    let root = scratch("manifest-chunks");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"m","private":true,"dependencies":{"mylib":"1.0.0"}}"#,
            ),
            (
                "node_modules/mylib/package.json",
                r#"{"name":"mylib","version":"1.0.0","type":"module","main":"index.js"}"#,
            ),
            // Non-trivial, so rolldown keeps it as its own chunk (tiny modules
            // get inlined below the chunk-size threshold even with manualChunks).
            (
                "node_modules/mylib/index.js",
                "export const hello = 'hi from mylib';\nexport function greet(name) { return hello + ' ' + name.toUpperCase(); }\nexport const data = Array.from({ length: 64 }, (_, i) => ({ i, sq: i * i }));\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#,
            ),
            (
                "src/main.tsx",
                "import { greet, data } from \"mylib\";\nwindow.__h = greet(\"world\") + data.length;\n",
            ),
            (
                "oj.config.mjs",
                "export default { build: { manifest: true, rollupOptions: { output: { manualChunks: { vendor: [\"mylib\"] } } } } };\n",
            ),
        ],
    );
    build_app(&root).await.expect("the manifest app builds");

    let raw = fs::read_to_string(root.join("dist/.vite/manifest.json")).expect("manifest written");
    let manifest: Value = serde_json::from_str(&raw).expect("manifest is JSON");
    let m = manifest.as_object().expect("manifest is an object");
    let keys: Vec<&String> = m.keys().collect();

    let entry = m
        .get("src/main.tsx")
        .unwrap_or_else(|| panic!("entry keyed by its source path: {keys:?}"));
    assert_eq!(entry["isEntry"], true, "entry row is isEntry:\n{raw}");

    let vendor_re = regex::Regex::new(r"^_vendor-.*\.js$").unwrap();
    let vendor_key = keys
        .iter()
        .find(|k| vendor_re.is_match(k))
        .unwrap_or_else(|| panic!("non-entry vendor chunk keyed _<file>: {keys:?}"));
    let vendor = &m[vendor_key.as_str()];
    assert!(
        vendor.get("src").is_none(),
        "non-entry chunk has no src:\n{raw}"
    );
    assert!(
        vendor.get("isEntry").is_none() || vendor["isEntry"] == false,
        "non-entry chunk is not isEntry:\n{raw}"
    );

    let imports: Vec<&str> = entry["imports"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        imports.contains(&vendor_key.as_str()),
        "entry.imports references the vendor KEY {vendor_key}, not a file name: {imports:?}"
    );

    for (key, row) in m {
        for field in ["imports", "dynamicImports"] {
            for r in row[field].as_array().into_iter().flatten() {
                let r = r.as_str().expect("references are strings");
                assert!(
                    m.contains_key(r),
                    "{key}.{field} references missing manifest key {r}:\n{raw}"
                );
            }
        }
    }
}
