// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A TanStack Start app's `resolve.alias`ed import, next to tsconfig `paths`,
//! renders the module Vite's dev server renders, on the server and once the
//! page hydrates. Vite takes the first alias entry that matches, in the order
//! the config wrote them, ahead of tsconfig `paths`: the Supabase client
//! import an app points at its own backend goes there, not to the generated
//! client `@` would give it, nor to the declaration file the `paths` name.
//! oj's server render took `@` (aliases sorted) or the `paths` first.
//!
//! The browser half skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use browser::Page;
use start_app::{app, free_port, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { who } from "@/integrations/supabase/client";

export const Route = createFileRoute("/")({
  component: () => <p id="who">{who}</p>,
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

const TSCONFIG: &str = r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true, "baseUrl": ".",
  "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/*": ["./src/*"] } } }
"#;

/// Each config: its name, its `resolve.alias`, and what the page renders.
const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "specific-first",
        r#"{ "@/integrations/supabase/client": src("appBackend.ts"), "@": src("") }"#,
        "app-backend",
    ),
    (
        "at-first",
        r#"{ "@": src(""), "@/integrations/supabase/client": src("appBackend.ts") }"#,
        "generated-client",
    ),
];

fn config(alias: &str) -> String {
    format!(
        r#"import path from "node:path";
import {{ tanstackStart }} from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import {{ defineConfig }} from "vite";

const src = (p: string) => path.resolve(__dirname, "src", p);

export default defineConfig({{
  resolve: {{ alias: {alias} }},
  plugins: [tanstackStart(), react()],
}});
"#
    )
}

/// The Start fixture with one page importing the client, and one config per
/// alias order.
fn alias_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    for (file, body) in [
        (
            "src/integrations/supabase/client.ts",
            "export const who = \"generated-client\";\n",
        ),
        ("src/appBackend.ts", "export const who = \"app-backend\";\n"),
        (
            "src/types/existingSupabaseClient.d.ts",
            "export declare const supabase: unknown;\n",
        ),
        ("tsconfig.json", TSCONFIG),
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    for (name, alias, _) in CONFIGS {
        std::fs::write(root.join(format!("vite.{name}.config.ts")), config(alias)).unwrap();
    }
    Some(app)
}

/// Vite's dev server on `root` with `config`, once it answers.
struct ViteDev {
    child: Child,
    port: u16,
}

impl ViteDev {
    fn start(root: &Path, config: &str) -> ViteDev {
        let port = free_port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args(["dev", "--host", "127.0.0.1", "--strictPort", "--port"])
            .arg(port.to_string())
            .args(["--config", config])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "vite dev never listened");
            std::thread::sleep(Duration::from_millis(100));
        }
        ViteDev { child, port }
    }

    fn get(&self, path: &str) -> String {
        let out = Command::new("curl")
            .args(["-s", "-m", "60"])
            .arg(format!("http://127.0.0.1:{}{path}", self.port))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the server rendered into `#who`.
fn server_who(html: &str) -> Option<String> {
    let (_, rest) = html.split_once("<p id=\"who\">")?;
    Some(rest.split_once("</p>")?.0.to_string())
}

/// What `#who` shows once the page hydrated, and the page's errors.
async fn hydrated_who(base: &str) -> (String, Vec<String>) {
    let page = Page::open(&format!("{base}/")).await;
    let hydrated = r#"document.getElementById("who") && self.__TSR_ROUTER__ && document.readyState === "complete""#;
    assert!(
        page.poll(hydrated, Duration::from_secs(60)).await,
        "the page never hydrated: {:?}",
        page.errors()
    );
    // Late hydration errors surface after the first paint.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let who: String = page
        .eval(r#"document.getElementById("who").textContent"#)
        .await;
    let errors = page.errors();
    page.close().await;
    (who, errors)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_import_renders_the_module_vite_renders() {
    let Some(app) = alias_app("alias-order") else {
        return;
    };
    let root = &app.0;
    for (name, _, want) in CONFIGS {
        let config = format!("vite.{name}.config.ts");
        let vite = ViteDev::start(root, &config);
        let theirs = server_who(&vite.get("/"));
        drop(vite);
        assert_eq!(
            theirs.as_deref(),
            Some(*want),
            "vite dev, {name}: the test's premise"
        );

        let _ = std::fs::remove_dir_all(root.join(".oj-cache"));
        let dev = Server::oj_dev_with_args(root, &["--config", &config], &[]);
        let (status, html) = dev.request("GET", "/", &[]);
        assert_eq!(
            (status, server_who(&html)),
            (200, theirs.clone()),
            "oj dev vs vite dev, {name}: the server render\n{}",
            dev.output()
        );
        if !browser::skip() {
            let (who, errors) = hydrated_who(&dev.url("")).await;
            assert_eq!(who, *want, "oj dev, {name}: the hydrated page");
            assert_eq!(errors, Vec::<String>::new(), "oj dev, {name}: page errors");
        }
    }
}
