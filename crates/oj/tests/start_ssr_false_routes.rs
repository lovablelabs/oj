// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start routes the server does not render (`ssr: false`) or
//! renders without their component (`ssr: "data-only"`), as Vite serves them,
//! checked in a browser: where each visit lands, what renders, and that the
//! page hydrates with no error.
//!
//! The app has a protected layout (`ssr: false`, its `beforeLoad` redirects to
//! `/auth` without a session), a page under it, a sign-in page that is
//! `ssr: false` itself, a `ssr: "data-only"` page whose loader runs on the
//! server, and a server-rendered home linking to the protected page. Every
//! case runs on `oj dev`, on Vite's dev server for the same app, and on the
//! app `oj build` built; each must land where Vite's does and render what
//! Vite's does, with no page error.
//!
//! Before route code splitting (oj 0.4.1), a hard visit to the protected page
//! rendered its component on the server while the client treated it as
//! client-only: React threw "There was an error while hydrating" and "an
//! error during concurrent rendering" (the TanStack `MatchView` "reading
//! routeId" crash), and recovered by client rendering.
//!
//! The browser cases skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use browser::Page;
use start_app::{app, build, free_port, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "ssr modes" }] }),
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const INDEX: &str = r#"import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <main>
      <p id="home">home page</p>
      <Link id="to-dashboard" to="/dashboard">dashboard</Link>
    </main>
  ),
});
"#;

const AUTH: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/auth")({
  ssr: false,
  component: () => <p id="auth">sign in</p>,
});
"#;

const AUTHED: &str = r#"import { createFileRoute, Outlet, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed")({
  ssr: false,
  beforeLoad: () => {
    if (!localStorage.getItem("session")) {
      throw redirect({ to: "/auth" });
    }
  },
  component: () => (
    <div id="authed">
      <Outlet />
    </div>
  ),
});
"#;

const DASHBOARD: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/dashboard")({
  component: () => <p id="dash">dashboard page</p>,
});
"#;

const REPORT: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/report")({
  ssr: "data-only",
  loader: () => ({ figure: "data-only-figure" }),
  component: ReportPage,
});

function ReportPage() {
  const { figure } = Route.useLoaderData();
  return <p id="report">{figure}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn ssr_modes_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(routes.join("_authed")).unwrap();
    for (file, code) in [
        ("__root.tsx", ROOT),
        ("index.tsx", INDEX),
        ("auth.tsx", AUTH),
        ("_authed.tsx", AUTHED),
        ("_authed/dashboard.tsx", DASHBOARD),
        ("report.tsx", REPORT),
    ] {
        std::fs::write(routes.join(file), code).unwrap();
    }
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// Vite's dev server on `root`, once it answers.
struct ViteDev {
    child: Child,
    port: u16,
}

impl ViteDev {
    fn start(root: &Path) -> ViteDev {
        let port = free_port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args([
                "dev",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--strictPort",
            ])
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
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One visit: how it starts, and the element that shows it settled.
struct Case {
    name: &'static str,
    /// The page opened first.
    open: &'static str,
    /// JS run once `ready` holds on the first page (a session, a click).
    then: Option<&'static str>,
    /// The element whose presence ends the visit.
    settled: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "a hard visit to the protected page without a session",
        open: "/dashboard",
        then: None,
        settled: "auth",
    },
    Case {
        name: "a hard visit to the client-only sign-in page",
        open: "/auth",
        then: None,
        settled: "auth",
    },
    Case {
        name: "a hard visit to the protected page with a session",
        open: "/auth",
        then: Some(r#"localStorage.setItem("session", "1"); location.assign("/dashboard")"#),
        settled: "dash",
    },
    Case {
        name: "a client navigation to the protected page without a session",
        open: "/",
        then: Some(r##"document.querySelector("#to-dashboard").click()"##),
        settled: "auth",
    },
    Case {
        name: "a hard visit to a data-only page",
        open: "/report",
        then: None,
        settled: "report",
    },
    Case {
        name: "a hard visit to the server-rendered home",
        open: "/",
        then: None,
        settled: "home",
    },
];

/// Where a visit ended: the path, what rendered, the page's errors.
#[derive(Debug, PartialEq)]
struct Outcome {
    path: String,
    ids: Vec<String>,
    text: String,
}

const READY: &str = r#"document.readyState === "complete""#;

async fn visit(base: &str, case: &Case) -> (Outcome, Vec<String>) {
    let page = Page::open(&format!("{base}{}", case.open)).await;
    assert!(
        page.poll(READY, Duration::from_secs(60)).await,
        "{}: the page never loaded: {:?}",
        case.name,
        page.errors()
    );
    if let Some(then) = case.then {
        // Hydrated first, so a click is the router's, not the browser's.
        page.poll(
            r##"document.querySelector("#auth, #home") && self.__TSR_ROUTER__"##,
            Duration::from_secs(30),
        )
        .await;
        let _: bool = page
            .eval(&format!("(() => {{ {then}; return true; }})()"))
            .await;
    }
    let settled = format!(
        r#"document.getElementById("{}") && self.__TSR_ROUTER__"#,
        case.settled
    );
    let found = page.poll(&settled, Duration::from_secs(30)).await;
    // Late hydration errors surface after the first paint.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let path: String = page.eval("location.pathname").await;
    let ids: Vec<String> = page
        .eval(r#"[...document.body.querySelectorAll("[id]")].map((e) => e.id).sort()"#)
        .await;
    let text: String = page
        .eval(&format!(
            r#"document.getElementById("{}")?.textContent ?? """#,
            case.settled
        ))
        .await;
    let errors = page.errors();
    page.close().await;
    assert!(
        found,
        "{}: #{} never rendered at {base} (at {path}, {ids:?}): {errors:?}",
        case.name, case.settled
    );
    (Outcome { path, ids, text }, errors)
}

/// Every case on `base`: each settles with no page error. The outcomes.
async fn run_cases(base: &str, label: &str) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for case in CASES {
        let (outcome, errors) = visit(base, case).await;
        assert_eq!(
            errors,
            Vec::<String>::new(),
            "{label}: {}: page errors",
            case.name
        );
        outcomes.push(outcome);
    }
    outcomes
}

/// What the server renders of each page: a client-only route's component
/// never, a data-only route's loader data but not its component.
fn assert_server_renders(html_of: impl Fn(&str) -> (u16, String), label: &str) {
    let (status, home) = html_of("/");
    assert!(
        status == 200 && home.contains("home page"),
        "{label} /: {status}\n{home}"
    );
    for path in ["/auth", "/dashboard"] {
        let (status, html) = html_of(path);
        assert_eq!(status, 200, "{label} {path}\n{html}");
        assert!(
            !html.contains("sign in") && !html.contains("dashboard page"),
            "{label} {path}: the server rendered a client-only component\n{html}"
        );
    }
    let (status, report) = html_of("/report");
    assert_eq!(status, 200, "{label} /report\n{report}");
    assert!(
        report.contains("data-only-figure"),
        "{label} /report: no loader data in the page\n{report}"
    );
    assert!(
        !report.contains("<p id=\"report\">"),
        "{label} /report: the server rendered a data-only component\n{report}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_only_and_data_only_routes_match_vite_in_dev() {
    let Some(app) = ssr_modes_app("ssr-modes-dev") else {
        return;
    };
    let root = &app.0;
    let dev = Server::oj_dev(root);
    assert_server_renders(|path| dev.request("GET", path, &[]), "oj dev");
    if browser::skip() {
        return;
    }
    let oj = run_cases(&dev.url(""), "oj dev").await;
    drop(dev);

    let vite = ViteDev::start(root);
    let vite_base = format!("http://127.0.0.1:{}", vite.port);
    let theirs = run_cases(&vite_base, "vite dev").await;
    drop(vite);
    for ((case, ours), theirs) in CASES.iter().zip(&oj).zip(&theirs) {
        assert_eq!(ours, theirs, "{}: oj dev vs vite dev", case.name);
    }
    assert_eq!(
        oj[0].path, "/auth",
        "the protected page redirects to sign in"
    );
    assert_eq!(
        oj[2].path, "/dashboard",
        "a session reaches the protected page"
    );
    assert_eq!(
        oj[4].text, "data-only-figure",
        "the data-only page renders its loader data"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_only_and_data_only_routes_render_from_the_build() {
    let Some(app) = ssr_modes_app("ssr-modes-build") else {
        return;
    };
    let root = &app.0;
    build(root);
    let server = Server::node_server(root);
    assert_server_renders(|path| server.request("GET", path, &[]), "oj build");
    if browser::skip() {
        return;
    }
    let outcomes = run_cases(&server.url(""), "oj build").await;
    assert_eq!(outcomes[0].path, "/auth");
    assert_eq!(outcomes[2].path, "/dashboard");
    assert_eq!(outcomes[4].text, "data-only-figure");
}
