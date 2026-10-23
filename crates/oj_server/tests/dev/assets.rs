// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Asset urls in dev, as Vite serves them: a bare `import url from "./pic.svg"`
//! is the file's root-relative path (`/src/pic.svg`), `new URL("./pic.svg",
//! import.meta.url)` resolves to that same file, and the url the page got
//! fetches with 200.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

/// A 1x1 and a 2x2 PNG: real binary images, told apart by `naturalWidth`.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0xf8, 0x0f,
    0x00, 0x01, 0x03, 0x01, 0x00, 0x08, 0x89, 0xc2, 0xec, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];
const PNG_2X2: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a,
    0x73, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x44, 0x0c, 0x10, 0x0a, 0x00, 0x1f, 0xee, 0x03, 0xfd, 0x8b, 0x5f, 0x14, 0xd4, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test(flavor = "multi_thread")]
async fn svg_import_and_new_url_resolve_to_the_served_file() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"asset-app","version":"1.0.0"}"#),
        (
            "src/pic.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#,
        ),
        (
            "src/main.js",
            "import picUrl from \"./pic.svg\";\nconst asUrl = new URL(\"./pic.svg\", import.meta.url);\nwindow.__PIC = picUrl;\nwindow.__ASURL = asUrl.href;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        "window.__PIC !== undefined && window.__ASURL !== undefined",
        WAIT,
    )
    .await;

    let pic: String = page.eval("window.__PIC").await;
    let as_url: String = page.eval("window.__ASURL").await;
    // Fetched from the page, so it is the url the browser resolves.
    let status: u16 = page.eval("fetch(window.__PIC).then((r) => r.status)").await;
    assert_eq!(pic, "/src/pic.svg", "bare import url");
    assert!(as_url.ends_with("/src/pic.svg"), "new URL: {as_url}");
    assert_eq!(status, 200, "asset fetch of {pic}");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn png_import_preloads_its_url_module_not_the_image() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"png-preload","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "import hero from \"./hero.png\";\nwindow.__HERO = hero;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/hero.png"), PNG_1X1).unwrap();
    let server = app.serve().await;

    // The preload list comes from the eager crawl, which runs in the background.
    let deadline = Instant::now() + WAIT;
    let html = loop {
        let html = server.text("/").await;
        if html.contains("hero.png") || Instant::now() > deadline {
            break html;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // `/src/hero.png` is the `<img src>`'s raw file: preloading it as a module
    // only works while every hop forwards `Sec-Fetch-Dest`.
    assert!(
        html.contains(r#"<link rel="modulepreload" href="/src/hero.png?url" />"#),
        "{html}"
    );
    assert!(!html.contains(r#"href="/src/hero.png""#), "{html}");

    for (dest, mime) in [("script", "text/javascript"), ("image", "image/png")] {
        let req = Request::get("/src/hero.png")
            .header("sec-fetch-dest", dest)
            .body(Body::empty())
            .unwrap();
        let res = server.request(req).await;
        assert_eq!(res.status, 200, "{dest}");
        assert_eq!(res.headers["content-type"], mime, "{dest}");
        assert_eq!(res.headers["vary"], "sec-fetch-dest", "{dest}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overwritten_png_hot_updates_its_importer() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"png-hmr","version":"1.0.0"}"#),
        ("src/main.js", "import \"./view.js\";\n"),
        (
            "src/view.js",
            "import hero from \"./hero.png\";\n\
             const img = document.querySelector(\"img\") ?? document.body.appendChild(document.createElement(\"img\"));\n\
             img.src = hero;\n\
             import.meta.hot.accept();\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/hero.png"), PNG_1X1).unwrap();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r#"document.querySelector("img")?.naturalWidth === 1"#, WAIT)
        .await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    // An image generator rewriting the file in place.
    std::fs::write(app.root.join("src/hero.png"), PNG_2X2).unwrap();
    page.wait_for(r#"document.querySelector("img")?.naturalWidth === 2"#, WAIT)
        .await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "page was reloaded"
    );
    let src: String = page
        .eval(r#"document.querySelector("img").getAttribute("src")"#)
        .await;
    assert!(src.starts_with("/src/hero.png?t="), "new image url: {src}");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
