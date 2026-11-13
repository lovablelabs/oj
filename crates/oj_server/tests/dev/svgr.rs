// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import Icon from "./icon.svg?react"` in dev: the svg becomes a React
//! component that renders an `<svg>` keeping the file's attributes
//! (`stroke-width` survives the JSX attribute rewrite), with the caller's
//! props spread onto it (`className` replaces the file's class). The build
//! half is oj's `svgr_build`. With vite-plugin-svgr in the config, its
//! `load` decides which svgs are components: configured for every `.svg`
//! (`include: "**/*.svg"`), a plain import is the component it makes, named
//! or default export; left at its default (`**/*.svg?react`), a plain import
//! stays the URL and `?react` is the plugin's component, not oj's.

use std::time::Duration;

use crate::browser::Page;
use crate::listen::{close, frame_matching, is_reload};
use crate::plugin_middleware::middleware_ready;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const ICON: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" stroke-width="2" class="base"><path d="M4 4h16" stroke="currentColor"/></svg>"#;

const MAIN: &str =
    "import { createRoot } from \"react-dom/client\";\nimport Icon from \"./icon.svg?react\";\n\
const el = document.createElement(\"div\"); el.id = \"root\"; document.body.appendChild(el);\n\
createRoot(el).render(<Icon className=\"added\" data-testid=\"icon\" />);\n";

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn svg_react_import_renders_component_with_props_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!("react", "react-dom", "scheduler");
    let app = App::new(&[
        ("package.json", r#"{"name":"svgr-app","version":"1.0.0"}"#),
        ("src/icon.svg", ICON),
        ("src/main.tsx", MAIN),
        ("index.html", INDEX),
    ]);
    for pkg in ["react", "react-dom", "scheduler"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r##"document.querySelector("#root svg")"##, WAIT)
        .await;
    let outer: String = page
        .eval(r##"document.querySelector("#root svg").outerHTML"##)
        .await;
    assert!(outer.starts_with("<svg"), "no svg rendered: {outer}");
    assert!(
        outer.contains(r#"class="added""#),
        "props not spread onto svg: {outer}"
    );
    assert!(
        outer.contains(r#"stroke-width="2""#),
        "attribute rewrite lost stroke-width: {outer}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// The whole fixture install: vite-plugin-svgr imports `vite`, `@svgr/core`
/// and `@rollup/pluginutils`.
fn link_fixture_install(app: &App) {
    let node_modules = crate::fixture("vite-plugin-svgr")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
}

/// `path` as a module import fetches it: the browser's `sec-fetch-dest: script`.
async fn imported(server: &crate::Server, path: &str) -> crate::Response {
    server
        .request(
            axum::http::Request::get(path)
                .header("sec-fetch-dest", "script")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
}

/// An app whose vite.config lists `svgr(<options>)` and whose entry runs
/// `main` (TSX, React linked) over `src/logo.svg`.
fn svgr_app(options: &str, main: &str) -> App {
    let config = format!(
        "import svgr from \"vite-plugin-svgr\";\nexport default {{ plugins: [svgr({options})] }};\n"
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"svgr-cfg","version":"1.0.0","type":"module"}"#,
        ),
        ("vite.config.js", &config),
        ("src/logo.svg", ICON),
        ("src/main.tsx", main),
        ("index.html", INDEX),
    ]);
    link_fixture_install(&app);
    app
}

const CRA_OPTIONS: &str = r#"{ include: "**/*.svg", svgrOptions: { exportType: "named", namedExport: "ReactComponent" } }"#;

/// Renders `Logo` into `#root` with a marker class.
fn render_main(import: &str) -> String {
    format!(
        "import {{ createRoot }} from \"react-dom/client\";\n{import}\n\
const el = document.createElement(\"div\"); el.id = \"root\"; document.body.appendChild(el);\n\
createRoot(el).render(<Logo className=\"added\" />);\n"
    )
}

// vite-plugin-svgr configured for every `.svg` (`include: "**/*.svg"`, the
// CRA convention with a named `ReactComponent` export): the plugin's `load`
// claims the plain `.svg` id, so the module is the component, not the URL.
#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_configured_for_plain_svgs_serves_named_components() {
    crate::require_fixture!("vite-plugin-svgr", "@svgr/core", "vite");
    let app = svgr_app(
        CRA_OPTIONS,
        "import { ReactComponent as Logo } from \"./logo.svg\";\nwindow.__LOGO = Logo;\n",
    );
    let server = app.serve().await;
    let main = server.text("/src/main.tsx").await;
    assert!(
        main.contains("\"/src/logo.svg\""),
        "the importer keeps the plain svg import (no ?url):\n{main}"
    );
    let logo = imported(&server, "/src/logo.svg").await;
    assert_eq!(logo.status, 200, "{}", logo.body);
    assert!(
        logo.body.contains("as ReactComponent"),
        "the plugin's named export is served, not the URL module:\n{}",
        logo.body
    );
    assert!(
        !logo.body.contains("export default \"/src/logo.svg\""),
        "not the URL module:\n{}",
        logo.body
    );
    // The server render takes the plugin's load too (vite:asset's load runs
    // after it in every environment).
    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}",
            app.root.join("src/logo.svg").display()
        ))
        .await;
    assert!(
        ssr.contains("ReactComponent"),
        "the ssr module is the plugin's component:\n{ssr}"
    );
    // The default-export shape over every `.svg` too.
    let app = svgr_app(
        r#"{ include: "**/*.svg", svgrOptions: { exportType: "default" } }"#,
        "import Logo from \"./logo.svg\";\nwindow.__LOGO = Logo;\n",
    );
    let server = app.serve().await;
    let logo = imported(&server, "/src/logo.svg").await;
    assert_eq!(logo.status, 200, "{}", logo.body);
    assert!(
        logo.body.contains("export default") && logo.body.contains("jsx"),
        "a component is the default export:\n{}",
        logo.body
    );
}

// The CRA migration idiom imports the same file twice: `ReactComponent`
// from the plain id and the URL from `?url`. The plugin's filter
// (`**/*.svg`) does not match the query form, so under Vite `vite:asset`
// answers `?url` with the URL. oj strips only the `?url` it added itself
// from a plain import; one the import asked for stays, and is the URL.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_url_import_stays_the_url_beside_the_component() {
    crate::require_fixture!("vite-plugin-svgr", "@svgr/core", "vite");
    let app = svgr_app(
        CRA_OPTIONS,
        "import { ReactComponent as Logo } from \"./logo.svg\";\nimport logoUrl from \"./logo.svg?url\";\nwindow.__LOGO = [Logo, logoUrl];\n",
    );
    let server = app.serve().await;
    let main = server.text("/src/main.tsx").await;
    assert!(
        main.contains("\"/src/logo.svg\"") && main.contains("\"/src/logo.svg?url\""),
        "the plain import stays plain, the ?url import keeps its query:\n{main}"
    );
    let url = imported(&server, "/src/logo.svg?url").await;
    assert_eq!(url.status, 200, "{}", url.body);
    assert!(
        url.body.starts_with("export default \"data:image/svg+xml,")
            && !url.body.contains("ReactComponent"),
        "the ?url import is the URL, not the component:\n{}",
        url.body
    );
    let component = imported(&server, "/src/logo.svg").await;
    assert!(
        component.body.contains("as ReactComponent"),
        "the plain import is the component:\n{}",
        component.body
    );
}

// The plugin's own default (`**/*.svg?react`): a plain import stays the URL
// (inlined, the file being small), as nothing claimed it, and `?react` is the
// plugin's component, served as its `load` made it: oj's own `?react` svgr
// only takes markup, so it does not wrap the plugin's code a second time.
#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_default_react_query_is_the_plugins_component() {
    crate::require_fixture!("vite-plugin-svgr", "@svgr/core", "vite");
    let app = svgr_app(
        "",
        "import url from \"./logo.svg\";\nimport Logo from \"./logo.svg?react\";\nwindow.__LOGO = [url, Logo];\n",
    );
    let server = app.serve().await;
    let plain = imported(&server, "/src/logo.svg").await;
    assert!(
        plain
            .body
            .starts_with("export default \"data:image/svg+xml,")
            && !plain.body.contains("jsx"),
        "unclaimed plain svg is its URL:\n{}",
        plain.body
    );
    let react = imported(&server, "/src/logo.svg?react").await;
    assert_eq!(
        react.status, 200,
        "?react is the plugin's component:\n{}",
        react.body
    );
    assert!(
        react.body.contains("export default") && react.body.contains("jsx"),
        "?react is a component:\n{}",
        react.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_named_component_renders_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!(
        "vite-plugin-svgr",
        "@svgr/core",
        "vite",
        "react",
        "react-dom"
    );
    let app = svgr_app(
        CRA_OPTIONS,
        &render_main("import { ReactComponent as Logo } from \"./logo.svg\";"),
    );
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r##"document.querySelector("#root svg")"##, WAIT)
        .await;
    let outer: String = page
        .eval(r##"document.querySelector("#root svg").outerHTML"##)
        .await;
    assert!(
        outer.starts_with("<svg") && outer.contains(r#"class="added""#),
        "the named component rendered with its props: {outer}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// lovable-tagger's shape: a `pre` plugin whose function-form `load` is
/// offered every id (Vite's container has no filter to ask) and claims no
/// svg (it returns null), reporting the ids it saw at `/__loads`.
const TAGGER_LIKE: &str = r#"const seen = [];
export default [{
  name: "tagger-like",
  enforce: "pre",
  load(id) {
    if (id.endsWith(".svg")) seen.push(id);
    return null;
  },
  configureServer(server) {
    server.middlewares.use("/__loads", (_req, res) => {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(seen));
    });
  },
}];
"#;

/// A plugin whose `load` has a filter no file id matches (a virtual-module
/// plugin): Vite's container never calls it for an svg.
const VIRTUAL_ONLY: &str = r#"export default [{
  name: "virtual-only",
  resolveId(id) {
    return id === "virtual:answer" ? id : null;
  },
  load: {
    filter: { id: /^virtual:/ },
    handler() {
      return "export default 42;\n";
    },
  },
}];
"#;

fn plugin_app(plugins: &str, svg: (&str, &str), main: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"svg-load-gate","version":"1.0.0","type":"module"}"#,
        ),
        ("oj.plugins.mjs", plugins),
        svg,
        ("src/main.tsx", main),
        ("index.html", INDEX),
    ])
}

fn svg_loads(body: &str) -> usize {
    serde_json::from_str::<Vec<String>>(body)
        .unwrap_or_else(|e| panic!("the load list ({e}): {body}"))
        .iter()
        .filter(|id| id.ends_with("/src/logo.svg"))
        .count()
}

/// A plain svg under a `pre` plugin with a function-form `load` (lovable-tagger):
/// Vite offers the plugin the id, it declines, and `vite:asset` answers the
/// URL. oj asks the hook once and stores the URL module under the file's key:
/// the next request for the unchanged file is the warm path (one stat), with
/// a real ETag, and the hooks are not run again. The import stays plain, as
/// under Vite, and the server render takes the same decision.
#[tokio::test(flavor = "multi_thread")]
async fn an_unfiltered_load_hook_is_asked_once_and_the_plain_svg_stays_the_url() {
    // The entry accepts itself, so an edit to the svg is a hot update of the
    // entry (under a React boundary it would be the component's), not a
    // page reload: the page keeps its module map, and the stamp matters.
    let app = plugin_app(
        TAGGER_LIKE,
        ("src/logo.svg", ICON),
        "import url from \"./logo.svg\";\nconsole.log(url);\nif (import.meta.hot) import.meta.hot.accept();\n",
    );
    let server = app.serve().await;
    middleware_ready(&server).await;

    let main = server.text("/src/main.tsx").await;
    assert!(
        main.contains("\"/src/logo.svg\""),
        "the import stays plain for the hooks to see:\n{main}"
    );
    let first = imported(&server, "/src/logo.svg").await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert!(
        first
            .body
            .starts_with("export default \"data:image/svg+xml,"),
        "unclaimed, the svg is its URL:\n{}",
        first.body
    );
    let asked = svg_loads(&server.text("/__loads").await);
    assert!(asked >= 1, "the load hook was offered the svg");
    let etag = first
        .headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        etag.len() > 2,
        "the URL module carries the file's key as its ETag, not an empty one: {etag:?}"
    );

    let second = imported(&server, "/src/logo.svg").await;
    assert_eq!(second.body, first.body);
    assert_eq!(
        svg_loads(&server.text("/__loads").await),
        asked,
        "the unchanged file's second request is the warm path, not the hooks again"
    );

    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}",
            app.root.join("src/logo.svg").display()
        ))
        .await;
    assert!(
        ssr.contains("export default") && !ssr.contains("jsx"),
        "the ssr module is the URL too:\n{ssr}"
    );

    // An edit, with a page on the socket: the importer's import carries
    // Vite's `?t=` stamp, as a `?url` import does, so the page loads the new
    // module rather than the one its module map holds; the stored module
    // misses on the new bytes (the hook is asked again, once) and is the
    // new URL.
    let before_edit = svg_loads(&server.text("/__loads").await);
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    app.write(&[(
        "src/logo.svg",
        &ICON.replace("viewBox=\"0 0 24 24\"", "viewBox=\"0 0 48 48\""),
    )]);
    let frame = frame_matching(&mut socket, "the svg change's update", is_reload).await;
    assert_eq!(
        frame["type"], "update",
        "a hot update of the entry: {frame}"
    );
    close(socket).await;
    let importer = server.text("/src/main.tsx").await;
    let stamped = stamped_import(&importer, "/src/logo.svg?t=");
    let edited = imported(&server, &stamped).await;
    assert_eq!(edited.status, 200, "{}", edited.body);
    assert!(
        edited
            .body
            .starts_with("export default \"data:image/svg+xml,")
            && edited.body != first.body,
        "the edited svg is a new URL module:\n{}",
        edited.body
    );
    assert_eq!(
        svg_loads(&server.text("/__loads").await),
        before_edit + 1,
        "the changed file is offered to the hook once more"
    );
    let again = imported(&server, &stamped).await;
    assert_eq!(again.body, edited.body);
    assert_eq!(
        svg_loads(&server.text("/__loads").await),
        before_edit + 1,
        "the edited file's next request is the warm path again"
    );
}

/// The stamped import url (`needle`, ending in `?t=`) in an importer's code.
fn stamped_import(importer: &str, needle: &str) -> String {
    let start = importer
        .find(needle)
        .unwrap_or_else(|| panic!("the import of the edited svg is not stamped:\n{importer}"));
    let rest = &importer[start..];
    rest[..rest.find('"').unwrap_or(rest.len())].to_string()
}

/// A plugin whose function-form `load` claims every `.svg` with a module of
/// its own, at the given plugin `enforce` and hook `order`.
fn claimer(enforce: &str, order: &str) -> String {
    let load = if order.is_empty() {
        "load(id) { if (!id.endsWith(\".svg\")) return null; seen.push(id); return \"export default 'claimed';\\n\"; }".to_string()
    } else {
        format!(
            "load: {{ order: \"{order}\", handler(id) {{ if (!id.endsWith(\".svg\")) return null; seen.push(id); return \"export default 'claimed';\\n\"; }} }}"
        )
    };
    let enforce = if enforce.is_empty() {
        String::new()
    } else {
        format!("enforce: \"{enforce}\",")
    };
    format!(
        "const seen = [];\nexport default [{{\n  name: \"claimer\",\n  {enforce}\n  {load},\n  configureServer(server) {{\n    server.middlewares.use(\"/__loads\", (_req, res) => {{\n      res.setHeader(\"content-type\", \"application/json\");\n      res.end(JSON.stringify(seen));\n    }});\n  }},\n}}];\n"
    )
}

/// Vite's container runs `vite:asset`'s `load` between the `pre` plugins
/// and the normal ones, a hook with `order: "pre"` ahead of them all, and
/// takes the first answer: a plain svg is claimed by a `pre` plugin's load,
/// or a normal plugin's `order: "pre"` load, and never by a normal plugin's
/// plain load, which vite:asset's URL module answers ahead of. oj asks only
/// the hooks Vite runs before vite:asset.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_load_hook_vite_runs_before_vite_asset_claims_a_plain_svg() {
    let main = "import logo from \"./logo.svg\";\nconsole.log(logo);\n";
    for (enforce, order, claims) in [
        ("", "", false),
        ("pre", "", true),
        ("", "pre", true),
        ("post", "", false),
    ] {
        let app = plugin_app(&claimer(enforce, order), ("src/logo.svg", ICON), main);
        let server = app.serve().await;
        middleware_ready(&server).await;
        let shape = format!("enforce={enforce:?} order={order:?}");
        let importer = server.text("/src/main.tsx").await;
        let module = imported(
            &server,
            if claims {
                "/src/logo.svg"
            } else {
                "/src/logo.svg?url"
            },
        )
        .await;
        assert_eq!(module.status, 200, "{shape}: {}", module.body);
        let asked = svg_loads(&server.text("/__loads").await);
        if claims {
            assert!(
                importer.contains("\"/src/logo.svg\""),
                "{shape}: the import stays plain for the plugin to claim:\n{importer}"
            );
            assert!(
                module.body.contains("claimed"),
                "{shape}: the plugin's module is served:\n{}",
                module.body
            );
            assert!(asked >= 1, "{shape}: the plugin was asked");
        } else {
            assert!(
                importer.contains("\"/src/logo.svg?url\""),
                "{shape}: the import is the URL asset's, vite:asset answering first:\n{importer}"
            );
            assert!(
                module
                    .body
                    .starts_with("export default \"data:image/svg+xml,"),
                "{shape}: the svg is its URL:\n{}",
                module.body
            );
            assert_eq!(
                asked, 0,
                "{shape}: a load vite:asset precedes is never asked for an svg"
            );
        }
    }
}

/// A load hook whose filter cannot match a file id is never asked for an svg
/// (Vite's container tests the filter first): the import is rewritten to
/// `?url` and served natively, with no round trip to the host.
#[tokio::test(flavor = "multi_thread")]
async fn a_load_hook_whose_filter_excludes_the_svg_leaves_it_the_url_asset() {
    let app = plugin_app(
        VIRTUAL_ONLY,
        ("src/logo.svg", ICON),
        "import url from \"./logo.svg\";\nimport answer from \"virtual:answer\";\nconsole.log(url, answer);\n",
    );
    let server = app.serve().await;
    let main = server.text("/src/main.tsx").await;
    assert!(
        main.contains("\"/src/logo.svg?url\""),
        "the svg import is the URL asset's, as with no plugin:\n{main}"
    );
    let url = imported(&server, "/src/logo.svg?url").await;
    assert_eq!(url.status, 200, "{}", url.body);
    assert!(
        url.body.starts_with("export default \"data:image/svg+xml,"),
        "{}",
        url.body
    );
}

/// A BOM-prefixed svg is markup too: `?react` with no plugin is oj's own
/// component (the BOM is no Unicode white space, so a plain
/// `trim_start()` left the `<` hidden behind it), and a plain import under a
/// load plugin that declines it is the URL, not a compile of the markup.
#[tokio::test(flavor = "multi_thread")]
async fn a_bom_prefixed_svg_is_markup_too() {
    crate::require_fixture!("react", "react-dom", "scheduler");
    let bom = format!("\u{feff}{ICON}");
    let app = plugin_app(
        "export default [];\n",
        ("src/logo.svg", &bom),
        "import Icon from \"./logo.svg?react\";\nconsole.log(Icon);\n",
    );
    // The component's JSX imports react's runtime.
    for pkg in ["react", "react-dom", "scheduler"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let react = imported(&server, "/src/logo.svg?react").await;
    assert_eq!(react.status, 200, "{}", react.body);
    assert!(
        react.body.contains("export default") && !react.body.trim_start().starts_with('<'),
        "oj's svgr made the component:\n{}",
        react.body
    );

    let app = plugin_app(
        TAGGER_LIKE,
        ("src/logo.svg", &bom),
        "import url from \"./logo.svg\";\nconsole.log(url);\n",
    );
    let server = app.serve().await;
    middleware_ready(&server).await;
    let plain = imported(&server, "/src/logo.svg").await;
    assert_eq!(plain.status, 200, "{}", plain.body);
    assert!(
        plain
            .body
            .starts_with("export default \"data:image/svg+xml,"),
        "declined by the plugin, the svg is its URL:\n{}",
        plain.body
    );
}
