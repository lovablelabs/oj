// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::*;

/// What extraction records for `@lovable.dev/vite-tanstack-config`'s
/// defaults: errors in dev, `**/server/**` and `server-only` denied to the
/// client on top of Start's own rules.
fn lovable_options() -> Value {
    serde_json::json!({
        "version": "1.171.49",
        "srcDirectory": "src",
        "enabled": true,
        "devBehavior": "error",
        "mockAccess": "error",
        "onViolation": false,
        "include": [],
        "exclude": [],
        "ignoreImporters": [],
        "client": {
            "specifiers": [{ "text": "server-only" }],
            "files": [{ "text": "**/server/**" }],
            "excludeFiles": null,
        },
        "server": { "specifiers": null, "files": null, "excludeFiles": null },
        "maxTraceDepth": null,
    })
}

struct App {
    _dir: tempfile::TempDir,
    root: String,
    ip: Arc<ImportProtection>,
}

fn app_with(options: &Value, files: &[(&str, &str)]) -> App {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for (name, code) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, code).unwrap();
    }
    let ip = ImportProtection::native(&root, Some(options)).expect("native import protection");
    App {
        _dir: dir,
        root: root.to_string_lossy().into_owned(),
        ip,
    }
}

fn app(files: &[(&str, &str)]) -> App {
    app_with(&lovable_options(), files)
}

impl App {
    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    /// One import as a bundle resolves it: `to` is the file `source` names.
    fn import(&self, side: Side, from: &str, source: &str, to: &str) -> Result<(), String> {
        let from = self.path(from);
        match self.ip.before_resolve(side, &from, source) {
            Before::Pass | Before::Virtual(_) => Ok(()),
            Before::Denied(message) => Err(message),
            Before::Check => self.ip.after_resolve(side, &from, source, &self.path(to)),
        }
    }
}

const ROUTER: &str = "import { createRouter } from \"@tanstack/react-router\";\n\nimport { routeTree } from \"./routeTree.gen\";\n";
const ROUTE_TREE: &str = "import { Route as IndexRouteImport } from \"./routes/index\";\n";

#[test]
fn a_denied_file_reads_as_starts_message() {
    let app = app(&[
        ("src/router.tsx", ROUTER),
        ("src/routeTree.gen.ts", ROUTE_TREE),
        (
            "src/routes/index.tsx",
            "import { readSecret } from \"../server/secret\";\n\nexport function Index() {\n  return (\n    <img\n      alt={readSecret()}\n    />\n  );\n}\n",
        ),
        ("src/server/secret.ts", "export const readSecret = () => 1;\n"),
    ]);
    let client = Side::Client;
    app.import(
        client,
        "src/router.tsx",
        "./routeTree.gen",
        "src/routeTree.gen.ts",
    )
    .unwrap();
    app.import(
        client,
        "src/routeTree.gen.ts",
        "./routes/index",
        "src/routes/index.tsx",
    )
    .unwrap();
    let denied = app
        .import(
            client,
            "src/routes/index.tsx?tsr-split=component",
            "../server/secret",
            "src/server/secret.ts",
        )
        .unwrap_err();
    assert_eq!(
        denied,
        r#"
[import-protection] Import denied in client environment

  Denied by file pattern: **/server/**
  Importer: src/routes/index.tsx:6:12
  Import: "../server/secret"
  Resolved: src/server/secret.ts

  Trace:
    1. src/router.tsx:3:28 (entry) (import "./routeTree.gen")
    2. src/routeTree.gen.ts:1:44 (import "./routes/index")
    3. src/routes/index.tsx:6:12 (import "../server/secret")

  Code:
    4 |   return (
    5 |     <img
  > 6 |       alt={readSecret()}
      |            ^
    7 |     />
    8 |   );

  src/routes/index.tsx:6:12

  Suggestions:
    - Use createServerFn().handler(() => ...) to keep the logic on the server and call it from the client via an RPC bridge
    - Use createServerOnlyFn(() => ...) to mark it as server-only (it will throw if accidentally called from the client)
    - Use createIsomorphicFn().client(() => ...).server(() => ...) to provide separate client and server implementations
    - Move the server-only import out of this file into a separate .server.ts module that is not imported by any client code
"#
    );
}

#[test]
fn a_use_inside_a_server_function_is_not_where_the_violation_points() {
    let app = app(&[
        (
            "src/a.ts",
            "import { db } from \"./server/db\";\nconst load = createServerFn().handler(() => db.all());\nexport const leak = db;\n",
        ),
        ("src/server/db.ts", "export const db = {};\n"),
    ]);
    let denied = app
        .import(Side::Client, "src/a.ts", "./server/db", "src/server/db.ts")
        .unwrap_err();
    assert!(denied.contains("  Importer: src/a.ts:3:21\n"), "{denied}");
}

#[test]
fn a_denied_specifier_is_reported_before_it_resolves() {
    let app = app(&[("src/a.ts", "import \"server-only\";\n")]);
    let before = app
        .ip
        .before_resolve(Side::Client, &app.path("src/a.ts"), "server-only");
    let Before::Denied(message) = before else {
        panic!("{before:?}");
    };
    assert!(
        message.contains("  Denied by specifier pattern: server-only\n  Importer: src/a.ts:1:9\n  Import: \"server-only\"\n\n  Trace:"),
        "{message}"
    );
    // The server may import it.
    assert_eq!(
        app.ip
            .before_resolve(Side::Server, &app.path("src/a.ts"), "server-only"),
        Before::Check
    );
}

#[test]
fn a_marked_module_is_denied_to_the_other_side() {
    let app = app(&[
        ("src/a.ts", "import { db } from \"./db\";\ndb();\n"),
        (
            "src/db.ts",
            "import \"@tanstack/react-start/server-only\";\nexport const db = () => 1;\n",
        ),
    ]);
    let marker = "@tanstack/react-start/server-only";
    // The server render reads the marker first and takes the marker module.
    assert_eq!(
        app.ip
            .before_resolve(Side::Server, &app.path("src/db.ts"), marker),
        Before::Virtual("\0tanstack-start-import-protection:marker:server-only".into())
    );
    let denied = app
        .import(Side::Client, "src/a.ts", "./db", "src/db.ts")
        .unwrap_err();
    assert!(
        denied.contains("  Denied by marker: module is restricted to the opposite environment\n  Importer: src/a.ts:2:1\n"),
        "{denied}"
    );
    // The marked module's own import is the violation where the client
    // reaches it before any importer was checked against the marker.
    let Before::Denied(own) = app
        .ip
        .before_resolve(Side::Client, &app.path("src/db.ts"), marker)
    else {
        panic!("the client took a server-only marker");
    };
    assert!(own.contains("  Importer: src/db.ts:1:9\n"), "{own}");
}

#[test]
fn a_file_with_both_markers_is_refused() {
    let app = app(&[("src/a.ts", "")]);
    let file = app.path("src/a.ts");
    app.ip
        .before_resolve(Side::Server, &file, "@tanstack/react-start/server-only");
    assert_eq!(
        app.ip
            .before_resolve(Side::Server, &file, "@tanstack/react-start/client-only"),
        Before::Denied(
            "[import-protection] File \"src/a.ts\" has both server-only and client-only markers. This is not allowed."
                .into()
        )
    );
}

#[test]
fn an_edit_forgets_the_files_marker() {
    let app = app(&[("src/a.ts", "import \"./db\";\n"), ("src/db.ts", "")]);
    app.ip.before_resolve(
        Side::Server,
        &app.path("src/db.ts"),
        "@tanstack/react-start/server-only",
    );
    assert!(app
        .import(Side::Client, "src/a.ts", "./db", "src/db.ts")
        .is_err());
    app.ip.changed(Side::Server, &app.path("src/db.ts"));
    app.import(Side::Client, "src/a.ts", "./db", "src/db.ts")
        .unwrap();
}

#[test]
fn importers_and_files_outside_the_rules_pass() {
    let app = app(&[]);
    // An importer outside `srcDirectory` is not checked.
    assert_eq!(
        app.ip
            .before_resolve(Side::Client, &app.path("scripts/a.ts"), "../src/server/x"),
        Before::Pass
    );
    assert_eq!(
        app.ip.before_resolve(
            Side::Client,
            &app.path("node_modules/pkg/index.js"),
            "./x.server.js"
        ),
        Before::Pass
    );
    // A dependency's `.server.` file is excluded by default; `**/server/**`
    // replaced Start's `**/*.server.*` for the client.
    app.import(
        Side::Client,
        "src/a.ts",
        "pkg",
        "node_modules/pkg/dist/server/index.js",
    )
    .unwrap();
    app.import(Side::Client, "src/a.ts", "./b.server", "src/b.server.ts")
        .unwrap();
    // The server's default rule denies client files.
    assert!(app
        .import(Side::Server, "src/a.ts", "./b.client", "src/b.client.ts")
        .is_err());
    app.import(Side::Client, "src/a.ts", "./b.client", "src/b.client.ts")
        .unwrap();
}

#[test]
fn a_denied_file_an_unchecked_importer_reaches_is_mocked() {
    let app = app(&[]);
    let code = "export const a = 1;\nexport function b() {}\nexport { c as \"d-e\", f as default };\nexport type T = string;\nexport default 1;\n";
    let mock = app
        .ip
        .self_denial(Side::Client, &app.path("src/server/x.ts"), code)
        .expect("a mock");
    let (import, exports) = mock.split_once('\n').unwrap();
    assert!(
        import.starts_with("import mock from \"tanstack-start-import-protection:mock-runtime:"),
        "{mock}"
    );
    assert_eq!(
        exports,
        "export const a = mock.a;\nexport const b = mock.b;\nconst __tss_str_2 = mock[\"d-e\"];\nexport { __tss_str_2 as \"d-e\" };\nexport default mock;\n"
    );
    // The mock's runtime resolves and loads, and reports each access.
    let id = import
        .trim_start_matches("import mock from \"")
        .trim_end_matches("\";");
    let resolved = resolve_virtual(id).expect("resolves");
    let runtime = load_virtual(&resolved).expect("loads");
    assert!(
        runtime.contains("\"specifier\":\"src/server/x.ts\""),
        "{runtime}"
    );
    assert!(runtime.contains("function __createMock(name)"), "{runtime}");
    // The server gets the silent mock; a file no rule denies gets none.
    assert!(app
        .ip
        .self_denial(Side::Server, &app.path("src/x.client.ts"), "")
        .is_some_and(
            |m| m.starts_with("import mock from \"tanstack-start-import-protection:mock\";")
        ));
    assert!(app
        .ip
        .self_denial(Side::Client, &app.path("src/x.ts"), code)
        .is_none());
}

#[test]
fn only_what_rust_reads_as_start_does_runs_natively() {
    let dir = tempfile::tempdir().unwrap();
    let native = |edit: &dyn Fn(&mut Value)| {
        let mut options = lovable_options();
        edit(&mut options);
        ImportProtection::native(dir.path(), Some(&options)).is_some()
    };
    assert!(native(&|_| {}));
    assert!(ImportProtection::native(dir.path(), None).is_none());
    // A plugin core this was not checked against.
    assert!(!native(&|o| o["version"] = "1.169.6".into()));
    assert!(!native(&|o| o["version"] = "1.171.33".into()));
    assert!(native(&|o| o["version"] = "1.171.34".into()));
    assert!(!native(&|o| o["version"] = "1.173.0".into()));
    assert!(!native(&|o| o["version"] = Value::Null));
    assert!(native(&|o| o["version"] = "1.172.0".into()));
    assert!(!native(&|o| o["devBehavior"] = "mock".into()));
    assert!(!native(&|o| o["onViolation"] = true.into()));
    assert!(!native(&|o| o["enabled"] = false.into()));
    // An extglob, which globset has no spelling for.
    assert!(!native(
        &|o| o["client"]["files"] = serde_json::json!([{ "text": "**/!(a).ts" }])
    ));
    assert!(!native(
        &|o| o["exclude"] = serde_json::json!([{ "other": true }])
    ));
    // A regular expression Rust reads the same way.
    assert!(native(
        &|o| o["client"]["files"] = serde_json::json!([{ "source": "\\.secret\\.", "flags": "" }])
    ));
    // A lookahead, which it cannot.
    assert!(!native(
        &|o| o["client"]["files"] = serde_json::json!([{ "source": "a(?=b)", "flags": "" }])
    ));
}

#[test]
fn a_trace_still_reaches_the_entry_after_the_importer_is_edited() {
    let app = app(&[
        ("src/router.tsx", ROUTER),
        ("src/routeTree.gen.ts", ROUTE_TREE),
        (
            "src/routes/index.tsx",
            "import { secret } from \"../server/secret\";\nsecret();\n",
        ),
        ("src/server/secret.ts", "export const secret = () => 1;\n"),
    ]);
    app.import(
        Side::Client,
        "src/router.tsx",
        "./routeTree.gen",
        "src/routeTree.gen.ts",
    )
    .unwrap();
    app.import(
        Side::Client,
        "src/routeTree.gen.ts",
        "./routes/index",
        "src/routes/index.tsx",
    )
    .unwrap();
    app.ip
        .changed(Side::Client, &app.path("src/routes/index.tsx"));
    let message = app
        .import(
            Side::Client,
            "src/routes/index.tsx",
            "../server/secret",
            "src/server/secret.ts",
        )
        .unwrap_err();
    assert!(message.contains("1. src/router.tsx"), "{message}");
}

#[test]
fn a_glob_picomatch_reads_another_way_stays_with_starts_plugin() {
    for glob in [
        "./src/generated/**",
        "src/routes/(auth)/**",
        "src/routes/{-$locale}/**",
        "src/legacy[old/*",
    ] {
        assert!(Pattern::from_text(glob).is_none(), "{glob}");
    }
    assert!(Pattern::from_text("**/*.{server,client}.*").is_some());
}

#[test]
fn a_trace_starts_at_the_router_a_route_imports_back() {
    let app = app(&[
        ("src/router.tsx", ROUTER),
        ("src/routeTree.gen.ts", ROUTE_TREE),
        (
            "src/routes/index.tsx",
            "import { getRouter } from \"../router\";\nimport { secret } from \"../server/secret\";\nsecret(getRouter);\n",
        ),
        ("src/server/secret.ts", "export const secret = () => 1;\n"),
    ]);
    for (from, source, to) in [
        ("src/router.tsx", "./routeTree.gen", "src/routeTree.gen.ts"),
        (
            "src/routeTree.gen.ts",
            "./routes/index",
            "src/routes/index.tsx",
        ),
        ("src/routes/index.tsx", "../router", "src/router.tsx"),
    ] {
        app.import(Side::Client, from, source, to).unwrap();
    }
    let message = app
        .import(
            Side::Client,
            "src/routes/index.tsx",
            "../server/secret",
            "src/server/secret.ts",
        )
        .unwrap_err();
    assert!(message.contains("1. src/router.tsx"), "{message}");
    assert!(message.contains("3. src/routes/index.tsx"), "{message}");
}

#[test]
fn a_pattern_ending_in_a_globstar_matches_its_prefix_as_picomatch_does() {
    let pattern = Pattern::from_text("drizzle-orm/**").unwrap();
    assert!(pattern.is_match("drizzle-orm"));
    assert!(pattern.is_match("drizzle-orm/pg-core"));
    assert!(!pattern.is_match("drizzle-orm-extra"));
}
