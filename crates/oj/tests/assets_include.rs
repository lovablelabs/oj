// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's `assetsInclude`, through the `oj` binary, for an app with no Vite
//! installed (the config is read directly):
//!
//! - however the root is named (the directory oj starts in, an argument,
//!   relative or through a symlink), the app's own filter is the one
//!   consulted;
//! - a glob is read as Vite's `createFilter(config.assetsInclude)` reads it:
//!   `**/*.gltf` and a RegExp name a file wherever oj was started, a relative
//!   glob (`models/*.obj`) is resolved against the directory oj was started
//!   in (`process.cwd()` for Vite), so it names the app's files only when
//!   that is the app's root;
//! - `oj build` loads a named file as an asset, as vite:asset's load does
//!   (`config.assetsInclude(cleanUrl(id))`), inlined under the limit and
//!   emitted over it.

#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::process::Command;

use start_app::Server;

const CONFIG: &str =
    "export default { assetsInclude: [\"**/*.gltf\", \"models/*.obj\", /\\.bin$/] };\n";

/// `dir/app`: the app, with a file of each named type; `dir/app-link`: a
/// symlink to it. No node_modules.
fn app(dir: &Path) -> PathBuf {
    let root = dir.join("app");
    for (file, body) in [
        ("package.json", r#"{"name":"assets-include","type":"module"}"#.to_string()),
        (
            "index.html",
            "<!doctype html><html><body><script type=\"module\" src=\"/src/main.js\"></script></body></html>"
                .to_string(),
        ),
        ("vite.config.mjs", CONFIG.to_string()),
        ("src/scene.gltf", r#"{"asset":{"version":"2.0"}}"#.to_string()),
        ("src/data.bin", "bin-bytes".to_string()),
        ("models/x.obj", "obj-bytes".to_string()),
        // Over Vite's 4096-byte inline limit: emitted as a file by a build.
        ("src/big.gltf", format!("{{\"pad\":\"{}\"}}", "x".repeat(6000))),
        (
            "src/main.js",
            "import scene from \"./scene.gltf\";\nimport big from \"./big.gltf\";\nimport data from \"./data.bin\";\nimport obj from \"../models/x.obj\";\ndocument.title = [scene, big, data, obj].join();\n"
                .to_string(),
        ),
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    std::os::unix::fs::symlink(&root, dir.join("app-link")).unwrap();
    root
}

/// Whether `dev` answers a module import of `path` with a URL module.
fn is_url_module(dev: &Server, path: &str) -> bool {
    let (status, body) = dev.request("GET", path, &[("sec-fetch-dest", "script".to_string())]);
    assert_eq!(status, 200, "GET {path}: {body}\n{}", dev.output());
    body.contains(&format!("export default \"{path}\""))
}

/// `oj dev` started in `cwd` with `args`: the `**` glob and the RegExp name
/// their files in every case; the relative glob names its file only when oj
/// was started in the app's root (`relative`).
fn dev_names_the_files(what: &str, cwd: &Path, args: &[&str], relative: bool) {
    let dev = Server::oj_dev_in(cwd, args);
    for path in ["/src/scene.gltf", "/src/data.bin"] {
        assert!(
            is_url_module(&dev, path),
            "{what}: {path} is not a URL module\n{}",
            dev.output()
        );
    }
    assert_eq!(
        is_url_module(&dev, "/models/x.obj"),
        relative,
        "{what}: a relative glob resolves against the directory oj was started in\n{}",
        dev.output()
    );
}

#[test]
fn dev_consults_the_apps_filter_however_its_root_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let root = app(&dir);
    let link = dir.join("app-link");

    dev_names_the_files("started in the root", &root, &[], true);
    dev_names_the_files("started in the root through a symlink", &link, &[], true);
    dev_names_the_files("a relative root from the parent", &dir, &["./app"], false);
    dev_names_the_files(
        "a symlinked root from the parent",
        &dir,
        &["./app-link"],
        false,
    );
    dev_names_the_files(
        "an absolute symlinked root from elsewhere",
        Path::new("/"),
        &[&link.to_string_lossy()],
        false,
    );
    // The root named through the symlink, oj started in the real directory:
    // one app under two spellings, and the relative glob is the root's.
    dev_names_the_files(
        "a symlinked root, started in the real one",
        &root,
        &[&link.to_string_lossy()],
        true,
    );
}

// vite:asset's load admits a file `config.assetsInclude` names. The plain
// build asked the known types only: a named `.gltf` reached the bundler as a
// script ("Expected a semicolon ... src/scene.gltf").
#[test]
fn build_loads_a_named_file_as_an_asset() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let root = app(&dir);
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

    let assets = root.join("dist/assets");
    let names: Vec<String> = std::fs::read_dir(&assets)
        .unwrap_or_else(|e| panic!("no dist/assets ({e}):\n{log}"))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    // Over the inline limit: the file itself, hashed, with its bytes.
    let big = names
        .iter()
        .find(|n| n.starts_with("big-") && n.ends_with(".gltf"))
        .unwrap_or_else(|| panic!("big.gltf was not emitted: {names:?}\n{log}"));
    assert_eq!(
        std::fs::read(assets.join(big)).unwrap(),
        std::fs::read(root.join("src/big.gltf")).unwrap()
    );
    let script = names
        .iter()
        .find(|n| n.ends_with(".js"))
        .unwrap_or_else(|| panic!("no script: {names:?}\n{log}"));
    let code = std::fs::read_to_string(assets.join(script)).unwrap();
    assert!(
        code.contains(big.as_str()),
        "the script does not name {big}:\n{code}"
    );
    // Under it: inlined as data urls, one per file (the glob's, the
    // RegExp's and the relative glob's, the build having started in the root).
    for (what, bytes) in [
        ("scene.gltf", r#"{"asset":{"version":"2.0"}}"#),
        ("data.bin", "bin-bytes"),
        ("models/x.obj", "obj-bytes"),
    ] {
        let inlined = base64(bytes.as_bytes());
        assert!(
            code.contains(&inlined),
            "{what} is not inlined as its bytes ({inlined}):\n{code}"
        );
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
