// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Optimizer quarantine: pre-seed Vite's per-environment deps caches in a
//! one-shot child so the plugin host never runs a dep optimization — a
//! rolldown `build()` whose napi binding retains native memory at PROCESS
//! scope (only exit releases it) — inside oj's own process.
//!
//! A runner-backed config makes the host build the app's real Vite
//! DevEnvironments; a cold cache then makes Vite optimize in-host. The child
//! (`oj start-script` + `optimize-env.mjs`) runs the SAME optimization with
//! the app's own Vite, so the cache it writes is hash-identical to what the
//! in-host check expects; the host then loads it and skips its pass.
//!
//! Cold/warm is decided without imitating Vite's hashes: Vite's own code (in
//! the child) does all hash work, and Rust only detects change since the last
//! seed — the extraction cache freshness (config inputs), a lockfile stamp
//! (Rust-to-Rust comparison), and a digest of each seeded metadata file. Every
//! failure mode of this gate leans cold, whose only cost is running the child,
//! which performs Vite's exact check anyway.
//!
//! Known gaps, on purpose: a mid-session re-optimization (lockfile edit while
//! serving, a discovery environment finding new deps) still runs in-host, and
//! `optimizeDeps.force` re-optimizes in-host regardless of any seed. Both
//! degrade to today's behavior, never to breakage.

use std::path::{Path, PathBuf};

pub(crate) const OPTIMIZE_ENV_JS: &str = include_str!("assets/optimize-env.mjs");

const STAMP_FILE: &str = "optimize-env-stamp.json";
const REPORT_FILE: &str = "optimize-env-report.json";

/// Everything that invalidates a stamp besides the app's own inputs: the oj
/// version and the pre-seed script itself.
fn stamp_key() -> String {
    format!(
        "{}:{}",
        env!("CARGO_PKG_VERSION"),
        blake3::hash(OPTIMIZE_ENV_JS.as_bytes()).to_hex()
    )
}

/// A digest of the dependency-tree manifests: the shared memoized lockfile
/// digest (Vite-style lookup, walking up from the root). A superset of Vite's
/// own lockfile lookup on purpose: this stamp is only compared against itself,
/// so extra sensitivity can at worst re-run the child (which then performs
/// Vite's exact freshness check and exits).
fn lockfile_stamp(root: &Path) -> String {
    let lock = oj_cache::lockfile_digest(root);
    if lock.dir.is_none() {
        return "none".to_string();
    }
    lock.digest.to_hex().to_string()
}

fn file_digest(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| blake3::hash(&b).to_hex().to_string())
}

/// One environment the child seeded: where Vite committed its metadata.
#[derive(Debug)]
pub(crate) struct SeededEnv {
    name: String,
    metadata_path: PathBuf,
}

fn stamp_path(root: &Path) -> PathBuf {
    oj_cache::cache_root(root).join(STAMP_FILE)
}

/// Whether the last seed still covers this boot: same oj/script, same
/// dependency tree, and every seeded metadata file untouched since (the
/// in-host optimizer rewriting one — a mid-session re-optimization — reads
/// as cold, so the next boot re-seeds and self-heals the stamp).
pub(crate) fn stamp_is_warm(root: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(stamp_path(root)) else {
        return false;
    };
    let Ok(stamp) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    if stamp.get("key").and_then(|v| v.as_str()) != Some(stamp_key().as_str()) {
        return false;
    }
    if stamp.get("lockstamp").and_then(|v| v.as_str()) != Some(lockfile_stamp(root).as_str()) {
        return false;
    }
    let Some(envs) = stamp.get("envs").and_then(|v| v.as_array()) else {
        return false;
    };
    envs.iter().all(|e| {
        let path = e.get("metadataPath").and_then(|v| v.as_str());
        let hash = e.get("hash").and_then(|v| v.as_str());
        match (path, hash) {
            (Some(p), Some(h)) => file_digest(Path::new(p)).as_deref() == Some(h),
            _ => false,
        }
    })
}

pub(crate) fn write_stamp(root: &Path, seeded: &[SeededEnv]) {
    let mut envs = Vec::new();
    for s in seeded {
        let Some(hash) = file_digest(&s.metadata_path) else {
            // A metadata file that cannot be read back must not be stamped
            // warm; leaving the stamp absent re-runs the child next boot.
            return;
        };
        envs.push(serde_json::json!({
            "name": s.name,
            "metadataPath": s.metadata_path.to_string_lossy(),
            "hash": hash,
        }));
    }
    let stamp = serde_json::json!({
        "key": stamp_key(),
        "lockstamp": lockfile_stamp(root),
        "envs": envs,
    });
    let dir = oj_cache::cache_root(root);
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!("{STAMP_FILE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, stamp.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, stamp_path(root));
    }
}

fn parse_report(raw: &str) -> Option<Vec<SeededEnv>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("failed").and_then(|f| f.as_bool()) == Some(true) {
        return None;
    }
    let seeded = v.get("seeded")?.as_array()?;
    let mut out = Vec::new();
    for s in seeded {
        out.push(SeededEnv {
            name: s.get("name")?.as_str()?.to_string(),
            metadata_path: PathBuf::from(s.get("metadataPath")?.as_str()?),
        });
    }
    Some(out)
}

fn preseed_timeout() -> std::time::Duration {
    let secs = std::env::var("OJ_PRESEED_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

async fn run_child(root: &Path, env_mode: &str) -> anyhow::Result<Vec<SeededEnv>> {
    let cache = oj_cache::cache_root(root);
    std::fs::create_dir_all(&cache)?;
    // Atomic rename: an engine could import the script while a concurrent oj
    // process rewrites it.
    let script = cache.join("optimize-env.mjs");
    if std::fs::read(&script).ok().as_deref() != Some(OPTIMIZE_ENV_JS.as_bytes()) {
        let tmp = cache.join(format!("optimize-env-{}.tmp.mjs", std::process::id()));
        std::fs::write(&tmp, OPTIMIZE_ENV_JS)?;
        std::fs::rename(&tmp, &script)?;
    }
    let report = cache.join(REPORT_FILE);
    let _ = std::fs::remove_file(&report);
    let exe = std::env::current_exe()?;
    // The one-shot `oj start-script` child (the client-rebundle pattern): a
    // fresh embedded engine in a process of its own, whose exit is what
    // releases rolldown's native retention. The env pairs travel on stdin
    // (argv would print values in `ps`).
    let mut child = tokio::process::Command::new(exe)
        .arg("start-script")
        .env("OJ_PARENT_PID", std::process::id().to_string())
        .arg(&script)
        .arg("--root")
        .arg(root)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let env: Vec<(String, String)> = vec![
            ("OJ_APP_ROOT".into(), root.to_string_lossy().into_owned()),
            ("OJ_ENV_MODE".into(), env_mode.to_string()),
            ("OJ_PRESEED_REPORT".into(), report.to_string_lossy().into_owned()),
        ];
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(serde_json::to_string(&env)?.as_bytes()).await?;
        // Dropping closes the pipe; the child's read_to_string completes.
    }
    let status = match tokio::time::timeout(preseed_timeout(), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "did not finish within {}s (raise OJ_PRESEED_TIMEOUT for slower machines)",
                preseed_timeout().as_secs()
            );
        }
    };
    if !status.success() {
        anyhow::bail!("child exited with {status}");
    }
    let raw = std::fs::read_to_string(&report)
        .map_err(|e| anyhow::anyhow!("child wrote no report: {e}"))?;
    parse_report(&raw).ok_or_else(|| anyhow::anyhow!("child reported an incomplete seed"))
}

/// Pre-seed the deps caches Vite would otherwise build inside the plugin
/// host. Called only for runner-backed vite configs, before the host spawns.
/// Failure is never fatal: the host's own optimizer remains the fallback
/// (correctness over memory), with one warning.
pub(crate) async fn preseed_server_deps(root: &Path, env_mode: &str) {
    if std::env::var("OJ_NO_DEPS_PRESEED").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    // A fresh config extraction means the config inputs changed (or were never
    // seen): Vite's configHash may have moved, which the stamp cannot see.
    if !crate::plugins::extraction_ran_fresh() && stamp_is_warm(root) {
        crate::boot_phase("deps preseed skipped (warm)");
        return;
    }
    crate::boot_phase("deps preseed begin");
    println!("  deps: pre-optimizing server environments in a one-shot child");
    match run_child(root, env_mode).await {
        Ok(seeded) => {
            write_stamp(root, &seeded);
            crate::boot_phase("deps preseed done");
        }
        Err(e) => {
            eprintln!(
                "oj: deps pre-optimization incomplete ({e}); the plugin host optimizes in-process this session"
            );
            crate::boot_phase("deps preseed failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The trigger's cold side: no stamp, a bad key, a moved lockfile, or a
    // touched metadata file each read as cold; the warm side needs all of
    // them intact. Cold is always safe (the child performs Vite's own exact
    // check), so these pin the sensitivity, not just the happy path.
    #[test]
    fn stamp_warm_requires_key_lockstamp_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("package-lock.json"), b"{\"v\":1}").unwrap();
        let deps = root.join("node_modules/.vite/deps_ssr");
        std::fs::create_dir_all(&deps).unwrap();
        let meta = deps.join("_metadata.json");
        std::fs::write(&meta, b"{\"hash\":\"abc\"}").unwrap();

        // No stamp: cold.
        assert!(!stamp_is_warm(root));

        write_stamp(
            root,
            &[SeededEnv { name: "ssr".into(), metadata_path: meta.clone() }],
        );
        assert!(stamp_is_warm(root));

        // The dependency tree changed: cold.
        std::fs::write(root.join("package-lock.json"), b"{\"v\":2}").unwrap();
        assert!(!stamp_is_warm(root));
        std::fs::write(root.join("package-lock.json"), b"{\"v\":1}").unwrap();
        assert!(stamp_is_warm(root));

        // The seeded metadata was rewritten (an in-host or foreign
        // re-optimization): cold, so the next boot re-seeds and self-heals.
        std::fs::write(&meta, b"{\"hash\":\"other\"}").unwrap();
        assert!(!stamp_is_warm(root));

        // The metadata is gone entirely (rm -rf node_modules/.vite): cold.
        std::fs::remove_file(&meta).unwrap();
        assert!(!stamp_is_warm(root));
    }

    // An empty seed (no environment optimizes) is still a valid warm stamp:
    // without it, such apps would pay the child's config resolve every boot.
    #[test]
    fn empty_seed_stamps_warm_until_lockfile_moves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("yarn.lock"), b"a").unwrap();
        write_stamp(root, &[]);
        assert!(stamp_is_warm(root));
        std::fs::write(root.join("yarn.lock"), b"b").unwrap();
        assert!(!stamp_is_warm(root));
    }

    // A stamp written by another oj version (or another copy of the script)
    // never serves: the seed's semantics may have changed with it.
    #[test]
    fn stamp_key_mismatch_is_cold() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_stamp(root, &[]);
        let p = stamp_path(root);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["key"] = serde_json::json!("other-version:hash");
        std::fs::write(&p, v.to_string()).unwrap();
        assert!(!stamp_is_warm(root));
    }

    // The lockfile lookup walks up like Vite's, so a workspace member whose
    // lockfile lives at the monorepo root is still change-sensitive.
    #[test]
    fn lockfile_stamp_walks_up_and_tracks_content() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let app = ws.join("apps/web");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(ws.join("pnpm-lock.yaml"), b"one").unwrap();
        let a = lockfile_stamp(&app);
        assert_ne!(a, "none");
        std::fs::write(ws.join("pnpm-lock.yaml"), b"two").unwrap();
        let b = lockfile_stamp(&app);
        assert_ne!(a, b);
        // No manifest anywhere: a stable "none", not an error.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(lockfile_stamp(bare.path()), "none");
    }

    // The child's report is trusted only when whole: a failed or malformed
    // report yields no seed list, so no stamp is written and the next boot
    // stays cold.
    #[test]
    fn report_parsing_rejects_failed_or_malformed() {
        assert!(parse_report("{\"seeded\":[],\"failed\":true}").is_none());
        assert!(parse_report("not json").is_none());
        assert!(parse_report("{\"seeded\":[{\"name\":\"ssr\"}],\"failed\":false}").is_none());
        let ok = parse_report(
            "{\"seeded\":[{\"name\":\"ssr\",\"metadataPath\":\"/x/_metadata.json\"}],\"failed\":false}",
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].name, "ssr");
    }
}
