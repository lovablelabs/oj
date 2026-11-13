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
    let env = booting_env(file, ready);
    env.serving();
    env
}

/// As built, before the server listens.
fn booting_env(file: Option<PathBuf>, ready: Option<PathBuf>) -> RuntimeEnv {
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

// A mounted volume publishes its files as symlinks (a Kubernetes secret or
// config map): refused, such a file was invalid for good and every document
// answered the hold page.
#[cfg(unix)]
#[test]
fn a_symlinked_values_file_is_read_through_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let keys = vec![key("VITE_A", "a")];
    let target = dir.path().join("real.json");
    std::fs::write(&target, r#"{"a":"x"}"#).unwrap();
    let link = dir.path().join("env.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (values, state) = read_file(&link, &keys);
    assert_eq!(state, FileState::Complete);
    assert_eq!(values["VITE_A"], "x");
    // A link to nothing is no file; one to a directory is no regular file.
    let dangling = dir.path().join("dangling.json");
    std::os::unix::fs::symlink(dir.path().join("nope.json"), &dangling).unwrap();
    assert_eq!(read_file(&dangling, &keys).1, FileState::Absent);
    let to_dir = dir.path().join("dir.json");
    std::os::unix::fs::symlink(dir.path(), &to_dir).unwrap();
    assert!(matches!(
        read_file(&to_dir, &keys).1,
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

    // Removed: back to the literals. No value was in force, but documents
    // serve again, so the pages are told.
    std::fs::remove_file(&file).unwrap();
    assert!(env.load_file(), "documents stopped waiting");
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
    // The literal reads back as a value, not as the define's expression.
    assert_eq!(keys[0]["value"], "https://baked.supabase.co");
    assert_eq!(keys[0]["source"], "fallback");
    assert_eq!(keys[1]["value"], serde_json::Value::Null);
    assert_eq!(keys[1]["source"], serde_json::Value::Null);
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

// ---- second review ---------------------------------------------------------------------

// The header rule alone let any machine that reaches the port set the
// values: `curl -X POST http://<lan-ip>:5173/@oj/env` has no Origin. The
// connection must come from this machine, and a browser's request is refused
// whatever its origin (a page's own same-origin GET has no Origin either).
#[test]
fn only_this_machine_and_no_browser_may_control_the_values() {
    let headers = HeaderMap::new();
    let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());
    assert!(control_caller(&headers, None), "the router driven directly");
    assert!(control_caller(&headers, ip("127.0.0.1")));
    assert!(control_caller(&headers, ip("::1")));
    assert!(control_caller(&headers, ip("::ffff:127.0.0.1")));
    // No address of this machine: documentation ranges, and the unspecified.
    for remote in [
        "192.0.2.20",
        "203.0.113.5",
        "::ffff:192.0.2.20",
        "2001:db8::1",
        "0.0.0.0",
    ] {
        assert!(!control_caller(&headers, ip(remote)), "{remote}");
    }
    for (name, value) in [
        ("sec-fetch-site", "same-origin"),
        ("sec-fetch-site", "none"),
        ("origin", "http://localhost:5199"),
        ("x-forwarded-host", "preview.example"),
        // A proxy on this machine relaying a caller that is not.
        ("x-forwarded-for", "203.0.113.5"),
        ("forwarded", "for=203.0.113.5"),
        ("x-real-ip", "203.0.113.5"),
        ("via", "1.1 tunnel"),
    ] {
        let mut browser = HeaderMap::new();
        browser.insert(name, value.parse().unwrap());
        assert!(
            !control_caller(&browser, ip("127.0.0.1")),
            "{name}: {value}"
        );
    }
}

// The file removed while pending: no value changed, yet documents serve
// again, and a tab on the hold page must be told. A change that leaves the
// documents waiting, or that moves nothing, tells nobody.
#[test]
fn leaving_pending_reloads_the_pages_though_no_value_changed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    let env = env_with(Some(file.clone()), None);
    assert!(env.pending());
    assert!(!env.load_file(), "still pending, nothing moved");

    std::fs::write(&file, "not json").unwrap();
    assert!(!env.load_file(), "invalid holds the documents too");
    assert!(env.pending());

    std::fs::remove_file(&file).unwrap();
    assert!(env.load_file(), "documents serve again");
    assert_eq!(env.state(), Binding::Unbound);
    assert_eq!(env.generation(), 0, "no value changed");
    assert!(!env.load_file(), "and only once");

    // The same through a set that covers what the file lacks.
    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    assert!(!env.load_file());
    assert!(env.pending());
    assert!(env
        .set(updates(&[
            ("VITE_SUPABASE_URL", Some("/a")),
            ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k")),
            ("VITE_SUPABASE_PROJECT_ID", Some("p")),
        ]))
        .unwrap());
    assert_eq!(env.state(), Binding::Bound);
}

// A stopped server serves no value: the ready file it wrote goes with it.
#[test]
fn the_ready_file_goes_when_the_server_does() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let ready = dir.path().join("ready");
    std::fs::write(&file, bound_json()).unwrap();
    let env = env_with(Some(file.clone()), Some(ready.clone()));
    assert!(ready.exists());
    drop(env);
    assert!(!ready.exists(), "left behind, it said ready with no server");

    // One that was never written is not this server's to remove.
    std::fs::write(&file, r#"{"pending":true}"#).unwrap();
    let env = env_with(Some(file), Some(ready.clone()));
    std::fs::write(&ready, "someone else's\n").unwrap();
    drop(env);
    assert!(ready.exists());
}

// A classic worker cannot hold a static import: it loads the script form of
// the module, which has no export. One of unknown type tries that, and a
// module worker keeps the import.
#[test]
fn a_classic_worker_loads_the_values_with_import_scripts() {
    let env = env();
    let compiled =
        "import \"/@oj/env.js\";\nself.postMessage(globalThis.__oj_env.VITE_SUPABASE_URL);\n";
    let body = |url: &str| worker_body(&env, url, compiled.to_string());
    assert_eq!(body("/src/w.js?worker_file&type=module"), compiled);
    assert_eq!(body("/src/w.js"), compiled);
    assert_eq!(
        body("/src/w.js?type=classic"),
        compiled,
        "not a worker file"
    );
    let classic = body("/src/w.js?worker_file&type=classic");
    assert!(
        classic.starts_with("importScripts(\"/@oj/env.js?classic\");\nself.postMessage("),
        "{classic}"
    );
    assert!(!classic.contains("import \""), "{classic}");
    assert_eq!(classic.lines().count(), compiled.lines().count());

    // In a block, so no name is left in the worker's scope, where the
    // worker's own top-level `env` would be a redeclaration.
    let script = env.classic_js();
    assert!(!script.contains("export"), "{script}");
    assert!(script.starts_with("{\nconst env = {"), "{script}");
    assert!(script.trim_end().ends_with('}'), "{script}");
    assert!(
        script.contains("Object.defineProperty(globalThis, \"__oj_env\""),
        "{script}"
    );
    let parses = |code: &str, kind: oxc_span::SourceType| {
        let allocator = oxc_allocator::Allocator::default();
        let parsed = oxc_parser::Parser::new(&allocator, code, kind).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{code}",
            parsed.diagnostics
        );
    };
    parses(
        &format!("{script}const env = 1;\n"),
        oxc_span::SourceType::cjs(),
    );

    // A worker of unknown type gets the script itself, on the import's
    // line: a module worker runs it as a classic one does (`importScripts`
    // throws there, which left every value undefined).
    let unknown = body("/src/w.js?worker_file&type=ignore");
    assert_eq!(
        unknown.lines().count(),
        compiled.lines().count(),
        "{unknown}"
    );
    assert!(unknown.starts_with("{ const env = {"), "{unknown}");
    assert!(!unknown.contains("importScripts"), "{unknown}");
    assert!(!unknown.contains("import \""), "{unknown}");
    assert!(
        unknown.contains("\"VITE_SUPABASE_URL\": \"https://baked.supabase.co\""),
        "{unknown}"
    );
    parses(&unknown, oxc_span::SourceType::cjs());
    parses(&unknown, oxc_span::SourceType::mjs());
}

// The module carries the generation it was rendered at and asks for the one
// in force once the page has loaded: a page handed the old values with no
// socket open yet reloads by itself.
#[test]
fn the_module_checks_the_generation_it_was_rendered_at() {
    let env = env();
    let js = env.module_js();
    parse_module(&js);
    assert!(js.contains("fetch(\"/@oj/env.js?generation\""), "{js}");
    assert!(js.contains("g === \"0\""), "{js}");
    // Once for a generation in a tab: a stale copy of the module cannot
    // reload the page over and over.
    assert!(
        js.contains("sessionStorage.getItem(\"__oj_env_reloaded\") === g"),
        "{js}"
    );
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    assert!(env.module_js().contains("g === \"1\""));
}

#[test]
fn a_write_beside_the_values_file_is_told_from_one_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let env = env_with(Some(file.clone()), None);
    assert!(env.beside_file(&dir.path().join("..data")));
    assert!(env.beside_file(&file));
    assert!(!env.beside_file(&dir.path().join("sub/env.json")));
    assert!(!env.beside_file(dir.path()));
    assert!(!super::RuntimeEnv::beside_file(
        &env_with(None, None),
        &file
    ));
}

// A directory missing at boot has no real path yet: once it is watched the
// file is known by the path the watcher names it with.
#[cfg(unix)]
#[test]
fn the_watchers_name_for_the_file_is_taken_again_once_its_directory_exists() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap().join("real");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let env = env_with(Some(link.join("state/env.json")), None);
    let watched = real.join("state/env.json");
    assert!(!env.is_file(&watched), "no directory, no real path");
    std::fs::create_dir_all(real.join("state")).unwrap();
    env.armed();
    assert!(env.is_file(&watched));
    assert!(env.beside_file(&real.join("state/other")));
}

// What a restarted server hands the next: its set values, taken for the
// keys still listed and counted as set, so a config or `.env` edit does not
// put a key back on the file's value. Anything else of it is ignored.
#[test]
fn set_values_handed_over_a_restart_are_set_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    std::fs::write(&file, bound_json()).unwrap();
    let env = env_with(Some(file), None);
    env.restore_set(r#"{"VITE_SUPABASE_URL":"/_port_9000","VITE_GONE":"x"}"#);
    let status = env.status();
    assert_eq!(status["keys"][0]["value"], "/_port_9000", "{status}");
    assert_eq!(status["keys"][0]["source"], "set");
    assert_eq!(status["keys"][1]["source"], "file");
    assert_eq!(env.state(), Binding::Bound);

    let env = env_with(None, None);
    for bad in ["", "not json", "[1]", r#"{"VITE_SUPABASE_URL":1}"#] {
        env.restore_set(bad);
        assert!(env.values().is_empty(), "{bad}");
    }
}

// A server bound to one of the machine's own addresses is reached there by
// `oj env`, so its connection comes from that address, not from loopback.
#[test]
fn an_address_of_this_machine_is_a_local_caller() {
    let headers = HeaderMap::new();
    // The address this machine reaches the network from, when it has one.
    let own = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").and_then(|()| s.local_addr()))
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    if let Some(own) = own {
        assert!(control_caller(&headers, Some(own)), "{own}");
    }
    assert!(!control_caller(
        &headers,
        Some("192.0.2.77".parse().unwrap())
    ));
}

// Ready says a server answers with every value: it is not written before
// the server listens (a consumer saw it and was refused the connection, and
// a boot that failed left it behind).
#[test]
fn the_ready_file_waits_for_the_server_to_listen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("env.json");
    let ready = dir.path().join("ready");
    std::fs::write(&file, bound_json()).unwrap();
    let env = booting_env(Some(file.clone()), Some(ready.clone()));
    assert_eq!(env.state(), Binding::Bound);
    assert!(!ready.exists(), "bound, but nothing listens yet");
    env.load_file();
    assert!(!ready.exists());
    env.serving();
    assert!(ready.exists());
}

// The file is read no further than the size limit, whatever it was when
// first looked at.
#[test]
fn the_values_file_is_read_no_further_than_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let keys = vec![key("VITE_A", "a")];
    let file = dir.path().join("env.json");
    let filler = "x".repeat(MAX_FILE_BYTES as usize);
    std::fs::write(&file, format!(r#"{{"a":"{filler}"}}"#)).unwrap();
    assert!(matches!(
        read_file(&file, &keys).1,
        FileState::Invalid(e) if e.contains("larger than")
    ));
    let fits = "x".repeat(MAX_FILE_BYTES as usize - 8);
    std::fs::write(&file, format!(r#"{{"a":"{fits}"}}"#)).unwrap();
    assert_eq!(read_file(&file, &keys).1, FileState::Complete);
}
