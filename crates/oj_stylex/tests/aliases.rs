//! The `aliases` option and `rewriteAliases`, pinned against
//! @stylexjs/babel-plugin 0.19.0 (conformance corpus regressions/aliases).

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, SourceCompileResult, transform_source};
use oj_stylex::errors::{ErrorCode, StylexError};
use oj_stylex::module_resolution::StdFs;
use oj_stylex::options::CompilerOptions;
use serde_json::{Value, json};

const THEME: &str = "import * as stylex from '@stylexjs/stylex';\nexport const vars = stylex.defineVars({ c: 'red' });\n";

/// Package name and relative paths match the conformance fixture, so every
/// hash below is the oracle's own output for the same canonical name.
fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("aliases-fixture");
    ONCE.call_once(|| {
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dirs");
            std::fs::write(path, body).expect("fixture file");
        };
        write("package.json", "{\"name\":\"alias-pkg\"}\n");
        write("sub/package.json", "{\"name\":\"sub-pkg\"}\n");
        write(
            "node_modules/realpkg/package.json",
            "{\"name\":\"realpkg\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
        );
        write("node_modules/realpkg/index.js", "module.exports = {};\n");
        for rel in [
            "node_modules/realpkg/theme.stylex.ts",
            "lib/colors.stylex.ts",
            "lib/dup.stylex/dup.stylex.ts",
            "lib2/colors.stylex.js",
            "lib2/colors.stylex.ts",
            "alt/colors.stylex.ts",
            "sub/theme.stylex.ts",
            "src/rel.stylex.ts",
            "plainfiles/notatheme.ts",
        ] {
            write(rel, THEME);
        }
        let link = root.join("linked");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("lib", link).expect("symlink");
    });
    root
}

fn compile(
    source: &str,
    filename: &str,
    options: Value,
) -> Result<SourceCompileResult, StylexError> {
    let root = fixture_root();
    let text = serde_json::to_string(&options)
        .expect("options json")
        .replace("<ROOT>", &root.to_string_lossy());
    let options =
        CompilerOptions::from_json(&serde_json::from_str(&text).expect("options reparse"))
            .expect("options parse")
            .resolve()
            .expect("options resolve");
    let filename = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: &root,
    };
    transform_source(&ctx, &options, &StdFs).map(|r| r.expect("pre-gate must pass"))
}

fn commonjs(aliases: Value) -> Value {
    let mut options = json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" },
    });
    if !aliases.is_null() {
        options["aliases"] = aliases;
    }
    options
}

/// The theme-ref rule of `src/input.ts` importing `specifier`, which decodes
/// the canonical path the alias resolved to.
fn theme_rule(specifier: &str, aliases: Value) -> String {
    theme_rule_from("src/input.ts", specifier, commonjs(aliases))
}

fn theme_rule_from(filename: &str, specifier: &str, options: Value) -> String {
    let source = format!(
        "import * as stylex from '@stylexjs/stylex';\nimport {{ vars }} from '{specifier}';\nexport const styles = stylex.create({{ root: {{ color: vars.c }} }});\n"
    );
    let rules = compile(&source, filename, options).expect("compile").rules;
    let rule = rules.first().expect("one rule");
    rule.ltr.to_string()
}

fn unresolvable(specifier: &str, aliases: Value) {
    let source = format!(
        "import * as stylex from '@stylexjs/stylex';\nimport {{ vars }} from '{specifier}';\nexport const styles = stylex.create({{ root: {{ color: vars.c }} }});\n"
    );
    let err = compile(&source, "src/input.ts", commonjs(aliases)).expect_err("must error");
    assert_eq!(err.code, ErrorCode::NonStaticValue, "{}", err.message);
    assert!(
        err.message
            .contains("Could not resolve the path to the imported file."),
        "{}",
        err.message
    );
}

const LIB: &str = ".x1x01j6y{color:var(--xjr5t5l)}";
const ALT: &str = ".xanp1uo{color:var(--x62h7hm)}";
const REALPKG: &str = ".x1n47vpi{color:var(--x1pp1ts4)}";
const LIB_DIR: &str = ".xevabzp{color:var(--xa8aeqo)}";

#[test]
fn value_shapes_normalize_and_fall_back_in_order() {
    let lib = "<ROOT>/lib/colors.stylex.ts";
    assert_eq!(
        theme_rule("@lib/colors.stylex", json!({ "@lib/*": "<ROOT>/lib/*" })),
        LIB
    );
    assert_eq!(
        theme_rule("@lib/colors.stylex", json!({ "@lib/*": ["<ROOT>/lib/*"] })),
        LIB
    );
    // Within one value array the first entry that exists wins.
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "@lib/*": ["<ROOT>/nope/*", "<ROOT>/lib/*"] })
        ),
        LIB
    );
    assert_eq!(
        theme_rule("@lib/colors.stylex", json!({ "@lib/colors.stylex": lib })),
        LIB
    );
}

/// logAndDefault, not a throw: one bad entry discards the whole map, valid
/// siblings included, and the failure lands at the use site instead.
#[test]
fn malformed_maps_discard_every_alias() {
    let lib = "<ROOT>/lib/colors.stylex.ts";
    unresolvable("@lib/colors.stylex", json!({ "@lib/*": 42 }));
    unresolvable(
        "@lib/colors.stylex",
        json!({ "@lib/*": ["<ROOT>/lib/*", 7] }),
    );
    unresolvable("@lib/colors.stylex", json!("nonsense"));
    unresolvable("@lib/colors.stylex", json!({ "@lib/*": { "a": 1 } }));
    unresolvable(
        "@lib/colors.stylex",
        json!({ "@lib/colors.stylex": lib, "@bad/*": 1 }),
    );
    // A top-level array validates (index keys) and is merely unreachable.
    unresolvable("@lib/colors.stylex", json!([lib]));
    // Empty shapes are inert, never errors.
    assert_eq!(
        theme_rule("./rel.stylex", json!({})),
        ".x1tpc6t2{color:var(--xhxmx4l)}"
    );
    assert_eq!(
        theme_rule("./rel.stylex", Value::Null),
        ".x1tpc6t2{color:var(--xhxmx4l)}"
    );
}

#[test]
fn key_matching_is_exact_or_split_on_the_first_star() {
    // A star-free key is equality, never a prefix.
    unresolvable("@lib/colors.stylex", json!({ "@lib": "<ROOT>/lib" }));
    // before/after split, a bare star, and a suffixed key.
    assert_eq!(
        theme_rule("@lib/colors.stylex", json!({ "@lib/*": "<ROOT>/lib/*" })),
        LIB
    );
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "*": "<ROOT>/lib/colors.stylex.ts" })
        ),
        LIB
    );
    assert_eq!(
        theme_rule(
            "~colors.stylex",
            json!({ "~*.stylex": "<ROOT>/lib/*.stylex.ts" })
        ),
        LIB
    );
    // A third star is ignored: only the first two split parts are read.
    assert_eq!(
        theme_rule(
            "@a/colors.stylex",
            json!({ "@a/*.stylex*": "<ROOT>/lib/*.stylex.ts" })
        ),
        LIB
    );
    // before and after may overlap; the capture is then empty.
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "@lib/colors.stylex*colors.stylex": "<ROOT>/alt/*colors.stylex" })
        ),
        ALT
    );
    // Every star in the value takes the same capture.
    assert_eq!(
        theme_rule("@dup/dup.stylex", json!({ "@dup/*": "<ROOT>/lib/*/*" })),
        ".x1hqtl6z{color:var(--x1dzupuu)}"
    );
}

/// Declaration order decides, not specificity: the first candidate that
/// resolves on disk wins, and a miss falls through to the next key.
#[test]
fn precedence_is_declaration_order() {
    let lib = "<ROOT>/lib/colors.stylex.ts";
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "@lib/*": "<ROOT>/alt/*", "@lib/colors.stylex": lib })
        ),
        ALT
    );
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "@lib/colors.stylex": lib, "@lib/*": "<ROOT>/alt/*" })
        ),
        LIB
    );
    assert_eq!(
        theme_rule(
            "@lib/colors.stylex",
            json!({ "@lib/*": "<ROOT>/nope/*", "@l*": "<ROOT>/alt/colors.stylex.ts" })
        ),
        ALT
    );
}

/// The raw specifier is candidate #0 of every outer iteration, so an alias
/// that hits at iteration 0 beats a real package that needs iteration 1.
#[test]
fn raw_specifier_wins_only_within_its_own_outer_iteration() {
    let to_alt = json!({ "realpkg/*": "<ROOT>/alt/colors.stylex.ts" });
    assert_eq!(
        theme_rule("realpkg/theme.stylex.ts", to_alt.clone()),
        REALPKG
    );
    assert_eq!(theme_rule("realpkg/theme.stylex", to_alt), ALT);
    assert_eq!(theme_rule("realpkg/theme.stylex", Value::Null), REALPKG);
}

#[test]
fn relative_specifiers_bypass_aliases_entirely() {
    unresolvable(
        "./rel-missing.stylex",
        json!({ "./rel-missing.stylex": "<ROOT>/lib/colors.stylex.ts" }),
    );
    assert_eq!(
        theme_rule(
            "./rel.stylex.ts",
            json!({ "./rel.stylex.ts": "<ROOT>/alt/colors.stylex.ts" })
        ),
        ".x1tpc6t2{color:var(--xhxmx4l)}"
    );
    // The guard is `startsWith('.')`, wider than a real relative specifier.
    unresolvable(
        ".hidden/colors.stylex",
        json!({ ".hidden/*": "<ROOT>/lib/*" }),
    );
}

#[test]
fn substitution_base_differs_per_target_form() {
    // Relative targets resolve against the importing file, never rootDir.
    assert_eq!(
        theme_rule_from(
            "src/input.ts",
            "@rel/colors.stylex",
            commonjs(json!({ "@rel/*": "../lib/*" }))
        ),
        LIB
    );
    // Bare targets go through real node resolution.
    assert_eq!(
        theme_rule("@pkg/theme.stylex", json!({ "@pkg/*": "realpkg/*" })),
        REALPKG
    );
    // /ROOT/ needs a rootDir; without one it falls through to the plain
    // absolute branch, where no literal /ROOT directory exists.
    assert_eq!(theme_rule("/ROOT/lib/colors.stylex", Value::Null), LIB);
    let no_root = json!({
        "unstable_moduleResolution": { "type": "commonJS" },
        "aliases": { "@r/*": "/ROOT/lib/*" },
    });
    let source = "import * as stylex from '@stylexjs/stylex';\nimport { vars } from '@r/colors.stylex';\nexport const styles = stylex.create({ root: { color: vars.c } });\n";
    assert_eq!(
        compile(source, "src/input.ts", no_root)
            .expect_err("no rootDir")
            .code,
        ErrorCode::NonStaticValue
    );
}

/// The `/ROOT/` and absolute branches probe with `fs.existsSync`, so a
/// directory resolves and the path is never percent-decoded.
#[test]
fn existence_probes_accept_directories() {
    assert_eq!(
        theme_rule("@dir.stylex", json!({ "@dir.stylex": "<ROOT>/lib" })),
        LIB_DIR
    );
    assert_eq!(
        theme_rule("@ts.stylex", json!({ "@ts.stylex": "<ROOT>/lib/" })),
        LIB_DIR
    );
    // A package.json inside the resolved directory is not consulted.
    assert_eq!(
        theme_rule("@sub.stylex", json!({ "@sub.stylex": "<ROOT>/sub" })),
        ".x3kftls{color:var(--x3qo1ps)}"
    );
    // `..` in a target normalizes away, leaving the same canonical name.
    assert_eq!(
        theme_rule(
            "@dd/colors.stylex",
            json!({ "@dd/*": "<ROOT>/src/../lib/*" })
        ),
        LIB
    );
}

/// Upstream bug, reproduced deliberately: moduleResolve realpaths its answer
/// and the existsSync branches do not, so one file gets two CSS variables.
#[test]
fn symlinked_targets_hash_by_spelling() {
    assert_eq!(
        theme_rule("@sym/colors.stylex", json!({ "@sym/*": "<ROOT>/linked/*" })),
        ".x1fuo6x6{color:var(--xxwadsu)}"
    );
    assert_eq!(theme_rule("../linked/colors.stylex", Value::Null), LIB);
    assert_eq!(
        theme_rule("@sym/colors.stylex", json!({ "@sym/*": "../linked/*" })),
        LIB
    );
}

#[test]
fn extension_probe_order_and_gate_on_the_raw_specifier() {
    let lib2 = ".x10fpyh6{color:var(--x5c0log)}";
    assert_eq!(
        theme_rule("@e/colors.stylex", json!({ "@e/*": "<ROOT>/lib2/*" })),
        lib2
    );
    assert_eq!(theme_rule("../lib2/colors.stylex", Value::Null), lib2);
    // The gate reads the specifier, never the target's filename.
    assert_eq!(
        theme_rule(
            "@plain/x.stylex",
            json!({ "@plain/x.stylex": "<ROOT>/plainfiles/notatheme.ts" })
        ),
        ".x3tpbnh{color:var(--x132lfe9)}"
    );
    // A specifier that fails the theme gate never reaches the alias map.
    unresolvable(
        "@colors",
        json!({ "@colors": "<ROOT>/lib/colors.stylex.ts" }),
    );
}

fn rewrite(source: &str, filename: &str, options: Value) -> String {
    compile(source, filename, options).expect("compile").code
}

#[test]
fn rewrite_aliases_rewrites_import_sources() {
    let opts = json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" },
        "aliases": { "@lib/*": "<ROOT>/lib/*" },
        "rewriteAliases": true,
    });
    let code = rewrite(
        "import * as stylex from '@stylexjs/stylex';\nimport { vars } from '@lib/colors.stylex';\nexport const styles = stylex.create({ root: { color: vars.c } });\n",
        "src/input.ts",
        opts,
    );
    assert!(
        code.contains("import { vars } from \"../lib/colors.stylex\";"),
        "{code}"
    );
    // No unstable_moduleResolution at all, side-effect and type imports, and
    // a plain relative source that only needs its extension stripped.
    let bare = json!({ "aliases": { "@lib/*": "<ROOT>/lib/*" }, "rewriteAliases": true });
    assert!(
        rewrite(
            "import '@lib/colors.stylex';\nexport const x = 1;\n",
            "src/input.ts",
            bare.clone()
        )
        .contains("import \"../lib/colors.stylex\";")
    );
    assert!(
        rewrite(
            "import type { vars } from '@lib/colors.stylex';\nexport const x = 1;\n",
            "src/input.ts",
            bare,
        )
        .contains("import type { vars } from \"../lib/colors.stylex\";")
    );
    assert!(
        rewrite(
            "import './rel.stylex.ts';\nexport const x = 1;\n",
            "src/input.ts",
            json!({ "rewriteAliases": true }),
        )
        .contains("import \"./rel.stylex\";")
    );
}

#[test]
fn rewrite_aliases_leaves_everything_else_byte_identical() {
    let unresolvable =
        json!({ "aliases": { "@gone/*": "<ROOT>/nowhere/*" }, "rewriteAliases": true });
    let source = "import '@gone/colors.stylex';\nexport const x = 1;\n";
    assert_eq!(rewrite(source, "src/input.ts", unresolvable), source);
    // The `.stylex` suffix is hardcoded: themeFileExtension does not move it,
    // and /ROOT/ is dead here because no rootDir reaches the resolver.
    let vars_ext = json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>", "themeFileExtension": ".vars" },
        "aliases": { "@lib/*": "<ROOT>/lib/*" },
        "rewriteAliases": true,
    });
    assert!(
        rewrite(
            "import '@lib/colors.stylex';\nexport const x = 1;\n",
            "src/input.ts",
            vars_ext
        )
        .contains("import \"../lib/colors.stylex\";")
    );
    let rooted = json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" },
        "rewriteAliases": true,
    });
    let root_source = "import '/ROOT/lib/colors.stylex';\nexport const x = 1;\n";
    assert_eq!(rewrite(root_source, "src/input.ts", rooted), root_source);
    // A non-boolean value reads as off, with no diagnostic.
    let off = json!({ "aliases": { "@lib/*": "<ROOT>/lib/*" }, "rewriteAliases": "yes" });
    let aliased = "import '@lib/colors.stylex';\nexport const x = 1;\n";
    assert_eq!(rewrite(aliased, "src/input.ts", off), aliased);
}

/// treeshakeCompensation inserts before Program.exit runs, so the inserted
/// copy is rewritten too.
#[test]
fn rewrite_aliases_reaches_treeshake_compensation_imports() {
    let opts = json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" },
        "aliases": { "@lib/*": "<ROOT>/lib/*" },
        "rewriteAliases": true,
        "treeshakeCompensation": true,
    });
    let code = rewrite(
        "import * as stylex from '@stylexjs/stylex';\nimport { vars } from '@lib/colors.stylex';\nexport const styles = stylex.create({ root: { color: vars.c } });\n",
        "src/input.ts",
        opts,
    );
    assert!(
        code.contains(
            "import \"../lib/colors.stylex\";\nimport { vars } from \"../lib/colors.stylex\";"
        ),
        "{code}"
    );
}
