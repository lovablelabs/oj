//! `oj dev` eagerly crawls the module graph by default (parallel pre-compile,
//! warm HMR); `--lazy` compiles on demand (Vite's model) and never runs the
//! crawl. The witness is the graph snapshot the crawl writes once the graph is
//! ready, which nothing else writes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{serve, App, Server};

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"lazy","version":"1.0.0"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>"#,
        ),
        ("dep.tsx", "export const dep = 7;\n"),
        ("unused.tsx", "export const unused = 99;\n"),
        (
            "main.tsx",
            "import { dep } from \"./dep\";\nexport const value = dep + 1;\nconsole.log(value);\n",
        ),
    ])
}

/// The crawl's snapshot file, or None when OJ_CACHE_DIR points the cache
/// outside the app, where concurrent tests would share one snapshot.
fn snapshot(root: &Path) -> Option<PathBuf> {
    let cache = oj_cache::cache_root(root);
    cache
        .starts_with(root)
        .then(|| cache.join("graph-snapshot.json"))
}

async fn serves_on_demand(server: &Server) {
    let root = server.get("/").await;
    assert_eq!(root.status, 200, "index serves:\n{}", root.body);
    let main = server.get("/main.tsx").await;
    assert_eq!(main.status, 200, "on-demand module serves:\n{}", main.body);
    assert!(
        main.body.contains("const value"),
        "module compiled:\n{}",
        main.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn default_runs_the_eager_crawl() {
    let app = app();
    let Some(snapshot) = snapshot(&app.root) else {
        eprintln!("skipping: OJ_CACHE_DIR shares the graph snapshot");
        return;
    };
    let server = app.serve().await;
    serves_on_demand(&server).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !snapshot.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "default must run the eager crawl (no graph snapshot at {})",
            snapshot.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let crawled = std::fs::read_to_string(&snapshot).unwrap();
    assert!(
        crawled.contains("main.tsx"),
        "crawl reached the entry:\n{crawled}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_never_runs_the_crawl() {
    let app = app();
    let Some(snapshot) = snapshot(&app.root) else {
        eprintln!("skipping: OJ_CACHE_DIR shares the graph snapshot");
        return;
    };
    let mut dev = app.dev();
    dev.lazy = true;
    let server = serve(dev).await;
    serves_on_demand(&server).await;
    // Settle window: a crawl that did start would land within 3s.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !snapshot.exists(),
        "--lazy must NOT run the eager crawl (found {})",
        snapshot.display()
    );
}
