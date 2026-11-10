// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Two files a Start server render reaches inside a dependency and used to
//! hand to Node's ESM loader, which cannot take them:
//!
//! - A directory imported as a bare `.` or `..` from a sibling module of the
//!   package's ES build (antd's `es/*/style/pure-panel.js` imports `'.'`;
//!   132 files of antd 5 import `'.'` or `'..'`). oj loads such a build
//!   under Node semantics and only took `./x` and `../x` as relative, so the
//!   bare form went to Node, which refuses a directory in an ES module
//!   (ERR_UNSUPPORTED_DIR_IMPORT) and every page 500'd. Vite's resolve
//!   plugin reads every specifier starting with `.` as relative
//!   (`id[0] === '.'`, plugins/resolve.ts) and finishes the directory at
//!   its `index.js`, as oj already did for `./style`.
//! - A package's `.svg` imported by its deep path
//!   (`icon-set/round/home.svg`). Vite externalizes a resolved dependency
//!   file only when `canExternalizeFile` admits it (js, mjs, cjs or no
//!   extension; external.ts), so the SVG goes through its asset plugin and
//!   is its URL; oj externalized it and Node parsed the markup as a module
//!   ("SyntaxError: Expected ';', '}' or <eof>").
//!
//! The app is the Start fixture (see `start_app`), with the two packages in
//! a node_modules of its own beside the fixture's.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, build, Server};

/// `dir-imports` is antd's shape: a CommonJS `main`, an ES `module` build
/// whose files import siblings extensionless and directories bare.
/// `icon-set` is an icon package of plain SVG files and no entry.
const PACKAGES: &[(&str, &str)] = &[
    (
        "dir-imports/package.json",
        r#"{"name":"dir-imports","main":"lib/index.js","module":"es/index.js"}"#,
    ),
    (
        "dir-imports/lib/index.js",
        "exports.Tag = function () { return \"cjs-tag\"; };\n",
    ),
    (
        "dir-imports/es/index.js",
        "export { default as Tag } from './tag';\n",
    ),
    (
        "dir-imports/es/tag/index.js",
        "import useStyle from './style';\nimport PresetCmp from './style/presetCmp';\nimport { panel } from './style/pure-panel';\nimport { notice } from './style/inner/notice';\nexport default function Tag() {\n  return `tag:${useStyle()}:${PresetCmp()}:${panel()}:${notice()}`;\n}\n",
    ),
    (
        "dir-imports/es/tag/style/index.js",
        "import { preset } from './presetCmp';\nexport const token = 'token';\nexport default function useStyle() {\n  return `style-${preset}`;\n}\n",
    ),
    (
        "dir-imports/es/tag/style/presetCmp.js",
        "export const preset = 'preset';\nexport default function PresetCmp() {\n  return 'preset-cmp';\n}\n",
    ),
    // antd's `pure-panel.js`: the directory it sits in, imported as `.`.
    (
        "dir-imports/es/tag/style/pure-panel.js",
        "import { token } from '.';\nexport function panel() {\n  return `panel-${token}`;\n}\n",
    ),
    // The parent directory, imported as `..`.
    (
        "dir-imports/es/tag/style/inner/notice.js",
        "import { token } from '..';\nexport function notice() {\n  return `notice-${token}`;\n}\n",
    ),
    (
        "icon-set/package.json",
        r#"{"name":"icon-set","version":"1.0.0"}"#,
    ),
    (
        "icon-set/round/home.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\"><path d=\"M10 19v-5h4v5\"/></svg>\n",
    ),
];

fn write_app(root: &Path) {
    // The fixture's node_modules is one symlink; a real directory of links to
    // its packages takes the two test packages beside them, where an app's
    // deps live.
    let shared = std::fs::read_link(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("node_modules")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    for entry in std::fs::read_dir(&shared).unwrap().flatten() {
        std::os::unix::fs::symlink(
            entry.path(),
            root.join("node_modules").join(entry.file_name()),
        )
        .unwrap();
    }
    for (file, body) in PACKAGES {
        let path = root.join("node_modules").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    std::fs::write(
        root.join("src/routes/dep-files.tsx"),
        r#"import { createRoute } from "@tanstack/react-router";
import { Tag } from "dir-imports";
import home from "icon-set/round/home.svg";

import { rootRoute } from "./__root";

export const depFilesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/dep-files",
  component: DepFiles,
});

function DepFiles() {
  return (
    <main>
      <pre id="dep-tag">{Tag()}</pre>
      <pre id="dep-svg">{home}</pre>
    </main>
  );
}
"#,
    )
    .unwrap();
    let tree = root.join("src/routeTree.ts");
    let source = std::fs::read_to_string(&tree).unwrap();
    let source = source
        .replace(
            "import { rootRoute } from \"./routes/__root\";",
            "import { rootRoute } from \"./routes/__root\";\nimport { depFilesRoute } from \"./routes/dep-files\";",
        )
        .replace("addChildren([", "addChildren([depFilesRoute, ");
    assert!(source.contains("depFilesRoute, "), "{source}");
    std::fs::write(tree, source).unwrap();
}

fn pre<'h>(html: &'h str, id: &str) -> &'h str {
    html.split_once(&format!("<pre id=\"{id}\">"))
        .and_then(|(_, rest)| rest.split_once("</pre>"))
        .map(|(text, _)| text)
        .unwrap_or_default()
}

#[test]
fn a_dependency_directory_import_and_svg_render_on_the_server() {
    let Some(app) = app("dep-files") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    let (status, html) = dev.request("GET", "/dep-files", &[]);
    assert_eq!(
        status,
        200,
        "the page importing a dependency's directories and SVG\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("ERR_UNSUPPORTED_DIR_IMPORT"),
        "Node was handed a directory import\n{}",
        dev.output()
    );
    // The `module` build rendered, each directory finished at its index.
    assert_eq!(
        pre(&html, "dep-tag"),
        "tag:style-preset:preset-cmp:panel-token:notice-token",
        "the server render of the dependency:\n{html}\n{}",
        dev.output()
    );
    // A small SVG is its data URL in dev, as Vite's `fileToDevUrl` inlines
    // what the build would; never the file's markup run as code.
    let svg = pre(&html, "dep-svg");
    assert!(
        svg.starts_with("data:image/svg+xml,%3csvg"),
        "the dependency SVG rendered as {svg:?}\n{}",
        dev.output()
    );
    drop(dev);

    build(root);
}
