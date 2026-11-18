// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` applies `resolve.dedupe` as `vite build` does: a bare import of
//! a listed package resolves from the root for every importer (Vite's
//! tryNodeResolve, `basedir = root`), so a copy a dependency installs under
//! its own `node_modules` stays out of the bundle. Without the list, each
//! importer gets its nearest copy in both tools.
//!
//! Uses e2e/fixtures/start-app's install (vite); skips without it, unless
//! `OJ_REQUIRE_FIXTURES`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite/package.json").is_file());
    if dir.is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
    }
    dir
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

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A package `name` at `dir` whose entry and `sub` subpath carry `marker`.
fn package(dir: &Path, name: &str, marker: &str) {
    write(
        &dir.join("package.json"),
        &format!(r#"{{ "name": "{name}", "type": "module", "main": "index.js" }}"#),
    );
    write(
        &dir.join("index.js"),
        &format!("export const copy = \"{marker}\";\n"),
    );
    write(
        &dir.join("sub.js"),
        &format!("export const sub = \"{marker}-sub\";\n"),
    );
}

/// An app importing `shared`, `shared/sub` and `@scope/shared` itself and
/// through `consumer`, which has copies of both packages of its own, built by
/// Vite and by oj with `config`; per tool, the text of every emitted `.js`
/// file. With `root_copies` false the root has no `shared` package, only an
/// app folder of that name.
fn build_both(config: &str, root_copies: bool) -> Option<[String; 2]> {
    let fixture = node_modules()?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let modules = root.join("node_modules");
    std::fs::create_dir(&modules).unwrap();
    for entry in std::fs::read_dir(&fixture).unwrap().flatten() {
        std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    let consumer = modules.join("consumer");
    write(
        &consumer.join("package.json"),
        r#"{ "name": "consumer", "type": "module", "main": "index.js" }"#,
    );
    package(
        &consumer.join("node_modules/shared"),
        "shared",
        "nested-shared-copy",
    );
    let main = if root_copies {
        package(&modules.join("shared"), "shared", "root-shared-copy");
        package(
            &modules.join("@scope/shared"),
            "@scope/shared",
            "root-scoped-copy",
        );
        package(
            &consumer.join("node_modules/@scope/shared"),
            "@scope/shared",
            "nested-scoped-copy",
        );
        write(
            &consumer.join("index.js"),
            "import { copy } from \"shared\";\nimport { sub } from \"shared/sub\";\nimport { copy as scoped } from \"@scope/shared\";\nexport const viaConsumer = copy + sub + scoped;\n",
        );
        "import { copy } from \"shared\";\nimport { sub } from \"shared/sub\";\nimport { copy as scoped } from \"@scope/shared\";\nimport { viaConsumer } from \"consumer\";\ndocument.title = copy + sub + scoped + viaConsumer;\n"
    } else {
        write(
            &root.join("shared/index.js"),
            "export const copy = \"app-folder-copy\";\n",
        );
        write(
            &consumer.join("index.js"),
            "import { copy } from \"shared\";\nexport const viaConsumer = copy;\n",
        );
        "import { viaConsumer } from \"consumer\";\ndocument.title = viaConsumer;\n"
    };
    write(
        &root.join("package.json"),
        r#"{ "name": "dedupe-build", "type": "module" }"#,
    );
    write(
        &root.join("index.html"),
        "<!doctype html><html><body><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n",
    );
    write(&root.join("src/main.js"), main);
    write(&root.join("vite.config.js"), config);
    run(Command::new("node")
        .arg(fixture.join("vite/bin/vite.js"))
        .args(["build", "--outDir", "vite-dist"])
        .current_dir(&root));
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "oj-dist"])
        .arg(&root)
        .current_dir(&root));
    Some([
        js_text(&root.join("vite-dist")),
        js_text(&root.join("oj-dist")),
    ])
}

fn js_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "js") {
                out.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

const DEDUPE: &str = "export default { resolve: { dedupe: [\"shared\", \"@scope/shared\"] } };\n";

#[test]
fn a_deduped_package_is_bundled_once_from_the_root() {
    let Some(built) = build_both(DEDUPE, true) else {
        return;
    };
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        for root in [
            "root-shared-copy",
            "root-shared-copy-sub",
            "root-scoped-copy",
        ] {
            assert!(code.contains(root), "{tool} lacks {root}:\n{code}");
        }
        for nested in ["nested-shared-copy", "nested-scoped-copy"] {
            assert!(
                !code.contains(nested),
                "{tool} bundled the copy under consumer ({nested}):\n{code}"
            );
        }
    }
}

#[test]
fn without_dedupe_each_importer_gets_its_nearest_copy() {
    let Some(built) = build_both("export default {};\n", true) else {
        return;
    };
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        for copy in [
            "root-shared-copy",
            "root-shared-copy-sub",
            "root-scoped-copy",
            "nested-shared-copy",
            "nested-shared-copy-sub",
            "nested-scoped-copy",
        ] {
            assert!(code.contains(copy), "{tool} lacks {copy}:\n{code}");
        }
    }
}

/// With no root copy the importer's own stands; an app folder named like the
/// package is never the answer to a bare import.
#[test]
fn a_deduped_package_with_no_root_copy_keeps_the_importers_copy() {
    let Some(built) = build_both(DEDUPE, false) else {
        return;
    };
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("nested-shared-copy"), "{tool}:\n{code}");
        assert!(
            !code.contains("app-folder-copy"),
            "{tool} resolved the bare import to the app's folder:\n{code}"
        );
    }
}
