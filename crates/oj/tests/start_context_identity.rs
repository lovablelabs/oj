// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A React context module shared by the root route (its provider) and the
//! route components (its consumers) stays one module after an edit to it: a
//! page loaded fresh after the hot update renders the provider's value on
//! every route, the one in a lazily loaded chunk included, with no "must be
//! used within a provider" error. Two instances of the module would give the
//! provider and the consumers different context objects, as under Vite when
//! an importer keeps an unstamped url of a module another importer loads
//! stamped (`?t=`). The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect } from "react";
import { ThemeProvider } from "../lib/theme";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "context identity" }] }),
  component: RootComponent,
});

function RootComponent() {
  useEffect(() => {
    (self as { __ojHydrated?: boolean }).__ojHydrated = true;
  }, []);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <ThemeProvider>
          <Outlet />
        </ThemeProvider>
        <Scripts />
      </body>
    </html>
  );
}
"#;

/// The shared module: a context, its provider and its hook. Not a refresh
/// boundary (the hook and the context are not components), so an edit to it
/// propagates to every importer.
const THEME: &str = r#"import { createContext, useContext, type ReactNode } from "react";

const ThemeContext = createContext<string | null>(null);

export function ThemeProvider({ children }: { children: ReactNode }) {
  return <ThemeContext.Provider value="theme-v1">{children}</ThemeContext.Provider>;
}

export function useTheme() {
  const theme = useContext(ThemeContext);
  if (theme === null) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return theme;
}
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { useTheme } from "../lib/theme";

export const Route = createFileRoute("/")({
  component: HomePage,
});

function HomePage() {
  return <p id="home">{useTheme()}</p>;
}
"#;

/// A second route: its component lives in its own chunk, loaded when the
/// page is visited.
const ABOUT: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { useTheme } from "../lib/theme";

export const Route = createFileRoute("/about")({
  component: AboutPage,
});

function AboutPage() {
  return <p id="about">{useTheme()}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn context_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(routes.join("about.tsx"), ABOUT).unwrap();
    std::fs::write(root.join("src/lib/theme.tsx"), THEME).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// Opens `path` fresh and waits for `selector` to show `text` on a hydrated
/// page; returns the page's errors (empty on success) with the server output.
async fn fresh_page(dev: &Server, path: &str, selector: &str, text: &str) -> Vec<String> {
    let page = Page::open(&dev.url(path)).await;
    let shown = format!(
        r##"self.__ojHydrated === true && document.querySelector("{selector}")?.textContent === "{text}""##
    );
    let rendered = page.poll(&shown, Duration::from_secs(30)).await;
    let mut errors = page.errors();
    page.close().await;
    if !rendered {
        errors.push(format!("{path}: {selector} never showed {text:?}"));
    }
    errors
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_context_edited_hot_is_one_module_on_a_fresh_page() {
    let Some(app) = context_app("context-identity") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = &app.0;
    let dev = Server::oj_dev(root);

    // Both routes render the provider's value before the edit, so the
    // lazily loaded chunk is in the bundle and the graph.
    for (path, selector) in [("/", "#home"), ("/about", "#about")] {
        let errors = fresh_page(&dev, path, selector, "theme-v1").await;
        assert_eq!(
            errors,
            Vec::<String>::new(),
            "before the edit\n{}",
            dev.output()
        );
    }

    // An open page takes the edit (hot or by a reload; either is fine here).
    let page = Page::open(&dev.url("/")).await;
    assert!(
        page.poll(
            r##"self.__ojHydrated === true && document.querySelector("#home")?.textContent === "theme-v1""##,
            Duration::from_secs(30),
        )
        .await,
        "the open page did not render: {:?}\n{}",
        page.errors(),
        dev.output()
    );
    let theme = root.join("src/lib/theme.tsx");
    std::fs::write(&theme, THEME.replace("theme-v1", "theme-v2")).unwrap();
    let updated = page
        .poll(
            r##"document.querySelector("#home")?.textContent === "theme-v2""##,
            Duration::from_secs(30),
        )
        .await;
    let errors = page.errors();
    page.close().await;
    assert!(
        updated,
        "the open page kept the old value: {errors:?}\n{}",
        dev.output()
    );
    assert_eq!(errors, Vec::<String>::new(), "the open page's errors");

    // Fresh pages after the edit: one context module, so the consumers see
    // the provider on every route.
    for (path, selector) in [("/", "#home"), ("/about", "#about"), ("/", "#home")] {
        let errors = fresh_page(&dev, path, selector, "theme-v2").await;
        assert_eq!(
            errors,
            Vec::<String>::new(),
            "after the edit, {path}\n{}",
            dev.output()
        );
    }
}
