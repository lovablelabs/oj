// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Renaming a route file in Start dev. The rename reaches the dev server as
//! the old file's deletion and the new file's creation, after which the
//! route generator rewrites `routeTree.gen.ts`. The served bundle has to
//! follow: no chunk of the deleted file, the renamed route rendered at its
//! path, and an open page onto the new route tree (reloaded, as Vite reloads
//! a page whose module graph lost a module that is still imported), never
//! left on the old one. The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use browser::Page;
use start_app::{app, Server, WAIT};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts, useRouter } from "@tanstack/react-router";
import { useEffect } from "react";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "renamed routes" }] }),
  component: RootComponent,
});

function RootComponent() {
  const router = useRouter();
  useEffect(() => {
    const page = self as { __ojHydrated?: boolean; __ojNavigate?: (to: string) => void };
    page.__ojNavigate = (to) => {
      void router.navigate({ to });
    };
    page.__ojHydrated = true;
  }, [router]);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home page</p>,
});
"#;

/// The `/members` layout: the detail route starts out nested under it.
const MEMBERS: &str = r#"import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/members")({
  component: Members,
});

function Members() {
  return (
    <main>
      <p id="members">members page</p>
      <Outlet />
    </main>
  );
}
"#;

/// The detail route, `/members/$id`, as `members.$id.tsx`.
const MEMBER: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/members/$id")({
  component: Member,
});

function Member() {
  const { id } = Route.useParams();
  return <p id="member">{`member-${id}`}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The fixture app moved to file-based routing, with the members routes.
fn members_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(routes.join("members.tsx"), MEMBERS).unwrap();
    std::fs::write(routes.join("members.$id.tsx"), MEMBER).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// The dev bundle's cache directory (`.oj-cache/v<n>/start`).
fn start_cache(root: &Path) -> Option<PathBuf> {
    let versions = std::fs::read_dir(root.join(".oj-cache")).ok()?;
    versions
        .flatten()
        .map(|e| e.path().join("start"))
        .find(|p| p.join("client-chunks.json").is_file())
}

/// The text of every chunk the dev bundle wrote last, and the chunk index.
fn written_bundle(root: &Path) -> String {
    let Some(cache) = start_cache(root) else {
        return String::new();
    };
    let mut text = std::fs::read_to_string(cache.join("client-chunks.json")).unwrap_or_default();
    for entry in std::fs::read_dir(cache.join("client-chunks"))
        .into_iter()
        .flatten()
        .flatten()
    {
        text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    text
}

/// Polls `server`'s page `path` until its server render satisfies `done`.
fn server_render(server: &Server, path: &str, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = server.request("GET", path, &[]);
        if status == 200 && done(&body) {
            return body;
        }
        if Instant::now() >= deadline {
            return body;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Renames the detail route out of the members layout: `members.$id.tsx`
/// becomes `members_.$id.tsx` (the same `/members/$id` path, no longer
/// nested), then waits for the generator's tree and for the written bundle
/// to carry the renamed file and nothing of the old one.
fn rename_member_route(root: &Path, dev: &Server) {
    std::fs::rename(
        root.join("src/routes/members.$id.tsx"),
        root.join("src/routes/members_.$id.tsx"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let tree = root.join("src/routeTree.gen.ts");
    loop {
        let tree_text = std::fs::read_to_string(&tree).unwrap_or_default();
        let bundle = written_bundle(root);
        if tree_text.contains("members_.$id")
            && bundle.contains("members_.$id.tsx")
            && !bundle.contains("members.$id.tsx")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the bundle never moved to the renamed route\n--- tree\n{tree_text}\n--- output\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// After the rename, the server renders the detail route without the
/// members layout, the bundle has no chunk of the old file, and a later
/// edit bundles without the bundler reporting the deleted file.
#[test]
fn a_renamed_route_file_leaves_the_bundle_and_renders_at_its_path() {
    let Some(app) = members_app("route-rename") else {
        return;
    };
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let body = server_render(&dev, "/members/7", |b| b.contains("member-7"));
    assert!(
        body.contains("members page") && body.contains("member-7"),
        "the nested detail route: {body}\n{}",
        dev.output()
    );
    let before = dev.output().len();
    rename_member_route(&root, &dev);
    let body = server_render(&dev, "/members/7", |b| {
        b.contains("member-7") && !b.contains("members page")
    });
    assert!(
        body.contains("member-7") && !body.contains("members page"),
        "the renamed detail route: {body}\n{}",
        dev.output()
    );
    // A later edit: bundled against the renamed route, not the deleted file.
    let index = root.join("src/routes/index.tsx");
    std::fs::write(&index, INDEX.replace("home page", "home page edited")).unwrap();
    let body = server_render(&dev, "/", |b| b.contains("home page edited"));
    assert!(
        body.contains("home page edited"),
        "{body}\n{}",
        dev.output()
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while !written_bundle(&root).contains("home page edited") {
        assert!(
            Instant::now() < deadline,
            "the edit never reached the bundle\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let output = dev.output();
    let since = &output[before.min(output.len())..];
    assert!(
        !since.contains("Could not load") && !since.contains("client bundle failed"),
        "the bundler kept the deleted file:\n{since}"
    );
}

/// An open page follows the rename: a soft navigation to the detail path
/// renders the renamed route (no members layout around it), not the old
/// tree's nested route or its layout alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_follows_a_renamed_route() {
    let Some(app) = members_app("route-rename-page") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        r##"self.__ojHydrated === true && document.querySelector("#home")?.textContent === "home page""##,
        WAIT,
    )
    .await;
    let before = dev.output().len();
    rename_member_route(&root, &dev);
    // The page navigates to the detail path (again after a reload, whose
    // document starts at `/`), and ends up on the renamed route.
    const AT_MEMBER: &str = r##"(() => {
      if (location.pathname !== "/members/7") {
        if (self.__ojHydrated === true && self.__ojNavigate) self.__ojNavigate("/members/7");
        return false;
      }
      return document.querySelector("#member")?.textContent === "member-7" && !document.querySelector("#members");
    })()"##;
    let rendered = page.poll(AT_MEMBER, Duration::from_secs(30)).await;
    let shown: String = page
        .eval("location.pathname + ' ' + (document.querySelector('main, p')?.outerHTML ?? '')")
        .await;
    let errors = page.errors();
    let console = page.console();
    page.close().await;
    let output = dev.output();
    let since = &output[before.min(output.len())..];
    assert!(
        rendered,
        "the open page did not reach the renamed route: {shown}\nerrors: {errors:?}\nconsole: {console:?}\n--- output since the rename\n{since}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
    // A fresh load of the path hydrates the server's render of the renamed
    // route: the same tree on both sides, so no members layout to reconcile.
    let fresh = Page::open(&dev.url("/members/7")).await;
    let hydrated = fresh
        .poll(
            r##"self.__ojHydrated === true && document.querySelector("#member")?.textContent === "member-7" && !document.querySelector("#members")"##,
            Duration::from_secs(30),
        )
        .await;
    let errors = fresh.errors();
    let console = fresh.console();
    fresh.close().await;
    assert!(
        hydrated,
        "the fresh load kept the members layout: errors: {errors:?}\nconsole: {console:?}\n{}",
        dev.output()
    );
    assert_eq!(errors, Vec::<String>::new(), "the fresh load's page errors");
    assert!(
        !console.iter().any(|line| line.contains("hydrat")),
        "the fresh load reconciled a server render from the old tree: {console:?}"
    );
}
