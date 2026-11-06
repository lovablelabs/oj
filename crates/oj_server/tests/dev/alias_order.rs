// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Which file a `resolve.alias`ed import serves, next to tsconfig `paths`,
//! against Vite's dev server with vite-tsconfig-paths on the same app.
//! Vite's alias plugin runs ahead of every resolver plugin and takes the
//! first entry that matches, in the order the config wrote them
//! (normalizeAlias keeps an object's key order), so
//! `{ "@/integrations/supabase/client": backend, "@": src }` sends the client
//! import to `backend`, and `@` written first takes it. The rewritten id goes
//! on through tsconfig `paths`, so an alias to `@/lib` still lands.
//! vite-tsconfig-paths tries `@/*` before an exact key (a pattern with no `*`
//! has no prefix), and a `paths` answer that is a declaration file is none.
//!
//! oj's resolver tried tsconfig `paths` before the aliases, the exact key
//! first, and kept the aliases sorted, so `@` always came first: the import
//! served the `.d.ts` the paths named, or `@`'s file, over the alias written
//! for it.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.ts"></script></body></html>"#;

/// The project's own client module, a declaration the tsconfig `paths` name
/// for it, and the app backend an alias sends it to.
const FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"alias-order","private":true,"type":"module"}"#,
    ),
    ("index.html", INDEX),
    (
        "src/main.ts",
        "import { who } from \"@/integrations/supabase/client\";\nimport { helper } from \"@/lib/helper\";\ndocument.body.dataset.who = who + helper;\n",
    ),
    (
        "src/tilde.ts",
        "import { helper } from \"~lib/helper\";\nexport const tilde = helper;\n",
    ),
    ("src/lib/helper.ts", "export const helper = \"-helper\";\n"),
    (
        "src/integrations/supabase/client.ts",
        "export const who = \"generated-client\";\n",
    ),
    ("src/appBackend.ts", "export const who = \"app-backend\";\n"),
    (
        "src/types/existingSupabaseClient.d.ts",
        "export declare const supabase: unknown;\n",
    ),
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/*": ["./src/*"] } } }"#,
    ),
];

const HEADER: &str = "import path from \"node:path\";\nimport tsConfigPaths from \"vite-tsconfig-paths\";\nconst dir = path.dirname(new URL(import.meta.url).pathname);\nconst src = (p) => path.resolve(dir, \"src\", p);\n";

fn config(alias: &str) -> String {
    format!(
        "{HEADER}export default {{ resolve: {{ alias: {alias} }}, plugins: [tsConfigPaths()] }};\n"
    )
}

/// Each config: its name, its `resolve.alias`, and the file the client import
/// serves (on Vite and on oj).
const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "specific-first",
        r#"{ "@/integrations/supabase/client": src("appBackend.ts"), "~lib": "@/lib", "@": src("") }"#,
        "/src/appBackend.ts",
    ),
    (
        "at-first",
        r#"{ "@": src(""), "~lib": "@/lib", "@/integrations/supabase/client": src("appBackend.ts") }"#,
        "/src/integrations/supabase/client.ts",
    ),
    (
        "array",
        r#"[{ find: "@/integrations/supabase/client", replacement: src("appBackend.ts") }, { find: "~lib", replacement: "@/lib" }, { find: "@", replacement: src("") }]"#,
        "/src/appBackend.ts",
    ),
    // No alias for it: vite-tsconfig-paths tries `@/*` before the exact key.
    (
        "paths-only",
        r#"{ "~lib": "@/lib" }"#,
        "/src/integrations/supabase/client.ts",
    ),
];

/// The specifiers a served module imports, in order.
fn imports(js: &str) -> Vec<String> {
    js.split("from \"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(spec, _)| spec.to_string()))
        .collect()
}

/// Vite's dev server on `root` with `config`, once it answers.
struct ViteDev {
    child: Child,
    port: u16,
}

impl ViteDev {
    fn start(root: &Path, config: &str) -> ViteDev {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args(["--host", "127.0.0.1", "--strictPort", "--port"])
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

    /// The status and body of `path`.
    fn get(&self, path: &str) -> (u16, String) {
        let out = Command::new("curl")
            .args(["-s", "-m", "60", "-w", "\n%{http_code}"])
            .arg(format!("http://127.0.0.1:{}{path}", self.port))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, status) = text.rsplit_once('\n').unwrap_or(("", "0"));
        (status.trim().parse().unwrap_or(0), body.to_string())
    }

    /// The imports of `path`, once Vite has served it.
    fn imports(&self, path: &str) -> Vec<String> {
        let (status, body) = self.get(path);
        assert_eq!(status, 200, "vite dev {path}:\n{body}");
        imports(&body)
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_import_serves_the_entry_vite_picks() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    for (name, alias, want) in CONFIGS {
        let config_file = format!("vite.{name}.config.js");
        app.write(&[(config_file.as_str(), &config(alias))]);

        let vite = ViteDev::start(&app.root, &config_file);
        let theirs = vite.imports("/src/main.ts");
        let tilde = vite.imports("/src/tilde.ts");
        drop(vite);
        assert_eq!(
            theirs,
            [*want, "/src/lib/helper.ts"],
            "vite dev, {name}: the alias test's premise"
        );
        assert_eq!(tilde, ["/src/lib/helper.ts"], "vite dev, {name}: ~lib");

        let mut dev = app.dev();
        dev.config = Some(app.root.join(&config_file));
        let server = crate::serve(dev).await;
        let ours = imports(&server.text("/src/main.ts").await);
        let our_tilde = imports(&server.text("/src/tilde.ts").await);
        drop(server);
        assert_eq!(ours, theirs, "oj dev vs vite dev, {name}");
        assert_eq!(
            our_tilde, tilde,
            "oj dev vs vite dev, {name}: an alias to a paths name"
        );
    }
}

// When the only `paths` answer is a declaration file, it is none: Vite fails
// the import (vite-tsconfig-paths passes, vite:resolve finds nothing), and oj
// fails it too rather than serving the `.d.ts` as JavaScript.
#[tokio::test(flavor = "multi_thread")]
async fn a_paths_target_that_is_a_declaration_file_is_never_served() {
    crate::require_fixture!("vite", "vite-tsconfig-paths");
    let app = App::new(FILES);
    app.link_fixture("vite");
    app.link_fixture("vite-tsconfig-paths");
    app.write(&[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/lib/*": ["./src/lib/*"] } } }"#,
        ),
        ("vite.paths.config.js", &config("{}")),
    ]);
    let vite = ViteDev::start(&app.root, "vite.paths.config.js");
    let (status, body) = vite.get("/src/main.ts");
    drop(vite);
    assert_eq!(status, 500, "vite dev fails the import: {body}");
    assert!(body.contains("@/integrations/supabase/client"), "{body}");

    let mut dev = app.dev();
    dev.config = Some(app.root.join("vite.paths.config.js"));
    let server = crate::serve(dev).await;
    // oj leaves an import it cannot resolve as an `/@id/` URL where Vite's
    // import analysis fails the importer; either way the import fails.
    let main = server.text("/src/main.ts").await;
    let client = imports(&main).into_iter().next().unwrap();
    assert!(client.starts_with("/@id/"), "{main}");
    let served = server.get(&client).await;
    assert_eq!(served.status, 500, "{}", served.body);
    assert!(
        served
            .body
            .contains("Failed to resolve import \"@/integrations/supabase/client\""),
        "{}",
        served.body
    );
    let helper = server.text("/src/lib/helper.ts").await;
    assert!(helper.contains("-helper"), "{helper}");
}
