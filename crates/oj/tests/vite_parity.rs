// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` writes the same files as `vite build`, byte for byte: each app
//! here is built by the real Vite (8.3.5, from e2e/fixtures/stylex-app's
//! install, whose react/vite/plugin-react they share) and by oj, and every
//! JS chunk, stylesheet and asset must be identical under the same name.
//! Pages are compared apart: Vite moves the entry `<script>` into `<head>`.
//!
//! Without the fixture installed the tests skip, unless `OJ_REQUIRE_FIXTURES`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/stylex-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite/bin/vite.js").is_file());
    if dir.is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the stylex-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/stylex-app");
    }
    dir
}

/// A scratch app from `files` (paths to contents; a value of `None` writes
/// 6000 random-ish bytes, an asset too big to inline).
fn app(name: &str, files: &[(&str, Option<&str>)]) -> Option<PathBuf> {
    let modules = node_modules()?;
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-vite-parity-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match body {
            Some(text) => std::fs::write(path, text).unwrap(),
            None => std::fs::write(
                path,
                (0..6000u32)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
                    .collect::<Vec<u8>>(),
            )
            .unwrap(),
        }
    }
    std::os::unix::fs::symlink(modules, root.join("node_modules")).unwrap();
    Some(root)
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every file under `dir`, relative path to bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// Builds `root` with Vite and with oj and asserts every non-page file is
/// the same file, with the same bytes.
fn assert_same_output(root: &Path) {
    run(Command::new("node")
        .arg(root.join("node_modules/vite/bin/vite.js"))
        .args(["build", "--outDir", "vite-dist"])
        .current_dir(root));
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "oj-dist"])
        .arg(root)
        .current_dir(root));
    let not_page = |(name, _): &(&String, &Vec<u8>)| !name.ends_with(".html");
    let vite: BTreeMap<_, _> = files(&root.join("vite-dist")).into_iter().collect();
    let oj: BTreeMap<_, _> = files(&root.join("oj-dist")).into_iter().collect();
    let vite_names: Vec<&String> = vite.iter().filter(not_page).map(|(n, _)| n).collect();
    let oj_names: Vec<&String> = oj.iter().filter(not_page).map(|(n, _)| n).collect();
    assert_eq!(oj_names, vite_names, "the same files, named the same");
    for name in vite_names {
        assert!(
            oj[name] == vite[name],
            "{name} differs from Vite's:\n--- vite\n{}\n--- oj\n{}",
            String::from_utf8_lossy(&vite[name]),
            String::from_utf8_lossy(&oj[name])
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

const PAGE: &str = r#"<!doctype html><html><head><title>p</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

/// A dynamic import sharing a module with its importer, its stylesheet, a
/// JSON import, an emitted image and the modulepreload polyfill.
fn lazy_app(name: &str, config: &str) -> Option<PathBuf> {
    app(
        name,
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            ("vite.config.js", Some(config)),
            ("index.html", Some(PAGE)),
            (
                "src/main.js",
                Some(
                    "import data from \"./data.json\";\nimport { shared } from \"./shared.js\";\nimport logo from \"./logo.png\";\nimport \"./main.css\";\n\
                     document.body.dataset.logo = logo;\ndocument.title = `${data.name} ${shared(1)}`;\n\
                     window.__lazy = () => import(\"./lazy.js\").then((m) => m.mount());\n",
                ),
            ),
            (
                "src/lazy.js",
                Some("import { shared } from \"./shared.js\";\nimport \"./lazy.css\";\nexport const mount = () => shared(41);\n"),
            ),
            ("src/shared.js", Some("export const shared = (n) => `count ${n + 1}`;\n")),
            ("src/data.json", Some("{\"name\":\"parity\",\"unused\":1}\n")),
            ("src/main.css", Some("body { margin: 0; }\n")),
            ("src/lazy.css", Some(".lazy { color: rebeccapurple; }\n")),
            ("src/logo.png", None),
        ],
    )
}

#[test]
fn a_lazy_route_app_builds_vites_exact_files() {
    let Some(root) = lazy_app("lazy", "export default {};\n") else {
        return;
    };
    assert_same_output(&root);
}

#[test]
fn a_relative_base_builds_vites_exact_files() {
    let Some(root) = lazy_app("relative", "export default { base: \"./\" };\n") else {
        return;
    };
    assert_same_output(&root);
}

#[test]
fn without_modulepreload_vites_exact_files() {
    let Some(root) = lazy_app(
        "no-preload",
        "export default { build: { modulePreload: false } };\n",
    ) else {
        return;
    };
    assert_same_output(&root);
}

#[test]
fn css_only_pages_build_vites_exact_files() {
    let Some(root) = app(
        "css-pages",
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            (
                "vite.config.js",
                Some("export default { build: { rollupOptions: { input: { main: \"index.html\", admin: \"admin/index.html\" } } } };\n"),
            ),
            ("index.html", Some(PAGE)),
            (
                "admin/index.html",
                Some(r#"<!doctype html><html><head><title>a</title></head><body><script type="module" src="/src/admin.js"></script></body></html>"#),
            ),
            ("src/main.js", Some("import \"./main.css\";\n")),
            ("src/admin.js", Some("import \"./admin.css\";\n")),
            ("src/main.css", Some("body { color: rgb(1, 2, 3); }\n")),
            ("src/admin.css", Some("body { color: rgb(4, 5, 6); }\n")),
        ],
    ) else {
        return;
    };
    assert_same_output(&root);
}
