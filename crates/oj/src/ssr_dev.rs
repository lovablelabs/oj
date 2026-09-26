// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};

use crate::ssr_host::SsrEngine;

struct SsrState {
    client_url: Option<String>,
    proxy_prefixes: Vec<String>,
    root: PathBuf,
    engine: SsrEngine,
}

pub async fn ssr_dev(
    root: PathBuf,
    entry: String,
    port: Option<u16>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let built = oj_server::DevServer {
        root,
        port,
        host,
        config: None,
        enable_cache: false,
        no_cache: false,
        lazy: false,
        mode: None,
    }
    .build_app()
    .await?;

    let client_url =
        crate::build::derive_client_entry(&built.root, &entry).map(|rel| format!("/{rel}"));
    let entry_abs = built.root.join(&entry);
    let engine = SsrEngine::new(built.root.clone(), entry_abs, built.ssr.clone())?;

    // The runner used to hold an HMR socket for eager invalidation between
    // requests; in-process, the host subscribes to the same broadcast.
    let host = engine.host();
    let mut frames = built.reload_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match frames.recv().await {
                Ok(frame) => {
                    let is_error = serde_json::from_str::<serde_json::Value>(&frame)
                        .ok()
                        .and_then(|v| v.get("type").map(|t| t == "error"))
                        .unwrap_or(false);
                    if is_error {
                        continue;
                    }
                    let dropped = host.invalidate();
                    if dropped > 0 {
                        eprintln!("oj ssr: hmr push -> invalidated {dropped} module(s)");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let ssr_state = Arc::new(SsrState {
        client_url: client_url.clone(),
        proxy_prefixes: built.proxy_prefixes.clone(),
        root: built.root.clone(),
        engine,
    });

    let app = built.router.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&ssr_state),
        ssr_route,
    ));

    let (listener, port) =
        oj_server::bind_dev_listener(built.host, built.port, built.strict_port).await?;
    println!("  {} dev (ssr + module runner)", oj_server::oj_brand());
    println!("  entry:  {entry}");
    match &client_url {
        Some(u) => println!("  client: {u} (hydration + hmr on)"),
        None => println!("  client: none (SSR only; add a *-client entry to hydrate)"),
    }
    let url = format!("http://localhost:{}/", port);
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

fn server_fn_module(root: &Path, module_url: &str) -> Option<PathBuf> {
    const SERVER_SUFFIXES: [&str; 8] = [
        ".server.ts",
        ".server.tsx",
        ".server.js",
        ".server.jsx",
        ".server.mts",
        ".server.mjs",
        ".server.cts",
        ".server.cjs",
    ];

    let rel = module_url.split('?').next().unwrap_or(module_url);
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let candidate = normalize_path(&root.join(rel));
    if !candidate.starts_with(normalize_path(root)) {
        return None;
    }
    let name = candidate.file_name().and_then(|n| n.to_str())?;
    if !SERVER_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return None;
    }
    if !candidate.is_file() {
        return None;
    }
    Some(candidate)
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

enum Route {
    Document,
    Loader,
    Action { json: bool },
    Pass,
}

async fn ssr_route(State(state): State<Arc<SsrState>>, req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::POST && req.uri().path() == "/__oj_fn" {
        let bytes = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap_or_default();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let module_url = payload.get("module").and_then(|m| m.as_str()).unwrap_or("");
        let Some(abs) = server_fn_module(&state.root, module_url) else {
            return (
                StatusCode::FORBIDDEN,
                format!("oj: not a server module: {module_url}"),
            )
                .into_response();
        };
        let name = payload
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        let args = payload
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        return match state.engine.call_server_fn(&abs, &name, args).await {
            Ok(value) => (
                [(header::CONTENT_TYPE, "application/json")],
                value.to_string(),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => render_route(&state, req.uri().path()).await,
        Route::Loader => data_response(state.engine.load(req.uri().path()).await),
        Route::Action { json } => {
            let path = req.uri().path().to_string();
            // The whole body reaches the action as bytes (no size cap, no
            // lossy decode on the way), as it did over the runner's loopback.
            let data = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
                Ok(bytes) => state.engine.action(&path, &bytes).await,
                Err(e) => Err(format!("could not read the action body: {e}")),
            };
            if json {
                data_response(data)
            } else if data.is_ok() {
                (StatusCode::SEE_OTHER, [(header::LOCATION, path)]).into_response()
            } else {
                error_page(&data.unwrap_err())
            }
        }
        Route::Pass => next.run(req).await,
    }
}

fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    if path.rsplit('/').next().is_some_and(|seg| seg.contains('.')) {
        return Route::Pass;
    }
    // Same rule as the dev server's proxy middleware: `^` contexts are regexes
    // over path plus query, the rest are prefixes.
    let url = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    if proxy_prefixes
        .iter()
        .any(|p| oj_server::proxy_context_matches(p, &url))
    {
        return Route::Pass;
    }
    let is_loader = req.headers().contains_key("oj-loader");
    match *req.method() {
        Method::GET if is_loader => Route::Loader,
        Method::GET => Route::Document,
        Method::POST => Route::Action { json: is_loader },
        _ => Route::Pass,
    }
}

fn data_response(data: Result<String, String>) -> Response {
    match data {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Render a document: the engine renders the entry (a streamed render is read
/// to completion, so deferred Suspense content is present), and the HTML is
/// wrapped in the document shell.
async fn render_route(state: &SsrState, path: &str) -> Response {
    use tokio_stream::StreamExt;
    let out = match state.engine.render(path).await {
        Ok(out) => out,
        Err(e) => return error_page(&e),
    };
    let head = tokio_stream::once(Ok::<_, axum::Error>(Bytes::from(page_head(
        &out.data_json,
        &out.head,
    ))));
    let html = tokio_stream::once(Ok::<_, axum::Error>(Bytes::from(out.html)));
    let tail = tokio_stream::once(Ok::<_, axum::Error>(Bytes::from(page_tail(state))));
    let body = head.chain(html).chain(tail);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from_stream(body),
    )
        .into_response()
}

fn page_head(data_json: &str, head_html: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n{head_html}\n\
         <script>window.__OJ_DATA__={data_json}</script>\n\
         <script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
         <script type=\"module\" src=\"/@oj/client.js\"></script>\n</head>\n\
         <body><div id=\"app\">"
    )
}

fn page_tail(state: &SsrState) -> String {
    let entry = match &state.client_url {
        Some(u) => format!("<script type=\"module\" src=\"{u}\"></script>"),
        None => String::new(),
    };
    format!("</div>\n{entry}\n</body>\n</html>\n")
}

fn error_page(msg: &str) -> Response {
    Html(format!(
        "<!doctype html><html><body><pre style=\"color:#c00;white-space:pre-wrap\">[oj ssr] {}</pre></body></html>",
        html_escape(msg)
    ))
    .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with one server module and one ordinary module, plus a sibling
    /// directory outside it.
    fn fixture() -> (PathBuf, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("oj-serverfn-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src/api")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        for name in [
            "src/greeting.server.ts",
            "src/greeting.server.tsx",
            "src/api/user.server.js",
            "src/api/user.server.mjs",
            "src/App.tsx",
            "src/main.tsx",
            "src/server.ts",
            "src/serverish.ts",
            "src/greeting.server",
            "src/greeting.server.css",
            "package.json",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        std::fs::write(base.join("outside/secrets.server.ts"), "x").unwrap();
        (base, root)
    }

    #[test]
    fn a_server_function_call_names_a_server_module_inside_the_project() {
        let (base, root) = fixture();
        for ok in [
            "src/greeting.server.ts",
            "/src/greeting.server.ts",
            "src/greeting.server.tsx",
            "src/api/user.server.js",
            "src/api/user.server.mjs",
            "src/greeting.server.ts?t=123",
            // `..` that lands back inside is still inside.
            "src/../src/greeting.server.ts",
        ] {
            let resolved = server_fn_module(&root, ok).unwrap_or_else(|| panic!("{ok} rejected"));
            assert!(resolved.starts_with(&root), "{ok} -> {resolved:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_server_function_call_cannot_name_a_module_outside_the_project() {
        let (base, root) = fixture();
        // The `module` field comes off the request body, and a browser can POST
        // here cross-origin without a preflight. Unchecked, this is "import and
        // run any module on the machine" from any page the developer has open.
        for hostile in [
            "../outside/secrets.server.ts",
            "../../../../etc/evil.server.js",
            "src/../../outside/secrets.server.ts",
            "/etc/passwd",
            "/Users/someone/.ssh/id_rsa",
            "..",
            "/",
            "",
            " ",
        ] {
            assert!(
                server_fn_module(&root, hostile).is_none(),
                "{hostile:?} accepted"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_server_function_call_cannot_name_an_ordinary_module() {
        let (base, root) = fixture();
        // Production dispatches through a table built from `**/*.server.*`; dev
        // must not be able to reach further than that.
        for not_a_server_module in [
            "src/App.tsx",
            "src/main.tsx",
            "package.json",
            "src/server.ts",
            "src/greeting.server",
            "src/greeting.server.css",
            "src/serverish.ts",
            "node_modules/react/index.js",
        ] {
            assert!(
                server_fn_module(&root, not_a_server_module).is_none(),
                "{not_a_server_module:?} accepted"
            );
        }
        // A server module that does not exist is refused too.
        assert!(server_fn_module(&root, "src/missing.server.ts").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_normalization_does_not_touch_the_filesystem() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
        assert_eq!(normalize_path(Path::new("/a/../..")), PathBuf::from("/"));
        assert_eq!(normalize_path(Path::new("a/b/../b")), PathBuf::from("a/b"));
    }

    #[test]
    fn requests_for_assets_and_internals_are_passed_through() {
        let prefixes: Vec<String> = vec!["/api".to_string()];
        let route = |method: &str, path: &str| {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            matches!(classify(&req, &prefixes), Route::Pass)
        };
        assert!(route("GET", "/@oj/client.js"), "internal urls pass through");
        assert!(
            route("GET", "/__oj_fn"),
            "double-underscore urls pass through"
        );
        assert!(route("GET", "/src/App.tsx"), "a file url passes through");
        assert!(
            route("GET", "/api/users"),
            "a proxied prefix passes through"
        );
        assert!(
            route("PUT", "/dashboard"),
            "an unhandled method passes through"
        );
        assert!(!route("GET", "/dashboard"), "a route is a document");
        assert!(!route("POST", "/dashboard"), "a post is an action");
    }
}
