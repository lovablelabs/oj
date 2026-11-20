//! A rules-only compile is the full compile minus its output JS: the same
//! rules, `modified`, create objects and error, on either backend.

use std::path::{Path, PathBuf};

use oj_stylex::api::{
    FileContext, parse_program, transform_program, transform_program_rules, transform_source_in,
    transform_source_rules_in,
};
use oj_stylex::errors::StylexError;
use oj_stylex::module_resolution::StdFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oj_stylex::rules::StylexRule;
use oxc_allocator::Allocator;
use serde_json::{Value, json};

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";

/// (`modified`, rules, create objects); `None` = pre-gate skip.
type Outcome = Result<Option<(bool, Vec<StylexRule>, String)>, StylexError>;

fn splice_outcomes(ctx: &FileContext<'_>, options: &ResolvedOptions) -> (Outcome, Outcome) {
    let allocator = Allocator::default();
    let full = transform_source_in(&allocator, ctx, options, &StdFs)
        .map(|result| result.map(|r| (r.modified, r.rules, format!("{:?}", r.create_objects))));
    let allocator = Allocator::default();
    let rules = transform_source_rules_in(&allocator, ctx, options, &StdFs)
        .map(|result| result.map(|r| (r.modified, r.rules, format!("{:?}", r.create_objects))));
    (full, rules)
}

/// `None` when the source does not parse: the AST entry points take a program.
fn ast_outcomes(ctx: &FileContext<'_>, options: &ResolvedOptions) -> Option<(Outcome, Outcome)> {
    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, ctx.source_text, "input.tsx").ok()?;
    let rules = transform_program_rules(&program, ctx, options, &StdFs)
        .map(|r| Some((r.modified, r.rules, format!("{:?}", r.create_objects))));
    let full = transform_program(&allocator, &mut program, ctx, options, &StdFs)
        .map(|r| Some((r.modified, r.rules, format!("{:?}", r.create_objects))));
    Some((full, rules))
}

/// The differences between the two compiles of one job, empty when none.
fn differences(root: &Path, filename: &str, source: &str, options: &Value) -> Vec<String> {
    let options = match CompilerOptions::from_json(options).and_then(|o| o.resolve()) {
        Ok(options) => options,
        // Options that do not resolve never reach a compile.
        Err(_) => return Vec::new(),
    };
    let abs = root.join(filename);
    let ctx = FileContext {
        filename: &abs,
        source_text: source,
        cwd: root,
    };
    let mut out = Vec::new();
    let (full, rules) = splice_outcomes(&ctx, &options);
    if full != rules {
        out.push(format!("splice: full {full:?}\n   rules-only {rules:?}"));
    }
    if let Some((full, rules)) = ast_outcomes(&ctx, &options)
        && full != rules
    {
        out.push(format!("ast: full {full:?}\n   rules-only {rules:?}"));
    }
    out
}

fn fixture_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rules-only-fixture");
    std::fs::create_dir_all(root.join("src")).expect("fixture dirs");
    root
}

fn assert_same(source: &str, options: Value) {
    let diffs = differences(&fixture_root(), "src/input.tsx", source, &options);
    assert!(diffs.is_empty(), "{source}\n{}", diffs.join("\n"));
}

fn rules_only(source: &str, options: Value) -> Result<(bool, usize), StylexError> {
    let root = fixture_root();
    let abs = root.join("src/input.tsx");
    let ctx = FileContext {
        filename: &abs,
        source_text: source,
        cwd: &root,
    };
    let options = CompilerOptions::from_json(&options)
        .expect("options parse")
        .resolve()
        .expect("options resolve");
    let allocator = Allocator::default();
    let result =
        transform_source_rules_in(&allocator, &ctx, &options, &StdFs)?.expect("pre-gate must pass");
    Ok((result.modified, result.rules.len()))
}

#[test]
fn every_site_kind_matches_the_full_compile() {
    let styles = format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ color: 'blue' }}, \
         d: (w) => ({{ width: w, color: 'green' }}) }});\n"
    );
    for body in [
        "",
        "export const p = stylex.props(styles.a, styles.b);\n",
        "export const p = stylex.attrs(styles.a);\n",
        "export const p = (on) => stylex.props(styles.a, on && styles.b);\n",
        "export const p = (on, off) => stylex.props(on ? styles.a : styles.b, off && styles.a);\n",
        "export const p = (other) => stylex.props(styles.a, other);\n",
        "export const p = (other) => stylex.props(other, stylex.props(styles.a).className);\n",
        "export const p = (w) => stylex.props(styles.d(w));\n",
        "export const p = (other) => stylex.props([styles.a, other && styles.b]);\n",
        "export const p = stylex.props(styles.a, styles.missing);\n",
        "export const p = (k) => stylex.props(styles[k]);\n",
        "export const C = () => <div {...stylex.props(styles.a, styles.b)} />;\n",
        "export const C = () => <div sx={styles.a} />;\n",
        "export const C = (p) => <div sx={[styles.a, p.on && styles.b]} />;\n",
        "export const C = (p) => <div sx={[styles.a, p.extra]} />;\n",
        "export const C = (p) => <div sx={p.extra} />;\n",
        "export { styles };\n",
        "const list = ['\u{1F600}'];\nexport const p = stylex.props(list, styles.a);\n",
        "const list = [['a\u{1F600}']];\nexport const p = (on) => stylex.props(on && list);\n",
        "const list = ['plain'];\nexport const p = stylex.props(list, styles.a);\n",
    ] {
        assert_same(&format!("{styles}{body}"), json!({}));
        assert_same(&format!("{styles}{body}"), json!({ "dev": true }));
        assert_same(
            &format!("{styles}{body}"),
            json!({ "enableInlinedConditionalMerge": false }),
        );
    }
    for source in [
        "import stylex from 'stylex';\nconst s = stylex.create({ a: { color: 'red' } });\nexport const c = (on) => stylex(s.a, on && s.a);\n",
        "import stylex from 'stylex';\nconst s = stylex.create({ a: { color: 'red' } });\nexport const c = (other) => stylex(s.a, other);\n",
        "import * as stylex from '@stylexjs/stylex';\nexport const k = stylex.keyframes({ from: { opacity: 0 } });\n",
        "import * as stylex from '@stylexjs/stylex';\nexport const s = stylex.create({ a: { color: 1n } });\n",
        "import * as stylex from '@stylexjs/stylex';\nexport const unused = 1;\n",
        "export const C = (p) => <div sx={p.extra} />;\n",
        "export const answer = 42;\n",
        "export const tsx = (;\n",
    ] {
        assert_same(source, json!({}));
        assert_same(
            source,
            json!({ "runtimeInjection": true, "legacyDisableLayers": true }),
        );
    }
}

#[test]
fn modified_counts_the_rewrites_that_never_ran() {
    let styles = format!("{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\n");
    // The create call itself is replaced.
    assert_eq!(rules_only(&styles, json!({})), Ok((true, 1)));
    // No call to compile: the import alone changes nothing.
    assert_eq!(
        rules_only(&format!("{IMP}export const unused = 1;\n"), json!({})),
        Ok((false, 0))
    );
    // A bailed sx attribute, the file's only edit: it becomes a props() spread.
    assert_eq!(
        rules_only(
            &format!("{IMP}export const C = (p) => <div sx={{p.extra}} />;\n"),
            json!({})
        ),
        Ok((true, 0))
    );
    // An inlined props() call, the file's only edit.
    assert_eq!(
        rules_only(
            &format!("{IMP}export const p = stylex.props();\n"),
            json!({})
        ),
        Ok((true, 0))
    );
    // A props() call left as written.
    assert_eq!(
        rules_only(
            &format!("{IMP}export const p = (other) => stylex.props(other);\n"),
            json!({})
        ),
        Ok((false, 0))
    );
}

#[test]
fn the_merge_that_never_ran_still_fails_the_compile() {
    // styleq indexes a string argument per UTF-16 unit and splits the pair.
    let source =
        format!("{IMP}const list = ['\u{1F600}'];\nexport const p = stylex.props(list);\n");
    let error = rules_only(&source, json!({})).expect_err("a lone surrogate");
    assert!(error.message.contains("surrogate"), "{}", error.message);
}

#[test]
fn corpus_jobs_match_the_full_compile() {
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance");
    // Vendored copies of this crate ship without the conformance corpus.
    let Ok(dirs) = std::fs::read_dir(conformance.join("corpus")) else {
        eprintln!("skipping corpus_jobs_match_the_full_compile: corpus not vendored");
        return;
    };
    let mut files: Vec<PathBuf> = dirs
        .flat_map(|dir| std::fs::read_dir(dir.expect("corpus dir").path()))
        .flatten()
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| path.to_string_lossy().ends_with(".jobs.json"))
        .collect();
    files.sort();
    let (mut checked, mut failures) = (0usize, Vec::new());
    for file in files {
        let text = std::fs::read_to_string(&file).expect("jobs file");
        let parsed: Value = serde_json::from_str(&text).expect("jobs json");
        for job in parsed["jobs"].as_array().expect("jobs array") {
            // `sourceFile` corpora are fetched on demand; skip what is absent.
            let source = match job.get("source").and_then(Value::as_str) {
                Some(source) => source.to_string(),
                None => {
                    let file = job["sourceFile"].as_str().expect("source or sourceFile");
                    match std::fs::read_to_string(conformance.join(file)) {
                        Ok(source) => source,
                        Err(_) => continue,
                    }
                }
            };
            let root = conformance.join(job["root"].as_str().expect("job root"));
            let filename = job["filename"].as_str().unwrap_or("input.tsx");
            let options = with_root(job.get("options").unwrap_or(&Value::Null), &root);
            checked += 1;
            let diffs = differences(&root, filename, &source, &options);
            if !diffs.is_empty() {
                failures.push(format!("{}:\n  {}", job["id"], diffs.join("\n  ")));
            }
        }
    }
    eprintln!(
        "rules-only parity: {checked} jobs, {} with differences",
        failures.len()
    );
    assert!(checked > 0, "the corpus holds no job");
    assert!(
        failures.is_empty(),
        "{} jobs differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The harness's `<ROOT>` placeholder, resolved like its job runner does.
fn with_root(value: &Value, root: &Path) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace("<ROOT>", &root.to_string_lossy())),
        Value::Array(items) => Value::Array(items.iter().map(|v| with_root(v, root)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| (key.clone(), with_root(v, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}
