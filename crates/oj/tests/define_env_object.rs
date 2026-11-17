// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` of an app whose config defines `import.meta.env.VITE_X` (no
//! `.env` entry) and whose code reads the key off the `import.meta.env`
//! object rather than by dotted access: the value is in the output, as under
//! Vite, whose define plugin folds the user's env defines into the object it
//! replaces `import.meta.env` with (plugins/define.ts `definePlugin`). The
//! app has no node_modules: the config is read directly.

use std::path::Path;
use std::process::Command;

const CONFIG: &str = "export default { define: { \"import.meta.env.VITE_API_URL\": JSON.stringify(\"https://api.example\") } };\n";

const MAIN: &str = "const env = import.meta.env;\n\
const copy = { ...import.meta.env };\n\
document.body.textContent = [env.VITE_API_URL, copy.VITE_API_URL].join(\" \");\n";

fn app(root: &Path) {
    for (file, body) in [
        ("package.json", r#"{"name":"define-env","type":"module"}"#),
        (
            "index.html",
            "<!doctype html><html><body><script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        ),
        ("vite.config.mjs", CONFIG),
        ("src/main.js", MAIN),
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

#[test]
fn a_defined_env_key_is_read_off_the_import_meta_env_object() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    app(&root);
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .current_dir(&root)
        .output()
        .unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "oj build failed:\n{log}");

    let mut js = String::new();
    for entry in std::fs::read_dir(root.join("dist/assets"))
        .unwrap()
        .flatten()
    {
        if entry.path().extension().is_some_and(|e| e == "js") {
            js.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    assert!(
        js.contains("https://api.example"),
        "the defined key is missing from the built import.meta.env object:\n{js}"
    );
}
