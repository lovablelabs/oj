// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev workers as Vite serves them (vite:worker, workerImportMetaUrl): a
//! worker's URL is fileToUrl of its file with `worker_file&type=<type>`,
//! `?worker` exports a WorkerWrapper passing `options.name`, `?worker&url`
//! the URL; a `worker_file` module gets the env prelude of its type, and
//! `/@vite/env` assigns the client's user defines. Also a function-valued
//! `build.assetsInlineLimit`, which dev asks per file (shouldInline).

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn workers_take_vites_worker_file_urls_and_env_prelude() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"workers","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "base": "/app/", "server": { "origin": "https://cdn.example" },
                 "define": { "__APP__": "\"demo\"", "import.meta.env.SKIP": "1" } }"#,
        ),
        ("src/w.js", "self.onmessage = () => self.postMessage(__APP__);\n"),
        (
            "src/main.js",
            "export const w = new Worker(new URL(\"./w.js\", import.meta.url), { type: \"module\" });\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let url = "https://cdn.example/app/src/w.js?worker_file&type=module";

    let wrapper = server.text("/app/src/w.js?worker").await;
    assert!(
        wrapper.contains("export default function WorkerWrapper(options)")
            && wrapper.contains(&format!("\"{url}\""))
            && wrapper.contains("name: options?.name"),
        "{wrapper}"
    );
    let url_only = server.text("/app/src/w.js?worker&url").await;
    assert!(
        url_only.contains(&format!("export default \"{url}\"")),
        "{url_only}"
    );

    // `new Worker(new URL(...), { type: "module" })` keeps its type.
    let main = server.text("/app/src/main.js").await;
    assert!(main.contains("w.js?worker&url&type=module"), "{main}");
    let typed = server.text("/app/src/w.js?worker&url&type=classic").await;
    assert!(
        typed.contains(
            "export default \"https://cdn.example/app/src/w.js?worker_file&type=classic\""
        ),
        "{typed}"
    );

    let module = server.text("/app/src/w.js?worker_file&type=module").await;
    assert!(
        module.starts_with("import \"/app/@vite/env\"\n;\n"),
        "{module}"
    );
    let classic = server.text("/app/src/w.js?worker_file&type=classic").await;
    assert!(
        classic.starts_with("importScripts(\"/app/@vite/env\")\n;\n"),
        "{classic}"
    );

    let env = server.text("/app/@vite/env").await;
    assert!(
        env.contains(r#"const defines = {"__APP__": "demo"};"#),
        "{env}"
    );
}

// Vite's shouldInline calls a function `build.assetsInlineLimit` with the
// file and its content: true inlines (even past 4 KiB), false keeps the URL
// (even for a tiny svg), and undefined falls back to the 4096 byte default.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_assets_inline_limit_decides_per_file_in_dev() {
    crate::require_fixture!("vite");
    let big = format!("<svg>{}</svg>\n", "<g/>".repeat(1100));
    let config = r#"export default {
  build: {
    assetsInlineLimit(file, content) {
      if (!Buffer.isBuffer(content)) throw new Error("content is not a Buffer");
      if (file.endsWith("big.svg")) return true;
      if (file.endsWith("kept.svg")) return false;
    },
  },
};
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"inlinefn","private":true,"type":"module"}"#,
        ),
        ("src/big.svg", &big),
        ("src/kept.svg", "<svg></svg>\n"),
        ("src/small.svg", "<svg></svg>\n"),
        (
            "src/style.css",
            ".a { background: url(./kept.svg) } .b { background: url(./big.svg) }\n",
        ),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    let tiny = "data:image/svg+xml,%3csvg%3e%3c/svg%3e";

    let big_url = server.text("/src/big.svg?url").await;
    assert!(
        big_url.contains("export default \"data:image/svg+xml"),
        "{big_url}"
    );
    let kept = server.text("/src/kept.svg?url").await;
    assert!(kept.contains("export default \"/src/kept.svg\""), "{kept}");
    let small = server.text("/src/small.svg?url").await;
    assert!(
        small.contains(&format!("export default \"{tiny}\"")),
        "{small}"
    );

    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}%3Furl",
            app.root.join("src/kept.svg").display()
        ))
        .await;
    assert!(ssr.contains("export default \"/src/kept.svg\""), "{ssr}");

    let css = server.text("/src/style.css?direct").await;
    assert!(
        css.contains(r#"url("/src/kept.svg")"#) && css.contains(r#"url("data:image/svg+xml"#),
        "{css}"
    );
}
