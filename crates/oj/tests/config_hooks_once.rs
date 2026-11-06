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
