// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::Path;

/// The rolldown vendored next to the binary for oj's own Start bundles. The
/// runtime variable wins over the nix build's compile-time store path; a
/// set-but-empty variable is an explicit opt-out (the app's copy serves).
///
/// A configured vendor is resolved once, here, to the identity of the
/// rolldown that will actually build (the version read from exactly
/// `<path>/node_modules/rolldown/package.json`, never node's walk-up). That
/// identity salts the bundle cache key, the way Vite's dep cache keys on the
/// lockfile pinning its bundler. A vendor that is set but holds no rolldown
/// is `Broken`: the command fails loudly before any bundle runs, and its
/// salt is distinct so a broken window never aliases a real one.
pub enum VendoredRolldown {
    None,
    Broken { path: String, reason: String },
    Resolved { path: String, version: String },
}

/// Resolved once per process: resolving at each call site would let an
/// in-place vendor change slip between the cache-key read and the script-env
/// read, persisting one rolldown's bundle under another's key.
pub fn vendored_rolldown() -> &'static VendoredRolldown {
    static RESOLVED: std::sync::OnceLock<VendoredRolldown> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(resolve_vendored_rolldown)
}

/// The configured vendor path, or the variant to return without probing.
fn configured_vendor() -> Result<String, VendoredRolldown> {
    match std::env::var_os("OJ_VENDORED_ROLLDOWN") {
        Some(v) if v.is_empty() => Err(VendoredRolldown::None),
        Some(v) => v.into_string().map_err(|raw| VendoredRolldown::Broken {
            path: raw.to_string_lossy().into_owned(),
            reason: "the value is not valid UTF-8".into(),
        }),
        None => match option_env!("OJ_VENDORED_ROLLDOWN").filter(|v| !v.is_empty()) {
            Some(v) => Ok(v.to_string()),
            None => Err(VendoredRolldown::None),
        },
    }
}

fn resolve_vendored_rolldown() -> VendoredRolldown {
    let configured = match configured_vendor() {
        Ok(path) => path,
        Err(resolved) => return resolved,
    };
    // Absolute and symlink-free: the bundle scripts run with cwd at the app
    // root, where a relative path would name a different directory. A path
    // that cannot canonicalize is kept verbatim and fails below.
    let configured = fs::canonicalize(&configured)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(configured);
    let pkg = Path::new(&configured)
        .join("node_modules")
        .join("rolldown")
        .join("package.json");
    let bytes = match fs::read(&pkg) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return VendoredRolldown::Broken {
                path: configured,
                reason: "it has no node_modules/rolldown".into(),
            }
        }
        Err(e) => {
            return VendoredRolldown::Broken {
                path: configured,
                reason: format!("node_modules/rolldown/package.json is unreadable: {e}"),
            }
        }
    };
    let version = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|pkg| pkg.get("version")?.as_str().map(str::to_string));
    match version {
        Some(version) => VendoredRolldown::Resolved {
            path: configured,
            version,
        },
        None => VendoredRolldown::Broken {
            path: configured,
            reason: "node_modules/rolldown/package.json has no parseable version".into(),
        },
    }
}

impl VendoredRolldown {
    /// The cache-key input: what will actually build the bundle. Variants are
    /// tagged so a version string that happens to read "broken" can never
    /// alias the Broken salt.
    pub(super) fn epoch_input(&self) -> Option<String> {
        match self {
            VendoredRolldown::None => None,
            VendoredRolldown::Broken { path, .. } => Some(format!("broken\0{path}")),
            VendoredRolldown::Resolved { path, version } => {
                Some(format!("resolved\0{version}\0{path}"))
            }
        }
    }
}

/// Hash of every input outside the module closure that changes the client
/// bundle. Order of the files, env pairs and vendor identity is part of the
/// key and must not change.
pub(super) fn epoch(root: &Path, mode: &str, vendored_rolldown_identity: Option<&str>) -> String {
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
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("VITE_") || k == "TSS_SERVER_FN_BASE" || k == "NODE_ENV")
        .collect();
    env.sort();
    for (k, v) in env {
        hasher.update(b"\0e");
        hasher.update(k.as_bytes());
        hasher.update(&[0]);
        hasher.update(v.as_bytes());
    }
    // A vendor change, including an in-place upgrade at the same path, must
    // not restore a bundle another rolldown produced.
    if let Some(identity) = vendored_rolldown_identity {
        hasher.update(b"\0vendored-rolldown\0");
        hasher.update(identity.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}
