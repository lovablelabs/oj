//! Vite's exports-first entry for a directory reached by path.
//!
//! Vite resolves such a directory through its manifest's `exports["."]` before
//! the mainFields walk (resolvePackageEntry); Node, and so oxc_resolver, binds
//! `exports` only at a package-name boundary. Once `exports` names an entry,
//! Vite never falls back to mainFields: a missing target or no derivable "."
//! target goes to `index.*`, then fails.

use std::path::{Path, PathBuf};

use oxc_resolver::Resolver;

use crate::spec::{lexical_join, strip_query};

/// How the directory-entry override landed for a specifier.
pub(crate) enum DirectoryEntry {
    /// Not a directory hit, or the manifest has no truthy `exports`.
    NotApplicable,
    /// The exports root target, or the directory's index when it is missing.
    Resolved(PathBuf),
    /// Neither the exports target nor an index exists.
    Unresolvable(PathBuf),
}

/// Applies the override when `resolved` came out of a directory the
/// path-shaped `specifier` named. Only real directory hits read a manifest: an
/// exact file hit is the equality case and an extensionless hit never lands
/// inside its own specifier's directory. Symlinked directories miss the
/// lexical gate and keep Node's behavior.
pub(crate) fn directory_entry(
    resolver: &Resolver,
    base: &Path,
    specifier: &str,
    resolved: &Path,
) -> DirectoryEntry {
    let path_shaped =
        specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/');
    if !path_shaped {
        return DirectoryEntry::NotApplicable;
    }
    let dir = lexical_join(base, strip_query(specifier));
    if resolved == dir || !resolved.starts_with(&dir) {
        return DirectoryEntry::NotApplicable;
    }
    let Some(exports) = manifest_exports(&dir) else {
        return DirectoryEntry::NotApplicable;
    };
    let conditions = &resolver.options().condition_names;
    let entry = exports_dot_target(&exports, conditions)
        .and_then(|target| resolver.resolve(&dir, &target).ok())
        .or_else(|| resolver.resolve(&dir, "./index").ok());
    match entry {
        Some(resolution) => DirectoryEntry::Resolved(resolution.full_path()),
        None => DirectoryEntry::Unresolvable(dir),
    }
}

/// `dir/package.json`'s `exports` when JS-truthy (Vite's `if (data.exports)`).
fn manifest_exports(dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(dir.join("package.json")).ok()?;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match manifest.get_mut("exports")?.take() {
        serde_json::Value::Null | serde_json::Value::Bool(false) => None,
        serde_json::Value::String(s) if s.is_empty() => None,
        exports => Some(exports),
    }
}

/// Node's PACKAGE_EXPORTS_RESOLVE for the "." subpath under `conditions` plus
/// the always-matching "default", walking condition maps in source order and
/// arrays first-hit (Vite's resolveExportsOrImports). Source order holds
/// because oxc_resolver enables serde_json's preserve_order. A bare string
/// target (`"lib.js"`) is joined onto the directory like Vite does.
pub(crate) fn exports_dot_target(
    exports: &serde_json::Value,
    conditions: &[String],
) -> Option<String> {
    match exports {
        serde_json::Value::String(target) if target.is_empty() => None,
        serde_json::Value::String(target) => {
            if target.starts_with("./") || target.starts_with("../") {
                Some(target.clone())
            } else {
                Some(format!("./{target}"))
            }
        }
        serde_json::Value::Array(entries) => entries
            .iter()
            .find_map(|entry| exports_dot_target(entry, conditions)),
        // Subpath map (dotted keys) or condition map; Node forbids mixing.
        serde_json::Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
            exports_dot_target(map.get(".")?, conditions)
        }
        serde_json::Value::Object(map) => map.iter().find_map(|(key, value)| {
            (key == "default" || conditions.iter().any(|c| c == key))
                .then(|| exports_dot_target(value, conditions))
                .flatten()
        }),
        _ => None,
    }
}
