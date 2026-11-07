//! unstable_moduleResolution.themeFileExtension, pinned against
//! @stylexjs/babel-plugin 0.19.0 (conformance corpus regressions/tfe-*).

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, SourceCompileResult, transform_source};
use oj_stylex::errors::{ErrorCode, StylexError};
use oj_stylex::module_resolution::BoundedFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";
const VARS: &str = "import * as stylex from '@stylexjs/stylex';\nexport const vars = stylex.defineVars({ color: 'red' });\n";

/// Package name and relative paths match conformance's pkgA fixture, so every
/// hash below is the oracle's own output for the same canonical name.
fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("theme-file-extension-fixture");
    ONCE.call_once(|| {
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("fixture dirs");
        std::fs::write(root.join("package.json"), "{\"name\":\"tfe-a\"}\n").expect("pkg");
        for name in ["tokens.css.ts", "tokens.stylex.ts", "tokens.transformed.ts"] {
            std::fs::write(src.join(name), VARS).expect("theme fixture");
        }
        std::fs::write(
            src.join("tokens.css.const.ts"),
            format!("{IMP}export const cst = stylex.defineConsts({{ gutter: '16px' }});\n"),
        )
        .expect("consts fixture");
        std::fs::write(src.join("plain.css"), ".plain { color: red; }\n").expect("css fixture");
    });
    root
}

fn options(theme_file_extension: Option<&str>) -> ResolvedOptions {
    let root = fixture_root();
    let mut resolution = serde_json::json!({ "type": "commonJS", "rootDir": root });
    if let Some(ext) = theme_file_extension {
        resolution["themeFileExtension"] = serde_json::Value::String(ext.to_string());
    }
    CompilerOptions::from_json(&serde_json::json!({ "unstable_moduleResolution": resolution }))
        .expect("options parse")
        .resolve()
        .expect("options resolve")
}

fn compile_with(
    source: &str,
    filename: &str,
    theme_file_extension: Option<&str>,
) -> Result<SourceCompileResult, StylexError> {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let filename = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: &root,
    };
    transform_source(&ctx, &options(theme_file_extension), &fs)
        .map(|r| r.expect("pre-gate must pass for stylex sources"))
}

fn rules(source: &str, filename: &str, theme_file_extension: Option<&str>) -> Vec<String> {
    compile_with(source, filename, theme_file_extension)
        .expect("compile")
        .rules
        .into_iter()
        .map(|r| format!("{}|{}", r.class_name, r.ltr))
        .collect()
}

fn vars_rules(filename: &str, theme_file_extension: Option<&str>) -> Vec<String> {
    rules(VARS, filename, theme_file_extension)
}

fn hash_error(source: &str, filename: &str, theme_file_extension: Option<&str>) -> StylexError {
    let err = compile_with(source, filename, theme_file_extension).expect_err("must error");
    assert_eq!(err.code, ErrorCode::CannotGenerateHash, "{}", err.message);
    err
}

#[test]
fn configured_extension_gates_define_vars_both_ways() {
    assert_eq!(
        vars_rules("src/tokens.css.ts", Some(".css")),
        [":root, .xpbs572{--x1o3m372:red;}"].map(|css| format!("xpbs572|{css}"))
    );
    hash_error(VARS, "src/tokens.css.ts", None);
    hash_error(VARS, "src/tokens.stylex.ts", Some(".css"));
    assert_eq!(
        vars_rules("src/tokens.stylex.ts", None),
        ["x16s59xu|:root, .x16s59xu{--x1d10t7z:red;}"]
    );
}

#[test]
fn suffix_matches_bare_and_the_six_code_extensions() {
    assert_eq!(
        vars_rules("src/plain.css", Some(".css")),
        ["x1uxlvug|:root, .x1uxlvug{--x1onqde4:red;}"]
    );
    assert_eq!(
        vars_rules("src/tokens.css.cjs", Some(".css")),
        ["x1eo12tr|:root, .x1eo12tr{--xng7037:red;}"]
    );
    hash_error(VARS, "src/tokens.css.mts", Some(".css"));
}

#[test]
fn suffix_needs_no_leading_dot_and_has_no_word_boundary() {
    assert_eq!(
        vars_rules("src/default.cssvars.js", Some("cssvars")),
        ["xijaleb|:root, .xijaleb{--x1rd7qpm:red;}"]
    );
    assert_eq!(
        vars_rules("src/defaultcssvars.js", Some("cssvars")),
        ["x116dkp6|:root, .x116dkp6{--x1w5xexy:red;}"]
    );
    assert_eq!(
        vars_rules("src/theme/vars.ts", Some("/vars")),
        ["x67ryda|:root, .x67ryda{--x1g9ztt7:red;}"]
    );
    // The suffix must be terminal: a match in the middle is not a match.
    hash_error(VARS, "src/tokensXcssY.ts", Some(".css"));
}

#[test]
fn suffix_matching_is_case_sensitive() {
    hash_error(VARS, "src/tokens.CSS.ts", Some(".css"));
    assert_eq!(
        vars_rules("src/tokens.CSS.ts", Some(".CSS")),
        ["x79yb93|:root, .x79yb93{--xj6yq4l:red;}"]
    );
}

#[test]
fn empty_extension_makes_every_file_a_theme_file() {
    assert_eq!(
        vars_rules("src/whatever.ts", Some("")),
        ["xdpv3c|:root, .xdpv3c{--xk89fij:red;}"]
    );
}

#[test]
fn the_option_value_never_enters_the_hash() {
    let under_css = vars_rules("src/tokens.css.ts", Some(".css"));
    for ext in ["", "s", ".css.ts"] {
        assert_eq!(vars_rules("src/tokens.css.ts", Some(ext)), under_css);
    }
    // The canonical name keeps the extension verbatim, so a different file
    // under the same option is a different hash.
    assert_eq!(
        vars_rules("src/tokens.css.ts.js", Some(".css.ts")),
        ["x1ruzl1a|:root, .x1ruzl1a{--x1mz6czq:red;}"]
    );
}

#[test]
fn the_consts_extension_is_derived_from_the_option() {
    let consts = format!("{IMP}export const cst = stylex.defineConsts({{ gutter: '16px' }});\n");
    assert_eq!(
        rules(&consts, "src/tokens.css.const.ts", Some(".css")),
        ["xzaqpy3|"]
    );
    hash_error(&consts, "src/tokens.stylex.const.ts", Some(".css"));
    // A consts file is just a second way to unlock hashing, not a restriction.
    assert_eq!(
        vars_rules("src/tokens.css.const.ts", Some(".css")),
        ["xdql73a|:root, .xdql73a{--xtte67e:red;}"]
    );
}

#[test]
fn import_specifiers_are_gated_as_written() {
    let importing = |specifier: &str| {
        format!(
            "{IMP}import {{ vars }} from '{specifier}';\nexport const styles = stylex.create({{ root: {{ color: vars.color }} }});\n"
        )
    };
    assert_eq!(
        rules(&importing("./tokens.css"), "src/app.ts", Some(".css")),
        ["xvz48rq|.xvz48rq{color:var(--x1o3m372)}"]
    );
    // `.transformed` stays allowed under any configured extension.
    assert_eq!(
        rules(
            &importing("./tokens.transformed"),
            "src/app.ts",
            Some(".css")
        ),
        ["x1nxmr0q|.x1nxmr0q{color:var(--xkwk9w4)}"]
    );
    // The suffix runs on the specifier, never on what it resolves to.
    for specifier in ["./tokens", "./tokens.stylex"] {
        let err = compile_with(&importing(specifier), "src/app.ts", Some(".css"))
            .expect_err("must error");
        assert_eq!(err.code, ErrorCode::NonStaticValue, "{}", err.message);
    }
}

#[test]
fn create_and_create_theme_are_not_gated_by_the_filename() {
    assert_eq!(
        rules(
            &format!("{IMP}export const styles = stylex.create({{ root: {{ color: 'red' }} }});\n"),
            "src/tokens.css.ts",
            Some(".css")
        ),
        ["x1e2nbdu|.x1e2nbdu{color:red}"]
    );
    assert_eq!(
        rules(
            &format!(
                "{IMP}import {{ vars }} from './tokens.css';\nexport const theme = stylex.createTheme(vars, {{ color: 'blue' }});\n"
            ),
            "src/app.ts",
            Some(".css")
        ),
        ["xcyc3co|.xcyc3co, .xcyc3co:root{--x1o3m372:blue;}"]
    );
}

#[test]
fn define_marker_is_gated() {
    let marker = format!("{IMP}export const marker = stylex.defineMarker();\n");
    assert!(
        rules(&marker, "src/tokens.css.ts", Some(".css")).is_empty(),
        "defineMarker emits no rules"
    );
    hash_error(&marker, "src/app.ts", Some(".css"));
}
