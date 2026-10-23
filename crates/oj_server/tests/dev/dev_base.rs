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
