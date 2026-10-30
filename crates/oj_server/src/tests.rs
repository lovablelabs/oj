use super::*;

// The cached body reaches the wire verbatim and the header tuple
// *overwrites* the `application/octet-stream` that `Bytes` sets (insert,
// not append), so content-type stays a single text/javascript.
#[tokio::test]
async fn cached_js_response_serves_bytes_verbatim_with_text_javascript() {
    let body = Bytes::from("console.log(1)".to_string());
    let resp = cached_js_response(&HeaderMap::new(), "\"e\"".to_string(), body.clone());
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(
        resp.headers().get_all(header::CONTENT_TYPE).iter().count(),
        1
    );
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(resp.headers()[header::ETAG], "\"e\"");
    let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(got, body);
}

#[tokio::test]
async fn cached_js_response_304_on_matching_etag_has_empty_body() {
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_NONE_MATCH, "\"e\"".parse().unwrap());
    let resp = cached_js_response(&headers, "\"e\"".to_string(), Bytes::from_static(b"x"));
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::ETAG], "\"e\"");
    let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(got.is_empty());
}

// The late-activation resync ENQUEUE is confirmed only on an ACK:
// transient failures are retried with backoff, and a middleware that never
// acknowledges makes the helper report failure (the caller then warns
// about stale edits) instead of logging progress over a resync that never
// reached the queue. (Execution is a separate signal — see
// resync_completion_is_claimed_on_the_done_signal_never_on_the_ack.)
#[tokio::test]
async fn resync_retries_until_the_middleware_acks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut n = 0u32;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            n += 1;
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // The first two attempts fail; the third is the real ACK.
            let resp = if n < 3 {
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n"
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    assert!(!notify_plugin_mw_resync(port).await, "a 500 is not an ACK");
    assert!(
        resync_plugin_mw_with_retry(port).await,
        "the retry loop must reach the eventual ACK"
    );
}

#[tokio::test]
async fn resync_reports_failure_when_nothing_acks() {
    // A port with nothing listening: every attempt is refused.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    assert!(!resync_plugin_mw_with_retry(port).await);
}

// The ack is 202-style ("enqueued"); "resynced" is claimed only when the
// host's { ojResyncDone } completion signal moves the counter past the
// pre-enqueue baseline. A busy queue completes late (still claimed), a
// completion racing ahead of the wait is not missed (baseline semantics),
// and a stuck queue times out into the caller's warning path.
#[tokio::test]
async fn resync_completion_is_claimed_on_the_done_signal_never_on_the_ack() {
    let (tx, mut rx) = tokio::sync::watch::channel(0u64);
    // Busy queue: the completion lands after the wait began.
    let baseline = *rx.borrow_and_update();
    let signal = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send_modify(|c| *c += 1);
        tx
    });
    assert!(
        await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
        "a late completion is still claimed"
    );
    let tx = signal.await.unwrap();

    // Fast queue: the completion already landed before the wait started —
    // the pre-enqueue baseline catches it.
    let baseline = *rx.borrow_and_update();
    tx.send_modify(|c| *c += 1);
    assert!(
        await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
        "a completion racing ahead of the wait is never missed"
    );

    // Stuck queue: no signal within the bound — the caller warns instead
    // of logging success off the enqueue ack.
    let baseline = *rx.borrow_and_update();
    assert!(
        !await_resync_completion(&mut rx, baseline, std::time::Duration::from_millis(50)).await,
        "a stuck queue must not be reported as resynced"
    );
}

// One packed word: every reader sees (port, runner_environments) from the
// same write, and a late activation runs the handler BEFORE the flip is
// visible — no reader can observe the new mode with the catch-up unarmed.
#[test]
fn plugin_serve_packs_one_snapshot_and_arms_before_the_flip() {
    let info = |port: Option<u16>, runner: bool| plugins::ServeInfo {
        middleware_port: port,
        runner_environments: runner,
    };
    let serve = PluginServe::default();
    assert_eq!(serve.mw_port(), None);
    assert!(!serve.runner_environments());

    // The handler observes the pre-flip state: set() runs it first.
    let seen = Arc::new(Mutex::new(None::<(Option<u16>, bool)>));
    {
        let serve = Arc::new(PluginServe::default());
        let inner = Arc::clone(&serve);
        let sink = Arc::clone(&seen);
        serve.set_on_activate(Box::new(move || {
            *sink.lock().unwrap() = Some((inner.mw_port(), inner.runner_environments()));
        }));
        assert!(!serve.activated_late());
        serve.set(&info(Some(4001), true));
        assert_eq!(
            *seen.lock().unwrap(),
            Some((None, false)),
            "the activation handler must run before readers can see the flip"
        );
        assert_eq!(serve.mw_port(), Some(4001));
        assert!(serve.runner_environments());
        // A registrar that lost the race to this activation can catch up.
        assert!(serve.activated_late());
        // A repeat set with the same info is not a second activation.
        *seen.lock().unwrap() = None;
        serve.set(&info(Some(4001), true));
        assert_eq!(*seen.lock().unwrap(), None);
    }

    // runner_environments only counts with a middleware port.
    let no_port = PluginServe::from_info(&info(None, true));
    assert_eq!(no_port.mw_port(), None);
    assert!(!no_port.runner_environments());
    let no_runner = PluginServe::from_info(&info(Some(4002), false));
    assert_eq!(no_runner.mw_port(), Some(4002));
    assert!(!no_runner.runner_environments());
    // The boot fill is not a late activation: a caller's post-registration
    // catch-up must not fire (and reload the runner) on every normal boot.
    assert!(!no_runner.activated_late());
}

#[test]
fn dep_transform_gate_matches_only_marker_sources() {
    // The plugins' own transform code-filter patterns (getDepTransformFilters).
    let res: Vec<regex::Regex> = [
        r"\bcreateServerFn\b|\.\s*handler\s*\(",
        "createIsomorphicFn",
    ]
    .iter()
    .map(|s| regex::Regex::new(s).unwrap())
    .collect();
    let wants = |src: &str| res.iter().any(|re| re.is_match(src));
    assert!(wants(
        "export const f = createIsomorphicFn().client(() => 1)"
    ));
    assert!(wants("const x = createServerFn()"));
    assert!(wants("route.handler ( () => {} )"));
    assert!(!wants("export const x = 1;"));
    assert!(!wants(
        "import { getStartContext } from '@tanstack/start-storage-context'"
    ));
}

#[test]
fn decode_at_id_handles_hex_and_raw_vite_ids() {
    assert_eq!(decode_at_id(&hex_encode("virtual:x")), "virtual:x");
    assert_eq!(
        decode_at_id("virtual:tanstack-start-dev-client-entry"),
        "virtual:tanstack-start-dev-client-entry"
    );
    assert_eq!(decode_at_id("__x00__virtual:foo"), "\0virtual:foo");
}

#[test]
fn parse_hmr_filter_reads_filter_module_urls() {
    let seeds =
        parse_hmr_filter(r#"{"action":"filter","modules":["/src/a.tsx","/src/b.tsx"]}"#).unwrap();
    assert_eq!(
        seeds,
        vec![PathBuf::from("/src/a.tsx"), PathBuf::from("/src/b.tsx")]
    );
}

#[test]
fn parse_hmr_filter_ignores_non_filter_payloads() {
    assert!(parse_hmr_filter("skip").is_none());
    assert!(parse_hmr_filter("full-reload").is_none());
    assert!(parse_hmr_filter(r#"{"action":"reload"}"#).is_none());
    assert!(parse_hmr_filter(r#"{"action":"filter"}"#).is_none());
    assert!(parse_hmr_filter("not json at all").is_none());
}

#[tokio::test]
async fn bind_dev_listener_increments_unless_strict() {
    use std::net::{IpAddr, Ipv4Addr};
    let host = IpAddr::V4(Ipv4Addr::LOCALHOST);
    // Hold an ephemeral port so the preferred one is busy.
    let occupied = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
    let taken = occupied.local_addr().unwrap().port();

    // strict: a busy preferred port is a hard error, never moved.
    assert!(
        bind_dev_listener(host, taken, true).await.is_err(),
        "strict must reject a busy port",
    );

    // non-strict (Vite default): hop to the next free port.
    let (listener, port) = bind_dev_listener(host, taken, false).await.unwrap();
    assert_ne!(port, taken, "non-strict must pick a different port");
    assert_eq!(listener.local_addr().unwrap().port(), port);
}

// A stylesheet's `?inline` (compiled css string through the full pipeline,
// not a data URI) is covered end to end by e2e/css-vite-parity.mjs, since
// it runs the server's compile path.

#[tokio::test]
async fn non_css_inline_stays_a_data_uri() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("pixel.png");
    std::fs::write(&png, [0u8, 1, 2, 3]).unwrap();
    let out = asset_module(
        &png,
        "/pixel.png?inline",
        "inline",
        &DevAssetUrls::default(),
        "client",
    )
    .await
    .unwrap();
    assert!(
        out.contains("data:"),
        "binary asset stays a data URI: {out}"
    );
}

#[test]
fn postcss_config_is_found_like_postcss_load_config() {
    let base = std::env::temp_dir().join(format!("oj-postcss-find-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("packages/web");
    std::fs::create_dir_all(&app).unwrap();
    // Workspace root marker: Vite's searchForWorkspaceRoot is the stopDir of
    // its postcss-load-config search (`.git` is not a marker in Vite).
    std::fs::write(base.join("pnpm-workspace.yaml"), "packages: ['packages/*']").unwrap();
    assert!(find_postcss_config(&app).is_none());
    // A config at the workspace root applies to the package.
    std::fs::write(base.join(".postcssrc.json"), r#"{"plugins":{}}"#).unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(base.join(".postcssrc.json"))
    );
    // package.json#postcss in the package itself wins (nearest first).
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"web","postcss":{"plugins":{}}}"#,
    )
    .unwrap();
    assert_eq!(find_postcss_config(&app), Some(app.join("package.json")));
    // ...and a config file in the package beats its package.json key.
    std::fs::write(app.join("postcss.config.ts"), "export default {}").unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(app.join("postcss.config.ts"))
    );
    // A package.json without the key does not count.
    std::fs::remove_file(app.join("postcss.config.ts")).unwrap();
    std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(base.join(".postcssrc.json"))
    );
    // Nothing above the workspace root is consulted.
    std::fs::remove_file(base.join(".postcssrc.json")).unwrap();
    assert!(find_postcss_config(&app).is_none());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn fs_deny_blocks_dotenv_and_git_by_default() {
    // No user config: Vite's default deny set still protects secrets.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&[]);
    assert!(path_is_denied(&root.join(".env"), root, &deny));
    assert!(path_is_denied(&root.join(".env.local"), root, &deny));
    assert!(path_is_denied(&root.join("certs/server.pem"), root, &deny));
    assert!(path_is_denied(&root.join(".git/config"), root, &deny));
    assert!(path_is_denied(
        &root.join("packages/app/.git/HEAD"),
        root,
        &deny
    ));
    // Ordinary source is served.
    assert!(!path_is_denied(&root.join("src/main.tsx"), root, &deny));
    assert!(!path_is_denied(&root.join("src/env.ts"), root, &deny));
}

#[test]
fn fs_deny_honors_user_patterns() {
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
    assert!(path_is_denied(&root.join("secrets/token.txt"), root, &deny));
    assert!(path_is_denied(&root.join("id_rsa.key"), root, &deny));
    assert!(!path_is_denied(&root.join("public/logo.svg"), root, &deny));
}

#[test]
fn fs_deny_is_case_insensitive() {
    // On a case-insensitive filesystem `.ENV` opens the same bytes as
    // `.env`, so a case-sensitive deny glob would leak it. Every case
    // variant of a denied name (base-name and path patterns) must be denied.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
    assert!(path_is_denied(&root.join(".ENV"), root, &deny));
    assert!(path_is_denied(&root.join(".Env.Production"), root, &deny));
    assert!(path_is_denied(&root.join("certs/SERVER.PEM"), root, &deny));
    assert!(path_is_denied(&root.join(".GIT/config"), root, &deny));
    assert!(path_is_denied(&root.join("Secrets/token.txt"), root, &deny));
    assert!(path_is_denied(&root.join("id_rsa.KEY"), root, &deny));
}

#[test]
fn fs_deny_expands_brace_groups() {
    // Vite's own default deny list is brace-form (*.{crt,pem,key,...});
    // the glob crate has no brace support, so without expansion those
    // patterns match nothing and denied files get served.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&[
        "*.{key,p12,pfx}".to_string(),
        "secrets/{a,b}/**".to_string(),
    ]);
    assert!(path_is_denied(&root.join("server.key"), root, &deny));
    assert!(path_is_denied(&root.join("bundle.p12"), root, &deny));
    assert!(path_is_denied(&root.join("cert.pfx"), root, &deny));
    assert!(path_is_denied(&root.join("secrets/a/token"), root, &deny));
    assert!(path_is_denied(&root.join("secrets/b/token"), root, &deny));
    assert!(!path_is_denied(&root.join("secrets/c/token"), root, &deny));
    assert!(!path_is_denied(&root.join("server.kee"), root, &deny));
}

#[test]
fn warmup_paths_matches_exclusions_root_relative() {
    // A './'-spelled exclusion and a bare positive pattern must agree
    // about the same file, and a root containing glob metacharacters must
    // not break the walk (the root is escaped, patterns are relative).
    let dir = std::env::temp_dir().join(format!("oj-warmup-[x]-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/generated")).unwrap();
    std::fs::write(dir.join("src/a.js"), "a").unwrap();
    std::fs::write(dir.join("src/generated/b.js"), "b").unwrap();
    let picked = warmup_paths(
        &dir,
        &[
            "src/**/*.js".to_string(),
            "!./src/generated/*.js".to_string(),
        ],
    );
    assert_eq!(picked.len(), 1, "exclusion must apply: {picked:?}");
    assert!(picked[0].ends_with("src/a.js"));
}

#[test]
fn bare_specifier_classification_routes_plugin_virtuals_to_the_fallback() {
    // Plugin virtuals (`virtual:pwa-register`, \0-prefixed ids) count as
    // bare, so the plugin-host fallback (/@id/) resolves them; relative and
    // absolute-URL specifiers do not.
    assert!(is_bare_specifier("react"));
    assert!(is_bare_specifier("virtual:pwa-register"));
    assert!(is_bare_specifier("\0oj-virtual"));
    assert!(!is_bare_specifier("./local"));
    assert!(!is_bare_specifier("../up"));
    assert!(!is_bare_specifier("/abs"));
    assert!(!is_bare_specifier("https://cdn/x.js"));
}

#[test]
fn sec_fetch_dest_decides_raw_vs_module_form() {
    // A `<link>`/`<img>`/@font-face request wants the raw resource; a JS
    // `import` (script/empty dest) wants the JS-module form. This is how a
    // `.css` reached from JS is served as JS, not a text/css module script.
    let raw = |d: &str| {
        let mut h = HeaderMap::new();
        h.insert("sec-fetch-dest", d.parse().unwrap());
        wants_raw_resource(&h)
    };
    assert!(raw("style"));
    assert!(raw("image"));
    assert!(raw("font"));
    assert!(!raw("script"));
    assert!(!raw("empty"));
    // No header (older browsers / non-browser clients): default to the
    // module form so JS imports keep working.
    assert!(!wants_raw_resource(&HeaderMap::new()));
}

#[test]
fn imports_a_plugin_virtual_flags_only_missing_fs_paths() {
    // A cached module is re-transformed on warm start only if it imports a
    // plugin-served virtual: a filesystem-path import with no file on disk.
    // Real files (source, svgr's .svg) and oj-internal /@ routes keep the
    // fast persistent cache.
    let root = std::env::temp_dir().join(format!("oj-ipv-test-{}", std::process::id()));
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("real.ts"), "export {}\n").unwrap();
    let dc = Mutex::new(DirCache::new());
    let r = root.as_path();

    // All-real imports -> keep cache.
    assert!(!imports_a_plugin_virtual(
        &["/src/real.ts".to_string()],
        r,
        &dc
    ));
    // oj-internal routes and external urls are never plugin-state virtuals.
    assert!(!imports_a_plugin_virtual(
        &[
            "/@id/abc".to_string(),
            "/@virtual/x".to_string(),
            "https://cdn/x.js".to_string()
        ],
        r,
        &dc,
    ));
    // A missing absolute path (wyw's .wyw-in-js.css) -> re-transform.
    assert!(imports_a_plugin_virtual(
        &[
            "/src/real.ts".to_string(),
            "/Users/nope/x.wyw-in-js.css".to_string()
        ],
        r,
        &dc,
    ));
    // A missing root-relative path -> re-transform.
    assert!(imports_a_plugin_virtual(
        &["/src/gone.css".to_string()],
        r,
        &dc
    ));
    // Query strings are stripped before the on-disk check.
    assert!(!imports_a_plugin_virtual(
        &["/src/real.ts?import".to_string()],
        r,
        &dc
    ));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn lingui_macro_specifiers_are_matched_exactly() {
    assert!(is_lingui_macro_specifier("@lingui/macro"));
    assert!(is_lingui_macro_specifier("@lingui/core/macro"));
    assert!(is_lingui_macro_specifier("@lingui/react/macro"));
    // The runtime packages (not the macro entrypoints) must NOT be shimmed.
    assert!(!is_lingui_macro_specifier("@lingui/core"));
    assert!(!is_lingui_macro_specifier("@lingui/react"));
    assert!(!is_lingui_macro_specifier("@lingui/macro/extra"));
}

#[test]
fn node_builtins_are_recognized_for_stubbing() {
    assert!(is_node_builtin("fs"));
    assert!(is_node_builtin("node:fs"));
    assert!(is_node_builtin("fs/promises"));
    assert!(is_node_builtin("crypto"));
    assert!(is_node_builtin("perf_hooks"));
    // Vite's isNodeBuiltin: every `node:` id is a builtin, including the
    // scheme-only modules and ones newer than any hardcoded list.
    assert!(is_node_builtin("node:sqlite"));
    assert!(is_node_builtin("node:sea"));
    assert!(is_node_builtin("node:test"));
    assert!(is_node_builtin("node:whatever-node-adds-next"));
    assert!(is_node_builtin("_http_common"));
    // Not builtins: real packages and app specifiers. The scheme-only names
    // are not builtins when bare, exactly like Node.
    assert!(!is_node_builtin("sqlite"));
    assert!(!is_node_builtin("test"));
    assert!(!is_node_builtin("source-map-support"));
    assert!(!is_node_builtin("react"));
    assert!(!is_node_builtin("./local"));
}

#[test]
fn workspace_root_matches_vite_search_for_workspace_root() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    // A git repository is NOT a workspace marker (Vite comments `.git` out):
    // the default fs.allow for a project nested in a repo stays the project.
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    let app = repo.join("apps/web");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
    assert_eq!(
        workspace_root(&app),
        app,
        "nearest package.json, not the repo"
    );

    // A pnpm workspace marker above it widens the root to the workspace.
    std::fs::write(repo.join("pnpm-workspace.yaml"), "packages: ['apps/*']").unwrap();
    assert_eq!(workspace_root(&app), repo);
    std::fs::remove_file(repo.join("pnpm-workspace.yaml")).unwrap();

    // ...as does a root package.json with a `workspaces` field.
    std::fs::write(repo.join("package.json"), r#"{"workspaces":["apps/*"]}"#).unwrap();
    assert_eq!(workspace_root(&app), repo);

    // No package.json anywhere: the app root itself.
    let bare = repo.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    assert_eq!(
        workspace_root(&bare),
        repo,
        "nearest ancestor with a package.json"
    );

    // The PARSED field decides, not a substring: a dependency literally
    // named "workspaces" must not widen the served root.
    std::fs::write(
        repo.join("package.json"),
        r#"{"dependencies":{"workspaces":"1.0.0"}}"#,
    )
    .unwrap();
    assert_eq!(workspace_root(&app), app, "substring is not a marker");
    std::fs::write(repo.join("package.json"), "{}").unwrap();

    // A deno.json with a truthy `workspace` is a marker (Vite's
    // hasWorkspaceDenoJSON); a deno.jsonc counts only as valid JSON.
    std::fs::write(repo.join("deno.json"), r#"{"workspace":["./apps/web"]}"#).unwrap();
    assert_eq!(workspace_root(&app), repo, "deno.json workspace widens");
    std::fs::remove_file(repo.join("deno.json")).unwrap();
    std::fs::write(
        repo.join("deno.jsonc"),
        "// comment\n{\"workspace\":[\"./apps/web\"]}",
    )
    .unwrap();
    assert_eq!(
        workspace_root(&app),
        app,
        "a deno.jsonc with comments is skipped like Vite"
    );
}

#[test]
fn browser_external_stub_warns_like_vite_on_property_access() {
    let src = browser_external_stub_source("node:fs");
    assert!(src.contains("has been externalized for browser compatibility"));
    assert!(src.contains("Cannot access \"${\"node:fs\"}.${key}\" in client code"));
    // Interop reads names off __cjs_exports and probes __esModule first; the
    // probe must stay silent and the module must still export both shapes.
    assert!(src.contains("key !== \"__esModule\""));
    assert!(src.contains("export default __oj_ext"));
    assert!(src.contains("export const __cjs_exports = __oj_ext"));
    // The stub is JS-safe for any id (quotes escaped through JSON).
    let quoted = browser_external_stub_source("a\"b");
    assert!(quoted.contains("\"a\\\"b\""));
}

#[test]
fn importable_asset_exts_exclude_code_and_svg() {
    assert!(is_importable_asset_ext("webp"));
    assert!(is_importable_asset_ext("png"));
    assert!(is_importable_asset_ext("woff2"));
    // Vite's list is case-insensitive and includes documents and more media.
    assert!(is_importable_asset_ext("PNG"));
    assert!(is_importable_asset_ext("pdf"));
    assert!(is_importable_asset_ext("flac"));
    assert!(is_asset_ext("Jpg"));
    assert_eq!(content_type("JPG"), "image/jpeg");
    // svg is routed to vite-plugin-svgr, not URL-exported here.
    assert!(!is_importable_asset_ext("svg"));
    assert!(!is_importable_asset_ext("SVG"));
    assert!(!is_importable_asset_ext("css"));
    assert!(!is_importable_asset_ext("js"));
}

#[test]
fn ts_source_outside_root_is_not_treated_as_a_dep() {
    // A monorepo package reached through a resolve.alias is served via /@fs/
    // but is TS/JSX source that must be transpiled, not dep/CJS-interop'd.
    let fs = Path::new("/repo/packages/ui/src/Button.tsx");
    assert!(!is_dep_module("/@fs/repo/packages/ui/src/Button.tsx", fs));
    let ts = Path::new("/repo/packages/ui/src/index.ts");
    assert!(!is_dep_module("/@fs/repo/packages/ui/src/index.ts", ts));
    // A real dependency (.js/.mjs, or anything under node_modules that is not
    // TS/JSX source) stays on the dep path.
    let dep = Path::new("/app/node_modules/react/index.js");
    assert!(is_dep_module("/node_modules/react/index.js", dep));
    // A linked workspace package (realpath outside node_modules) is source,
    // not a dep: plugins and the source compile path apply (Vite treats
    // linked packages the same way).
    let fs_js = Path::new("/repo/packages/ui/dist/index.mjs");
    assert!(!is_dep_module(
        "/@fs/repo/packages/ui/dist/index.mjs",
        fs_js
    ));
    let fs_dep = Path::new("/repo/node_modules/.pnpm/x@1/node_modules/x/index.js");
    assert!(is_dep_module(
        "/@fs/repo/node_modules/.pnpm/x@1/node_modules/x/index.js",
        fs_dep
    ));
    // App-local source (not node_modules, not /@fs/) is never a dep.
    let local = Path::new("/app/src/App.tsx");
    assert!(!is_dep_module("/src/App.tsx", local));
}

#[test]
fn glue_only_added_for_boundary_modules() {
    assert!(hot_glue(
        "/src/util.ts",
        None,
        false,
        false,
        &OjPublicPaths::new(None)
    )
    .is_empty());
    let glue = hot_glue(
        "/src/App.tsx",
        Some("t=1700000000000"),
        true,
        false,
        &OjPublicPaths::new(None),
    );
    assert!(glue.contains(r#"createHotContext("/src/App.tsx")"#));
    assert!(
        glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
        "{glue}"
    );
    assert!(glue.contains("validateRefreshBoundaryAndEnqueueUpdate"));
    assert!(glue.contains("function $RefreshReg$"));
}

// A module that reads import.meta.hot itself gets the hot-context banner
// PREPENDED (svelte_hot_glue); the appended refresh glue must then reuse
// that context, never re-import it — an import binding is a lexical
// declaration, and a second `__oj_createHotContext` in the same module
// scope is a SyntaxError that kills the module (seen on TanStack route
// files, whose router plugin injects its own import.meta.hot handler).
#[test]
fn glue_reuses_a_predefined_hot_context_instead_of_redeclaring_it() {
    let banner = svelte_hot_glue("/src/routes/a.tsx", &OjPublicPaths::new(None));
    let glue = hot_glue(
        "/src/routes/a.tsx",
        Some("t=1700000000000"),
        true,
        true,
        &OjPublicPaths::new(None),
    );
    assert!(!glue.contains("__oj_createHotContext"), "{glue}");
    assert!(glue.contains("registerExportsForReactRefresh"), "{glue}");
    let combined = format!("{banner}{glue}");
    assert_eq!(
        combined
            .matches("createHotContext as __oj_createHotContext")
            .count(),
        1,
        "exactly one declaration per module scope: {combined}"
    );
}

#[test]
fn glue_never_doubles_a_query_the_url_already_carries() {
    // serve_compiled keys per full url, so the real call hands hot_glue a url
    // that already has its query AND the same query again. The self-import
    // must not grow (`?t=X?t=X` grew per edit until hyper answered 414) and
    // the hot-context id must stay the clean path the server sends updates for.
    let glue = hot_glue(
        "/src/App.tsx?t=1700000000000",
        Some("t=1700000000000"),
        true,
        false,
        &OjPublicPaths::new(None),
    );
    assert!(
        glue.contains(r#"createHotContext("/src/App.tsx")"#),
        "{glue}"
    );
    assert!(
        glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
        "{glue}"
    );
    assert!(!glue.contains("?t=1700000000000?t=1700000000000"), "{glue}");
    assert!(
        glue.contains(r#"registerExportsForReactRefresh("/src/App.tsx","#),
        "{glue}"
    );
    // Only the hmr timestamp is stripped from the id; a semantic query that
    // makes a distinct module (router `?tsr-shared=1`) is kept in both.
    let v = hot_glue(
        "/src/r.tsx?tsr-shared=1&t=1700000000000",
        Some("tsr-shared=1&t=1700000000000"),
        true,
        false,
        &OjPublicPaths::new(None),
    );
    assert!(
        v.contains(r#"createHotContext("/src/r.tsx?tsr-shared=1")"#),
        "{v}"
    );
    assert!(
        v.contains(r#"from "/src/r.tsx?tsr-shared=1&t=1700000000000""#),
        "{v}"
    );
}

// Vite's clientPublicPath: every import oj writes of its client and refresh
// runtime carries the base, as the page's injected tags do, so one module
// instance serves the page.
#[test]
fn oj_modules_are_imported_under_the_base() {
    let paths = OjPublicPaths::new(Some("/app/"));
    assert_eq!(paths.client, "/app/@oj/client.js");
    assert_eq!(paths.refresh_runtime, "/app/@oj/refresh-runtime.js");
    assert_eq!(paths.refresh_preamble, "/app/@oj/refresh-preamble.js");
    let glue = hot_glue("/src/App.tsx", None, true, false, &paths);
    assert!(glue.contains(r#"from "/app/@oj/client.js""#), "{glue}");
    assert!(
        glue.contains(r#"from "/app/@oj/refresh-runtime.js""#),
        "{glue}"
    );
    let banner = svelte_hot_glue("/src/a.svelte", &paths);
    assert!(banner.contains(r#"from "/app/@oj/client.js""#), "{banner}");
    let runtime = std::str::from_utf8(&paths.refresh_runtime_js).unwrap();
    assert!(
        runtime.starts_with("import \"/app/@oj/client.js\";\n"),
        "{runtime}"
    );
    let preamble = std::str::from_utf8(&paths.refresh_preamble_js).unwrap();
    assert!(
        preamble.contains(r#"from "/app/@oj/refresh-runtime.js""#),
        "{preamble}"
    );
    assert!(!preamble.contains(r#""/@oj/"#), "{preamble}");
    assert!(paths.owns("/app/@oj/refresh-runtime.js") && paths.owns("/@oj/routes.js"));
    assert!(!paths.owns("/app/src/main.tsx"));

    let root = OjPublicPaths::new(None);
    assert_eq!(root.client, "/@oj/client.js");
    assert!(std::str::from_utf8(&root.refresh_runtime_js)
        .unwrap()
        .starts_with("import \"/@oj/client.js\";\n"));
}

#[test]
fn restart_triggers_are_the_files_oj_loaded() {
    let root = Path::new("/app");
    let files = restart_files(
        root,
        &root.join("config/../env"),
        "staging",
        Some(Path::new("/elsewhere/custom.config.ts")),
    );
    for f in [
        "/app/oj.config.ts",
        "/app/oj.config.json",
        "/app/vite.config.mts",
        "/app/postcss.config.cjs",
        "/app/env/.env",
        "/app/env/.env.local",
        "/app/env/.env.staging",
        "/app/env/.env.staging.local",
        "/elsewhere/custom.config.ts",
    ] {
        assert!(is_restart_trigger(&files, Path::new(f)), "{f}");
    }
    // Vite restarts for the mode's env files in envDir and the config only:
    // the same names elsewhere (a sibling package an outside-root `@source`
    // watches, a nested dir, another mode) are files like any other.
    for f in [
        "/app/src/main.ts",
        "/app/package.json",
        "/app/.env",
        "/app/env/.env.production",
        "/app/src/.env",
        "/app/packages/ui/vite.config.ts",
        "/sibling/.env",
        "/sibling/postcss.config.js",
    ] {
        assert!(!is_restart_trigger(&files, Path::new(f)), "{f}");
    }
}

#[test]
fn tsconfig_files_trigger_the_reload_path() {
    for f in ["tsconfig.json", "app/tsconfig.json", "tsconfig.base.json"] {
        assert!(is_tsconfig_file(Path::new(f)), "{f}");
    }
    for f in ["package.json", "src/tsconfig.ts", "not-tsconfig.json"] {
        assert!(!is_tsconfig_file(Path::new(f)), "{f}");
    }
}

#[test]
fn error_frame_is_a_vite_error_payload() {
    let f: serde_json::Value = serde_json::from_str(&error_frame(
        "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>",
    ))
    .unwrap();
    assert_eq!(f["type"], "error");
    assert_eq!(
        f["err"]["message"],
        "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>"
    );
    assert_eq!(f["err"]["id"], "src/App.tsx");
    assert_eq!(f["err"]["loc"]["line"], 3);
    assert_eq!(f["err"]["loc"]["column"], 7);
    assert!(f["err"]["frame"]
        .as_str()
        .unwrap()
        .contains("Unexpected token"));
    let plain: serde_json::Value = serde_json::from_str(&error_frame("boom")).unwrap();
    assert!(plain["err"]["id"].is_null() && plain["err"]["frame"].is_null());
    let u = update_entry("css-update", "/a.css", 5);
    assert_eq!(u["acceptedPath"], "/a.css");
    assert_eq!(u["type"], "css-update");
}

#[test]
fn ws_proxy_target_and_header_rules() {
    assert_eq!(
        ws_target_url("http://localhost:4000"),
        "ws://localhost:4000"
    );
    assert_eq!(ws_target_url("https://api.test"), "wss://api.test");
    assert_eq!(
        ws_target_origin("wss://api.test:8443/socket?x=1"),
        "https://api.test:8443"
    );
    assert_eq!(
        ws_target_origin("ws://localhost:3000"),
        "http://localhost:3000"
    );
    assert_eq!(ws_target_url("ws://x:1"), "ws://x:1");
    assert!(ws_forwardable_header(&header::COOKIE));
    assert!(ws_forwardable_header(&header::SEC_WEBSOCKET_PROTOCOL));
    assert!(ws_forwardable_header(&header::ORIGIN));
    for h in [
        header::HOST,
        header::CONNECTION,
        header::UPGRADE,
        header::SEC_WEBSOCKET_KEY,
        header::SEC_WEBSOCKET_VERSION,
        header::SEC_WEBSOCKET_EXTENSIONS,
    ] {
        assert!(!ws_forwardable_header(&h), "{h}");
    }
    let mut h = HeaderMap::new();
    h.insert(header::UPGRADE, "WebSocket".parse().unwrap());
    assert!(is_websocket_upgrade(&h));
    assert!(!is_websocket_upgrade(&HeaderMap::new()));
}

#[test]
fn proxy_tls_configs_build_for_both_secure_settings() {
    assert!(proxy_tls_config(false).is_ok(), "accept-any config");
    assert!(proxy_tls_config(true).is_ok(), "platform verifier config");
}

#[test]
fn localhost_origin_default_matches_vite_regex() {
    for ok in [
        "http://localhost",
        "http://localhost:5173",
        "https://app.localhost:3000",
        "http://127.0.0.1:8080",
        "http://[::1]:5173",
    ] {
        assert!(is_localhost_origin(ok), "{ok}");
    }
    for bad in [
        "http://evil.com",
        "http://localhost.evil.com",
        "http://127.0.0.1.nip.io",
        "ftp://localhost",
        "http://localhost:abc",
    ] {
        assert!(!is_localhost_origin(bad), "{bad}");
    }
}

#[test]
fn host_policy_allows_localhost_ips_and_configured_hosts_only() {
    let server = oj_config::ServerConfig {
        allowed_hosts: Some(oj_config::AllowedHosts::List(vec![
            "app.test".into(),
            ".corp.example".into(),
        ])),
        ..Default::default()
    };
    let p = HostPolicy::from_config(&server, Some("dev.local"));
    for ok in [
        "localhost",
        "sub.localhost",
        "127.0.0.1",
        "[::1]",
        "10.0.0.5",
        "app.test",
        "APP.TEST",
        "corp.example",
        "x.corp.example",
        "dev.local",
    ] {
        assert!(p.hostname_allowed(ok), "{ok}");
    }
    for bad in ["evil.com", "notcorp.example", "app.test.evil", ""] {
        assert_eq!(p.hostname_allowed(bad), bad.is_empty(), "{bad}");
    }
    assert_eq!(
        HostPolicy::host_header_name("example.com:5173"),
        "example.com"
    );
    assert_eq!(HostPolicy::host_header_name("[::1]:5173"), "::1");
    assert_eq!(HostPolicy::host_header_name("example.com"), "example.com");
    let all = HostPolicy::from_config(
        &oj_config::ServerConfig {
            allowed_hosts: Some(oj_config::AllowedHosts::All(true)),
            ..Default::default()
        },
        None,
    );
    assert!(all.allow_all && all.hostname_allowed("evil.com"));
}

#[test]
fn cors_policy_forms() {
    let default = CorsPolicy::from_config(None).unwrap();
    assert!(default.allows("http://localhost:5173") && !default.allows("http://evil.com"));
    assert!(CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(false))).is_none());
    let any = CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(true))).unwrap();
    assert!(any.allows("http://evil.com"));
    let opts: oj_config::CorsOptions = serde_json::from_value(serde_json::json!({
        "origin": ["http://a.test", "http://b.test"], "credentials": true, "methods": ["GET", "POST"], "maxAge": 60
    }))
    .unwrap();
    let list =
        CorsPolicy::from_config(Some(&oj_config::CorsConfig::Options(Box::new(opts)))).unwrap();
    assert!(list.allows("http://a.test") && !list.allows("http://localhost:5173"));
    assert!(list.credentials && list.methods == "GET,POST" && list.max_age == Some(60));
}

#[test]
fn asset_requests_are_modules_only_for_imports() {
    let mut h = HeaderMap::new();
    assert!(!wants_module_import(&h, None), "a bare fetch gets the file");
    assert!(wants_module_import(&h, Some("import")));
    assert!(wants_module_import(&h, Some("t=1&import")));
    h.insert("sec-fetch-dest", "empty".parse().unwrap());
    assert!(!wants_module_import(&h, None));
    h.insert("sec-fetch-dest", "image".parse().unwrap());
    assert!(!wants_module_import(&h, None));
    h.insert("sec-fetch-dest", "script".parse().unwrap());
    assert!(wants_module_import(&h, None), "a module import of the url");
}

#[test]
fn watch_ignored_globs_match_relative_and_absolute_paths() {
    let root = Path::new("/app");
    let pats = watch_ignored_patterns(
        root,
        &[
            "**/generated/**".to_string(),
            "docs/*.md".to_string(),
            "/tmp/out/**".to_string(),
        ],
    );
    assert!(is_watch_ignored(
        &pats,
        root,
        Path::new("/app/src/generated/x.ts")
    ));
    assert!(
        is_watch_ignored(&pats, root, Path::new("/app/docs/intro.md")),
        "root-relative pattern"
    );
    assert!(
        !is_watch_ignored(&pats, root, Path::new("/app/docs/deep/intro.md")),
        "* stops at /"
    );
    assert!(
        is_watch_ignored(&pats, root, Path::new("/tmp/out/a/b.js")),
        "absolute pattern"
    );
    assert!(!is_watch_ignored(
        &pats,
        root,
        Path::new("/app/src/main.ts")
    ));
    assert!(!is_watch_ignored(
        &[],
        root,
        Path::new("/app/src/generated/x.ts")
    ));
}

#[test]
fn watch_feed_drops_excluded_paths_and_dropped_subscribers() {
    let root = Path::new("/app");
    let cache = Path::new("/app/relocated-cache");
    let ignored = watch_ignored_patterns(root, &["**/generated/**".to_string()]);
    let feed = WatchFeed::default();
    let ev = |paths: &[&str]| {
        let mut ev = notify::Event::new(notify::EventKind::Any);
        ev.paths = paths.iter().map(PathBuf::from).collect();
        ev
    };
    let kept = feed.subscribe();
    let dropped = feed.subscribe();
    drop(dropped);

    feed.publish(
        &ev(&[
            "/app/shared/a.ts",
            "/app/src/generated/b.ts",
            "/app/relocated-cache/v1/start/chunk.js",
        ]),
        &ignored,
        root,
        cache,
    );
    feed.publish(&ev(&["/app/src/generated/c.ts"]), &ignored, root, cache);
    feed.publish(
        &ev(&["/app/relocated-cache/v1/mod.json"]),
        &ignored,
        root,
        cache,
    );

    feed.publish(
        &ev(&["/app/vite.config.ts.timestamp-1791586920568-4c4081376eef.mjs"]),
        &ignored,
        root,
        cache,
    );

    let got = kept.try_recv().expect("the unexcluded path is delivered");
    assert_eq!(got.paths, vec![PathBuf::from("/app/shared/a.ts")]);
    assert!(
        kept.try_recv().is_err(),
        "all-ignored, cache-dir and config-temp events are not delivered"
    );
    assert_eq!(
        feed.0.lock().unwrap().len(),
        1,
        "the dropped receiver is pruned"
    );
}

#[test]
fn html_fallback_rewrites_like_vite() {
    assert_eq!(
        html_fallback_candidate("nested/").as_deref(),
        Some("nested/index.html")
    );
    assert_eq!(
        html_fallback_candidate("about").as_deref(),
        Some("about.html")
    );
    assert_eq!(
        html_fallback_candidate("docs/intro").as_deref(),
        Some("docs/intro.html")
    );
    assert_eq!(
        html_fallback_candidate("about.html"),
        None,
        "an explicit html request is not rewritten"
    );
    assert_eq!(html_fallback_candidate(""), None);
    let mut h = HeaderMap::new();
    assert!(accepts_html_fallback(&h), "no Accept is */*");
    h.insert(
        header::ACCEPT,
        "text/html,application/xhtml+xml".parse().unwrap(),
    );
    assert!(accepts_html_fallback(&h));
    h.insert(header::ACCEPT, "*/*".parse().unwrap());
    assert!(accepts_html_fallback(&h));
    h.insert(header::ACCEPT, "application/json".parse().unwrap());
    assert!(
        !accepts_html_fallback(&h),
        "an API-style request never gets html"
    );
}

#[test]
fn client_js_is_rendered_from_server_hmr_options() {
    let tpl = "a=__HMR_PROTOCOL__;b=__HMR_HOSTNAME__;c=__HMR_PORT__;d=__HMR_PATH__;e=__HMR_ENABLE_OVERLAY__;f=__WS_TOKEN__;";
    let fc_off = resolve_forward_console(None, &oj_env::Knobs::default());
    assert_eq!(hmr_socket_path(None), "/__ws");
    assert_eq!(
        render_client_js(tpl, None, "/__ws", "tok", &fc_off),
        r#"a=null;b=null;c=null;d="/__ws";e=true;f="tok";"#
    );
    let opts = oj_config::HmrOptions {
        path: Some("hmr".into()),
        port: Some(24678),
        client_port: Some(443),
        host: Some("app.test".into()),
        protocol: Some("wss".into()),
        overlay: Some(false),
        timeout: None,
    };
    let path = hmr_socket_path(Some(&opts));
    assert_eq!(path, "/hmr", "a relative hmr.path is made absolute");
    assert_eq!(
        render_client_js(tpl, Some(&opts), &path, "tok", &fc_off),
        r#"a="wss";b="app.test";c=443;d="/hmr";e=false;f="tok";"#,
        "clientPort, not port, is what the browser dials"
    );
    let real = render_client_js(CLIENT_JS, None, "/__ws", "tok", &fc_off);
    assert!(
        !real.contains("__HMR_")
            && !real.contains("__WS_TOKEN__")
            && !real.contains("__FORWARD_CONSOLE__"),
        "no placeholder left"
    );
}

#[test]
fn forward_console_resolves_like_vite() {
    // Unset, no agent environment: off.
    let plain = oj_env::Knobs::default();
    let off = resolve_forward_console(None, &plain);
    assert_eq!(off["enabled"], false);
    // An agent environment or the editor gate turns the default on.
    let mut agent = oj_env::Knobs {
        agent_env: true,
        ..Default::default()
    };
    let on = resolve_forward_console(None, &agent);
    assert_eq!(on["enabled"], true);
    assert_eq!(on["unhandledErrors"], true);
    assert_eq!(on["logLevels"], serde_json::json!(["error", "warn"]));
    let gated = oj_env::Knobs {
        hmr_gate: true,
        ..Default::default()
    };
    assert_eq!(resolve_forward_console(None, &gated)["enabled"], true);
    // The env override beats detection; the config beats the env override.
    agent.forward_console = Some(false);
    assert_eq!(resolve_forward_console(None, &agent)["enabled"], false);
    let cfg = serde_json::json!(true);
    assert_eq!(resolve_forward_console(Some(&cfg), &agent)["enabled"], true);
    // The object form: unhandledErrors defaults true; empty levels alone
    // still enable via unhandledErrors, and levels alone enable too.
    let obj = serde_json::json!({ "logLevels": ["warn"] });
    let r = resolve_forward_console(Some(&obj), &plain);
    assert_eq!(r["enabled"], true);
    assert_eq!(r["unhandledErrors"], true);
    assert_eq!(r["logLevels"], serde_json::json!(["warn"]));
    let neither = serde_json::json!({ "unhandledErrors": false });
    assert_eq!(
        resolve_forward_console(Some(&neither), &plain)["enabled"],
        false
    );
}

#[test]
fn ws_token_is_demanded_only_from_browser_upgrades() {
    let mut h = HeaderMap::new();
    assert!(
        !ws_token_rejected(true, "t0k", &h, None),
        "no Origin: not a browser"
    );
    h.insert(header::ORIGIN, "http://localhost:5199".parse().unwrap());
    assert!(ws_token_rejected(true, "t0k", &h, None));
    assert!(ws_token_rejected(true, "t0k", &h, Some("token=nope")));
    assert!(!ws_token_rejected(true, "t0k", &h, Some("token=t0k")));
    assert!(!ws_token_rejected(true, "t0k", &h, Some("a=1&token=t0k")));
    assert!(
        !ws_token_rejected(false, "t0k", &h, None),
        "legacy.skipWebSocketTokenCheck"
    );
    let token = new_ws_token();
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(token, new_ws_token());
}

#[test]
fn stamp_import_url_marks_only_compilable_module_urls() {
    assert_eq!(stamp_import_url("/src/utils.ts", 5), "/src/utils.ts?t=5");
    assert_eq!(
        stamp_import_url("/src/r.tsx?tsr-shared=1", 5),
        "/src/r.tsx?tsr-shared=1&t=5"
    );
    assert_eq!(
        stamp_import_url("/src/utils.ts", 0),
        "/src/utils.ts",
        "unstamped module"
    );
    assert_eq!(
        stamp_import_url("/src/utils.ts?t=3", 5),
        "/src/utils.ts?t=3",
        "never doubled"
    );
    assert_eq!(
        stamp_import_url("/src/data.json", 5),
        "/src/data.json?t=5",
        "json is a module"
    );
    assert_eq!(
        stamp_import_url("/src/a.css?import", 5),
        "/src/a.css?import&t=5",
        "a CSS module's importer must fetch its new class exports"
    );
    assert_eq!(
        stamp_import_url("/src/a.css?inline", 5),
        "/src/a.css?inline&t=5"
    );
    assert_eq!(
        stamp_import_url("/src/hero.png?url", 5),
        "/src/hero.png?url&t=5",
        "an updated image's importer must fetch its new url"
    );
    assert_eq!(
        stamp_import_url("/@oj-deps/react.js", 5),
        "/@oj-deps/react.js"
    );
    assert_eq!(
        stamp_import_url("/@fs/x/node_modules/a/index.js", 5),
        "/@fs/x/node_modules/a/index.js"
    );
}

#[test]
fn proxy_regex_contexts_match_path_and_query_like_vite() {
    let entries: Vec<(String, oj_config::ProxyEntry)> = [
        ("/api", "http://a"),
        ("/api/v2", "http://a2"),
        ("^/re/.*", "http://re"),
        ("^/search\\?q=", "http://q"),
        ("^(", "http://bad"),
    ]
    .iter()
    .map(|(c, t)| (c.to_string(), oj_config::ProxyEntry::Target(t.to_string())))
    .collect();
    let regexes: Vec<Option<regex::Regex>> = entries
        .iter()
        .map(|(c, _)| proxy_context_regex(c))
        .collect();
    assert!(regexes[0].is_none() && regexes[2].is_some());
    assert!(
        regexes[4].is_none(),
        "an invalid pattern degrades to a never-matching prefix"
    );
    let pick =
        |url: &str| select_proxy(&entries, &regexes, url).map(|(_, e)| e.target().to_string());
    assert_eq!(pick("/api/x"), Some("http://a".into()));
    assert_eq!(
        pick("/api/v2/x"),
        Some("http://a2".into()),
        "longest prefix wins"
    );
    assert_eq!(pick("/re/anything?x=1"), Some("http://re".into()));
    assert_eq!(
        pick("/search?q=oj"),
        Some("http://q".into()),
        "regex sees the query"
    );
    assert_eq!(pick("/search"), None);
    assert_eq!(pick("/other"), None);
    assert!(proxy_context_matches("^/re/.*", "/re/x"));
    assert!(
        !proxy_context_matches("^/re/.*", "/api/re/x"),
        "anchored, not a substring"
    );
    assert!(proxy_context_matches("/api", "/api/x?y=1"));
}

#[test]
fn unresolved_import_error_points_at_the_specifier() {
    let root = Path::new("/app");
    let file = Path::new("/app/src/App.tsx");
    let source = "import { useState } from \"react\";\nimport { Later } from './Later';\n";
    let err = unresolved_import_error(root, file, source, "./Later");
    assert!(is_unresolved_import_error(&err));
    assert!(
        err.contains("src/App.tsx:2:24 Failed to resolve import \"./Later\" from \"src/App.tsx\". Does the file exist?"),
        "{err}"
    );
    assert!(
        err.contains("   2 | import { Later } from './Later';"),
        "{err}"
    );
    // The overlay's ErrorPayload lifts the location out of the message.
    let frame: serde_json::Value = serde_json::from_str(&error_frame(&err)).unwrap();
    assert_eq!(frame["err"]["id"], "src/App.tsx");
    assert_eq!(frame["err"]["loc"]["line"], 2);
    assert_eq!(frame["err"]["loc"]["column"], 24);
    // A specifier not found verbatim (plugin-rewritten source) still errors.
    let err = unresolved_import_error(root, file, "export {};\n", "./gone");
    assert!(
        err.contains("src/App.tsx:1:1 Failed to resolve import \"./gone\""),
        "{err}"
    );
    assert!(!is_unresolved_import_error(
        "compile error:\nparse error in x.tsx"
    ));
}

#[test]
fn preload_hints_name_the_exact_import_url() {
    let entries: HashMap<String, String> = [(
        "react.mjs".to_string(),
        "/@oj-deps/react.mjs?v=1d335a14".to_string(),
    )]
    .into();
    assert_eq!(
        preload_href("/src/main.tsx", &entries, false),
        "/src/main.tsx"
    );
    assert_eq!(
        preload_href("/src/a.css", &entries, false),
        "/src/a.css?import"
    );
    // A pre-bundled entry preloads under the URL importers name it by now,
    // not the session's base version, so the preload and the later import
    // hit one immutable cache entry and a rerun leaves no stale hint.
    assert_eq!(
        preload_href("/@oj-deps/react.mjs", &entries, false),
        "/@oj-deps/react.mjs?v=1d335a14"
    );
    assert_eq!(
        preload_href("/@oj-deps/chunk-AB12CD.mjs", &entries, false),
        "/@oj-deps/chunk-AB12CD.mjs",
        "dep files import chunks bare"
    );
    assert_eq!(
        preload_href("/@oj-deps/react.mjs", &HashMap::new(), false),
        "/@oj-deps/react.mjs",
        "no entry, no query"
    );
    assert_eq!(
        preload_href("/@id/6e6f6465", &entries, false),
        "/@id/6e6f6465",
        "stubs are unversioned"
    );
    // The bare asset url is the `<img src>`'s raw file; the import is `?url`.
    assert_eq!(
        preload_href("/src/assets/hero.png", &entries, false),
        "/src/assets/hero.png?url"
    );
    assert_eq!(
        preload_href("/src/logo.svg", &entries, false),
        "/src/logo.svg?url"
    );
    assert_eq!(
        preload_href("/src/logo.svg", &entries, true),
        "/src/logo.svg",
        "a transform plugin may componentize a bare svg import"
    );
    assert_eq!(
        preload_href("/node_modules/pkg/icon.png", &entries, false),
        "/node_modules/pkg/icon.png",
        "a dependency's asset import is not marked"
    );
}

#[test]
fn optional_peer_dep_resolves_to_a_lazy_error_stub() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let parent = root.join("node_modules/devtools-hook");
    std::fs::create_dir_all(parent.join("lib")).unwrap();
    std::fs::write(
        parent.join("package.json"),
        r#"{"name":"devtools-hook","peerDependencies":{"react":">=18","@scope/opt":"*","required-peer":"*"},
            "peerDependenciesMeta":{"react":{"optional":false},"@scope/opt":{"optional":true}}}"#,
    )
    .unwrap();
    // The lookup runs from the importing file's directory inside the package.
    let from = parent.join("lib");
    let url = optional_peer_dep_url(&root, &from, "@scope/opt/sub").expect("optional peer");
    assert!(url.starts_with(OPTIONAL_PEER_PREFIX), "{url}");
    let stub = optional_peer_dep_stub(url.strip_prefix(OPTIONAL_PEER_PREFIX).unwrap()).unwrap();
    assert!(stub.contains("Could not resolve \"${\"@scope/opt/sub\"}\" imported by \"${\"devtools-hook\"}\". Is it installed?"), "{stub}");
    assert!(
        stub.contains("throw new Error("),
        "errors when evaluated: {stub}"
    );
    // A declared but non-optional peer, an undeclared package, a builtin, and
    // an import from the app root itself all fall through to the normal error.
    assert!(
        optional_peer_dep_url(&root, &from, "react").is_none(),
        "optional: false"
    );
    assert!(
        optional_peer_dep_url(&root, &from, "required-peer").is_none(),
        "no meta"
    );
    assert!(optional_peer_dep_url(&root, &from, "unknown-pkg").is_none());
    assert!(optional_peer_dep_url(&root, &from, "node:fs").is_none());
    assert!(optional_peer_dep_url(&root, &from, "./local").is_none());
    std::fs::write(root.join("package.json"), r#"{"name":"app","peerDependencies":{"x":"*"},"peerDependenciesMeta":{"x":{"optional":true}}}"#).unwrap();
    assert!(
        optional_peer_dep_url(&root, &root, "x").is_none(),
        "root has no peer deps (Vite: basedir !== root)"
    );
    assert!(optional_peer_dep_stub("zz").is_none(), "malformed id");
}

#[test]
fn bare_import_unresolved_only_for_packages_nothing_answers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("node_modules/real")).unwrap();
    std::fs::write(
        root.join("node_modules/real/package.json"),
        r#"{"name":"real","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("node_modules/real/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"app","browser":{"mapped-off":false}}"#,
    )
    .unwrap();
    let resolver = OjResolver::new(root);
    // Vite's importAnalysis fails the importer for a package that is not
    // installed (typo, missing install), subpaths included.
    assert!(bare_import_unresolved(root, &resolver, "not-installed-pkg"));
    assert!(bare_import_unresolved(
        root,
        &resolver,
        "@scope/not-installed/sub"
    ));
    assert!(
        bare_import_unresolved(root, &resolver, "not-installed-pkg?url"),
        "query dropped"
    );
    // Everything something else answers is not an error here.
    assert!(
        !bare_import_unresolved(root, &resolver, "real"),
        "installed"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "node:fs"),
        "builtin stub"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "fs"),
        "builtin stub"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "virtual:thing"),
        "plugin virtual"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "\0resolved"),
        "plugin resolved id"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "data:text/javascript,export{}"),
        "data url"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "https://cdn.example/x.js"),
        "external url"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "./nope"),
        "relative is the other check"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "/src/nope.ts"),
        "root-absolute"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "@lingui/macro"),
        "shimmed macro entry"
    );
}

#[test]
fn relative_import_missing_only_for_unresolvable_relative_specifiers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/widgets")).unwrap();
    std::fs::write(root.join("src/util.ts"), "export const u = 1;\n").unwrap();
    std::fs::write(root.join("src/widgets/index.ts"), "export const w = 1;\n").unwrap();
    let resolver = OjResolver::new(root);
    let src = root.join("src");
    assert!(relative_import_missing(&src, &resolver, "./nope"));
    assert!(relative_import_missing(&src, &resolver, "../nope.js"));
    assert!(
        !relative_import_missing(&src, &resolver, "./util"),
        "extension probing"
    );
    assert!(!relative_import_missing(&src, &resolver, "./util.ts"));
    assert!(
        !relative_import_missing(&src, &resolver, "./widgets"),
        "directory index"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "./util.ts?worker&inline"),
        "query dropped"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "react"),
        "bare specifiers are not ours"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "/src/nope.ts"),
        "root-absolute is not ours"
    );
    // A miss cached by the resolver clears once the file exists.
    assert!(relative_import_missing(&src, &resolver, "./later"));
    std::fs::write(root.join("src/later.ts"), "export {};\n").unwrap();
    resolver.clear_cache();
    assert!(!relative_import_missing(&src, &resolver, "./later"));
}

#[test]
fn strip_hmr_timestamp_removes_only_the_t_param() {
    assert_eq!(strip_hmr_timestamp("/src/App.tsx"), "/src/App.tsx");
    assert_eq!(
        strip_hmr_timestamp("/src/App.tsx?t=1700000000000"),
        "/src/App.tsx"
    );
    assert_eq!(
        strip_hmr_timestamp("/a.tsx?tsr-shared=1&t=1700000000000"),
        "/a.tsx?tsr-shared=1"
    );
    assert_eq!(
        strip_hmr_timestamp("/a.tsx?t=1700000000000&tsr-shared=1"),
        "/a.tsx?tsr-shared=1"
    );
    // Only a 13-digit millisecond timestamp is oj's `t=` (Vite's timestampRE is
    // /\bt=\d{13}&?\b/). A short numeric or non-numeric `t=` is a user's own
    // query and must be kept.
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=9"), "/a.tsx?t=9");
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=123"), "/a.tsx?t=123");
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=abc"), "/a.tsx?t=abc");
    assert_eq!(strip_hmr_timestamp("/a.tsx?type=x"), "/a.tsx?type=x");
}

#[test]
fn resolve_host_maps_wildcards_to_all_interfaces() {
    let any: std::net::IpAddr = [0, 0, 0, 0].into();
    let local: std::net::IpAddr = [127, 0, 0, 1].into();
    assert_eq!(resolve_host(Some("true")), any);
    assert_eq!(resolve_host(Some("0.0.0.0")), any);
    assert_eq!(resolve_host(Some("::")), any);
    assert_eq!(resolve_host(Some("[::]")), any);
    assert_eq!(resolve_host(Some("localhost")), local);
    assert_eq!(resolve_host(None), local);
    let lan: std::net::IpAddr = [192, 168, 1, 5].into();
    assert_eq!(resolve_host(Some("192.168.1.5")), lan);
    assert_eq!(resolve_host(Some("bogus")), local);
}

#[test]
fn normalize_resolves_parent_components() {
    assert_eq!(
        normalize(Path::new("/a/b/../c/./d.ts")),
        PathBuf::from("/a/c/d.ts")
    );
}

#[test]
fn preview_html_fallback_prefers_page_index_then_sibling_then_root() {
    let dir = std::env::temp_dir().join(format!("oj-preview-fb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("index.html"), "root").unwrap();
    std::fs::write(dir.join("nested/index.html"), "nested").unwrap();
    std::fs::write(dir.join("about.html"), "about").unwrap();
    assert_eq!(
        preview_html_fallback(&dir, "nested/", true),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "nested", true),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "about", true),
        Some(dir.join("about.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "missing/route", true),
        Some(dir.join("index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "", true),
        Some(dir.join("index.html"))
    );
    // appType mpa: `/x.html` and `/x/index.html` still resolve, nothing falls back to the root page.
    assert_eq!(
        preview_html_fallback(&dir, "about", false),
        Some(dir.join("about.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "nested", false),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(preview_html_fallback(&dir, "missing/route", false), None);
    assert_eq!(
        preview_html_fallback(&dir, "", false),
        Some(dir.join("index.html"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_rel_maps_base_and_guards_traversal() {
    assert_eq!(preview_rel("/", "/").as_deref(), Some("index.html"));
    assert_eq!(
        preview_rel("/assets/x.js", "/").as_deref(),
        Some("assets/x.js")
    );
    assert_eq!(
        preview_rel("/app/assets/x.js", "/app/").as_deref(),
        Some("assets/x.js")
    );
    assert_eq!(preview_rel("/app/", "/app/").as_deref(), Some("index.html"));
    assert_eq!(preview_rel("/../etc/passwd", "/"), None);
}

#[test]
fn html_entry_src_normalizes_relative_and_excludes_external() {
    assert_eq!(
        html_entry_src("src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(
        html_entry_src("./src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(
        html_entry_src("/src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(html_entry_src("https://cdn/x.js"), None);
    assert_eq!(html_entry_src("//cdn/x.js"), None);
    assert_eq!(html_entry_src("data:text/js,1"), None);
}

#[test]
fn csp_nonce_stamps_scripts_styles_and_preload_links_once() {
    let html = "<html><head>\
        <link rel=\"stylesheet\" href=\"/a.css\">\
        <link rel=\"icon\" href=\"/i.png\">\
        <link rel=\"modulepreload\" href=\"/m.js\" />\
        <style>.a{color:red}</style>\
        <script nonce=\"keep\">if (1 < 2) {}</script>\
        </head><body><script type='module' src='/main.js'></script><p>a < b</p></body></html>";
    let out = inject_csp_nonce(html, "n0nce");
    assert!(
        out.contains("<link rel=\"stylesheet\" href=\"/a.css\" nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(
        out.contains("<link rel=\"icon\" href=\"/i.png\">"),
        "non-preload links untouched: {out}"
    );
    assert!(
        out.contains("<link rel=\"modulepreload\" href=\"/m.js\" nonce=\"n0nce\" />"),
        "{out}"
    );
    assert!(
        out.contains("<style nonce=\"n0nce\">.a{color:red}</style>"),
        "{out}"
    );
    assert!(
        out.contains("<script nonce=\"keep\">if (1 < 2) {}</script>"),
        "existing nonce kept: {out}"
    );
    assert!(
        out.contains("<script type='module' src='/main.js' nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(
        out.contains("<head>\n<meta property=\"csp-nonce\" nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(out.contains("<p>a < b</p>"), "{out}");
    assert_eq!(out.matches("csp-nonce").count(), 1);
    // Idempotent: a second pass adds nothing.
    assert_eq!(inject_csp_nonce(&out, "n0nce"), out);
}

#[test]
fn html_entries_read_module_scripts_with_any_quoting() {
    let root = std::env::temp_dir().join(format!("oj-html-entries-quoting-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("index.html"),
        "<html><body>\
         <script type='module' src='/src/a.ts'></script>\
         <script type=module src=src/b.ts></script>\
         <script type = \"module\" data-src=\"/ignored.js\" src = \"./src/c.ts\"></script>\
         <script src=\"/legacy.js\"></script>\
         <script type=\"module\">inline()</script>\
         </body></html>",
    )
    .unwrap();
    assert_eq!(
        html_entries(&root),
        vec![
            "/src/a.ts".to_string(),
            "/src/b.ts".to_string(),
            "/src/c.ts".to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn spa_navigation_falls_back_only_for_routes() {
    let html = {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml".parse().unwrap(),
        );
        h
    };
    let empty = HeaderMap::new();
    assert!(is_spa_navigation("dashboard", &empty));
    assert!(is_spa_navigation("users/123/edit", &empty));
    assert!(is_spa_navigation("report.v2", &html));
    assert!(!is_spa_navigation("missing.png", &empty));
    assert!(!is_spa_navigation("assets/app.js", &empty));
    assert!(!is_spa_navigation("@vite/client", &html));
    assert!(!is_spa_navigation("src/does-not-exist.tsx", &html));
    assert!(!is_spa_navigation("node_modules/react/missing.js", &html));
}

// The persisted child registry: a dead owner's identity-verified children are
// SIGKILLed and the file dropped; a live owner's file is untouched; a child
// whose recorded start time no longer matches (recycled pid) is never signaled.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn sweep_reaps_only_identified_children_of_dead_owners() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();

    // A pid that is certainly dead: spawn and fully reap a child.
    let dead = {
        let mut c = Command::new("true").spawn().unwrap();
        c.wait().unwrap();
        c.id()
    };
    let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut survivor = Command::new("sleep").arg("60").spawn().unwrap();
    let start = child_groups::proc_start_time(sleeper.id()).unwrap();
    let survivor_start = child_groups::proc_start_time(survivor.id()).unwrap();

    // Dead owner: the identified sleeper dies, the mismatched one survives.
    let stale = dir.path().join("1.json");
    std::fs::write(
        &stale,
        serde_json::json!({
            "owner": { "pid": dead, "start": 1 },
            "children": [
                { "pid": sleeper.id(), "start": start, "own_group": false },
                { "pid": survivor.id(), "start": survivor_start + 1, "own_group": false },
            ],
        })
        .to_string(),
    )
    .unwrap();
    // Live owner (us): its file and children must be left alone entirely.
    let own = std::process::id();
    let live = dir.path().join("2.json");
    std::fs::write(
        &live,
        serde_json::json!({
            "owner": { "pid": own, "start": child_groups::proc_start_time(own) },
            "children": [{ "pid": survivor.id(), "start": survivor_start, "own_group": false }],
        })
        .to_string(),
    )
    .unwrap();

    assert_eq!(child_groups::sweep_stale(dir.path()), 1);
    assert!(!stale.exists(), "dead owner's file must be removed");
    assert!(live.exists(), "live owner's file must be kept");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = sleeper.try_wait().unwrap() {
            assert!(
                !status.success(),
                "sleeper must die from the sweep's SIGKILL"
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "sleeper not killed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        survivor.try_wait().unwrap().is_none(),
        "mismatched identity must survive"
    );
    survivor.kill().unwrap();
    survivor.wait().unwrap();
}

// Start-time identity: stable for a live process, gone once the pid is reaped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn proc_start_time_identifies_live_processes() {
    let own = std::process::id();
    let a = child_groups::proc_start_time(own).unwrap();
    let b = child_groups::proc_start_time(own).unwrap();
    assert_eq!(a, b, "identity must be stable across reads");
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    assert_eq!(child_groups::proc_start_time(pid), None);
}

// An unreadable registry file is dropped without signaling anything; our own
// process's file is never touched by a sweep.
#[test]
fn sweep_drops_unreadable_files_and_skips_our_own() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("7.json");
    std::fs::write(&bad, "not json").unwrap();
    let ours = dir.path().join(format!("{}.json", std::process::id()));
    std::fs::write(&ours, "also not json").unwrap();
    assert_eq!(child_groups::sweep_stale(dir.path()), 0);
    assert!(!bad.exists(), "unreadable stale file must be dropped");
    assert!(ours.exists(), "our own file must never be swept");
}

// The registry round trip behind the deno_process hook: init writes our file,
// register records the child with its identity, unregister removes it, and
// kill_all persists the drained state. The one test allowed to init_registry
// (the process-global registry path is set once).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn child_registry_lifecycle_persists_and_drains() {
    let dir = tempfile::tempdir().unwrap();
    child_groups::init_registry(dir.path());
    let own_file = dir.path().join(format!("{}.json", std::process::id()));
    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&own_file).unwrap()).unwrap()
    };
    assert!(own_file.exists(), "init must write our registry file");
    assert_eq!(
        read()["owner"]["pid"].as_u64(),
        Some(std::process::id() as u64)
    );
    assert!(
        read()["owner"]["start"].as_u64().is_some(),
        "owner identity recorded"
    );

    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = sleeper.id();
    child_groups::register(pid, false);
    let doc = read();
    let kid = doc["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["pid"].as_u64() == Some(pid as u64))
        .expect("registered child must be persisted");
    assert_eq!(kid["own_group"].as_bool(), Some(false));
    assert_eq!(
        kid["start"].as_u64(),
        child_groups::proc_start_time(pid),
        "child identity recorded"
    );

    child_groups::unregister(pid);
    let no_entry = |doc: serde_json::Value| {
        doc["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["pid"].as_u64() != Some(pid as u64))
    };
    assert!(
        no_entry(read()),
        "unregister must remove the persisted entry"
    );

    child_groups::register(pid, false);
    assert!(
        child_groups::kill_all() >= 1,
        "kill_all must kill the sleeper"
    );
    assert!(
        !own_file.exists(),
        "a drained kill_all must remove our registry file"
    );
    child_groups::register(std::process::id(), false);
    assert!(
        own_file.exists(),
        "a register after kill_all must recreate the file"
    );
    child_groups::unregister(std::process::id());
    drop(sleeper.try_wait());
}

/// Start scripts import siblings by `./name`: each one must be written into
/// the same directory, or the script fails to load in the app.
#[test]
fn start_assets_sibling_imports_are_written() {
    let names: std::collections::HashSet<&str> = START_ASSETS.iter().map(|(n, _)| *n).collect();
    // Top-level import/export statements only (a script may write import
    // lines into code it generates, as string literals).
    let sibling =
        regex::Regex::new(r#"(?m)^(?:import|export)\b[^;]*?\bfrom\s*["']\./([^"'/]+)["']"#)
            .unwrap();
    for (name, source) in START_ASSETS {
        for cap in sibling.captures_iter(source) {
            assert!(
                names.contains(&cap[1]),
                "{name} imports ./{} but START_ASSETS does not write it",
                &cap[1]
            );
        }
    }
    assert!(names.contains("vite-globs.mjs"));
}

// @cloudflare/vite-plugin (1.55+) adds `**/.wrangler/**` to the resolved
// `server.watch.ignored` (wrangler-watch-ignore.ts), so miniflare's state
// writes on every SSR request never reach the watcher's consumers.
#[test]
fn the_cloudflare_plugin_ignore_covers_miniflare_state() {
    let root = Path::new("/repo/web");
    let pats = watch_ignored_patterns(root, &["**/.wrangler/**".to_string()]);
    for p in [
        "/repo/web/.wrangler/state/v3/observability/miniflare-wobs-trace-store/abc.sqlite",
        "/repo/web/.wrangler/state/v3/d1/x.sqlite-wal",
    ] {
        assert!(is_watch_ignored(&pats, root, Path::new(p)), "{p}");
    }
    assert!(!is_watch_ignored(
        &pats,
        root,
        Path::new("/repo/web/src/routes/index.tsx")
    ));
}

// A directory that appears reports its files as creates, as chokidar's
// addDir walk does, except where Vite's chokidar never looks: a fresh
// install's top-level node_modules is one Create event and must not report
// every package file. Nested dirs are walked too (a staged tree moved under
// a glob), and a symlink back up the tree does not loop.
#[test]
fn created_dirs_skip_what_vite_never_watches() {
    use notify::event::{CreateKind, ModifyKind, RenameMode};
    use notify::{Event, EventKind};
    let root = std::env::temp_dir().join(format!("oj-created-dirs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let file = |rel: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x").unwrap();
        p
    };
    file("node_modules/react/index.js");
    file(".git/HEAD");
    file("dist/a.js");
    file("generated/a.ts");
    file(".oj-cache/a");
    file("cache/a");
    file("src/pkg/node_modules/x/index.js");
    file("src/vendor/.git/HEAD");
    file("src/file.ts");
    let top = file("content/a.md");
    let moved_a = file("src/content/moved/a.json");
    let moved_dist = file("src/content/moved/dist/b.json");
    file("src/content/moved/node_modules/x/y.json");
    file("src/content/moved/.git/z");
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("src"), root.join("src/content/moved/up")).unwrap();
    let moved = root.join("src/content/moved");
    let outside = std::env::temp_dir();

    let ignored = watch_ignored_patterns(&root, &["generated".to_string()]);
    let cache = root.join("cache");
    let ev = |kind| {
        let mut ev = Event::new(kind);
        ev.paths = [
            "node_modules",
            ".git",
            "dist",
            "generated",
            ".oj-cache",
            "cache",
            "src/pkg/node_modules",
            "src/vendor/.git",
            "src/file.ts",
            "content",
            "src/content/moved",
        ]
        .iter()
        .map(|r| root.join(r))
        .chain([outside.clone()])
        .collect();
        ev
    };
    let want = vec![root.join("content"), moved.clone()];
    for kind in [
        EventKind::Create(CreateKind::Folder),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
    ] {
        assert_eq!(
            crate::watch::created_dirs(&ignored, &root, &cache, &ev(kind)),
            want,
            "{kind:?}"
        );
    }
    assert!(
        crate::watch::created_dirs(
            &ignored,
            &root,
            &cache,
            &ev(EventKind::Modify(ModifyKind::Any))
        )
        .is_empty(),
        "a write's parent-directory event is no new directory"
    );

    let mut found = Vec::new();
    crate::watch::scan_files(&moved, &mut found);
    found.sort();
    let mut expected = vec![moved_a, moved_dist];
    expected.sort();
    assert_eq!(
        found, expected,
        "a nested dist is watched; node_modules, .git, the symlink are not"
    );
    let mut found = Vec::new();
    crate::watch::scan_files(&root.join("content"), &mut found);
    assert_eq!(found, [top]);
    let _ = std::fs::remove_dir_all(&root);
}

// The boot scan is chokidar's initial watched set: every file the root watch
// covers, under the root's real path, never one inside an unwatched dir.
#[test]
fn watched_files_cover_what_the_root_watch_covers() {
    let root = std::env::temp_dir().join(format!("oj-watched-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let file = |rel: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x").unwrap();
        p
    };
    let real = |p: PathBuf| std::fs::canonicalize(p).unwrap();
    let want: std::collections::HashSet<PathBuf> = [
        real(file("index.html")),
        real(file("src/main.ts")),
        real(file("src/dist/kept.ts")),
        real(file(".wrangler/state/a.sqlite")),
    ]
    .into();
    for rel in [
        "node_modules/react/index.js",
        ".git/HEAD",
        "dist/a.js",
        ".oj-cache/a",
        "src/vendor/node_modules/x/index.js",
        "src/vendor/.git/HEAD",
    ] {
        file(rel);
    }
    assert_eq!(watched_files(&root), want);
    let _ = std::fs::remove_dir_all(&root);
}

// Zed's reading of a rescan: an inotify overflow has no path (the whole
// root), FSEvents' MustScanSubDirs names a subtree or an ancestor of the root
// (the whole root), and a subtree the root watch leaves out is no rescan.
#[test]
fn rescan_roots_read_overflow_and_must_scan_subdirs() {
    use notify::event::Flag;
    use notify::{Event, EventKind};
    let root = Path::new("/app");
    let cache = Path::new("/app/.oj-cache");
    let ignored = watch_ignored_patterns(root, &["generated".to_string()]);
    let rescan = |paths: &[&str]| {
        let mut ev = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        ev.paths = paths.iter().map(PathBuf::from).collect();
        crate::watch::rescan_roots(&ignored, root, cache, &ev)
    };
    assert_eq!(rescan(&[]), [PathBuf::from("/app")], "inotify overflow");
    assert_eq!(rescan(&["/"]), [PathBuf::from("/app")], "an ancestor");
    assert_eq!(
        rescan(&["/app/src/routes"]),
        [PathBuf::from("/app/src/routes")]
    );
    for skipped in [
        "/app/node_modules/react",
        "/app/src/pkg/node_modules",
        "/app/.git",
        "/app/dist",
        "/app/.oj-cache/start",
        "/app/generated",
        "/elsewhere",
    ] {
        assert!(rescan(&[skipped]).is_empty(), "{skipped}");
    }
    let mut plain = Event::new(EventKind::Other);
    plain.paths = vec![PathBuf::from("/app/src")];
    assert!(crate::watch::rescan_roots(&ignored, root, cache, &plain).is_empty());

    let collapsed = crate::watch::collapse_rescans(
        [
            "/app/src/a",
            "/app/src",
            "/app/src-b",
            "/app/lib/x",
            "/app/lib/x",
        ]
        .iter()
        .map(PathBuf::from)
        .collect(),
    );
    assert_eq!(
        collapsed,
        ["/app/lib/x", "/app/src", "/app/src-b"].map(PathBuf::from)
    );
}

// A rescan diffs the subtree against the watcher's snapshot, as Zed's does:
// unknown files are adds (a moved-in tree keeps its old mtimes), known paths
// gone are removals, known files whose mtime, inode or size moved are edits
// (a rename onto one brings its own inode), and directories it never knew,
// or knew under another inode or birth, are new and get re-armed. A chmod, untouched
// files, ignored paths and node_modules stay silent.
#[test]
fn rescan_changes_diff_against_the_known_files() {
    let base = std::env::temp_dir().join(format!("oj-rescan-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("app")).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let root = base.join("app");
    let write = |p: &Path, body: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    let old = SystemTime::now() - Duration::from_secs(3600);
    let age = |p: &Path| {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap()
    };
    for rel in [
        "src/edited.ts",
        "src/gone.ts",
        "src/replaced.ts",
        "src/untouched.ts",
        "src/chmod.ts",
        "src/recreated/a.ts",
        "generated/a.ts",
        "index.html",
    ] {
        let p = root.join(rel);
        write(&p, "x");
        age(&p);
    }
    let staged = base.join("staged/tree/inner.ts");
    write(&staged, "x");
    age(&staged);
    let cache = root.join(".oj-cache");
    let ignored = watch_ignored_patterns(&root, &["**/generated/**".to_string()]);
    let known = crate::watch::boot_snapshot(&ignored, &root, &cache);
    assert!(
        known.contains_key(&root.join("src")),
        "directories are known"
    );
    assert!(known.contains_key(&root.join("src/recreated")));
    assert!(
        !known.contains_key(&root.join("generated/a.ts")),
        "ignored paths stay out of the snapshot"
    );

    write(&root.join("src/edited.ts"), "yy");
    std::fs::remove_file(root.join("src/gone.ts")).unwrap();
    let tmp = root.join("src/.replaced.ts.tmp");
    write(&tmp, "z");
    age(&tmp);
    std::fs::rename(&tmp, root.join("src/replaced.ts")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("src/chmod.ts"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let added = root.join("src/added.ts");
    write(&added, "n");
    age(&added);
    std::fs::rename(staged.parent().unwrap(), root.join("src/moved")).unwrap();
    // Deleted and recreated under the same path, the freed inode possibly
    // handed back: a new birth, and no watch.
    std::fs::remove_dir_all(root.join("src/recreated")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    write(&root.join("src/recreated/a.ts"), "xy");
    write(&root.join("generated/b.ts"), "x");
    write(&root.join("node_modules/dep/index.js"), "x");
    write(&root.join("src/pkg/node_modules/a.js"), "x");

    let found =
        crate::watch::rescan_changes(std::slice::from_ref(&root), &ignored, &root, &cache, &known);
    assert_eq!(
        found.added,
        [root.join("src/added.ts"), root.join("src/moved/inner.ts")]
    );
    assert_eq!(
        found.changed,
        [
            root.join("src/edited.ts"),
            root.join("src/recreated/a.ts"),
            root.join("src/replaced.ts"),
        ]
    );
    assert_eq!(found.removed, [root.join("src/gone.ts")]);
    let mut new_dirs = vec![root.join("src/moved"), root.join("src/pkg")];
    // Only a filesystem that keeps birth times can tell a reused inode.
    if crate::watch::stamp(&root.join("src/recreated"))
        .unwrap()
        .born
        .is_some()
    {
        new_dirs.push(root.join("src/recreated"));
    }
    assert_eq!(found.new_dirs, new_dirs);

    // A subtree rescan reports only under it.
    let sub =
        crate::watch::rescan_changes(&[root.join("src/moved")], &ignored, &root, &cache, &known);
    assert_eq!(sub.added, [root.join("src/moved/inner.ts")]);
    assert_eq!(sub.new_dirs, [root.join("src/moved")]);
    assert!(sub.changed.is_empty() && sub.removed.is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

// Vite's CLI puts only the flags it was given in the inline config.
#[test]
fn cli_inline_carries_only_the_flags_passed() {
    assert_eq!(cli_inline(None, None), serde_json::json!({}));
    assert_eq!(
        cli_inline(Some(4000), None),
        serde_json::json!({ "server": { "port": 4000 } })
    );
    assert_eq!(
        cli_inline(Some(4000), Some("0.0.0.0")),
        serde_json::json!({ "server": { "port": 4000, "host": "0.0.0.0" } })
    );
}

// Vite's canExternalizeFile reads `path.extname`: the query is part of the
// extension, a leading dot is no extension, and a trailing dot is one.
#[test]
fn can_externalize_file_reads_the_extension_as_path_extname_does() {
    for path in [
        "/m/pkg/index.js",
        "/m/pkg/a.mjs",
        "/m/pkg/a.cjs",
        "/m/pkg/bin",
        "/m/pkg/.bin",
    ] {
        assert!(can_externalize_file(path), "{path}");
    }
    for path in [
        "/m/pkg/a.css?url",
        "/m/pkg/a.js?raw",
        "/m/pkg/a.ts",
        "/m/pkg/a.json",
        "/m/pkg/a.",
    ] {
        assert!(!can_externalize_file(path), "{path}");
    }
}

// Vite's bareImportRE and getNpmPackageName: a drive letter, a URL or a `#`
// import is never bare; an `@scope` with no name skips the external lists,
// and `noExternal` matches the package name, not a subpath.
#[test]
fn ssr_externalizes_reads_specifiers_as_vite_does() {
    for spec in ["pkg", "@s/pkg/x", "_p", "node:fs"] {
        assert!(crate::ssr::is_bare_import(spec), "{spec}");
    }
    for spec in ["./x", "/x", "#internal", "C:/x", "https://x/y", "é"] {
        assert!(!crate::ssr::is_bare_import(spec), "{spec}");
    }
    let externals = oj_config::SsrExternals {
        no_external: vec!["@s".into(), "pkg/sub".into()],
        ..Default::default()
    };
    let js = "/m/node_modules/x/index.js";
    assert!(crate::ssr::ssr_externalizes(&externals, "@s", js));
    assert!(crate::ssr::ssr_externalizes(&externals, "pkg/sub/x", js));
    // createIsExternal: an exact `ssr.external` id is external before the
    // bareImportRE check, unless it is relative or absolute.
    let exact = oj_config::SsrExternals {
        external: vec!["#db".into(), "./local".into(), "/abs".into()],
        ..Default::default()
    };
    let app = "/m/src/db.js";
    assert!(crate::ssr::ssr_externalizes(&exact, "#db", app));
    assert!(!crate::ssr::ssr_externalizes(&exact, "#other", app));
    assert!(!crate::ssr::ssr_externalizes(&exact, "./local", app));
    assert!(!crate::ssr::ssr_externalizes(&exact, "/abs", app));
}

// Expected strings are Vite's own svgToDataURL (plugins/asset.ts) run on the
// same inputs: URL-encoded markup, base64 when the svg holds text or nested
// quotes.
#[test]
fn svg_data_urls_match_vites_svg_to_data_url() {
    let cases = [
        (
            r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
            "data:image/svg+xml,%3csvg%20xmlns='http://www.w3.org/2000/svg'%3e%3c/svg%3e",
        ),
        (
            "  <svg viewBox=\"0 0 10 10\">\n  <path d=\"M0 0h10v10z\" fill=\"#f00\"/>\n  <g>  </g> 50%</svg>\n",
            "data:image/svg+xml,%3csvg%20viewBox='0%200%2010%2010'%3e%3cpath%20d='M0%200h10v10z'%20fill='%23f00'/%3e%3cg%3e%3c/g%3e%2050%25%3c/svg%3e",
        ),
        (
            "<svg><text>Hi</text></svg>",
            "data:image/svg+xml;base64,PHN2Zz48dGV4dD5IaTwvdGV4dD48L3N2Zz4=",
        ),
        (
            r#"<svg a="it's"></svg>"#,
            "data:image/svg+xml;base64,PHN2ZyBhPSJpdCdzIj48L3N2Zz4=",
        ),
    ];
    for (svg, want) in cases {
        assert_eq!(svg_to_data_url(svg.as_bytes()), want, "{svg}");
    }
}

// Vite's dev fileToDevUrl: an svg under assetsInlineLimit inlines unless
// `?no-inline` or a fragment says otherwise; every other URL takes
// `server.origin` and the base.
#[test]
fn dev_asset_urls_inline_small_svgs_and_prefix_origin_and_base() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("icon.svg");
    std::fs::write(&small, "<svg></svg>").unwrap();
    let big = dir.path().join("big.svg");
    std::fs::write(&big, format!("<svg>{}</svg>", "<g/>".repeat(1100))).unwrap();
    let lfs = dir.path().join("lfs.svg");
    std::fs::write(&lfs, "version https://git-lfs.github.com/spec/v1\n").unwrap();
    let png = dir.path().join("pic.png");
    std::fs::write(&png, [0u8; 4]).unwrap();

    let urls = DevAssetUrls::new(Some("https://cdn.example/"), Some("/app/"), None);
    assert_eq!(
        urls.file_url("client", &small, "/src/icon.svg?url")
            .unwrap(),
        "data:image/svg+xml,%3csvg%3e%3c/svg%3e"
    );
    assert_eq!(
        urls.file_url("client", &small, "/src/icon.svg?no-inline")
            .unwrap(),
        "https://cdn.example/app/src/icon.svg?no-inline"
    );
    assert_eq!(
        urls.file_url("client", &small, "/src/icon.svg#a").unwrap(),
        "https://cdn.example/app/src/icon.svg#a"
    );
    assert_eq!(
        urls.file_url("client", &big, "/src/big.svg").unwrap(),
        "https://cdn.example/app/src/big.svg"
    );
    assert_eq!(
        urls.file_url("client", &lfs, "/src/lfs.svg").unwrap(),
        "https://cdn.example/app/src/lfs.svg"
    );
    assert_eq!(
        urls.file_url("client", &png, "/src/pic.png?url&t=5")
            .unwrap(),
        "https://cdn.example/app/src/pic.png?t=5"
    );

    let limit = DevAssetUrls::new(None, Some("./"), Some(0));
    assert_eq!(
        limit.file_url("client", &small, "/src/icon.svg").unwrap(),
        "/src/icon.svg"
    );
    let external = DevAssetUrls::new(None, Some("https://cdn.example/sub/"), None);
    assert_eq!(external.join("/a.png"), "/sub/a.png");
}

/// A root with `dual` (`main` server.js, `browser` browser.js) and `cond`
/// (`exports` with a `custom` condition) installed.
fn resolve_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    write(
        "node_modules/dual/package.json",
        r#"{"name":"dual","main":"server.js","browser":"browser.js"}"#,
    );
    write("node_modules/dual/server.js", "");
    write("node_modules/dual/browser.js", "");
    write(
        "node_modules/cond/package.json",
        r#"{"name":"cond","exports":{"custom":"./custom.js","default":"./default.js"}}"#,
    );
    write("node_modules/cond/custom.js", "");
    write("node_modules/cond/default.js", "");
    dir
}

/// The file `spec` resolves to from the root, as `<package>/<file>`.
fn resolved(resolver: &OjResolver, root: &Path, spec: &str) -> String {
    let path = resolver.resolve(root, spec).unwrap();
    let parts: Vec<_> = path.iter().rev().take(2).collect();
    format!(
        "{}/{}",
        parts[1].to_string_lossy(),
        parts[0].to_string_lossy()
    )
}

fn config(json: &str) -> oj_config::OjConfig {
    serde_json::from_str(json).unwrap()
}

#[test]
fn a_server_environment_resolves_with_its_own_conditions_not_the_ssr_ones() {
    let dir = resolve_fixture();
    let root = dir.path().canonicalize().unwrap();
    let cfg =
        config(r#"{ "environments": { "worker": { "resolve": { "conditions": ["custom"] } } } }"#);
    let worker = environment_resolver(&root, &cfg, "worker", true, true, None);
    assert_eq!(resolved(&worker, &root, "cond"), "cond/custom.js");
    assert_eq!(resolved(&worker, &root, "dual"), "dual/server.js");
    let ssr = environment_resolver(&root, &cfg, "ssr", true, true, None);
    assert_eq!(resolved(&ssr, &root, "cond"), "cond/default.js");
}

#[test]
fn a_webworker_ssr_target_keeps_the_client_resolve_defaults() {
    let dir = resolve_fixture();
    let root = dir.path().canonicalize().unwrap();
    let cfg = config(r#"{ "ssr": { "target": "webworker" } }"#);
    let ssr = environment_resolver(&root, &cfg, "ssr", true, true, None);
    assert_eq!(resolved(&ssr, &root, "dual"), "dual/browser.js");
    let other = environment_resolver(&root, &cfg, "worker", true, true, None);
    assert_eq!(resolved(&other, &root, "dual"), "dual/server.js");
}

#[test]
fn reported_environment_options_win_over_the_config_and_defaults() {
    let dir = resolve_fixture();
    let root = dir.path().canonicalize().unwrap();
    // The config names the defaults explicitly; the reported lists still win.
    let cfg = config(
        r#"{ "environments": { "ssr": { "resolve": { "conditions": ["default"], "mainFields": ["main"] } } } }"#,
    );
    let reported = plugins::ReportedResolve {
        conditions: Some(vec!["custom".into()]),
        main_fields: Some(vec!["browser".into(), "module".into()]),
        ..Default::default()
    };
    let ssr = environment_resolver(&root, &cfg, "ssr", true, true, Some(&reported));
    assert_eq!(resolved(&ssr, &root, "cond"), "cond/custom.js");
    assert_eq!(resolved(&ssr, &root, "dual"), "dual/browser.js");
}

// Vite's createIsBuiltin over nodeLikeBuiltins: an exact builtinModules name
// or the `node:` / `bun:` namespace; a subpath of a builtin name is not one.
#[test]
fn ssr_builtins_match_vites_node_like_builtins() {
    for spec in [
        "events",
        "fs",
        "fs/promises",
        "node:events",
        "bun:sqlite",
        "_http_agent",
    ] {
        assert!(crate::ssr::is_ssr_builtin(spec), "{spec}");
    }
    for spec in ["events/x", "fs/extra", "eventsx", "@scope/fs"] {
        assert!(!crate::ssr::is_ssr_builtin(spec), "{spec}");
    }
}

// Vite's preAlias `matches`: the find itself or anything under `find/`.
#[test]
fn alias_finds_match_as_vites_pattern_matcher() {
    let finds = vec!["react".to_string(), "@/".to_string()];
    assert!(crate::ssr::matches_alias(&finds, "react"));
    assert!(crate::ssr::matches_alias(&finds, "react/jsx-runtime"));
    assert!(crate::ssr::matches_alias(&finds, "@/lib/x"));
    assert!(!crate::ssr::matches_alias(&finds, "react-dom"));
    assert!(!crate::ssr::matches_alias(&finds, "@scope/x"));
}

// Vite's encodeURIPath and decodedBase: the path encoded as encodeURI does,
// the query kept; the base decoded before it is joined.
#[test]
fn dev_asset_urls_encode_the_path_and_decode_the_base() {
    assert_eq!(
        encode_uri_path("/src/my pic.png?v=1 2"),
        "/src/my%20pic.png?v=1 2"
    );
    assert_eq!(encode_uri_path("/src/é.png"), "/src/%C3%A9.png");
    assert_eq!(
        encode_uri_path("data:image/png;base64,a b"),
        "data:image/png;base64,a b"
    );
    let urls = DevAssetUrls::new(None, Some("/my%20app/"), None);
    assert_eq!(urls.join("/a.png"), "/my app/a.png");
    assert_eq!(encode_uri_path(&urls.join("/a.png")), "/my%20app/a.png");
    assert_eq!(
        asset_export_url("/a.png?import&url&t=5&v=2"),
        "/a.png?t=5&v=2"
    );
}

// Vite's dev css url rewrite (rewriteCssUrls, doUrlReplace, urlResolver,
// rewriteCssImageSet): a public file is the raw base plus its decoded path,
// never inlined; a root file as fileToUrl names it (a small svg inlined) with
// its HMR `t=`; quotes kept as written, added only when the new url needs
// them; `@import`, `my-url(...)`, external, data and unresolvable urls as
// written; inside a string the url is rewritten without breaking it.
#[test]
fn served_sheet_urls_follow_vites_url_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::write(root.join("src/i.svg"), "<svg></svg>").unwrap();
    std::fs::write(root.join("src/p.png"), "png").unwrap();
    std::fs::write(root.join("src/my file.png"), "png").unwrap();
    std::fs::write(root.join("public/l.svg"), "<svg></svg>").unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(root.join("node_modules/pkg/img.png"), "png").unwrap();
    let urls = DevAssetUrls::new(Some("https://cdn"), Some("/my%20app/"), None);
    let stamp = |file: &std::path::Path| u64::from(file.ends_with("p.png")) * 7;
    let rewrite = |css: &str| {
        crate::css_urls::CssUrls {
            root: &root,
            public_dir: Some(&root.join("public")),
            urls: &urls,
            env: "client",
            stamp: &stamp,
            dir: Some(&root.join("src")),
            // A root-relative alias target, and an absolute one.
            alias: &|spec| {
                spec.strip_prefix("@/")
                    .map(|rest| format!("/src/{rest}"))
                    .or_else(|| {
                        spec.strip_prefix("~abs/")
                            .map(|rest| format!("{}/src/{rest}", root.display()))
                    })
            },
            // The package's file where its `exports` would point.
            package: &|_, spec| Some(root.join("node_modules").join(spec)).filter(|p| p.is_file()),
        }
        .rewrite(css)
        .unwrap()
        .0
    };
    let cases = [
        ("a{b:url(/src/i.svg)}", r#"a{b:url("data:image/svg+xml,%3csvg%3e%3c/svg%3e")}"#),
        ("a{b:url('/src/p.png')}", "a{b:url('https://cdn/my app/src/p.png?t=7')}"),
        ("a{b:url(/src/p.png#x)}", r#"a{b:url("https://cdn/my app/src/p.png?t=7#x")}"#),
        ("a{b:url(/src/my%20file.png)}", r#"a{b:url("https://cdn/my app/src/my file.png")}"#),
        // encodeURI changes `%20`, so doUrlReplace adds quotes.
        ("a{b:url(/l.svg)}", r#"a{b:url("https://cdn/my%20app/l.svg")}"#),
        ("@import url(/src/x.css);", "@import url(/src/x.css);"),
        ("a{b:my-url(/src/p.png)}", "a{b:my-url(/src/p.png)}"),
        ("a{b:url(/missing.png) url(https://x/y.png) url(data:a,b) url(#f)}", "a{b:url(/missing.png) url(https://x/y.png) url(data:a,b) url(#f)}"),
        (
            "a{b:image-set(\"/src/p.png\" 1x, url(/l.svg) 2x)}",
            "a{b:image-set(url(\"https://cdn/my app/src/p.png?t=7\") 1x, url(\"https://cdn/my%20app/l.svg\") 2x)}",
        ),
        // Relative to the sheet, and through an alias.
        ("a{b:url(./p.png)}", r#"a{b:url("https://cdn/my app/src/p.png?t=7")}"#),
        ("a{b:url(@/p.png)}", r#"a{b:url("https://cdn/my app/src/p.png?t=7")}"#),
        ("a{b:url(~abs/p.png)}", r#"a{b:url("https://cdn/my app/src/p.png?t=7")}"#),
        // A package's file, from a node_modules above the sheet.
        ("a{b:url(pkg/img.png)}", r#"a{b:url("https://cdn/my app/node_modules/pkg/img.png")}"#),
        // A `/*` inside a string opens no comment.
        (r#"a{c:"/*"} b{b:url(/src/p.png)} d{c:"*/"}"#, r#"a{c:"/*"} b{b:url("https://cdn/my app/src/p.png?t=7")} d{c:"*/"}"#),
        // An unclosed quote with a multibyte character keeps it whole.
        (r#"a{b:image-set("café)}"#, r#"a{b:image-set("café)}"#),
        // A comment is not a declaration.
        ("/* url(/src/p.png) */a{b:url(/src/p.png)}", r#"/* url(/src/p.png) */a{b:url("https://cdn/my app/src/p.png?t=7")}"#),
        // Only an ASCII punctuation escape unescapes (JS `\W`).
        (r"a{b:url(/src/my\ file.png)}", r#"a{b:url("https://cdn/my app/src/my file.png")}"#),
    ];
    for (css, want) in cases {
        assert_eq!(rewrite(css), want, "{css}");
    }
    // A url inside a string is rewritten as Vite rewrites it, unquoted when
    // the new url needs no quotes, so the string stays whole.
    let plain = DevAssetUrls::new(None, Some("/app/"), None);
    let out = crate::css_urls::CssUrls {
        root: &root,
        public_dir: Some(&root.join("public")),
        urls: &plain,
        env: "client",
        stamp: &|_| 0,
        dir: None,
        alias: &|_| None,
        package: &|_, _| None,
    }
    .rewrite(
        "a::before{content:\"url(/l.svg)\"} a{b:url(/src/p.png) url(/src/i.svg) url(/src/p.png)}",
    )
    .unwrap();
    assert_eq!(
        out.0,
        "a::before{content:\"url(/app/l.svg)\"} a{b:url(/app/src/p.png) url(\"data:image/svg+xml,%3csvg%3e%3c/svg%3e\") url(/app/src/p.png)}"
    );
    // The sheet's asset deps: each named root file once, no public file.
    assert_eq!(out.1, vec![root.join("src/i.svg"), root.join("src/p.png")]);
    // A malformed escape fails the sheet, as decodeURI throws.
    let malformed = crate::css_urls::CssUrls {
        root: &root,
        public_dir: None,
        urls: &plain,
        env: "client",
        stamp: &|_| 0,
        dir: None,
        alias: &|_| None,
        package: &|_, _| None,
    }
    .rewrite("a{b:url(/src/%E0%A4%A.png)}");
    assert!(malformed.is_err_and(|e| e.contains("URI malformed")));
}

// vite:worker's dev transform: a `worker_file` module of type module imports
// the env module (importAnalysis joins the base), classic loads it with
// importScripts under the base, ignore inlines it; any other request is none.
#[test]
fn worker_file_preludes_follow_vites_worker_transform() {
    let env = "/* env */";
    assert_eq!(
        worker_file_prelude("/src/w.js?worker_file&type=module", "/app", env).as_deref(),
        Some("import \"/app/@vite/env\"\n;\n")
    );
    assert_eq!(
        worker_file_prelude("/src/w.js?worker_file&type=classic", "", env).as_deref(),
        Some("importScripts(\"/@vite/env\")\n;\n")
    );
    assert_eq!(
        worker_file_prelude("/src/w.js?worker_file&type=ignore", "", env).as_deref(),
        Some("/* env */;\n")
    );
    assert_eq!(worker_file_prelude("/src/w.js?worker", "", env), None);
    assert_eq!(worker_file_prelude("/src/w.js", "", env), None);
}

// Vite's /@vite/env: the client's user defines, sorted, without
// `import.meta.env.*`, string values as written and others as JSON.
#[test]
fn the_vite_env_module_assigns_the_clients_user_defines() {
    let cfg: oj_config::OjConfig = serde_json::from_str(
        r#"{ "define": { "__B__": "\"b\"", "__A__": 1, "import.meta.env.X": "1" },
             "environments": { "client": { "define": { "app.flag": true } } } }"#,
    )
    .unwrap();
    let js = vite_env_module(&cfg);
    assert!(
        js.contains(r#"const defines = {"__A__": 1, "__B__": "b", "app.flag": true};"#),
        "{js}"
    );
    assert!(js.contains("target[segment] = defines[key];"), "{js}");
}

// A function `assetsInlineLimit` decides first; when it returns nothing the
// default 4096 bytes apply, not a configured number, and an environment's own
// number is its limit (Vite's shouldInline on environment.config.build). A
// function that throws fails the URL.
#[test]
fn a_function_inline_limit_decides_before_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("small.svg");
    std::fs::write(&small, "<svg></svg>").unwrap();
    let never = dir.path().join("never.svg");
    std::fs::write(&never, "<svg></svg>").unwrap();
    let big = dir.path().join("big.svg");
    std::fs::write(&big, format!("<svg>{}</svg>", "<g/>".repeat(1100))).unwrap();
    let boom = dir.path().join("boom.svg");
    std::fs::write(&boom, "<svg></svg>").unwrap();
    let mut urls = DevAssetUrls::new(None, None, Some(1)).with_environments(None, true);
    urls.decider = Some(InlineDecider(std::sync::Arc::new(
        |env: &str, file: &std::path::Path| match (env, file.file_name().and_then(|n| n.to_str())) {
            (_, Some("boom.svg")) => Err("assetsInlineLimit threw".into()),
            ("ssr", _) => Ok(InlineRule::Limit(0)),
            (_, Some("big.svg")) => Ok(InlineRule::Decide(true)),
            (_, Some("never.svg")) => Ok(InlineRule::Decide(false)),
            _ => Ok(InlineRule::Limit(4096)),
        },
    )));
    let client = |file: &std::path::Path, url: &str| urls.file_url("client", file, url);
    assert!(client(&big, "/big.svg")
        .unwrap()
        .starts_with("data:image/svg+xml"));
    assert_eq!(client(&never, "/never.svg").unwrap(), "/never.svg");
    assert!(client(&small, "/small.svg")
        .unwrap()
        .starts_with("data:image/svg+xml"));
    assert_eq!(
        client(&small, "/small.svg?no-inline").unwrap(),
        "/small.svg?no-inline"
    );
    assert_eq!(
        urls.file_url("ssr", &small, "/small.svg").unwrap(),
        "/small.svg"
    );
    assert_eq!(
        client(&boom, "/boom.svg").unwrap_err(),
        "assetsInlineLimit threw"
    );
}

// Vite's `[?&]inline\b` and `[?&]no-inline\b`: a parameter that starts with
// the word and continues with a non-word character counts.
#[test]
fn inline_params_match_as_vites_word_bounded_regexes() {
    for url in [
        "/a.png?inline",
        "/a.png?x&inline=1",
        "/a.png?inline-x",
        "/a.png?inline.x",
    ] {
        assert!(has_word_param(url, "inline"), "{url}");
    }
    for url in [
        "/a.png?inlinex",
        "/a.png?no-inline",
        "/a.png",
        "/a.png?x=inline",
    ] {
        assert!(!has_word_param(url, "inline"), "{url}");
    }
    assert_eq!(query_asset_kind(Some("inline-x")), Some("inline"));
}

// A prelude of N lines moves every generated line of the map down by N.
#[test]
fn a_prelude_shifts_the_source_map_down() {
    let map = r#"{"version":3,"mappings":"AAAA;AACA","sources":["w.js"]}"#;
    let shifted: serde_json::Value =
        serde_json::from_str(&crate::serve::shift_map_lines(map, 2)).unwrap();
    assert_eq!(shifted["mappings"], ";;AAAA;AACA");
}

// vite:worker's dev load always names a `?worker` import's worker `module`;
// only oj's own rewrite of `new Worker(new URL(...), opts)` passes a type.
#[tokio::test]
async fn a_user_type_parameter_does_not_change_a_workers_type() {
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path().join("w.js");
    std::fs::write(&w, "").unwrap();
    let urls = DevAssetUrls::default();
    let user = asset_module(&w, "/w.js?worker&type=classic", "worker", &urls, "client")
        .await
        .unwrap();
    assert!(
        user.contains(r#""/w.js?worker_file&type=module""#),
        "{user}"
    );
    let ours = asset_module(
        &w,
        "/w.js?worker&url&worker_type=classic",
        "url",
        &urls,
        "client",
    )
    .await
    .unwrap();
    assert!(
        ours.contains(r#""/w.js?worker_file&type=classic""#),
        "{ours}"
    );
}

// createIsBuiltin's RegExp entries: `i`, `m` and `s` keep their meaning, a
// sticky pattern matches only at the start, and `g`/`u` change nothing a
// single test sees.
#[test]
fn ssr_builtin_patterns_keep_their_regexp_flags() {
    let re = |source: &str, flags: &str| crate::ssr::js_regex(source, flags).unwrap();
    assert!(re("^NODE:", "i").is_match("node:fs"));
    assert!(!re("^NODE:", "").is_match("node:fs"));
    assert!(re("^fs$", "m").is_match("x\nfs"));
    assert!(re("a.b", "s").is_match("a\nb"));
    assert!(re("fs", "y").is_match("fs/promises"));
    assert!(!re("fs", "y").is_match("node:fs"));
    assert!(re("fs", "gu").is_match("node:fs"));
}

// A runner-backed ssr environment's builtins describe its runtime (workerd),
// so oj's Node consumer keeps Node's defaults rather than externalize only
// `cloudflare:*`.
#[test]
fn a_runner_backed_ssr_environment_keeps_nodes_builtins() {
    let cfg = |runner_backed: bool| oj_config::OjConfig {
        ssr: Some(serde_json::json!({
            "runnerBacked": runner_backed,
            "builtins": ["cloudflare:workers", { "regex": "^cloudflare:", "flags": "" }],
        })),
        ..Default::default()
    };
    assert!(crate::ssr::Builtin::ssr_list(&cfg(true)).is_none());
    let list = crate::ssr::Builtin::ssr_list(&cfg(false)).unwrap();
    assert!(list.iter().any(|b| b.matches("cloudflare:sockets")));
    assert!(!list.iter().any(|b| b.matches("node:fs")));
}

// @rollup/plugin-alias on a specifier Node will load: an exact find, a find
// followed by a subpath, a `/`-ended prefix; a longer name sharing the find's
// prefix is another package.
#[test]
fn an_aliased_external_hands_node_the_alias_target() {
    let aliases = [
        ("my-fs".to_string(), "node:fs".to_string()),
        ("lib/".to_string(), "real-lib/dist/".to_string()),
    ];
    let apply = |spec| crate::ssr::apply_alias(&aliases, spec);
    assert_eq!(apply("my-fs"), "node:fs");
    assert_eq!(apply("my-fs/promises"), "node:fs/promises");
    assert_eq!(apply("my-fsx"), "my-fsx");
    assert_eq!(apply("lib/a.js"), "real-lib/dist/a.js");
    assert_eq!(apply("react"), "react");
}

#[test]
fn the_route_tree_is_the_configured_one_or_the_generator_default() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        start_route_tree(dir.path()),
        dir.path().join("src/routeTree.gen.ts")
    );
    std::fs::write(
        dir.path().join("tsr.config.json"),
        r#"{ "generatedRouteTree": "./app/routes.gen.ts" }"#,
    )
    .unwrap();
    assert_eq!(
        start_route_tree(dir.path()),
        dir.path().join("app/routes.gen.ts")
    );
}
