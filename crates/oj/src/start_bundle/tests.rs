// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start bundle plugins through a real rolldown bundle over temp files
//! (no plugin host): what each import convention becomes in the output.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};

use super::alias::AppAliasPlugin;
use super::assets::{AssetMode, DevStyleState, Emitter, StartAssetsPlugin, Styles};
use super::code::{CodeTarget, StartCodePlugin};
use super::errors::{unresolved_import_in, BundleError};
use super::facts::normalize_alias;
use super::shims::NodeBuiltinShims;
use super::StartConfig;

struct App {
    dir: PathBuf,
}

impl App {
    fn new(files: &[(&str, &str)]) -> App {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oj-start-bundle-it-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        App { dir }
    }

    fn start(&self) -> StartConfig {
        super::dev::tests::start_at(&self.dir)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // An in-process plugin host moves the whole test process into its
        // app root: step out before the root goes, or every later
        // `current_dir()` (rolldown's diagnostics) fails.
        if std::env::current_dir().map_or(true, |cwd| cwd.starts_with(&self.dir)) {
            let _ = std::env::set_current_dir(std::env::temp_dir());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The bundle of `entry` with `plugins`; the output's code, or its error.
async fn bundle(
    app: &App,
    entry: &str,
    plugins: Vec<SharedPluginable>,
) -> Result<String, BundleError> {
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("entry".into()),
                import: app.dir.join(entry).to_string_lossy().into_owned(),
            }]),
            cwd: Some(app.dir.clone()),
            platform: Some(Platform::Browser),
            format: Some(OutputFormat::Esm),
            ..Default::default()
        })
        .build()
        .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
    let out = bundler
        .generate()
        .await
        .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
    let _ = bundler.close().await;
    if let Some(err) = unresolved_import_in(&out.warnings) {
        return Err(err);
    }
    Ok(out
        .assets
        .iter()
        .filter_map(|o| match o {
            rolldown_common::Output::Chunk(c) => Some(c.code.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn dev_assets(app: &App, css_urls: &Arc<Mutex<Vec<String>>>) -> SharedPluginable {
    Pluginable::new_shared(StartAssetsPlugin::new(
        &app.dir,
        Arc::new(Styles::new(&app.start())),
        AssetMode::Dev {
            css_urls: Arc::clone(css_urls),
            state: Arc::new(DevStyleState::default()),
            url_base: String::new(),
        },
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_asset_imports_point_at_the_dev_server() {
    let app = App::new(&[
        (
            "src/entry.js",
            r#"import raw from "./note.txt?raw";
import url from "./logo.png";
import inline from "./dot.png?inline";
import icon from "./icon.svg";
import worker from "./w.js?worker&url";
import "./plain.css";
import styles from "./card.module.css";
console.log(raw, url, inline, icon, worker, styles.card);
"#,
        ),
        ("src/note.txt", "hello raw"),
        ("src/logo.png", "png-bytes"),
        ("src/dot.png", "dot"),
        ("src/icon.svg", "<svg/>"),
        ("src/w.js", "self.onmessage = () => {};"),
        ("src/plain.css", ".plain { color: red }"),
        ("src/card.module.css", ".card { color: blue }"),
    ]);
    let css_urls = Arc::new(Mutex::new(Vec::new()));
    let code = bundle(&app, "src/entry.js", vec![dev_assets(&app, &css_urls)])
        .await
        .unwrap();
    let src = app.dir.join("src");
    assert!(
        code.contains("\"hello raw\""),
        "?raw exports the text:\n{code}"
    );
    assert!(
        code.contains(&format!(
            "\"/@oj-start/fs{}\"",
            src.join("logo.png").display()
        )),
        "{code}"
    );
    assert!(
        code.contains("\"data:image/png;base64,ZG90\""),
        "?inline is a data URI:\n{code}"
    );
    assert!(
        code.contains(&format!(
            "\"/@oj-start/fs{}\"",
            src.join("icon.svg").display()
        )),
        "{code}"
    );
    let rel = src
        .join("w.js")
        .strip_prefix(&app.dir)
        .unwrap()
        .display()
        .to_string();
    assert!(
        code.contains(&format!("\"/{rel}\"")),
        "a dev worker URL is its dev-pipeline URL:\n{code}"
    );
    assert!(
        !code.contains("color: red"),
        "a plain sheet is linked, never inlined:\n{code}"
    );
    let linked = css_urls.lock().unwrap().clone();
    assert_eq!(linked.len(), 2, "both sheets are linked: {linked:?}");
    assert!(linked[0].ends_with("/src/plain.css"), "{linked:?}");
    assert!(
        code.contains("card"),
        "a CSS module exports its class map:\n{code}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_emits_assets_once_by_content_hash() {
    let app = App::new(&[
        (
            "src/entry.js",
            r#"import a from "./logo.png";
import b from "./copy.png?url";
import "./style.css";
console.log(a, b);
"#,
        ),
        ("src/logo.png", "same-bytes"),
        ("src/copy.png", "same-bytes"),
        ("src/font.woff2", "font"),
        ("src/style.css", ".a { background: url(./font.woff2) }"),
    ]);
    let start = app.start();
    let client = app.dir.join("dist/client");
    let styles = Arc::new(Styles::new(&start));
    let emitter = Arc::new(Emitter::new(&client, "/", Arc::clone(&styles)));
    let plugin = Pluginable::new_shared(StartAssetsPlugin::new(
        &app.dir,
        styles,
        AssetMode::Build {
            emitter: Arc::clone(&emitter),
            server: false,
            worker_chain: Vec::new(),
        },
    ));
    let code = bundle(&app, "src/entry.js", vec![plugin]).await.unwrap();
    let urls: Vec<&str> = code
        .split('"')
        .filter(|s| s.starts_with("/assets/"))
        .collect();
    assert_eq!(urls.len(), 2, "{code}");
    assert_ne!(urls[0], urls[1], "names differ, the hash agrees");
    assert_eq!(
        &urls[0][urls[0].len() - 12..],
        &urls[1][urls[1].len() - 12..],
        "same bytes, same hash"
    );
    let css = emitter.css_urls();
    assert_eq!(css.len(), 1, "{css:?}");
    let sheet = std::fs::read_to_string(client.join(css[0].trim_start_matches('/'))).unwrap();
    assert!(
        sheet.contains("url(/assets/font-") || sheet.contains("url(\"/assets/font-"),
        "a sheet's url() points at the emitted file:\n{sheet}"
    );
    assert!(std::fs::read_dir(client.join("assets")).unwrap().count() >= 3);
}

/// The StyleX sheet joins the first stylesheet the client emitted, under the
/// name and link it already has, as the plugin's generateBundle appends it;
/// with none it is the plugin's unlinked `assets/stylex.css`.
#[tokio::test(flavor = "multi_thread")]
async fn late_css_joins_the_first_client_sheet_in_place() {
    let app = App::new(&[
        ("src/a.css", ".a{color:red}"),
        ("src/b.css", ".b{color:blue}"),
    ]);
    let start = app.start();
    let client = app.dir.join("dist/client");
    let emitter = Emitter::new(&client, "/", Arc::new(Styles::new(&start)));
    let a = emitter.emit(&app.dir.join("src/a.css")).await.unwrap();
    let b = emitter.emit(&app.dir.join("src/b.css")).await.unwrap();
    let file = |url: &str| client.join(url.trim_start_matches('/'));
    let before = std::fs::read_to_string(file(&a)).unwrap();
    emitter
        .append_generated_css("stylex.css", ".x1{color:green}")
        .unwrap();

    assert_eq!(
        emitter.css_urls(),
        vec![a.clone(), b.clone()],
        "no new link"
    );
    assert_eq!(
        std::fs::read_to_string(file(&a)).unwrap(),
        format!("{before}\n.x1{{color:green}}"),
        "appended after the first sheet"
    );
    assert!(!std::fs::read_to_string(file(&b)).unwrap().contains(".x1"));
    assert!(!client.join("assets/stylex.css").exists());

    let bare = App::new(&[]);
    let bare_client = bare.dir.join("dist/client");
    let emitter = Emitter::new(&bare_client, "/", Arc::new(Styles::new(&bare.start())));
    emitter
        .append_generated_css("stylex.css", ".x1{color:green}")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(bare_client.join("assets/stylex.css")).unwrap(),
        ".x1{color:green}"
    );
    assert!(
        emitter.css_urls().is_empty(),
        "the fallback is linked from nowhere"
    );
}

/// A stylesheet that fails to compile names why and which plugin, as Vite's
/// error does; rolldown alone said only "plugin threw an error".
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_asset_load_says_why() {
    let app = App::new(&[
        ("src/entry.js", "import \"./b.styl\";\n"),
        ("src/b.styl", ".box\n  color red\n"),
    ]);
    let start = app.start();
    let styles = Arc::new(Styles::new(&start));
    let emitter = Arc::new(Emitter::new(
        &app.dir.join("dist/client"),
        "/",
        Arc::clone(&styles),
    ));
    let plugin = Pluginable::new_shared(StartAssetsPlugin::new(
        &app.dir,
        styles,
        AssetMode::Build {
            emitter,
            server: false,
            worker_chain: Vec::new(),
        },
    ));
    let err = bundle(&app, "src/entry.js", vec![plugin])
        .await
        .expect_err("stylus is not installed");
    assert!(
        err.message.contains("is stylus installed?"),
        "{}",
        err.message
    );
    assert!(err.message.contains("[oj:start-assets]"), "{}", err.message);
    // The overlay names the plugin and the module, as Vite's does.
    assert_eq!(err.plugin.as_deref(), Some("oj:start-assets"));
    let id = err.id.clone().unwrap_or_default();
    assert!(id.ends_with("src/b.styl"), "{id:?}");
    assert!(
        err.loc.is_none() && err.frame.is_empty(),
        "a load has no spot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn node_builtins_become_browser_stubs_unless_a_package_owns_the_name() {
    let app = App::new(&[
        (
            "src/entry.js",
            r#"import { createHmac } from "crypto";
import { AsyncLocalStorage } from "node:async_hooks";
import { EventEmitter } from "events";
console.log(typeof createHmac, new AsyncLocalStorage(), EventEmitter);
"#,
        ),
        (
            "node_modules/events/package.json",
            r#"{"name":"events","main":"index.js"}"#,
        ),
        (
            "node_modules/events/index.js",
            "exports.EventEmitter = class FromNpm {};",
        ),
    ]);
    let code = bundle(
        &app,
        "src/entry.js",
        vec![Pluginable::new_shared(NodeBuiltinShims {
            production: false,
        })],
    )
    .await
    .unwrap();
    assert!(
        code.contains("has been externalized for browser compatibility"),
        "{code}"
    );
    assert!(
        code.contains("AsyncLocalStorage") && code.contains("getStore()"),
        "the working stand-in:\n{code}"
    );
    assert!(
        code.contains("FromNpm"),
        "the installed `events` package wins:\n{code}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_alias_runs_ahead_and_a_miss_fails_loudly() {
    let app = App::new(&[
        (
            "src/entry.js",
            "import { x } from \"@/lib/x\";\nconsole.log(x);\n",
        ),
        ("src/lib/x.js", "export const x = \"aliased-x\";"),
    ]);
    let alias = |target: &str| {
        Pluginable::new_shared(AppAliasPlugin {
            entries: normalize_alias(&serde_json::json!({ "@": target })),
        })
    };
    let code = bundle(
        &app,
        "src/entry.js",
        vec![alias(&app.dir.join("src").to_string_lossy())],
    )
    .await
    .unwrap();
    assert!(code.contains("aliased-x"), "{code}");
    let missing = bundle(&app, "src/entry.js", vec![alias("/nowhere")]).await;
    assert!(
        missing.is_err(),
        "an alias to nothing never ships a bare import"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unresolved_import_fails_with_vites_error_and_a_frame() {
    let app = App::new(&[(
        "src/typo.js",
        "import { thing } from \"no-such-dep-typo\";\nconsole.log(thing);\n",
    )]);
    let err = bundle(&app, "src/typo.js", Vec::new()).await.unwrap_err();
    assert!(
        err.message
            .contains("Rolldown failed to resolve import \"no-such-dep-typo\""),
        "{}",
        err.message
    );
    assert!(err.message.contains("build.rolldownOptions.external"));
    assert!(
        err.id
            .as_deref()
            .is_some_and(|id| id.ends_with("src/typo.js")),
        "{err:?}"
    );
    assert!(
        err.frame
            .starts_with("  1|  import { thing } from \"no-such-dep-typo\";"),
        "{}",
        err.frame
    );
    assert!(!err.frame.contains("external dependency"));
}

#[tokio::test(flavor = "multi_thread")]
async fn app_code_gets_globs_expanded_and_server_fns_split() {
    let app = App::new(&[
        (
            "src/entry.ts",
            r#"import { getGreeting } from "./server/data";
const pages = import.meta.glob("./pages/*.ts", { eager: true, import: "title" });
console.log(getGreeting, pages);
"#,
        ),
        ("src/pages/a.ts", "export const title = \"page-a\";"),
        ("src/pages/b.ts", "export const title = \"page-b\";"),
        (
            "src/server/data.ts",
            "import { createServerFn } from \"./fake-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"secret-server-only\");\n",
        ),
        (
            "src/server/fake-start.ts",
            "export const createServerFn = () => ({ handler: (fn: unknown) => fn });\n",
        ),
        (
            "node_modules/@tanstack/react-start/package.json",
            r#"{"name":"@tanstack/react-start","exports":{"./client-rpc":"./client-rpc.js"}}"#,
        ),
        (
            "node_modules/@tanstack/react-start/client-rpc.js",
            "export const createClientRpc = (id) => ({ rpc: id });",
        ),
    ]);
    let code = bundle(
        &app,
        "src/entry.ts",
        vec![Pluginable::new_shared(StartCodePlugin::new(
            &app.dir,
            CodeTarget::Client,
        ))],
    )
    .await
    .unwrap();
    assert!(code.contains("page-a") && code.contains("page-b"), "{code}");
    let id = oj_compiler::server_fns::server_fn_id("src/server/data.ts", "getGreeting");
    assert!(
        code.contains(&id),
        "the client calls the server fn by id:\n{code}"
    );
    assert!(
        !code.contains("secret-server-only"),
        "the handler never ships to the browser:\n{code}"
    );
}

// An aliased glob (`@/pages/*.ts`) expands through the app's `resolve.alias`
// in a Start bundle, as Vite's toAbsoluteGlob resolves it, not as a path
// under the importer's directory.
#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_glob_expands_through_resolve_alias() {
    let app = App::new(&[
        (
            "src/entry.ts",
            "const pages = import.meta.glob(\"@/pages/*.ts\", { eager: true });\n\
             console.log(Object.values(pages).map((p: any) => p.title));\n",
        ),
        ("src/pages/a.ts", "export const title = \"page-a\";"),
        ("src/pages/b.ts", "export const title = \"page-b\";"),
    ]);
    let alias = normalize_alias(&serde_json::json!({ "@": app.dir.join("src") }));
    let code = bundle(
        &app,
        "src/entry.ts",
        vec![Pluginable::new_shared(
            StartCodePlugin::new(&app.dir, CodeTarget::Client).with_alias(alias),
        )],
    )
    .await
    .unwrap();
    assert!(code.contains("page-a") && code.contains("page-b"), "{code}");
}

#[test]
fn the_test_app_root_is_canonical() {
    let app = App::new(&[]);
    assert_eq!(app.dir, app.dir.canonicalize().unwrap());
    assert!(Path::new(&app.dir).is_absolute());
}

// The top-level `resolve.mainFields` is the client's (a resolved Vite config
// always fills it browser-first); a server bundle takes its own environment's
// list, so a package's `browser` build never reaches the server render.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_bundle_never_takes_a_packages_browser_build() {
    let app = App::new(&[
        (
            "src/entry.js",
            "import { side } from \"dual\";\nconsole.log(side);\n",
        ),
        (
            "node_modules/dual/package.json",
            r#"{"name":"dual","main":"node.js","browser":"browser.js"}"#,
        ),
        (
            "node_modules/dual/node.js",
            "exports.side = \"node-build\";",
        ),
        (
            "node_modules/dual/browser.js",
            "exports.side = \"browser-build\";",
        ),
    ]);
    let mut start = app.start();
    start.config = serde_json::from_value(serde_json::json!({
        "resolve": { "mainFields": ["browser", "module", "jsnext:main", "jsnext"] }
    }))
    .unwrap();
    let run = |main_fields: Option<Vec<String>>, platform| {
        let options = BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("entry".into()),
                import: app.dir.join("src/entry.js").to_string_lossy().into_owned(),
            }]),
            cwd: Some(app.dir.clone()),
            platform: Some(platform),
            format: Some(OutputFormat::Esm),
            resolve: Some(start.resolve_options(&[], main_fields, None)),
            ..Default::default()
        };
        async move {
            let mut bundler = BundlerBuilder::default()
                .with_options(options)
                .build()
                .unwrap();
            let out = bundler.generate().await.unwrap();
            out.assets
                .iter()
                .filter_map(|o| match o {
                    rolldown_common::Output::Chunk(c) => Some(c.code.clone()),
                    _ => None,
                })
                .collect::<String>()
        }
    };
    let server = run(Some(start.server_main_fields()), Platform::Node).await;
    assert!(
        server.contains("node-build") && !server.contains("browser-build"),
        "{server}"
    );
    let client = run(start.client_main_fields(), Platform::Browser).await;
    assert!(
        client.contains("browser-build"),
        "the client keeps its browser build:\n{client}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn environment_functions_compile_per_bundle_in_app_and_dep_code() {
    // A dep compiled the same way: TanStack's own packages call the factories.
    let app = App::new(&[
        (
            "src/entry.ts",
            "import { who, secret } from \"./env\";\nimport { router } from \"tanstack-dep\";\nconsole.log(who(), secret, router());\n",
        ),
        (
            "src/env.ts",
            "import { createIsomorphicFn, createServerOnlyFn } from \"@tanstack/start-fn-stubs\";\nexport const who = createIsomorphicFn().server(() => \"app-server-side\").client(() => \"app-client-side\");\nexport const secret = createServerOnlyFn(() => \"app-server-only\");\n",
        ),
        (
            "node_modules/tanstack-dep/package.json",
            r#"{"name":"tanstack-dep","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/tanstack-dep/index.js",
            "import { createIsomorphicFn } from \"@tanstack/start-fn-stubs\";\nexport const router = createIsomorphicFn().client(() => \"dep-client-side\").server(() => \"dep-server-side\");\n",
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/package.json",
            r#"{"name":"@tanstack/start-fn-stubs","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/index.js",
            "export const createIsomorphicFn = () => ({});\nexport const createServerOnlyFn = (f) => f;\n",
        ),
    ]);
    let build = |target: CodeTarget| {
        bundle(
            &app,
            "src/entry.ts",
            vec![Pluginable::new_shared(StartCodePlugin::new(
                &app.dir, target,
            ))],
        )
    };
    let client = build(CodeTarget::Client).await.unwrap();
    for gone in ["app-server-side", "app-server-only", "dep-server-side"] {
        assert!(
            !client.contains(gone),
            "{gone} in the client bundle:\n{client}"
        );
    }
    for kept in [
        "app-client-side",
        "dep-client-side",
        "can only be called on the server!",
    ] {
        assert!(
            client.contains(kept),
            "{kept} missing from the client bundle:\n{client}"
        );
    }
    let server = build(CodeTarget::Server).await.unwrap();
    for gone in ["app-client-side", "dep-client-side"] {
        assert!(
            !server.contains(gone),
            "{gone} in the server bundle:\n{server}"
        );
    }
    for kept in ["app-server-side", "app-server-only", "dep-server-side"] {
        assert!(
            server.contains(kept),
            "{kept} missing from the server bundle:\n{server}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_sides_imports_leave_the_bundle() {
    // Plain JS has no TypeScript import elision: the unused import stays
    // unless the env-fn compile drops it, as TanStack's dead-code pass does.
    let app = App::new(&[
        ("src/entry.ts", "import { who } from \"./env\";\nconsole.log(who());\n"),
        (
            "src/env.js",
            "import { createIsomorphicFn } from \"@tanstack/start-fn-stubs\";\nimport { db } from \"./db\";\nexport const who = createIsomorphicFn().server(() => db.q()).client(() => \"client-side\");\n",
        ),
        (
            "src/db.ts",
            "import net from \"node:net\";\nconsole.log(\"db-module\", net);\nexport const db = { q: () => 1 };\n",
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/package.json",
            r#"{"name":"@tanstack/start-fn-stubs","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/index.js",
            "export const createIsomorphicFn = () => ({});\n",
        ),
    ]);
    let client = bundle(
        &app,
        "src/entry.ts",
        vec![Pluginable::new_shared(StartCodePlugin::new(
            &app.dir,
            CodeTarget::Client,
        ))],
    )
    .await
    .unwrap_or_else(|e| panic!("the client bundle: {}", e.message));
    assert!(client.contains("client-side"), "{client}");
    assert!(
        !client.contains("db-module"),
        "the server side's import in the client bundle:\n{client}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_factory_reexported_by_app_modules_compiles() {
    // TanStack follows an import through the modules that re-export it.
    let app = App::new(&[
        (
            "src/entry.ts",
            "import { who } from \"./env\";\nconsole.log(who());\n",
        ),
        (
            "src/env.ts",
            "import { createIsomorphicFn } from \"./lib/start\";\nexport const who = createIsomorphicFn().server(() => \"server-side\").client(() => \"client-side\");\n",
        ),
        ("src/lib/start.ts", "export * from \"./inner\";\n"),
        (
            "src/lib/inner.ts",
            "export { createIsomorphicFn } from \"@tanstack/start-fn-stubs\";\n",
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/package.json",
            r#"{"name":"@tanstack/start-fn-stubs","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/@tanstack/start-fn-stubs/index.js",
            "export const createIsomorphicFn = () => ({});\n",
        ),
    ]);
    let client = bundle(
        &app,
        "src/entry.ts",
        vec![Pluginable::new_shared(StartCodePlugin::new(
            &app.dir,
            CodeTarget::Client,
        ))],
    )
    .await
    .unwrap_or_else(|e| panic!("the client bundle: {}", e.message));
    assert!(client.contains("client-side"), "{client}");
    assert!(!client.contains("server-side"), "{client}");
}

/// Vite's bundleWorkerEntry: a worker a worker imports bundles under the
/// importer's chain, and a worker reached again through its own chain fails
/// naming it (it would otherwise wait on its own unfinished bundle).
#[tokio::test(flavor = "multi_thread")]
async fn nested_workers_carry_the_chain_and_a_cycle_fails() {
    let app = App::new(&[("src/a.js", ""), ("src/b.js", "")]);
    let start = app.start();
    let styles = Arc::new(Styles::new(&start));
    let emitter = Arc::new(Emitter::new(
        &app.dir.join("dist/client"),
        "/",
        Arc::clone(&styles),
    ));
    let (a, b) = (app.dir.join("src/a.js"), app.dir.join("src/b.js"));
    let chains: Arc<Mutex<Vec<Vec<PathBuf>>>> = Arc::default();
    let weak = Arc::downgrade(&emitter);
    let seen = Arc::clone(&chains);
    let next = b.clone();
    emitter.set_worker_bundler(
        Arc::new(move |file: PathBuf, chain: Vec<PathBuf>| {
            seen.lock().unwrap().push(chain.clone());
            let emitter = weak.upgrade().unwrap();
            // a inlines b, b takes a's URL (as the client did).
            let into_b = file != next;
            let import = if into_b {
                next.clone()
            } else {
                chain[0].clone()
            };
            Box::pin(async move {
                if into_b {
                    emitter.worker_code(&import, &chain).await?;
                } else {
                    emitter.worker(&import, &chain).await?;
                }
                Ok(super::assets::WorkerBundle {
                    code: String::new(),
                    file_name: "w.js".into(),
                })
            })
        }),
        false,
    );
    let err = match emitter.worker(&a, &[]).await {
        Ok(_) => panic!("a circular worker import bundled"),
        Err(e) => e,
    };
    assert_eq!(
        err,
        "Circular worker imports detected. Vite does not support it. Import chain: src/a.js -> src/b.js -> src/a.js"
    );
    assert_eq!(
        *chains.lock().unwrap(),
        vec![vec![a.clone()], vec![a.clone(), b.clone()]],
        "each bundle sees its importers, then itself"
    );
}

/// Two workers importing each other, both imported by the client: each
/// bundle runs under its own chain (Vite shares only finished bundles), so
/// both fail naming the cycle instead of waiting on each other.
#[tokio::test(flavor = "multi_thread")]
async fn a_cycle_between_two_client_workers_fails_both() {
    let app = App::new(&[("src/a.js", ""), ("src/b.js", "")]);
    let start = app.start();
    let styles = Arc::new(Styles::new(&start));
    let emitter = Arc::new(Emitter::new(
        &app.dir.join("dist/client"),
        "/",
        Arc::clone(&styles),
    ));
    let (a, b) = (app.dir.join("src/a.js"), app.dir.join("src/b.js"));
    // Both client-started bundles are in flight before either imports.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let weak = Arc::downgrade(&emitter);
    let (a2, b2) = (a.clone(), b.clone());
    emitter.set_worker_bundler(
        Arc::new(move |file: PathBuf, chain: Vec<PathBuf>| {
            let emitter = weak.upgrade().unwrap();
            let import = if file == a2 { b2.clone() } else { a2.clone() };
            let barrier = Arc::clone(&barrier);
            Box::pin(async move {
                if chain.len() == 1 {
                    barrier.wait().await;
                }
                emitter.worker(&import, &chain).await?;
                Ok(super::assets::WorkerBundle {
                    code: String::new(),
                    file_name: "w.js".into(),
                })
            })
        }),
        false,
    );
    let (ra, rb) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(emitter.worker(&a, &[]), emitter.worker(&b, &[]))
    })
    .await
    .expect("the two bundles wait on each other");
    let cycle = "Circular worker imports detected. Vite does not support it. Import chain: ";
    assert_eq!(
        ra.unwrap_err(),
        format!("{cycle}src/a.js -> src/b.js -> src/a.js")
    );
    assert_eq!(
        rb.unwrap_err(),
        format!("{cycle}src/b.js -> src/a.js -> src/b.js")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stylex_compiles_app_modules_and_loads_its_runtime() {
    let app = App::new(&[
        (
            "src/entry.ts",
            "import * as stylex from \"@stylexjs/stylex\";\n\
             import \"virtual:stylex:runtime\";\n\
             const styles = stylex.create({ a: { color: \"olive\" } });\n\
             export const c = stylex.props(styles.a).className;\n",
        ),
        (
            "node_modules/@stylexjs/stylex/package.json",
            r#"{"name":"@stylexjs/stylex","main":"index.js"}"#,
        ),
        (
            "node_modules/@stylexjs/stylex/index.js",
            "export const create = () => { throw new Error(\"uncompiled\"); };\n",
        ),
    ]);
    let plugin = std::sync::Arc::new(
        oj_stylex::plugin::StylexPlugin::new(&serde_json::json!({}), &app.dir, false, false)
            .unwrap(),
    );
    let code = bundle(
        &app,
        "src/entry.ts",
        vec![crate::stylex_plugin::transform(&plugin)],
    )
    .await
    .unwrap();
    assert!(
        !code.contains("uncompiled"),
        "stylex.create compiled away:\n{code}"
    );
    assert!(
        code.contains("__stylex_virtual__"),
        "the runtime module loaded:\n{code}"
    );
    let sheet = plugin.stylesheet().unwrap().expect("the module's rules");
    assert!(sheet.contains("color:olive"), "{sheet}");
    let class = sheet.trim_start_matches('.').split('{').next().unwrap();
    assert!(code.contains(class), "the bundle uses {class}:\n{code}");
}

const SLOT_SPIES: &str = r#"import { appendFileSync } from "node:fs";
const record = (name) => function (code, id) {
  if (!/\.(scss|css)(\?|$)/.test(id)) return null;
  const kind = code.includes("$c")
    ? "source"
    : /export (default|const|\{)|^$/.test(code) || code.startsWith("import")
      ? "js"
      : /\.m[\s{]/.test(code) || !id.includes(".module.")
        ? "css"
        : "css-scoped";
  appendFileSync(__SEEN__, `${this.environment?.name} ${name} ${id.slice(id.lastIndexOf("/") + 1)} ${kind}\n`);
  return null;
};
const spy = (name, enforce) => ({ name, enforce, transform: record(name) });
const virtualCss = {
  name: "virtual-css",
  resolveId(id) { if (id === "virtual:theme.css") return "\0virtual:theme.css"; },
  load(id) { if (id === "\0virtual:theme.css") return ".from-plugin { color: rebeccapurple; }\n"; },
};
export default [spy("post", "post"), spy("normal"), spy("pre", "pre"), virtualCss];
"#;

/// What the spies record in the client bundle of a Start app importing a
/// Sass sheet, an `?inline` one, a CSS module and a sheet a plugin serves,
/// in `command`; and the stylesheets a build emits.
async fn client_slot_records(command: super::Command) -> (Vec<String>, String) {
    // Outside the app, beside it.
    let seen = std::env::temp_dir().join(format!(
        "oj-start-slots-{}-{command:?}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&seen);
    let spies = SLOT_SPIES.replace(
        "__SEEN__",
        &serde_json::to_string(&seen.display().to_string()).unwrap(),
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"start-slots","type":"module"}"#),
        ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
        ("src/b.scss", "$c: blue;\n.b { color: $c; }\n"),
        ("src/m.module.css", ".m { color: teal; }\n"),
        ("src/u.scss", "$c: green;\n.u { color: $c; }\n"),
        (
            "src/entry.js",
            "import \"./a.scss\";\nimport b from \"./b.scss?inline\";\nimport m from \"./m.module.css\";\nimport \"virtual:theme.css\";\nimport u from \"./u.scss?url\";\nconsole.log(b, m.m, u);\n",
        ),
        ("oj.plugins.mjs", &spies),
    ]);
    let start = super::StartConfig {
        command,
        ..app.start()
    };
    let host = super::hosts::spawn(&start, None, "client")
        .await
        .unwrap()
        .expect("the app's plugins");
    let styles = Arc::new(Styles::new(&start));
    let mode = match command {
        super::Command::Serve => AssetMode::Dev {
            css_urls: Arc::default(),
            state: Arc::new(DevStyleState::default()),
            url_base: String::new(),
        },
        super::Command::Build => AssetMode::Build {
            emitter: Arc::new(Emitter::new(
                &app.dir.join("dist/client"),
                "/",
                Arc::clone(&styles),
            )),
            server: false,
            worker_chain: Vec::new(),
        },
    };
    let plugins = vec![
        Pluginable::new_shared(super::host::StartHostPlugin::new(
            Arc::clone(&host.host),
            host.ctx.clone(),
            &host.facts,
            command,
            &app.dir,
            Arc::new(crate::build::EmitState::new(app.dir.clone())),
            start.jsx_config(),
        )),
        Pluginable::new_shared(
            StartAssetsPlugin::new(&app.dir, styles, mode).with_plugins(Some(host.host)),
        ),
    ];
    bundle(&app, "src/entry.js", plugins).await.unwrap();
    let css = std::fs::read_dir(app.dir.join("dist/client/assets"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "css"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect::<String>();
    let text = std::fs::read_to_string(&seen).expect("the spies ran");
    let _ = std::fs::remove_file(&seen);
    let seen = text;
    let mut seen: Vec<String> = seen.lines().map(str::to_string).collect();
    seen.sort_unstable();
    (seen, css)
}

/// The client environment's lines Vite 8.3.5 records for this app under
/// TanStack Start, in dev and in the build: each sheet once through each slot,
/// "pre" plugins on the source, "normal" ones on the CSS (a module's classes
/// scoped), "post" ones on the module standing in for it. A `?url` import is
/// JS to every plugin; the build compiles its sheet as `?transform-only`
/// ([`VITE_CLIENT_BUILD_URL_SLOTS`]).
const VITE_CLIENT_SLOTS: [&str; 15] = [
    "client normal \0virtual:theme.css css",
    "client normal a.scss css",
    "client normal b.scss?inline css",
    "client normal m.module.css css-scoped",
    "client post \0virtual:theme.css js",
    "client post a.scss js",
    "client post b.scss?inline js",
    "client post m.module.css js",
    "client pre \0virtual:theme.css css",
    "client pre a.scss source",
    "client pre b.scss?inline source",
    "client pre m.module.css css",
    "client normal u.scss?url js",
    "client post u.scss?url js",
    "client pre u.scss?url js",
];

const VITE_CLIENT_BUILD_URL_SLOTS: [&str; 3] = [
    "client normal u.scss?transform-only css",
    "client post u.scss?transform-only js",
    "client pre u.scss?transform-only source",
];

fn sorted(lines: &[&[&str]]) -> Vec<String> {
    let mut all: Vec<String> = lines
        .iter()
        .flat_map(|l| l.iter().map(|s| s.to_string()))
        .collect();
    all.sort_unstable();
    all
}

#[tokio::test(flavor = "multi_thread")]
async fn stylesheet_transforms_run_in_vites_slots_in_the_client_build() {
    let (seen, css) = client_slot_records(super::Command::Build).await;
    assert_eq!(
        seen,
        sorted(&[&VITE_CLIENT_SLOTS, &VITE_CLIENT_BUILD_URL_SLOTS])
    );
    // Vite ships the plugin-served sheet with the client's CSS, and the
    // `?url` one compiled.
    assert!(css.contains(".from-plugin{color:#639}"), "{css}");
    assert!(css.contains(".u{color:green}"), "{css}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stylesheet_transforms_run_in_vites_slots_in_the_client_dev_bundle() {
    assert_eq!(
        client_slot_records(super::Command::Serve).await.0,
        sorted(&[&VITE_CLIENT_SLOTS])
    );
}

/// CSS a plugin hands vite:css-post for a module (UnoCSS's layers) takes the
/// place of the sheet emitted for it, in the links and on disk; CSS for no
/// emitted sheet is linked as its own.
#[tokio::test(flavor = "multi_thread")]
async fn plugin_css_takes_the_place_of_its_modules_sheet() {
    let app = App::new(&[("src/a.css", ".a{color:red}")]);
    let start = app.start();
    let client = app.dir.join("dist/client");
    let emitter = Emitter::new(&client, "/", Arc::new(Styles::new(&start)));
    let placeholder = app.dir.join("src/__uno.css");
    let a = emitter.emit(&app.dir.join("src/a.css")).await.unwrap();
    let old = emitter
        .css(&placeholder, "#--unocss--{layer:__ALL__}".into(), None)
        .await
        .unwrap();
    emitter
        .plugin_css(&placeholder.display().to_string(), ".p-4{padding:1rem}")
        .unwrap();
    emitter.plugin_css("", ".late{margin:1px}").unwrap();

    let urls = emitter.css_urls();
    assert_eq!(urls.len(), 3, "{urls:?}");
    assert_eq!(urls[0], a, "the other sheet keeps its place");
    assert_ne!(urls[1], old, "a new name for the new content");
    let file = |url: &str| client.join(url.trim_start_matches('/'));
    assert!(!file(&old).exists(), "the placeholder sheet is gone");
    assert!(std::fs::read_to_string(file(&urls[1]))
        .unwrap()
        .contains(".p-4{padding:1rem}"));
    assert!(std::fs::read_to_string(file(&urls[2]))
        .unwrap()
        .contains(".late{margin:1px}"));
}

/// A sheet the dev bundle ran through the plugins serves the page's `<link>`
/// until the next bundle or edit, so each plugin sees it once, as in Vite.
#[test]
fn a_dev_sheet_is_kept_until_the_next_bundle_or_edit() {
    let app = App::new(&[("src/a.css", ".a{}")]);
    let styles = Styles::new(&app.start());
    let a = app.dir.join("src/a.css");
    assert_eq!(styles.dev_sheet(&a), None);
    styles.dev_sheets_stale();
    styles.keep_dev_sheet(&a, ".a{color:red}".into(), styles.dev_stamp(), None);
    assert_eq!(styles.dev_sheet(&a).as_deref(), Some(".a{color:red}"));
    assert_eq!(styles.dev_sheet(&app.dir.join("src/b.css")), None);
    styles.dev_sheets_stale();
    assert_eq!(
        styles.dev_sheet(&a),
        None,
        "stale after the next bundle or edit"
    );
}

/// With only a `?url` sheet emitted (`import appCss from "./styles.css?url"`,
/// linked by the app's own `head`), late CSS joins that sheet, the first CSS
/// asset in the bundle, as the StyleX plugin's generateBundle finds it; the
/// page's links stay the app's.
#[tokio::test(flavor = "multi_thread")]
async fn late_css_joins_a_url_sheet_when_nothing_is_linked() {
    let app = App::new(&[("src/styles.css", "body{margin:0}")]);
    let start = app.start();
    let client = app.dir.join("dist/client");
    let emitter = Emitter::new(&client, "/", Arc::new(Styles::new(&start)));
    let url = emitter
        .css_asset(
            &app.dir.join("src/styles.css"),
            "body{margin:0}".into(),
            None,
        )
        .await
        .unwrap();
    assert!(emitter.css_urls().is_empty(), "a ?url sheet is not linked");
    let file = client.join(url.trim_start_matches('/'));
    let before = std::fs::read_to_string(&file).unwrap();
    emitter
        .append_generated_css("stylex.css", ".x1{color:green}")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{before}\n.x1{{color:green}}")
    );
    assert!(
        !client.join("assets/stylex.css").exists(),
        "no separate sheet"
    );
}

/// UnoCSS's placeholder sheet replaced by its generated CSS, then StyleX's
/// late CSS: it joins the generated sheet, the placeholder's file being gone.
#[tokio::test(flavor = "multi_thread")]
async fn late_css_joins_the_sheet_plugin_css_replaced() {
    let app = App::new(&[]);
    let start = app.start();
    let client = app.dir.join("dist/client");
    let emitter = Emitter::new(&client, "/", Arc::new(Styles::new(&start)));
    let placeholder = app.dir.join("src/__uno.css");
    emitter
        .css(&placeholder, "#--unocss--{layer:__ALL__}".into(), None)
        .await
        .unwrap();
    emitter
        .plugin_css(&placeholder.display().to_string(), ".p-4{padding:1rem}")
        .unwrap();
    emitter
        .append_generated_css("stylex.css", ".x1{margin:1px}")
        .unwrap();
    let urls = emitter.css_urls();
    assert_eq!(urls.len(), 1, "{urls:?}");
    let sheet = std::fs::read_to_string(client.join(urls[0].trim_start_matches('/'))).unwrap();
    assert!(
        sheet.contains(".p-4{padding:1rem}") && sheet.ends_with(".x1{margin:1px}"),
        "{sheet}"
    );
}

/// A sheet a plugin serves with no file is current while the plugin loads
/// the same text for it, bundles and edits aside; once the plugin's text
/// grows (UnoCSS's, as modules transform), it compiles again.
#[test]
fn a_served_sheet_is_kept_while_the_plugin_loads_the_same_text() {
    let app = App::new(&[]);
    let styles = Styles::new(&app.start());
    let id = Path::new("\0virtual:theme.css");
    styles.keep_dev_sheet(id, ".t{}".into(), styles.dev_stamp(), Some(".t {}"));
    styles.dev_sheets_stale();
    assert_eq!(
        styles.dev_served_sheet(id, ".t {}").as_deref(),
        Some(".t{}")
    );
    assert_eq!(styles.dev_served_sheet(id, ".t {} .u {}"), None);
    assert_eq!(styles.dev_sheet(id), None, "not a file's sheet");
}

/// How often a plugin's renderStart ran over three bundles (a boot and two
/// edits' rebundles) of a Start client in `command`.
async fn render_starts(command: super::Command) -> usize {
    let seen = std::env::temp_dir().join(format!(
        "oj-start-render-start-{}-{command:?}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&seen);
    let plugins = format!(
        r#"import fs from "node:fs";
export default [{{
  name: "render-start-spy",
  renderStart() {{ fs.appendFileSync({}, "renderStart\n"); }},
}}];
"#,
        serde_json::to_string(&seen.display().to_string()).unwrap()
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"start-render-start","type":"module"}"#,
        ),
        ("src/entry.js", "console.log(1);\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let start = super::StartConfig {
        command,
        ..app.start()
    };
    let host = super::hosts::spawn(&start, None, "client")
        .await
        .unwrap()
        .expect("the app's plugins");
    for edit in 0..3 {
        std::fs::write(
            app.dir.join("src/entry.js"),
            format!("console.log({edit});\n"),
        )
        .unwrap();
        let plugin = super::host::StartHostPlugin::new(
            Arc::clone(&host.host),
            host.ctx.clone(),
            &host.facts,
            command,
            &app.dir,
            Arc::new(crate::build::EmitState::new(app.dir.clone())),
            start.jsx_config(),
        );
        bundle(&app, "src/entry.js", vec![Pluginable::new_shared(plugin)])
            .await
            .unwrap();
    }
    let text = std::fs::read_to_string(&seen).unwrap_or_default();
    let _ = std::fs::remove_file(&seen);
    text.lines().count()
}

/// Vite's dev server runs no output-generation hook: the Start dev bundles,
/// over the dev server's own plugin instances, never call renderStart; the
/// build does, per bundle.
#[tokio::test(flavor = "multi_thread")]
async fn render_start_runs_in_the_build_only() {
    assert_eq!(render_starts(super::Command::Serve).await, 0);
    assert_eq!(render_starts(super::Command::Build).await, 3);
}
