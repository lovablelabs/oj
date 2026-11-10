// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A restart's config evaluation runs in a child process, which the kernel's
//! memory killer or a supervisor's signal to the process group can end
//! before it finishes. That is not the config failing: Vite's restartServer
//! reports "server restart failed" only when the config itself does not load
//! (its evaluation runs in the server's own process, where nothing ends it
//! apart from the server). So an interrupted evaluation is run again, after
//! a short wait, before the restart is declared failed; the running server
//! serves meanwhile.
//!
//! The scratch app's config kills the process evaluating it, once, on the
//! evaluation a config edit triggers. Shares the start-app fixture's
//! install; skips without it, unless `OJ_REQUIRE_FIXTURES`.

#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use start_app::Server;

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

/// The config counts its evaluations in `{{RUNS}}` and, on the one `{{KILL}}`
/// names (0-based), kills the process evaluating it with `{{SIGNAL}}` before
/// exporting anything. `{{TAG}}` is a comment an edit changes.
const CONFIG: &str = r#"import fs from "node:fs";
// {{TAG}}
const runs = fs.existsSync("{{RUNS}}") ? Number(fs.readFileSync("{{RUNS}}", "utf8")) : 0;
fs.writeFileSync("{{RUNS}}", String(runs + 1));
if (runs === {{KILL}}) process.kill(process.pid, "{{SIGNAL}}");
export default { define: { __RUN__: JSON.stringify(runs + 1) } };
"#;

struct Spa {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    runs: PathBuf,
}

impl Spa {
    fn new(kill: usize, signal: &str) -> Option<Spa> {
        let modules = fixture_modules()?;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let runs = root.join("runs.txt");
        for (file, body) in [
            ("package.json", r#"{"name":"interrupted","type":"module"}"#.to_string()),
            (
                "index.html",
                "<!doctype html><html><body><script type=\"module\" src=\"/src/main.ts\"></script></body></html>"
                    .to_string(),
            ),
            ("src/main.ts", "console.log(__RUN__);\n".to_string()),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();
        let spa = Spa {
            _tmp: tmp,
            root,
            runs,
        };
        spa.write_config("boot", kill, signal);
        Some(spa)
    }

    fn write_config(&self, tag: &str, kill: usize, signal: &str) {
        let config = CONFIG
            .replace("{{RUNS}}", &self.runs.display().to_string())
            .replace("{{TAG}}", tag)
            .replace("{{KILL}}", &kill.to_string())
            .replace("{{SIGNAL}}", signal);
        std::fs::write(self.root.join("vite.config.js"), config).unwrap();
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.runs)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Waits until `dev`'s output has `needle` past `from`, or the deadline.
fn logs_after(dev: &Server, from: usize, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if dev.output()[from..].contains(needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn restart_survives_an_interrupted_evaluation(signal: &str) {
    // Evaluation 0 is the boot's; the config edit's check is evaluation 1,
    // the one killed. Its second attempt is evaluation 2.
    let Some(app) = Spa::new(1, signal) else {
        return;
    };
    let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
    let (status, body) = dev.request("GET", "/src/main.ts", &[]);
    assert_eq!(status, 200, "{body}\n{}", dev.output());
    assert!(
        body.contains("console.log(1)"),
        "the boot's evaluation: {body}"
    );
    assert_eq!(app.runs(), 1, "{}", dev.output());

    let from = dev.output().len();
    app.write_config("edited", 1, signal);
    assert!(
        logs_after(
            &dev,
            from,
            "the config evaluation was interrupted",
            Duration::from_secs(60)
        ),
        "the edit's evaluation was not killed by the config\n{}",
        dev.output()
    );
    assert!(
        logs_after(&dev, from, "restarting dev server", Duration::from_secs(60)),
        "an interrupted evaluation never led to the restart\n{}",
        dev.output()
    );
    let output = dev.output();
    assert!(
        !output[from..].contains("server restart failed"),
        "an interrupted evaluation was reported as a failed config\n{output}"
    );
    // Boot, the killed check, the check again. The restarted boot reads the
    // check's stored evaluation (asserted below: it serves run 3's values).
    assert!(
        app.runs() >= 3,
        "the killed evaluation was not run again ({} runs)\n{output}",
        app.runs()
    );

    // The restarted server serves the edited config's values.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = dev.request("GET", "/src/main.ts", &[]);
        if status == 200 && body.contains("console.log(3)") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted server never served the new config: {status} {body}\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_restart_check_whose_evaluation_was_killed_runs_it_again() {
    restart_survives_an_interrupted_evaluation("SIGKILL");
}

#[test]
fn a_restart_check_whose_evaluation_was_stopped_runs_it_again() {
    restart_survives_an_interrupted_evaluation("SIGTERM");
}

// A config that fails on its own is still a failed restart, reported once,
// with no second evaluation: Vite's "server restart failed".
#[test]
fn a_restart_check_whose_config_throws_fails_without_a_second_run() {
    let Some(app) = Spa::new(usize::MAX, "SIGKILL") else {
        return;
    };
    let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
    dev.request("GET", "/src/main.ts", &[]);
    assert_eq!(app.runs(), 1, "{}", dev.output());

    let from = dev.output().len();
    std::fs::write(
        app.root.join("vite.config.js"),
        format!(
            "import fs from \"node:fs\";\nfs.writeFileSync({:?}, String(Number(fs.readFileSync({:?}, \"utf8\")) + 1));\nthrow new Error(\"config exploded\");\n",
            app.runs.display().to_string(),
            app.runs.display().to_string()
        ),
    )
    .unwrap();
    assert!(
        logs_after(&dev, from, "server restart failed", Duration::from_secs(60)),
        "a throwing config did not fail the restart\n{}",
        dev.output()
    );
    // Settle: a second evaluation would land within the schedule's first wait.
    std::thread::sleep(Duration::from_millis(1500));
    let output = dev.output();
    assert!(
        !output[from..].contains("restarting dev server"),
        "a throwing config restarted the server\n{output}"
    );
    assert!(
        !output[from..].contains("evaluating again"),
        "a throwing config was evaluated again\n{output}"
    );
}
