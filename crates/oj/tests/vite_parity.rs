// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` writes the same files as `vite build`, byte for byte: each app
//! here is built by the real Vite (8.3.5, from e2e/fixtures/stylex-app's
//! install, whose react/vite/plugin-react they share) and by oj, and every
//! page, JS chunk, stylesheet and asset must be identical under the same name.
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

/// Builds `root` with Vite and with oj and asserts every file is the same
/// file, with the same bytes.
fn assert_same_output(root: &Path) {
    run(Command::new("node")
        .arg(root.join("node_modules/vite/bin/vite.js"))
        .args(["build", "--outDir", "vite-dist"])
        .current_dir(root));
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "oj-dist"])
        .arg(root)
        .current_dir(root));
    let vite = files(&root.join("vite-dist"));
    let oj = files(&root.join("oj-dist"));
    assert_eq!(
        oj.keys().collect::<Vec<_>>(),
        vite.keys().collect::<Vec<_>>(),
        "the same files, named the same"
    );
    for name in vite.keys() {
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

/// React through `@vitejs/plugin-react`: a lazy route, state, JSON, an env
/// variable and an image, the shape of a small React app.
#[test]
fn a_react_app_builds_vites_exact_files() {
    let Some(root) = app(
        "react",
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            (
                "vite.config.js",
                Some("import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()] };\n"),
            ),
            (
                "index.html",
                Some(r#"<!doctype html><html><head><title>r</title></head><body><div id="root"></div><script type="module" src="/src/main.jsx"></script></body></html>"#),
            ),
            (".env", Some("VITE_GREETING=hello\n")),
            (
                "src/main.jsx",
                Some(
                    "import { createRoot } from \"react-dom/client\";\nimport { useState, lazy, Suspense } from \"react\";\n\
                     import data from \"./data.json\";\nimport { shared } from \"./shared.js\";\nimport logo from \"./logo.png\";\nimport \"./app.css\";\n\
                     const Lazy = lazy(() => import(\"./Lazy.jsx\"));\n\
                     function App() {\n  const [n, setN] = useState(0);\n  return (\n    <main>\n      <img src={logo} alt=\"\" />\n\
                     \x20     <h1>{import.meta.env.VITE_GREETING} {import.meta.env.MODE} {data.name}</h1>\n\
                     \x20     <button onClick={() => setN(n + 1)}>{shared(n)}</button>\n\
                     \x20     <Suspense fallback={null}><Lazy /></Suspense>\n    </main>\n  );\n}\n\
                     createRoot(document.getElementById(\"root\")).render(<App />);\n",
                ),
            ),
            (
                "src/Lazy.jsx",
                Some("import { shared } from \"./shared.js\";\nexport default function Lazy() { return <p>{shared(41)}</p>; }\n"),
            ),
            ("src/shared.js", Some("export const shared = (n) => `count ${n + 1}`;\n")),
            ("src/data.json", Some("{\"name\":\"corpus\",\"unused\":1}\n")),
            ("src/app.css", Some("h1 { color: rebeccapurple; }\n")),
            ("src/logo.png", None),
        ],
    ) else {
        return;
    };
    assert_same_output(&root);
}

/// A formatted page: an inline module script, a linked stylesheet and an
/// indented `<head>` the tags are written into, one step deeper.
#[test]
fn a_formatted_page_builds_vites_exact_files() {
    let Some(root) = app(
        "formatted",
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            ("vite.config.js", Some("export default {};\n")),
            (
                "index.html",
                Some(
                    "<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"UTF-8\" />\n\
                     \x20   <link rel=\"stylesheet\" href=\"/src/linked.css\" />\n    <title>f</title>\n  </head>\n\
                     \x20 <body>\n    <div id=\"app\"></div>\n    <script type=\"module\">\n\
                     \x20     import { shared } from \"./src/shared.js\";\n      document.title = shared(2);\n    </script>\n\
                     \x20   <script type=\"module\" src=\"/src/main.js\"></script>\n  </body>\n</html>\n",
                ),
            ),
            (
                "src/main.js",
                Some("import { shared } from \"./shared.js\";\nimport \"./main.css\";\nconsole.log(shared(1));\nimport(\"./lazy.js\");\n"),
            ),
            ("src/shared.js", Some("export const shared = (n) => `s${n}`;\n")),
            ("src/lazy.js", Some("export const lazy = 1;\n")),
            ("src/main.css", Some("body { margin: 0; }\n")),
            ("src/linked.css", Some(".linked { color: red; }\n")),
        ],
    ) else {
        return;
    };
    assert_same_output(&root);
}

/// Pages under a relative base: a nested page (`../` hrefs), a page of only
/// a stylesheet link, a `media` stylesheet (a plain asset) and an image.
#[test]
fn relative_base_pages_build_vites_exact_files() {
    let Some(root) = app(
        "pages",
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            (
                "vite.config.js",
                Some("export default { base: \"./\", build: { rollupOptions: { input: { main: \"index.html\", deep: \"a/b/index.html\", styles: \"s.html\" } } } };\n"),
            ),
            (
                "index.html",
                Some("<!doctype html><html><head>\n  <link rel=\"stylesheet\" href=\"/src/print.css\" media=\"print\">\n</head><body><img src=\"/src/logo.png\"><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n"),
            ),
            (
                "a/b/index.html",
                Some("<html><head><title>d</title></head><body><script type=\"module\" src=\"../../src/deep.js\"></script></body></html>\n"),
            ),
            (
                "s.html",
                Some("<html><head><link rel=\"stylesheet\" href=\"./src/s.css\"></head><body></body></html>\n"),
            ),
            ("src/main.js", Some("import { s } from \"./shared.js\";\nimport \"./m.css\";\nconsole.log(s);\n")),
            ("src/deep.js", Some("import { s } from \"./shared.js\";\nimport \"./d.css\";\nconsole.log(s, 2);\n")),
            ("src/shared.js", Some("import \"./sh.css\";\nexport const s = 1;\n")),
            ("src/m.css", Some(".m { color: red; }\n")),
            ("src/d.css", Some(".d { color: red; }\n")),
            ("src/sh.css", Some(".sh { color: red; }\n")),
            ("src/s.css", Some(".s { color: red; }\n")),
            ("src/print.css", Some(".p { color: red; }\n")),
            ("src/logo.png", None),
        ],
    ) else {
        return;
    };
    assert_same_output(&root);
}

/// One stylesheet for the whole build, and an `async` entry on a page with
/// no `<head>` (its tags go before `<body>`).
#[test]
fn without_css_code_split_vites_exact_files() {
    let Some(root) = app(
        "no-split",
        &[
            ("package.json", Some(r#"{"name":"parity","type":"module"}"#)),
            (
                "vite.config.js",
                Some("export default { build: { cssCodeSplit: false } };\n"),
            ),
            (
                "index.html",
                Some("<body><script async type=\"module\" src=\"/src/main.js\"></script></body>\n"),
            ),
            (
                "src/main.js",
                Some("import \"./a.css\";\nimport(\"./lazy.js\");\n"),
            ),
            (
                "src/lazy.js",
                Some("import \"./b.css\";\nexport default 1;\n"),
            ),
            ("src/a.css", Some("a { color: red; }\n")),
            ("src/b.css", Some("b { color: blue; }\n")),
        ],
    ) else {
        return;
    };
    assert_same_output(&root);
}
