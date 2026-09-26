// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj.config.{ts,js,mjs,json}` is a file oj executes. The contract for every
//! shape of it -- malformed, hostile, runaway, or merely surprising -- is a
//! `ConfigError` naming the file, never a hang, a crash, or a default config
//! silently standing in for one that failed to load.

use std::path::Path;

use oj_config::{load, load_with, ConfigError};

struct Project(tempfile::TempDir);

impl Project {
    fn with(name: &str, source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), source).unwrap();
        Self(dir)
    }

    fn ts(source: &str) -> Self {
        Self::with("oj.config.ts", source)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn load(&self) -> Result<oj_config::OjConfig, ConfigError> {
        load(self.path())
    }
}

fn assert_error_names_the_file(err: &ConfigError, name: &str) {
    let text = err.to_string();
    assert!(text.contains(name), "error does not name {name}: {text}");
}

#[test]
fn a_project_with_no_config_gets_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let config = load(dir.path()).unwrap();
    assert!(config.server.is_none());
    assert!(config.build.is_none());
}

#[test]
fn a_config_that_never_finishes_is_stopped() {
    let project = Project::ts("while (true) {}\nexport default {};");
    let err = project.load().unwrap_err();
    assert!(matches!(err, ConfigError::Eval(..)), "{err}");
    assert!(
        err.to_string().contains("limit"),
        "the message must explain the limit: {err}"
    );
    assert_error_names_the_file(&err, "oj.config.ts");
}

#[test]
fn a_config_that_allocates_without_bound_is_stopped() {
    let project = Project::ts(
        "const a = [];\nwhile (true) { a.push(new Array(1e6).fill(0)); }\nexport default {};",
    );
    assert!(matches!(project.load().unwrap_err(), ConfigError::Eval(..)));
}

#[test]
fn a_config_that_recurses_without_bound_is_stopped() {
    let project = Project::ts("function f() { return f(); }\nexport default f();");
    assert!(matches!(project.load().unwrap_err(), ConfigError::Eval(..)));
}

#[test]
fn a_config_that_throws_reports_the_exception() {
    let project = Project::ts("throw new Error('deliberate failure');");
    let err = project.load().unwrap_err();
    assert!(
        err.to_string().contains("deliberate failure"),
        "the thrown message must reach the user: {err}"
    );
}

#[test]
fn a_config_with_a_throwing_getter_is_an_error_not_a_partial_config() {
    let project =
        Project::ts("export default { base: '/ok/', get server() { throw new Error('boom'); } };");
    let err = project.load().unwrap_err();
    assert!(matches!(err, ConfigError::Eval(..)), "{err}");
}

#[test]
fn a_cyclic_config_object_is_an_error() {
    let project = Project::ts("const a = { base: '/x/' };\na.self = a;\nexport default a;");
    assert!(matches!(project.load().unwrap_err(), ConfigError::Eval(..)));
}

#[test]
fn a_config_that_exports_nothing_says_so() {
    // A file that exists but exports nothing is a mistake, and the message has
    // to say which mistake -- not "invalid type: null, expected struct".
    for source in [
        "const unused = 1;",
        "export default null;",
        "export default undefined;",
        "",
    ] {
        let project = Project::ts(source);
        let err = project.load().unwrap_err();
        assert!(
            err.to_string().contains("export default"),
            "{source:?}: {err}"
        );
        assert_error_names_the_file(&err, "oj.config.ts");
    }
}

#[test]
fn a_config_that_is_not_an_object_is_a_schema_error() {
    for source in [
        "export default 42;",
        "export default 'string';",
        "export default true;",
        // An array must not be read as a struct of defaults.
        "export default [];",
        "export default [{ base: '/x/' }];",
    ] {
        let project = Project::ts(source);
        match project.load() {
            Err(ConfigError::Schema(..)) => {}
            Err(other) => panic!("{source:?}: unexpected {other}"),
            Ok(config) => panic!("{source:?}: accepted as {config:?}"),
        }
    }
}

#[test]
fn fields_of_the_wrong_type_are_schema_errors_naming_the_file() {
    for source in [
        "export default { base: 42 };",
        "export default { server: 'nope' };",
        "export default { server: { port: 'not a number' } };",
        "export default { build: { rollupOptions: 1, outDir: [] } };",
        "export default { resolve: { alias: 42 } };",
    ] {
        let project = Project::ts(source);
        let err = project.load().unwrap_err();
        assert!(matches!(err, ConfigError::Schema(..)), "{source:?}: {err}");
        assert_error_names_the_file(&err, "oj.config.ts");
    }
}

#[test]
fn unknown_fields_are_ignored_rather_than_fatal() {
    let project = Project::ts(
        "export default { base: '/app/', somethingViteHasThatOjDoesNot: { deep: [1, 2] } };",
    );
    let config = project.load().unwrap();
    assert_eq!(config.base.as_deref(), Some("/app/"));
}

#[test]
fn a_syntax_error_is_reported_as_a_parse_error() {
    for source in [
        "export default {",
        "const a = (",
        "export default { base: };",
        "!!!",
        "\0\0\0",
    ] {
        let project = Project::ts(source);
        let err = project.load().unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse(..) | ConfigError::Eval(..)),
            "{source:?}: {err}"
        );
    }
}

#[test]
fn typescript_in_a_config_is_stripped() {
    let project = Project::ts(
        "interface Shape { base: string }\n\
         type Alias = Shape;\n\
         const config: Alias & { server: { port: number } } = {\n\
           base: '/ts/',\n\
           server: { port: 5199 as number },\n\
         };\n\
         export default config satisfies Alias;",
    );
    let config = project.load().unwrap();
    assert_eq!(config.base.as_deref(), Some("/ts/"));
    assert_eq!(config.server.unwrap().port, Some(5199));
}

#[test]
fn a_config_function_receives_the_command_and_mode() {
    let project =
        Project::ts("export default ({ command, mode }) => ({ base: `/${command}-${mode}/` });");
    assert_eq!(
        load_with(project.path(), "build", "production")
            .unwrap()
            .base
            .as_deref(),
        Some("/build-production/")
    );
    assert_eq!(
        load_with(project.path(), "serve", "development")
            .unwrap()
            .base
            .as_deref(),
        Some("/serve-development/")
    );
}

#[test]
fn define_config_is_available_without_an_import() {
    // The import line is stripped and `defineConfig` is provided by the
    // sandbox, so the idiomatic Vite-style config loads as written.
    let project = Project::ts(
        "import { defineConfig } from 'vite';\n\
         export default defineConfig({ base: '/defined/' });",
    );
    assert_eq!(project.load().unwrap().base.as_deref(), Some("/defined/"));
}

#[test]
fn an_import_of_a_real_module_is_stripped_and_its_use_is_an_error_with_a_hint() {
    let project = Project::ts(
        "import react from '@vitejs/plugin-react';\n\
         export default { plugins: [react()] };",
    );
    let err = project.load().unwrap_err();
    let text = err.to_string();
    assert!(text.contains("react"), "{text}");
    assert!(
        text.contains("oj.plugins.mjs"),
        "the error should point at the supported alternative: {text}"
    );
}

#[test]
fn a_multiline_import_statement_is_not_mistaken_for_config() {
    // `to_script` works line by line, so an import spanning lines leaves its
    // tail behind. Whatever it does, it must not be a crash or a wrong config.
    let project = Project::ts(
        "import {\n  defineConfig,\n} from 'vite';\nexport default { base: '/multi/' };",
    );
    match project.load() {
        Ok(config) => assert_eq!(config.base.as_deref(), Some("/multi/")),
        Err(err) => assert!(
            matches!(err, ConfigError::Parse(..) | ConfigError::Eval(..)),
            "{err}"
        ),
    }
}

#[test]
fn a_deeply_nested_config_object_is_an_error_not_a_crash() {
    let project = Project::ts(
        "let root = {}; let cursor = root;\n\
         for (let i = 0; i < 100000; i++) { cursor.next = {}; cursor = cursor.next; }\n\
         export default { build: { rollupOptions: root } };",
    );
    // Either bound (the JSON serializer's or serde's recursion limit) is fine.
    assert!(project.load().is_err());
}

#[test]
fn process_env_is_readable_but_a_missing_variable_is_undefined() {
    let project =
        Project::ts("export default { base: process.env.OJ_TEST_NOT_SET ? '/set/' : '/unset/' };");
    assert_eq!(project.load().unwrap().base.as_deref(), Some("/unset/"));
}

#[test]
fn a_config_cannot_reach_the_filesystem_or_the_network() {
    // The sandbox has no module loader and no host bindings: these either throw
    // or come back undefined.
    for expression in [
        "typeof require",
        "typeof module",
        "typeof fetch",
        "typeof process.binding",
        "typeof process.dlopen",
        "typeof globalThis.Deno",
        "typeof new Function('return this')().require",
    ] {
        let project = Project::ts(&format!("export default {{ base: String({expression}) }};"));
        match project.load() {
            Err(ConfigError::Eval(..)) | Err(ConfigError::Schema(..)) => {}
            Err(other) => panic!("{expression}: unexpected {other}"),
            Ok(config) => assert_eq!(
                config.base.as_deref(),
                Some("undefined"),
                "{expression} resolved to something"
            ),
        }
    }
    // `process` itself exists, but only as the injected environment.
    let project = Project::ts("export default { base: Object.keys(process).sort().join(',') };");
    assert_eq!(project.load().unwrap().base.as_deref(), Some("env"));
}

#[test]
fn the_first_candidate_filename_wins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("oj.config.json"), r#"{"base":"/json/"}"#).unwrap();
    assert_eq!(load(dir.path()).unwrap().base.as_deref(), Some("/json/"));

    std::fs::write(
        dir.path().join("oj.config.js"),
        "export default { base: '/js/' };",
    )
    .unwrap();
    assert_eq!(load(dir.path()).unwrap().base.as_deref(), Some("/js/"));

    std::fs::write(
        dir.path().join("oj.config.ts"),
        "export default { base: '/ts/' };",
    )
    .unwrap();
    assert_eq!(load(dir.path()).unwrap().base.as_deref(), Some("/ts/"));
}

#[test]
fn a_json_config_is_not_evaluated_as_javascript() {
    let project = Project::with("oj.config.json", "{\"base\": \"/j/\", \"//\": \"comment\"}");
    assert_eq!(project.load().unwrap().base.as_deref(), Some("/j/"));

    // JSON with JS in it is a schema error, not code that runs.
    let hostile = Project::with(
        "oj.config.json",
        "{\"base\": (function(){ return '/x/' })()}",
    );
    assert!(matches!(
        hostile.load().unwrap_err(),
        ConfigError::Schema(..)
    ));
}

#[test]
fn a_config_directory_instead_of_a_file_is_not_a_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("oj.config.ts")).unwrap();
    // Not a file, so it is skipped rather than read.
    assert!(load(dir.path()).unwrap().base.is_none());
}

#[test]
fn invalid_utf8_in_a_config_is_a_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("oj.config.ts"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
    let err = load(dir.path()).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(..)), "{err}");
}

#[test]
fn loading_the_same_config_twice_gives_the_same_result() {
    let project = Project::ts(
        "export default { base: '/stable/', server: { port: 1234 }, define: { A: 1 } };",
    );
    let first = format!("{:?}", project.load().unwrap());
    for _ in 0..3 {
        assert_eq!(format!("{:?}", project.load().unwrap()), first);
    }
}
