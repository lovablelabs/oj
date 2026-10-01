// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The shared content store: cache entries whose keys and artifacts are
//! worktree-independent live here, one directory per user rather than per
//! app, so a second worktree of the same app (or a second app on the same
//! dependencies) starts warm instead of rebuilding content that already
//! exists on this machine.
//!
//! Only a store whose keys and artifacts carry no absolute paths may move
//! here. Everything else stays under the per-app [`crate::cache_root`]:
//! scratch scripts that resolve bare imports relative to where they live,
//! per-root pointer state (`current` files, memo maps), and any artifact
//! that still embeds the app root.

use std::path::{Path, PathBuf};

/// Bumped when the layout of the shared store changes shape; an older layout
/// is simply a different directory, never misread.
pub const STORE_VERSION: u32 = 1;

/// The shared store directory, or `None` when the environment offers no
/// stable per-user location, in which case callers fall back to the per-app
/// `cache_root` and lose sharing, never correctness. The base is
/// `OJ_STORE_DIR`, else `$XDG_CACHE_HOME/oj/store`, else
/// `$HOME/.cache/oj/store`; an empty `OJ_STORE_DIR` turns sharing off, for
/// builds that must not write outside the tree they were pointed at.
pub fn store_root() -> Option<PathBuf> {
    store_root_in(oj_env::get().knobs.store_dir.as_deref())
}

/// [`store_root`] with the base dir given: the version segment is appended,
/// mirroring `cache_root`.
pub fn store_root_in(base: Option<&Path>) -> Option<PathBuf> {
    Some(base?.join(format!("v{STORE_VERSION}")))
}

/// Root-relative identity for a path under `root`, with forward slashes on
/// every platform: the form that makes a key or artifact mean the same thing
/// in every worktree. A path outside `root` comes back `None`; callers treat
/// that as "not shareable" explicitly rather than silently keying on an
/// absolute path.
pub fn rel_key(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        Some(s.into_owned())
    } else {
        Some(s.replace(std::path::MAIN_SEPARATOR, "/"))
    }
}

/// The workspace root an app belongs to: the FARTHEST ancestor that carries a
/// `node_modules` directory (pnpm/yarn workspaces hoist there), the app itself
/// when no ancestor does. Monorepo files outside the app root get their
/// worktree-independent identity relative to this.
pub fn workspace_root(app: &Path) -> PathBuf {
    let mut best = app.to_path_buf();
    let mut cur = app;
    while let Some(parent) = cur.parent() {
        if parent.join("node_modules").is_dir() {
            best = parent.to_path_buf();
        }
        cur = parent;
    }
    best
}

/// The worktree-independent spelling of a path for hashing into cache keys:
/// `ws:<rel>` under the app root, `mono:<rel>` under the workspace root (the
/// monorepo-sibling case that would otherwise poison a key with a
/// worktree-absolute path), the path verbatim when neither contains it.
pub fn salt_key(root: &Path, workspace: &Path, path: &Path) -> String {
    if let Some(rel) = rel_key(root, path) {
        return format!("ws:{rel}");
    }
    if let Some(rel) = rel_key(workspace, path) {
        return format!("mono:{rel}");
    }
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_root_is_versioned_or_off() {
        assert_eq!(
            store_root_in(Some(Path::new("/shared/oj"))),
            Some(Path::new("/shared/oj").join(format!("v{STORE_VERSION}")))
        );
        assert_eq!(store_root_in(None), None);
    }

    #[test]
    fn rel_key_is_root_relative_or_refused() {
        let root = Path::new("/work/a/app");
        assert_eq!(
            rel_key(root, Path::new("/work/a/app/src/App.tsx")).as_deref(),
            Some("src/App.tsx")
        );
        // Two worktrees, same file: same key.
        assert_eq!(
            rel_key(
                Path::new("/work/b/app"),
                Path::new("/work/b/app/src/App.tsx")
            ),
            rel_key(root, Path::new("/work/a/app/src/App.tsx")),
        );
        // Outside the root is refused, never keyed absolute by accident.
        assert_eq!(rel_key(root, Path::new("/elsewhere/x.ts")), None);
    }

    #[test]
    fn workspace_root_is_the_farthest_node_modules_ancestor() {
        let base = std::env::temp_dir().join(format!("oj-wsroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mono = base.join("mono");
        std::fs::create_dir_all(mono.join("node_modules")).unwrap();
        std::fs::create_dir_all(mono.join("apps/web/node_modules")).unwrap();
        assert_eq!(
            workspace_root(&mono.join("apps/web")),
            mono,
            "farthest wins"
        );
        // No ancestor with node_modules: the app is its own workspace.
        let lone = base.join("lone/app");
        std::fs::create_dir_all(&lone).unwrap();
        assert_eq!(workspace_root(&lone), lone);
    }

    #[test]
    fn salt_key_spellings_are_worktree_independent() {
        let a_root = Path::new("/wt-a/mono/app");
        let a_ws = Path::new("/wt-a/mono");
        let b_root = Path::new("/somewhere/else/b/mono/app");
        let b_ws = Path::new("/somewhere/else/b/mono");
        // In-root, monorepo-sibling, and external: the first two agree across
        // worktrees, the third stays absolute (and so, correctly, disagrees).
        assert_eq!(
            salt_key(a_root, a_ws, Path::new("/wt-a/mono/app/src/x.ts")),
            salt_key(
                b_root,
                b_ws,
                Path::new("/somewhere/else/b/mono/app/src/x.ts")
            ),
        );
        assert_eq!(
            salt_key(
                a_root,
                a_ws,
                Path::new("/wt-a/mono/packages/common/src/index.ts")
            ),
            "mono:packages/common/src/index.ts"
        );
        assert_eq!(
            salt_key(a_root, a_ws, Path::new("/wt-a/mono/packages/x.ts")),
            salt_key(
                b_root,
                b_ws,
                Path::new("/somewhere/else/b/mono/packages/x.ts")
            ),
        );
        assert_eq!(
            salt_key(a_root, a_ws, Path::new("/opt/external/x.ts")),
            "/opt/external/x.ts"
        );
        // A sibling-prefix workspace must not relativize (root + "/" rule).
        assert_eq!(
            salt_key(a_root, a_ws, Path::new("/wt-a/mono2/x.ts")),
            "/wt-a/mono2/x.ts"
        );
    }
}
