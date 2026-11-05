// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Which file a `resolve.alias`ed import serves, next to tsconfig `paths`,
//! against Vite's dev server on the same app. Vite's alias plugin runs ahead
//! of every resolver plugin and takes the first entry that matches, in the
//! order the config wrote them (normalizeAlias keeps an object's key order),
//! so `{ "@/integrations/supabase/client": backend, "@": src }` sends the
//! client import to `backend`, and `@` written first takes it. tsconfig
//! `paths` come after the aliases, and a `paths` target that is a declaration
//! file is no answer (vite-tsconfig-paths passes on one).
//!
//! oj's resolver tried tsconfig `paths` before the aliases and kept the
//! aliases sorted, so `@` always came first: the import served the `.d.ts`
//! the paths named, or `@`'s file, over the alias written for it.

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

const HEADER: &str = "import path from \"node:path\";\nconst dir = path.dirname(new URL(import.meta.url).pathname);\nconst src = (p) => path.resolve(dir, \"src\", p);\n";

/// Each config: its name, its `resolve.alias`, and the file the client import
/// serves (on Vite and on oj).
const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "specific-first",
        r#"{ "@/integrations/supabase/client": src("appBackend.ts"), "@": src("") }"#,
        "/src/appBackend.ts",
    ),
    (
        "at-first",
        r#"{ "@": src(""), "@/integrations/supabase/client": src("appBackend.ts") }"#,
        "/src/integrations/supabase/client.ts",
    ),
    (
        "array",
        r#"[{ find: "@/integrations/supabase/client", replacement: src("appBackend.ts") }, { find: "@", replacement: src("") }]"#,
        "/src/appBackend.ts",
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

#[tokio::test(flavor = "multi_thread")]
async fn an_aliased_import_serves_the_entry_vite_picks() {
    crate::require_fixture!("vite");
    let app = App::new(FILES);
    app.link_fixture("vite");
    for (name, alias, want) in CONFIGS {
        let config = format!("vite.{name}.config.js");
        app.write(&[(
            config.as_str(),
            &format!("{HEADER}export default {{ resolve: {{ alias: {alias} }} }};\n"),
        )]);

        let vite = ViteDev::start(&app.root, &config);
        let theirs = imports(&vite.get("/src/main.ts"));
        drop(vite);
        assert_eq!(
            theirs,
            [*want, "/src/lib/helper.ts"],
            "vite dev, {name}: the alias test's premise"
        );

        let mut dev = app.dev();
        dev.config = Some(app.root.join(&config));
        let server = crate::serve(dev).await;
        let ours = imports(&server.text("/src/main.ts").await);
        drop(server);
        assert_eq!(ours, theirs, "oj dev vs vite dev, {name}");
    }
}

// With no alias for it, the client import's `paths` target is a declaration
// file, which is no answer: the import fails as Vite's does, rather than
// serving the `.d.ts` as JavaScript. tsconfig `paths` alone still answer the
// helper import (oj applies them without vite-tsconfig-paths, where Vite 8
// needs the plugin or `resolve.tsconfigPaths`).
#[tokio::test(flavor = "multi_thread")]
async fn a_paths_target_that_is_a_declaration_file_is_never_served() {
    let app = App::new(FILES);
    let server = app.serve().await;
    let js = server.get("/src/main.ts").await.body;
    assert!(
        !js.contains(".d.ts"),
        "the declaration file was served for the import:\n{js}"
    );
    let helper = server.text("/src/lib/helper.ts").await;
    assert!(helper.contains("-helper"), "{helper}");
    app.write(&[(
        "src/other.ts",
        "import { helper } from \"@/lib/helper\";\nexport const x = helper;\n",
    )]);
    let other = server.text("/src/other.ts").await;
    assert_eq!(imports(&other), ["/src/lib/helper.ts"], "{other}");
}
