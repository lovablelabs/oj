use super::{HookFilter, HookFilterPlan};

fn plan(json: &str) -> HookFilterPlan {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    HookFilterPlan::from_json(Some(&v))
}

#[test]
fn absent_hook_wants_nothing() {
    let p = plan(r#"{"present":false,"unfiltered":false,"plugins":[]}"#);
    assert!(!p.wants("/app/src/main.tsx", None));
}

#[test]
fn unfiltered_hook_wants_everything() {
    let p = plan(r#"{"present":true,"unfiltered":true,"plugins":[]}"#);
    assert!(p.wants("anything", None));
}

#[test]
fn id_filter_gates_by_module_id() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"\\.tsx$","flags":""}],"code":[]}]}"#,
    );
    assert!(p.wants("/app/src/App.tsx", None));
    assert!(!p.wants("/app/node_modules/react/index.js", None));
}

#[test]
fn id_and_code_filters_of_one_plugin_both_apply() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"\\.js$","flags":""}],"code":[{"literal":"import.meta.glob"}]}]}"#,
    );
    assert!(p.wants("/a.js", Some("import.meta.glob(\"./x\")")));
    assert!(!p.wants("/a.js", Some("plain code")));
    assert!(!p.wants("/a.ts", Some("import.meta.glob(\"./x\")")));
    // No code available (load/resolveId shape): the code half must pass.
    assert!(p.wants("/a.js", None));
}

#[test]
fn filters_union_across_plugins() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"\\.md$","flags":""}],"code":[]},{"id":[{"source":"\\.svg$","flags":""}],"code":[]}]}"#,
    );
    assert!(p.wants("/doc.md", None));
    assert!(p.wants("/icon.svg", None));
    assert!(!p.wants("/main.ts", None));
}

#[test]
fn case_insensitive_js_regex_carries_over() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"\\.SVG$","flags":"i"}],"code":[]}]}"#,
    );
    assert!(p.wants("/icon.svg", None));
}

#[test]
fn dotall_carries_and_multiline_fails_open() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[{"source":"a.b","flags":"s"}]}]}"#,
    );
    assert!(p.wants("/a.js", Some("a\nb")));
    assert!(!p.wants("/a.js", Some("ab")));
    // JS multiline anchors honor \r and \u2028, Rust's (?m) only \n.
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[{"source":"^import\\s","flags":"m"}]}]}"#,
    );
    assert!(p.wants("/a.js", Some("// banner\rimport x from \"y\";")));
}

#[test]
fn windows_ids_match_slash_normalized_like_the_host() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"/src/.*\\.tsx$","flags":""}],"code":[]}]}"#,
    );
    assert!(p.wants(r"C:\app\src\App.tsx", None));
    assert!(!p.wants(r"C:\app\node_modules\x\index.js", None));
}

#[test]
fn uncompilable_regex_fails_open_to_unfiltered() {
    // JS lookahead does not compile in the regex crate; the plan must then
    // treat that plugin as unfiltered rather than never offering it modules.
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"source":"(?!never)x","flags":""}],"code":[]}]}"#,
    );
    assert!(p.wants("/anything/at/all", None));
}

#[test]
fn malformed_plan_fails_open() {
    let p = HookFilterPlan::from_json(None);
    assert!(p.wants("x", None));
    let p = plan(r#"{"plugins":"nope"}"#);
    assert!(p.wants("x", None));
}

#[test]
fn empty_filter_entry_fails_open() {
    let p = plan(r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[]}]}"#);
    assert!(p.wants("/anything", None));
}

#[test]
fn string_id_globs_gate_like_vites_picomatch() {
    // gatePatterns sends a string id filter as Vite's cwd-joined glob.
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"glob":"**/*.svg"},{"glob":"/app/src/*.{md,mdx}"}],"code":[]}]}"#,
    );
    assert!(p.wants("/app/src/icon.svg", None));
    assert!(p.wants("/app/src/.hidden/icon.svg", None), "dot: true");
    assert!(
        p.wants("/app/node_modules/x/icon.svg", None),
        "no node_modules ignore"
    );
    assert!(p.wants("/app/src/doc.mdx", None));
    assert!(!p.wants("/app/src/deep/doc.md", None));
    assert!(!p.wants("/app/src/main.ts", None));
}

#[test]
fn extglobs_and_unknown_patterns_fail_open() {
    for item in [
        r#"{"glob":"**/*.@(svg|png)"}"#,
        r#"{"glob":"/app/src/[[:alpha:]].ts"}"#,
        r#"{"glob":"/app/src/*.{1..3}"}"#,
        r#"{"glob":"**/*.[!j]s"}"#,
        r#"{"glob":"!**/*.ts"}"#,
        r#"{"any":true}"#,
        r#"{"source":"(?<=x)y","flags":""}"#,
    ] {
        let f = HookFilter::from_json(&serde_json::from_str(item).unwrap());
        assert!(f.is_match("/anything.ts"), "{item} matches everything");
    }
}

#[test]
fn ascii_word_boundaries_match_like_js() {
    // JS's \b is ASCII: next to a non-ASCII letter JS sees a boundary, Rust's
    // Unicode \b does not.
    let filters = HookFilter::list_from_json(r#"[{"source":"\\bimport\\b","flags":""}]"#);
    assert!(filters[0].is_match("éimport x"));
    assert!(!filters[0].is_match("reimported"));
}

#[test]
fn rust_only_regex_syntax_reads_as_js_does() {
    use super::rust_regex_from_js as js;
    let re = |source: &str| regex::Regex::new(&js(source, "").unwrap()).unwrap();
    // Rust's set operators and nested class are JS's plain characters.
    assert!(re("[a&&b]").is_match("&"));
    assert!(re("[a~~b]").is_match("~"));
    assert!(re("[[x]").is_match("["));
    // A letter escape JS gives no meaning is the letter (no u flag here).
    assert!(re(r"\pL").is_match("pL"));
    assert!(!re(r"\pL").is_match("é"));
    assert!(re(r"\Afoo\z").is_match("xAfooz"));
    // JS's \d \w \s are ASCII (\s its own set), in a set and out of it.
    assert!(!re(r"^\d$").is_match("\u{0663}"));
    assert!(!re(r"^[\w]$").is_match("é"));
    assert!(re(r"^\s$").is_match("\u{FEFF}"));
    assert!(re(r"^\W$").is_match("é"));
    assert_eq!(js(r"\.svg$", "i").as_deref(), Some(r"(?i)\.svg$"));
}

#[test]
fn js_regex_syntax_rust_cannot_carry_fails_open() {
    use super::rust_regex_from_js as js;
    for (source, flags) in [
        ("x", "m"),
        ("x", "u"),
        ("[+--]", ""),
        (r"[\W]", ""),
        (r"\x{41}", ""),
        (r"\u{41}", ""),
    ] {
        assert_eq!(js(source, flags), None, "/{source}/{flags}");
    }
    let f = HookFilter::from_json(&serde_json::json!({"source": "(?<=x)y", "flags": ""}));
    assert!(
        f.is_match("/anything.ts"),
        "an uncompilable regex matches everything"
    );
}
