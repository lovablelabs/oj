// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An asset a `<link>`-loaded sheet names is one of the sheet's deps (Vite's
//! watch files): editing it sends a `css-update` for the sheet, whose
//! recompile names the asset with its new `t=`, and a sheet naming assets is
//! never served from the persistent cache with an asset as it was.

use axum::body::Body;
use axum::http::Request;

use crate::listen::{close, frame_matching};
use crate::{App, Server};

const INDEX: &str = r#"<!doctype html><html><head><link rel="stylesheet" href="/src/style.css"></head><body></body></html>"#;

async fn linked_sheet(server: &Server) -> String {
    let res = server
        .request(
            Request::get("/src/style.css")
                .header("sec-fetch-dest", "style")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.body);
    res.body
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linked_sheets_asset_edit_sends_a_css_update_for_the_sheet() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-asset-hmr","version":"1.0.0"}"#,
        ),
        (
            "src/style.css",
            ".a { background: url(./bg.png) } .b { background: url(./dot.svg) }\n",
        ),
        ("src/bg.png", "png-v1"),
        ("src/dot.svg", "<svg><g/></svg>"),
        ("index.html", INDEX),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    let first = linked_sheet(&server).await;
    assert!(first.contains(r#"url("/src/bg.png")"#), "{first}");
    app.write(&[("src/bg.png", "png-v2")]);
    let frame = frame_matching(&mut socket, "css-update for the sheet", |f| {
        f["type"] == "update"
            && f["updates"].as_array().is_some_and(|u| {
                u.iter()
                    .any(|u| u["type"] == "css-update" && u["path"] == "/src/style.css")
            })
    })
    .await;
    assert!(
        !frame["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["type"] == "js-update"),
        "{frame}"
    );
    let second = linked_sheet(&server).await;
    assert!(second.contains("/src/bg.png?t="), "{second}");
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sheet_naming_assets_is_not_served_stale_from_the_persistent_cache() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-asset-cache","version":"1.0.0"}"#,
        ),
        ("src/style.css", ".b { background: url(./dot.svg) }\n"),
        ("src/dot.svg", "<svg><g/></svg>"),
        ("index.html", INDEX),
    ]);
    let serve = || {
        let mut dev = app.dev();
        dev.enable_cache = true;
        crate::serve(dev)
    };
    let server = serve().await;
    let first = linked_sheet(&server).await;
    assert!(first.contains("%3cg/%3e"), "{first}");
    drop(server);

    app.write(&[("src/dot.svg", "<svg><rect/></svg>")]);
    let server = serve().await;
    let second = linked_sheet(&server).await;
    assert!(
        second.contains("%3crect/%3e") && !second.contains("%3cg/%3e"),
        "{second}"
    );
}
