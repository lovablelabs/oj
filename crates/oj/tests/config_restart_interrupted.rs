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

/// The config counts its evaluations in `{{RUNS}}` and, on those the
/// `{{KILL}}` expression over `runs` (0-based) holds for, kills the process
/// evaluating it with `{{SIGNAL}}` before exporting anything. `{{TAG}}` is a
/// comment an edit changes.
const CONFIG: &str = r#"import fs from "node:fs";
// {{TAG}}
const runs = fs.existsSync("{{RUNS}}") ? Number(fs.readFileSync("{{RUNS}}", "utf8")) : 0;
fs.writeFileSync("{{RUNS}}", String(runs + 1));
if ({{KILL}}) process.kill(process.pid, "{{SIGNAL}}");
export default { define: { __RUN__: JSON.stringify(runs + 1) } };
"#;

/// A restart replaces the server's process, which binds its port again: with
/// these tests' servers running side by side, one restarting could find its
/// port taken by another's and come back on a different one, where the test
/// no longer reaches it. One app, and so one server, at a time.
static ONE_SERVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Spa {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    runs: PathBuf,
    _one_server: std::sync::MutexGuard<'static, ()>,
}

impl Spa {
    fn new(kill: &str, signal: &str) -> Option<Spa> {
        let modules = fixture_modules()?;
        let one_server = ONE_SERVER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            _one_server: one_server,
        };
        spa.write_config("boot", kill, signal);
        Some(spa)
    }

    fn write_config(&self, tag: &str, kill: &str, signal: &str) {
        let config = CONFIG
            .replace("{{RUNS}}", &self.runs.display().to_string())
            .replace("{{TAG}}", tag)
            .replace("{{KILL}}", kill)
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
    let Some(app) = Spa::new("runs === 1", signal) else {
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
    app.write_config("edited", "runs === 1", signal);
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
    // Vite's playground/cli "should restart": a config edit restarts with no
    // error in the log.
    for failure in ["server restart failed", "failed to load config"] {
        assert!(
            !output[from..].contains(failure),
            "an interrupted evaluation was reported as a failed config\n{output}"
        );
    }
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
    let Some(app) = Spa::new("false", "SIGKILL") else {
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

    // One evaluation per failed check, as Vite loads the config once per
    // restart: _createServer resolves it, and loadConfigFromFile's error
    // leaves resolveConfig with nothing loading the file again. (Counted per
    // reported failure: the watcher may report one edit twice, which is two
    // checks.) oj resolved from the file after the load had thrown, running
    // the config's module-level code twice for the one failure.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let failures = dev.output()[from..]
            .matches("server restart failed")
            .count();
        let evaluations = app.runs() - 1;
        if evaluations == failures
            && dev.output()[from..]
                .matches("server restart failed")
                .count()
                == failures
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{evaluations} evaluations of the throwing config for {failures} failed check(s)\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Waits until `dev` serves `/src/main.ts` logging `run`, the evaluation whose
/// values the server booted from.
fn serves_run(dev: &Server, run: usize, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let (status, body) = dev.request("GET", "/src/main.ts", &[]);
        if status == 200 && body.contains(&format!("console.log({run})")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the server never served run {run}'s config: {status} {body}\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

// Vite's restartServer keeps the running server until the new one exists
// (`await server.close()` comes after `_createServer` resolved) and, when it
// throws, returns with the running server and its watcher untouched: the next
// config edit restarts. So through an evaluation interrupted every time the
// running server serves the config it booted from, the restart then fails
// naming the signal, the server still serves, and an edit that evaluates
// restarts it.
#[test]
fn an_evaluation_interrupted_every_time_keeps_the_running_server_until_an_edit_loads() {
    let Some(app) = Spa::new("runs >= 1", "SIGKILL") else {
        return;
    };
    let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
    serves_run(&dev, 1, Duration::from_secs(60));

    let from = dev.output().len();
    app.write_config("edited", "runs >= 1", "SIGKILL");
    assert!(
        logs_after(
            &dev,
            from,
            "evaluating again in 2s",
            Duration::from_secs(60)
        ),
        "the evaluation was not run again\n{}",
        dev.output()
    );
    // Between evaluations the running server answers, with its own config.
    let (status, body) = dev.request("GET", "/src/main.ts", &[]);
    assert_eq!(status, 200, "{body}\n{}", dev.output());
    assert!(body.contains("console.log(1)"), "{body}");

    assert!(
        logs_after(&dev, from, "server restart failed", Duration::from_secs(60)),
        "an evaluation interrupted every time never failed the restart\n{}",
        dev.output()
    );
    let output = dev.output();
    assert!(
        output[from..].contains("failed to load config from")
            && output[from..].contains("interrupted every time")
            && output[from..].contains("SIGKILL (signal 9)"),
        "the failure does not say what interrupted the evaluation\n{output}"
    );
    assert!(
        !output[from..].contains("restarting dev server"),
        "a config that never evaluated restarted the server\n{output}"
    );
    // Boot, then the check's evaluation and one more per wait of the
    // schedule (more only if the watcher reported the edit twice, which is
    // checked again).
    assert!(app.runs() >= 5, "{} runs\n{output}", app.runs());
    let (status, body) = dev.request("GET", "/src/main.ts", &[]);
    assert_eq!(status, 200, "{body}\n{output}");
    assert!(body.contains("console.log(1)"), "{body}");

    // The watcher is still the running server's: an edit that loads restarts.
    let from = dev.output().len();
    app.write_config("fixed", "false", "SIGKILL");
    assert!(
        logs_after(&dev, from, "restarting dev server", Duration::from_secs(60)),
        "the fixed config did not restart the server\n{}",
        dev.output()
    );
    serves_run(&dev, app.runs(), Duration::from_secs(60));
}

// A process that faults on its own (an addon's SIGSEGV, the engine's abort)
// is the config failing to load, which Vite reports once (its load would take
// the server down; oj's child takes only the check down): the restart fails
// at once, with no second evaluation, and the running server serves on.
#[test]
fn a_restart_check_whose_evaluation_crashed_fails_without_a_second_run() {
    for signal in ["SIGABRT", "SIGILL"] {
        // Every evaluation after the boot's crashes, so a check the watcher
        // asks for twice fails twice instead of restarting.
        let Some(app) = Spa::new("runs >= 1", signal) else {
            return;
        };
        let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
        serves_run(&dev, 1, Duration::from_secs(60));

        let from = dev.output().len();
        app.write_config("edited", "runs >= 1", signal);
        assert!(
            logs_after(&dev, from, "server restart failed", Duration::from_secs(60)),
            "a crashed evaluation ({signal}) did not fail the restart\n{}",
            dev.output()
        );
        // Settle: a second evaluation would land within the schedule's first wait.
        std::thread::sleep(Duration::from_millis(1500));
        let output = dev.output();
        for retried in [
            "evaluating again",
            "was interrupted",
            "restarting dev server",
        ] {
            assert!(
                !output[from..].contains(retried),
                "a crashed evaluation ({signal}) was treated as interrupted\n{output}"
            );
        }
        let (status, body) = dev.request("GET", "/src/main.ts", &[]);
        assert_eq!(status, 200, "{body}\n{output}");
        assert!(body.contains("console.log(1)"), "{body}");
    }
}

// An edit saved while the check waits to evaluate again ends the wait, and
// the evaluation that follows answers it: the config is not evaluated once
// more for the edit before the restart. (Vite's restart() during a restart
// joins the one running, `_restartPromise`: one config load per restart.)
#[test]
fn an_edit_during_the_wait_is_answered_by_the_next_evaluation() {
    // Killed on evaluations 1 to 3, so the third wait is the schedule's
    // longest (4s): the edit lands well inside it.
    let kill = "runs >= 1 && runs <= 3";
    let Some(app) = Spa::new(kill, "SIGKILL") else {
        return;
    };
    let dev = Server::oj_dev_with_args(&app.root, &[], &[]);
    serves_run(&dev, 1, Duration::from_secs(60));

    let from = dev.output().len();
    app.write_config("edited", kill, "SIGKILL");
    assert!(
        logs_after(
            &dev,
            from,
            "evaluating again in 4s",
            Duration::from_secs(60)
        ),
        "the evaluation was not interrupted three times\n{}",
        dev.output()
    );
    let waiting = Instant::now();
    app.write_config("edited again", kill, "SIGKILL");
    assert!(
        logs_after(&dev, from, "restarting dev server", Duration::from_secs(60)),
        "the edit during the wait never led to the restart\n{}",
        dev.output()
    );
    let waited = waiting.elapsed();
    let output = dev.output();
    assert!(
        !output[from..].contains("server restart failed"),
        "{output}"
    );
    // Boot, three killed evaluations, and one for both the retry and the edit.
    assert_eq!(app.runs(), 5, "the edit was evaluated on its own\n{output}");
    assert!(
        waited < Duration::from_secs(4),
        "the edit did not end the 4s wait ({waited:?})\n{output}"
    );
    serves_run(&dev, 5, Duration::from_secs(60));
}
