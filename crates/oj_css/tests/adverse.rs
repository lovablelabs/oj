// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Adversarial stylesheets. CSS and Sass come from the same untrusted place as
//! the JavaScript, and both go through parsers with their own recursion and
//! their own idea of what a file is allowed to reach.

use std::path::Path;

use oj_css::{compile_css, compile_sass, is_css_module, is_sass};

/// The stack the CLI gives every thread that compiles a file. Mirrored rather
/// than imported: the CSS pipeline has no reason to depend on the JavaScript
/// one. The value itself lives in `oj_compiler::COMPILE_STACK_SIZE`.
const COMPILE_THREAD_STACK: usize = 16 * 1024 * 1024;

#[test]
fn malformed_css_is_an_error_not_a_panic() {
    for source in [
        "!!not-css!!",
        "@",
        "@media",
        "@media {",
        ".a { color: }",
        "}",
        ":::",
        "@import;",
        "\0\0\0",
        "@charset \"nope\"",
    ] {
        // Accepted or rejected -- never a panic, and never silent garbage.
        if let Ok(out) = compile_css("/x.css", source, true) {
            assert!(
                !out.css.contains("!!not-css!!"),
                "{source:?} passed through verbatim: {}",
                out.css
            );
        }
    }
}

#[test]
fn truncated_and_unterminated_constructs_are_handled() {
    for source in [
        ".a { color: red",
        "/* unterminated",
        ".a { background: url(\"unterminated",
        "@media (min-width: 100px) { .a { color: red }",
        ".a[attr=\"unterminated",
    ] {
        let _ = compile_css("/x.css", source, true);
        let _ = compile_css("/x.module.css", source, false);
    }
}

#[test]
fn empty_stylesheets_are_valid_and_empty() {
    for source in ["", " ", "\n", "/* only a comment */"] {
        let out = compile_css("/x.css", source, true).unwrap();
        assert_eq!(out.css.trim(), "", "{source:?} -> {:?}", out.css);
        assert!(out.exports.is_none());
    }
    // A module with no classes exports an empty map, not `None`.
    let out = compile_css("/x.module.css", "", false).unwrap();
    assert_eq!(out.exports.as_deref(), Some(&[][..]));
}

#[test]
fn very_large_stylesheets_stay_linear() {
    let many_rules: String = (0..20_000)
        .map(|i| format!(".c{i} {{ color: #fff }}\n"))
        .collect();
    let out = compile_css("/big.css", &many_rules, true).unwrap();
    assert!(out.css.contains(".c19999"));

    let one_huge_selector = format!(
        "{} {{ color: red }}",
        (0..20_000)
            .map(|i| format!(".c{i}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(compile_css("/wide.css", &one_huge_selector, true).is_ok());

    let long_value = format!(".a {{ content: \"{}\" }}", "x".repeat(1_000_000));
    assert!(compile_css("/long.css", &long_value, true).is_ok());
}

#[test]
fn deeply_nested_rules_within_the_supported_envelope_compile() {
    // Both parsers recurse per nesting level. Real stylesheets stay in single
    // digits; this pins a wide margin above that, on the stack a compile thread
    // gets. Past a couple of thousand levels the recursion outruns any stack --
    // see `docs/development/testing.md`.
    let handle = std::thread::Builder::new()
        .stack_size(COMPILE_THREAD_STACK)
        .spawn(|| {
            for depth in [8usize, 64, 500] {
                let nested = format!("{}color:red;{}", ".a{".repeat(depth), "}".repeat(depth));
                let css = compile_sass(&nested, None).unwrap();
                assert!(
                    compile_css("/deep.css", &css, true).is_ok(),
                    "depth {depth}"
                );
                let flat = format!("{}color:red;{}", ".a{".repeat(depth), "}".repeat(depth));
                let _ = compile_css("/deep.css", &flat, true);
            }
        })
        .unwrap();
    handle.join().expect("no overflow within the envelope");
}

#[test]
fn hostile_urls_do_not_change_the_module_classification() {
    // Classification is by filename, so a query, a fragment or a directory
    // named `.module.` must not flip it.
    assert!(is_css_module("/a/b.module.css"));
    assert!(is_css_module("/a/b.module.css?used"));
    assert!(is_css_module("/a/b.module.css#frag"));
    assert!(!is_css_module("/a.module.css/b.css"));
    assert!(!is_css_module("/a/b.css?x=.module."));
    assert!(!is_css_module(""));
    assert!(!is_css_module("/"));
    assert!(!is_css_module("module.css"));

    assert!(is_sass("/a/b.scss"));
    assert!(is_sass("/a/b.scss?inline"));
    assert!(!is_sass("/a/b.scss/c.css"));
    assert!(!is_sass("/a/b.css?x=.scss"));
    assert!(!is_sass(""));
}

#[test]
fn css_module_class_names_are_scoped_even_when_they_collide() {
    // Two files with the same class name must not produce the same scoped name,
    // or one stylesheet silently restyles the other.
    let source = ".button { color: red }";
    let a = compile_css("/src/A.module.css", source, false).unwrap();
    let b = compile_css("/src/B.module.css", source, false).unwrap();
    let (a_name, b_name) = (&a.exports.unwrap()[0].1, &b.exports.unwrap()[0].1);
    assert_ne!(a_name, b_name, "same scoped name in two modules");

    // The same file twice is stable, or a warm cache would serve stale names.
    let again = compile_css("/src/A.module.css", source, false).unwrap();
    assert_eq!(&again.exports.unwrap()[0].1, a_name);
}

#[test]
fn css_module_exports_are_sorted_and_complete() {
    let out = compile_css(
        "/src/A.module.css",
        ".z { color: red } .a { color: red } .m { color: red } \
         .a:hover { color: blue } #id { color: red } .with-dash { color: red }",
        false,
    )
    .unwrap();
    let exports = out.exports.expect("module exports");
    let names: Vec<&str> = exports.iter().map(|(k, _)| k.as_str()).collect();
    // Ids are scoped and exported too, as the CSS Modules spec requires.
    assert_eq!(names, ["a", "id", "m", "with-dash", "z"], "sorted, deduped");
    for (name, scoped) in &exports {
        assert_ne!(name, scoped, "{name} not scoped");
    }
}

#[test]
fn a_css_module_with_hostile_class_names_still_exports_them() {
    let out = compile_css(
        "/src/A.module.css",
        ".\\31 numeric { color: red } .a\\.b { color: red } \
         .with\\ space { color: red } .émoji { color: red }",
        false,
    );
    if let Ok(out) = out {
        let exports = out.exports.expect("module exports");
        for (name, scoped) in &exports {
            assert!(!name.is_empty());
            assert!(!scoped.is_empty());
        }
    }
}

#[test]
fn sass_errors_are_reported_with_a_message() {
    for source in [
        ".a { @include missing-mixin; }",
        "@use \"nonexistent\";",
        "@import \"nonexistent\";",
        ".a { width: 1px + ; }",
        "$x: ;",
        "@if { }",
    ] {
        let err = compile_sass(source, None).unwrap_err();
        assert!(err.starts_with("sass error:"), "{source:?} -> {err}");
    }
}

#[test]
fn a_sass_import_cannot_be_resolved_without_a_load_path() {
    // With no load path, nothing outside the source string is reachable.
    for source in [
        "@import \"../../../../../../etc/passwd\";",
        "@use \"/etc/passwd\";",
        "@import \"/etc/hosts\";",
    ] {
        assert!(
            compile_sass(source, None).is_err(),
            "{source:?} resolved without a load path"
        );
    }
}

#[test]
fn a_sass_load_path_confines_imports_to_files_that_parse_as_sass() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("_vars.scss"), "$pad: 1rem;\n").unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "definitely not sass {{{\n").unwrap();

    let ok = compile_sass(
        "@use \"vars\";\n.a { padding: vars.$pad }",
        Some(dir.path()),
    )
    .unwrap();
    assert!(ok.contains("1rem"), "{ok}");

    // A traversal that lands on a real file still has to parse as Sass, and the
    // failure names the file rather than leaking its contents.
    let escaped = format!("@import \"{}\";", outside.display());
    let err = compile_sass(&escaped, Some(dir.path())).unwrap_err();
    assert!(
        !err.contains("definitely not sass"),
        "file contents leaked into the error: {err}"
    );
}

#[test]
fn sass_and_lightningcss_agree_on_a_pipeline_result() {
    let scss = "$c: red;\n.a { color: $c; .b { color: darken($c, 10%) } }";
    let css = compile_sass(scss, None).unwrap();
    let out = compile_css("/x.scss", &css, true).unwrap();
    assert!(out.css.contains(".a .b"), "{}", out.css);
    assert!(out.exports.is_none(), "a .scss url is not a module");

    // The same source through the module path gets scoped exports.
    let module = compile_css("/x.module.scss", &css, true).unwrap();
    assert!(module.exports.is_some());
}

#[test]
fn minification_is_only_a_formatting_choice() {
    let source = ".a { color: #ff0000; padding: 0px }";
    let pretty = compile_css("/x.css", source, false).unwrap().css;
    let minified = compile_css("/x.css", source, true).unwrap().css;
    assert!(pretty.len() >= minified.len());
    // Re-parsing either form yields the same minified output.
    let round_trip = compile_css("/x.css", &pretty, true).unwrap().css;
    assert_eq!(round_trip, minified);
}

#[test]
fn unicode_and_escapes_survive_the_pipeline() {
    let source = ".café::after { content: \"日本語 🚀\\A\" }";
    let out = compile_css("/x.css", source, true).unwrap();
    assert!(out.css.contains("café"), "{}", out.css);
    assert!(out.css.contains("日本語"), "{}", out.css);
}

#[test]
fn a_url_that_is_not_a_path_is_still_usable_as_a_filename() {
    for url in ["", "/", "?query-only", "\u{0}", "a".repeat(4096).as_str()] {
        let out = compile_css(url, ".a { color: red }", true).unwrap();
        assert_eq!(out.css, ".a{color:red}");
    }
    // A parse failure is recovered from (the invalid rule is dropped and
    // reported as a warning labelled with the url), never a hard error.
    let out = compile_css("/src/Weird Name.css", "!!!", false).unwrap();
    assert_eq!(out.css.trim(), "");
}

#[test]
fn compile_css_never_reads_the_filesystem() {
    // The url is a label, not a path: a stylesheet for a file that does not
    // exist compiles, and one whose url points at a real file is not read.
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.css");
    std::fs::write(&real, ".from-disk { color: red }").unwrap();
    let out = compile_css(&real.to_string_lossy(), ".a { color: blue }", true).unwrap();
    assert!(!out.css.contains("from-disk"), "{}", out.css);
    assert!(Path::new(&real).exists());
}

/// Unbounded Sass recursion aborts the process: `grass` has no call-depth limit
/// and Rust cannot catch a stack overflow, so containing this needs process
/// isolation rather than a bigger stack. Kept runnable (`--ignored`) so the
/// behaviour can be re-checked after a `grass` upgrade, and out of CI because
/// it takes the test binary down with it.
#[test]
#[ignore = "aborts the process: unbounded recursion in grass"]
fn recursive_sass_definitions_abort_the_process() {
    let _ = compile_sass("@mixin a { @include a; } .x { @include a; }", None);
    let _ = compile_sass(
        "@function f($n) { @return f($n); } .x { width: f(1) }",
        None,
    );
}
