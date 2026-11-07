// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The deps pre-seed child (oj_server's assets/optimize-env.mjs), run the way
//! the dev server runs it: `oj start-script` on oj's own engine, its env as
//! JSON on stdin. The child runs Vite's dep optimizer for the server
//! environments so the plugin host, which builds the app's real
//! DevEnvironments in process, finds a warm cache and never runs a rolldown
//! build in oj's process. The acceptance is hash acceptance, not file
//! existence: after the seed, Vite's own DevEnvironment optimizer, resolved
//! the way the host resolves it, must load the cache and not re-optimize.
//! The app's Vite is the start-app fixture's; without it the tests skip,
//! unless `OJ_REQUIRE_FIXTURES` is set.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::SystemTime;

use serde_json::{json, Value};

fn assets() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../oj_server/src/assets")
        .canonicalize()
        .unwrap()
}

/// A scratch app sharing the start-app fixture's node_modules, removed on
/// drop.
struct App {
    tmp: PathBuf,
    root: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// The app, or None (after saying why) when the fixture is not installed.
/// `environments` is the body of the config's `environments` object.
fn app(name: &str, environments: &str) -> Option<App> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app")
        .canonicalize()
        .unwrap();
    let modules = fixture.join("node_modules");
    if !modules.join("vite/package.json").is_file() || !modules.join("react").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
        return None;
    }
    let tmp = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-optimize-env-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("app");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"oj-preseed-fixture","private":true,"type":"module"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();
    let app = App { tmp, root };
    // Its own cacheDir: the default would resolve through the node_modules
    // symlink into the shared fixture and collide across runs.
    app.config(environments);
    Some(app)
}

impl App {
    fn config(&self, environments: &str) {
        std::fs::write(
            self.root.join("vite.config.mjs"),
            format!(
                "export default {{\n  cacheDir: {},\n  environments: {{\n{environments}\n  }},\n}};\n",
                json!(self.path(".vite-cache").to_string_lossy())
            ),
        )
        .unwrap();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn snapshot_path(&self) -> PathBuf {
        self.path("preseed-include.json")
    }

    fn metadata_path(&self) -> PathBuf {
        self.path(".vite-cache/deps_ssr/_metadata.json")
    }

    fn read(&self, path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn report(&self) -> Value {
        self.read(&self.path("optimize-env-report.json"))
    }

    fn snapshot(&self) -> Value {
        self.read(&self.snapshot_path())
    }

    /// `oj start-script <script>` in this app with `env` on stdin.
    fn script(&self, script: &Path, env: &[(&str, String)]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oj"))
            .arg("start-script")
            .arg(script)
            .arg("--root")
            .arg(&self.root)
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        std::io::Write::write_all(
            child.stdin.as_mut().unwrap(),
            serde_json::to_string(&env).unwrap().as_bytes(),
        )
        .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    /// One pre-seed run, as preseed.rs spawns it; the snapshot at `snapshot`.
    fn seed_with(&self, snapshot: &Path) -> Output {
        self.script(
            &assets().join("optimize-env.mjs"),
            &[
                ("OJ_APP_ROOT", self.root.to_string_lossy().into_owned()),
                ("OJ_ENV_MODE", "dev".into()),
                (
                    "OJ_PRESEED_REPORT",
                    self.path("optimize-env-report.json")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "OJ_PRESEED_INCLUDE",
                    snapshot.to_string_lossy().into_owned(),
                ),
            ],
        )
    }

    /// A pre-seed run that must succeed; its stdout and stderr together.
    fn seed(&self) -> String {
        let out = self.seed_with(&self.snapshot_path());
        let log = log(&out);
        assert!(out.status.success(), "the seed failed:\n{log}");
        log
    }

    /// `body` as the `run(env)` of a script on oj's engine, `env.OUT` a file
    /// for its JSON answer.
    fn eval(&self, body: &str) -> Value {
        let script = self.path("check.mjs");
        let out_file = self.path("check-out.json");
        std::fs::write(
            &script,
            format!(
                "import {{ writeFileSync }} from \"node:fs\";\n\
                 import {{ pathToFileURL }} from \"node:url\";\n\
                 import {{ appViteEntry, asNode }} from {};\n\
                 import {{ viteLockfileHash }} from {};\n\
                 export async function run(env) {{\n\
                 const answer = await (async () => {{\n{body}\n}})();\n\
                 writeFileSync(env.OUT, JSON.stringify(answer ?? null));\n}}\n",
                json!(assets().join("vite-config-node.mjs").to_string_lossy()),
                json!(assets().join("discovered-deps.mjs").to_string_lossy()),
            ),
        )
        .unwrap();
        let out = self.script(
            &script,
            &[
                ("OUT", out_file.to_string_lossy().into_owned()),
                ("ROOT", self.root.to_string_lossy().into_owned()),
            ],
        );
        assert!(
            out.status.success(),
            "the check script failed:\n{}",
            log(&out)
        );
        let answer = self.read(&out_file);
        let _ = std::fs::remove_file(&out_file);
        answer
    }

    /// Vite's lockfile hash for this app, by the mirror both sides use.
    fn lockfile_hash(&self) -> String {
        self.eval("return viteLockfileHash(env.ROOT);")
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A prior session's metadata the way Vite leaves it on disk: a
    /// committed dep set under `optimized`, keyed by a lockfile hash.
    fn plant_ssr_metadata(&self, ids: &[&str], lockfile_hash: &str) {
        let optimized: serde_json::Map<String, Value> =
            ids.iter().map(|id| (id.to_string(), json!({}))).collect();
        write_json(
            &self.metadata_path(),
            &json!({
                "lockfileHash": lockfile_hash,
                "configHash": "deadbeef",
                "optimized": optimized,
                "chunks": {},
            }),
        );
    }

    fn plant_snapshot(&self, environments: Value) {
        write_json(
            &self.snapshot_path(),
            &json!({ "lockfile": self.lockfile_hash(), "environments": environments }),
        );
    }
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

fn log(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

fn seeded_names(report: &Value) -> Vec<String> {
    report["seeded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

const SSR_REACT: &str = r#"    ssr: { optimizeDeps: { include: ["react"] } },"#;

#[test]
fn the_seed_writes_a_deps_cache_vites_own_optimizer_accepts_as_warm() {
    let Some(app) = app("warm", SSR_REACT) else {
        return;
    };
    app.seed();
    let meta_path = app.metadata_path();
    let meta = app.read(&meta_path);
    assert!(
        meta["optimized"]["react"].is_object(),
        "react pre-bundled for ssr: {meta}"
    );
    assert!(
        meta["lockfileHash"].is_string(),
        "metadata carries Vite's lockfileHash"
    );
    assert!(
        meta["configHash"].is_string(),
        "metadata carries Vite's configHash"
    );
    let before = mtime(&meta_path);

    // The report for the parent's stamp: the seeded env and its metadata.
    let report = app.report();
    assert_eq!(report["failed"], json!(false));
    assert_eq!(seeded_names(&report), ["ssr"]);
    assert_eq!(
        report["seeded"][0]["metadataPath"],
        json!(meta_path.to_string_lossy())
    );

    // The acceptance: the config resolved the way the plugin host's
    // buildEnvironments resolves it, the in-host environment's own optimizer
    // init LOADS the cache. A re-optimization would rewrite the metadata.
    let loaded = app.eval(
        r#"// The app's Vite as the plugin host loads it (its client paths feed the hash).
const vite = await import(pathToFileURL(appViteEntry(env.ROOT)).href);
const rc = await asNode(() =>
  vite.resolveConfig({ root: env.ROOT, configFile: undefined, mode: "dev" }, "serve", "development", "development"),
);
const de = new vite.DevEnvironment("ssr", rc, { hot: false });
if (!de.depsOptimizer) return { optimizer: false };
await de.init();
await de.depsOptimizer.init();
const react = !!de.depsOptimizer.metadata.optimized.react;
await de.close();
return { optimizer: true, react };"#,
    );
    assert_eq!(loaded, json!({ "optimizer": true, "react": true }));
    assert_eq!(
        mtime(&meta_path),
        before,
        "the in-host optimizer loaded the seeded cache instead of re-optimizing"
    );

    // A second seed against the warm cache: also a load, never a rebuild.
    app.seed();
    assert_eq!(mtime(&meta_path), before, "a warm re-seed is a no-op");

    // The replay gate matches Vite's lockfile hash in VALUE: a drifted mirror
    // would leave the snapshot's environments empty, disabling the replay.
    assert_eq!(json!(app.lockfile_hash()), meta["lockfileHash"]);
    assert_eq!(app.snapshot()["environments"]["ssr"], json!(["react"]));
}

#[test]
fn environments_without_an_enabled_optimizer_seed_nothing_and_report_cleanly() {
    // No include list: Vite disables the ssr optimizer (noDiscovery is the
    // server consumers' default), so there is nothing to pre-seed.
    let Some(app) = app("disabled", "") else {
        return;
    };
    app.seed();
    assert!(
        !app.path(".vite-cache/deps_ssr").exists(),
        "no ssr cache invented"
    );
    let report = app.report();
    assert_eq!(report["failed"], json!(false));
    assert_eq!(report["seeded"], json!([]));
}

// The wedge, pinned: a prior session's metadata names a dep whose package
// still resolves but whose exports map no longer answers the subpath. As an
// include it would make Vite's optimizer THROW, and the thrown optimize is the
// one thing that could age the id out. It never reaches the include: Vite's
// include resolver throws on it, so the replay leaves it out.
#[test]
fn a_replayed_dep_id_the_exports_map_now_refuses_never_wedges_the_seed() {
    let Some(app) = app("refused", SSR_REACT) else {
        return;
    };
    // Same install state (the current hash): the lockfile gate must not eat it.
    app.plant_ssr_metadata(&["react", "react/presets"], &app.lockfile_hash());
    app.seed();
    let report = app.report();
    assert_eq!(report["failed"], json!(false));
    assert_eq!(seeded_names(&report), ["ssr"]);
    let meta = app.read(&app.metadata_path());
    assert!(
        meta["optimized"]["react"].is_object(),
        "the user's own include still seeds"
    );
    assert!(
        meta["optimized"]["react/presets"].is_null(),
        "the refused id is not committed"
    );
    assert!(
        !app.snapshot().to_string().contains("react/presets"),
        "the snapshot does not replay it"
    );
}

// The guard on the heal: when the snapshot cannot be rewritten, the seed must
// fail rather than retry to success. The host folds the same file, so a warm
// stamp over a snapshot that still replays a toxic set would skip the child
// (and the heal) on every later boot.
#[test]
fn a_heal_that_cannot_rewrite_the_snapshot_fails_the_seed() {
    use std::os::unix::fs::PermissionsExt;
    let Some(app) = app("unhealable", SSR_REACT) else {
        return;
    };
    let dir = app.path("snap");
    std::fs::create_dir_all(&dir).unwrap();
    let snapshot = dir.join("preseed-include.json");
    write_json(
        &snapshot,
        &json!({ "lockfile": app.lockfile_hash(), "environments": { "ssr": ["react", "react/presets"] } }),
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = app.seed_with(&snapshot);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !out.status.success(),
        "an unhealable snapshot fails the seed, so no warm stamp covers it:\n{}",
        log(&out)
    );
    assert!(
        app.read(&snapshot).to_string().contains("react/presets"),
        "the file still replays, and nothing pretended otherwise"
    );
}

// Metadata from another install state (Vite itself discards it by its
// lockfileHash) is no source of replayable ids at all.
#[test]
fn metadata_from_a_previous_install_state_is_not_replayed() {
    let Some(app) = app("stale", SSR_REACT) else {
        return;
    };
    app.plant_ssr_metadata(&["react", "react/presets"], "00000000");
    app.seed();
    let report = app.report();
    assert_eq!(report["failed"], json!(false));
    assert_eq!(seeded_names(&report), ["ssr"]);
    assert!(
        !app.snapshot().to_string().contains("react/presets"),
        "stale ids never enter the snapshot"
    );
}

// Vite resolves an include id from the root with no importer and warns
// "Failed to resolve dependency" when it cannot, so the replay carries only
// ids that resolve there: not a dep that is gone, not a subpath its exports
// map refuses (the resolver throws), including entries the previous snapshot
// kept for an environment with no metadata on disk.
#[test]
fn the_replay_keeps_only_ids_vites_include_resolver_reaches_from_the_root() {
    let Some(app) = app("resolver", SSR_REACT) else {
        return;
    };
    app.plant_snapshot(json!({ "ssr": ["react", "react/presets", "oj-not-installed-dep"] }));
    let log = app.seed();
    assert_eq!(app.snapshot()["environments"]["ssr"], json!(["react"]));
    assert!(
        !log.contains("Failed to resolve dependency")
            && !log.contains("Cannot optimize dependency"),
        "{log}"
    );
}

// An environment other than client and ssr resolves through Vite's
// createIdResolver over that environment (createBackCompatIdResolver), and
// the replay keeps the same ids for it.
#[test]
fn a_custom_environments_replay_keeps_only_ids_its_resolver_reaches() {
    let Some(app) = app(
        "custom",
        r#"    worker: { consumer: "server", optimizeDeps: { include: ["react"] } },"#,
    ) else {
        return;
    };
    app.plant_snapshot(json!({ "worker": ["react", "react/presets", "oj-not-installed-dep"] }));
    let log = app.seed();
    assert_eq!(app.snapshot()["environments"]["worker"], json!(["react"]));
    assert_eq!(seeded_names(&app.report()), ["worker"]);
    assert!(
        !log.contains("Failed to resolve dependency")
            && !log.contains("Cannot optimize dependency"),
        "{log}"
    );
}
