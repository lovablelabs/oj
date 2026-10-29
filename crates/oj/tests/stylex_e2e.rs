// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! StyleX end to end: the `oj` binary serves and builds React apps that
//! install the real `@stylexjs/stylex` and `@stylexjs/unplugin` 0.19.0, a
//! Vite SPA (e2e/fixtures/stylex-app) and a TanStack Start app
//! (e2e/fixtures/start-stylex-app), and a headless Chrome checks what the
//! user sees: computed styles from `create`, `defineVars` tokens, a
//! conditional style, `:hover` and `keyframes`, an edit restyling the page in
//! place, and a production build whose rules are byte-identical to Vite's.
//!
//! The fixtures are installed by `npm ci --prefix e2e/fixtures/<name>`;
//! without them (or Chrome) the tests skip, unless `OJ_REQUIRE_FIXTURES` (or
//! `OJ_REQUIRE_BROWSER`) is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use browser::Page;

const WAIT: Duration = Duration::from_secs(30);

const SPA: &str = "stylex-app";
const START: &str = "start-stylex-app";

/// The installed fixture, or None (after saying why) when it is not.
fn fixture(fixture: &str) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures")
        .join(fixture);
    let dir = dir.canonicalize().unwrap();
    if dir.join("node_modules/@stylexjs/unplugin").is_dir() {
        return Some(dir);
    }
    assert!(
        std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
        "OJ_REQUIRE_FIXTURES is set but the {fixture} fixture is not installed"
    );
    eprintln!("skipping: npm ci --prefix e2e/fixtures/{fixture}");
    None
}

/// A private copy of the fixture's sources (dev and build write into the
/// app), sharing its installed `node_modules`.
fn app(fixture_name: &str, name: &str) -> Option<PathBuf> {
    let fixture = fixture(fixture_name)?;
    if browser::skip() {
        return None;
    }
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-stylex-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy_sources(&fixture, &root);
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    Some(root)
}

fn copy_sources(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name();
        if matches!(name.to_str(), Some("node_modules" | "dist" | ".oj-cache")) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_sources(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A server process, stopped on drop.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// `oj <args> <root> --port <free>`.
    fn start(root: &Path, args: &[&str]) -> Server {
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oj"));
        cmd.args(args).arg(root).args(["--port", &port.to_string()]);
        Server::spawn(cmd.current_dir(root), port)
    }

    /// `cmd` once it listens on `port`.
    fn spawn(cmd: &mut Command, port: u16) -> Server {
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Server { child, port };
        let deadline = Instant::now() + WAIT;
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("{cmd:?} exited before listening: {status}");
            }
            assert!(Instant::now() < deadline, "{cmd:?} never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for Server {
    /// SIGTERM first: oj stops the children it spawned (Start's bundler)
    /// on it, where a SIGKILL would orphan them.
    fn drop(&mut self) {
        // SAFETY: a signal to the child this test spawned and still owns.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn computed(id: &str, prop: &str) -> String {
    format!("getComputedStyle(document.getElementById({id:?})).{prop}")
}

/// What every page of the fixture shows before any interaction.
async fn assert_styled(page: &Page) {
    page.wait_for(
        &format!("{} === 'rgb(0, 0, 255)'", computed("title", "color")),
        WAIT,
    )
    .await;
    let card_bg: String = page.eval(&computed("card", "backgroundColor")).await;
    let padding: String = page.eval(&computed("card", "paddingTop")).await;
    let radius: String = page.eval(&computed("card", "borderTopLeftRadius")).await;
    let weight: String = page.eval(&computed("title", "fontWeight")).await;
    let animation: String = page.eval(&computed("card", "animationName")).await;
    assert_eq!(card_bg, "rgb(240, 240, 240)", "defineVars token");
    assert_eq!(padding, "12px");
    assert_eq!(radius, "4px");
    assert_eq!(weight, "700");
    assert!(animation.starts_with('x'), "keyframes name: {animation}");
    // `:hover` cannot be forced from script; its rule must be in a sheet.
    let hover: bool = page
        .eval(
            "[...document.styleSheets].some((s) => [...s.cssRules].some((r) => \
             r.selectorText?.includes(':hover') && r.style.color === 'red'))",
        )
        .await;
    assert!(hover, "the :hover rule reached the page");

    // A conditional style applies on a state change, once React owns the
    // page: TanStack drops `$_TSR` when a server render has hydrated.
    page.wait_for("window.$_TSR === undefined", WAIT).await;
    page.eval::<bool>("document.getElementById('toggle').click(), true")
        .await;
    page.wait_for(
        &format!("{} === 'rgb(0, 128, 0)'", computed("title", "color")),
        WAIT,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_styles_the_page_and_an_edit_restyles_it_in_place() {
    let Some(root) = app(SPA, "dev") else { return };
    let server = Server::start(&root, &["dev"]);
    let page = Page::open(&server.url()).await;
    assert_styled(&page).await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    // A changed rule and a new one, from the same edit.
    let card = root.join("src/Card.tsx");
    let source = std::fs::read_to_string(&card).unwrap();
    let edited = source.replace("padding: 12,", "padding: 20,").replace(
        "fontWeight: 700,",
        "fontWeight: 700,\n    letterSpacing: 2,",
    );
    assert_ne!(source, edited);
    std::fs::write(&card, edited).unwrap();
    page.wait_for(
        &format!(
            "{} === '20px' && {} === '2px'",
            computed("card", "paddingTop"),
            computed("title", "letterSpacing")
        ),
        WAIT,
    )
    .await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "the edit reloaded the page"
    );
    let color: String = page.eval(&computed("title", "color")).await;
    assert_eq!(color, "rgb(0, 128, 0)", "component state survived the edit");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_styles_the_page_with_vites_exact_sheet() {
    let Some(root) = app(SPA, "build") else {
        return;
    };
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build"])
        .arg(&root)
        .current_dir(&root));

    // The same app through Vite and the real plugin.
    let vite_out = root.join("vite-dist");
    run(Command::new("node")
        .arg(root.join("node_modules/vite/bin/vite.js"))
        .args(["build", "--outDir"])
        .arg(&vite_out)
        .current_dir(&root));
    let sheet = |dir: &Path| -> String {
        let assets = std::fs::read_dir(dir.join("assets")).unwrap();
        let css: Vec<PathBuf> = assets
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "css"))
            .collect();
        assert_eq!(css.len(), 1, "one stylesheet in {}: {css:?}", dir.display());
        std::fs::read_to_string(&css[0]).unwrap()
    };
    let ours = sheet(&root.join("dist"));
    let vites = sheet(&vite_out);
    assert!(ours.contains("margin:0"), "the app's own sheet: {ours}");
    assert_eq!(
        ours.trim_end(),
        vites.trim_end(),
        "oj's build sheet differs from Vite's"
    );
    let html = std::fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(
        html.contains("rel=\"stylesheet\""),
        "the sheet is linked:\n{html}"
    );

    let server = Server::start(&root, &["preview"]);
    let page = Page::open(&server.url()).await;
    assert_styled(&page).await;
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// The `.css` files under `dir`, by file name.
fn css_files(dir: &Path) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "css"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect();
    files.sort();
    files
}

/// Start under `oj dev`: the server-rendered page already carries StyleX's
/// classes, the dev sheet the root route links styles it, and an edit
/// restyles it in place.
#[tokio::test(flavor = "multi_thread")]
async fn start_dev_styles_the_page_and_an_edit_restyles_it_in_place() {
    let Some(root) = app(START, "start-dev") else {
        return;
    };
    let server = Server::start(&root, &["dev"]);
    let page = Page::open(&server.url()).await;
    let ssr: String = page.eval("fetch('/').then((r) => r.text())").await;
    assert!(
        regex_class(&ssr, "title"),
        "the server render names StyleX classes:\n{ssr}"
    );
    assert_styled(&page).await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    let card = root.join("src/Card.tsx");
    let source = std::fs::read_to_string(&card).unwrap();
    let edited = source.replace("padding: 12,", "padding: 20,").replace(
        "fontWeight: 700,",
        "fontWeight: 700,\n    letterSpacing: 2,",
    );
    assert_ne!(source, edited);
    std::fs::write(&card, edited).unwrap();
    page.wait_for(
        &format!(
            "{} === '20px' && {} === '2px'",
            computed("card", "paddingTop"),
            computed("title", "letterSpacing")
        ),
        WAIT,
    )
    .await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "the edit reloaded the page"
    );
    let color: String = page.eval(&computed("title", "color")).await;
    assert_eq!(color, "rgb(0, 128, 0)", "component state survived the edit");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// Whether the element with `id` in `html` has a StyleX class (`x` + base36).
fn regex_class(html: &str, id: &str) -> bool {
    let tag = format!("id=\"{id}\"");
    html.split('<').any(|el| {
        el.contains(&tag)
            && el
                .split("class=\"")
                .nth(1)
                .and_then(|c| c.split('"').next())
                .is_some_and(|c| c.split(' ').any(|n| n.starts_with('x') && n.len() > 4))
    })
}

/// The Start fixture rendered by @cloudflare/vite-plugin's worker: the app
/// copy beside the plugin's install (e2e/fixtures/cloudflare-deps, its
/// parent's node_modules), the fixture's own install in the app. Returns the
/// directory holding both.
fn cloudflare_app(name: &str) -> Option<(PathBuf, PathBuf)> {
    let deps = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/cloudflare-deps")
        .canonicalize()
        .unwrap();
    if !deps.join("node_modules/@cloudflare/vite-plugin").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the cloudflare-deps fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/cloudflare-deps");
        return None;
    }
    let source = app(START, name)?;
    let parent = source.with_extension("cf");
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).unwrap();
    std::os::unix::fs::symlink(deps.join("node_modules"), parent.join("node_modules")).unwrap();
    let root = parent.join("app");
    std::fs::rename(&source, &root).unwrap();
    std::fs::write(
        root.join("wrangler.jsonc"),
        r#"{"name":"oj-start-stylex","main":"@tanstack/react-start/server-entry","compatibility_date":"2025-09-01","compatibility_flags":["nodejs_compat"]}"#,
    )
    .unwrap();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let config = config
        .replacen(
            "import { defineConfig } from \"vite\";",
            "import { defineConfig } from \"vite\";\nimport { cloudflare } from \"@cloudflare/vite-plugin\";",
            1,
        )
        .replacen(
            "tanstackStart(),",
            "cloudflare({ viteEnvironment: { name: \"ssr\" } }), tanstackStart(),",
            1,
        );
    assert!(
        config.contains("cloudflare({"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();
    Some((parent, root))
}

/// Start under `oj dev` with the server render in a Cloudflare worker, a
/// runner environment the plugin host runs through the app's Vite: the
/// worker's modules are compiled as the browser's are, so the page renders
/// with StyleX's classes and the dev sheet styles it, and an edit reaches
/// both the worker's render and the page.
#[tokio::test(flavor = "multi_thread")]
async fn start_dev_in_a_cloudflare_worker_styles_the_page() {
    let Some((parent, root)) = cloudflare_app("start-cf-dev") else {
        return;
    };
    let server = Server::start(&root, &["dev"]);
    let page = Page::open(&server.url()).await;
    let (status, ssr): (u16, String) = page
        .eval("fetch('/').then(async (r) => [r.status, await r.text()])")
        .await;
    assert_eq!(status, 200, "the worker's render failed:\n{ssr}");
    assert!(
        regex_class(&ssr, "title"),
        "the worker's render names StyleX classes:\n{ssr}"
    );
    assert_styled(&page).await;
    let before = class_of(&ssr, "card").expect("the card's class");

    let card = root.join("src/Card.tsx");
    let source = std::fs::read_to_string(&card).unwrap();
    let edited = source.replace("padding: 12,", "padding: 20,");
    assert_ne!(source, edited);
    std::fs::write(&card, edited).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let ssr: String = page.eval("fetch('/').then((r) => r.text())").await;
        if class_of(&ssr, "card").is_some_and(|c| c != before) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the worker's render never picked up the edit:\n{ssr}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    page.wait_for(
        &format!("{} === '20px'", computed("card", "paddingTop")),
        WAIT,
    )
    .await;
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&parent);
}

/// The class attribute of the element with `id` in `html`.
fn class_of(html: &str, id: &str) -> Option<String> {
    let tag = format!("id=\"{id}\"");
    html.split('<').find(|el| el.contains(&tag)).and_then(|el| {
        el.split("class=\"")
            .nth(1)
            .and_then(|c| c.split('"').next())
            .map(str::to_string)
    })
}

/// A Start build: the client's stylesheet is Vite's byte for byte, StyleX's
/// rules appended to the app's own sheet as the plugin's generateBundle does,
/// and the production server's page is styled by it.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_build_styles_the_page_with_vites_sheet() {
    let Some(root) = app(START, "start-build") else {
        return;
    };
    let Some(vite_root) = app(START, "start-vite") else {
        return;
    };
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build"])
        .arg(&root)
        .current_dir(&root));
    run(Command::new("node")
        .arg(vite_root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&vite_root));

    let vites = css_files(&vite_root.join("dist/client/assets"));
    let ours = css_files(&root.join("dist/client/assets"));
    assert_eq!(vites.len(), 1, "Vite's client sheets: {vites:?}");
    assert_eq!(ours.len(), 1, "oj's client sheets: {ours:?}");
    assert_eq!(
        ours[0].1, vites[0].1,
        "oj's Start client sheet differs from Vite's"
    );

    let port = free_port();
    let server = Server::spawn(
        Command::new("node")
            .arg(root.join("dist/server.mjs"))
            .env("PORT", port.to_string())
            .current_dir(&root),
        port,
    );
    let page = Page::open(&server.url()).await;
    assert_styled(&page).await;
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&vite_root);
}
