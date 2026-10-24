// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::Path;

/// Hash of every input outside the module closure that changes the client
/// bundle. Order of the files and env pairs is part of the key and must not
/// change.
pub(super) fn epoch(root: &Path, mode: &str) -> String {
    epoch_in(oj_env::get(), root, mode)
}

/// [`epoch`] with the process env given as `env`.
pub(super) fn epoch_in(env: &oj_env::Env, root: &Path, mode: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    let mode_env = [format!(".env.{mode}"), format!(".env.{mode}.local")];
    for name in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "package.json",
        "vite.config.ts",
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.mts",
        "vite.config.cjs",
        "vite.config.cts",
        "oj.config.ts",
        "oj.config.js",
        "oj.config.mjs",
        ".env",
        ".env.local",
        mode_env[0].as_str(),
        mode_env[1].as_str(),
    ] {
        if let Ok(bytes) = fs::read(root.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&[0]);
            hasher.update(&bytes);
        }
    }
    // NODE_ENV decides DEV/PROD and the React build, as under Vite.
    // Env::vars() is sorted by name, as the old explicit sort was.
    let env = env
        .vars()
        .filter(|(k, _)| k.starts_with("VITE_") || *k == "TSS_SERVER_FN_BASE" || *k == "NODE_ENV");
    for (k, v) in env {
        hasher.update(b"\0e");
        hasher.update(k.as_bytes());
        hasher.update(&[0]);
        hasher.update(v.as_bytes());
    }
    // Which `src/start.*` the `#tanstack-start-entry` alias probes to (the
    // client bundle's ladder; a package.json `imports` target rides the
    // package.json hash above). The chosen file's CONTENT is a closure input
    // once bundled, but its presence and identity are not: a `src/start.ts`
    // created after a stub bundle was cached leaves every old closure file
    // unchanged, and the stale stub bundle would restore on the next boot.
    hasher.update(b"\0start-entry\0");
    for ext in [".ts", ".js", ".mts", ".mjs", ".tsx", ".jsx"] {
        let name = format!("src/start{ext}");
        if root.join(&name).is_file() {
            hasher.update(name.as_bytes());
            break;
        }
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oj_env::Env;

    #[test]
    fn epoch_keys_on_vite_and_node_env_vars_only() {
        let root = std::env::temp_dir().join(format!("oj-epoch-{}", std::process::id()));
        let of = |vars: &[(&str, &str)]| {
            epoch_in(&Env::from_vars(vars.iter().copied()), &root, "development")
        };
        let base = of(&[]);
        assert_eq!(base, of(&[("UNRELATED", "1")]));
        assert_ne!(base, of(&[("VITE_API", "x")]));
        assert_ne!(base, of(&[("NODE_ENV", "production")]));
        assert_ne!(base, of(&[("TSS_SERVER_FN_BASE", "/fn")]));
        assert_eq!(base, of(&[("TSS_OTHER", "1")]));
    }

    // The probed start entry is a bundle input outside the closure: a
    // src/start.ts created after a stub bundle was cached must re-key, or the
    // stale stub restores on the next boot and the app's createStart options
    // silently stay dropped client-side.
    #[test]
    fn epoch_keys_on_the_probed_start_entry() {
        let root = std::env::temp_dir().join(format!("oj-start-epoch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let env = Env::from_vars(std::iter::empty::<(&str, &str)>());
        let of = || epoch_in(&env, &root, "development");

        let without = of();
        std::fs::write(
            root.join("src/start.ts"),
            "export const startInstance = 1;\n",
        )
        .unwrap();
        let with_ts = of();
        assert_ne!(
            without, with_ts,
            "a created src/start.ts re-keys the bundle"
        );

        // Same presence, same choice: content changes are the closure's job.
        std::fs::write(
            root.join("src/start.ts"),
            "export const startInstance = 2;\n",
        )
        .unwrap();
        assert_eq!(with_ts, of());

        std::fs::remove_file(root.join("src/start.ts")).unwrap();
        assert_eq!(without, of(), "a delete re-keys back to the stub bundle");
    }
}
