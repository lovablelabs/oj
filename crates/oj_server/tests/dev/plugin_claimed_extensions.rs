// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module import of a file whose extension oj does not compile (`.mdx`
//! with an MDX plugin, `hero.jpg?w=72&format=webp` with an image plugin) is
//! served by the plugin pipeline, as Vite's transformMiddleware hands every
//! module request (`sec-fetch-dest: script`, `?import`, a known JS source)
//! to `transformRequest` whatever the extension, and only a request nothing
//! loads falls to the static middlewares. oj served such a file as its raw
//! bytes (`application/octet-stream`) or, for an image, as its url module,
//! without asking the plugin host, so the plugin never ran. A fetch of the
//! same url as a resource (`<img src>`) is still the file.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use regex::Regex;

use crate::browser::Page;
use crate::{App, Server, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

/// A `transform` for `.mdx` (what `@mdx-js/rollup` does on the file's text)
/// and a `load` for an image's `?w=` query (what `vite-imagetools` does).
const PLUGINS: &str = r#"export default [
  {
    name: "mdx-lite",
    transform: {
      filter: { id: /\.mdx$/ },
      handler(code, id) {
        const title = code.split("\n")[0].replace(/^#\s*/, "");
        return "export const frontmatter = " + JSON.stringify({ title }) + ";\n"
          + "export default function Post() { return " + JSON.stringify(code.trim()) + "; }\n";
      },
    },
  },
  {
    name: "image-queries",
    load: {
      filter: { id: /\?w=\d+/ },
      handler(id) {
        const [file, query] = id.split("?");
        return "export default " + JSON.stringify("/@resized/" + file.split("/").pop() + "?" + query) + ";\n";
      },
    },
  },
];
"#;

const MAIN: &str = "import a from \"./content/blog/a.mdx\";\n\
import hero from \"./hero.jpg?w=72&format=webp\";\n\
import plain from \"./hero.jpg\";\n\
const posts = import.meta.glob(\"./content/blog/*.mdx\", { eager: true });\n\
window.__R = { a, hero, plain, posts };\n\
if (import.meta.hot) import.meta.hot.accept();\n";

/// `path` as a module import fetches it: the browser's `sec-fetch-dest: script`.
async fn imported(server: &Server, path: &str) -> crate::Response {
    with_dest(server, path, "script").await
}

async fn with_dest(server: &Server, path: &str, dest: &str) -> crate::Response {
    server
        .request(
            Request::get(path)
                .header("sec-fetch-dest", dest)
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

fn content_type(res: &crate::Response) -> String {
    res.headers
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// The url `code` imports `imported` by, query and all.
fn import_url_of(code: &str, imported: &str) -> String {
    let re = Regex::new(&format!(r#""({}[^"]*)""#, regex::escape(imported))).unwrap();
    re.captures(code)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| panic!("no import of {imported} in:\n{code}"))
}

fn app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"plugin-claimed","version":"1.0.0"}"#,
        ),
        (
            "src/content/blog/a.mdx",
            "# First\n\nHello from the first post.\n",
        ),
        ("src/content/blog/b.mdx", "# Second\n\nAnd the second.\n"),
        ("src/hero.jpg", "jpeg-bytes"),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_request_for_an_extension_a_plugin_claims_runs_the_plugin() {
    let app = app();
    let server = app.serve().await;

    // The importer names the files by their urls; an unknown query stays on
    // the image's, as Vite's import analysis keeps it for the plugins.
    let main = server.text("/src/main.js").await;
    let mdx_url = import_url_of(&main, "/src/content/blog/a.mdx");
    assert_eq!(mdx_url, "/src/content/blog/a.mdx", "{main}");
    let hero_url = import_url_of(&main, "/src/hero.jpg?w=");
    assert_eq!(hero_url, "/src/hero.jpg?w=72&format=webp", "{main}");
    assert!(
        main.contains("/src/content/blog/b.mdx"),
        "the eager glob names the second post:\n{main}"
    );

    // Imported, the mdx file is the module the plugin's transform made of it.
    let post = imported(&server, &mdx_url).await;
    assert_eq!(post.status, 200, "{}", post.body);
    assert!(
        content_type(&post).starts_with("text/javascript"),
        "an mdx module import answered {} with:\n{}",
        content_type(&post),
        post.body
    );
    assert!(
        post.body.contains("export const frontmatter") && post.body.contains(r#""First""#),
        "the plugin's transform did not run:\n{}",
        post.body
    );

    // Imported with the query the plugin claims, the image is the plugin's
    // module; without it, oj's url module as before.
    let hero = imported(&server, &hero_url).await;
    assert_eq!(hero.status, 200, "{}", hero.body);
    assert!(
        content_type(&hero).starts_with("text/javascript"),
        "the queried image import answered {}",
        content_type(&hero)
    );
    assert!(
        hero.body
            .contains(r#"export default "/@resized/hero.jpg?w=72&format=webp""#),
        "the plugin's load did not run:\n{}",
        hero.body
    );
    let plain = imported(&server, "/src/hero.jpg").await;
    assert!(
        plain.body.contains(r#"export default "/src/hero.jpg""#),
        "a plain image import is its url module:\n{}",
        plain.body
    );

    // Fetched as a resource, the url is the file.
    let img = with_dest(&server, &hero_url, "image").await;
    assert_eq!(img.status, 200);
    assert_eq!(img.body, "jpeg-bytes");
    assert!(
        content_type(&img).starts_with("image/jpeg"),
        "{}",
        content_type(&img)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_of_a_plugin_claimed_module_reaches_its_importer_as_a_hot_update() {
    let app = app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let mdx_url = import_url_of(&main, "/src/content/blog/a.mdx");
    let post = imported(&server, &mdx_url).await;
    assert!(
        post.body.contains("export const frontmatter") && post.body.contains(r#""First""#),
        "{}",
        post.body
    );

    app.write(&[(
        "src/content/blog/a.mdx",
        "# First, edited\n\nHello again.\n",
    )]);
    // The self-accepting importer's re-fetch names the edited module under a
    // `?t=` stamp, so the page loads the new version rather than the one its
    // module map holds.
    let deadline = tokio::time::Instant::now() + WAIT;
    let stamped = loop {
        let main = server.text("/src/main.js").await;
        let url = import_url_of(&main, "/src/content/blog/a.mdx");
        if url.contains("?t=") {
            break url;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "main.js never named the edited mdx module under a stamp:\n{main}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let post = imported(&server, &stamped).await;
    assert!(
        post.body.contains(r#""First, edited""#),
        "the stamped request did not serve the edit:\n{}",
        post.body
    );
}

/// `@mdx-js/rollup` as an app configures it, posts as MDX files with their
/// own `frontmatter` export, rendered by React off an eager glob.
const MDX_PLUGINS: &str = "import mdx from \"@mdx-js/rollup\";\nexport default [mdx()];\n";

const MDX_MAIN: &str = r#"import { createRoot } from "react-dom/client";
const posts = import.meta.glob("./content/blog/*.mdx", { eager: true });
function render() {
  let el = document.getElementById("root");
  if (!el) { el = document.createElement("div"); el.id = "root"; document.body.appendChild(el); }
  const root = (window.__root ??= createRoot(el));
  root.render(
    <main>
      {Object.entries(posts).map(([path, mod]) => {
        const Post = mod.default;
        return <article key={path} data-title={mod.frontmatter.title}><Post /></article>;
      })}
    </main>
  );
}
render();
if (import.meta.hot) import.meta.hot.accept();
"#;

const MDX_INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.jsx"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_eager_glob_of_mdx_posts_renders_and_hot_updates_in_the_browser() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!("react", "react-dom", "scheduler", "@mdx-js/rollup");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"mdx-blog","version":"1.0.0","type":"module"}"#,
        ),
        (
            "src/content/blog/a.mdx",
            "export const frontmatter = { title: \"First\" };\n\n# First\n\nHello from the first post.\n",
        ),
        (
            "src/content/blog/b.mdx",
            "export const frontmatter = { title: \"Second\" };\n\n# Second\n\nAnd the second.\n",
        ),
        ("src/main.jsx", MDX_MAIN),
        ("index.html", MDX_INDEX),
        ("oj.plugins.mjs", MDX_PLUGINS),
    ]);
    for pkg in ["react", "react-dom", "scheduler", "@mdx-js/rollup"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    // Held for the test: dropping it stops the listener.
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r##"document.querySelectorAll("#root article").length === 2"##,
        WAIT,
    )
    .await;
    let titles: Vec<String> = page
        .eval(r##"[...document.querySelectorAll("#root article")].map((a) => a.dataset.title + ":" + a.querySelector("h1").textContent)"##)
        .await;
    assert_eq!(titles, ["First:First", "Second:Second"]);
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");

    // An edit of a post reaches the page as a hot update of its importer.
    app.write(&[(
        "src/content/blog/a.mdx",
        "export const frontmatter = { title: \"First, edited\" };\n\n# First, edited\n\nHello again.\n",
    )]);
    page.wait_for(
        r##"document.querySelector("#root article")?.dataset.title === "First, edited""##,
        WAIT,
    )
    .await;
    assert_eq!(
        page.errors(),
        Vec::<String>::new(),
        "page errors after the edit"
    );
    page.close().await;
}
