use super::*;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|k| k.to_string()).collect()
}

fn supabase_keys() -> Vec<String> {
    keys(&[
        "VITE_SUPABASE_URL",
        "VITE_SUPABASE_PUBLISHABLE_KEY",
        "VITE_SUPABASE_PROJECT_ID",
    ])
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

fn env_with(ready: Option<PathBuf>) -> RuntimeEnv {
    let env = booting_env(ready);
    env.serving();
    env
}

/// As built, before the server listens.
fn booting_env(ready: Option<PathBuf>) -> RuntimeEnv {
    RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: supabase_keys(),
            ready_file: ready,
        },
        &defines(),
        None,
    )
}

fn env() -> RuntimeEnv {
    env_with(None)
}

/// Every key, set.
fn bound() -> BTreeMap<String, Option<String>> {
    updates(&[
        ("VITE_SUPABASE_URL", Some("/_port_8001")),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("sb_publishable_abc")),
        ("VITE_SUPABASE_PROJECT_ID", Some("")),
    ])
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

#[test]
fn an_empty_string_is_a_value() {
    // A draft backend has no project id: the key is bound, to nothing.
    let env = env();
    assert!(env.set(bound()).unwrap());
    assert_eq!(env.state(), Binding::Bound);
    assert_eq!(env.values()["VITE_SUPABASE_PROJECT_ID"], "");
    let js = env.module_js();
    parse_module(&js);
    assert!(js.contains("\"VITE_SUPABASE_PROJECT_ID\": \"\""), "{js}");
}

// ---- values in force -----------------------------------------------------------

#[test]
fn with_nothing_set_every_key_serves_its_literal() {
    let env = env();
    assert_eq!(env.state(), Binding::Unbound);
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
fn a_set_value_wins_over_the_literal() {
    let env = env();
    assert!(env.module_js().contains("baked.supabase.co"));

    assert!(env
        .set(updates(&[("VITE_SUPABASE_URL", Some("/_port_9000"))]))
        .unwrap());
    assert_eq!(env.values()["VITE_SUPABASE_URL"], "/_port_9000");
    assert_eq!(env.generation(), 1);
    let js = env.module_js();
    parse_module(&js);
    assert!(
        js.contains("\"VITE_SUPABASE_URL\": \"/_port_9000\""),
        "{js}"
    );
    assert!(!js.contains("baked.supabase.co"), "{js}");

    // Unsetting falls back to the literal.
    assert!(env.set(updates(&[("VITE_SUPABASE_URL", None)])).unwrap());
    assert!(env.values().is_empty());
    assert_eq!(env.generation(), 2);
    assert!(env.module_js().contains("baked.supabase.co"));
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
fn binding_states_follow_the_sets() {
    let env = env();
    assert_eq!(env.state(), Binding::Unbound);
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    assert_eq!(env.state(), Binding::Partial);
    env.set(updates(&[("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k"))]))
        .unwrap();
    assert_eq!(env.state(), Binding::Partial, "one key still has no value");

    // A key with a literal is not bound by it: only a set value counts.
    env.set(updates(&[
        ("VITE_SUPABASE_URL", None),
        ("VITE_SUPABASE_PROJECT_ID", Some("")),
    ]))
    .unwrap();
    assert_eq!(env.state(), Binding::Partial);

    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    assert_eq!(env.state(), Binding::Bound);

    env.set(updates(&[
        ("VITE_SUPABASE_URL", None),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", None),
        ("VITE_SUPABASE_PROJECT_ID", None),
    ]))
    .unwrap();
    assert_eq!(env.state(), Binding::Unbound);
}

#[test]
fn the_ready_file_exists_exactly_while_bound() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let env = env_with(Some(ready.clone()));
    assert!(!ready.exists(), "unbound at boot");

    env.set(updates(&[
        ("VITE_SUPABASE_URL", Some("/a")),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("k")),
    ]))
    .unwrap();
    assert!(!ready.exists(), "partial: one key has no value");

    env.set(updates(&[("VITE_SUPABASE_PROJECT_ID", Some("p"))]))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "1\n");

    env.set(updates(&[("VITE_SUPABASE_URL", None)])).unwrap();
    assert!(!ready.exists());
    // Removing an already removed ready file is fine.
    env.set(updates(&[("VITE_SUPABASE_PUBLISHABLE_KEY", None)]))
        .unwrap();
    assert!(!ready.exists());

    env.set(bound()).unwrap();
    assert!(ready.exists(), "bound again");
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
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: keys(&["VITE_RT"]),
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
            keys: keys(&["VITE_A"]),
            ready_file: None,
        },
        &[],
        Some("/app/"),
    );
    assert_eq!(env.compiler_defines().module_url, "/app/@oj/env.js");
    assert_eq!(env.compiler_defines().global, GLOBAL);
    assert_eq!(env.compiler_defines().keys, ["VITE_A"]);
    let none = self::env();
    assert_eq!(none.compiler_defines().module_url, "/@oj/env.js");
}

#[test]
fn an_empty_define_list_still_serves_a_valid_module() {
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: keys(&["VITE_A"]),
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
    let env = env_with(Some(dir.path().join("ready")));
    env.set(updates(&[("VITE_SUPABASE_PROJECT_ID", Some("p"))]))
        .unwrap();
    let status = env.status();
    assert_eq!(status["state"], "partial");
    assert_eq!(status["generation"], 1);
    assert_eq!(status["module"], "/@oj/env.js");
    assert_eq!(
        status["readyFile"],
        dir.path().join("ready").display().to_string()
    );
    let mut fields: Vec<&str> = status
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["generation", "keys", "module", "readyFile", "state"],
        "{status}"
    );
    let keys = status["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(
        keys[0],
        serde_json::json!({
            "key": "VITE_SUPABASE_URL",
            // The literal reads back as a value, not as the define's expression.
            "value": "https://baked.supabase.co",
            "source": "fallback",
        })
    );
    assert_eq!(
        keys[1],
        serde_json::json!({
            "key": "VITE_SUPABASE_PUBLISHABLE_KEY",
            "value": null,
            "source": null,
        })
    );
    assert_eq!(
        keys[2],
        serde_json::json!({
            "key": "VITE_SUPABASE_PROJECT_ID",
            "value": "p",
            "source": "set",
        })
    );
}

#[test]
fn the_status_follows_the_state() {
    let env = env();
    let status = env.status();
    assert_eq!(status["state"], "unbound");
    assert_eq!(status["generation"], 0);
    assert_eq!(status["readyFile"], serde_json::Value::Null);
    env.set(bound()).unwrap();
    let status = env.status();
    assert_eq!(status["state"], "bound");
    assert_eq!(status["keys"][0]["value"], "/_port_8001");
    assert_eq!(status["keys"][0]["source"], "set");
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
        ("OJ_RUNTIME_ENV_KEYS", "VITE_SUPABASE_URL, VITE_X"),
        ("OJ_RUNTIME_ENV_READY_FILE", "/run/ready"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.keys, ["VITE_SUPABASE_URL", "VITE_X"]);
    assert_eq!(cfg.ready_file.as_deref(), Some(Path::new("/run/ready")));

    let err = RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_READY_FILE", "/run/ready")]))
        .unwrap_err();
    assert!(
        err.contains("OJ_RUNTIME_ENV_KEYS"),
        "a ready file without keys: {err}"
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

// A key is no longer mapped to a field: the entry is a startup error that
// names it, not a key called `VITE_A=field`.
#[test]
fn a_mapped_key_is_a_startup_error_naming_the_entry() {
    let knobs =
        |vars: &[(&str, &str)]| oj_env::Env::from_vars(vars.iter().map(|(k, v)| (*k, *v))).knobs;
    let err = RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_KEYS", "VITE_A=field")]))
        .unwrap_err();
    assert!(err.starts_with("OJ_RUNTIME_ENV_KEYS:"), "{err}");
    assert!(err.contains("`VITE_A=field`"), "{err}");
    let err =
        RuntimeEnvConfig::from_knobs(&knobs(&[("OJ_RUNTIME_ENV_KEYS", "VITE_B,VITE_A=field")]))
            .unwrap_err();
    assert!(err.contains("`VITE_A=field`"), "{err}");
}

// `OJ_RUNTIME_ENV_FILE` is no setting: like any variable oj does not read,
// it changes nothing and is no error, with keys or without.
#[test]
fn the_runtime_env_file_variable_is_ignored() {
    let knobs =
        |vars: &[(&str, &str)]| oj_env::Env::from_vars(vars.iter().map(|(k, v)| (*k, *v))).knobs;
    let stray = ("OJ_RUNTIME_ENV_FILE", "/persist/env.json");
    assert_eq!(
        RuntimeEnvConfig::from_knobs(&knobs(&[stray])).unwrap(),
        None
    );
    let keyed = ("OJ_RUNTIME_ENV_KEYS", "VITE_A");
    assert_eq!(
        RuntimeEnvConfig::from_knobs(&knobs(&[keyed, stray])).unwrap(),
        RuntimeEnvConfig::from_knobs(&knobs(&[keyed])).unwrap()
    );
}

#[test]
fn the_cache_salt_follows_the_key_list_and_the_module_url() {
    let make = |names: &[&str], base: Option<&str>| {
        RuntimeEnv::new(
            RuntimeEnvConfig {
                keys: keys(names),
                ready_file: None,
            },
            &[],
            base,
        )
        .cache_salt()
    };
    let a = make(&["VITE_A"], None);
    assert_eq!(a, make(&["VITE_A"], None));
    assert_ne!(a, make(&["VITE_B"], None));
    assert_ne!(a, make(&["VITE_A", "VITE_B"], None));
    assert_ne!(a, make(&["VITE_A"], Some("/app/")));
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
fn a_stale_ready_file_goes_at_boot() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    std::fs::write(&ready, "1\n").unwrap();
    let env = env_with(Some(ready.clone()));
    assert_eq!(env.state(), Binding::Unbound);
    assert!(
        !ready.exists(),
        "a previous run's ready file does not vouch for this one"
    );
    env.set(bound()).unwrap();
    assert!(ready.exists());
}

#[test]
fn a_relative_ready_file_is_taken_from_the_working_directory() {
    let cwd = std::env::current_dir().unwrap();
    let env = env_with(Some("state/ready".into()));
    assert_eq!(
        env.status()["readyFile"],
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
        "runtime env: VITE_SUPABASE_URL, VITE_SUPABASE_PUBLISHABLE_KEY, VITE_SUPABASE_PROJECT_ID, values from POST /@oj/env"
    );
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

// A stopped server serves no value: the ready file it wrote goes with it.
#[test]
fn the_ready_file_goes_when_the_server_does() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let env = env_with(Some(ready.clone()));
    env.set(bound()).unwrap();
    assert!(ready.exists());
    drop(env);
    assert!(!ready.exists(), "left behind, it said ready with no server");

    // One that was never written is not this server's to remove.
    let env = env_with(Some(ready.clone()));
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
        js.contains("sessionStorage.getItem(\"__oj_env_reloaded\") === seen"),
        "{js}"
    );
    // By this server's name: another image counts its generations from
    // zero again, and its generation 2 is not the one reloaded for.
    let seen = |js: &str| {
        let at = js.find("const seen = ").expect("the guard");
        js[at..].split(" + g").next().unwrap().to_string()
    };
    assert!(seen(&js).contains(':'), "{js}");
    assert_eq!(seen(&js), seen(&env.module_js()));
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_ne!(seen(&js), seen(&self::env().module_js()));
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    assert!(env.module_js().contains("g === \"1\""));
}

// A restart hands the set values on as one environment variable: together
// they stay under a limit, a set past it refused whole.
#[test]
fn set_values_past_the_limit_are_refused_and_nothing_changes() {
    let env = env();
    env.set(updates(&[("VITE_SUPABASE_URL", Some("/a"))]))
        .unwrap();
    let big = "x".repeat(MAX_SET_BYTES as usize);
    let err = env
        .set(updates(&[
            ("VITE_SUPABASE_URL", Some("/b")),
            ("VITE_SUPABASE_PUBLISHABLE_KEY", Some(&big)),
        ]))
        .unwrap_err();
    assert!(err.contains("larger than"), "{err}");
    assert_eq!(
        env.values().get("VITE_SUPABASE_URL").map(String::as_str),
        Some("/a")
    );
    assert!(!env.values().contains_key("VITE_SUPABASE_PUBLISHABLE_KEY"));
    assert_eq!(env.generation(), 1);
    // Just under it is taken.
    let fits = "x".repeat(MAX_SET_BYTES as usize - 256);
    assert!(env
        .set(updates(&[("VITE_SUPABASE_PUBLISHABLE_KEY", Some(&fits))]))
        .unwrap());
}

// A `<script src>` without `type="module"` and a service worker's script:
// neither may hold a static import.
#[test]
fn a_classic_script_request_is_told_by_its_fetch_headers() {
    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, v.parse().unwrap());
        }
        map
    };
    for classic in [
        headers(&[("sec-fetch-dest", "script"), ("sec-fetch-mode", "no-cors")]),
        headers(&[
            ("sec-fetch-dest", "serviceworker"),
            ("sec-fetch-mode", "same-origin"),
        ]),
    ] {
        assert!(classic_script_request(&classic), "{classic:?}");
    }
    for module in [
        headers(&[("sec-fetch-dest", "script"), ("sec-fetch-mode", "cors")]),
        headers(&[
            ("sec-fetch-dest", "worker"),
            ("sec-fetch-mode", "same-origin"),
        ]),
        headers(&[("sec-fetch-dest", "empty"), ("sec-fetch-mode", "no-cors")]),
        headers(&[]),
    ] {
        assert!(!classic_script_request(&module), "{module:?}");
    }
}

// What a restarted server hands the next: its set values, taken for the
// keys still listed and counted as set, so a config or `.env` edit does not
// put a key back on its literal. Anything else of it is ignored.
#[test]
fn set_values_handed_over_a_restart_are_set_again() {
    let env = env();
    env.restore_set(r#"{"VITE_SUPABASE_URL":"/_port_9000","VITE_GONE":"x"}"#);
    let status = env.status();
    assert_eq!(status["keys"][0]["value"], "/_port_9000", "{status}");
    assert_eq!(status["keys"][0]["source"], "set");
    assert_eq!(status["keys"][1]["source"], serde_json::Value::Null);
    assert_eq!(env.state(), Binding::Partial);
    assert_eq!(env.values().len(), 1, "a key no longer listed is dropped");

    let env = self::env();
    for bad in ["", "not json", "[1]", r#"{"VITE_SUPABASE_URL":1}"#] {
        env.restore_set(bad);
        assert!(env.values().is_empty(), "{bad}");
    }
}

// A server bound to one of the machine's own addresses is reached there by
// a command on this machine, so its connection comes from that address, not
// from loopback.
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
    let ready = dir.path().join("ready");
    let env = booting_env(Some(ready.clone()));
    // As a restart hands every value over, before the listener is up.
    env.restore_set(&serde_json::json!(bound()).to_string());
    assert_eq!(env.state(), Binding::Bound);
    assert!(!ready.exists(), "bound, but nothing listens yet");
    env.serving();
    assert!(ready.exists());
}

// ---- more tests ------------------------------------------------------------------------

fn based_env(base: &str) -> RuntimeEnv {
    let env = RuntimeEnv::new(
        RuntimeEnvConfig {
            keys: supabase_keys(),
            ready_file: None,
        },
        &defines(),
        Some(base),
    );
    env.serving();
    env
}

// Under a base every url the module and a worker's file name is under it:
// the generation the page asks for, and the script a classic worker loads.
#[test]
fn under_a_base_the_generation_and_the_worker_script_are_asked_for_under_it() {
    let env = based_env("/app/");
    let js = env.module_js();
    parse_module(&js);
    assert!(js.contains("fetch(\"/app/@oj/env.js?generation\""), "{js}");
    let compiled =
        "import \"/app/@oj/env.js\";\nself.postMessage(globalThis.__oj_env.VITE_SUPABASE_URL);\n";
    let classic = worker_body(
        &env,
        "/app/src/w.js?worker_file&type=classic",
        compiled.to_string(),
    );
    assert!(
        classic.starts_with("importScripts(\"/app/@oj/env.js?classic\");\n"),
        "{classic}"
    );
    // The unbased import is another module's: left alone.
    let other = "import \"/@oj/env.js\";\n";
    assert_eq!(
        worker_body(
            &env,
            "/app/src/w.js?worker_file&type=classic",
            other.to_string()
        ),
        other
    );
}

// Only the first import is the env module's: the same text further down (a
// string the worker holds) stays as written.
#[test]
fn only_the_leading_import_of_a_worker_file_is_replaced() {
    let env = env();
    let compiled = "import \"/@oj/env.js\";\nconst text = 'import \"/@oj/env.js\";';\n";
    let classic = worker_body(
        &env,
        "/src/w.js?worker_file&type=classic",
        compiled.to_string(),
    );
    assert_eq!(
        classic,
        "importScripts(\"/@oj/env.js?classic\");\nconst text = 'import \"/@oj/env.js\";';\n"
    );
    // A worker file that reads no key has no import to replace.
    let plain = "self.postMessage(1);\n";
    for kind in ["classic", "ignore", "module"] {
        assert_eq!(
            worker_body(
                &env,
                &format!("/src/w.js?worker_file&type={kind}"),
                plain.to_string()
            ),
            plain
        );
    }
}

// The inline form stays on one line whatever a value holds: line breaks and
// the separators JavaScript counts as ones are inside JSON strings.
#[test]
fn the_inline_script_is_one_line_whatever_the_values_hold() {
    let env = env();
    env.set(updates(&[
        ("VITE_SUPABASE_URL", Some("a\nb\r\nc\u{2028}d\u{2029}e")),
        ("VITE_SUPABASE_PUBLISHABLE_KEY", Some("</script><!--")),
        ("VITE_SUPABASE_PROJECT_ID", Some("};alert(1);//\\\"")),
    ]))
    .unwrap();
    let compiled = "import \"/@oj/env.js\";\nself.postMessage(1);\n";
    let inline = worker_body(
        &env,
        "/src/w.js?worker_file&type=ignore",
        compiled.to_string(),
    );
    assert_eq!(inline.split('\n').count(), compiled.split('\n').count());
    assert!(!inline.contains("alert(1);//\\\"\" }"), "{inline}");
    for kind in [oxc_span::SourceType::cjs(), oxc_span::SourceType::mjs()] {
        let allocator = oxc_allocator::Allocator::default();
        let parsed = oxc_parser::Parser::new(&allocator, &inline, kind).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{inline}",
            parsed.diagnostics
        );
        // One block, one call: nothing a value holds became a statement.
        assert_eq!(parsed.program.body.len(), 2, "{inline}");
    }
    parse_module(&env.module_js());
}

// What moves the pages and what does not: a set that changes nothing and an
// unset of a key that was not set tell nobody; each real change tells them
// once.
#[test]
fn the_pages_reload_once_for_each_change_and_never_for_none() {
    let env = env();
    let set = |pairs: &[(&str, Option<&str>)]| env.set(updates(pairs)).unwrap();

    assert!(set(&[("VITE_SUPABASE_URL", Some("/a"))]));
    assert!(!set(&[("VITE_SUPABASE_URL", Some("/a"))]), "the same value");
    assert!(!set(&[("VITE_SUPABASE_PROJECT_ID", None)]), "was not set");
    assert!(!set(&[]), "nothing asked");
    assert_eq!(env.generation(), 1);

    assert!(env.set(bound()).unwrap(), "every key, one change");
    assert!(!env.set(bound()).unwrap(), "again, the same");
    assert_eq!(env.generation(), 2);

    assert!(set(&[("VITE_SUPABASE_URL", None)]), "back on the literal");
    assert!(!set(&[("VITE_SUPABASE_URL", None)]));
    assert_eq!(env.generation(), 3);
}

// Addresses that are nobody's: the unspecified ones never count as this
// machine, mapped or not.
#[test]
fn the_unspecified_address_is_not_this_machine() {
    let headers = HeaderMap::new();
    for ip in ["0.0.0.0", "::", "::ffff:0.0.0.0"] {
        assert!(!control_caller(&headers, Some(ip.parse().unwrap())), "{ip}");
    }
}
