// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Which file a `resolve.alias`ed import serves, next to tsconfig `paths`,
//! against Vite's dev server with vite-tsconfig-paths on the same app.
//! Vite's alias plugin runs ahead of every resolver plugin and takes the
//! first entry that matches, in the order the config wrote them
//! (normalizeAlias keeps an object's key order), so
//! `{ "@/integrations/supabase/client": backend, "@": src }` sends the client
//! import to `backend`, and `@` written first takes it. The rewritten id goes
//! on through tsconfig `paths`, so an alias to `@/lib` still lands.
//! vite-tsconfig-paths tries `@/*` before an exact key (a pattern with no `*`
//! has no prefix), and a `paths` answer that is a declaration file is none.
//!
//! oj's resolver tried tsconfig `paths` before the aliases, the exact key
//! first, and kept the aliases sorted, so `@` always came first: the import
//! served the `.d.ts` the paths named, or `@`'s file, over the alias written
//! for it.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.ts"></script></body></html>"#;

/// The project's own client module, a declaration the tsconfig `paths` name
/// for it, and the app backend an alias sends it to.
const FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"alias-order","private":true,"type":"module"}"#,
    ),
    ("index.html", INDEX),
    (
        "src/main.ts",
        "import { who } from \"@/integrations/supabase/client\";\nimport { helper } from \"@/lib/helper\";\ndocument.body.dataset.who = who + helper;\n",
    ),
    (
        "src/tilde.ts",
        "import { helper } from \"~lib/helper\";\nexport const tilde = helper;\n",
    ),
    ("src/lib/helper.ts", "export const helper = \"-helper\";\n"),
    (
        "src/integrations/supabase/client.ts",
        "export const who = \"generated-client\";\n",
    ),
    ("src/appBackend.ts", "export const who = \"app-backend\";\n"),
    (
        "src/types/existingSupabaseClient.d.ts",
        "export declare const supabase: unknown;\n",
    ),
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/*": ["./src/*"] } } }"#,
    ),
];

const HEADER: &str = "import path from \"node:path\";\nimport tsConfigPaths from \"vite-tsconfig-paths\";\nconst dir = path.dirname(new URL(import.meta.url).pathname);\nconst src = (p) => path.resolve(dir, \"src\", p);\n";

fn config(alias: &str) -> String {
    format!(
        "{HEADER}export default {{ resolve: {{ alias: {alias} }}, plugins: [tsConfigPaths()] }};\n"
    )
}

/// Each config: its name, its `resolve.alias`, and the file the client import
/// serves (on Vite and on oj).
const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "specific-first",
        r#"{ "@/integrations/supabase/client": src("appBackend.ts"), "~lib": "@/lib", "@": src("") }"#,
        "/src/appBackend.ts",
    ),
    (
        "at-first",
        r#"{ "@": src(""), "~lib": "@/lib", "@/integrations/supabase/client": src("appBackend.ts") }"#,
        "/src/integrations/supabase/client.ts",
    ),
    (
        "array",
        r#"[{ find: "@/integrations/supabase/client", replacement: src("appBackend.ts") }, { find: "~lib", replacement: "@/lib" }, { find: "@", replacement: src("") }]"#,
        "/src/appBackend.ts",
    ),
    // RegExp finds test and replace as JavaScript: an anchored `^@\/lib$`
    // never takes `@/lib/helper`, and `$1` carries the capture.
    (
        "regex",
        r#"[{ find: /^@\/lib$/, replacement: src("appBackend.ts") }, { find: /^@\/integrations\/supabase\/client$/, replacement: src("appBackend.ts") }, { find: /^~lib\/(.*)$/, replacement: src("lib/$1") }, { find: "@", replacement: src("") }]"#,
        "/src/appBackend.ts",
    ),
    // No alias for it: vite-tsconfig-paths tries `@/*` before the exact key.
    (
        "paths-only",
        r#"{ "~lib": "@/lib" }"#,
        "/src/integrations/supabase/client.ts",
    ),
];

/// The specifiers a served module imports, in order.
fn imports(js: &str) -> Vec<String> {
    js.split("from \"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(spec, _)| spec.to_string()))
        .collect()
}

/// Vite's dev server on `root` with `config`, once it answers.
struct ViteDev {
    child: Child,
    port: u16,
}

impl ViteDev {
    fn start(root: &Path, config: &str) -> ViteDev {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args(["--host", "127.0.0.1", "--strictPort", "--port"])
            .arg(port.to_string())
            .args(["--config", config])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "vite dev never listened");
            std::thread::sleep(Duration::from_millis(100));
        }
        ViteDev { child, port }
    }

    /// The status and body of `path`.
    fn get(&self, path: &str) -> (u16, String) {
        let out = Command::new("curl")
            .args(["-s", "-m", "60", "-w", "\n%{http_code}"])
            .arg(format!("http://127.0.0.1:{}{path}", self.port))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, status) = text.rsplit_once('\n').unwrap_or(("", "0"));
        (status.trim().parse().unwrap_or(0), body.to_string())
    }

    /// The imports of `path`, once Vite has served it.
    fn imports(&self, path: &str) -> Vec<String> {
        let (status, body) = self.get(path);
        assert_eq!(status, 200, "vite dev {path}:\n{body}");
        imports(&body)
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_import_serves_the_entry_vite_picks() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    for (name, alias, want) in CONFIGS {
        let config_file = format!("vite.{name}.config.js");
        app.write(&[(config_file.as_str(), &config(alias))]);

        let vite = ViteDev::start(&app.root, &config_file);
        let theirs = vite.imports("/src/main.ts");
        let tilde = vite.imports("/src/tilde.ts");
        drop(vite);
        assert_eq!(
            theirs,
            [*want, "/src/lib/helper.ts"],
            "vite dev, {name}: the alias test's premise"
        );
        assert_eq!(tilde, ["/src/lib/helper.ts"], "vite dev, {name}: ~lib");

        let mut dev = app.dev();
        dev.config = Some(app.root.join(&config_file));
        let server = crate::serve(dev).await;
        let ours = imports(&server.text("/src/main.ts").await);
        let our_tilde = imports(&server.text("/src/tilde.ts").await);
        drop(server);
        assert_eq!(ours, theirs, "oj dev vs vite dev, {name}");
        assert_eq!(
            our_tilde, tilde,
            "oj dev vs vite dev, {name}: an alias to a paths name"
        );
    }
}

// When the only `paths` answer is a declaration file, it is none: Vite fails
// the import (vite-tsconfig-paths passes, vite:resolve finds nothing), and oj
// fails it too rather than serving the `.d.ts` as JavaScript.
#[tokio::test(flavor = "multi_thread")]
async fn a_paths_target_that_is_a_declaration_file_is_never_served() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    app.write(&[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/lib/*": ["./src/lib/*"] } } }"#,
        ),
        ("vite.paths.config.js", &config("{}")),
    ]);
    let vite = ViteDev::start(&app.root, "vite.paths.config.js");
    let (status, body) = vite.get("/src/main.ts");
    drop(vite);
    assert_eq!(status, 500, "vite dev fails the import: {body}");
    assert!(body.contains("@/integrations/supabase/client"), "{body}");

    let mut dev = app.dev();
    dev.config = Some(app.root.join("vite.paths.config.js"));
    let server = crate::serve(dev).await;
    // Vite's import analysis fails the importer; so does oj's.
    let main = server.get("/src/main.ts").await;
    assert_eq!(main.status, 500, "{}", main.body);
    assert!(
        main.body.contains("@/integrations/supabase/client"),
        "{}",
        main.body
    );
    let helper = server.text("/src/lib/helper.ts").await;
    assert!(helper.contains("-helper"), "{helper}");
}

/// An exact `paths` key next to `@/*`, and a `paths` key named like an
/// installed package.
const MODE_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"paths-modes","private":true,"type":"module"}"#,
    ),
    ("index.html", INDEX),
    (
        "src/main.ts",
        "import { x } from \"@/x\";\ndocument.body.dataset.x = x;\n",
    ),
    (
        "src/pkg.ts",
        "import { ev } from \"evpkg\";\ndocument.body.dataset.ev = ev;\n",
    ),
    ("src/a.ts", "export const x = \"exact-key\";\n"),
    ("src/x.ts", "export const x = \"star\";\n"),
    ("src/evshim.ts", "export const ev = \"shim\";\n"),
    (
        "node_modules/evpkg/package.json",
        r#"{"name":"evpkg","type":"module","main":"index.js"}"#,
    ),
    (
        "node_modules/evpkg/index.js",
        "export const ev = \"package\";\n",
    ),
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/x": ["./src/a.ts"], "@/*": ["./src/*"], "evpkg": ["./src/evshim.ts"] } } }"#,
    ),
];

// oj applies tsconfig `paths` the way the app's Vite does: vite-tsconfig-paths
// tries `@/*` before the exact key, Vite 8's `resolve.tsconfigPaths` takes the
// exact key first, and with neither Vite applies none, so the package stands
// (oj keeps `paths` only for what plain resolution misses).
#[tokio::test(flavor = "multi_thread")]
async fn tsconfig_paths_follow_how_the_config_turns_them_on() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(MODE_FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    // Each config: its body, whether `@/x` resolves (with neither, Vite
    // fails it), and whether `evpkg` takes the shim.
    let configs: &[(&str, &str, Option<&str>, bool)] = &[
        (
            "plugin",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths()] };\n",
            Some("/src/x.ts"),
            true,
        ),
        (
            "native",
            "export default { resolve: { tsconfigPaths: true } };\n",
            Some("/src/a.ts"),
            true,
        ),
        ("neither", "export default {};\n", None, false),
    ];
    for (name, body, x, shim) in configs {
        let config_file = format!("vite.{name}.config.js");
        app.write(&[(config_file.as_str(), body)]);
        let vite = ViteDev::start(&app.root, &config_file);
        let pkg = vite.imports("/src/pkg.ts");
        let theirs_x = x.map(|_| vite.imports("/src/main.ts"));
        drop(vite);
        let takes_shim = |imports: &[String]| imports.iter().any(|i| i == "/src/evshim.ts");
        assert_eq!(
            takes_shim(&pkg),
            *shim,
            "vite dev, {name}: the premise ({pkg:?})"
        );
        if let (Some(theirs), Some(want)) = (&theirs_x, x) {
            assert_eq!(theirs, &[*want], "vite dev, {name}: the premise");
        }

        let mut dev = app.dev();
        dev.config = Some(app.root.join(&config_file));
        let server = crate::serve(dev).await;
        let ours_pkg = imports(&server.text("/src/pkg.ts").await);
        assert_eq!(
            takes_shim(&ours_pkg),
            *shim,
            "oj dev vs vite dev, {name}: evpkg ({ours_pkg:?})"
        );
        if let Some(theirs) = theirs_x {
            let ours = imports(&server.text("/src/main.ts").await);
            assert_eq!(ours, theirs, "oj dev vs vite dev, {name}: @/x");
        }
        drop(server);
    }
}

/// A root tsconfig and a nested package's own, each with `paths`, and a glob
/// through each.
const GLOB_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"native-globs","private":true,"type":"module"}"#,
    ),
    ("index.html", INDEX),
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
    ),
    (
        "src/m.ts",
        "export const pages = import.meta.glob(\"@/pages/*.ts\");\n",
    ),
    ("src/pages/p.ts", "export const p = 1;\n"),
    (
        "pkg/tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "~/*": ["./lib/*"] } } }"#,
    ),
    (
        "pkg/m.ts",
        "export const views = import.meta.glob(\"~/views/*.ts\");\n",
    ),
    ("pkg/lib/views/v.ts", "export const v = 1;\n"),
    (
        "vite.config.js",
        "export default { resolve: { tsconfigPaths: true } };\n",
    ),
];

/// The `import("…")` targets of a served module, in order.
fn dynamic_imports(js: &str) -> Vec<String> {
    js.split("import(\"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(spec, _)| spec.to_string()))
        .collect()
}

// With `resolve.tsconfigPaths`, Vite's resolve plugin maps an
// `import.meta.glob` pattern through the `paths` of the tsconfig that owns
// the importer (resolve_tsconfig_path_alias): a nested package's glob goes
// through its own tsconfig, not the root one.
#[tokio::test(flavor = "multi_thread")]
async fn a_glob_maps_through_the_importers_own_tsconfig_paths() {
    crate::require_fixture!("vite");
    let app = App::new(GLOB_FILES);
    app.link_fixture("vite");
    let vite = ViteDev::start(&app.root, "vite.config.js");
    let theirs = [
        dynamic_imports(&vite.get("/src/m.ts").1),
        dynamic_imports(&vite.get("/pkg/m.ts").1),
    ];
    drop(vite);
    assert_eq!(
        theirs,
        [
            vec!["/src/pages/p.ts".to_string()],
            vec!["/pkg/lib/views/v.ts".to_string()]
        ],
        "vite dev: the test's premise"
    );
    let mut dev = app.dev();
    dev.config = Some(app.root.join("vite.config.js"));
    let server = crate::serve(dev).await;
    let ours = [
        dynamic_imports(&server.text("/src/m.ts").await),
        dynamic_imports(&server.text("/pkg/m.ts").await),
    ];
    assert_eq!(ours, theirs, "oj dev vs vite dev");
}

// vite-tsconfig-paths maps only TypeScript importers without `allowJs`: a
// `.js` file's `@/a` fails in Vite's dev server, and in oj's, rather than
// shipping the bare `@/a` to the browser.
#[tokio::test(flavor = "multi_thread")]
async fn a_js_importers_paths_import_fails_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"js-importer","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        ("src/a.ts", "export const a = 1;\n"),
        (
            "src/m.js",
            "import { a } from \"@/a\";\nexport const m = a;\n",
        ),
        (
            "src/m.ts",
            "import { a } from \"@/a\";\nexport const m = a;\n",
        ),
    ]);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    app.write(&[("vite.plugin.config.js", &config("{}"))]);
    let vite = ViteDev::start(&app.root, "vite.plugin.config.js");
    let (js_status, _) = vite.get("/src/m.js");
    let ts = vite.imports("/src/m.ts");
    drop(vite);
    assert_eq!(js_status, 500, "vite dev fails the .js importer's import");
    assert_eq!(ts, ["/src/a.ts"]);
    let mut dev = app.dev();
    dev.config = Some(app.root.join("vite.plugin.config.js"));
    let server = crate::serve(dev).await;
    let ours = server.get("/src/m.js").await;
    assert_eq!(ours.status, 500, "{}", ours.body);
    assert!(ours.body.contains("@/a"), "{}", ours.body);
    assert_eq!(imports(&server.text("/src/m.ts").await), ts);
}

/// A root tsconfig, an app one, and a nested package's own, each mapping
/// differently, for vite-tsconfig-paths' options to choose between.
const OPTION_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"plugin-options","private":true,"type":"module"}"#,
    ),
    ("index.html", INDEX),
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./root/*"] } } }"#,
    ),
    (
        "tsconfig.app.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
    ),
    ("root/x.ts", "export const x = \"root\";\n"),
    ("src/x.ts", "export const x = \"app\";\n"),
    (
        "src/main.ts",
        "import { x } from \"@/x\";\nexport const m = x;\n",
    ),
    (
        "src/legacy/old.ts",
        "import { x } from \"@/x\";\nexport const o = x;\n",
    ),
    (
        "pkg/tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "~/*": ["./lib/*"] } } }"#,
    ),
    ("pkg/lib/b.ts", "export const b = 1;\n"),
    (
        "pkg/m.ts",
        "import { b } from \"~/b\";\nexport const p = b;\n",
    ),
];

// vite-tsconfig-paths decides with its own options, which live in its
// closure (Vite never exposes them): a computed `projects` list, an
// `importerFilter` and a `skip` function. oj's plugin host runs the plugin
// ahead of oj's resolver, so oj serves what Vite serves, the imports a
// copy of the plugin with default options would map otherwise included.
#[tokio::test(flavor = "multi_thread")]
async fn the_plugins_own_options_decide_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(OPTION_FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    // Each config, and what Vite 8.2.1 with vite-tsconfig-paths 6.1.1 serves
    // for /src/main.ts, /src/legacy/old.ts and /pkg/m.ts.
    let configs: &[(&str, &str, [&str; 3])] = &[
        (
            "projects",
            "import path from \"node:path\";\nimport tsConfigPaths from \"vite-tsconfig-paths\";\nconst dir = path.dirname(new URL(import.meta.url).pathname);\nconst names = [\"app\"].map((n) => path.join(dir, `tsconfig.${n}.json`));\nexport default { plugins: [tsConfigPaths({ projects: names })] };\n",
            ["/src/x.ts", "/src/x.ts", "status 500"],
        ),
        (
            "filter",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths({ importerFilter: (file) => !file.includes(\"/legacy/\") })] };\n",
            ["/root/x.ts", "status 500", "/pkg/lib/b.ts"],
        ),
        (
            "skip",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths({ skip: (dir) => dir === \"pkg\" })] };\n",
            ["/root/x.ts", "/root/x.ts", "status 500"],
        ),
    ];
    let served = |status: u16, body: &str| -> String {
        if status == 200 {
            imports(body).join(",")
        } else {
            format!("status {status}")
        }
    };
    for (name, body, want) in configs {
        let file = format!("vite.{name}.config.js");
        app.write(&[(file.as_str(), body)]);
        let vite = ViteDev::start(&app.root, &file);
        let theirs: Vec<String> = ["/src/main.ts", "/src/legacy/old.ts", "/pkg/m.ts"]
            .iter()
            .map(|p| {
                let (status, body) = vite.get(p);
                served(status, &body)
            })
            .collect();
        drop(vite);
        assert_eq!(theirs, want, "vite dev, {name}");
        let mut dev = app.dev();
        dev.config = Some(app.root.join(&file));
        let server = crate::serve(dev).await;
        let mut ours = Vec::new();
        for p in ["/src/main.ts", "/src/legacy/old.ts", "/pkg/m.ts"] {
            let r = server.get(p).await;
            ours.push(served(r.status, &r.body));
        }
        drop(server);
        assert_eq!(ours, theirs, "oj dev vs vite dev, {name}");
    }
}

/// An edit while dev runs, in Vite and in oj: `files` laid out, dev served
/// from `web` (relative to the app), `url` fetched before (`before`), then
/// each of `edits` written a second apart; `url` must come to serve `after`.
/// `<root>` in `url` and in served imports is the app's directory.
struct EditCase<'a> {
    files: &'a [(&'a str, &'a str)],
    web: &'a str,
    url: &'a str,
    edits: &'a [(&'a str, &'a str)],
    before: &'a str,
    after: &'a str,
    /// What a served module's body says; its imports when None.
    probe: Option<fn(&str) -> String>,
}

impl EditCase<'_> {
    fn app(&self) -> App {
        let app = App::new(self.files);
        let modules = app.root.join(self.web).join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        for pkg in ["vite", "vite-tsconfig-paths"] {
            std::os::unix::fs::symlink(crate::fixture(pkg).unwrap(), modules.join(pkg)).unwrap();
        }
        app
    }

    fn served(&self, app: &App, status: u16, body: &str) -> String {
        if status == 200 {
            self.probe
                .map_or_else(|| imports(body).join(","), |probe| probe(body))
                .replace(&app.root.display().to_string(), "<root>")
        } else {
            format!("status {status}")
        }
    }

    fn edit(&self, app: &App) {
        // A watcher's `add` settles in the background, chokidar's as oj's.
        std::thread::sleep(Duration::from_millis(1000));
        // A second apart: each edit is its own change, as the test means it
        // (a plugin reloads a project between). Under load the OS delivers
        // writes a few hundred ms apart in one event batch.
        for (file, body) in self.edits {
            std::thread::sleep(Duration::from_millis(1000));
            app.write(&[(*file, *body)]);
        }
    }

    fn url(&self, app: &App) -> String {
        self.url.replace("<root>", &app.root.display().to_string())
    }

    async fn check(&self, label: &str) {
        let app = self.app();
        let vite = ViteDev::start(&app.root.join(self.web), "vite.config.js");
        let url = self.url(&app);
        let (status, body) = vite.get(&url);
        assert_eq!(
            self.served(&app, status, &body),
            self.before,
            "vite before, {label}"
        );
        self.edit(&app);
        // An edit that must change nothing: read once, after a while.
        let wait = if self.before == self.after { 0 } else { 20 };
        if wait == 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        let theirs = loop {
            let (status, body) = vite.get(&url);
            let got = self.served(&app, status, &body);
            if got == self.after || Instant::now() > deadline {
                break got;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        drop(vite);
        assert_eq!(theirs, self.after, "vite after, {label}");

        let app = self.app();
        let mut dev = app.dev();
        dev.root = app.root.join(self.web);
        let server = crate::serve(dev).await;
        let url = self.url(&app);
        let r = server.get(&url).await;
        assert_eq!(
            self.served(&app, r.status, &r.body),
            self.before,
            "oj before, {label}"
        );
        self.edit(&app);
        if wait == 0 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        let ours = loop {
            let r = server.get(&url).await;
            let got = self.served(&app, r.status, &r.body);
            if got == self.after || Instant::now() > deadline {
                break got;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        assert_eq!(ours, self.after, "oj after, {label}");
    }
}

// vite-tsconfig-paths reloads its projects on a watcher event for a
// tsconfig: a `paths` key added while dev runs answers the next transform of
// an importer, in Vite and in oj (whose host plugin hears the same event),
// also when another plugin adds a middleware (oj's middleware server runs).
#[tokio::test(flavor = "multi_thread")]
async fn a_paths_key_added_while_dev_runs_answers_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    const AFTER: &str =
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"], "~lib/*": ["./lib/*"] } } }"#;
    for (label, config) in [
        (
            "the plugin alone",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths()] };\n",
        ),
        (
            "with a middleware plugin",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths(), { name: \"mw\", configureServer(s) { s.middlewares.use((q, r, n) => n()); } }] };\n",
        ),
    ] {
        EditCase {
            files: &[
                ("index.html", "<script type=\"module\" src=\"/src/main.ts\"></script>"),
                ("tsconfig.json", r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#),
                ("lib/x.ts", "export const x = 1;\n"),
                ("src/main.ts", "import { x } from \"~lib/x\";\nconsole.log(x);\n"),
                ("vite.config.js", config),
            ],
            web: ".",
            url: "/src/main.ts",
            edits: &[
                ("tsconfig.json", AFTER),
                ("src/main.ts", "import { x } from \"~lib/x\";\nconsole.log(x, 1);\n"),
            ],
            before: "status 500",
            after: "/lib/x.ts",
            probe: None,
        }
        .check(label)
        .await;
    }
}

// vite-tsconfig-paths watches each project's tsconfig it loads
// (`watcher.add`), one outside Vite's root too: a workspace package's
// tsconfig edit remaps that package's imports, in Vite and in oj.
#[tokio::test(flavor = "multi_thread")]
async fn a_tsconfig_outside_the_root_the_plugin_watches_answers_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    const BEFORE: &str = r#"{ "compilerOptions": { "paths": { "~/*": ["./a/*"] } } }"#;
    const AFTER: &str = r#"{ "compilerOptions": { "paths": { "~/*": ["./b/*"] } } }"#;
    EditCase {
        files: &[
            ("pnpm-workspace.yaml", "packages: ['apps/*', 'packages/*']\n"),
            ("package.json", r#"{"name":"ws","private":true}"#),
            ("apps/web/package.json", r#"{"name":"web","type":"module"}"#),
            ("apps/web/index.html", "<script type=\"module\" src=\"/src/main.ts\"></script>"),
            ("apps/web/src/main.ts", "import { x } from \"../../../packages/ui/src/x.ts\";\nconsole.log(x);\n"),
            (
                "apps/web/vite.config.js",
                "import tsConfigPaths from \"vite-tsconfig-paths\";\nexport default { plugins: [tsConfigPaths({ root: \"../..\" })] };\n",
            ),
            ("packages/ui/tsconfig.json", BEFORE),
            ("packages/ui/a/btn.ts", "export const btn = \"a\";\n"),
            ("packages/ui/b/btn.ts", "export const btn = \"b\";\n"),
            ("packages/ui/src/x.ts", "import { btn } from \"~/btn\";\nexport const x = btn;\n"),
        ],
        web: "apps/web",
        url: "/@fs<root>/packages/ui/src/x.ts",
        edits: &[
            ("packages/ui/tsconfig.json", AFTER),
            ("packages/ui/src/x.ts", "import { btn } from \"~/btn\";\nexport const x = btn + 1;\n"),
        ],
        before: "/@fs<root>/packages/ui/a/btn.ts",
        after: "/@fs<root>/packages/ui/b/btn.ts",
        probe: None,
    }
    .check("a workspace package")
    .await;
}

// The app's tsconfig extends the workspace's, outside the root, whose
// `useDefineForClassFields` decides the class field. Vite's chokidar watches
// the root, and its transform names no tsconfig to watch (rolldown's
// `transformSync` gives `tsconfigFilePaths: []`), so an edit there changes
// nothing until a restart, in Vite and in oj.
#[tokio::test(flavor = "multi_thread")]
async fn an_extended_tsconfig_outside_the_root_changes_nothing_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    fn field(body: &str) -> String {
        if body.contains("this.x = 1") {
            "assigned".to_string()
        } else if body.contains("x = 1") {
            "defined".to_string()
        } else {
            format!("neither: {body}")
        }
    }
    EditCase {
        files: &[
            ("pnpm-workspace.yaml", "packages: ['apps/*']\n"),
            ("package.json", r#"{"name":"ws","private":true}"#),
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "useDefineForClassFields": false } }"#,
            ),
            ("apps/web/package.json", r#"{"name":"web","type":"module"}"#),
            (
                "apps/web/tsconfig.json",
                r#"{ "extends": "../../tsconfig.json" }"#,
            ),
            (
                "apps/web/index.html",
                "<script type=\"module\" src=\"/src/main.ts\"></script>",
            ),
            ("apps/web/src/main.ts", "export class A {\n  x = 1;\n}\n"),
            ("apps/web/vite.config.js", "export default {};\n"),
        ],
        web: "apps/web",
        url: "/src/main.ts",
        edits: &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "useDefineForClassFields": true } }"#,
        )],
        before: "assigned",
        after: "assigned",
        probe: Some(field),
    }
    .check("an extended workspace tsconfig")
    .await;
}

// Vite's reloadOnTsconfigChange takes a file named `tsconfig.json` and no
// other: an edit of the `tsconfig.app.json` the root one references keeps
// the cached tsconfig, so the class field compiles as before, in Vite and
// in oj (which cleared and reloaded for any `tsconfig.*.json`).
#[tokio::test(flavor = "multi_thread")]
async fn a_referenced_tsconfig_edit_keeps_the_cached_one_as_in_vite() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    fn field(body: &str) -> String {
        if body.contains("this.x = 1") {
            "assigned".to_string()
        } else if body.contains("x = 1") {
            "defined".to_string()
        } else {
            format!("neither: {body}")
        }
    }
    let app_json = |define: bool| {
        format!(
            r#"{{ "compilerOptions": {{ "useDefineForClassFields": {define} }}, "include": ["src"] }}"#
        )
    };
    let before = app_json(false);
    let after = app_json(true);
    EditCase {
        files: &[
            ("package.json", r#"{"name":"web","type":"module"}"#),
            (
                "tsconfig.json",
                r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }] }"#,
            ),
            ("tsconfig.app.json", &before),
            (
                "index.html",
                "<script type=\"module\" src=\"/src/main.ts\"></script>",
            ),
            ("src/main.ts", "export class A {\n  x = 1;\n}\n"),
            ("vite.config.js", "export default {};\n"),
        ],
        web: ".",
        url: "/src/main.ts",
        edits: &[("tsconfig.app.json", &after)],
        before: "assigned",
        after: "assigned",
        probe: Some(field),
    }
    .check("a referenced tsconfig")
    .await;
}
