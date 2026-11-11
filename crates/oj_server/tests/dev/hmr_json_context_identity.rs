// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `hmr_importer_stamps` with a JSON file under the context module and a
//! lazily loaded route (`React.lazy(() => import("./pages/X"))`) among the
//! hook's consumers. A JSON file is a JavaScript module to Vite
//! (plugins/json.ts: `export default {...}`), so its edit is a plain
//! js-update: `updateModules` stamps the JSON module and every importer
//! `invalidateModule` reaches with one `lastHMRTimestamp`, and each
//! importer's next transform names the context module under that stamp
//! (transformRequest.ts `handleModuleSoftInvalidation`). A lazy route the
//! graph knows (served once) is such an importer; one served for the first
//! time after the edit bakes the stamp of now. Either way one url, one
//! instance, and the hook finds the provider the root rendered. The edits
//! land with no page open, as the editor's flush delivers them.

use std::time::Duration;

use crate::browser::Page;
use crate::hmr_importer_stamps::{stamp_of, stamped_after};
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

const MAIN: &str = "import { createRoot } from \"react-dom/client\";\n\
    import { Provider } from \"./LanguageContext\";\nimport { App } from \"./App\";\n\
    createRoot(document.getElementById(\"root\")!).render(<Provider><App /></Provider>);\n";

/// The lazy route renders only when the page asks for it (`#lazy`), so a
/// test chooses whether its module is served before or after an edit.
const APP: &str = "import { lazy, Suspense } from \"react\";\n\
    import { Index } from \"./Index\";\n\
    const Lazy = lazy(() => import(\"./pages/Lazy\"));\n\
    export function App() {\n\
    \x20 const lazyRoute = window.location.hash === \"#lazy\";\n\
    \x20 return <div><Index />{lazyRoute ? <Suspense fallback={null}><Lazy /></Suspense> : null}</div>;\n}\n";

/// The context, its provider and the hook that reads it, in one module
/// importing JSON files (translation tables).
const CONTEXT: &str = "import { createContext, useContext, type ReactNode } from \"react\";\n\
    import en from \"./i18n/en.json\";\nimport es from \"./i18n/es.json\";\n\
    export const Ctx = createContext<string | null>(null);\n\
    export const fallback = es.greeting;\n\
    export function Provider({ children }: { children: ReactNode }) {\n\
    \x20 return <Ctx.Provider value={en.greeting}>{children}</Ctx.Provider>;\n}\n\
    export function useLanguage() {\n  const v = useContext(Ctx);\n\
    \x20 if (v === null) throw new Error(\"useLanguage must be used within Provider\");\n\
    \x20 return v;\n}\n";

const INDEX_PAGE: &str = "import { useLanguage } from \"./LanguageContext\";\n\
    export function Index() {\n  return <p id=\"index\">{useLanguage()}</p>;\n}\n";

const LAZY_PAGE: &str = "import { useLanguage } from \"../LanguageContext\";\n\
    export default function Lazy() {\n  return <p id=\"lazy\">{useLanguage()}</p>;\n}\n";

fn en(greeting: &str) -> String {
    format!("{{ \"greeting\": \"{greeting}\" }}\n")
}

const CONTEXT_URL: &str = "/src/LanguageContext.tsx";
const JSON_URL: &str = "/src/i18n/en.json";
const JSON_ES_URL: &str = "/src/i18n/es.json";
const LAZY_URL: &str = "/src/pages/Lazy.tsx";

/// The importers of the context module: the root's, the index page's and
/// the lazy route's.
const IMPORTERS_OF_CONTEXT: [&str; 3] = ["/src/main.tsx", "/src/Index.tsx", LAZY_URL];

/// What a page load at `/` serves: everything but the lazy route.
const ROOT_PAGE: [&str; 6] = [
    "/src/main.tsx",
    "/src/App.tsx",
    "/src/Index.tsx",
    CONTEXT_URL,
    JSON_URL,
    JSON_ES_URL,
];

fn app() -> App {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-json-context","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.tsx", MAIN),
        ("src/App.tsx", APP),
        ("src/LanguageContext.tsx", CONTEXT),
        ("src/i18n/en.json", &en("hello")),
        ("src/i18n/es.json", &en("hola")),
        ("src/Index.tsx", INDEX_PAGE),
        ("src/pages/Lazy.tsx", LAZY_PAGE),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    app
}

/// The stamp each importer of the context module names it under.
async fn context_stamps(server: &crate::Server) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for url in IMPORTERS_OF_CONTEXT {
        out.push((url, stamp_of(&server.text(url).await, CONTEXT_URL)));
    }
    out
}

fn assert_all_named_under(stamps: &[(&str, String)], stamp: &str, when: &str) {
    for (url, s) in stamps {
        assert_eq!(
            s, stamp,
            "{url} names {CONTEXT_URL} under {s:?} {when}, the module carries {stamp:?}: {stamps:?}"
        );
    }
}

/// Edits the JSON file and waits for the context module to name it under a
/// stamp other than `not`.
async fn edit_json(app: &App, server: &crate::Server, greeting: &str, not: &str) -> String {
    app.write(&[("src/i18n/en.json", &en(greeting))]);
    stamped_after(server, CONTEXT_URL, JSON_URL, not).await
}

#[tokio::test(flavor = "multi_thread")]
async fn every_importer_names_the_context_under_one_url_after_json_edits() {
    require_fixture!("react", "react-dom", "scheduler");
    let app = app();
    let server = app.serve().await;
    // A page load at `/`, then the lazy route: every module served once.
    for url in ROOT_PAGE.iter().chain([&LAZY_URL]) {
        server.text(url).await;
    }
    assert!(
        context_stamps(&server)
            .await
            .iter()
            .all(|(_, s)| s.is_empty()),
        "nothing is stamped before an edit"
    );

    let first = edit_json(&app, &server, "edited", "").await;
    assert_all_named_under(&context_stamps(&server).await, &first, "after a JSON edit");

    // The second edit lands on a later mtime, and after the first settled.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = edit_json(&app, &server, "again", &first).await;
    assert_ne!(second, first, "a second edit moves the stamp");
    assert_all_named_under(
        &context_stamps(&server).await,
        &second,
        "after a second JSON edit",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_json_files_edited_in_one_batch_leave_every_importer_on_one_url() {
    require_fixture!("react", "react-dom", "scheduler");
    let app = app();
    let server = app.serve().await;
    for url in ROOT_PAGE.iter().chain([&LAZY_URL]) {
        server.text(url).await;
    }

    // Two files of one save (a translations folder regenerated): one stamp
    // per changed file, as Vite takes one `monotonicDateNow()` per event,
    // and the context module's importers must all settle on the newest.
    app.write(&[
        ("src/i18n/en.json", &en("edited")),
        ("src/i18n/es.json", &en("editado")),
    ]);
    let en_stamp = stamped_after(&server, CONTEXT_URL, JSON_URL, "").await;
    let es_stamp = stamped_after(&server, CONTEXT_URL, JSON_ES_URL, "").await;
    let newest = std::cmp::max(&en_stamp, &es_stamp);
    assert_all_named_under(
        &context_stamps(&server).await,
        newest,
        "after two JSON files changed in one batch",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_first_served_after_a_json_edit_names_the_context_under_its_stamp() {
    require_fixture!("react", "react-dom", "scheduler");
    let app = app();
    let server = app.serve().await;
    // A page load at `/`: the lazy route is never requested before the edits.
    for url in ROOT_PAGE {
        server.text(url).await;
    }

    let first = edit_json(&app, &server, "edited", "").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = edit_json(&app, &server, "again", &first).await;

    // The route's first compile, after both edits, must name the context
    // module as the root's importer does now.
    assert_all_named_under(
        &context_stamps(&server).await,
        &second,
        "when the lazy route is first served after two JSON edits",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_page_loads_the_lazy_route_with_the_provider_after_json_edits() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let base = format!("http://127.0.0.1:{}/", listening.addr.port());
    let lazy = format!("{base}#lazy");

    // The root route, then the lazy route, both rendered once.
    let page = Page::open(&base).await;
    page.wait_for(
        r##"document.querySelector("#index")?.textContent === "hello""##,
        WAIT,
    )
    .await;
    page.close().await;
    let page = Page::open(&lazy).await;
    page.wait_for(
        r##"document.querySelector("#index")?.textContent === "hello" && document.querySelector("#lazy")?.textContent === "hello""##,
        WAIT,
    )
    .await;
    assert_eq!(
        page.errors(),
        Vec::<String>::new(),
        "first lazy page errors"
    );
    page.close().await;

    // Two JSON edits with no page open.
    let first = edit_json(&app, &server, "edited", "").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    edit_json(&app, &server, "again", &first).await;

    // A fresh load of the lazy route: the hook reads the provider the root
    // rendered, and the root route alone renders too.
    for url in [&lazy, &base, &lazy] {
        let fresh = Page::open(url).await;
        let want_lazy = url.ends_with("#lazy");
        let rendered = fresh
            .poll(
                &format!(
                    r##"document.querySelector("#index")?.textContent === "again" && ({} || document.querySelector("#lazy")?.textContent === "again")"##,
                    if want_lazy { "false" } else { "true" }
                ),
                WAIT,
            )
            .await;
        assert!(
            rendered,
            "a fresh page at {url} did not render; errors: {:?}",
            fresh.errors()
        );
        assert_eq!(
            fresh.errors(),
            Vec::<String>::new(),
            "fresh page errors at {url}"
        );
        fresh.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_lazy_route_follows_json_edits_and_a_fresh_page_still_renders_it() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let lazy = format!("http://127.0.0.1:{}/#lazy", listening.addr.port());

    let page = Page::open(&lazy).await;
    page.wait_for(
        r##"document.querySelector("#index")?.textContent === "hello" && document.querySelector("#lazy")?.textContent === "hello""##,
        WAIT,
    )
    .await;

    // The edits land with the page open: the hook export keeps the context
    // module from being a Fast Refresh boundary, so the update climbs to the
    // entry and the page reloads, as under Vite; either way it shows the
    // edit with no error.
    let first = edit_json(&app, &server, "edited", "").await;
    let shown = page
        .poll(
            r##"document.querySelector("#index")?.textContent === "edited" && document.querySelector("#lazy")?.textContent === "edited""##,
            WAIT,
        )
        .await;
    assert!(
        shown,
        "the open page did not show the first edit; errors: {:?}",
        page.errors()
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    edit_json(&app, &server, "again", &first).await;
    let shown = page
        .poll(
            r##"document.querySelector("#index")?.textContent === "again" && document.querySelector("#lazy")?.textContent === "again""##,
            WAIT,
        )
        .await;
    assert!(
        shown,
        "the open page did not show the second edit; errors: {:?}",
        page.errors()
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "open page errors");
    page.close().await;

    let fresh = Page::open(&lazy).await;
    let rendered = fresh
        .poll(
            r##"document.querySelector("#index")?.textContent === "again" && document.querySelector("#lazy")?.textContent === "again""##,
            WAIT,
        )
        .await;
    assert!(
        rendered,
        "a fresh lazy page did not render after edits with a page open; errors: {:?}",
        fresh.errors()
    );
    assert_eq!(fresh.errors(), Vec::<String>::new(), "fresh page errors");
    fresh.close().await;
}
