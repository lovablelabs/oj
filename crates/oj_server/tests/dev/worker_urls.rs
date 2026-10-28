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
        (
            "src/typed.ts",
            "export const w = new Worker(new URL(\"./w.js\", import.meta.url), { type: \"module\" as const });\n",
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
    assert!(
        main.contains("w.js?worker&url&worker_type=module"),
        "{main}"
    );
    let classic_url = server
        .text("/app/src/w.js?worker&url&worker_type=classic")
        .await;
    assert!(
        classic_url.contains(
            "export default \"https://cdn.example/app/src/w.js?worker_file&type=classic\""
        ),
        "{classic_url}"
    );

    let module = server.text("/app/src/w.js?worker_file&type=module").await;
    assert!(
        module.starts_with("import \"/app/@vite/env\"\n;\n"),
        "{module}"
    );
    // `{ type: "module" as const }` is static once the TS is stripped.
    let typed = server.text("/app/src/typed.ts").await;
    assert!(
        typed.contains("w.js?worker&url&worker_type=module"),
        "{typed}"
    );
    // A user `type=` on `?worker` is not the worker's type.
    let user = server.text("/app/src/w.js?worker&type=classic").await;
    assert!(user.contains(&format!("\"{url}\"")), "{user}");
    let ignore = server.text("/app/src/w.js?worker_file&type=ignore").await;
    assert!(ignore.starts_with("const context = "), "{ignore}");
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
      if (file.endsWith("boom.svg")) throw new Error("no inline decision for boom");
    },
  },
  environments: { ssr: { build: { assetsInlineLimit: 0 } } },
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
        ("src/boom.svg", "<svg></svg>\n"),
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

    // The ssr environment's own limit (0) applies there, not the function.
    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}%3Furl",
            app.root.join("src/big.svg").display()
        ))
        .await;
    assert!(ssr.contains("export default \"/src/big.svg\""), "{ssr}");
    // A function that throws fails the request, as in Vite.
    let boom = server.get("/src/boom.svg?url").await;
    assert_eq!(boom.status, 500, "{}", boom.body);
    assert!(
        boom.body.contains("no inline decision for boom"),
        "{}",
        boom.body
    );

    let css = server.text("/src/style.css?direct").await;
    assert!(
        css.contains(r#"url("/src/kept.svg")"#) && css.contains(r#"url("data:image/svg+xml"#),
        "{css}"
    );
}

// The ssr environment's resolved `resolve.builtins` decide what is a builtin
// there (Vite's createIsBuiltin over environment.config.resolve.builtins).
#[tokio::test(flavor = "multi_thread")]
async fn the_ssr_environments_own_builtins_decide() {
    crate::require_fixture!("vite");
    let config = r#"export default {
  ssr: { noExternal: ["events"] },
  resolve: { alias: { "aliased-builtin": "events", "my-fs": "node:fs" } },
  environments: {
    ssr: { resolve: { builtins: ["custom-builtin", "aliased-builtin", /^my:/] } },
  },
};
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrbuiltins","private":true,"type":"module"}"#,
        ),
        (
            "node_modules/events/package.json",
            r#"{"name":"events","version":"3.3.0","main":"events.js"}"#,
        ),
        ("node_modules/events/events.js", "module.exports = {};\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    let main = app.root.join("src/main.js");
    let resolve = |spec: &'static str| {
        let url = format!("/@ssr-resolve?importer={}&spec={spec}", main.display());
        let server = &server;
        async move { server.text(&url).await }
    };
    for spec in ["custom-builtin", "my:thing"] {
        let r = resolve(spec).await;
        assert!(r.contains(r#""external":true"#), "{spec}: {r}");
    }
    // Not a builtin there any more: `ssr.noExternal` keeps it in the pipeline.
    // An alias goes first (importAnalysis' matchAlias), so a builtin's name
    // that is also an alias is the alias's target.
    for spec in ["events", "aliased-builtin"] {
        let r = resolve(spec).await;
        assert!(
            !r.contains(r#""external""#) && r.contains("/node_modules/events/events.js\""),
            "{spec}: {r}"
        );
    }
    // An alias Node loads gets the target, not the alias name.
    let fs = resolve("my-fs").await;
    assert!(
        fs.contains(r#""external":true"#) && fs.contains(r#""node:fs""#),
        "{fs}"
    );
}

// Each environment's own `build.assetsInlineLimit` decides there (Vite's
// environment.config.build): a number under a function top level, a function
// under a number, and a larger env limit over a top-level 0.
#[tokio::test(flavor = "multi_thread")]
async fn each_environments_assets_inline_limit_decides_there() {
    crate::require_fixture!("vite");
    let big = format!("<svg>{}</svg>\n", "<g/>".repeat(1100));
    let cases = [
        (
            r#"export default {
  build: { assetsInlineLimit: 0 },
  environments: { ssr: { build: { assetsInlineLimit: () => true } } },
};
"#,
            false,
            true,
        ),
        (
            r#"export default {
  build: { assetsInlineLimit: 0 },
  environments: { ssr: { build: { assetsInlineLimit: 8192 } } },
};
"#,
            false,
            true,
        ),
        (
            r#"export default {
  build: { assetsInlineLimit: () => true },
  environments: { ssr: { build: { assetsInlineLimit: 0 } } },
};
"#,
            true,
            false,
        ),
    ];
    for (config, client_inlines, ssr_inlines) in cases {
        let app = App::new(&[
            (
                "package.json",
                r#"{"name":"inlineenvs","private":true,"type":"module"}"#,
            ),
            ("src/big.svg", &big),
            ("src/main.js", "export {};\n"),
            ("index.html", INDEX_HTML),
            ("vite.config.mjs", config),
        ]);
        app.link_fixture("vite");
        let server = app.serve().await;
        let client = server.text("/src/big.svg?url").await;
        let ssr = server
            .text(&format!(
                "/@ssr-module?id={}%3Furl",
                app.root.join("src/big.svg").display()
            ))
            .await;
        let inlined = |m: &str| m.contains("export default \"data:image/svg+xml");
        assert_eq!(
            inlined(&client),
            client_inlines,
            "{config} client: {client}"
        );
        assert_eq!(inlined(&ssr), ssr_inlines, "{config} ssr: {ssr}");
    }
}
