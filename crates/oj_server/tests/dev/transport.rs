//! What a module costs on the wire: its map is served beside it, by url and
//! compile key, instead of inlined as a data URL in every response, and a
//! text response leaves gzipped when the browser accepts gzip. Vite's dev
//! server inlines the map and compresses nothing; a preview over a real
//! network pays for both on every cold load.

use std::io::Read;

use axum::body::Body;
use axum::http::Request;

use crate::App;

/// A module past the compression threshold, plus a small one under it.
fn app(extra: &[(&str, &str)]) -> App {
    let body: String = (0..60)
        .map(|i| {
            format!(
                "export function component{i}(props) {{ return props.label + \" c{i} \" + {i}; }}\n"
            )
        })
        .collect();
    let mut files = vec![
        ("package.json", r#"{"name":"transport","private":true}"#),
        ("index.html", crate::INDEX_HTML),
        (
            "src/main.js",
            "import { x } from './x.js';\nimport './big.js';\nconsole.log(x);\n",
        ),
        ("src/x.js", "export const x = 1;\n"),
    ];
    let big: &'static str = Box::leak(body.into_boxed_str());
    files.push(("src/big.js", big));
    files.extend_from_slice(extra);
    App::new(&files)
}

/// The `sourceMappingURL` a served module names.
fn map_url(module: &str) -> String {
    let url = module
        .rsplit_once("//# sourceMappingURL=")
        .unwrap_or_else(|| panic!("no sourceMappingURL in:\n{module}"))
        .1
        .trim()
        .to_string();
    assert!(
        !url.starts_with("data:"),
        "the map is served beside the module, not inlined:\n{url}"
    );
    url
}

async fn gzipped(server: &crate::Server, path: &str) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    server
        .request_bytes(
            Request::get(path)
                .header("accept-encoding", "gzip, deflate, br, zstd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

fn gunzip(bytes: &[u8]) -> String {
    let mut out = String::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_string(&mut out)
        .expect("a gzip body");
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_names_its_map_beside_itself() {
    let app = app(&[]);
    let server = app.serve().await;
    let module = server.text("/src/big.js").await;
    let url = map_url(&module);
    assert!(
        url.starts_with("/@oj/map?u=/src/big.js&k="),
        "the map is named by the module's url and compile key: {url}"
    );
    let res = server.get(&url).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.headers["content-type"], "application/json");
    assert_eq!(res.headers["cache-control"], "max-age=31536000,immutable");
    let map: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert!(
        !map["mappings"].as_str().unwrap_or("").is_empty(),
        "a real map: {map}"
    );
    assert!(
        map["sourcesContent"][0]
            .as_str()
            .is_some_and(|s| s.contains("function component59(props)")),
        "the map carries the source: {map}"
    );

    // Another key is a compile that never happened.
    let (base, _) = url.rsplit_once("&k=").unwrap();
    assert_eq!(server.get(&format!("{base}&k=0000")).await.status, 404);

    // An edit moves the module to a new key and a new map URL.
    app.write(&[("src/big.js", "export const changed = 1;\n")]);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let edited = map_url(&server.text("/src/big.js").await);
    assert_ne!(edited, url, "the edited module names a new map");
    let map: serde_json::Value = serde_json::from_str(&server.text(&edited).await).unwrap();
    assert!(
        map["sourcesContent"][0]
            .as_str()
            .is_some_and(|s| s.contains("changed")),
        "the new map carries the new source: {map}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_modules_map_moves_down_by_its_prelude() {
    let app = app(&[]);
    let server = app.serve().await;
    let module = server.text("/src/big.js?worker_file&type=module").await;
    assert!(
        module.starts_with("import \"/@vite/env\"\n;\n"),
        "the worker env prelude leads:\n{module}"
    );
    let map: serde_json::Value =
        serde_json::from_str(&server.text(&map_url(&module)).await).unwrap();
    assert!(
        map["mappings"].as_str().unwrap().starts_with(";;"),
        "two prelude lines shift the map: {}",
        map["mappings"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_the_map_answers_under_the_base() {
    let app = app(&[("oj.config.json", r#"{"base":"/app/"}"#)]);
    let server = app.serve().await;
    let url = map_url(&server.text("/app/src/big.js").await);
    assert!(url.starts_with("/app/@oj/map?u="), "{url}");
    assert_eq!(server.get(&url).await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_response_leaves_gzipped_when_accepted() {
    let app = app(&[]);
    let server = app.serve().await;
    let plain = server.get("/src/big.js").await;
    assert!(
        !plain.headers.contains_key("content-encoding"),
        "no accept-encoding, no encoding: {:?}",
        plain.headers
    );
    let (status, headers, body) = gzipped(&server, "/src/big.js").await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-encoding"], "gzip", "{headers:?}");
    assert!(
        headers
            .get_all("vary")
            .iter()
            .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("accept-encoding")),
        "{headers:?}"
    );
    assert_eq!(
        headers["etag"], plain.headers["etag"],
        "one validator either way"
    );
    assert!(
        body.len() < plain.body.len() / 2,
        "{} vs {}",
        body.len(),
        plain.body.len()
    );
    assert_eq!(gunzip(&body), plain.body);

    // oj's own routed modules and the map compress too.
    let (_, headers, _) = gzipped(&server, "/@oj/client.js").await;
    assert_eq!(
        headers["content-encoding"], "gzip",
        "the client: {headers:?}"
    );
    let (_, headers, _) = gzipped(&server, &map_url(&plain.body)).await;
    assert_eq!(headers["content-encoding"], "gzip", "the map: {headers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_response_and_a_304_stay_plain() {
    let app = app(&[]);
    let server = app.serve().await;
    let (status, headers, _) = gzipped(&server, "/src/x.js").await;
    assert_eq!(status, 200);
    assert!(
        !headers.contains_key("content-encoding"),
        "a module under the threshold: {headers:?}"
    );
    let etag = server.get("/src/big.js").await.headers["etag"].clone();
    let (status, headers, body) = server
        .request_bytes(
            Request::get("/src/big.js")
                .header("accept-encoding", "gzip")
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, 304);
    assert!(!headers.contains_key("content-encoding"), "{headers:?}");
    assert!(body.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_reading_import_meta_hot_has_its_map_moved_down_by_the_glue() {
    let app = app(&[(
        "src/hot.js",
        "export const hot = 1;\nif (import.meta.hot) import.meta.hot.accept();\n",
    )]);
    let server = app.serve().await;
    let module = server.text("/src/hot.js").await;
    assert!(
        module.starts_with("\nimport { createHotContext as __oj_createHotContext }"),
        "the hot context glue leads:\n{module}"
    );
    let glue_lines = module
        .split("export const hot")
        .next()
        .unwrap()
        .matches('\n')
        .count();
    assert_eq!(glue_lines, 3);
    let map: serde_json::Value =
        serde_json::from_str(&server.text(&map_url(&module)).await).unwrap();
    let mappings = map["mappings"].as_str().unwrap();
    assert!(
        mappings.starts_with(";;;") && !mappings.starts_with(";;;;"),
        "three glue lines shift the map: {mappings}"
    );
}
