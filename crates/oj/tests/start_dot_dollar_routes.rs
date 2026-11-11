// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start route files in dot form with a dynamic segment
//! (`coach.member.$memberId.tsx` for `/coach/member/$memberId`): the router
//! generator imports them by file path, so `$` and `.` must stay plain
//! characters in oj's SSR module ids, the generated tree's imports and the
//! client bundle. The server renders such a route, the page hydrates on it
//! and navigates to another. The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.
//!
//! A dot-form name with a `client` segment (`coach.client.$id.tsx`) is a
//! different matter: Start's import protection denies every `**/*.client.*`
//! file in the server environment (start-plugin-core's
//! `getDefaultImportProtectionRules`), the route tree gets the plugin's mock
//! in place of the route module, and router-core's `processRouteTree` throws
//! "route.children is not iterable" on every request. Vite's dev server on
//! the same app answers the same 500, so oj matches it, and both print the
//! plugin's own "[import-protection] Import denied" report naming the file
//! and the pattern. Renaming the segment, or the folder form
//! (`coach/client/$id.tsx`), is the fix under either server.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "dot form routes" }] }),
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
      <Link id="to-coach" to="/coach/member/$memberId" params={{ memberId: "42" }}>coach</Link>
    </main>
  ),
});
"#;

/// `src/routes/coach.member.$memberId.tsx`: two dots and a `$` in the name.
const COACH_MEMBER: &str = r#"import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/coach/member/$memberId")({
  component: Coach,
});

function Coach() {
  const { memberId } = Route.useParams();
  return (
    <main>
      <p id="coach">{`coach-member-${memberId}`}</p>
      <Link id="to-plan" to="/plan/$id" params={{ id: "7" }}>plan</Link>
    </main>
  );
}
"#;

/// `src/routes/plan.$id.tsx`: the `$id` spelling.
const PLAN: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/plan/$id")({
  component: Plan,
});

function Plan() {
  const { id } = Route.useParams();
  return <p id="plan">{`plan-${id}`}</p>;
}
"#;

/// `src/routes/coach.client.$id.tsx`: a dot-form name whose segment is
/// `client`, which Start's import protection reads as a client-only file.
const COACH_CLIENT: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/coach/client/$id")({
  component: Coach,
});

function Coach() {
  const { id } = Route.useParams();
  return <p id="coach">{`coach-client-${id}`}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The fixture app made file-routed: its routes replaced by `routes`, its
/// router reading the generated tree.
fn file_routed_app(name: &str, routes: &[(&str, &str)]) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let dir = root.join("src/routes");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for (file, code) in routes {
        std::fs::write(dir.join(file), code).unwrap();
    }
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// The dev server's lines about the Start host, for a failure message.
fn start_lines(dev: &Server) -> String {
    dev.output()
        .lines()
        .filter(|l| l.contains("oj start:") || l.contains("import-protection"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn dot_form_route_files_with_dynamic_segments_render_on_the_server_and_hydrate() {
    let Some(app) = file_routed_app(
        "dot-dollar-routes",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("coach.member.$memberId.tsx", COACH_MEMBER),
            ("plan.$id.tsx", PLAN),
        ],
    ) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);

    let (status, home) = dev.request("GET", "/", &[]);
    assert!(
        status == 200 && home.contains("home page"),
        "GET /: {status}\n{home}\n{}",
        dev.output()
    );
    for (path, marker) in [
        ("/coach/member/42", "coach-member-42"),
        ("/plan/7", "plan-7"),
    ] {
        let (status, html) = dev.request("GET", path, &[]);
        assert!(
            status == 200 && html.contains(marker),
            "GET {path}: {status}, no {marker:?} in the server's render\n{html}\n{}",
            dev.output()
        );
    }
    assert!(
        !dev.output().contains("[import-protection]"),
        "a dot-form route file was denied by import protection\n{}",
        dev.output()
    );

    if browser::skip() {
        return;
    }
    // A hard visit to the dot-form route hydrates on it (the client bundle's
    // module for the same file), then the router navigates to the other one.
    let page = Page::open(&dev.url("/coach/member/42")).await;
    let hydrated = page
        .poll(
            r#"document.getElementById("coach")?.textContent === "coach-member-42" && !!self.__TSR_ROUTER__"#,
            Duration::from_secs(60),
        )
        .await;
    assert!(
        hydrated,
        "/coach/member/42 never hydrated: {:?}\n{}",
        page.errors(),
        start_lines(&dev)
    );
    page.eval::<bool>(r##"(document.querySelector("#to-plan").click(), true)"##)
        .await;
    let navigated = page
        .poll(
            r#"document.getElementById("plan")?.textContent === "plan-7" && location.pathname === "/plan/7""#,
            Duration::from_secs(30),
        )
        .await;
    // Late hydration errors surface after the first paint.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let errors = page.errors();
    page.close().await;
    assert!(
        navigated,
        "the router never reached /plan/7: {errors:?}\n{}",
        start_lines(&dev)
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

/// Not an oj defect, recorded so the next report of "route.children is not
/// iterable" on such a file finds its cause: Start's import protection denies
/// the `*.client.*` route file on the server, under oj as under Vite, and
/// says so.
#[test]
fn a_client_segment_in_a_dot_form_route_name_is_denied_by_start_as_under_vite() {
    let Some(app) = file_routed_app(
        "dot-client-route",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("coach.client.$id.tsx", COACH_CLIENT),
        ],
    ) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, body) = dev.request("GET", "/coach/client/42", &[]);
    assert_eq!(
        status,
        500,
        "Start's mocked route module fails every request under Vite\n{body}\n{}",
        dev.output()
    );
    for needle in [
        "[import-protection] Import denied in server environment",
        "Denied by file pattern: **/*.client.*",
        "coach.client.$id.tsx",
        "route.children is not iterable",
    ] {
        assert!(
            dev.logs(needle),
            "the server's output lacks {needle:?}\n{}",
            dev.output()
        );
    }
}
