// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The TanStack Start integration on the `oj` binary, in dev and in a build of
//! the Start fixture (see `start_app`): a server-rendered `/` wires together
//! routing, server functions, `import.meta.glob`, `?raw` / `?url`, svgr (bare
//! and `?react`), MDX, a plugin virtual module and load override, tsconfig
//! paths and package `imports` aliases, CommonJS deps, the Cloudflare context
//! and a non-default publicDir. Each check is its own test on its own copy of
//! the app, so one failure never hides the rest. The browser tests skip
//! without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::{app, build, Server, WAIT};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// What the fixture's `/` renders when each feature works, by feature.
const FEATURES: &[(&str, &str)] = &[
    ("#lib alias (shout)", "HOME!"),
    ("pre plugin load of an image query", "image-variant:64"),
    (
        "pre plugin resolveId of a bare query import",
        "fixture-sheet-url-ok",
    ),
    (
        "a plugin's file answer keeps its own query",
        "query-file-answer-ok",
    ),
    (
        "a plugin's claim wins over an installed package",
        "installed-package-claimed-ok",
    ),
    (
        "a plugin's claim wins over an existing relative file",
        "relative-file-claimed-ok",
    ),
    (
        "a /-prefixed query import reaches the pre hooks",
        "slash-claimed-ok",
    ),
    (
        "a claim of the bare id leaves the ?raw import",
        "bare-claim-kept-raw-ok",
    ),
    ("a plugin's remote URL answer", "remote-answer-ok"),
    ("server function ran", "server-fn-marker"),
    ("cloudflare wrangler var", "fixture-edition"),
    ("tsconfig paths alias", "ALIAS!"),
    ("commonjs dep facade", "[INTEROP]"),
    ("commonjs subpath (extensionless)", "[deep:ok]"),
    ("plugin virtual module", "fixture-virtual-ok"),
    (
        "plugin load override + buildStart + this.environment.config.consumer",
        "FRESH_via_buildStart_ssr-server",
    ),
    ("import.meta.glob", "Alpha Widget, Beta Widget"),
    (
        "import.meta.glob in .jsx",
        "jsxglob:Alpha Widget|Beta Widget",
    ),
    (
        "import.meta.glob in unclaimed .js",
        "jsglob:Alpha Widget|Beta Widget",
    ),
    (
        "import.meta.glob nested generic (.ts)",
        "tsglob:Alpha Widget|Beta Widget",
    ),
    ("?raw import", "raw-notes-marker"),
    ("svgr bare .svg component", "<rect"),
    ("svgr ?react component", "<polygon"),
    ("mdx module", "mdx-content-marker"),
    ("config define applied", "fixture-define-marker"),
    ("json named export", "json-named:Alpha Widget"),
];

/// React 18 and 19 hydration-mismatch messages: any of them in the browser's
/// output means the client threw the server's tree away.
const HYDRATION_MISMATCH: &[&str] = &[
    "Hydration failed because the server rendered",
    "didn't match the client",
    "tree will be regenerated on the client",
    "attributes of the server rendered HTML didn't match",
    "react.dev/link/hydration-mismatch",
    "Hydration failed because the initial UI does not match",
    "Text content did not match",
    "Expected server HTML to contain",
    "Did not expect server HTML",
    "An error occurred during hydration",
    "There was an error while hydrating",
];

fn get(server: &Server, path: &str) -> (u16, String) {
    server.request("GET", path, &[])
}

/// Every fixture feature in the render of `/`, plus `/about`, a publicDir
/// file and the stylesheet linked in the SSR head (no flash of unstyled
/// content).
fn assert_app(label: &str, server: &Server) {
    let (status, home) = get(server, "/");
    assert_eq!(status, 200, "{label}: /\n{home}\n{}", server.output());
    for (what, marker) in FEATURES {
        assert!(
            home.contains(marker),
            "{label}: missing {what} ({marker:?})\n{}",
            server.output()
        );
    }
    assert!(
        home.contains("jsenv:development:true") || home.contains("jsenv:production:true"),
        "{label}: plain .js module did not get import.meta.env (want jsenv:<mode>:true)"
    );
    // The plugin's load() overrides the on-disk stale.js, after buildStart
    // compiled its state: either failure leaves its marker in the render.
    assert!(
        !home.contains("STALE_ON_DISK"),
        "{label}: plugin load() did not override on-disk stale.js (fs read won)"
    );
    // A plugin that claims only `react/package.json` never takes its `?raw`
    // import (Vite resolves the import once, as written).
    assert!(
        !home.contains("bare-claim-took-the-raw-import"),
        "{label}: a claim of the bare id took the ?raw import"
    );
    assert!(
        !home.contains("BUILDSTART_SKIPPED"),
        "{label}: plugin load() ran before buildStart (compiled state missing)"
    );
    assert!(
        attribute_values(&home, "src=\"").any(|src| src.contains("hero") && src.ends_with(".png")),
        "{label}: missing ?url import (hero png src)"
    );
    let head = &home[..home.find("</head>").unwrap_or(home.len())];
    assert!(
        head.split("<link")
            .skip(1)
            .map(|tag| &tag[..tag.find('>').unwrap_or(tag.len())])
            .any(|tag| tag.contains("rel=\"stylesheet\"") && tag.contains(".css")),
        "{label}: stylesheet not linked in the SSR head"
    );

    let (status, about) = get(server, "/about");
    assert!(
        status == 200 && about.contains("about-page-marker"),
        "{label}: /about did not render (status {status})"
    );
    let (_, asset) = get(server, "/favicon.txt");
    assert!(
        asset.contains("public-dir-marker"),
        "{label}: publicDir asset not served"
    );
}

/// The values of every `<prefix>...\"` attribute in `html`.
fn attribute_values<'a>(html: &'a str, prefix: &'a str) -> impl Iterator<Item = &'a str> {
    html.split(prefix)
        .skip(1)
        .map(|rest| &rest[..rest.find('"').unwrap_or(rest.len())])
}

#[test]
fn dev_renders_every_fixture_feature() {
    let Some(app) = app("dev-features") else {
        return;
    };
    // The fixture's throwing resolveId hook (on /resolve-error) is armed for
    // this server alone.
    let dev = Server::oj_dev_with(&app.0, &[("OJ_FIXTURE_RESOLVE_THROWS", "1")]);
    assert_app("dev", &dev);
    // The remote URL a plugin answered reached the host as is: an `https:`
    // module it stubs, never fetched nor resolved as a package.
    assert!(
        dev.output()
            .contains("no SSR module for the 'https:' scheme"),
        "the remote answer was not handed to the host:\n{}",
        dev.output()
    );
    // A resolveId hook that throws fails its import, as Vite's import analysis
    // rethrows; the server keeps serving.
    let (status, page) = get(&dev, "/resolve-error");
    assert!(
        status >= 500 && page.contains("fixture resolveId hook threw"),
        "a throwing hook did not fail the import with its error (status {status}):\n{page}"
    );
    // Past a skipped hook, the import would go on as a package name.
    assert!(
        !page.contains("resolved-without-throwing") && !page.contains("Could not find package"),
        "the import went on past the throwing hook:\n{page}"
    );
    assert_eq!(get(&dev, "/about").0, 200, "{}", dev.output());
}

#[test]
fn build_renders_every_fixture_feature() {
    let Some(app) = app("build-features") else {
        return;
    };
    let root = &app.0;
    build(root);
    let dist = root.join("dist");
    for file in ["server.mjs", "server-bundle.mjs", "client"] {
        assert!(dist.join(file).exists(), "dist/{file} missing");
    }
    // A bare `process.env.X` in the client bundle crashes hydration.
    for entry in std::fs::read_dir(dist.join("client/assets"))
        .unwrap()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("client-") && name.ends_with(".js")) {
            continue;
        }
        let code = std::fs::read_to_string(entry.path()).unwrap();
        let bare = code.match_indices("process.env.").any(|(i, m)| {
            code[i + m.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        });
        assert!(!bare, "dist/client/assets/{name} reads a bare process.env");
    }
    let built = Server::node_server(root);
    assert_app("build", &built);
}

/// Every file under `publicDir` is served as it is, ahead of the router and
/// the SSR handler, as Vite's servePublicMiddleware runs ahead of its html
/// middlewares: an `index.html` (the router would answer the directory
/// document instead), a nested script, a binary, an extensionless file and a
/// percent-encoded name. A directory has no index (sirv with no extensions),
/// a `?url` request is the module, and a path no file owns still reaches
/// the app.
#[test]
fn dev_serves_public_dir_files_ahead_of_the_router() {
    let Some(app) = app("dev-public") else {
        return;
    };
    let public = app.0.join("public");
    let hub = b"<!DOCTYPE html><html><body><h1>hub-public-marker</h1></body></html>\n";
    let script = b"console.log('public-script-marker');\n";
    let clip: &[u8] = &[0, 0, 0, 0x18, b'f', b't', b'y', b'p', 0xff, 0xfe, 0, 0x80];
    let files: [(&str, &[u8]); 5] = [
        ("hub/index.html", hub),
        ("original/assets/app.js", script),
        ("clip.mp4", clip),
        ("with space.txt", b"spaced-public-marker"),
        ("noext", b"extensionless-public-marker"),
    ];
    for (rel, bytes) in files {
        let path = public.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let dev = Server::oj_dev(&app.0);
    let content_type = |headers: &[(String, String)]| {
        headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let expect = |path: &str, mime: &str, bytes: &[u8]| {
        let (status, headers, body) = dev.fetch("GET", path, &[]);
        assert_eq!(status, 200, "GET {path}\n{}", dev.output());
        assert!(
            content_type(&headers).starts_with(mime),
            "GET {path}: content-type {:?}, wanted {mime}",
            content_type(&headers)
        );
        assert_eq!(
            body,
            bytes,
            "GET {path}: served {:?}, not the public file",
            String::from_utf8_lossy(&body)
        );
    };
    expect("/hub/index.html", "text/html", hub);
    expect("/original/assets/app.js", "text/javascript", script);
    expect("/clip.mp4", "video/mp4", clip);
    expect("/with%20space.txt", "text/plain", b"spaced-public-marker");
    expect(
        "/noext",
        "application/octet-stream",
        b"extensionless-public-marker",
    );
    expect("/favicon.txt", "text/plain", b"public-dir-marker\n");
    // Vite's public middleware serves no directory index: `/hub/` is the
    // app's, whatever it answers.
    let (_, _, dir) = dev.fetch("GET", "/hub/", &[]);
    assert_ne!(dir, hub, "/hub/ served public/hub/index.html");
    // `?url` is the module the transform pipeline makes of a public file.
    let (status, module) = get(&dev, "/favicon.txt?url");
    assert!(
        status == 200 && module.contains("export default"),
        "/favicon.txt?url should be a module ({status}): {module}"
    );
    let (status, missing) = get(&dev, "/no-such-file.txt");
    assert!(
        missing.contains("<html") || missing.contains("<!DOCTYPE"),
        "an unowned dotted GET should still reach the app ({status}): {}",
        &missing[..missing.len().min(80)]
    );
}

/// The value of the response header `name` (lower-case), or "".
fn header_of(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// The fixture with a `server` block in its config: `server.headers`, cors
/// for any origin, the default host check (`allowedHosts` unset), a proxied
/// `/api` (its target refuses connections) and a plugin whose
/// configureServer middleware answers `/claimed/index.html`.
fn app_with_server_options(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let config = app.0.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let options = r#"export default defineConfig({
  server: {
    headers: { "X-Fixture-Header": "from-server-headers" },
    cors: true,
    proxy: { "/api": { target: "http://127.0.0.1:9", changeOrigin: true } },
  },
"#;
    let claims = r#"  plugins: [
    {
      name: "fixture-claims-a-public-url",
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url === "/claimed/index.html") {
            res.setHeader("content-type", "text/plain");
            res.end("claimed-by-the-plugin-middleware");
            return;
          }
          next();
        });
      },
    },
"#;
    assert!(
        source.contains("export default defineConfig({\n") && source.contains("  plugins: [\n")
    );
    let source = source
        .replacen("export default defineConfig({\n", options, 1)
        .replacen("  plugins: [\n", claims, 1);
    std::fs::write(&config, source).unwrap();
    Some(app)
}

/// Vite's middleware order, for a public file and for the app's own
/// responses (server/index.ts): cors and the host check come first, so a
/// host that is not allowed gets neither a public file, nor a document, nor
/// one of the server's own urls, and an allowed origin gets its cors header
/// on all of them; a plugin's configureServer middleware and the proxy come
/// before the public middleware, so a url either one owns is not the file;
/// and sirv sets `server.headers` on the file it serves.
#[test]
fn dev_public_files_run_inside_vites_middleware_order() {
    let Some(app) = app_with_server_options("dev-public-order") else {
        return;
    };
    let public = app.0.join("public");
    for (rel, bytes) in [
        ("hub/index.html", "<h1>hub-public-marker</h1>"),
        ("api/data.json", r#"{"from":"the-public-dir"}"#),
        ("claimed/index.html", "<h1>the-public-file</h1>"),
        ("unclaimed/index.html", "<h1>unclaimed-public-marker</h1>"),
    ] {
        let path = public.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let dev = Server::oj_dev(&app.0);
    let origin = ("Origin", "http://other.example".to_string());

    // server.headers and cors on a public file, dotted or a document-like one.
    for path in ["/favicon.txt", "/hub/index.html"] {
        let (status, headers, _) = dev.fetch("GET", path, std::slice::from_ref(&origin));
        assert_eq!(status, 200, "GET {path}\n{}", dev.output());
        assert_eq!(
            header_of(&headers, "x-fixture-header"),
            "from-server-headers",
            "GET {path}: server.headers are set on a public file (sirv's setHeaders)"
        );
        assert_eq!(
            header_of(&headers, "access-control-allow-origin"),
            "http://other.example",
            "GET {path}: cors comes before the public middleware"
        );
    }
    // cors is on the app's documents too (corsMiddleware is first for all);
    // server.headers are the files' only, not a framework's responses'.
    let (status, headers, _) = dev.fetch("GET", "/", std::slice::from_ref(&origin));
    assert_eq!(status, 200, "{}", dev.output());
    assert_eq!(
        header_of(&headers, "access-control-allow-origin"),
        "http://other.example"
    );
    assert_eq!(header_of(&headers, "x-fixture-header"), "");

    // The host check: nothing for a host that is not allowed, as Vite's
    // hostValidationMiddleware answers 403 before any other middleware.
    let evil = ("Host", "evil.example".to_string());
    for path in [
        "/favicon.txt",
        "/hub/index.html",
        "/",
        "/about",
        "/@oj-start/live-reload.js",
    ] {
        let (status, _, body) = dev.fetch("GET", path, std::slice::from_ref(&evil));
        assert_eq!(
            status,
            403,
            "GET {path} from a host that is not allowed: {}",
            String::from_utf8_lossy(&body[..body.len().min(120)])
        );
    }
    // The hosts Vite always allows still reach all of it.
    for host in ["localhost", "127.0.0.1", "app.localhost"] {
        let allowed = ("Host", format!("{host}:{}", dev.port));
        for path in ["/favicon.txt", "/"] {
            let (status, _, _) = dev.fetch("GET", path, std::slice::from_ref(&allowed));
            assert_eq!(status, 200, "GET {path} with Host {host}");
        }
    }

    // The proxy owns its prefix: the request goes to the target (which
    // refuses the connection), not to public/api/data.json.
    let (status, _, body) = dev.fetch("GET", "/api/data.json", &[]);
    assert!(
        status != 200 && !String::from_utf8_lossy(&body).contains("the-public-dir"),
        "a proxied url served the public file ({status})"
    );

    // A plugin's configureServer middleware answers its url ahead of the
    // public file; a url it passes on is the public file.
    let (status, claimed) = get(&dev, "/claimed/index.html");
    assert_eq!(status, 200, "{}", dev.output());
    assert_eq!(claimed, "claimed-by-the-plugin-middleware");
    let (status, unclaimed) = get(&dev, "/unclaimed/index.html");
    assert_eq!(status, 200, "{}", dev.output());
    assert_eq!(unclaimed, "<h1>unclaimed-public-marker</h1>");
}

/// A public file as Vite's sirv answers it (`dev: true, etag: true`): its
/// content type from the shared table (the TypeScript extensions as
/// JavaScript), a weak `W/"<size>-<mtime>"` validator answered with 304,
/// `Last-Modified`, and byte ranges: 206 with `Content-Range`, 416 past the
/// end. A browser plays and seeks a video through ranges.
#[test]
fn dev_public_files_answer_ranges_and_validators_as_sirv() {
    let Some(app) = app("dev-public-sirv") else {
        return;
    };
    let public = app.0.join("public");
    let clip: Vec<u8> = (0..300_000usize).map(|i| (i % 251) as u8).collect();
    let files: [(&str, &[u8]); 6] = [
        ("clip.mp4", &clip),
        ("clip.mov", b"quicktime-marker"),
        ("app.webmanifest", br#"{"name":"fixture"}"#),
        ("worker.ts", b"export const a: number = 1;\n"),
        ("Hub/Index.html", b"<h1>cased-public-marker</h1>"),
        ("a%2Fb.txt", b"encoded-separator-name"),
    ];
    for (rel, bytes) in files {
        let path = public.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    std::fs::create_dir_all(public.join("a")).unwrap();
    std::fs::write(public.join("a/b.txt"), "nested-file").unwrap();
    let dev = Server::oj_dev(&app.0);
    let size = clip.len();

    // Types: the shared asset table, not a short list.
    for (path, mime) in [
        ("/clip.mov", "video/quicktime"),
        ("/app.webmanifest", "application/manifest+json"),
        ("/worker.ts", "text/javascript"),
        ("/Hub/Index.html", "text/html"),
    ] {
        let (status, headers, _) = dev.fetch("GET", path, &[]);
        assert_eq!(status, 200, "GET {path}\n{}", dev.output());
        assert!(
            header_of(&headers, "content-type").starts_with(mime),
            "GET {path}: content-type {:?}, wanted {mime}",
            header_of(&headers, "content-type")
        );
    }
    // The TypeScript file is served as written, not compiled.
    let (_, _, worker) = dev.fetch("GET", "/worker.ts", &[]);
    assert_eq!(worker, b"export const a: number = 1;\n");

    // The whole file, with its validators.
    let (status, headers, body) = dev.fetch("GET", "/clip.mp4", &[]);
    assert_eq!(status, 200);
    assert_eq!(body, clip);
    assert_eq!(header_of(&headers, "accept-ranges"), "bytes");
    assert_eq!(header_of(&headers, "content-length"), size.to_string());
    assert_eq!(header_of(&headers, "cache-control"), "no-cache");
    assert!(header_of(&headers, "last-modified").ends_with(" GMT"));
    let etag = header_of(&headers, "etag");
    assert!(
        etag.starts_with(&format!("W/\"{size}-")),
        "sirv's weak size-mtime validator: {etag}"
    );

    // Ranges.
    let range = |spec: &str| dev.fetch("GET", "/clip.mp4", &[("Range", spec.to_string())]);
    let (status, headers, body) = range("bytes=100-199");
    assert_eq!(status, 206);
    assert_eq!(
        header_of(&headers, "content-range"),
        format!("bytes 100-199/{size}")
    );
    assert_eq!(header_of(&headers, "content-length"), "100");
    assert_eq!(body, &clip[100..200]);
    let (status, headers, body) = range("bytes=299000-");
    assert_eq!(status, 206);
    assert_eq!(
        header_of(&headers, "content-range"),
        format!("bytes 299000-{}/{size}", size - 1)
    );
    assert_eq!(body, &clip[299_000..]);
    let (status, _, body) = range("bytes=-500");
    assert_eq!(status, 206);
    assert_eq!(body, &clip[size - 500..]);
    let (status, headers, _) = range(&format!("bytes={size}-"));
    assert_eq!(status, 416);
    assert_eq!(
        header_of(&headers, "content-range"),
        format!("bytes */{size}")
    );

    // The validator: 304 and no body.
    let (status, _, body) = dev.fetch("GET", "/clip.mp4", &[("If-None-Match", etag)]);
    assert_eq!(status, 304);
    assert!(body.is_empty());
    // HEAD: the headers of the GET, no body.
    let (status, headers, body) = dev.fetch("HEAD", "/clip.mp4", &[]);
    assert_eq!(status, 200);
    assert_eq!(header_of(&headers, "content-length"), size.to_string());
    assert!(body.is_empty());

    // Vite matches the url against the names in the dir: another case is
    // not the file, even where the file system would open it.
    let (_, _, cased) = dev.fetch("GET", "/hub/index.html", &[]);
    assert_ne!(cased, b"<h1>cased-public-marker</h1>");
    // decodeURI keeps an encoded separator: `/a%2Fb.txt` is the file of
    // that name, not a/b.txt.
    let (status, _, encoded) = dev.fetch("GET", "/a%2Fb.txt", &[]);
    assert_eq!(status, 200);
    assert_eq!(encoded, b"encoded-separator-name");
}

/// A dotted GET no static file owns reaches the SSR handler (dotted route
/// params, robots.txt-style server routes), static files still win, and
/// requests run concurrently.
#[test]
fn dev_routes_dotted_gets_to_ssr_and_serves_concurrently() {
    let Some(app) = app("dev-routing") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, dotted) = get(&dev, "/users/john.doe");
    assert!(
        status == 200 && dotted.contains("user-john.doe-marker"),
        "/users/john.doe did not SSR (status {status})\n{}",
        dev.output()
    );
    let (status, public) = get(&dev, "/favicon.txt");
    assert!(
        status == 200 && public.contains("public-dir-marker") && !public.contains("<html"),
        "a publicDir file must still be served statically ({status})"
    );
    let (status, missing) = get(&dev, "/no-such-file.txt");
    assert!(
        missing.contains("<html") || missing.contains("<!DOCTYPE"),
        "an unowned dotted GET should reach the app's SSR handler, got {status}: {}",
        &missing[..missing.len().min(80)]
    );
    let statuses: Vec<u16> = std::thread::scope(|scope| {
        let calls: Vec<_> = (1..=6)
            .map(|i| {
                let dev = &dev;
                scope.spawn(move || get(dev, if i % 2 == 1 { "/" } else { "/about" }).0)
            })
            .collect();
        calls.into_iter().map(|c| c.join().unwrap()).collect()
    });
    assert!(
        statuses.iter().all(|s| *s == 200),
        "concurrent SSR requests: {statuses:?}"
    );
}

/// The SSR pipeline inlines a source map into each transformed module, so
/// stack traces point at the source. A module without `import.meta.glob`:
/// glob expansion has no faithful origin, so those modules skip the map.
#[test]
fn dev_inlines_ssr_source_maps() {
    let Some(app) = app("dev-source-maps") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let id = app.0.join("src/lib/format.ts");
    let (status, code) = get(
        &dev,
        &format!("/@ssr-module?id={}", percent_encode(&id.to_string_lossy())),
    );
    assert!(
        status == 200
            && code.contains("sourceMappingURL=data:application/json")
            && code.contains(";base64,"),
        "the SSR pipeline did not inline a source map into the transformed module ({status})"
    );
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A plugin whose buildStart throws never takes the dev server down: oj logs
/// it with the plugin's name and keeps serving, and only that plugin's own
/// output degrades (the fixture plugin throws under OJ_TEST_BUILDSTART_THROW,
/// and its load() falls back to a marker).
#[test]
fn dev_logs_and_skips_a_throwing_build_start() {
    let Some(app) = app("dev-build-start") else {
        return;
    };
    let dev = Server::oj_dev_with(&app.0, &[("OJ_TEST_BUILDSTART_THROW", "1")]);
    let (status, home) = get(&dev, "/");
    assert_eq!(
        status,
        200,
        "the server did not stay up after a throwing buildStart\n{}",
        dev.output()
    );
    assert!(
        home.contains("BUILDSTART_SKIPPED"),
        "expected the plugin's degraded load fallback after buildStart threw"
    );
    assert!(
        dev.logs("plugin \"fixture-fresh-module\" buildStart failed"),
        "buildStart failure not logged with attribution\n{}",
        dev.output()
    );
}

/// Readiness probes identify oj by the `oj-hmr` subprotocol echo on the Start
/// HMR socket, the trust mechanism of Vite's `vite-hmr` echo: an offered
/// `oj-hmr` comes back selected, a client offering none gets none, and a
/// foreign offer is never selected (stricter than ws's echo-anything).
#[tokio::test(flavor = "multi_thread")]
async fn dev_hmr_socket_echoes_only_the_oj_hmr_subprotocol() {
    let Some(app) = app("dev-hmr-identity") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let url = format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port);
    let dial = |protocol: Option<&'static str>| {
        let url = url.clone();
        async move {
            let mut request = url.into_client_request().unwrap();
            if let Some(protocol) = protocol {
                request
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", protocol.parse().unwrap());
            }
            tokio_tungstenite::connect_async(request)
                .await
                .map(|(_, response)| {
                    response
                        .headers()
                        .get("sec-websocket-protocol")
                        .map(|v| v.to_str().unwrap().to_string())
                })
        }
    };
    // The page may still be booting: the socket comes up with the server.
    let deadline = tokio::time::Instant::now() + WAIT;
    let echoed = loop {
        match dial(Some("oj-hmr")).await {
            Ok(protocol) => break protocol,
            Err(e) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "hmr socket dial failed: {e}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    assert_eq!(
        echoed.as_deref(),
        Some("oj-hmr"),
        "oj-hmr subprotocol not echoed"
    );
    let bare = dial(None).await.expect("bare dial");
    assert_eq!(bare, None, "a bare dial got a subprotocol");
    // A client that offered one must get it back or fail the handshake.
    let foreign = dial(Some("vite-hmr")).await;
    assert!(
        !matches!(&foreign, Ok(Some(p)) if p == "vite-hmr"),
        "a foreign subprotocol was selected: {foreign:?}"
    );
}

/// The page hydrates: the client graph serves, the runtime comes alive, and
/// no error or hydration mismatch reaches the browser's output. `/` mounts a
/// client-only probe and a counter whose click proves handlers attached;
/// `/about` is static and must load clean.
#[tokio::test(flavor = "multi_thread")]
async fn dev_hydrates_in_the_browser() {
    let Some(app) = app("dev-hydration") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let (status, home) = get(&dev, "/");
    assert!(status == 200 && home.contains("HOME!"), "/ SSR ({status})");
    let page = Page::open(&dev.url("/")).await;
    let mounted = page
        .poll(
            "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
            WAIT,
        )
        .await;
    assert!(
        mounted,
        "the client runtime did not mount; page errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        dev.output()
    );
    page.eval::<serde_json::Value>("document.querySelector('[data-testid=\"counter\"]').click()")
        .await;
    page.wait_for(
        "document.querySelector('[data-testid=\"counter\"]')?.textContent.includes('count: 1')",
        WAIT,
    )
    .await;
    assert_clean(&page, "/");
    page.close().await;

    let (status, about) = get(&dev, "/about");
    assert!(
        status == 200 && about.contains("about-page-marker"),
        "/about SSR ({status})"
    );
    let page = Page::open(&dev.url("/about")).await;
    page.wait_for("document.readyState === 'complete'", WAIT)
        .await;
    // Failed module loads are reported a beat after the load event.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_clean(&page, "/about");
    page.close().await;
}

/// No uncaught error, no console error (a failed module load included), and
/// no hydration mismatch in errors or warnings. The favicon 404 is noise.
fn assert_clean(page: &Page, path: &str) {
    let errors = page.errors();
    let console: Vec<String> = page
        .console()
        .into_iter()
        .filter(|line| !line.contains("favicon.ico"))
        .collect();
    let mismatches: Vec<&String> = errors
        .iter()
        .chain(&console)
        .filter(|line| HYDRATION_MISMATCH.iter().any(|sig| line.contains(sig)))
        .collect();
    assert!(
        mismatches.is_empty(),
        "{path}: hydration mismatch: {mismatches:?}"
    );
    assert!(errors.is_empty(), "{path}: page errors: {errors:?}");
    let console_errors: Vec<&String> = console.iter().filter(|l| l.starts_with("error:")).collect();
    assert!(
        console_errors.is_empty(),
        "{path}: console errors: {console_errors:?}"
    );
}
