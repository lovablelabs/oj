// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! pulp P1: with a `display_path`, a module's compiled artifact (code AND
//! sourcemap) must be byte-identical whatever worktree it was compiled in —
//! the property that makes the module cache shareable across worktrees.

use std::path::{Path, PathBuf};

use oj_compiler::{compile_module, CompileOptions};

const JSX_SOURCE: &str = r#"
export function App() {
  return <div className="x">hi</div>;
}
"#;

fn dev_opts(display: Option<&str>) -> CompileOptions {
    let mut opts = CompileOptions::dev();
    opts.display_path = display.map(PathBuf::from);
    opts
}

// Minimal base64 decode for data-URL sourcemaps (alphabet only; test input is
// well-formed by construction).
fn decode_map(data_url: &str) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b64 = data_url
        .rsplit_once("base64,")
        .expect("data url")
        .1
        .trim_end_matches('=');
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in b64.as_bytes() {
        let v = ABC.iter().position(|&a| a == c).expect("base64 char") as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    String::from_utf8(out).expect("utf8 map")
}

#[test]
fn dev_artifact_is_byte_identical_across_worktrees() {
    let opts = dev_opts(Some("/src/App.tsx"));
    let a = compile_module(Path::new("/work/wt-a/src/App.tsx"), JSX_SOURCE, &opts, None).unwrap();
    let b = compile_module(
        Path::new("/somewhere/else/wt-b/src/App.tsx"),
        JSX_SOURCE,
        &opts,
        None,
    )
    .unwrap();

    assert_eq!(a.code, b.code, "code must not depend on the real path");
    assert_eq!(
        a.map_data_url(),
        b.map_data_url(),
        "sourcemap must not depend on the real path"
    );
    assert!(
        !a.code.contains("/work/wt-a"),
        "no absolute path in code:\n{}",
        a.code
    );
    let map = decode_map(a.map_data_url().as_deref().unwrap());
    assert!(
        !map.contains("/work/wt-a"),
        "no absolute path in the map: {map}"
    );
}

#[test]
fn display_path_names_the_artifact_everywhere() {
    let out = compile_module(
        Path::new("/abs/checkout/src/App.tsx"),
        JSX_SOURCE,
        &dev_opts(Some("/src/App.tsx")),
        None,
    )
    .unwrap();

    // jsx dev __source carries the display name...
    assert!(
        out.code.contains("/src/App.tsx"),
        "__source fileName uses the display path:\n{}",
        out.code
    );
    // ...and so does the sourcemap, with content embedded so DevTools never
    // needs to resolve the relative name against the filesystem.
    let map = decode_map(out.map_data_url().as_deref().unwrap());
    assert!(map.contains("\"/src/App.tsx\""), "sources entry: {map}");
    assert!(
        map.contains("sourcesContent"),
        "content embedded so the name is a label: {map}"
    );
}

#[test]
fn without_display_path_the_real_path_still_names_the_artifact() {
    // The pre-pulp fallback: callers that pass no display identity keep the
    // absolute path, so nothing changes for them (and their artifacts remain
    // correctly non-shareable).
    let out = compile_module(
        Path::new("/abs/checkout/src/App.tsx"),
        JSX_SOURCE,
        &dev_opts(None),
        None,
    )
    .unwrap();
    assert!(out.code.contains("/abs/checkout/src/App.tsx"));
}

#[test]
fn prod_output_is_path_free_with_display_path() {
    let mut opts = CompileOptions::prod();
    opts.display_path = Some(PathBuf::from("/src/App.tsx"));
    let a = compile_module(Path::new("/wt-a/src/App.tsx"), JSX_SOURCE, &opts, None).unwrap();
    let b = compile_module(Path::new("/wt-b/src/App.tsx"), JSX_SOURCE, &opts, None).unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.map_data_url(), b.map_data_url());
    assert!(!a.code.contains("/wt-a"));
}

#[test]
fn refresh_boundary_detection_is_unchanged_by_display_path() {
    let with = compile_module(
        Path::new("/wt-a/src/App.tsx"),
        JSX_SOURCE,
        &dev_opts(Some("/src/App.tsx")),
        None,
    )
    .unwrap();
    let without = compile_module(
        Path::new("/wt-a/src/App.tsx"),
        JSX_SOURCE,
        &dev_opts(None),
        None,
    )
    .unwrap();
    assert_eq!(
        with.is_refresh_boundary, without.is_refresh_boundary,
        "display identity must not change refresh semantics"
    );
    assert!(with.is_refresh_boundary, "App is a refresh boundary");
}
