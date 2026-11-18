// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Every way of importing a stylesheet ships the COMPILED css (Vite parity):
//! in dev `?url` is the URL of the compiled `?direct` text, a `<link>`
//! (sec-fetch-dest: style) request is compiled too, `?inline` is the compiled
//! text, and an aliased plain `.svg` import is a URL module even with no svgr
//! plugin, while an `<img>` request still gets the image. The build half is
//! oj's `css_url_inline_build`.

use axum::body::Body;
use axum::http::Request;
use regex::Regex;

use crate::{App, Response, Server, INDEX_HTML};

async fn get_dest(server: &Server, path: &str, dest: &str) -> Response {
    server
        .request(
            Request::get(path)
                .header("sec-fetch-dest", dest)
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

fn content_type(res: &Response) -> &str {
    res.headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn stylesheet_imports_serve_compiled_css_and_svg_is_a_url_module() {
    // Past assetsInlineLimit: a smaller svg is a data URL in Vite's dev.
    let logo = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">{}</svg>"#,
        r#"<rect width="4" height="4"/>"#.repeat(160)
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"cssurl-app","version":"1.0.0"}"#),
        ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
        ("src/b.css", ".b {\n  color: blue;\n}\n"),
        ("src/assets/logo.svg", &logo),
        (
            "src/main.js",
            "import aUrl from \"./a.scss?url\";\nimport bInline from \"./b.css?inline\";\nimport logo from \"@/assets/logo.svg\";\nwindow.__A = aUrl; window.__B = bInline; window.__LOGO = logo;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    // The alias target is absolute, so the config needs the root first.
    let src = serde_json::to_string(&app.root.join("src").to_string_lossy()).unwrap();
    let config = format!("export default {{ resolve: {{ alias: {{ \"@\": {src} }} }} }};\n");
    app.write(&[("vite.config.js", config.as_str())]);
    let server = app.serve().await;
    let has = |re: &str, s: &str| Regex::new(re).unwrap().is_match(s);

    let main = server.text("/src/main.js").await;
    assert!(
        main.contains(r#""/src/assets/logo.svg?url""#),
        "aliased svg not marked as an asset:\n{main}"
    );

    let a_url_mod = server.text("/src/a.scss?url").await;
    assert!(
        a_url_mod.contains(r#"export default "/src/a.scss?direct""#),
        "scss ?url module:\n{a_url_mod}"
    );
    let direct = server.get("/src/a.scss?direct").await;
    assert_eq!(direct.status, 200, "?direct:\n{}", direct.body);
    assert!(
        content_type(&direct).contains("text/css"),
        "?direct is css text: {}",
        content_type(&direct)
    );
    assert!(
        has(r"color:\s*red", &direct.body),
        "?direct is compiled sass:\n{}",
        direct.body
    );
    assert!(
        !direct.body.contains("$c"),
        "sass variable left in ?direct output:\n{}",
        direct.body
    );

    let linked = get_dest(&server, "/src/a.scss", "style").await;
    assert_eq!(linked.status, 200, "link request:\n{}", linked.body);
    assert!(
        has(r"color:\s*red", &linked.body),
        "link request not compiled:\n{}",
        linked.body
    );

    let b_inline = server.text("/src/b.css?inline").await;
    assert!(
        has(
            r#"export default "\.b\{color:blue\}|export default ".b \{"#,
            &b_inline
        ),
        "css ?inline in dev:\n{b_inline}"
    );

    // Fetched the way the browser fetches an import (no image dest).
    let logo_mod = get_dest(&server, "/src/assets/logo.svg", "script").await;
    assert_eq!(logo_mod.status, 200, "svg import:\n{}", logo_mod.body);
    assert!(
        logo_mod
            .body
            .contains(r#"export default "/src/assets/logo.svg""#),
        "svg import is not a URL module:\n{}",
        logo_mod.body
    );
    let img = get_dest(&server, "/src/assets/logo.svg", "image").await;
    assert_eq!(img.status, 200, "img request");
    assert!(
        content_type(&img).contains("image/svg+xml"),
        "img request still serves the svg: {}",
        content_type(&img)
    );
}

/// Not every `<link rel="stylesheet" href="/src/a.css">` request carries
/// `Sec-Fetch-Dest`: a browser sends fetch metadata only where the request's
/// url is potentially trustworthy (https, `localhost`, `127.0.0.1`, `[::1]`;
/// not a LAN address or a custom host name over plain http), Safari before
/// 16.4 sends none, and a proxy may drop it. Such a request is known only by
/// its `Accept`, as Vite's transformMiddleware knows it. A server-rendered
/// document links a `styles.css?url` import that way.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_request_without_fetch_metadata_gets_the_stylesheet() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"csslink-app","version":"1.0.0"}"#,
        ),
        ("src/a.css", ".a {\n  color: blue;\n}\n"),
        ("src/main.js", "import \"./a.css\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let get_accept = |accept: &'static str| {
        server.request(
            Request::get("/src/a.css")
                .header("accept", accept)
                .body(Body::empty())
                .unwrap(),
        )
    };

    let linked = get_accept("text/css,*/*;q=0.1").await;
    assert_eq!(linked.status, 200, "link request:\n{}", linked.body);
    assert!(
        content_type(&linked).contains("text/css"),
        "a stylesheet request is answered with {}:\n{}",
        content_type(&linked),
        linked.body
    );
    assert!(
        linked.body.contains(".a {") && !linked.body.contains("updateStyle"),
        "link request body:\n{}",
        linked.body
    );
    let vary: Vec<&str> = linked
        .headers
        .get_all("vary")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(vary.contains(&"accept"), "vary: {vary:?}");

    // The same url imported by a module is still the module.
    let imported = get_accept("*/*").await;
    assert!(
        content_type(&imported).contains("javascript") && imported.body.contains("updateStyle"),
        "import request ({}):\n{}",
        content_type(&imported),
        imported.body
    );
}

/// What tells a stylesheet's `<link>` from a module's import of it, case by
/// case. `Accept: text/css` is the sheet whatever the query says, as in
/// Vite's transformMiddleware (`removeImportQuery` runs before its `Accept`
/// test): `?import`, the `?t=` the client puts on a link it swaps after an
/// edit, a `/@fs/` sheet outside the root. `Sec-Fetch-Dest: style` alone is
/// the sheet too, where Vite answers the module (no browser sends that).
#[tokio::test(flavor = "multi_thread")]
async fn accept_and_fetch_dest_tell_a_link_from_an_import() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"csslink-cases","version":"1.0.0"}"#,
        ),
        ("src/a.css", ".a {\n  color: blue;\n}\n"),
        ("src/main.js", "import \"./a.css\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    // A sheet beside the root, outside it, reached by its `/@fs/` url.
    let name = format!("{}-shared", app.root.file_name().unwrap().to_string_lossy());
    let shared = app.root.with_file_name(name);
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("out.css"), ".out {\n  color: red;\n}\n").unwrap();
    let allow = serde_json::to_string(&shared.to_string_lossy()).unwrap();
    let config = format!("export default {{ server: {{ fs: {{ allow: [\".\", {allow}] }} }} }};\n");
    app.write(&[("vite.config.js", config.as_str())]);
    let outside = format!("/@fs{}", shared.join("out.css").display());
    let server = app.serve().await;

    const LINK: &str = "text/css,*/*;q=0.1";
    // (url, headers, whether the answer is the stylesheet)
    let cases: [(&str, &[(&str, &str)], bool); 9] = [
        ("/src/a.css?import", &[("accept", LINK)], true),
        ("/src/a.css?import", &[("accept", "*/*")], false),
        ("/src/a.css", &[], false),
        ("/src/a.css?t=123", &[("accept", LINK)], true),
        ("/src/a.css?t=123", &[("accept", "*/*")], false),
        (
            "/src/a.css",
            &[("sec-fetch-dest", "style"), ("accept", "*/*")],
            true,
        ),
        // A repeated header: every line counts.
        ("/src/a.css", &[("accept", "*/*"), ("accept", LINK)], true),
        (&outside, &[("accept", LINK)], true),
        (&outside, &[("accept", "*/*")], false),
    ];
    for (url, headers, sheet) in cases {
        let mut request = Request::get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let res = server.request(request.body(Body::empty()).unwrap()).await;
        assert_eq!(res.status, 200, "{url} {headers:?}:\n{}", res.body);
        let css = content_type(&res).contains("text/css") && !res.body.contains("updateStyle");
        let module = content_type(&res).contains("javascript") && res.body.contains("updateStyle");
        assert!(
            if sheet { css } else { module },
            "{url} {headers:?} should be the {}, got {}:\n{}",
            if sheet { "stylesheet" } else { "module" },
            content_type(&res),
            res.body
        );
    }
    let _ = std::fs::remove_dir_all(&shared);
}
