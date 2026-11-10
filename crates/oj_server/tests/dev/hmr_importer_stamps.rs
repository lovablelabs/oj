// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module two or more modules import (a React context's provider and hook
//! in one file, a refresh boundary of its own) gets an HMR stamp when one of
//! its own imports is edited. From then on every importer must name it under
//! that stamp, however each importer was last served: Vite's
//! `moduleGraph.invalidateModule` walks the importers of a stamped module
//! past self-accepting ones (only `acceptedHmrDeps` stops it) and soft-
//! invalidates each, so its next transform carries the imported module's
//! `lastHMRTimestamp` (importAnalysis). An importer still served from its
//! pre-edit compile names the module by its bare url: two urls, two
//! instances, and the hook reads a context no provider was rendered for.
//! Nothing here needs a page open: the edits land with none.

use std::time::Duration;

use regex::Regex;

use crate::browser::Page;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

const MAIN: &str = "import { createRoot } from \"react-dom/client\";\n\
    import { Provider } from \"./shared\";\nimport { App } from \"./App\";\n\
    createRoot(document.getElementById(\"root\")!).render(<Provider><App /></Provider>);\n";

const APP: &str = "import { Child } from \"./Child\";\nimport { Index } from \"./Index\";\n\
    export function App() {\n  return <div><Child /><Index /></div>;\n}\n";

/// The context, its provider (a component, so the module is a refresh
/// boundary) and the hook that reads it, in one module importing a plain
/// `.ts` dep.
const SHARED: &str = "import { createContext, useContext, type ReactNode } from \"react\";\n\
    import { theme } from \"./theme\";\n\
    export const Ctx = createContext<string | null>(null);\n\
    export function Provider({ children }: { children: ReactNode }) {\n\
    \x20 return <Ctx.Provider value={theme}>{children}</Ctx.Provider>;\n}\n\
    export function useShared() {\n  const v = useContext(Ctx);\n\
    \x20 if (v === null) throw new Error(\"useShared must be used within Provider\");\n\
    \x20 return v;\n}\n";

fn theme(value: &str) -> String {
    format!("export const theme = \"{value}\";\n")
}

fn child(tag: &str) -> String {
    format!(
        "import {{ useShared }} from \"./shared\";\n\
         export function Child() {{\n  return <p id=\"child\" data-tag=\"{tag}\">{{useShared()}}</p>;\n}}\n"
    )
}

const INDEX_PAGE: &str = "import { useShared } from \"./shared\";\n\
    export function Index() {\n  return <p id=\"index\">{useShared()}</p>;\n}\n";

/// Every module of the app: the three importers of `shared` come first.
const IMPORTERS_OF_SHARED: [&str; 3] = ["/src/main.tsx", "/src/Child.tsx", "/src/Index.tsx"];
const ALL: [&str; 6] = [
    "/src/main.tsx",
    "/src/Child.tsx",
    "/src/Index.tsx",
    "/src/App.tsx",
    "/src/shared.tsx",
    "/src/theme.ts",
];

fn app() -> App {
    let app = App::new(&[
        ("package.json", r#"{"name":"hmr-stamps","version":"1.0.0"}"#),
        ("index.html", INDEX),
        ("src/main.tsx", MAIN),
        ("src/App.tsx", APP),
        ("src/shared.tsx", SHARED),
        ("src/theme.ts", &theme("ok")),
        ("src/Child.tsx", &child("v0")),
        ("src/Index.tsx", INDEX_PAGE),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    app
}

/// The `?t=` a module's import of `imported` carries ("" for a bare import).
fn stamp_of(code: &str, imported: &str) -> String {
    let re = Regex::new(&format!(r#""{}(\?t=\d+)?""#, regex::escape(imported))).unwrap();
    re.captures(code)
        .map(|c| c.get(1).map_or("", |m| m.as_str()).to_string())
        .unwrap_or_else(|| panic!("no import of {imported} in:\n{code}"))
}

/// Serves `url` until its import of `imported` carries a stamp other than
/// `not`: the watcher has taken the edit.
async fn stamped_after(server: &crate::Server, url: &str, imported: &str, not: &str) -> String {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let stamp = stamp_of(&server.text(url).await, imported);
        if !stamp.is_empty() && stamp != not {
            return stamp;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{url} never named {imported} under a new stamp"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The stamp each importer of `/src/shared.tsx` names it under.
async fn shared_stamps(server: &crate::Server) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for url in IMPORTERS_OF_SHARED {
        out.push((url, stamp_of(&server.text(url).await, "/src/shared.tsx")));
    }
    out
}

/// With no page open: the provider module's dep is edited, then one of the
/// hook's consumers.
async fn edit_dep_then_a_consumer(app: &App, server: &crate::Server) {
    app.write(&[("src/theme.ts", &theme("edited"))]);
    stamped_after(server, "/src/shared.tsx", "/src/theme.ts", "").await;
    // The second edit lands on a later mtime, and after the first settled.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    app.write(&[("src/Child.tsx", &child("v1"))]);
    stamped_after(server, "/src/App.tsx", "/src/Child.tsx", "").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_importer_names_a_stamped_module_under_its_stamp() {
    require_fixture!("react", "react-dom", "scheduler");
    let app = app();
    let server = app.serve().await;
    // A page load: every module served and cached once.
    for url in ALL {
        server.text(url).await;
    }
    assert!(
        shared_stamps(&server)
            .await
            .iter()
            .all(|(_, s)| s.is_empty()),
        "nothing is stamped before an edit"
    );

    app.write(&[("src/theme.ts", &theme("edited"))]);
    let stamp = stamped_after(&server, "/src/shared.tsx", "/src/theme.ts", "").await;
    // The provider module is stamped with its edited dep; each importer's
    // next serve names it under that stamp, as Vite's soft invalidation
    // rewrites it into every importer, re-fetched by the update or not.
    let stamps = shared_stamps(&server).await;
    for (url, s) in &stamps {
        assert_eq!(
            s, &stamp,
            "{url} names /src/shared.tsx under {s:?}, the module carries {stamp:?}: {stamps:?}"
        );
    }

    tokio::time::sleep(Duration::from_millis(1500)).await;
    app.write(&[("src/Child.tsx", &child("v1"))]);
    stamped_after(&server, "/src/App.tsx", "/src/Child.tsx", "").await;
    // The consumer's edit recompiles it with the stamp of now; the others
    // still agree with it.
    let stamps = shared_stamps(&server).await;
    for (url, s) in &stamps {
        assert_eq!(
            s, &stamp,
            "{url} names /src/shared.tsx under {s:?} after a consumer's edit: {stamps:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_page_after_edits_with_none_open_loads_one_provider() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let url = format!("http://127.0.0.1:{}/", listening.addr.port());
    let page = Page::open(&url).await;
    page.wait_for(
        r##"document.querySelector("#child")?.textContent === "ok" && document.querySelector("#index")?.textContent === "ok""##,
        WAIT,
    )
    .await;
    page.close().await;

    edit_dep_then_a_consumer(&app, &server).await;

    let fresh = Page::open(&url).await;
    let rendered = fresh
        .poll(
            r##"document.querySelector("#child")?.textContent === "edited" && document.querySelector("#index")?.textContent === "edited""##,
            WAIT,
        )
        .await;
    assert!(
        rendered,
        "a fresh page did not render both consumers; errors: {:?}",
        fresh.errors()
    );
    assert_eq!(fresh.errors(), Vec::<String>::new(), "fresh page errors");
    fresh.close().await;
}
