// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `?raw` imports in a Start app answer the file's text, whatever the file
//! is, as Vite's vite:asset load does (`rawRE` is checked before any
//! extension rule, `plugins/asset.ts`). An SVG is the case that went wrong:
//! a bare `.svg` import is a URL (or a component, when a plugin takes it),
//! and that rule took the `?raw` import too, so `import logo from
//! "./logo.svg?raw"` evaluated to the dev server's `/@oj-start/fs/...` URL
//! and a page that set it as HTML printed the path.
//!
//! The app is the Start fixture (see `start_app`), whose config has an SVG
//! component plugin on `src/**/*.svg`, as apps that hit this do.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, Server};

fn write_route(root: &Path) {
    std::fs::write(
        root.join("src/routes/raw-svg.tsx"),
        r#"import { createRoute } from "@tanstack/react-router";

import star from "../star.svg?raw";
import starInline from "../star.svg?inline";
import starUrl from "../star.svg?url";
import starFile from "../star.svg?url&no-inline";
import bigUrl from "../big.svg?url";
import dot from "../../art/dot.svg";
import note from "../content/answer.txt?raw";

import { rootRoute } from "./__root";

export const rawSvgRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/raw-svg",
  component: RawSvg,
});

function RawSvg() {
  return (
    <main>
      <div id="raw-svg" dangerouslySetInnerHTML={{ __html: star }} />
      <pre id="raw-svg-text">{star}</pre>
      <pre id="raw-note">{note}</pre>
      <pre id="inline-svg">{starInline}</pre>
      <pre id="url-svg">{starUrl}</pre>
      <pre id="file-svg">{starFile}</pre>
      <pre id="big-svg">{bigUrl}</pre>
      <pre id="bare-svg">{dot}</pre>
    </main>
  );
}
"#,
    )
    .unwrap();
    // Over Vite's 4096-byte assetsInlineLimit: stays a url.
    std::fs::write(
        root.join("src/big.svg"),
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>\n",
            "<g/>".repeat(1500)
        ),
    )
    .unwrap();
    // Outside `src/`, where the fixture's svgr `include` never reaches: a
    // bare import vite:asset answers.
    std::fs::create_dir_all(root.join("art")).unwrap();
    std::fs::write(
        root.join("art/dot.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"1\"/></svg>\n",
    )
    .unwrap();
    let tree = root.join("src/routeTree.ts");
    let source = std::fs::read_to_string(&tree).unwrap();
    let source = source
        .replace(
            "import { rootRoute } from \"./routes/__root\";",
            "import { rootRoute } from \"./routes/__root\";\nimport { rawSvgRoute } from \"./routes/raw-svg\";",
        )
        .replace("addChildren([", "addChildren([rawSvgRoute, ");
    assert!(source.contains("rawSvgRoute, "), "{source}");
    std::fs::write(tree, source).unwrap();
}

/// The route's client chunk: the one the entry imports for it.
fn client_chunk(dev: &Server) -> (String, String) {
    let (status, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let mut found = None;
    for name in entry
        .split('"')
        .filter(|s| s.starts_with("./") && s.ends_with(".js"))
    {
        let path = format!("/@oj-start/{}", &name[2..]);
        let (status, code) = dev.request("GET", &path, &[]);
        assert_eq!(status, 200, "{path}\n{}", dev.output());
        if code.contains("raw-svg-text") {
            found = Some((path, code));
            break;
        }
    }
    if let Some(found) = found {
        return found;
    }
    // The route may be in the entry itself.
    assert!(
        entry.contains("raw-svg-text"),
        "no chunk renders the route\n{entry}\n{}",
        dev.output()
    );
    ("/@oj-start/client-entry.js".to_string(), entry)
}

#[test]
fn a_raw_svg_import_is_the_files_text_on_both_sides() {
    let Some(app) = app("raw-svg") else {
        return;
    };
    let root = &app.0;
    write_route(root);
    let svg = std::fs::read_to_string(root.join("src/star.svg")).unwrap();
    let note = std::fs::read_to_string(root.join("src/content/answer.txt")).unwrap();

    let dev = Server::oj_dev(root);
    let (status, html) = dev.request("GET", "/raw-svg", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());
    // The server render sets the markup itself, and the text version shows
    // the file's first tag, not a URL.
    let text = html
        .split_once("<pre id=\"raw-svg-text\">")
        .and_then(|(_, rest)| rest.split_once("</pre>"))
        .map(|(text, _)| text)
        .unwrap_or_default();
    assert!(
        html.contains("<polygon") && text.starts_with("&lt;svg"),
        "the server render printed {text:?} for the ?raw SVG:\n{html}\n{}",
        dev.output()
    );
    assert!(
        html.contains(note.trim()),
        "the ?raw text import is missing from the render:\n{html}"
    );
    // `?inline` on the same file is a data URI, as vite:asset's shouldInline,
    // not the served URL either: Vite's `svgToDataURL` form, the markup
    // percent-encoded (a plain SVG; base64 only for `<text`, `<foreignObject`
    // or nested quotes), as plain dev and the build write it.
    let inline = html
        .split_once("<pre id=\"inline-svg\">")
        .and_then(|(_, rest)| rest.split_once("</pre>"))
        .map(|(text, _)| text)
        .unwrap_or_default();
    assert!(
        inline.starts_with("data:image/svg+xml,%3csvg") && inline.contains("%3cpolygon"),
        "the server render printed {inline:?} for the ?inline SVG\n{}",
        dev.output()
    );

    // Vite's dev inlines an svg the build would (`fileToDevUrl`): a small
    // `?url` import is the data URL, `?url&no-inline` and one over the
    // limit stay the served url, and a bare svg no plugin claims is inlined
    // as well.
    let pre = |id: &str| {
        html.split_once(&format!("<pre id=\"{id}\">"))
            .and_then(|(_, rest)| rest.split_once("</pre>"))
            .map(|(text, _)| text.to_string())
            .unwrap_or_default()
    };
    let url_svg = pre("url-svg");
    assert!(
        url_svg.starts_with("data:image/svg+xml,%3csvg"),
        "a small `?url` svg is its data URL in dev: {url_svg:?}"
    );
    let file_svg = pre("file-svg");
    assert!(
        file_svg.contains("/@oj-start/fs/") && file_svg.ends_with("/star.svg"),
        "`?url&no-inline` is the served url: {file_svg:?}"
    );
    let big_svg = pre("big-svg");
    assert!(
        big_svg.contains("/@oj-start/fs/") && big_svg.ends_with("/big.svg"),
        "an svg over assetsInlineLimit is the served url: {big_svg:?}"
    );
    let bare_svg = pre("bare-svg");
    assert!(
        bare_svg.starts_with("data:image/svg+xml,%3csvg") && bare_svg.contains("%3ccircle"),
        "a bare small svg outside the svgr include is inlined: {bare_svg:?}"
    );

    let (path, code) = client_chunk(&dev);
    let json = serde_json::Value::String(svg.clone()).to_string();
    assert!(
        code.contains(&json),
        "{path} carries a URL for the ?raw SVG instead of its text:\n{}",
        code.lines()
            .filter(|l| l.contains("svg"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // (The chunk still names the file for `?url&no-inline`, asserted below;
    // the `?raw` value is the text, asserted above.)
    // The same forms on the client, so the hydration agrees: the `?inline`,
    // `?url` and bare values as data URLs, `?url&no-inline` and the big one
    // as urls.
    assert!(
        code.contains("\"data:image/svg+xml,%3csvg"),
        "{path} carries no Vite-shaped data URI for the ?inline SVG"
    );
    // The render escapes the data URL's quotes as HTML text (`&#x27;`); the
    // chunk carries the value itself.
    let unescape = |s: &str| {
        s.replace("&#x27;", "'")
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    };
    let json = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    for (id, value) in [("url-svg", &url_svg), ("bare-svg", &bare_svg)] {
        assert!(
            code.contains(&json(&unescape(value))),
            "{path} does not carry the server's value for {id}:\n{value}"
        );
    }
    assert!(
        code.contains(&json(&file_svg)) && code.contains(&json(&big_svg)),
        "{path} does not carry the served urls the server rendered"
    );
}
