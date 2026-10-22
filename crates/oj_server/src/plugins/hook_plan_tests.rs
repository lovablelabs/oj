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
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"\\.tsx$"}],"code":[]}]}"#,
    );
    assert!(p.wants("/app/src/App.tsx", None));
    assert!(!p.wants("/app/node_modules/react/index.js", None));
}

#[test]
fn id_and_code_filters_of_one_plugin_both_apply() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"\\.js$"}],"code":[{"re":"import\\.meta\\.glob"}]}]}"#,
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
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"\\.md$"}],"code":[]},{"id":[{"re":"\\.svg$"}],"code":[]}]}"#,
    );
    assert!(p.wants("/doc.md", None));
    assert!(p.wants("/icon.svg", None));
    assert!(!p.wants("/main.ts", None));
}

#[test]
fn case_insensitive_js_regex_carries_over() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"(?i)\\.SVG$"}],"code":[]}]}"#,
    );
    assert!(p.wants("/icon.svg", None));
}

#[test]
fn multiline_and_dotall_inline_flags_apply() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[{"re":"(?m)^import\\s"}]}]}"#,
    );
    assert!(p.wants("/a.js", Some("// banner\nimport x from \"y\";")));
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[{"re":"(?s)a.b"}]}]}"#,
    );
    assert!(p.wants("/a.js", Some("a\nb")));
}

#[test]
fn windows_ids_match_slash_normalized_like_the_host() {
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"/src/.*\\.tsx$"}],"code":[]}]}"#,
    );
    assert!(p.wants(r"C:\app\src\App.tsx", None));
    assert!(!p.wants(r"C:\app\node_modules\x\index.js", None));
}

#[test]
fn uncompilable_regex_fails_open_to_unfiltered() {
    // JS lookahead does not compile in the regex crate; the plan must then
    // treat that plugin as unfiltered rather than never offering it modules.
    let p = plan(
        r#"{"present":true,"unfiltered":false,"plugins":[{"id":[{"re":"(?!never)x"}],"code":[]}]}"#,
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
        r#"{"any":true}"#,
        r#"{"re":"(?<=x)y"}"#,
    ] {
        let f = HookFilter::from_json(&serde_json::from_str(item).unwrap());
        assert!(f.is_match("/anything.ts"), "{item} matches everything");
    }
}

#[test]
fn ascii_word_boundaries_match_like_js() {
    // The host spells JS's ASCII \b as (?-u:\b): next to a non-ASCII letter
    // JS sees a boundary, Rust's Unicode \b does not.
    let filters = HookFilter::list_from_json(r#"[{"re":"(?-u:\\b)import(?-u:\\b)"}]"#);
    assert!(filters[0].is_match("éimport x"));
    assert!(!filters[0].is_match("reimported"));
}
