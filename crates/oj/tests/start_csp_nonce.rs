// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The script tag oj appends to a Start document (the live-reload client)
//! carries the document's CSP nonce. An app under a strict policy
//! (`script-src 'nonce-..' 'strict-dynamic'`) hands Start the nonce as the
//! request context and the router's `ssr.nonce`, so every tag Start renders
//! has it; a tag oj adds without one is refused by the browser, which under
//! Vite never happens: Vite adds no tag of its own to a Start document (its
//! client arrives as an import of the nonced client entry), and the tags it
//! does inject into a page take `html.cspNonce` (plugins/html.ts
//! `injectNonceAttributeTagHook`). So oj's tag takes the nonce the document's
//! own `<script>` tags carry (the one the request's policy names), else the
//! config's, and the tags the app wrote itself are left as they are. The browser half
//! skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server, WAIT};

const NONCE: &str = "fixture-nonce-7";

/// The app's server entry: the nonce goes to Start as the request context
/// and onto the document as its policy, the shape Start documents for a CSP.
const ENTRY: &str = r#"import serverEntry from "@tanstack/react-start/server-entry";

type Handler = {
  fetch: (request: Request, opts?: { context?: Record<string, unknown> }) => Promise<Response> | Response;
};

const NONCE = "fixture-nonce-7";

export default {
  async fetch(request: Request): Promise<Response> {
    const res = await (serverEntry as Handler).fetch(request, { context: { nonce: NONCE } });
    if (!(res.headers.get("content-type") ?? "").includes("text/html")) return res;
    const headers = new Headers(res.headers);
    headers.set("Content-Security-Policy", `script-src 'self' 'nonce-${NONCE}' 'strict-dynamic'`);
    return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
  },
};
"#;

/// The router reads the nonce back from the context, so `<Scripts>` and
/// `<HeadContent>` write it on every tag they render.
const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { getGlobalStartContext } from "@tanstack/react-start";

import { routeTree } from "./routeTree";

export function getRouter() {
  const nonce = (getGlobalStartContext() as { nonce?: string } | undefined)?.nonce;
  return createRouter({
    routeTree,
    defaultPreload: "intent",
    scrollRestoration: true,
    ssr: { nonce },
  });
}
"#;

fn write_nonced_app(root: &Path) {
    std::fs::write(root.join("src/ssr-entry.ts"), ENTRY).unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
}

/// `html.cspNonce` in the app's Vite config.
fn write_config_nonce(root: &Path, nonce: &str) {
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let patched = source.replacen(
        "export default defineConfig({",
        &format!("export default defineConfig({{\n  html: {{ cspNonce: \"{nonce}\" }},"),
        1,
    );
    assert_ne!(
        patched, source,
        "the fixture's config has no defineConfig call"
    );
    std::fs::write(config, patched).unwrap();
}

/// Every `<script ...>` start tag of a document, as written.
fn script_tags(html: &str) -> Vec<&str> {
    let mut tags = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<script") {
        let tag = &rest[at..];
        let end = tag.find('>').map_or(tag.len(), |e| e + 1);
        tags.push(&tag[..end]);
        rest = &tag[end..];
    }
    tags
}

fn has_nonce(tag: &str, nonce: &str) -> bool {
    tag.contains(&format!("nonce=\"{nonce}\"")) || tag.contains(&format!("nonce='{nonce}'"))
}

/// The served document's script tags, oj's live-reload tag among them, all
/// carry the nonce the app's own tags carry.
#[test]
fn ojs_script_tag_carries_the_documents_nonce() {
    let Some(app) = app("csp-nonce") else {
        return;
    };
    write_nonced_app(&app.0);
    let dev = Server::oj_dev(&app.0);
    let (status, headers, body) = dev.fetch("GET", "/", &[]);
    let html = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "/\n{html}\n{}", dev.output());
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "content-security-policy" && value.contains(NONCE)),
        "the entry's policy header did not reach the client: {headers:?}"
    );
    let tags = script_tags(&html);
    let live_reload: Vec<&&str> = tags
        .iter()
        .filter(|t| t.contains("/@oj-start/live-reload.js"))
        .collect();
    assert_eq!(live_reload.len(), 1, "one live-reload tag, got {tags:#?}");
    assert!(
        tags.iter()
            .any(|t| t.contains("/@oj-start/client-entry.js") && has_nonce(t, NONCE)),
        "the app's own client entry tag carries the nonce: {tags:#?}"
    );
    let without: Vec<&&str> = tags.iter().filter(|t| !has_nonce(t, NONCE)).collect();
    assert!(
        without.is_empty(),
        "script tags without the document's nonce: {without:#?}\n{}",
        dev.output()
    );
}

/// Under the policy, the page hydrates and oj's live-reload client runs: the
/// browser refused nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_page_hydrates_under_its_policy() {
    let Some(app) = app("csp-nonce-browser") else {
        return;
    };
    if browser::skip() {
        return;
    }
    write_nonced_app(&app.0);
    let dev = Server::oj_dev(&app.0);
    let (status, home) = dev.request("GET", "/", &[]);
    assert!(
        status == 200 && home.contains("HOME!"),
        "/ SSR ({status})\n{home}"
    );
    let page = Page::open(&dev.url("/")).await;
    let mounted = page
        .poll(
            "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
            WAIT,
        )
        .await;
    assert!(
        mounted,
        "the client runtime did not mount under the policy; page errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        dev.output()
    );
    // The live-reload client ran: it hangs its send on the shared bus.
    let reload_client = page
        .poll(
            "typeof globalThis.__ojStartHmr?.send === 'function'",
            Duration::from_secs(10),
        )
        .await;
    let refused: Vec<String> = page
        .console()
        .into_iter()
        .filter(|line| line.contains("Content Security Policy"))
        .collect();
    assert!(
        refused.is_empty() && reload_client,
        "the browser refused oj's tags under the policy (live-reload client ran: {reload_client}): {refused:#?}\nerrors: {:?}",
        page.errors()
    );
    page.close().await;
}

/// `html.cspNonce` names the nonce for oj's tag when the document's tags
/// carry none, as it does for the tags Vite injects into a page; the app's
/// own tags, which had none, get none, as Vite leaves a Start document's own
/// tags alone.
#[test]
fn the_configs_csp_nonce_names_ojs_tags_nonce() {
    let Some(app) = app("csp-nonce-config") else {
        return;
    };
    write_config_nonce(&app.0, "config-nonce-3");
    let dev = Server::oj_dev(&app.0);
    let (status, html) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "/\n{html}\n{}", dev.output());
    let tags = script_tags(&html);
    let live_reload = tags
        .iter()
        .find(|t| t.contains("/@oj-start/live-reload.js"))
        .unwrap_or_else(|| panic!("no live-reload tag in {tags:#?}"));
    assert!(
        has_nonce(live_reload, "config-nonce-3"),
        "oj's tag without the config's nonce: {live_reload}"
    );
    let client_entry = tags
        .iter()
        .find(|t| t.contains("/@oj-start/client-entry.js"))
        .unwrap_or_else(|| panic!("no client entry tag in {tags:#?}"));
    assert!(
        !client_entry.contains("nonce="),
        "the app's own tag was given a nonce: {client_entry}"
    );
}
