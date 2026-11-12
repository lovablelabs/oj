use super::*;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

fn key(key: &str, field: &str) -> RuntimeKey {
    RuntimeKey {
        key: key.into(),
        field: field.into(),
    }
}

fn supabase_keys() -> Vec<RuntimeKey> {
    vec![
        key("VITE_SUPABASE_URL", "url"),
        key("VITE_SUPABASE_PUBLISHABLE_KEY", "anonKey"),
        key("VITE_SUPABASE_PROJECT_ID", "projectId"),
    ]
}

fn defines() -> Vec<(String, String)> {
    vec![
        ("import.meta.env.MODE".into(), "\"development\"".into()),
        ("import.meta.env.DEV".into(), "true".into()),
        ("import.meta.env.BASE_URL".into(), "\"/\"".into()),
        (
            "import.meta.env.VITE_SUPABASE_URL".into(),
            "\"https://baked.supabase.co\"".into(),
        ),
        ("import.meta.env.VITE_TITLE".into(), "\"My <app>\"".into()),
        ("import.meta.env".into(), "({})".into()),
        ("process.env.NODE_ENV".into(), "\"development\"".into()),
        ("__APP__".into(), "1".into()),
    ]
}

fn env_with(file: Option<PathBuf>, ready: Option<PathBuf>) -> RuntimeEnv {
    RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: supabase_keys(),
            file,
            ready_file: ready,
        },
        &defines(),
        None,
    )
}

fn env() -> RuntimeEnv {
    env_with(None, None)
}

fn updates(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.map(str::to_owned)))
        .collect()
}

fn parse_module(code: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
    assert!(
        parsed.diagnostics.is_empty() && !parsed.fatal_error,
        "module does not parse: {:?}\n{code}",
        parsed.diagnostics
    );
}

fn bound_json() -> &'static str {
    r#"{"url":"/_port_8001","anonKey":"sb_publishable_abc","projectId":"","storageKey":"sb-localhost-auth-token"}"#
}

// ---- the values file ---------------------------------------------------------

#[test]
fn a_complete_file_binds_every_key_by_its_field() {
    let (values, state) = parse_values(
        br#"{"url":"/_port_8001","anonKey":"sb_publishable_abc","projectId":"p1","extra":"ignored"}"#,
        &supabase_keys(),
    );
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_SUPABASE_URL"], "/_port_8001");
    assert_eq!(
        values["VITE_SUPABASE_PUBLISHABLE_KEY"],
        "sb_publishable_abc"
    );
    assert_eq!(values["VITE_SUPABASE_PROJECT_ID"], "p1");
    assert_eq!(values.len(), 3, "fields the keys do not name are ignored");
}

#[test]
fn a_flat_file_names_the_keys_themselves() {
    let keys = vec![key("VITE_A", "VITE_A"), key("VITE_B", "VITE_B")];
    let (values, state) = parse_values(br#"{"VITE_A":"a","VITE_B":"b"}"#, &keys);
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_A"], "a");
    assert_eq!(values["VITE_B"], "b");
}

#[test]
fn the_pending_marker_leaves_every_key_missing() {
    let (values, state) = parse_values(br#"{"pending":true}"#, &supabase_keys());
    assert!(values.is_empty());
    assert_eq!(
        state,
        FileState::Pending {
            missing: vec![
                "VITE_SUPABASE_URL".into(),
                "VITE_SUPABASE_PUBLISHABLE_KEY".into(),
                "VITE_SUPABASE_PROJECT_ID".into()
            ]
        }
    );
}

#[test]
fn a_partial_file_binds_what_it_has_and_names_the_rest() {
    let (values, state) =
        parse_values(br#"{"url":"/_port_8001","anonKey":null}"#, &supabase_keys());
    assert_eq!(values.len(), 1);
    assert_eq!(values["VITE_SUPABASE_URL"], "/_port_8001");
    assert_eq!(
        state,
        FileState::Pending {
            missing: vec![
                "VITE_SUPABASE_PUBLISHABLE_KEY".into(),
                "VITE_SUPABASE_PROJECT_ID".into()
            ]
        }
    );
}

#[test]
fn an_empty_string_is_a_value() {
    // A draft backend publishes `"projectId": ""`; the key is bound, to nothing.
    let keys = vec![key("VITE_A", "a")];
    let (values, state) = parse_values(br#"{"a":""}"#, &keys);
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_A"], "");
}

#[test]
fn a_field_of_another_type_makes_the_file_invalid() {
    let keys = vec![key("VITE_A", "a"), key("VITE_B", "b")];
    for (body, kind) in [
        (r#"{"a":"x","b":1}"#, "a number"),
        (r#"{"a":"x","b":true}"#, "a boolean"),
        (r#"{"a":"x","b":["x"]}"#, "an array"),
        (r#"{"a":"x","b":{"v":"x"}}"#, "an object"),
    ] {
        let (values, state) = parse_values(body.as_bytes(), &keys);
        assert!(
            values.is_empty(),
            "{body}: nothing of an invalid file is used"
        );
        match state {
            FileState::Invalid(e) => assert!(
                e.contains("field b for VITE_B") && e.contains(kind),
                "{body}: {e}"
            ),
            other => panic!("{body}: {other:?}"),
        }
    }
}

#[test]
fn a_file_that_is_not_a_json_object_is_invalid() {
    let keys = vec![key("VITE_A", "a")];
    for body in [
        &b"not json"[..],
        b"",
        b"[\"a\"]",
        b"\"a\"",
        b"null",
        b"{\"a\":\"x\",}",
        b"\xff\xfe",
    ] {
        let (values, state) = parse_values(body, &keys);
        assert!(values.is_empty());
        assert!(
            matches!(state, FileState::Invalid(_)),
            "{:?}: {state:?}",
            String::from_utf8_lossy(body)
        );
    }
}

#[test]
fn a_proto_key_is_just_a_field_nobody_reads() {
    let keys = vec![key("VITE_A", "a")];
    let (values, state) = parse_values(br#"{"__proto__":{"a":"evil"},"a":"x"}"#, &keys);
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_A"], "x");
}

#[test]
fn a_file_may_name_a_proto_field_only_when_asked() {
    let keys = vec![key("VITE_A", "__proto__")];
    let (values, state) = parse_values(br#"{"__proto__":"x"}"#, &keys);
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_A"], "x");
}

#[test]
fn no_keys_means_any_object_is_complete() {
    let (values, state) = parse_values(br#"{"pending":true}"#, &[]);
    assert!(values.is_empty());
    assert_eq!(state, FileState::Complete);
}

#[test]
fn read_file_reports_absent_oversize_and_non_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let keys = vec![key("VITE_A", "a")];
    let path = dir.path().join("env.json");
    assert_eq!(read_file(&path, &keys).1, FileState::Absent);

    std::fs::write(&path, r#"{"a":"x"}"#).unwrap();
    assert_eq!(
        read_file(&path, &keys),
        (
            {
                let mut m = BTreeMap::new();
                m.insert("VITE_A".to_string(), "x".to_string());
                m
            },
            FileState::Complete
        )
    );

    let big = format!(r#"{{"a":"{}"}}"#, "x".repeat(MAX_FILE_BYTES as usize));
    std::fs::write(&path, big).unwrap();
    match read_file(&path, &keys).1 {
        FileState::Invalid(e) => assert!(e.contains("larger than"), "{e}"),
        other => panic!("{other:?}"),
    }

    assert!(matches!(
        read_file(dir.path(), &keys).1,
        FileState::Invalid(e) if e.contains("not a regular file")
    ));
}

#[cfg(unix)]
#[test]
fn a_symlinked_values_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let keys = vec![key("VITE_A", "a")];
    let target = dir.path().join("real.json");
    std::fs::write(&target, r#"{"a":"x"}"#).unwrap();
    let link = dir.path().join("env.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        read_file(&link, &keys).1,
        FileState::Invalid(e) if e.contains("not a regular file")
    ));
}

// ---- values in force -----------------------------------------------------------

#[test]
fn without_a_file_or_a_set_every_key_serves_its_literal() {
    let env = env();
    assert_eq!(env.state(), Binding::Unbound);
    assert!(!env.pending());
    assert!(env.values().is_empty());
    assert_eq!(env.generation(), 0);
    let js = env.module_js();
    parse_module(&js);
    assert!(
        js.contains("\"VITE_SUPABASE_URL\": \"https://baked.supabase.co\""),
        "the literal as the define held it:\n{js}"
    );
    assert!(
        !js.contains("VITE_SUPABASE_PUBLISHABLE_KEY"),
        "a key the env never had is left out, so its read is undefined:\n{js}"
    );
    assert!(js.contains("\"MODE\": \"development\""), "{js}");
    assert!(js.contains("\"DEV\": true"), "{js}");
    assert!(js.contains("\"VITE_TITLE\": \"My <app>\""), "{js}");
    assert!(
        !js.contains("NODE_ENV") && !js.contains("__APP__"),
        "only import.meta.env fields:\n{js}"
    );
    assert!(
        js.contains("Object.defineProperty(globalThis, \"__oj_env\""),
        "{js}"
    );
    assert!(js.contains("export default env;"), "{js}");
}

#[test]
fn a_set_value_wins_over_the_file_which_wins_over_the_literal() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, bound_json()).unwrap();
    let env = env_with(Some(file), None);
    assert_eq!(env.state(), Binding::Bound);
    assert_eq!(env.values()["VITE_SUPABASE_URL"], "/_port_8001");
    assert_eq!(env.generation(), 1, "the boot read counts as a change");

    assert!(env
        .set(updates(&[("VITE_SUPABASE_URL", Some("/_port_9000"))]))
        .unwrap());
    assert_eq!(env.values()["VITE_SUPABASE_URL"], "/_port_9000");
    assert_eq!(env.generation(), 2);
    let js = env.module_js();
    parse_module(&js);
    assert!(
        js.contains("\"VITE_SUPABASE_URL\": \"/_port_9000\""),
        "{js}"
    );
    assert!(!js.contains("baked.supabase.co"), "{js}");

    // Unsetting falls back to the file.
    assert!(env.set(updates(&[("VITE_SUPABASE_URL", None)])).unwrap());
    assert_eq!(env.values()["VITE_SUPABASE_URL"], "/_port_8001");
    assert_eq!(env.generation(), 3);
}

#[test]
fn the_generation_moves_only_when_the_values_in_force_do() {
    let env = env();
    assert!(env
        .set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap());
    assert_eq!(env.generation(), 1);
    assert!(
        !env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
            .unwrap(),
        "the same value again is no change"
    );
    assert_eq!(env.generation(), 1);
    assert!(
        !env.set(updates(&[("VITE_SUPABASE_PROJECT_ID", None)]))
            .unwrap(),
        "unsetting what was never set is no change"
    );
    assert_eq!(env.generation(), 1);
    assert!(!env.set(BTreeMap::new()).unwrap());
    assert!(!env.load_file(), "no file configured: nothing to load");
    assert_eq!(env.generation(), 1);
}

#[test]
fn a_key_outside_the_list_is_refused_whole() {
    let env = env();
    let err = env
        .set(updates(&[
            ("VITE_SUPABASE_URL", Some("/a")),
            ("VITE_NOPE", Some("x")),
            ("MODE", Some("production")),
        ]))
        .unwrap_err();
    assert!(err.contains("VITE_NOPE") && err.contains("MODE"), "{err}");
    assert!(
        err.contains("VITE_SUPABASE_URL"),
        "the accepted keys are listed: {err}"
    );
    assert!(
        env.values().is_empty(),
        "nothing of a refused request applies"
    );
    assert_eq!(env.generation(), 0);
}

#[test]
fn binding_states_follow_the_file_and_the_sets() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    // No file yet: unbound, then partial once something is set.
    let env = env_with(Some(file.clone()), None);
    assert_eq!(env.state(), Binding::Unbound);
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    assert_eq!(env.state(), Binding::Partial);
    assert!(
        !env.pending(),
        "no file on disk: documents serve on the literals"
    );

    // The backend publishes its pending marker: documents wait.
    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    assert!(!env.load_file(), "the marker carries no value");
    assert_eq!(env.state(), Binding::Pending);
    assert!(env.pending());

    // A set covering the rest ends the wait without the file.
    env.set(updates(&[
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k")),
        ("VITE_SUPABASE_PROJECT_ID", Some("p")),
    ]))
    .unwrap();
    assert_eq!(env.state(), Binding::Bound);

    // Back to pending when the sets go and the file is still incomplete.
    env.set(updates(&[
        ("VITE_SUPABASE_URL", None),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", None),
        ("VITE_SUPABASE_PROJECT_ID", None),
    ]))
    .unwrap();
    assert_eq!(env.state(), Binding::Pending);

    // The file completes.
    std::fs::write(&file, bound_json()).unwrap();
    assert!(env.load_file());
    assert_eq!(env.state(), Binding::Bound);

    // An invalid rewrite holds documents and keeps nothing of the file.
    std::fs::write(&file, r#"{"url":1}"#).unwrap();
    assert!(env.load_file());
    assert_eq!(env.state(), Binding::Pending);
    assert!(env.values().is_empty());

    // Removed: back to the literals.
    std::fs::remove_file(&file).unwrap();
    assert!(!env.load_file(), "nothing was in force");
    assert_eq!(env.state(), Binding::Unbound);
}

#[test]
fn the_ready_file_exists_exactly_while_bound() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let ready = dir.path().join("ready");
    let env = env_with(Some(file.clone()), Some(ready.clone()));
    assert!(!ready.exists(), "unbound at boot");

    std::fs::write(&file, bound_json()).unwrap();
    env.load_file();
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "1\n");

    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    env.load_file();
    assert!(!ready.exists(), "pending removes it");

    env.set(updates(&[
        ("VITE_SUPABASE_URL", Some("/a")),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k")),
        ("VITE_SUPABASE_PROJECT_ID", Some("p")),
    ]))
    .unwrap();
    assert!(ready.exists(), "sets bind too");

    env.set(updates(&[("VITE_SUPABASE_URL", None)])).unwrap();
    assert!(!ready.exists());
    // Removing an already removed ready file is fine.
    env.set(updates(&[("VITE_SUPABASE_PUBLISHABLE_KEY", None)]))
        .unwrap();
    assert!(!ready.exists());
}

#[test]
fn a_bound_file_at_boot_is_ready_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let ready = dir.path().join("ready");
    std::fs::write(&file, bound_json()).unwrap();
    let env = env_with(Some(file), Some(ready.clone()));
    assert_eq!(env.state(), Binding::Bound);
    assert!(ready.exists());
}

// ---- the served module -----------------------------------------------------------

#[test]
fn the_module_escapes_values_as_json() {
    let env = env();
    env.set(updates(&[(
        "VITE_SUPABASE_URL",
        Some("</script><script>alert(\"x\")</script>\n\u{2028}\\"),
    )]))
    .unwrap();
    let js = env.module_js();
    parse_module(&js);
    assert!(
        js.contains(r#""VITE_SUPABASE_URL": "</script><script>alert(\"x\")</script>\n \\""#),
        "{js}"
    );
}

#[test]
fn the_module_keeps_static_expressions_as_the_define_had_them() {
    let keys = vec![key("VITE_RT", "rt")];
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys,
            file: None,
            ready_file: None,
        },
        &[
            ("import.meta.env.FLAG".into(), "__FLAG__".into()),
            ("import.meta.env.N".into(), "1 + 2".into()),
            ("import.meta.env.VITE_RT".into(), "\"rt-literal\"".into()),
        ],
        None,
    );
    let js = env.module_js();
    parse_module(&js);
    assert!(js.contains("\"FLAG\": __FLAG__"), "{js}");
    assert!(js.contains("\"N\": 1 + 2"), "{js}");
    assert!(js.contains("\"VITE_RT\": \"rt-literal\""), "{js}");
}

#[test]
fn the_module_url_follows_the_base() {
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: vec![key("VITE_A", "a")],
            file: None,
            ready_file: None,
        },
        &[],
        Some("/app/"),
    );
    assert_eq!(env.compiler_defines().module_url, "/app/@oj/env.js");
    assert_eq!(env.compiler_defines().global, GLOBAL);
    assert_eq!(env.compiler_defines().keys, ["VITE_A"]);
    let none = env_with(None, None);
    assert_eq!(none.compiler_defines().module_url, "/@oj/env.js");
}

#[test]
fn an_empty_define_list_still_serves_a_valid_module() {
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: vec![key("VITE_A", "a")],
            file: None,
            ready_file: None,
        },
        &[],
        None,
    );
    let js = env.module_js();
    parse_module(&js);
    assert!(js.contains("const env = {\n\n};"), "{js}");
}

// ---- status -----------------------------------------------------------------------

#[test]
fn the_status_names_every_key_its_value_and_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, r#"{"url":"/_port_8001"}"#).unwrap();
    let env = env_with(Some(file.clone()), Some(dir.path().join("ready")));
    env.set(updates(&[("VITE_SUPABASE_PROJECT_ID", Some("p"))]))
        .unwrap();
    let status = env.status();
    assert_eq!(status["state"], "pending");
    assert_eq!(status["generation"], 2);
    assert_eq!(status["module"], "/@oj/env.js");
    assert_eq!(status["file"]["state"], "pending");
    assert_eq!(status["file"]["path"], file.display().to_string());
    assert_eq!(
        status["file"]["missing"],
        serde_json::json!(["VITE_SUPABASE_PUBLISHABLE_KEY", "VITE_SUPABASE_PROJECT_ID"])
    );
    assert_eq!(
        status["readyFile"],
        dir.path().join("ready").display().to_string()
    );
    let keys = status["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0]["key"], "VITE_SUPABASE_URL");
    assert_eq!(keys[0]["field"], "url");
    assert_eq!(keys[0]["value"], "/_port_8001");
    assert_eq!(keys[0]["source"], "file");
    assert_eq!(keys[1]["key"], "VITE_SUPABASE_PUBLISHABLE_KEY");
    assert_eq!(keys[1]["value"], serde_json::Value::Null);
    assert_eq!(keys[1]["source"], serde_json::Value::Null);
    assert_eq!(keys[2]["value"], "p");
    assert_eq!(keys[2]["source"], "set");
}

#[test]
fn the_status_reports_a_literal_as_the_fallback_and_an_invalid_file_with_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, "nope").unwrap();
    let env = env_with(Some(file), None);
    let status = env.status();
    assert_eq!(status["state"], "pending");
    assert_eq!(status["file"]["state"], "invalid");
    assert!(status["file"]["error"]
        .as_str()
        .unwrap()
        .contains("not JSON"));
    let keys = status["keys"].as_array().unwrap();
    assert_eq!(keys[0]["value"], "\"https://baked.supabase.co\"");
    assert_eq!(keys[0]["source"], "fallback");
    assert_eq!(status["readyFile"], serde_json::Value::Null);
}

#[test]
fn the_status_of_an_unconfigured_file_says_absent() {
    let status = env().status();
    assert_eq!(status["state"], "unbound");
    assert_eq!(status["file"]["state"], "absent");
    assert_eq!(status["file"]["path"], serde_json::Value::Null);
}

// ---- html -----------------------------------------------------------------------------

#[test]
fn the_html_map_lays_the_values_in_force_over_the_literals() {
    let env = env();
    let mut base = BTreeMap::new();
    base.insert("MODE".to_string(), "development".to_string());
    base.insert(
        "VITE_SUPABASE_URL".to_string(),
        "https://baked.supabase.co".to_string(),
    );
    assert!(
        matches!(env.html_env(&base), Cow::Borrowed(_)),
        "nothing in force: the literals as they are"
    );
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/_port_8001"))]))
        .unwrap();
    let over = env.html_env(&base);
    assert_eq!(over["VITE_SUPABASE_URL"], "/_port_8001");
    assert_eq!(over["MODE"], "development");
    assert_eq!(
        base["VITE_SUPABASE_URL"], "https://baked.supabase.co",
        "the base is untouched"
    );
}

// ---- config and helpers --------------------------------------------------------------

#[test]
fn the_config_comes_from_the_knobs() {
    let knobs =
        |vars: &[(&str, &str)]| oj_env::Env::from_vars(vars.iter().map(|(k, v)| (*k, *v))).knobs;
    assert_eq!(RuntimeEnvConfig::from_knobs(&knobs(&[])).unwrap(), None);
    let cfg = RuntimeEnvConfig::from_knobs(&knobs(&[
        ("OJ_RUNTIME_ENV_KEYS", "VITE_SUPABASE_URL=url,VITE_X"),
        ("OJ_RUNTIME_ENV_FILE", "/persist/env.json"),
        ("OJ_RUNTIME_ENV_READY_FILE", "/run/ready"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        cfg.keys,
        [key("VITE_SUPABASE_URL", "url"), key("VITE_X", "VITE_X")]
    );
    assert_eq!(cfg.file.as_deref(), Some(Path::new("/persist/env.json")));
    assert_eq!(cfg.ready_file.as_deref(), Some(Path::new("/run/ready")));

    let err = RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_FILE", "/persist/env.json")]))
        .unwrap_err();
    assert!(
        err.contains("OJ_RUNTIME_ENV_KEYS"),
        "a file without keys: {err}"
    );
    let err =
        RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_KEYS", "not-a-key")])).unwrap_err();
    assert!(err.contains("not a valid"), "{err}");
    assert_eq!(
        RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_KEYS", " , ")])).unwrap(),
        None,
        "a blank spec names no keys"
    );
}

#[test]
fn is_file_matches_the_path_as_given_and_as_the_watcher_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let env = env_with(Some(file.clone()), None);
    assert!(env.is_file(&file));
    assert!(env.is_file(&std::fs::canonicalize(dir.path()).unwrap().join("env.json")));
    assert!(!env.is_file(&dir.path().join("other.json")));
    assert!(!env_with(None, None).is_file(&file));
}

#[test]
fn the_cache_salt_follows_the_key_list_and_the_module_url() {
    let make = |keys: Vec<RuntimeKey>, base: Option<&str>| {
        RuntimeEnv::new(
            RuntimeEnvConfig {
                keys,
                file: None,
                ready_file: None,
            },
            &[],
            base,
        )
        .cache_salt()
    };
    let a = make(vec![key("VITE_A", "a")], None);
    assert_eq!(
        a,
        make(vec![key("VITE_A", "other-field")], None),
        "fields do not change compiles"
    );
    assert_ne!(a, make(vec![key("VITE_B", "b")], None));
    assert_ne!(a, make(vec![key("VITE_A", "a"), key("VITE_B", "b")], None));
    assert_ne!(a, make(vec![key("VITE_A", "a")], Some("/app/")));
    assert_eq!(a.len(), 16);
}

#[test]
fn updates_parse_strings_and_nulls_only() {
    let parsed = parse_updates(br#"{"VITE_A":"x","VITE_B":null}"#).unwrap();
    assert_eq!(parsed["VITE_A"].as_deref(), Some("x"));
    assert_eq!(parsed["VITE_B"], None);
    assert!(parse_updates(br#"{"VITE_A":1}"#)
        .unwrap_err()
        .contains("a number"));
    assert!(parse_updates(br#"["VITE_A"]"#)
        .unwrap_err()
        .contains("JSON object"));
    assert!(parse_updates(b"").unwrap_err().contains("not JSON"));
    assert!(parse_updates(b"{}").unwrap().is_empty());
}

#[test]
fn only_a_direct_local_caller_may_control_the_values() {
    let mut headers = HeaderMap::new();
    assert!(local_caller(&headers));
    headers.insert("host", "127.0.0.1:5199".parse().unwrap());
    assert!(local_caller(&headers));
    let mut browser = headers.clone();
    browser.insert(header::ORIGIN, "http://localhost:5199".parse().unwrap());
    assert!(!local_caller(&browser));
    let mut proxied = headers.clone();
    proxied.insert("x-forwarded-host", "preview.example".parse().unwrap());
    assert!(!local_caller(&proxied));
}

// ---- review follow-ups ---------------------------------------------------------------

#[test]
fn a_stale_ready_file_goes_at_boot_even_without_a_values_file() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    std::fs::write(&ready, "1\n").unwrap();
    let env = env_with(None, Some(ready.clone()));
    assert_eq!(env.state(), Binding::Unbound);
    assert!(
        !ready.exists(),
        "a previous run's ready file does not vouch for this one"
    );
    env.set(updates(&[
        ("VITE_SUPABASE_URL", Some("/a")),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k")),
        ("VITE_SUPABASE_PROJECT_ID", Some("p")),
    ]))
    .unwrap();
    assert!(ready.exists());
}

#[test]
fn relative_paths_are_taken_from_the_working_directory() {
    let cwd = std::env::current_dir().unwrap();
    let env = env_with(Some("state/env.json".into()), Some("state/ready".into()));
    assert_eq!(env.file(), Some(cwd.join("state/env.json").as_path()));
    assert!(env.is_file(&cwd.join("state/env.json")));
    assert!(
        !env.is_file(Path::new("state/env.json")),
        "the watcher names absolute paths"
    );
    let status = env.status();
    assert_eq!(
        status["file"]["path"],
        cwd.join("state/env.json").display().to_string()
    );
    assert_eq!(
        status["readyFile"],
        cwd.join("state/ready").display().to_string()
    );
}

#[test]
fn the_served_object_is_plain_so_a_field_assignment_does_not_throw() {
    let js = env().module_js();
    assert!(!js.contains("Object.freeze"), "{js}");
    assert!(
        js.contains("writable: false"),
        "the global itself stays fixed:\n{js}"
    );
    assert!(js.contains("configurable: true"), "{js}");
}

#[test]
fn the_banner_names_the_keys_and_the_source() {
    assert_eq!(
        env().banner(),
        "runtime env: VITE_SUPABASE_URL, VITE_SUPABASE_PUBLISHABLE_KEY, VITE_SUPABASE_PROJECT_ID from POST /@oj/env only"
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    let banner = env_with(Some(file.clone()), None).banner();
    assert!(
        banner.ends_with(&format!(
            "from file {} (pending, VITE_SUPABASE_URL, VITE_SUPABASE_PUBLISHABLE_KEY, VITE_SUPABASE_PROJECT_ID missing)",
            file.display()
        )),
        "{banner}"
    );
    std::fs::write(&file, "nope").unwrap();
    let banner = env_with(Some(file), None).banner();
    assert!(banner.contains("(invalid, not JSON"), "{banner}");
}
