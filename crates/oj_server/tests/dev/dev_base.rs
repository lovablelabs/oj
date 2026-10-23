//! A non-root `base` (Vite's baseMiddleware): the page names oj's client
//! scripts under the base and those URLs must answer, a page visit to `/`
//! redirects to the base, and unbased module URLs keep serving because oj's
//! rewritten imports carry no base.

use crate::App;

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"devbase","private":true}"#),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        ("index.html", crate::INDEX_HTML),
        (
            "public/logo.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ),
        (
            "src/main.js",
            "import { x } from './x.js';\nconsole.log(x);\n",
        ),
        ("src/x.js", "export const x = 1;\n"),
        (
            "public/apple.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn client_scripts_answer_under_the_base() {
    let app = app();
    let server = app.serve().await;
    let html = server.text("/app/").await;
    for src in [
        "/app/@oj/client.js",
        "/app/@oj/refresh-preamble.js",
        "/app/src/main.js",
    ] {
        assert!(
            html.contains(&format!("src=\"{src}\"")),
            "page lacks {src}:\n{html}"
        );
        server.text(src).await;
    }
    server.text("/app/index.html").await;
    server.text("/app/logo.svg").await;
    server.text("/src/x.js").await;
    // Shares the base's leading letters but is not under it.
    server.text("/apple.svg").await;
    server.text("/@oj/client.js").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn root_page_visit_redirects_to_the_base() {
    let app = app();
    let server = app.serve().await;
    let res = server.get("/?q=1").await;
    assert_eq!(res.status, 302, "{}", res.body);
    assert_eq!(res.headers["location"], "/app/?q=1");
}

// Vite serves a relative base ("./", "") from the root in dev, with no base
// middleware; a redirect to "./" would loop.
#[tokio::test(flavor = "multi_thread")]
async fn relative_base_serves_from_the_root() {
    let app = App::new(&[
        ("package.json", r#"{"name":"devbase","private":true}"#),
        ("oj.config.json", r#"{"base":"./"}"#),
        ("index.html", crate::INDEX_HTML),
        ("src/main.js", "console.log(1);\n"),
    ]);
    let server = app.serve().await;
    let html = server.text("/").await;
    assert!(html.contains("src=\"/@oj/client.js\""), "{html}");
}

// Vite's baseMiddleware runs between the configureServer PRE hooks and the
// POST hooks: PRE middleware sees the browser's URL, POST middleware the URL
// with the base stripped.
#[tokio::test(flavor = "multi_thread")]
async fn post_hook_middleware_sees_the_base_stripped() {
    let app = App::new(&[
        ("package.json", r#"{"name":"devbase","private":true}"#),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        (
            "oj.plugins.mjs",
            r#"export default [{
  name: "seen",
  configureServer(server) {
    server.middlewares.use((req, _res, next) => {
      req.preUrl = req.url;
      next();
    });
    return () => {
      server.middlewares.use((req, res, next) => {
        if (!req.url.startsWith("/__seen")) return next();
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify({ pre: req.preUrl, post: req.url }));
      });
    };
  },
}];
"#,
        ),
        ("index.html", crate::INDEX_HTML),
        ("src/main.js", "console.log(1);\n"),
    ]);
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let seen: serde_json::Value =
        serde_json::from_str(&server.text("/app/__seen?x=1").await).unwrap();
    assert_eq!(seen["pre"], "/app/__seen?x=1");
    assert_eq!(seen["post"], "/__seen?x=1");
    let unbased: serde_json::Value = serde_json::from_str(&server.text("/__seen").await).unwrap();
    assert_eq!(unbased["pre"], "/__seen");
    assert_eq!(unbased["post"], "/__seen");
}
