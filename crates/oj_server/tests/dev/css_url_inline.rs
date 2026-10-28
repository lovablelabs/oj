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
