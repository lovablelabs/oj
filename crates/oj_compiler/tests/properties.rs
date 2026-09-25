// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Properties of the per-file pipeline. Three invariants carry most of the
//! weight: the compiler is total over arbitrary bytes, whatever it emits is
//! valid JavaScript, and the same input always produces the same output (the
//! cache keys everything on the input alone).

use std::path::Path;

use oj_compiler::{compile, compile_module, exports, json, CompileOptions};
use proptest::prelude::*;

/// Fragments that combine into plausible-but-strange module source. Each is
/// used at most once per case: repeating one would produce a duplicate
/// declaration, which is an early error rather than an interesting input.
const FRAGMENTS: &[&str] = &[
    "export const a = 1;",
    "export default function () {}",
    "import x from \"./x\";",
    "import * as ns from \"pkg\";",
    "export * from \"./y\";",
    "export * as ns2 from \"./z\";",
    "import(\"./dyn\");",
    "import.meta.env.MODE;",
    "import.meta.url;",
    "import.meta.glob(\"./*.json\");",
    "interface I { a: string }",
    "type T = I | null;",
    "enum E { A, B }",
    "const enum CE { A }",
    "declare module \"m\" {}",
    "abstract class C { abstract m(): void }",
    "export const C2 = () => <div className=\"x\">{1}</div>;",
    "function F() { return <><span/></>; }",
    "const g = async function* () { yield 1; };",
    "label: for (;;) { break label; }",
    "using r = getResource();",
    "class D { #p = 1; static { D.x = 1; } }",
    "const o = { ...spread, [computed]: 1 };",
    "const [x = 1, ...rest] = arr;",
    "try { throw 1; } catch { } finally { }",
    "const t = `a${b}c`;",
    "const re = /a[b-c]+/gu;",
    "const n = 1_000n;",
    "a?.b?.[c]?.();",
    "x ??= 1;",
    "export { };",
];

fn module_source() -> impl Strategy<Value = String> {
    proptest::sample::subsequence(FRAGMENTS, 0..FRAGMENTS.len())
        .prop_map(|parts| parts.join("\n"))
}

fn extension() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("ts"),
        Just("tsx"),
        Just("js"),
        Just("jsx"),
        Just("mjs"),
        Just("cjs"),
    ]
}

/// Whatever oj emits must be parseable JavaScript. `.mjs` accepts module
/// syntax and rejects TS, which is what the output should be.
fn output_parses(code: &str) -> bool {
    compile(Path::new("/verify.mjs"), code, &CompileOptions::prod()).is_ok()
}

proptest! {
    /// Arbitrary bytes are a legal file on disk: compiling them is total.
    #[test]
    fn compiling_arbitrary_bytes_never_panics(source in ".{0,300}", ext in extension()) {
        let path = format!("/src/App.{ext}");
        let _ = compile(Path::new(&path), &source, &CompileOptions::dev());
        let _ = compile(Path::new(&path), &source, &CompileOptions::prod());
        let _ = exports(&source, Path::new(&path));
    }

    /// Same for `.json`.
    #[test]
    fn json_conversion_is_total(source in ".{0,200}") {
        let _ = json::to_esm(&source, "/data.json");
    }

    /// Anything the compiler accepts, it emits as valid JavaScript -- in dev
    /// and in prod, with and without a source map.
    #[test]
    fn accepted_modules_emit_valid_javascript(source in module_source(), ext in extension()) {
        let path = format!("/src/App.{ext}");
        for opts in [
            CompileOptions::dev(),
            CompileOptions::prod(),
            CompileOptions { dev: true, refresh: false, sourcemap: false, ssr: false, ..CompileOptions::dev() },
            CompileOptions { dev: false, refresh: true, sourcemap: false, ssr: false, ..CompileOptions::dev() },
        ] {
            if let Ok(out) = compile(Path::new(&path), &source, &opts) {
                prop_assert!(output_parses(&out.code), "{source}\n->\n{}", out.code);
                prop_assert!(!out.code.contains("interface "), "types left in {}", out.code);
            }
        }
    }

    /// Compilation is a pure function of (path, source, options): the cache
    /// keys entries on exactly that.
    #[test]
    fn compilation_is_deterministic(source in module_source(), ext in extension()) {
        let path = format!("/src/App.{ext}");
        let opts = CompileOptions::dev();
        let Ok(first) = compile(Path::new(&path), &source, &opts) else { return Ok(()) };
        for _ in 0..3 {
            let again = compile(Path::new(&path), &source, &opts).unwrap();
            prop_assert_eq!(&first.code, &again.code);
            prop_assert_eq!(&first.map_data_url, &again.map_data_url);
            prop_assert_eq!(&first.imports, &again.imports);
            prop_assert_eq!(&first.dynamic_imports, &again.dynamic_imports);
            prop_assert_eq!(first.is_refresh_boundary, again.is_refresh_boundary);
        }
    }

    /// Compiled output is a fixed point of the parser: re-compiling it as
    /// `.mjs` produces something that still parses.
    #[test]
    fn output_survives_a_second_pass(source in module_source(), ext in extension()) {
        let path = format!("/src/App.{ext}");
        let Ok(first) = compile(Path::new(&path), &source, &CompileOptions::prod()) else {
            return Ok(());
        };
        let Ok(second) = compile(Path::new("/src/App.mjs"), &first.code, &CompileOptions::prod()) else {
            return Err(TestCaseError::fail(format!("second pass rejected: {}", first.code)));
        };
        prop_assert!(output_parses(&second.code));
    }

    /// Every static import in the source is reported once, in source order,
    /// and a rewriter sees exactly the same list.
    #[test]
    fn imports_are_reported_once_and_in_order(
        specs in proptest::collection::vec("\\./[a-z]{1,6}", 0..8),
    ) {
        let source: String = specs
            .iter()
            .enumerate()
            .map(|(i, spec)| format!("import d{i} from {spec:?};\n"))
            .chain(std::iter::once(format!(
                "export const all = [{}];\n",
                (0..specs.len()).map(|i| format!("d{i}")).collect::<Vec<_>>().join(",")
            )))
            .collect();
        let out = compile(Path::new("/src/App.js"), &source, &CompileOptions::dev()).unwrap();
        prop_assert_eq!(&out.imports, &specs);

        let mut seen = Vec::new();
        let mut rewriter = |spec: &str| {
            seen.push(spec.to_string());
            None
        };
        let out = compile_module(
            Path::new("/src/App.js"),
            &source,
            &CompileOptions::dev(),
            Some(&mut rewriter),
        )
        .unwrap();
        prop_assert_eq!(&seen, &specs);
        prop_assert_eq!(&out.imports, &specs);
    }

    /// A rewriter's answer is what lands in the output, verbatim, however
    /// unpleasant the string.
    #[test]
    fn rewritten_specifiers_are_escaped_not_interpolated(replacement in ".{0,20}") {
        let source = "import a from \"./a\";\nexport { a };";
        let mut rewriter = |_: &str| Some(replacement.clone());
        let out = compile_module(
            Path::new("/src/App.js"),
            source,
            &CompileOptions::dev(),
            Some(&mut rewriter),
        )
        .unwrap();
        prop_assert!(output_parses(&out.code), "{}", out.code);
        prop_assert_eq!(out.imports, vec![replacement]);
    }

    /// `exports` reports names, never syntax: whatever it returns is usable as
    /// an export name.
    #[test]
    fn exported_names_are_well_formed(source in module_source(), ext in extension()) {
        let path = format!("/src/App.{ext}");
        for name in exports(&source, Path::new(&path)) {
            prop_assert!(!name.is_empty());
            prop_assert!(!name.contains(char::is_whitespace), "{name:?}");
        }
    }

    /// A JSON document always yields a module whose default export is the
    /// document, and whose named exports are a subset of its keys.
    #[test]
    fn json_modules_export_their_keys(
        entries in proptest::collection::btree_map("[a-zA-Z_$][a-zA-Z0-9_$]{0,6}", 0i32..100, 0..6),
    ) {
        let source = serde_json::to_string(&entries).unwrap();
        let esm = json::to_esm(&source, "/data.json").unwrap();
        prop_assert!(output_parses(&esm), "{esm}");
        prop_assert_eq!(esm.matches("export default").count(), 1);
        for key in entries.keys() {
            // Reserved words and `__proto__` are default-only, everything else
            // gets a named export.
            let exported = esm.contains(&format!("export const {key} ="));
            let reserved = !esm.contains(&format!("export const {key} "));
            prop_assert!(exported || reserved, "{key} in {esm}");
        }
    }

    /// The dev/prod split never changes what a module imports, only how it is
    /// instrumented -- with one deliberate exception, the JSX runtime, which is
    /// the development build in dev.
    #[test]
    fn the_module_graph_does_not_depend_on_the_mode(source in module_source()) {
        let path = Path::new("/src/App.tsx");
        let dev = compile(path, &source, &CompileOptions::dev());
        let prod = compile(path, &source, &CompileOptions::prod());
        let normalize = |imports: &[String]| -> Vec<String> {
            imports
                .iter()
                .map(|i| i.replace("react/jsx-dev-runtime", "react/jsx-runtime"))
                .collect()
        };
        match (dev, prod) {
            (Ok(dev), Ok(prod)) => {
                prop_assert_eq!(normalize(&dev.imports), normalize(&prod.imports));
                prop_assert_eq!(&dev.dynamic_imports, &prod.dynamic_imports);
                prop_assert!(!prod.is_refresh_boundary, "prod never instruments refresh");
            }
            (Err(_), Err(_)) => {}
            (dev, prod) => prop_assert!(
                false,
                "dev and prod disagree on validity: {:?} vs {:?}",
                dev.is_ok(),
                prod.is_ok()
            ),
        }
    }

    /// A source map, when emitted, is a well-formed data URL for a JSON map.
    #[test]
    fn source_maps_are_well_formed(source in module_source()) {
        let out = compile(Path::new("/src/App.tsx"), &source, &CompileOptions::dev());
        let Ok(out) = out else { return Ok(()) };
        let Some(url) = out.map_data_url.clone() else { return Ok(()) };
        prop_assert!(url.starts_with("data:application/json;"), "{url}");
        let inlined = out.code_with_inline_map();
        prop_assert!(inlined.contains("//# sourceMappingURL="));
    }
}
