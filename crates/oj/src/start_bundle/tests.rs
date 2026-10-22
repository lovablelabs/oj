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
