// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Start's import protection forgets a changed file (its server-only marker,
//! the resolutions cached for it) in its hotUpdate, for the modules the hook
//! is handed, and the host runs that hook once per change of the file,
//! whichever path reaches it first. The dev server's watcher pass can come
//! first with no module for the file (no graph it runs holds a file only the
//! Start client bundle imports); that pass must still name the file, or it
//! uses up the change and the client keeps a marker the file no longer has.

use super::*;

#[tokio::test]
async fn the_first_pass_over_a_change_names_the_changed_file() {
    let root = std::env::temp_dir().join(format!("oj-ip-pass-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    let log = root.join("hot-updates.log");
    let config = root.join("vite.config.mjs");
    std::fs::write(
        &config,
        format!(
            r#"import {{ appendFileSync }} from "node:fs";
export default {{
  plugins: [{{
    name: "tanstack-start-core:import-protection",
    hotUpdate(ctx) {{
      appendFileSync({log:?}, JSON.stringify(ctx.modules.map((m) => m.id ?? null)) + "\n");
    }},
  }}],
}};
"#,
            log = log.display().to_string()
        ),
    )
    .unwrap();
    let payload = serde_json::json!({
        "config": { "root": root.display().to_string(), "command": "serve", "mode": "development" },
        "env": { "command": "serve", "mode": "development" },
        "pluginsFormat": "vite",
    });
    let host = PluginHost::spawn(&root, &config, &payload.to_string(), None)
        .await
        .expect("the host spawns");
    let file = root.join("src/lib/secret.ts");
    std::fs::write(&file, "export const secret = () => \"server-secret\";\n").unwrap();
    let file = file.display().to_string();

    // The dev server's pass, whose graphs hold nothing for the file...
    host.handle_hot_update(&file, 1, "update", "[]")
        .await
        .unwrap();
    // ...then the dev engine's, before its rebuild: the same change.
    host.import_protection_changed(std::slice::from_ref(&file))
        .await
        .unwrap();
    host.shutdown();

    let passes: Vec<Vec<Option<String>>> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    crate::plugins::test_roots::remove_root(&root);
    assert_eq!(
        passes,
        [vec![Some(file)]],
        "one pass per change, and it names the changed file"
    );
}
