// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` applies `resolve.dedupe` as `vite build` does: a bare import of
//! a listed package resolves from the root for every importer (Vite's
//! resolve plugin with its `dedupe` option), so a copy a dependency installs under
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
    Some(dist_files(&root, &["js"]).map(|f| f.concat()))
}

/// Per tool, the text of each file with one of `exts` under its dist, one
/// entry per file.
fn dist_files(root: &Path, exts: &[&str]) -> [Vec<String>; 2] {
    ["vite-dist", "oj-dist"].map(|dist| {
        let mut out = Vec::new();
        let mut stack = vec![root.join(dist)];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .is_some_and(|e| exts.iter().any(|x| e == *x))
                {
                    out.push(std::fs::read_to_string(&path).unwrap());
                }
            }
        }
        out
    })
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

/// An app whose `consumer` dependency has its own copy of `shared` beside the
/// root's (each copy's entry, `sub`, `s.css` carrying `root-copy` or
/// `nested-copy`), with `files` written over it under the root.
fn shared_app(fixture: &Path, files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let modules = root.join("node_modules");
    std::fs::create_dir(&modules).unwrap();
    for entry in std::fs::read_dir(fixture).unwrap().flatten() {
        std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    for (dir, marker) in [
        ("node_modules/shared", "root-copy"),
        ("node_modules/consumer/node_modules/shared", "nested-copy"),
    ] {
        package(&root.join(dir), "shared", marker);
        write(
            &root.join(dir).join("s.css"),
            &format!(".x {{ content: \"{marker}-css\"; }}\n"),
        );
    }
    write(
        &modules.join("consumer/package.json"),
        r#"{ "name": "consumer", "type": "module", "main": "index.js" }"#,
    );
    write(
        &root.join("package.json"),
        r#"{ "name": "dedupe-build", "type": "module" }"#,
    );
    write(
        &root.join("index.html"),
        "<!doctype html><html><body><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n",
    );
    write(
        &root.join("vite.config.js"),
        "export default { resolve: { dedupe: [\"shared\"] }, build: { minify: false } };\n",
    );
    for (rel, body) in files {
        write(&root.join(rel), body);
    }
    tmp
}

/// `vite <args>` and `oj <args>` in `root`; per tool, its output.
fn both(root: &Path, fixture: &Path, args: &[&str]) -> [std::process::Output; 2] {
    let vite = Command::new("node")
        .arg(fixture.join("vite/bin/vite.js"))
        .args(args)
        .args(["--outDir", "vite-dist"])
        .current_dir(root)
        .output()
        .unwrap();
    let oj = Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(args)
        .args(["--outDir", "oj-dist"])
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    [vite, oj]
}

/// `both`, each required to succeed; per tool, the text of every emitted
/// file with one of `exts`.
fn built_text(root: &Path, fixture: &Path, args: &[&str], exts: &[&str]) -> [String; 2] {
    for (tool, out) in ["vite", "oj"].iter().zip(both(root, fixture, args)) {
        assert!(
            out.status.success(),
            "{tool} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    dist_files(root, exts).map(|f| f.concat())
}

/// Vite's CSS resolvers apply the client environment's dedupe.
#[test]
fn a_stylesheet_import_inside_a_dependency_takes_the_root_copy() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "src/main.js",
                "import \"consumer/style.css\";\ndocument.title = \"css\";\n",
            ),
            ("node_modules/consumer/index.js", "export {};\n"),
            (
                "node_modules/consumer/style.css",
                "@import \"shared/s.css\";\n.consumer { color: red; }\n",
            ),
        ],
    );
    let built = built_text(app.path(), &fixture, &["build"], &["css"]);
    for (tool, css) in ["vite", "oj"].iter().zip(built) {
        assert!(
            css.contains("root-copy-css"),
            "{tool} lacks the root copy:\n{css}"
        );
        assert!(
            !css.contains("nested-copy-css"),
            "{tool} inlined the copy under consumer:\n{css}"
        );
    }
}

/// The root's copy is the one taken, so its `exports` error fails the build.
#[test]
fn a_subpath_the_root_copys_exports_hide_fails_the_build() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "node_modules/shared/package.json",
                r#"{ "name": "shared", "type": "module", "exports": { ".": "./index.js" } }"#,
            ),
            (
                "node_modules/consumer/index.js",
                "import { sub } from \"shared/sub\";\nexport const v = sub;\n",
            ),
            (
                "src/main.js",
                "import { v } from \"consumer\";\ndocument.title = v;\n",
            ),
        ],
    );
    let [vite, oj] = both(app.path(), &fixture, &["build"]);
    let vite_err = String::from_utf8_lossy(&vite.stderr);
    assert!(
        !vite.status.success() && vite_err.contains("is not exported"),
        "vite: {vite_err}"
    );
    let err = String::from_utf8_lossy(&oj.stderr);
    assert!(
        !oj.status.success(),
        "oj built it with the copy under consumer"
    );
    assert!(
        err.contains("\"shared/sub\"") && err.contains("root's copy of \"shared\""),
        "{err}"
    );
}

#[test]
fn a_stylesheet_the_root_copys_exports_hide_fails_the_build() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "node_modules/shared/package.json",
                r#"{ "name": "shared", "type": "module", "exports": { ".": "./index.js" } }"#,
            ),
            (
                "src/main.js",
                "import \"consumer/style.css\";\ndocument.title = \"css\";\n",
            ),
            ("node_modules/consumer/index.js", "export {};\n"),
            (
                "node_modules/consumer/style.css",
                "@import \"shared/s.css\";\n.consumer { color: red; }\n",
            ),
        ],
    );
    let [vite, oj] = both(app.path(), &fixture, &["build"]);
    let vite_err = String::from_utf8_lossy(&vite.stderr);
    assert!(
        !vite.status.success() && vite_err.contains("is not exported"),
        "vite: {vite_err}"
    );
    let err = String::from_utf8_lossy(&oj.stderr);
    assert!(
        !oj.status.success(),
        "oj built it with the copy under consumer"
    );
    assert!(
        err.contains("\"shared/s.css\"") && err.contains("root's copy of \"shared\""),
        "{err}"
    );
}

/// The app's `browser` field maps a deduped package to `false`: both tools
/// bundle an empty module, not the copy under the dependency.
#[test]
fn a_deduped_package_the_root_maps_to_false_is_an_empty_module() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "package.json",
                r#"{ "name": "dedupe-build", "type": "module", "browser": { "shared": false } }"#,
            ),
            (
                "node_modules/consumer/index.js",
                "import { copy } from \"shared\";\nexport const v = copy;\n",
            ),
            (
                "src/main.js",
                "import { v } from \"consumer\";\ndocument.title = v;\n",
            ),
        ],
    );
    let built = built_text(app.path(), &fixture, &["build"], &["js"]);
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(
            !code.contains("nested-copy") && !code.contains("root-copy"),
            "{tool}:\n{code}"
        );
    }
}

#[test]
fn a_file_only_in_the_nested_copy_still_falls_back_to_it() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "node_modules/consumer/node_modules/shared/only.js",
                "export const only = \"nested-only-file\";\n",
            ),
            (
                "node_modules/consumer/index.js",
                "import { only } from \"shared/only.js\";\nimport { copy } from \"shared\";\nexport const v = only + copy;\n",
            ),
            (
                "src/main.js",
                "import { v } from \"consumer\";\ndocument.title = v;\n",
            ),
        ],
    );
    let built = built_text(app.path(), &fixture, &["build"], &["js"]);
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("nested-only-file"), "{tool}:\n{code}");
        assert!(code.contains("root-copy"), "{tool}:\n{code}");
        assert!(!code.contains("\"nested-copy\""), "{tool}:\n{code}");
    }
}

/// Vite's worker bundles resolve with the top-level `resolve.dedupe`.
#[test]
fn a_workers_nested_import_takes_the_root_copy() {
    let Some(fixture) = node_modules() else {
        return;
    };
    for main in [
        "const w = new Worker(new URL(\"./w.js\", import.meta.url), { type: \"module\" });\nw.onmessage = (e) => { document.title = e.data; };\n",
        "import W from \"./w.js?worker\";\nnew W().onmessage = (e) => { document.title = e.data; };\n",
    ] {
        let app = shared_app(
            &fixture,
            &[
                ("src/main.js", main),
                (
                    "src/w.js",
                    "import { v } from \"consumer\";\npostMessage(v);\n",
                ),
                (
                    "node_modules/consumer/index.js",
                    "import { copy } from \"shared\";\nexport const v = copy;\n",
                ),
            ],
        );
        let built = built_text(app.path(), &fixture, &["build"], &["js"]);
        for (tool, code) in ["vite", "oj"].iter().zip(built) {
            assert!(code.contains("root-copy"), "{tool} ({main}):\n{code}");
            assert!(
                !code.contains("nested-copy"),
                "{tool} bundled the copy under consumer into the worker ({main}):\n{code}"
            );
        }
    }
}

/// A deep import with a query names its package (`shared/sub.js?raw`), so a
/// worker bundle takes the root copy for it too.
#[test]
fn a_workers_queried_deep_import_of_a_deduped_package_takes_the_root_copy() {
    let Some(fixture) = node_modules() else {
        return;
    };
    for main in [
        "const w = new Worker(new URL(\"./w.js\", import.meta.url), { type: \"module\" });\nw.onmessage = (e) => { document.title = e.data; };\n",
        "import W from \"./w.js?worker\";\nnew W().onmessage = (e) => { document.title = e.data; };\n",
    ] {
        let app = shared_app(
            &fixture,
            &[
                ("src/main.js", main),
                (
                    "src/w.js",
                    "import { v } from \"consumer\";\npostMessage(v);\n",
                ),
                (
                    "node_modules/consumer/index.js",
                    "import s from \"shared/sub.js?raw\";\nexport const v = s;\n",
                ),
            ],
        );
        let built = built_text(app.path(), &fixture, &["build"], &["js"]);
        for (tool, code) in ["vite", "oj"].iter().zip(built) {
            assert!(code.contains("root-copy-sub"), "{tool} ({main}):\n{code}");
            assert!(
                !code.contains("nested-copy"),
                "{tool} bundled the copy under consumer into the worker ({main}):\n{code}"
            );
        }
    }
}

/// The ssr environment's list applies to what the server build bundles; an
/// externalized import stays a bare `shared`.
#[test]
fn a_bundled_server_dependency_takes_the_root_copy() {
    let Some(fixture) = node_modules() else {
        return;
    };
    for (no_external, bundled) in [("true", true), ("[\"consumer\"]", false)] {
        let config = format!(
            "export default {{ resolve: {{ dedupe: [\"shared\"] }}, ssr: {{ noExternal: {no_external} }}, build: {{ minify: false }} }};\n"
        );
        let app = shared_app(
            &fixture,
            &[
                ("vite.config.js", &config),
                (
                    "src/entry.js",
                    "import { v } from \"consumer\";\nexport const render = () => v;\n",
                ),
                (
                    "node_modules/consumer/index.js",
                    "import { copy } from \"shared\";\nexport const v = copy;\n",
                ),
            ],
        );
        let built = built_text(
            app.path(),
            &fixture,
            &["build", "--ssr", "src/entry.js"],
            &["js", "mjs"],
        );
        for (tool, code) in ["vite", "oj"].iter().zip(built) {
            assert!(
                !code.contains("nested-copy"),
                "{tool} bundled the copy under consumer (noExternal {no_external}):\n{code}"
            );
            if bundled {
                assert!(code.contains("root-copy"), "{tool}:\n{code}");
            } else {
                assert!(code.contains("from \"shared\""), "{tool}:\n{code}");
            }
        }
    }
}

/// A Node built-in named in the list stays external in a server build that
/// bundles its dependencies, as in Vite.
#[test]
fn a_listed_node_builtin_in_a_bundled_server_dependency_stays_external() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "vite.config.js",
                "export default { resolve: { dedupe: [\"shared\", \"buffer\"] }, ssr: { noExternal: true }, build: { minify: false } };\n",
            ),
            (
                "src/entry.js",
                "import { v } from \"consumer\";\nexport const render = () => v;\n",
            ),
            (
                "node_modules/consumer/index.js",
                "import { Buffer } from \"buffer\";\nimport { copy } from \"shared\";\nexport const v = copy + Buffer.from(\"x\").length;\n",
            ),
        ],
    );
    let built = built_text(
        app.path(),
        &fixture,
        &["build", "--ssr", "src/entry.js"],
        &["js", "mjs"],
    );
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("from \"buffer\""), "{tool}:\n{code}");
        assert!(code.contains("root-copy"), "{tool}:\n{code}");
    }
}

/// rolldown's resolver reads the package name with the query on, so a bare
/// `shared?raw` is not a listed name and keeps the importer's copy.
#[test]
fn a_bare_import_with_a_query_keeps_the_importers_copy() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "node_modules/consumer/index.js",
                "import raw from \"shared?raw\";\nexport const v = raw;\n",
            ),
            (
                "src/main.js",
                "import { v } from \"consumer\";\ndocument.title = v;\n",
            ),
        ],
    );
    let built = built_text(app.path(), &fixture, &["build"], &["js"]);
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("nested-copy"), "{tool}:\n{code}");
        assert!(
            !code.contains("root-copy"),
            "{tool} deduped the queried import:\n{code}"
        );
    }
}

/// The client environment's list applies to the page's bundle; a worker
/// bundle resolves with the top-level list only.
#[test]
fn a_client_environment_list_dedupes_the_page_but_not_a_worker() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "vite.config.js",
                "export default { environments: { client: { resolve: { dedupe: [\"shared\"] } } }, build: { minify: false } };\n",
            ),
            (
                "src/main.js",
                "const w = new Worker(new URL(\"./w.js\", import.meta.url), { type: \"module\" });\nw.onmessage = (e) => { document.title = e.data; };\nimport { v } from \"consumer\";\nconsole.log(v);\n",
            ),
            (
                "src/w.js",
                "import { v } from \"consumer\";\npostMessage(v);\n",
            ),
            (
                "node_modules/consumer/index.js",
                "import { copy } from \"shared\";\nexport const v = copy;\n",
            ),
        ],
    );
    let _ = built_text(app.path(), &fixture, &["build"], &["js"]);
    for (tool, files) in ["vite", "oj"].iter().zip(dist_files(app.path(), &["js"])) {
        let page: String = files
            .iter()
            .filter(|f| f.contains("document.title"))
            .map(String::as_str)
            .collect();
        let worker: String = files
            .iter()
            .filter(|f| f.contains("postMessage") && !f.contains("document.title"))
            .map(String::as_str)
            .collect();
        assert!(
            page.contains("root-copy") && !page.contains("nested-copy"),
            "{tool} page:\n{page}"
        );
        assert!(
            worker.contains("nested-copy") && !worker.contains("root-copy"),
            "{tool} worker:\n{worker}"
        );
    }
}

/// A list only in `environments.ssr` applies to the server build.
#[test]
fn an_ssr_environment_list_dedupes_the_server_bundle() {
    let Some(fixture) = node_modules() else {
        return;
    };
    let app = shared_app(
        &fixture,
        &[
            (
                "vite.config.js",
                "export default { environments: { ssr: { resolve: { dedupe: [\"shared\"] } } }, ssr: { noExternal: true }, build: { minify: false } };\n",
            ),
            (
                "src/entry.js",
                "import { v } from \"consumer\";\nexport const render = () => v;\n",
            ),
            (
                "node_modules/consumer/index.js",
                "import { copy } from \"shared\";\nexport const v = copy;\n",
            ),
        ],
    );
    let built = built_text(
        app.path(),
        &fixture,
        &["build", "--ssr", "src/entry.js"],
        &["js", "mjs"],
    );
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("root-copy"), "{tool}:\n{code}");
        assert!(
            !code.contains("nested-copy"),
            "{tool} bundled the copy under consumer:\n{code}"
        );
    }
}
