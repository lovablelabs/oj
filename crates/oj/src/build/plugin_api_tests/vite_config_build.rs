// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::Path;

use crate::build::{build, CliOptions};

use super::{scratch, write_files};

/// A function-form `vite.config.js` branching on `command` / `mode`.
const CONFIG: &str = r#"export default ({ command, mode }) => ({
  define: { __CMD__: JSON.stringify(command), __MODE__: JSON.stringify(mode) },
  build: { outDir: command === "build" ? "out-" + mode : "out-serve", sourcemap: true },
});
"#;

/// The `main-*.js` chunk under `<out_dir>/assets`, after checking the page
/// and the chunk's `.map` (build.sourcemap) were written.
fn built_main(root: &Path, out_dir: &str) -> String {
    let out = root.join(out_dir);
    assert!(
        out.join("index.html").exists(),
        "{out_dir}/index.html written"
    );
    let assets = out.join("assets");
    let names: Vec<String> = fs::read_dir(&assets)
        .unwrap_or_else(|e| panic!("{out_dir}/assets exists: {e}"))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let js = names
        .iter()
        .find(|f| f.starts_with("main-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("{out_dir}: main chunk emitted: {names:?}"));
    assert!(
        assets.join(format!("{js}.map")).exists(),
        "{out_dir}: build.sourcemap honored: {names:?}"
    );
    fs::read_to_string(assets.join(js)).unwrap()
}

/// `oj build` evaluates a function-form `vite.config` as Vite's build does,
/// with `command: "build"` and `mode` = `production` (or `--mode`), never as
/// `serve`/`development`, and honors the `build.outDir` / `build.sourcemap`
/// it returns. The defines it returns see the same command and mode. (The
/// loader's command/mode arguments are pinned on `oj.config` files by
/// `function_config_receives_command_and_mode`; this pins the vite.config
/// adoption path through a real build.)
#[tokio::test]
async fn function_vite_config_is_evaluated_as_build_for_each_mode() {
    let root = scratch("vitecfg-build");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"vitecfg-app","version":"1.0.0"}"#,
            ),
            ("vite.config.js", CONFIG),
            (
                "src/main.js",
                "window.__CMD = __CMD__; window.__MODE = __MODE__;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    let quoted = |word: &str| regex::Regex::new(&format!(r#"[`"]{word}[`"]"#)).unwrap();

    build(root.clone(), None, CliOptions::default())
        .await
        .expect("the default-mode build succeeds");
    assert!(
        !root.join("out-serve").exists(),
        "the config was evaluated as serve"
    );
    assert!(
        !root.join("dist").exists(),
        "build.outDir ignored (fell back to dist)"
    );
    let prod = built_main(&root, "out-production");
    assert!(
        quoted("build").is_match(&prod),
        "define saw command=build:\n{prod}"
    );
    assert!(
        quoted("production").is_match(&prod),
        "define saw mode=production:\n{prod}"
    );

    let _ = fs::remove_dir_all(root.join(".oj-cache"));
    build(root.clone(), Some("staging"), CliOptions::default())
        .await
        .expect("the --mode staging build succeeds");
    let staging = built_main(&root, "out-staging");
    assert!(
        quoted("staging").is_match(&staging),
        "define saw mode=staging:\n{staging}"
    );
}
