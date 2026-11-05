// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A dep whose `browser` entry is a UMD bundle `require()`ing a helper with
//! `import` (ESM) and `default` (CJS) builds in its `exports` map, the shape of
//! automation-events (under Tone) and `@babel/runtime`. Vite's client takes the
//! `browser` entry and resolves the `require()` with `require` in place of
//! `import` (getConditions), so the UMD factory gets the CJS function. The
//! ESM build would hand it a namespace, and the page throws `helper is not a
//! function` once it hydrates. Checked in the hydrated page, dev and build.
//!
//! The app is the Start fixture (see `start_app`). The browser half skips
//! without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;

use browser::Page;
use start_app::{app, build, Server, WAIT};

const FILES: &[(&str, &str)] = &[
    (
        "node_modules/umd-dep/package.json",
        r#"{"name":"umd-dep","main":"node.js","module":"esm.js","browser":"umd.js"}"#,
    ),
    (
        "node_modules/umd-dep/node.js",
        "exports.make = function () { return require(\"dual-helper/fn\")(); };\n",
    ),
    (
        "node_modules/umd-dep/esm.js",
        "export const make = () => \"umd-dep-esm-entry\";\n",
    ),
    (
        "node_modules/umd-dep/umd.js",
        r#"(function (global, factory) {
  typeof exports === "object" && typeof module !== "undefined" ? factory(exports, require("dual-helper/fn")) :
  factory(global.umdDep = {}, global.helper);
})(this, function (exports, helper) {
  exports.make = function () { return helper(); };
});
"#,
    ),
    (
        "node_modules/dual-helper/package.json",
        r#"{"name":"dual-helper","exports":{"./fn":{"node":"./fn.cjs","import":"./fn.mjs","default":"./fn.cjs"}}}"#,
    ),
    (
        "node_modules/dual-helper/fn.cjs",
        "function fn() { return \"helper-cjs-build\"; }\nmodule.exports = fn, module.exports.__esModule = true, module.exports[\"default\"] = module.exports;\n",
    ),
    (
        "node_modules/dual-helper/fn.mjs",
        "export default function fn() { return \"helper-esm-build\"; }\n",
    ),
];

/// The packages, and the about page showing what the dep's `make()` returns
/// once it hydrates.
fn write_app(root: &Path) {
    // The fixture's node_modules is one symlink; a real directory of links to
    // its packages takes the two test packages beside them, where an app's
    // deps live (outside `src/`, so Start's import protection skips their
    // imports as it does in Vite).
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
    for (name, body) in FILES {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let about = root.join("src/routes/about.tsx");
    let source = std::fs::read_to_string(&about).unwrap();
    let import = "import { rootRoute } from \"./__root\";";
    let heading = "<h1 className=\"fixture-heading\">about-page-marker</h1>";
    assert!(
        source.contains(import) && source.contains(heading),
        "{source}"
    );
    let source = source
        .replacen(
            import,
            &format!(
                r#"{import}
import {{ useEffect, useState }} from "react";
import {{ make }} from "umd-dep";
function Umd() {{
  const [out, setOut] = useState("pending");
  useEffect(() => {{
    try {{ setOut(make()); }} catch (e) {{ setOut("error " + String(e)); }}
  }}, []);
  return <p id="umd">{{out}}</p>;
}}"#
            ),
            1,
        )
        .replacen(heading, &format!("{heading}\n      <Umd />"), 1);
    std::fs::write(&about, source).unwrap();
}

async fn hydrates_with_the_cjs_helper(label: &str, server: &Server) {
    let (status, body) = server.request("GET", "/about", &[]);
    assert!(
        status == 200 && body.contains("about-page-marker"),
        "{label}: /about answered {status}, want the rendered page\n{}\n{}",
        &body[..body.len().min(500)],
        server.output()
    );
    if browser::skip() {
        return;
    }
    let page = Page::open(&server.url("/about")).await;
    let ran = page
        .poll(
            "document.querySelector('#umd')?.textContent === 'helper-cjs-build'",
            WAIT,
        )
        .await;
    let shown: Option<String> = page
        .eval("document.querySelector('#umd')?.textContent ?? null")
        .await;
    assert!(
        ran,
        "{label}: the dep's make() answered {shown:?}, want helper-cjs-build; page errors: {:?}\n{}",
        page.errors(),
        server.output()
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "{label}: page errors");
    page.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_umds_require_takes_the_cjs_build_of_a_dual_helper() {
    let Some(app) = app("require-conditions") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    hydrates_with_the_cjs_helper("dev", &dev).await;
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    hydrates_with_the_cjs_helper("build", &built).await;
}
