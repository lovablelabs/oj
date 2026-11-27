// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Golden tests for `oj build`: small apps under `tests/golden/<case>/app`,
//! each built by oj and compared, file by file, with the `vite build` output
//! recorded under `tests/golden/<case>/expected`. Content hashes in file
//! names and in the text that names them are normalized to `HASH`, so a
//! golden holds through a hash change and fails on anything else: a chunk
//! split, a line of the preload helper, a manifest row, a stylesheet's
//! order, an inlined asset's data URL.
//!
//! `OJ_UPDATE_GOLDEN=1 cargo test --test build_golden` re-records every
//! case from Vite: the one in the node_modules directory `OJ_GOLDEN_VITE`
//! names, else the fixture's (e2e/fixtures/start-app's install). The
//! recorded output names its version in `expected/VITE_VERSION`; record
//! from the Vite whose rolldown is the one oj embeds (Cargo.lock), since an
//! older rolldown prints other helpers and comments. The fixture's install
//! is needed either way (oj builds against it); without it the tests skip,
//! unless `OJ_REQUIRE_FIXTURES`. Commit the re-recorded files with the
//! Vite version bump they come from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const CASES: &[&str] = &[
    "basic",
    "css",
    "assets",
    "asset-url",
    "worker",
    "worker-es",
    "manifest",
    "glob-env",
    "lib",
    "sourcemap",
    "sourcemap-destructured",
    "minify-esbuild",
];

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

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

/// The case's app copied into a fresh root (dotfiles included: `.env`),
/// with the fixture's node_modules beside it.
fn stage(case: &str, modules: &Path) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    copy_tree(&golden_dir().join(case).join("app"), &root);
    std::os::unix::fs::symlink(modules, root.join("node_modules")).unwrap();
    tmp
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

/// Every file under `dir` by its path relative to it, hashes normalized in
/// the path and, for text, in the content.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(&path).unwrap();
            let bytes = match std::str::from_utf8(&bytes) {
                Ok(text) => normalize_hashes(text).into_bytes(),
                Err(_) => bytes,
            };
            out.insert(normalize_hashes(&rel), bytes);
        }
    }
    out
}

/// `name-Ab12_cD9.ext` → `name-HASH.ext`: Vite's eight base64url characters
/// before an extension, in a file name or a reference to one.
fn normalize_hashes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && i + 9 < bytes.len() && bytes[i + 9] == b'.' {
            let hash = &bytes[i + 1..i + 9];
            let is_hash = hash
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-');
            let ext_len = bytes[i + 10..]
                .iter()
                .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                .count();
            if is_hash && (1..=5).contains(&ext_len) {
                out.push_str("-HASH");
                i += 9;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn vite_version(modules: &Path) -> String {
    let package: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(modules.join("vite/package.json")).unwrap())
            .unwrap();
    package["version"].as_str().unwrap().to_string()
}

/// Records `vite build`'s output as the case's expected tree, with the
/// Vite `OJ_GOLDEN_VITE` names (a node_modules directory) or the fixture's.
fn record(case: &str, modules: &Path) {
    let vite_modules = std::env::var_os("OJ_GOLDEN_VITE")
        .map(|dir| {
            PathBuf::from(dir)
                .canonicalize()
                .expect("OJ_GOLDEN_VITE exists")
        })
        .unwrap_or_else(|| modules.to_path_buf());
    assert!(
        vite_modules.join("vite/package.json").is_file(),
        "no vite under {}",
        vite_modules.display()
    );
    let staged = stage(case, &vite_modules);
    let root = staged.path().canonicalize().unwrap();
    run(Command::new("node")
        .arg(vite_modules.join("vite/bin/vite.js"))
        .args(["build", "--outDir", "dist"])
        .current_dir(&root));
    let expected = golden_dir().join(case).join("expected");
    let _ = std::fs::remove_dir_all(&expected);
    for (rel, bytes) in tree(&root.join("dist")) {
        let dest = expected.join(&rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, bytes).unwrap();
    }
    std::fs::write(
        expected.join("VITE_VERSION"),
        format!("{}\n", vite_version(&vite_modules)),
    )
    .unwrap();
}

fn check(case: &str) {
    let Some(modules) = node_modules() else {
        return;
    };
    if std::env::var_os("OJ_UPDATE_GOLDEN").is_some() {
        record(case, &modules);
        return;
    }
    let expected_dir = golden_dir().join(case).join("expected");
    assert!(
        expected_dir.is_dir(),
        "no recorded output for `{case}`: OJ_UPDATE_GOLDEN=1 cargo test --test build_golden"
    );
    let mut expected = tree(&expected_dir);
    expected.remove("VITE_VERSION");
    let staged = stage(case, &modules);
    let root = staged.path().canonicalize().unwrap();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "dist"])
        .arg(&root)
        .current_dir(&root));
    let actual = tree(&root.join("dist"));

    let mut report = String::new();
    for name in expected.keys().filter(|k| !actual.contains_key(*k)) {
        report.push_str(&format!("  missing: {name}\n"));
    }
    for name in actual.keys().filter(|k| !expected.contains_key(*k)) {
        report.push_str(&format!("  extra:   {name}\n"));
    }
    for (name, want) in &expected {
        let Some(got) = actual.get(name) else {
            continue;
        };
        if got != want {
            report.push_str(&format!(
                "  differs: {name}\n{}",
                first_difference(want, got)
            ));
        }
    }
    assert!(
        report.is_empty(),
        "`oj build` of golden/{case} differs from the recorded `vite build`:\n{report}"
    );
}

/// The first difference of two texts: its line, and the text around the
/// first differing character of that line (a minified line is one line);
/// the sizes for binary content.
fn first_difference(want: &[u8], got: &[u8]) -> String {
    let (Ok(want), Ok(got)) = (std::str::from_utf8(want), std::str::from_utf8(got)) else {
        return format!(
            "    binary: {} bytes expected, {} bytes built\n",
            want.len(),
            got.len()
        );
    };
    let w: Vec<&str> = want.lines().collect();
    let g: Vec<&str> = got.lines().collect();
    let line = w.iter().zip(g.iter()).take_while(|(a, b)| a == b).count();
    let (a, b) = (
        w.get(line).copied().unwrap_or("<end>"),
        g.get(line).copied().unwrap_or("<end>"),
    );
    let col = a
        .char_indices()
        .zip(b.char_indices())
        .find(|((_, x), (_, y))| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i);
    let window = |s: &str| {
        let start = s[..col.min(s.len())]
            .char_indices()
            .rev()
            .nth(120)
            .map_or(0, |(i, _)| i);
        let end = s[col.min(s.len())..]
            .char_indices()
            .nth(160)
            .map_or(s.len(), |(i, _)| col + i);
        format!(
            "{}{}{}",
            if start > 0 { "…" } else { "" },
            &s[start..end],
            if end < s.len() { "…" } else { "" }
        )
    };
    format!(
        "    line {}, column {}:\n    - {}\n    + {}\n",
        line + 1,
        col + 1,
        window(a),
        window(b)
    )
}

macro_rules! golden {
    ($($name:ident => $case:literal),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                check($case);
            }
        )*
    };
}

golden! {
    basic_entry_with_a_lazy_import => "basic",
    css_global_module_and_lazy_sheet => "css",
    assets_inlined_emitted_and_public => "assets",
    workers_module_inline_and_url => "worker",
    workers_as_modules => "worker-es",
    manifest_with_css_and_lazy_chunk => "manifest",
    import_meta_glob_and_env => "glob-env",
    library_es_and_cjs => "lib",
    source_maps_of_chunks_sheets_and_assets => "sourcemap",
    minified_by_esbuild => "minify-esbuild",
}

/// `new URL("./file", import.meta.url)`: Vite's asset-import-meta-url plugin
/// rewrites the call in place to `new URL("/assets/file-HASH.ext", "" +
/// import.meta.url)` (a `__VITE_ASSET__` placeholder resolved as the chunk
/// renders); oj imports the file as an asset module and reads its URL from
/// the module's binding, which hoists a `var` and reorders the chunk's
/// declarations. A known gap, recorded here until the rewrite is in place.
#[test]
#[ignore = "new URL(file, import.meta.url) is an imported asset module in oj, not Vite's in-place rewrite"]
fn asset_url_and_worker_url_from_import_meta_url() {
    check("asset-url");
}

/// `const { lazy } = await import("./lazy.js")` with `build.sourcemap`: the
/// code matches, and so does the map but for the segments inside the preload
/// wrapper's repeated pattern, which Vite's text rewrite places one column
/// apart from oj's AST rewrite. A known difference, recorded here until the
/// wrapper's spans are shaped to match.
#[test]
#[ignore = "the destructured import's inner pattern maps a column apart from Vite's rewrite"]
fn source_maps_of_a_destructured_lazy_import() {
    check("sourcemap-destructured");
}

#[test]
fn every_case_directory_is_listed() {
    let mut on_disk: Vec<String> = std::fs::read_dir(golden_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = CASES.iter().map(|c| c.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed, "every tests/golden/<case> has a test");
}

#[test]
fn hashes_normalize_in_names_and_references() {
    assert_eq!(
        normalize_hashes("assets/index-BxYz12_a.js"),
        "assets/index-HASH.js"
    );
    assert_eq!(
        normalize_hashes("import(\"./lazy-Cd3f4Gh5.js\");url(/assets/dot-Ab12-cD9.svg)"),
        "import(\"./lazy-HASH.js\");url(/assets/dot-HASH.svg)"
    );
    assert_eq!(normalize_hashes("golden-lib.js"), "golden-lib.js");
    assert_eq!(
        normalize_hashes("a-short.js b-12345678x.js"),
        "a-short.js b-12345678x.js"
    );
}
