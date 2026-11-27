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
//! case from the fixture's Vite (e2e/fixtures/start-app's install, whose
//! Vite and rolldown are pinned to the rolldown oj embeds: an older
//! rolldown prints other helpers and comments), or from the one in the
//! node_modules directory `OJ_GOLDEN_VITE` names. Every other package
//! (React, the Start plugin, Tailwind, the app's plugins) comes from the
//! fixture's install on both sides either way, so a case can lean on them
//! without the two builds seeing two versions. The recorded output names
//! its Vite in `expected/VITE_VERSION`. The fixture's install is needed
//! either way (oj builds against it); without it the tests skip, unless
//! `OJ_REQUIRE_FIXTURES`. Commit the re-recorded files with the Vite
//! version bump they come from.
//!
//! The staged root's path is written as `/ROOT` in the compared text, so a
//! bundle naming the root (a Start server's module keys) compares across
//! the two temporary directories; a path into the fixture's install (a
//! source map's `sources` reach a package through the node_modules link,
//! resolved to where it is) is written from `/node_modules/` on, so the
//! recording holds on another machine.

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
    "html-multi",
    "json",
    "dynamic-vars",
    "base-relative",
    "base-path",
    "define",
    "typescript",
    "modulepreload-off",
    "lib-umd",
    "react-app",
    "tailwind",
    "alias-paths",
    "glob-variants",
    "code-splitting",
    "plugins",
    "plugins-bundle-assets",
    "sourcemap-esbuild",
    "start-app",
];

/// The part of a case's output that is compared, when not all of it: oj's
/// build of a TanStack Start app for a Node server writes a server of its
/// own (`server.mjs`, `worker.mjs` and their chunks), where `vite build`
/// writes Start's `server/server.js`, so a Start case compares the client.
fn compared_scope(case: &str) -> Option<&'static str> {
    match case {
        "start-app" => Some("client/"),
        _ => None,
    }
}

/// `tree` cut to the case's [`compared_scope`].
fn scoped(case: &str, mut tree: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    if let Some(prefix) = compared_scope(case) {
        tree.retain(|path, _| path.starts_with(prefix));
    }
    tree
}

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
/// the path and, for text, in the content, where the staged `root` reads
/// `/ROOT`.
fn tree(dir: &Path, root: &Path) -> BTreeMap<String, Vec<u8>> {
    let root_text = root.to_string_lossy().into_owned();
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
                Ok(text) => {
                    normalize_hashes(&normalize_module_paths(&text.replace(&root_text, "/ROOT")))
                        .into_bytes()
                }
                Err(_) => bytes,
            };
            out.insert(normalize_hashes(&rel), bytes);
        }
    }
    out
}

/// A path into the fixture's install, however it was reached (an absolute
/// path, or `../../..` from the output directory to the install's real
/// location), from `/node_modules/` on.
fn normalize_module_paths(text: &str) -> String {
    const MARKER: &str = "e2e/fixtures/start-app/node_modules/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARKER) {
        let head = &rest[..at];
        // The path's start: back over the characters a path is made of.
        let cut = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
            .last()
            .map_or(head.len(), |(i, _)| i);
        out.push_str(&head[..cut]);
        out.push_str("/node_modules/");
        rest = &rest[at + MARKER.len()..];
    }
    out.push_str(rest);
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

/// A node_modules for the recording: every package of the fixture's install
/// by a symlink, with `vite` the one under `vite_modules` (its own
/// dependencies resolve from there, as Node follows the link). The app's
/// plugins import `vite` from here, so they see the Vite that builds.
fn recording_modules(modules: &Path, vite_modules: &Path, into: &Path) -> PathBuf {
    let dir = into.join("node_modules");
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(modules).unwrap().flatten() {
        let name = entry.file_name();
        if name == "vite" || name == ".bin" {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), dir.join(&name)).unwrap();
    }
    std::os::unix::fs::symlink(vite_modules.join("vite"), dir.join("vite")).unwrap();
    dir
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
    let overlay = tempfile::tempdir().unwrap();
    let recording = if vite_modules == modules {
        modules.to_path_buf()
    } else {
        recording_modules(modules, &vite_modules, overlay.path())
    };
    let staged = stage(case, &recording);
    let root = staged.path().canonicalize().unwrap();
    run(Command::new("node")
        .arg(recording.join("vite/bin/vite.js"))
        .args(["build", "--outDir", "dist"])
        .current_dir(&root));
    let expected = golden_dir().join(case).join("expected");
    let _ = std::fs::remove_dir_all(&expected);
    for (rel, bytes) in scoped(case, tree(&root.join("dist"), &root)) {
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
    let mut expected = scoped(
        case,
        tree(&expected_dir, Path::new("/nonexistent-golden-root")),
    );
    expected.remove("VITE_VERSION");
    let staged = stage(case, &modules);
    let root = staged.path().canonicalize().unwrap();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "dist"])
        .arg(&root)
        .current_dir(&root));
    let actual = scoped(case, tree(&root.join("dist"), &root));

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
    source_maps_of_a_destructured_lazy_import => "sourcemap-destructured",
    source_maps_of_esbuild_minified_chunks => "sourcemap-esbuild",
    minified_by_esbuild => "minify-esbuild",
    two_html_entries_sharing_a_module_and_a_sheet => "html-multi",
    json_default_named_and_raw_imports => "json",
    dynamic_import_with_a_variable => "dynamic-vars",
    relative_base => "base-relative",
    base_path_with_assets_never_inlined => "base-path",
    define_and_env_replacements => "define",
    typescript_enums_and_a_jsx_pragma => "typescript",
    module_preload_off => "modulepreload-off",
    library_umd_and_iife => "lib-umd",
    react_with_svgr_mdx_and_a_lazy_component => "react-app",
    tailwind_through_postcss_config => "tailwind",
    regex_alias_tsconfig_paths_and_package_imports => "alias-paths",
    import_meta_glob_forms => "glob-variants",
    chunk_groups_and_the_build_block_of_a_large_app => "code-splitting",
    an_apps_own_plugins_hooks => "plugins",
}

/// A TanStack Start app's client bundle. The HTML, the manifest and every
/// chunk's code match; the maps differ in a few segments: the column a
/// server function's `createClientRpc` call maps to (oj's line map of the
/// rewrite against Start's compiler's), a JSX attribute's value a column
/// apart, two tokens of a `.js` module the lowering re-prints, of some
/// 78,000. A known gap.
#[test]
#[ignore = "a few segments of the client maps differ: a server function call's column, a JSX attribute's, two tokens of a lowered .js module"]
fn tanstack_start_client() {
    check("start-app");
}

/// The bundle a plugin's generateBundle and writeBundle see: the chunks'
/// stylesheets as assets, named as Vite's css-post names them, a chunk's
/// `viteMetadata.importedCss`, an asset emitted in buildStart, and the
/// page. All but the page match: oj's plain build renders its pages after
/// rolldown wrote the bundle, where Vite emits a page in a post plugin's
/// generateBundle, so writeBundle's bundle has no `index.html`. A known
/// gap.
#[test]
#[ignore = "a plain build renders its pages after the bundle hooks, so writeBundle's bundle lists no index.html"]
fn a_hooks_bundle_lists_sheets_pages_and_early_assets() {
    check("plugins-bundle-assets");
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
