// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! StyleX end to end: the `oj` binary serves and builds a React app that
//! installs the real `@stylexjs/stylex` and `@stylexjs/unplugin` 0.19.0
//! (e2e/fixtures/stylex-app), and a headless Chrome checks what the user
//! sees: computed styles from `create`, `defineVars` tokens, a conditional
//! style, `:hover` and `keyframes`, an edit restyling the page in place, and
//! a production build whose sheet is byte-identical to Vite's.
//!
//! The fixture is installed by `npm ci --prefix e2e/fixtures/stylex-app`;
//! without it (or Chrome) the tests skip, unless `OJ_REQUIRE_FIXTURES` (or
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

/// The installed fixture, or None (after saying why) when it is not.
fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/fixtures/stylex-app");
    let dir = dir.canonicalize().unwrap();
    if dir.join("node_modules/@stylexjs/unplugin").is_dir() {
        return Some(dir);
    }
    assert!(
        std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
        "OJ_REQUIRE_FIXTURES is set but the stylex-app fixture is not installed"
    );
    eprintln!("skipping: npm ci --prefix e2e/fixtures/stylex-app");
    None
}

/// A private copy of the fixture's sources (dev and build write into the
/// app), sharing its installed `node_modules`.
fn app(name: &str) -> Option<PathBuf> {
    let fixture = fixture()?;
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

/// An `oj` server process, killed on drop.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(root: &Path, args: &[&str]) -> Server {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_oj"))
            .args(args)
            .arg(root)
            .args(["--port", &port.to_string()])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Server { child, port };
        let deadline = Instant::now() + WAIT;
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("oj {args:?} exited before listening: {status}");
            }
            assert!(Instant::now() < deadline, "oj {args:?} never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
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

    // A conditional style applies on a state change.
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
    let Some(root) = app("dev") else { return };
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
    let Some(root) = app("build") else { return };
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
