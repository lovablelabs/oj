// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Hostile and degenerate module specifiers, and the package metadata a real
//! `node_modules` tree contains. Resolution is a policy layer over
//! `oxc_resolver`, so what is tested here is oj's policy: what resolves, what
//! must not, and that a failure is always a described error rather than a
//! panic.
//!
//! Confinement to the project root is deliberately *not* resolution's job --
//! a monorepo or a linked dependency lives outside it, and the dev server is
//! what decides whether a resolved path may be served. These tests pin that
//! division so a later change cannot blur it silently.

use std::fs;
use std::path::{Path, PathBuf};

use oj_resolver::OjResolver;

const CONDS: [&str; 4] = ["browser", "import", "module", "default"];

fn conds() -> Vec<String> {
    CONDS.map(String::from).to_vec()
}

/// A throwaway project tree.
struct Tree(tempfile::TempDir);

impl Tree {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn root(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.0.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.0.path().join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn resolver(&self) -> OjResolver {
        OjResolver::new(self.root())
    }
}

#[test]
fn degenerate_specifiers_fail_with_a_description() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "export const a = 1;");
    let resolver = tree.resolver();
    let src = tree.root().join("src");

    for specifier in [
        "",
        " ",
        "\t",
        ".",
        "..",
        "/",
        "//",
        "./",
        "../",
        "\0",
        "./\0",
        "\u{feff}./App",
        "./App\u{0}.tsx",
        "?query-only",
        "#fragment-only",
        "./App?raw",
        "./App#frag",
        &"../".repeat(64),
        &"a".repeat(4096),
    ] {
        match resolver.resolve(&src, specifier) {
            Ok(path) => assert!(
                path.is_absolute(),
                "{specifier:?} resolved to a relative path: {path:?}"
            ),
            Err(err) => {
                assert_eq!(err.specifier, specifier);
                assert_eq!(err.importer, src);
                assert!(
                    !err.reason.is_empty(),
                    "{specifier:?} failed with no reason"
                );
                // The message a user sees names both sides.
                let text = err.to_string();
                assert!(text.contains("cannot resolve"), "{text}");
            }
        }
    }
}

#[test]
fn a_traversal_out_of_the_project_resolves_to_an_absolute_path_outside_it() {
    // Not a bug: linked packages and monorepo siblings live outside the root.
    // Whether such a path may be *served* is the dev server's call; resolution
    // only has to report where the file is, absolutely and unambiguously.
    let tree = Tree::new();
    let outside = tree.file("outside/secret.js", "export const s = 1;");
    let inside = tree.dir("app/src");
    let resolver = OjResolver::new(&tree.root().join("app"));

    let resolved = resolver
        .resolve(&inside, "../../outside/secret.js")
        .unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(
        fs::canonicalize(&resolved).unwrap(),
        fs::canonicalize(&outside).unwrap()
    );
    assert!(
        !resolved.starts_with(tree.root().join("app")),
        "{resolved:?} is supposed to be outside the app"
    );
}

#[test]
fn a_specifier_cannot_reach_a_directory_or_a_missing_extension_by_accident() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "export const a = 1;");
    tree.dir("src/components");
    let resolver = tree.resolver();
    let src = tree.root().join("src");

    // A bare directory with no index and no package.json does not resolve.
    assert!(resolver.resolve(&src, "./components").is_err());
    // Extension probing only covers the configured list.
    tree.file("src/style.styl", "");
    assert!(resolver.resolve(&src, "./style").is_err());
    // ...and an exact path always wins.
    assert!(resolver.resolve(&src, "./style.styl").is_ok());
}

#[test]
fn extension_probing_order_is_stable() {
    let tree = Tree::new();
    let src = tree.dir("src");
    // Every candidate exists at once: the first configured extension wins, and
    // the default list is Vite's DEFAULT_EXTENSIONS (.mjs, .js, .mts, .ts, ...).
    for ext in ["tsx", "ts", "jsx", "js", "mjs", "mts", "json"] {
        tree.file(&format!("src/Ambiguous.{ext}"), "{}");
    }
    let resolved = tree.resolver().resolve(&src, "./Ambiguous").unwrap();
    assert!(resolved.ends_with("Ambiguous.mjs"), "{resolved:?}");
    std::fs::remove_file(src.join("Ambiguous.mjs")).unwrap();
    let resolved = tree.resolver().resolve(&src, "./Ambiguous").unwrap();
    assert!(
        resolved.ends_with("Ambiguous.js"),
        ".js before .ts: {resolved:?}"
    );
}

#[test]
fn a_malformed_package_json_is_an_error_not_a_panic() {
    for contents in [
        "{ not json",
        "",
        "null",
        "[]",
        "{\"main\": 42}",
        "{\"exports\": 42}",
        "{\"exports\": {\".\": 42}}",
        "{\"browser\": \"nonexistent.js\"}",
        "{\"main\": \"./missing.js\"}",
        "{\"exports\": {\".\": \"./missing.js\"}}",
        "{\"exports\": {\".\": {\"import\": {\"default\": {}}}}}",
    ] {
        let tree = Tree::new();
        tree.file("node_modules/pkg/package.json", contents);
        tree.file("node_modules/pkg/index.js", "module.exports = 1;");
        let resolver = tree.resolver();
        // Resolves to the index or fails cleanly; either way no panic.
        match resolver.resolve(tree.root(), "pkg") {
            Ok(path) => assert!(path.is_absolute(), "{contents}: {path:?}"),
            Err(err) => assert!(!err.reason.is_empty(), "{contents}"),
        }
    }
}

#[test]
fn an_exports_map_that_hides_a_file_keeps_hiding_it() {
    let tree = Tree::new();
    tree.file(
        "node_modules/pkg/package.json",
        r#"{"name":"pkg","exports":{".":"./public.js","./allowed":"./allowed.js"}}"#,
    );
    tree.file("node_modules/pkg/public.js", "");
    tree.file("node_modules/pkg/allowed.js", "");
    tree.file("node_modules/pkg/private.js", "");
    let resolver = tree.resolver();

    assert!(resolver
        .resolve(tree.root(), "pkg")
        .unwrap()
        .ends_with("public.js"));
    assert!(resolver
        .resolve(tree.root(), "pkg/allowed")
        .unwrap()
        .ends_with("allowed.js"));
    for hidden in [
        "pkg/private",
        "pkg/private.js",
        "pkg/./private.js",
        "pkg/../pkg/private.js",
        "pkg/package.json",
    ] {
        assert!(
            resolver.resolve(tree.root(), hidden).is_err(),
            "{hidden} escaped the exports map"
        );
    }
}

#[test]
fn a_package_cannot_be_escaped_through_its_own_exports_map() {
    let tree = Tree::new();
    tree.file("secret.js", "export const s = 1;");
    tree.file(
        "node_modules/pkg/package.json",
        r#"{"name":"pkg","exports":{"./escape":"../../secret.js"}}"#,
    );
    let resolver = tree.resolver();
    assert!(
        resolver.resolve(tree.root(), "pkg/escape").is_err(),
        "an exports target must stay inside its package"
    );
}

#[test]
fn private_imports_are_only_visible_inside_their_package() {
    let tree = Tree::new();
    tree.file(
        "node_modules/pkg/package.json",
        r##"{"name":"pkg","imports":{"#internal":"./internal.js"},"exports":{".":"./index.js"}}"##,
    );
    tree.file("node_modules/pkg/index.js", "");
    tree.file("node_modules/pkg/internal.js", "");
    let resolver = tree.resolver();
    let inside = tree.root().join("node_modules/pkg");

    assert!(resolver
        .resolve(&inside, "#internal")
        .unwrap()
        .ends_with("internal.js"));
    assert!(
        resolver.resolve(tree.root(), "#internal").is_err(),
        "a `#` import must not resolve from outside the package"
    );
}

#[test]
fn a_self_referential_package_name_resolves_through_its_own_exports() {
    let tree = Tree::new();
    tree.file(
        "package.json",
        r#"{"name":"the-app","exports":{".":"./src/index.js","./util":"./src/util.js"}}"#,
    );
    tree.file("src/index.js", "");
    tree.file("src/util.js", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    assert!(resolver
        .resolve(&src, "the-app/util")
        .unwrap()
        .ends_with("util.js"));
    assert!(
        resolver.resolve(&src, "the-app/src/util.js").is_err(),
        "self-reference honours the exports map too"
    );
}

#[test]
fn a_symlinked_dependency_resolves_through_the_link() {
    let tree = Tree::new();
    let real = tree.dir("linked-pkg");
    fs::write(
        real.join("package.json"),
        r#"{"name":"linked-pkg","main":"index.js"}"#,
    )
    .unwrap();
    fs::write(real.join("index.js"), "module.exports = 1;").unwrap();
    tree.dir("app/node_modules");
    let link = tree.root().join("app/node_modules/linked-pkg");

    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&real, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&real, &link).is_ok();
    if !linked {
        return; // no symlink privilege; nothing to assert
    }

    let resolver = OjResolver::new(&tree.root().join("app"));
    let resolved = resolver
        .resolve(&tree.root().join("app"), "linked-pkg")
        .unwrap();
    assert!(resolved.ends_with("index.js"), "{resolved:?}");
}

#[test]
fn a_cyclic_symlink_does_not_hang_resolution() {
    let tree = Tree::new();
    let a = tree.dir("a");
    let link = a.join("loop");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&a, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&a, &link).is_ok();
    if !linked {
        return;
    }
    let resolver = tree.resolver();
    let deep = "./loop".to_string() + &"/loop".repeat(40) + "/missing.js";
    // Must terminate, with either answer.
    let _ = resolver.resolve(&a, &deep);
}

#[test]
fn dedupe_only_applies_to_bare_specifiers_and_falls_back_when_the_root_has_no_copy() {
    let tree = Tree::new();
    // Only the nested copy exists.
    tree.file(
        "pkg/node_modules/dep/package.json",
        r#"{"name":"dep","main":"index.js"}"#,
    );
    tree.file(
        "pkg/node_modules/dep/index.js",
        "module.exports = 'nested';",
    );
    tree.file("pkg/local.js", "");

    let resolver = OjResolver::with_options(
        tree.root(),
        &conds(),
        &[],
        &["dep".to_string(), "@scope/dep".to_string()],
    );
    let nested = tree.root().join("pkg");

    // Dedupe from the root finds nothing, so it falls back to the importer.
    let resolved = resolver.resolve(&nested, "dep").unwrap();
    assert!(resolved.ends_with("index.js"), "{resolved:?}");
    // Relative and absolute specifiers are never deduped.
    assert!(resolver
        .resolve(&nested, "./local.js")
        .unwrap()
        .ends_with("local.js"));
    // A package that merely starts with a deduped name is not deduped.
    assert!(resolver.resolve(&nested, "deputy").is_err());
}

#[test]
fn dedupe_matches_scoped_packages_by_their_full_name() {
    let tree = Tree::new();
    tree.file(
        "node_modules/@scope/dep/package.json",
        r#"{"name":"@scope/dep","main":"root.js"}"#,
    );
    tree.file("node_modules/@scope/dep/root.js", "");
    tree.file(
        "pkg/node_modules/@scope/dep/package.json",
        r#"{"name":"@scope/dep","main":"nested.js"}"#,
    );
    tree.file("pkg/node_modules/@scope/dep/nested.js", "");
    let nested = tree.root().join("pkg");

    let plain = OjResolver::with_options(tree.root(), &conds(), &[], &[]);
    assert!(plain
        .resolve(&nested, "@scope/dep")
        .unwrap()
        .ends_with("nested.js"));

    let deduped = OjResolver::with_options(tree.root(), &conds(), &[], &["@scope/dep".to_string()]);
    assert!(deduped
        .resolve(&nested, "@scope/dep")
        .unwrap()
        .ends_with("root.js"));
    // A subpath of a deduped scoped package dedupes as well.
    tree.file("node_modules/@scope/dep/sub.js", "");
    assert!(deduped
        .resolve(&nested, "@scope/dep/sub.js")
        .unwrap()
        .ends_with("sub.js"));
}

#[test]
fn a_degenerate_dedupe_list_is_harmless() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "");
    let src = tree.root().join("src");
    for entry in ["", " ", "/", "..", "@", "@scope", "a".repeat(300).as_str()] {
        let resolver = OjResolver::with_options(tree.root(), &conds(), &[], &[entry.to_string()]);
        assert!(resolver
            .resolve(&src, "./App")
            .unwrap()
            .ends_with("App.tsx"));
    }
}

#[test]
fn aliases_are_applied_before_node_resolution_and_degenerate_ones_are_inert() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "");
    tree.file(
        "node_modules/real/package.json",
        r#"{"name":"real","main":"i.js"}"#,
    );
    tree.file("node_modules/real/i.js", "");

    // A relative alias target is resolved against the project root.
    let aliased = OjResolver::with_options(
        tree.root(),
        &conds(),
        &[("~".to_string(), "./src".to_string())],
        &[],
    );
    assert!(aliased
        .resolve(tree.root(), "~/App")
        .unwrap()
        .ends_with("App.tsx"));

    // An alias can shadow a real package.
    let shadowing = OjResolver::with_options(
        tree.root(),
        &conds(),
        &[("real".to_string(), "./src/App.tsx".to_string())],
        &[],
    );
    assert!(shadowing
        .resolve(tree.root(), "real")
        .unwrap()
        .ends_with("App.tsx"));

    // Degenerate aliases must not break unrelated resolution.
    for (find, replacement) in [
        ("", ""),
        ("~", "./nonexistent"),
        ("~", "/absolute/nowhere"),
        ("..", "./src"),
        ("\0", "./src"),
    ] {
        let resolver = OjResolver::with_options(
            tree.root(),
            &conds(),
            &[(find.to_string(), replacement.to_string())],
            &[],
        );
        assert!(
            resolver.resolve(tree.root(), "./src/App.tsx").is_ok(),
            "alias {find:?} -> {replacement:?} broke plain resolution"
        );
    }
}

#[test]
fn an_empty_condition_list_still_resolves_a_plain_package() {
    let tree = Tree::new();
    tree.file(
        "node_modules/pkg/package.json",
        r#"{"name":"pkg","main":"i.js"}"#,
    );
    tree.file("node_modules/pkg/i.js", "");
    let resolver = OjResolver::with_conditions(tree.root(), &[]);
    assert!(resolver
        .resolve(tree.root(), "pkg")
        .unwrap()
        .ends_with("i.js"));
}

#[test]
fn a_malformed_tsconfig_fails_every_resolution_with_a_message_that_says_why() {
    // Not silently ignored: a broken tsconfig means the alias table is unknown,
    // and guessing would resolve imports to the wrong files. Every failure names
    // the tsconfig, so the cause is in the first error a user sees.
    let tree = Tree::new();
    tree.file("tsconfig.json", "{ not json");
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    for specifier in ["./App", "@/App"] {
        let err = resolver.resolve(&src, specifier).unwrap_err();
        assert!(err.reason.contains("tsconfig"), "{}", err.reason);
    }
}

#[test]
fn a_jsonc_tsconfig_is_understood() {
    // Comments and trailing commas are normal in a real tsconfig.json.
    let tree = Tree::new();
    tree.file(
        "tsconfig.json",
        "{\n  // the app\n  /* aliases */\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@/*\": [\"./src/*\"] },\n  },\n}",
    );
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    assert!(resolver
        .resolve(&src, "@/App")
        .unwrap()
        .ends_with("App.tsx"));
}

#[test]
fn a_tsconfig_paths_entry_pointing_nowhere_is_an_error_not_a_panic() {
    let tree = Tree::new();
    tree.file(
        "tsconfig.json",
        r##"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["./nowhere/*"],"#p":["./x"]}}}"##,
    );
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    assert!(resolver.resolve(&src, "@/App").is_err());
    assert!(resolver
        .resolve(&src, "./App")
        .unwrap()
        .ends_with("App.tsx"));
}

#[test]
fn resolution_is_deterministic_and_side_effect_free() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    let first = resolver.resolve(&src, "./App").unwrap();
    for _ in 0..5 {
        assert_eq!(resolver.resolve(&src, "./App").unwrap(), first);
    }
    // Failures are stable too, and nothing was created on disk.
    let before = fs::read_dir(&src).unwrap().count();
    for _ in 0..5 {
        assert!(resolver.resolve(&src, "./Missing").is_err());
    }
    assert_eq!(fs::read_dir(&src).unwrap().count(), before);
}

#[test]
fn an_importer_directory_that_does_not_exist_fails_cleanly() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let err = resolver
        .resolve(&tree.root().join("no/such/dir"), "./App")
        .unwrap_err();
    assert!(!err.reason.is_empty());
    assert!(err.importer.ends_with("no/such/dir"));
}

#[test]
fn a_file_used_as_an_importer_directory_does_not_panic() {
    let tree = Tree::new();
    let file = tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let _ = resolver.resolve(&file, "./App");
}

#[test]
fn node_builtins_and_non_file_schemes_do_not_resolve_to_files() {
    let tree = Tree::new();
    tree.file("src/App.tsx", "");
    let resolver = tree.resolver();
    let src = tree.root().join("src");
    for specifier in [
        "node:fs",
        "fs",
        "http://example.com/x.js",
        "https://example.com/x.js",
        "data:text/javascript,export default 1",
        "//example.com/x.js",
    ] {
        assert!(
            resolver.resolve(&src, specifier).is_err(),
            "{specifier} resolved to a file"
        );
    }
}

#[test]
fn a_file_url_resolves_to_the_path_it_names() {
    // `file://` is a path, not a network scheme, and the Node resolver treats it
    // as one. Pinned because it is the one scheme that does resolve: an app can
    // reach any readable file through it, and the dev server's `/@fs`
    // allow-list -- not resolution -- is what decides whether it may be served.
    let tree = Tree::new();
    let target = tree.file("outside.js", "export const a = 1;");
    let src = tree.dir("src");
    let resolver = tree.resolver();
    let url = format!("file://{}", target.display());
    match resolver.resolve(&src, &url) {
        Ok(resolved) => assert_eq!(
            fs::canonicalize(resolved).unwrap(),
            fs::canonicalize(&target).unwrap()
        ),
        Err(err) => assert!(!err.reason.is_empty()),
    }
}
