// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite resolves the config once per start (`resolveConfig`): the config
//! file is evaluated once, each plugin factory called once, and `config`,
//! `configEnvironment` and `configResolved` run once each. oj read the config
//! in one engine and ran it again in the plugin host, so a cold start ran all
//! of it twice.
//!
//! Each test logs the config's work under oj and under the real Vite (the
//! start-app fixture's install) and asserts the same counts. Skips without
//! the fixture, unless `OJ_REQUIRE_FIXTURES`.

#[allow(dead_code)]
mod start_app;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use start_app::{free_port, Server};

fn fixture_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite/bin/vite.js").is_file());
    if dir.is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
    }
    dir
}

/// The spy config: `{{LOG}}` names the log, `{{EXTRA}}` more config fields.
const CONFIG: &str = r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync("{{LOG}}", line + "\n");
log("file");
const spy = () => {
  log("factory");
  return {
    name: "spy",
    config() { log("config"); },
    configEnvironment(name) { log(`configEnvironment ${name}`); },
    configResolved() { log("configResolved"); },
  };
};
export default { plugins: [spy()]{{EXTRA}} };
"#;

/// A scratch SPA whose config is the spy, sharing the fixture's install.
struct Spa {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
}

impl Spa {
    fn new(extra: &str) -> Option<Spa> {
        let modules = fixture_modules()?;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let log = root.join("spy.log");
        for (file, body) in [
            ("package.json", r#"{"name":"spy","type":"module"}"#.to_string()),
            (
                "index.html",
                "<!doctype html><html><body><script type=\"module\" src=\"/src/main.ts\"></script></body></html>"
                    .to_string(),
            ),
            ("src/main.ts", "console.log(1);\n".to_string()),
            ("vite.config.js", Spa::config(&log, extra)),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();
        Some(Spa {
            _tmp: tmp,
            root,
            log,
        })
    }

    fn config(log: &Path, extra: &str) -> String {
        CONFIG
            .replace("{{LOG}}", &log.display().to_string())
            .replace("{{EXTRA}}", extra)
    }

    /// The log so far, as counts, and a fresh one after.
    fn take(&self) -> BTreeMap<String, usize> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let _ = std::fs::remove_file(&self.log);
        let mut out = BTreeMap::new();
        for line in text.lines() {
            *out.entry(line.to_string()).or_default() += 1;
        }
        out
    }

    /// A clean slate for the next tool: no caches, no log.
    fn reset(&self) {
        let _ = std::fs::remove_dir_all(self.root.join(".oj-cache"));
        let _ = std::fs::remove_dir_all(self.root.join("node_modules/.vite"));
        let _ = std::fs::remove_dir_all(self.root.join("dist"));
        let _ = std::fs::remove_file(&self.log);
    }
}

/// Vite's dev server, once it answers.
struct ViteDev(Child, u16);

impl ViteDev {
    fn start(root: &Path) -> ViteDev {
        let port = free_port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args(["dev", "--host", "127.0.0.1", "--strictPort", "--port"])
            .arg(port.to_string())
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
        ViteDev(child, port)
    }

    fn get(&self, path: &str) {
        let _ = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-m", "30"])
            .arg(format!("http://127.0.0.1:{}{path}", self.1))
            .status();
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits until the log holds `lines` lines (or a deadline), then a little more.
fn settle(log: &Path, lines: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .count()
        < lines
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(1000));
}

fn lines(counts: &BTreeMap<String, usize>) -> usize {
    counts.values().sum()
}

#[test]
fn a_dev_start_resolves_the_config_once_as_in_vite() {
    let Some(app) = Spa::new("") else {
        return;
    };
    let vite = ViteDev::start(&app.root);
    vite.get("/src/main.ts");
    drop(vite);
    let theirs = app.take();
    assert_eq!(theirs.get("file"), Some(&1), "vite: {theirs:?}");

    app.reset();
    let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
    dev.request("GET", "/src/main.ts", &[]);
    settle(&app.log, lines(&theirs));
    let ours = app.take();
    drop(dev);
    assert_eq!(ours, theirs, "oj dev vs vite dev");
}

#[test]
fn a_build_resolves_the_config_once_as_in_vite() {
    let Some(app) = Spa::new("") else {
        return;
    };
    let run = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(Command::new("node")
        .arg(app.root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&app.root));
    let theirs = app.take();
    assert_eq!(theirs.get("file"), Some(&1), "vite: {theirs:?}");

    app.reset();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(&app.root)
        .current_dir(&app.root));
    assert_eq!(app.take(), theirs, "oj build vs vite build");
}

/// The Start fixture with the spy ahead of its own plugins.
fn start_app(name: &str) -> Option<(start_app::App, PathBuf)> {
    let app = start_app::app(name)?;
    let log = app.0.join("spy.log");
    let file = app.0.join("vite.config.ts");
    let config = std::fs::read_to_string(&file).unwrap();
    let spy = format!(
        r#"import fs from "node:fs";
const spyLog = (line: string) => fs.appendFileSync({log:?}, line + "\n");
spyLog("file");
const spy = () => {{
  spyLog("factory");
  return {{
    name: "spy",
    config() {{ spyLog("config"); }},
    configEnvironment(name: string) {{ spyLog(`configEnvironment ${{name}}`); }},
    configResolved() {{ spyLog("configResolved"); }},
  }};
}};
"#
    );
    assert!(
        config.contains("  plugins: [\n"),
        "the fixture config's plugin list"
    );
    let config = format!(
        "{spy}{}",
        config.replacen("  plugins: [\n", "  plugins: [\n    spy(),\n", 1)
    );
    std::fs::write(&file, config).unwrap();
    Some((app, log))
}

fn counts(log: &Path) -> BTreeMap<String, usize> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let _ = std::fs::remove_file(log);
    let mut out = BTreeMap::new();
    for line in text.lines() {
        *out.entry(line.to_string()).or_default() += 1;
    }
    out
}

#[test]
fn a_start_dev_start_resolves_the_config_once_as_in_vite() {
    let Some((app, log)) = start_app("config-once-dev") else {
        return;
    };
    let vite = ViteDev::start(&app.0);
    vite.get("/");
    drop(vite);
    let theirs = counts(&log);
    assert_eq!(theirs.get("file"), Some(&1), "vite: {theirs:?}");

    let _ = std::fs::remove_dir_all(app.0.join(".oj-cache"));
    let _ = std::fs::remove_dir_all(app.0.join("node_modules/.vite"));
    let dev = Server::oj_dev_with_args(&app.0, &[], &[]);
    dev.request("GET", "/", &[]);
    settle(&log, lines(&theirs));
    let ours = counts(&log);
    drop(dev);
    assert_eq!(ours, theirs, "oj dev vs vite dev (Start)");
}

// Vite's loadEnv takes `NODE_ENV` from `.env` (VITE_USER_NODE_ENV) and its
// resolveConfig sets it for the process: the plugins see that value, on the
// host oj starts for a cold build as on Vite.
#[test]
fn a_builds_plugins_see_vites_node_env() {
    let Some(app) = Spa::new("") else {
        return;
    };
    std::fs::write(app.root.join(".env"), "NODE_ENV=development\n").unwrap();
    std::fs::write(
        app.root.join("vite.config.js"),
        format!(
            r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({:?}, line + "\n");
export default {{ plugins: [{{
  name: "node-env",
  configResolved(c) {{ log(`configResolved ${{process.env.NODE_ENV}} ${{c.isProduction}}`); }},
  buildStart() {{ log(`buildStart ${{process.env.NODE_ENV}}`); }},
}}] }};
"#,
            app.log.display().to_string()
        ),
    )
    .unwrap();
    let run = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(Command::new("node")
        .arg(app.root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&app.root)
        .env_remove("NODE_ENV"));
    let theirs = app.take();
    assert_eq!(
        theirs.get("configResolved development false"),
        Some(&1),
        "vite: {theirs:?}"
    );

    app.reset();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(&app.root)
        .current_dir(&app.root)
        .env_remove("NODE_ENV"));
    assert_eq!(app.take(), theirs, "oj build vs vite build");
}

// A config that names its own `mode`: Vite resolves once under it
// (`mode = inline.mode || config.mode`); oj loads its own values under that
// mode again, from the same evaluation.
#[test]
fn a_build_under_the_configs_own_mode_resolves_once_as_in_vite() {
    let Some(app) = Spa::new(", mode: \"staging\"") else {
        return;
    };
    let run = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(Command::new("node")
        .arg(app.root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&app.root));
    let theirs = app.take();
    assert_eq!(theirs.get("file"), Some(&1), "vite: {theirs:?}");

    app.reset();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(&app.root)
        .current_dir(&app.root));
    assert_eq!(app.take(), theirs, "oj build vs vite build");
}

// Vite runs the configResolved hooks on the resolved config the build then
// reads (resolveConfig): an alias a hook pushes there resolves the build's
// import (vite build bundles the aliased module). oj read the config's
// values before those hooks on a cold build, so the import failed.
#[test]
fn a_config_resolved_alias_reaches_the_build_as_in_vite() {
    let Some(app) = Spa::new("") else {
        return;
    };
    std::fs::write(
        app.root.join("src/main.ts"),
        "import x from \"aliased\";\nconsole.log(x);\n",
    )
    .unwrap();
    std::fs::write(
        app.root.join("src/target.ts"),
        "export default \"from-alias\";\n",
    )
    .unwrap();
    std::fs::write(
        app.root.join("vite.config.js"),
        format!(
            r#"export default {{ plugins: [{{
  name: "alias-in-config-resolved",
  configResolved(c) {{ c.resolve.alias.push({{ find: "aliased", replacement: {:?} }}); }},
}}] }};
"#,
            app.root.join("src/target.ts").display().to_string()
        ),
    )
    .unwrap();
    let built = |cmd: &mut Command, dist: &str| -> String {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let mut js = String::new();
        for entry in std::fs::read_dir(app.root.join(dist).join("assets"))
            .unwrap()
            .flatten()
        {
            if entry.path().extension().is_some_and(|e| e == "js") {
                js.push_str(&std::fs::read_to_string(entry.path()).unwrap());
            }
        }
        js
    };
    let vite = built(
        Command::new("node")
            .arg(app.root.join("node_modules/vite/bin/vite.js"))
            .args(["build", "--outDir", "vite-dist"])
            .current_dir(&app.root),
        "vite-dist",
    );
    assert!(vite.contains("from-alias"), "vite:\n{vite}");
    app.reset();
    let oj = built(
        Command::new(env!("CARGO_BIN_EXE_oj"))
            .args(["build", "--outDir", "oj-dist"])
            .arg(&app.root)
            .current_dir(&app.root),
        "oj-dist",
    );
    assert!(oj.contains("from-alias"), "oj:\n{oj}");
}

/// Builds the scratch SPA with Vite then oj (fresh caches), each into its
/// default outDir: per tool, the dirs it wrote and its JS.
fn build_both_tools(app: &Spa) -> [(Vec<String>, String); 2] {
    let run = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let written = |root: &Path| -> (Vec<String>, String) {
        let mut dirs = Vec::new();
        let mut js = String::new();
        for name in ["dist", "edited-out"] {
            let assets = root.join(name).join("assets");
            if !assets.is_dir() {
                continue;
            }
            dirs.push(name.to_string());
            for entry in std::fs::read_dir(&assets).unwrap().flatten() {
                if entry.path().extension().is_some_and(|e| e == "js") {
                    js.push_str(&std::fs::read_to_string(entry.path()).unwrap());
                }
            }
            let _ = std::fs::remove_dir_all(root.join(name));
        }
        (dirs, js)
    };
    run(Command::new("node")
        .arg(app.root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&app.root));
    let vite = written(&app.root);
    app.reset();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(&app.root)
        .current_dir(&app.root));
    let oj = written(&app.root);
    [vite, oj]
}

// What Vite does with configResolved edits in a build: the env variable and
// the define set in place reach the code (its define plugin reads them as
// it starts, the client environment holding those objects); `build.outDir`
// does not (the builder copied the build options before the hooks).
#[test]
fn config_resolved_build_edits_apply_where_vite_reads_them() {
    let Some(app) = Spa::new(", define: { __INPLACE__: JSON.stringify(\"before\") }") else {
        return;
    };
    std::fs::write(
        app.root.join("src/main.ts"),
        "console.log(import.meta.env.VITE_EDIT, __INPLACE__);\n",
    )
    .unwrap();
    std::fs::write(
        app.root.join("vite.config.js"),
        r#"export default { define: { __INPLACE__: JSON.stringify("before") }, plugins: [{ name: "edits", configResolved(c) {
  c.env.VITE_EDIT = "env-edited";
  c.define.__INPLACE__ = JSON.stringify("inplace-edited");
  c.build.outDir = "edited-out";
} }] };
"#,
    )
    .unwrap();
    let [(vite_dirs, vite), (oj_dirs, oj)] = build_both_tools(&app);
    assert_eq!(vite_dirs, ["dist"], "vite");
    assert!(
        vite.contains("env-edited") && vite.contains("inplace-edited"),
        "vite:\n{vite}"
    );
    assert_eq!(oj_dirs, vite_dirs, "oj writes where vite does");
    assert!(oj.contains("env-edited"), "oj, the env edit:\n{oj}");
    assert!(oj.contains("inplace-edited"), "oj, the define edit:\n{oj}");
}

// A define replaced in configResolved reaches no environment in Vite: the
// code keeps the config's own value.
#[test]
fn a_replaced_define_changes_nothing_as_in_vite() {
    let Some(app) = Spa::new("") else {
        return;
    };
    std::fs::write(app.root.join("src/main.ts"), "console.log(__D__);\n").unwrap();
    std::fs::write(
        app.root.join("vite.config.js"),
        r#"export default { define: { __D__: JSON.stringify("from-config") }, plugins: [{ name: "replace", configResolved(c) {
  c.define = { __D__: JSON.stringify("replaced") };
} }] };
"#,
    )
    .unwrap();
    let [(_, vite), (_, oj)] = build_both_tools(&app);
    assert!(
        vite.contains("from-config") && !vite.contains("replaced"),
        "vite:\n{vite}"
    );
    assert!(
        oj.contains("from-config") && !oj.contains("replaced"),
        "oj:\n{oj}"
    );
}

// What the config's values warn about prints the same on a cold build (the
// host takes the run over) and a warm one (the stored values).
#[test]
fn a_builds_config_warnings_are_the_same_cold_and_warm() {
    let Some(app) = Spa::new(", build: { target: false }") else {
        return;
    };
    let oj_build = || -> Vec<String> {
        let out = Command::new(env!("CARGO_BIN_EXE_oj"))
            .arg("build")
            .arg(&app.root)
            .current_dir(&app.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        text.lines()
            .filter(|l| l.contains("vite.config:"))
            .map(str::to_string)
            .collect()
    };
    app.reset();
    let cold = oj_build();
    let warm = oj_build();
    assert!(!cold.is_empty(), "the premise: the config warns");
    assert_eq!(cold, warm, "cold vs warm build");
}
