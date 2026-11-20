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
//!
//! Neither does a name read as a test, a story or a declaration keep a file
//! out of the tree: router-generator's crawl (`getRouteNodes`) takes every
//! `.ts`/`.tsx`/`.js`/`.jsx` file under the routes directory that
//! `routeFilePrefix` (unset), `routeFileIgnorePrefix` (`-` by default) and
//! `routeFileIgnorePattern` (unset) let through, and drops one that exports
//! no `Route` with a warning. So
//! `admin.stories.tsx` and `x.test.tsx` exporting a `Route` are routes under
//! Vite, on the server and in the client, and an app that wants them out
//! says so in `tanstackStart({ router })` or tsr.config.json.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use browser::Page;
use start_app::{app, build, Server};

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

/// A dot-form route named like a story, a test or a spec.
fn named_route(path: &str, marker: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";

export const Route = createFileRoute("{path}")({{
  component: () => <p id="named">{marker}</p>,
}});
"#
    )
}

/// The client bundle the dev server serves: its entry and every chunk the
/// entry names, concatenated.
fn client_bundle(dev: &Server) -> String {
    let (status, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let mut text = entry.clone();
    for chunk in entry
        .split(['"', '\''])
        .filter_map(|s| s.strip_prefix("./"))
        .filter(|s| s.ends_with(".js"))
    {
        let (_, code) = dev.request("GET", &format!("/@oj-start/{chunk}"), &[]);
        text.push_str(&code);
    }
    text
}

/// The route ids the client's route tree configures (`id: '/admin/stories'`).
fn client_route_ids(bundle: &str) -> Vec<String> {
    bundle
        .split("id:")
        .skip(1)
        .filter_map(|rest| {
            let rest = rest.trim_start();
            let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
            let rest = &rest[1..];
            Some(rest[..rest.find(quote)?].to_string())
        })
        .collect()
}

/// `(file, path, id, marker)`: route files whose names a test runner or a
/// story tool would claim, each exporting a `Route`.
const NAMED: &[(&str, &str, &str, &str)] = &[
    (
        "admin.stories.tsx",
        "/admin/stories",
        "/admin/stories",
        "admin-stories-page",
    ),
    (
        "en_.stories.tsx",
        "/en/stories",
        "/en_/stories",
        "en-stories-page",
    ),
    ("x.test.tsx", "/x/test", "/x/test", "x-test-page"),
    ("x.spec.tsx", "/x/spec", "/x/spec", "x-spec-page"),
];

#[tokio::test(flavor = "multi_thread")]
async fn route_files_named_like_stories_or_tests_are_routes_on_the_server_and_in_the_client() {
    let named: Vec<(&str, String)> = NAMED
        .iter()
        .map(|(file, _, id, marker)| (*file, named_route(id, marker)))
        .collect();
    let mut routes: Vec<(&str, &str)> = vec![("__root.tsx", ROOT), ("index.tsx", INDEX)];
    routes.extend(named.iter().map(|(file, code)| (*file, code.as_str())));
    // Under the routes directory but no route: the generator skips them.
    routes.push(("helpers.test.tsx", "export const helper = 1;\n"));
    routes.push(("types.d.ts", "export interface Member { id: string }\n"));
    let Some(app) = file_routed_app("named-like-tests", &routes) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);

    for (_, path, _, marker) in NAMED {
        let (status, html) = dev.request("GET", path, &[]);
        assert!(
            status == 200 && html.contains(marker),
            "GET {path}: {status}, no {marker:?} in the server's render\n{html}\n{}",
            dev.output()
        );
    }
    let ids = client_route_ids(&client_bundle(&dev));
    for (file, _, id, _) in NAMED {
        assert!(
            ids.iter().any(|i| i == id),
            "the client's route tree has no {id} ({file}): {ids:?}\n{}",
            dev.output()
        );
    }
    assert!(
        dev.logs("2 file(s) under src/routes export no Route, skipped"),
        "the files that export no Route were not reported as skipped\n{}",
        dev.output()
    );
    for file in ["helpers.test.tsx", "types.d.ts"] {
        let code = std::fs::read_to_string(app.0.join("src/routes").join(file)).unwrap();
        assert!(
            !code.contains("createFileRoute"),
            "the generator scaffolded {file}: {code}"
        );
    }

    if browser::skip() {
        return;
    }
    // A hard visit hydrates on the route, not on the root's not-found.
    let page = Page::open(&dev.url("/admin/stories")).await;
    let hydrated = page
        .poll(
            r#"document.getElementById("named")?.textContent === "admin-stories-page" && !!self.__TSR_ROUTER__ && self.__TSR_ROUTER__.state.matches.some((m) => m.routeId === "/admin/stories")"#,
            Duration::from_secs(60),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let errors = page.errors();
    let text = page.eval::<String>("document.body.innerText").await;
    page.close().await;
    assert!(
        hydrated,
        "/admin/stories did not hydrate on its route: {text:?} {errors:?}\n{}",
        start_lines(&dev)
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

/// `(name in the tree, path, marker)` of a route file the tests below keep
/// in or leave out.
type Named = (&'static str, &'static str, &'static str);
const STORIES: Named = ("admin.stories", "/admin/stories", "admin-stories-page");
const DRAFT: Named = ("about.draft", "/about/draft", "about-draft-page");
const HIDDEN: Named = ("~hidden", "/~hidden", "hidden-page");

/// The app's `tanstackStart()` given `router` (a JS object literal).
fn set_plugin_router(root: &Path, router: &str) {
    let config_file = root.join("vite.config.ts");
    let config = std::fs::read_to_string(&config_file).unwrap();
    let from = r#"tanstackStart({ server: { entry: "ssr-entry" } })"#;
    assert!(config.contains(from), "{config}");
    let to = format!(r#"tanstackStart({{ server: {{ entry: "ssr-entry" }}, router: {router} }})"#);
    std::fs::write(&config_file, config.replacen(from, &to, 1)).unwrap();
}

/// `kept` renders on the server and is in the generated and the client's
/// route trees; `left` is in neither tree and does not render.
fn assert_kept_and_left(dev: &Server, root: &Path, how: &str, kept: Named, left: Named) {
    let (status, html) = dev.request("GET", kept.1, &[]);
    assert!(
        status == 200 && html.contains(kept.2),
        "{how}: GET {}: {status}\n{html}\n{}",
        kept.1,
        dev.output()
    );
    let (_, html) = dev.request("GET", left.1, &[]);
    assert!(
        !html.contains(left.2),
        "{how}: the ignored route {} rendered on the server\n{html}\n{}",
        left.1,
        dev.output()
    );
    let tree = std::fs::read_to_string(root.join("src/routeTree.gen.ts")).unwrap();
    assert!(
        !tree.contains(left.0) && tree.contains(kept.0),
        "{how}: the generated tree\n{tree}"
    );
    let ids = client_route_ids(&client_bundle(dev));
    assert!(
        !ids.iter().any(|id| id == left.1) && ids.iter().any(|id| id == kept.1),
        "{how}: the client's route tree: {ids:?}\n{}",
        dev.output()
    );
}

/// The way out of the tree is the app's own: a `routeFileIgnorePattern` in
/// `tanstackStart({ router })` or in tsr.config.json leaves the files it
/// matches out on the server and in the client, as under Vite. With both,
/// the plugin's wins (router-generator's `getConfig` spreads Start's inline
/// options over tsr.config.json).
#[test]
fn a_route_file_ignore_pattern_the_app_configures_leaves_its_files_out() {
    let drafts = named_route(DRAFT.1, DRAFT.2);
    let stories = named_route(STORIES.1, STORIES.2);
    let draft = serde_json::to_string(r"\.draft\.").unwrap();
    for (name, plugin, tsr) in [
        ("ignore-in-plugin", true, None),
        ("ignore-in-tsr-config", false, Some(r"\.draft\.")),
        ("ignore-in-both", true, Some(r"\.stories\.")),
    ] {
        let Some(app) = file_routed_app(
            name,
            &[
                ("__root.tsx", ROOT),
                ("index.tsx", INDEX),
                ("about.draft.tsx", &drafts),
                ("admin.stories.tsx", &stories),
            ],
        ) else {
            return;
        };
        if plugin {
            set_plugin_router(&app.0, &format!("{{ routeFileIgnorePattern: {draft} }}"));
        }
        if let Some(pattern) = tsr {
            std::fs::write(
                app.0.join("tsr.config.json"),
                serde_json::json!({ "routeFileIgnorePattern": pattern }).to_string(),
            )
            .unwrap();
        }
        let dev = Server::oj_dev(&app.0);
        assert_kept_and_left(&dev, &app.0, name, STORIES, DRAFT);
    }
}

/// A `routeFileIgnorePrefix` in `tanstackStart({ router })` leaves the files
/// it prefixes out, as under Vite.
#[test]
fn a_route_file_ignore_prefix_the_app_configures_leaves_its_files_out() {
    let hidden = named_route(HIDDEN.1, HIDDEN.2);
    let stories = named_route(STORIES.1, STORIES.2);
    let Some(app) = file_routed_app(
        "ignore-prefix-in-plugin",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("~hidden.tsx", &hidden),
            ("admin.stories.tsx", &stories),
        ],
    ) else {
        return;
    };
    set_plugin_router(&app.0, r#"{ routeFileIgnorePrefix: "~" }"#);
    let dev = Server::oj_dev(&app.0);
    assert_kept_and_left(&dev, &app.0, "routeFileIgnorePrefix", STORIES, HIDDEN);
}

/// `oj build` generates the same tree: a route file named like a story is
/// in the written `routeTree.gen.ts` and the built client, and the built
/// server renders it.
#[test]
fn a_route_file_named_like_a_story_is_a_route_in_the_build() {
    let stories = named_route(STORIES.1, STORIES.2);
    let Some(app) = file_routed_app(
        "named-like-tests-build",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("admin.stories.tsx", &stories),
        ],
    ) else {
        return;
    };
    let root = &app.0;
    build(root);
    let tree = std::fs::read_to_string(root.join("src/routeTree.gen.ts")).unwrap();
    assert!(
        tree.contains("./routes/admin.stories") && tree.contains("/admin/stories"),
        "the generated tree\n{tree}"
    );
    assert!(
        holds(&root.join("dist/client"), STORIES.2),
        "the built client lacks the route's component"
    );
    let served = Server::node_server(root);
    let (status, html) = served.request("GET", STORIES.1, &[]);
    assert!(
        status == 200 && html.contains(STORIES.2),
        "the built server: GET {}: {status}\n{html}\n{}",
        STORIES.1,
        served.output()
    );
}

/// Some file under `dir` contains `needle`.
fn holds(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let path = e.path();
            if path.is_dir() {
                holds(&path, needle)
            } else {
                std::fs::read_to_string(&path).is_ok_and(|s| s.contains(needle))
            }
        })
}

// The router generator's other options, set in `tanstackStart({ router })`:
// Start passes the generator options it resolved (`parseStartConfig`, its
// schema's defaults included) inline over tsr.config.json, and oj's route tree
// generation passes the same ones, so the tree oj writes is the one Vite
// writes.

/// A layout route rendering `marker` above its `Outlet`.
fn layout_route(path: &str, marker: &str) -> String {
    format!(
        r#"import {{ createFileRoute, Outlet }} from "@tanstack/react-router";

export const Route = createFileRoute("{path}")({{
  component: () => <div><p id="layout">{marker}</p><Outlet /></div>,
}});
"#
    )
}

/// Each `(path, marker)` renders on the server.
fn assert_renders(dev: &Server, how: &str, pages: &[(&str, &str)]) {
    for (path, marker) in pages {
        let (status, html) = dev.request("GET", path, &[]);
        assert!(
            status == 200 && html.contains(marker),
            "{how}: GET {path}: {status}, no {marker:?} in the server's render\n{html}\n{}",
            dev.output()
        );
    }
}

/// oj generated the tree, and the client's route tree configures `ids`.
fn assert_client_routes(dev: &Server, how: &str, ids: &[&str]) {
    assert!(
        !dev.logs("route tree generation failed"),
        "{how}: oj's route tree generation failed\n{}",
        dev.output()
    );
    let held = client_route_ids(&client_bundle(dev));
    for id in ids {
        assert!(
            held.iter().any(|i| i == id),
            "{how}: the client's route tree has no {id}: {held:?}\n{}",
            dev.output()
        );
    }
}

fn generated_tree(root: &Path) -> String {
    std::fs::read_to_string(root.join("src/routeTree.gen.ts")).unwrap()
}

/// Start's route tree footer for the file-routed app (route-tree-footer.js).
const START_FOOTER: &str = "import type { getRouter } from './router.tsx'
import type { createStart } from '@tanstack/react-start'
declare module '@tanstack/react-start' {
  interface Register {
    ssr: true
    router: Awaited<ReturnType<typeof getRouter>>
  }
}
";

/// Routes a virtual route config names, none of them in Start's file layout
/// (no `__root.tsx`, no `index.tsx`).
fn virtual_routes() -> Vec<(&'static str, String)> {
    vec![
        ("root.tsx", ROOT.to_string()),
        ("home.tsx", named_route("/", "home-page")),
        ("about-page.tsx", named_route("/about", "about-page")),
        ("contact-page.tsx", named_route("/contact", "contact-page")),
    ]
}

fn routed_app(name: &str, routes: &[(&str, String)]) -> Option<start_app::App> {
    let routes: Vec<(&str, &str)> = routes.iter().map(|(f, c)| (*f, c.as_str())).collect();
    file_routed_app(name, &routes)
}

/// The app's vite.config.ts importing the virtual route builders.
fn import_virtual_routes(root: &Path) {
    let config_file = root.join("vite.config.ts");
    let config = std::fs::read_to_string(&config_file).unwrap();
    std::fs::write(
        &config_file,
        format!("import {{ rootRoute, route, index }} from \"@tanstack/virtual-file-routes\";\n{config}"),
    )
    .unwrap();
}

const VIRTUAL_CONFIG: &str =
    r#"rootRoute("root.tsx", [index("home.tsx"), route("/about", "about-page.tsx")])"#;

#[test]
fn a_virtual_route_config_in_the_plugin_is_the_tree_on_the_server_and_in_the_client() {
    let Some(app) = routed_app("virtual-config-object", &virtual_routes()) else {
        return;
    };
    import_virtual_routes(&app.0);
    set_plugin_router(
        &app.0,
        &format!("{{ virtualRouteConfig: {VIRTUAL_CONFIG} }}"),
    );
    let dev = Server::oj_dev(&app.0);
    let how = "virtualRouteConfig object";
    assert_renders(&dev, how, &[("/", "home-page"), ("/about", "about-page")]);
    assert_client_routes(&dev, how, &["/", "/about"]);
}

/// A `virtualRouteConfig` module decides the tree: an edit to it is in the
/// tree the next server generates, the cache kept (it is one of the tree's
/// inputs), as Vite's next start generates it.
#[test]
fn a_virtual_route_config_file_is_the_tree_and_an_edit_to_it_is_not_restored_from_the_cache() {
    let Some(app) = routed_app("virtual-config-file", &virtual_routes()) else {
        return;
    };
    let root = &app.0;
    let module = |routes: &str| {
        std::fs::write(
            root.join("routes.ts"),
            format!("import {{ rootRoute, route, index }} from \"@tanstack/virtual-file-routes\";\nexport const routes = {routes};\n"),
        )
        .unwrap();
    };
    module(VIRTUAL_CONFIG);
    set_plugin_router(root, r#"{ virtualRouteConfig: "./routes.ts" }"#);
    let how = "virtualRouteConfig file";
    let port = start_app::free_port();
    {
        let dev = Server::oj_dev_on(root, port);
        assert_renders(&dev, how, &[("/", "home-page"), ("/about", "about-page")]);
        assert_client_routes(&dev, how, &["/", "/about"]);
    }
    module(&VIRTUAL_CONFIG.replace(
        r#"route("/about", "about-page.tsx")"#,
        r#"route("/about", "about-page.tsx"), route("/contact", "contact-page.tsx")"#,
    ));
    let dev = Server::oj_dev_on(root, port);
    let how = "virtualRouteConfig file edited, server restarted";
    assert_renders(&dev, how, &[("/contact", "contact-page")]);
    assert_client_routes(&dev, how, &["/contact"]);
}

/// The options Start resolved come from tsr.config.json too: an edit to it
/// is in the next server's tree, the extraction cache kept, as Vite's next
/// start parses it again.
#[test]
fn a_tsr_config_edit_reaches_the_tree_after_a_restart() {
    let drafts = named_route(DRAFT.1, DRAFT.2);
    let Some(app) = file_routed_app(
        "tsr-config-edit",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("about.draft.tsx", &drafts),
        ],
    ) else {
        return;
    };
    let root = &app.0;
    let tsr = |json: serde_json::Value| {
        std::fs::write(root.join("tsr.config.json"), json.to_string()).unwrap();
    };
    tsr(serde_json::json!({ "routeFileIgnorePattern": r"\.draft\." }));
    let port = start_app::free_port();
    {
        let dev = Server::oj_dev_on(root, port);
        let (status, _) = dev.request("GET", DRAFT.1, &[]);
        assert_eq!(status, 404, "the ignored route rendered\n{}", dev.output());
    }
    tsr(serde_json::json!({}));
    let dev = Server::oj_dev_on(root, port);
    let how = "tsr.config.json edited, server restarted";
    assert_renders(&dev, how, &[(DRAFT.1, DRAFT.2)]);
    assert_client_routes(&dev, how, &[DRAFT.1]);
}

/// `indexToken` and `routeToken` as a string, a RegExp, or the schema's
/// `{ regex }` form name the same routes.
#[test]
fn custom_index_and_route_tokens_name_the_index_and_layout_routes_in_every_form() {
    let routes = vec![
        ("__root.tsx", ROOT.to_string()),
        ("home.tsx", named_route("/", "home-page")),
        ("index.tsx", named_route("/index", "literal-index-page")),
        ("posts.layout.tsx", layout_route("/posts", "posts-layout")),
        ("posts.home.tsx", named_route("/posts/", "posts-index-page")),
    ];
    for (name, tokens) in [
        (
            "tokens-string",
            r#"indexToken: "home", routeToken: "layout""#,
        ),
        (
            "tokens-regexp",
            r#"indexToken: /hom(e)?/, routeToken: /lay(out)?/"#,
        ),
        (
            "tokens-json-regex",
            r#"indexToken: { regex: "hom(e)?" }, routeToken: { regex: "lay(out)?" }"#,
        ),
    ] {
        let Some(app) = routed_app(name, &routes) else {
            return;
        };
        set_plugin_router(&app.0, &format!("{{ {tokens} }}"));
        let dev = Server::oj_dev(&app.0);
        assert_renders(
            &dev,
            name,
            &[
                ("/", "home-page"),
                ("/posts", "posts-layout"),
                ("/posts", "posts-index-page"),
                ("/index", "literal-index-page"),
            ],
        );
        assert_client_routes(&dev, name, &["/", "/posts", "/index"]);
    }
}

fn plain_routes() -> Vec<(&'static str, String)> {
    vec![
        ("__root.tsx", ROOT.to_string()),
        ("index.tsx", named_route("/", "home-page")),
        ("about.tsx", named_route("/about", "about-page")),
    ]
}

/// Each option alone shapes its own part of the tree; `quoteStyle` and
/// `semicolons` act through the generator's formatting (on by default).
#[test]
fn quote_style_semicolons_and_extensions_in_the_plugin_each_shape_the_generated_tree() {
    for (name, option, has, lacks) in [
        (
            "tree-quote-style",
            r#"{ quoteStyle: "double" }"#,
            r#"  id: "/about","#,
            "} as any);",
        ),
        (
            "tree-semicolons",
            "{ semicolons: true }",
            "} as any);",
            r#"  id: "/about","#,
        ),
        (
            "tree-add-extensions",
            "{ addExtensions: true }",
            "./routes/about.tsx",
            "'./routes/about'",
        ),
    ] {
        let Some(app) = routed_app(name, &plain_routes()) else {
            return;
        };
        set_plugin_router(&app.0, option);
        let dev = Server::oj_dev(&app.0);
        assert_renders(&dev, name, &[("/about", "about-page")]);
        let tree = generated_tree(&app.0);
        assert!(
            tree.contains(has) && !tree.contains(lacks),
            "{name}: the generated tree should hold {has:?} and not {lacks:?}\n{tree}"
        );
    }
}

/// Start's footer is its declare-module block, then the app's own
/// `routeTreeFileFooter` (route-tree-footer.js).
#[test]
fn the_apps_route_tree_footer_follows_starts_declare_module_block() {
    let Some(app) = routed_app("tree-footer", &plain_routes()) else {
        return;
    };
    set_plugin_router(&app.0, r#"{ routeTreeFileFooter: ["// app footer"] }"#);
    let dev = Server::oj_dev(&app.0);
    assert_renders(&dev, "app footer", &[("/about", "about-page")]);
    let tree = generated_tree(&app.0);
    let end = format!("{START_FOOTER}\n// app footer\n");
    assert!(
        tree.ends_with(&end),
        "the tree does not end with\n{end}\n---\n{tree}"
    );
}

/// Start's schema fills its `router` options' defaults and passes them over
/// tsr.config.json, so a `quoteStyle` or footer there has no effect.
#[test]
fn tsr_config_options_start_gives_a_default_do_not_shape_the_tree() {
    let Some(app) = routed_app("tsr-config-overridden", &plain_routes()) else {
        return;
    };
    std::fs::write(
        app.0.join("tsr.config.json"),
        r#"{ "quoteStyle": "double", "semicolons": true, "routeTreeFileFooter": ["// tsr footer"] }"#,
    )
    .unwrap();
    let dev = Server::oj_dev(&app.0);
    assert_renders(&dev, "tsr.config.json", &[("/about", "about-page")]);
    let tree = generated_tree(&app.0);
    assert!(
        tree.contains("import { Route as AboutRouteImport } from './routes/about'\n")
            && tree.ends_with(START_FOOTER),
        "the tree took tsr.config.json's options\n{tree}"
    );
}

/// `disableTypes` writes `routeTree.gen.js` without types in its body. Start
/// still appends its TypeScript footer, which neither Vite nor oj can parse
/// in a `.js` module: the render fails under both.
#[test]
fn disable_types_writes_an_untyped_js_tree_as_start_does() {
    let Some(app) = routed_app("disable-types", &plain_routes()) else {
        return;
    };
    set_plugin_router(&app.0, "{ disableTypes: true }");
    let dev = Server::oj_dev(&app.0);
    let _ = dev.request("GET", "/about", &[]);
    let tree = std::fs::read_to_string(app.0.join("src/routeTree.gen.js")).unwrap_or_default();
    let body = tree.strip_suffix(START_FOOTER).unwrap_or_else(|| {
        panic!(
            "no routeTree.gen.js ending in Start's footer\n{tree}\n{}",
            dev.output()
        )
    });
    assert!(
        body.contains(
            "export const routeTree = rootRouteImport._addFileChildren(rootRouteChildren)\n"
        ) && !body.contains("as any")
            && !body.contains("export interface"),
        "the .js tree has types\n{tree}"
    );
}

#[test]
fn a_virtual_route_config_is_the_tree_in_the_build() {
    let Some(app) = routed_app("virtual-config-build", &virtual_routes()) else {
        return;
    };
    let root = &app.0;
    import_virtual_routes(root);
    set_plugin_router(root, &format!("{{ virtualRouteConfig: {VIRTUAL_CONFIG} }}"));
    build(root);
    assert!(
        holds(&root.join("dist/client"), "about-page"),
        "the built client lacks the route's component"
    );
    let served = Server::node_server(root);
    let (status, html) = served.request("GET", "/about", &[]);
    assert!(
        status == 200 && html.contains("about-page"),
        "the built server: GET /about: {status}\n{html}\n{}",
        served.output()
    );
}

/// `routeTreeFileHeader` opens the tree, and `enableRouteTreeFormatting:
/// false` leaves the generator's raw text unformatted.
#[test]
fn the_apps_route_tree_header_opens_an_unformatted_tree() {
    let Some(app) = routed_app("tree-header-unformatted", &plain_routes()) else {
        return;
    };
    set_plugin_router(
        &app.0,
        r#"{ routeTreeFileHeader: ["// app header"], enableRouteTreeFormatting: false }"#,
    );
    let dev = Server::oj_dev(&app.0);
    assert_renders(&dev, "header, unformatted", &[("/about", "about-page")]);
    let tree = generated_tree(&app.0);
    assert!(
        tree.starts_with("// app header\n"),
        "the tree does not start with the app's header\n{tree}"
    );
    assert!(tree.contains("}as any)"), "the tree was formatted\n{tree}");
}

/// `importRoutesUsingAbsolutePaths` imports each route by its absolute path,
/// and the route still renders on the server and hydrates in the client.
#[tokio::test(flavor = "multi_thread")]
async fn routes_imported_by_absolute_path_render_and_hydrate() {
    let Some(app) = routed_app("tree-absolute-imports", &plain_routes()) else {
        return;
    };
    set_plugin_router(&app.0, "{ importRoutesUsingAbsolutePaths: true }");
    let dev = Server::oj_dev(&app.0);
    let how = "importRoutesUsingAbsolutePaths";
    assert_renders(&dev, how, &[("/", "home-page"), ("/about", "about-page")]);
    let tree = generated_tree(&app.0);
    let import = format!("from '{}'", app.0.join("src/routes/about").display());
    assert!(tree.contains(&import), "the tree lacks {import:?}\n{tree}");
    assert_client_routes(&dev, how, &["/", "/about"]);

    if browser::skip() {
        return;
    }
    let page = Page::open(&dev.url("/about")).await;
    let hydrated = page
        .poll(
            r#"document.getElementById("named")?.textContent === "about-page" && !!self.__TSR_ROUTER__ && self.__TSR_ROUTER__.state.matches.some((m) => m.routeId === "/about")"#,
            Duration::from_secs(60),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let errors = page.errors();
    page.close().await;
    assert!(
        hydrated,
        "/about did not hydrate on its route: {errors:?}\n{}",
        start_lines(&dev)
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

/// With `routeFilePrefix`, only the prefixed files are routes: an unprefixed
/// file exporting a `Route` is out of the tree and answers 404.
#[test]
fn a_route_file_prefix_makes_only_prefixed_files_routes() {
    let routes = vec![
        ("r-__root.tsx", ROOT.to_string()),
        ("r-index.tsx", named_route("/", "home-page")),
        ("r-about.tsx", named_route("/about", "about-page")),
        ("contact.tsx", named_route("/contact", "contact-page")),
    ];
    let Some(app) = routed_app("tree-route-file-prefix", &routes) else {
        return;
    };
    set_plugin_router(&app.0, r#"{ routeFilePrefix: "r-" }"#);
    let dev = Server::oj_dev(&app.0);
    let how = "routeFilePrefix";
    assert_renders(&dev, how, &[("/", "home-page"), ("/about", "about-page")]);
    let (status, html) = dev.request("GET", "/contact", &[]);
    assert!(
        status == 404 && !html.contains("contact-page"),
        "{how}: GET /contact: {status}\n{html}\n{}",
        dev.output()
    );
    let tree = generated_tree(&app.0);
    assert!(
        tree.contains("./routes/r-about") && !tree.contains("contact"),
        "{how}: the generated tree\n{tree}"
    );
    let ids = client_route_ids(&client_bundle(&dev));
    assert!(
        !ids.iter().any(|id| id == "/contact"),
        "{how}: the client's route tree: {ids:?}\n{}",
        dev.output()
    );
    assert_client_routes(&dev, how, &["/", "/about"]);
}

/// `f` holds within the start-up wait, checked every 100ms.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + start_app::WAIT;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// An empty route file added while dev runs is scaffolded from the app's
/// `customScaffolding` template, and the generator works in the app's
/// `tmpDir`, not in `.tanstack`.
#[test]
fn a_route_file_added_during_dev_is_scaffolded_from_the_apps_template_in_its_tmp_dir() {
    let Some(app) = routed_app("tree-scaffolding", &plain_routes()) else {
        return;
    };
    let root = &app.0;
    set_plugin_router(
        root,
        r#"{ customScaffolding: { routeTemplate: "%%tsrImports%%\n\n// app scaffold\n%%tsrExportStart%%{ component: () => <p>scaffolded-page</p> }%%tsrExportEnd%%\n" }, tmpDir: "./.tree-tmp" }"#,
    );
    let dev = Server::oj_dev(root);
    assert_renders(&dev, "customScaffolding", &[("/about", "about-page")]);
    let fresh = root.join("src/routes/fresh.tsx");
    std::fs::write(&fresh, "").unwrap();
    let scaffolded = eventually(|| {
        std::fs::read_to_string(&fresh).is_ok_and(|code| {
            code.contains("// app scaffold") && code.contains("createFileRoute('/fresh')")
        })
    });
    assert!(
        scaffolded,
        "fresh.tsx was not scaffolded from the app's template: {:?}\n{}",
        std::fs::read_to_string(&fresh),
        dev.output()
    );
    let rendered = eventually(|| {
        let (status, html) = dev.request("GET", "/fresh", &[]);
        status == 200 && html.contains("scaffolded-page")
    });
    assert!(rendered, "/fresh never rendered\n{}", dev.output());
    assert!(
        root.join(".tree-tmp").is_dir(),
        "the generator's tmpDir was not created\n{}",
        dev.output()
    );
    assert!(
        !root.join(".tanstack").exists(),
        "the generator wrote .tanstack despite tmpDir\n{}",
        dev.output()
    );
}

/// A `__virtual.ts` subtree config importing a module outside the routes
/// folder: an edit to that module is in the tree after a restart with
/// `.oj-cache` kept.
#[test]
fn a_virtual_subtree_importing_a_module_outside_the_routes_is_not_restored_from_the_cache() {
    let mut routes = plain_routes();
    routes.push((
        "blog/__virtual.ts",
        "import { blogRoutes } from \"../../blog-routes\";\nexport default blogRoutes;\n"
            .to_string(),
    ));
    routes.push(("blog/home.tsx", named_route("/blog/", "blog-home-page")));
    routes.push(("blog/post.tsx", named_route("/blog/post", "blog-post-page")));
    let Some(app) = file_routed_app("virtual-subtree-import", &[]) else {
        return;
    };
    let root = &app.0;
    for (file, code) in &routes {
        let path = root.join("src/routes").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, code).unwrap();
    }
    let module = |routes: &str| {
        std::fs::write(
            root.join("src/blog-routes.ts"),
            format!("import {{ index, route }} from \"@tanstack/virtual-file-routes\";\nexport const blogRoutes = [{routes}];\n"),
        )
        .unwrap();
    };
    module(r#"index("home.tsx")"#);
    let port = start_app::free_port();
    {
        let dev = Server::oj_dev_on(root, port);
        let how = "__virtual.ts subtree";
        assert_renders(&dev, how, &[("/blog", "blog-home-page")]);
        let (status, _) = dev.request("GET", "/blog/post", &[]);
        assert_eq!(
            status,
            404,
            "{how}: /blog/post is not in the subtree yet\n{}",
            dev.output()
        );
    }
    module(r#"index("home.tsx"), route("/post", "post.tsx")"#);
    let dev = Server::oj_dev_on(root, port);
    let how = "the subtree's module edited, server restarted";
    assert_renders(&dev, how, &[("/blog/post", "blog-post-page")]);
    assert_client_routes(&dev, how, &["/blog/post"]);
}

/// A tsr.config.json edit while dev runs reaches the tree: oj restarts on
/// it, where Vite applies it at the next start.
#[test]
fn a_tsr_config_edit_reaches_the_tree_without_a_manual_restart() {
    let drafts = named_route(DRAFT.1, DRAFT.2);
    let Some(app) = file_routed_app(
        "tsr-config-live-edit",
        &[
            ("__root.tsx", ROOT),
            ("index.tsx", INDEX),
            ("about.draft.tsx", &drafts),
        ],
    ) else {
        return;
    };
    let root = &app.0;
    let tsr = |json: serde_json::Value| {
        std::fs::write(root.join("tsr.config.json"), json.to_string()).unwrap();
    };
    tsr(serde_json::json!({ "routeFileIgnorePattern": r"\.draft\." }));
    let dev = Server::oj_dev(root);
    let (status, _) = dev.request("GET", DRAFT.1, &[]);
    assert_eq!(status, 404, "the ignored route rendered\n{}", dev.output());
    tsr(serde_json::json!({}));
    let rendered = eventually(|| {
        let (status, html) = dev.request("GET", DRAFT.1, &[]);
        status == 200 && html.contains(DRAFT.2)
    });
    assert!(
        rendered,
        "{} never rendered after the tsr.config.json edit\n{}",
        DRAFT.1,
        dev.output()
    );
    assert!(
        generated_tree(root).contains("about.draft"),
        "the generated tree lacks the route\n{}",
        generated_tree(root)
    );
    assert_client_routes(&dev, "tsr.config.json edited during dev", &[DRAFT.1]);
}
