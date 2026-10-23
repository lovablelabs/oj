//! Oracle-pinned tests for unstable_moduleResolution.type "haste" vs
//! @stylexjs/babel-plugin 0.19.0 (probe corpus regressions/r6-haste-*).

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, SourceCompileResult, transform_source};
use oj_stylex::module_resolution::{BoundedFs, add_file_extension, node_basename, node_extname};
use oj_stylex::options::{CompilerOptions, ResolvedOptions};

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";
const DEF: &str = "export const vars = stylex.defineVars({ foo: 'red', bar: '10px' });\n";

/// The fixture package name is the one the probe corpus ran under, so the
/// commonJS pins below are comparable byte for byte.
fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("haste-fixture");
    ONCE.call_once(|| {
        std::fs::create_dir_all(root.join("a/b/c")).expect("fixture dirs");
        std::fs::write(
            root.join("package.json"),
            "{ \"name\": \"conformance-fixture-up\" }\n",
        )
        .expect("pkg");
    });
    root
}

fn options(json: serde_json::Value) -> ResolvedOptions {
    CompilerOptions::from_json(&json)
        .expect("options parse")
        .resolve()
        .expect("options resolve")
}

fn haste() -> ResolvedOptions {
    options(serde_json::json!({ "unstable_moduleResolution": { "type": "haste" } }))
}

fn compile_with(source: &str, filename: &str, opts: &ResolvedOptions) -> SourceCompileResult {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let filename = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: &root,
    };
    transform_source(&ctx, opts, &fs)
        .expect("compile")
        .expect("pre-gate must pass for stylex sources")
}

fn define(filename: &str, opts: &ResolvedOptions) -> SourceCompileResult {
    compile_with(&format!("{IMP}{DEF}"), filename, opts)
}

fn consume(specifier: &str, filename: &str) -> SourceCompileResult {
    compile_with(
        &format!(
            "{IMP}import {{ vars }} from '{specifier}';\n\
             export const styles = stylex.create({{ a: {{ color: vars.bar }} }});\n"
        ),
        filename,
        &haste(),
    )
}

fn css(out: &SourceCompileResult) -> String {
    out.rules
        .iter()
        .map(|r| r.ltr.as_ref())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn definition_side_hashes_the_basename_only() {
    let out = define("tokens.stylex.js", &haste());
    assert!(css(&out).contains("--x1lbo1ib:red"), "css: {}", css(&out));
    assert!(css(&out).contains("--x1li60ln:10px"), "css: {}", css(&out));
    assert!(out.code.contains("xsg933n"), "code: {}", out.code);

    // The directory is dropped entirely: same basename, same bytes.
    let nested = define("a/b/c/tokens.stylex.js", &haste());
    assert_eq!(nested.code, out.code);
    assert_eq!(css(&nested), css(&out));
}

#[test]
fn definition_side_keeps_the_whole_extension() {
    for (filename, bar) in [
        ("tokens.stylex.js", "--x1li60ln"),
        ("tokens.stylex.ts", "--x1ebhwif"),
        ("tokens.stylex", "--x1kih2tx"),
        (".stylex.js", "--xphx3wb"),
        ("my.tokens.stylex.js", "--xjvc9zv"),
    ] {
        let out = define(filename, &haste());
        assert!(
            css(&out).contains(&format!("{bar}:10px")),
            "{filename} css: {}",
            css(&out)
        );
    }
}

#[test]
fn haste_drops_the_package_prefix_commonjs_keeps() {
    let cjs = options(serde_json::json!({
        "unstable_moduleResolution": { "type": "commonJS" }
    }));
    let out = define("tokens.stylex.js", &cjs);
    assert!(css(&out).contains("--x1wly2bu:10px"), "css: {}", css(&out));
    assert!(out.code.contains("x15582sb"), "code: {}", out.code);
}

#[test]
fn root_dir_is_dropped_under_haste() {
    let rooted = options(serde_json::json!({
        "unstable_moduleResolution": { "type": "haste", "rootDir": "/nowhere" }
    }));
    assert_eq!(
        define("a/b/c/tokens.stylex.js", &rooted).code,
        define("tokens.stylex.js", &haste()).code
    );
}

#[test]
fn import_side_appends_the_importing_files_extension() {
    for (specifier, filename, bar) in [
        // The bare specifier is the only form that meets the definition side.
        ("tokens.stylex.js", "MyComponent.js", "--x1li60ln"),
        ("tokens.stylex", "MyComponent.js", "--x1li60ln"),
        ("tokens.stylex.js", "a/b/c/MyComponent.js", "--x1li60ln"),
        ("tokens.stylex", "MyComponent.react.js", "--x1li60ln"),
        ("tokens.stylex", "MyComponent.tsx", "--x1ykveyb"),
        ("tokens.stylex", "MyComponent.ts", "--x1ebhwif"),
        ("tokens.stylex", "MyComponent.jsx", "--x1pk0x2h"),
        ("tokens.stylex", "MyComponent.mjs", "--x1ard0tz"),
        ("tokens.stylex", "MyComponent.cjs", "--x1jlqola"),
        // path.extname is never checked against the known code extensions.
        ("tokens.stylex", "MyComponent.flow", "--xlwxe9h"),
        ("tokens.stylex", "MyComponent", "--x1kih2tx"),
        // A specifier that already ends in a code extension keeps it.
        ("tokens.stylex.ts", "MyComponent.js", "--x1ebhwif"),
        ("tokens.transformed", "MyComponent.js", "--xjc53tw"),
    ] {
        let out = consume(specifier, filename);
        assert!(
            css(&out).contains(&format!("color:var({bar})")),
            "{specifier} from {filename} css: {}",
            css(&out)
        );
    }
}

#[test]
fn import_side_uses_the_specifier_verbatim() {
    // Silently mismatched with the definition side, exactly as upstream.
    for (specifier, bar) in [
        ("./tokens.stylex.js", "--x1lcaplt"),
        ("../tokens.stylex.js", "--x1dlcjlx"),
        ("sub/tokens.stylex.js", "--x7919qi"),
        ("/html/js/tokens.stylex.js", "--x1a5koyj"),
        // A JS-escaped backslash: the specifier is `sub\tokens.stylex.js`.
        ("sub\\\\tokens.stylex.js", "--x1oil9va"),
        ("@stylexjs/open-props/lib/colors.stylex", "--x11kif4r"),
    ] {
        let out = consume(specifier, "MyComponent.js");
        assert!(
            css(&out).contains(&format!("color:var({bar})")),
            "{specifier} css: {}",
            css(&out)
        );
    }
}

#[test]
fn import_side_never_touches_the_filesystem() {
    // No such file exists under the fixture root, yet the theme ref resolves.
    let out = consume("tokens.stylex", "MyComponent.js");
    assert!(
        css(&out).contains("color:var(--x1li60ln)"),
        "css: {}",
        css(&out)
    );
}

#[test]
fn non_theme_specifiers_still_fail_the_suffix_gate() {
    let err = transform_source(
        &FileContext {
            filename: &fixture_root().join("MyComponent.js"),
            source_text: &format!(
                "{IMP}import {{ vars }} from 'tokens.js';\n\
                 export const styles = stylex.create({{ a: {{ color: vars.bar }} }});\n"
            ),
            cwd: &fixture_root(),
        },
        &haste(),
        &BoundedFs {
            root: fixture_root(),
        },
    )
    .expect_err("must error");
    assert!(
        err.message
            .contains("Could not resolve the path to the imported file"),
        "message: {}",
        err.message
    );
}

#[test]
fn node_path_helpers() {
    assert_eq!(node_basename("/a/b/tokens.stylex.js"), "tokens.stylex.js");
    assert_eq!(node_basename("a/b/"), "b");
    assert_eq!(node_basename("/"), "");
    assert_eq!(node_basename(".."), "..");
    assert_eq!(node_basename("sub\\tokens.js"), "sub\\tokens.js");

    assert_eq!(node_extname("MyComponent.js"), ".js");
    assert_eq!(node_extname("MyComponent.react.js"), ".js");
    assert_eq!(node_extname("MyComponent"), "");
    assert_eq!(node_extname(".stylex"), "");
    assert_eq!(node_extname(".stylex.js"), ".js");
    assert_eq!(node_extname(".."), "");
    assert_eq!(node_extname("..."), ".");
    assert_eq!(node_extname("a."), ".");
    assert_eq!(node_extname("a/b/"), "");
    assert_eq!(node_extname("a/b.js/"), ".js");

    let src = Path::new("/x/MyComponent.flow");
    assert_eq!(
        add_file_extension("tokens.stylex", src),
        "tokens.stylex.flow"
    );
    assert_eq!(
        add_file_extension("tokens.stylex.ts", src),
        "tokens.stylex.ts"
    );
    assert_eq!(
        add_file_extension("tokens.stylex", Path::new("/x/MyComponent")),
        "tokens.stylex"
    );
}
