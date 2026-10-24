//! `fru compile <dir>` walks a project and emits the assembled sheet.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fru")
}

fn fixture(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fru-compile-fixture-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::create_dir_all(dir.join("node_modules/junk")).expect("mkdir nm");
    std::fs::write(dir.join("package.json"), r#"{"name":"fx"}"#).expect("pkg");
    std::fs::write(
        dir.join("src/tokens.stylex.ts"),
        "import * as stylex from '@stylexjs/stylex';\n\
         export const colors = stylex.defineVars({ accent: 'rebeccapurple' });\n",
    )
    .expect("tokens");
    std::fs::write(
        dir.join("src/app.tsx"),
        "import * as stylex from '@stylexjs/stylex';\n\
         import { colors } from './tokens.stylex';\n\
         export const s = stylex.create({ a: { color: colors.accent } });\n",
    )
    .expect("app");
    // Must be skipped by the walk despite containing a stylex import.
    std::fs::write(
        dir.join("node_modules/junk/x.ts"),
        "import * as stylex from '@stylexjs/stylex';\nsyntax error here(",
    )
    .expect("junk");
    dir
}

#[test]
fn walks_compiles_and_assembles() {
    let dir = fixture("walk");
    let out = Command::new(bin())
        .args(["compile", dir.to_str().unwrap(), "--threads", "2"])
        .output()
        .expect("run fru compile");
    let css = String::from_utf8(out.stdout).expect("utf8 css");
    let summary = String::from_utf8(out.stderr).expect("utf8 summary");
    assert!(out.status.success(), "exit: {summary}");
    assert!(css.contains("rebeccapurple"), "css: {css}");
    assert!(
        css.contains("color:var(--"),
        "cross-file var missing: {css}"
    );
    assert!(summary.contains("\"errors\":0"), "summary: {summary}");
    // node_modules was skipped, so its intentional syntax error never surfaced.
    assert!(!summary.contains("junk"), "walked node_modules: {summary}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn out_flag_writes_the_sheet() {
    let dir = fixture("out");
    let css_path = dir.join("styles.css");
    let out = Command::new(bin())
        .args([
            "compile",
            dir.to_str().unwrap(),
            "--out",
            css_path.to_str().unwrap(),
        ])
        .output()
        .expect("run fru compile");
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "css must not also hit stdout");
    let css = std::fs::read_to_string(&css_path).expect("sheet written");
    assert!(css.contains("rebeccapurple"));
    let _ = std::fs::remove_dir_all(&dir);
}
