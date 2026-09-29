// An idle engine polls its drained event loop on every wake, so V8's own
// delayed tasks (the MemoryReducer) run between jobs. That must not turn into
// a busy loop: a drained poll only registers the waker, and the engine wakes
// again only for a job, a due V8 task, or a timer. Its own binary, so no other
// test's CPU lands in the measurement.
use std::time::Duration;

/// This process's CPU time (user + system), from `ps` (`[[dd-]hh:]mm:ss.cc`).
fn cpu_seconds() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().unwrap_or(0.0), r.to_string()),
        None => (0.0, text),
    };
    let secs = rest.split(':').fold(0.0, |acc, part| {
        acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)
    });
    days * 86_400.0 + secs
}

#[tokio::test]
async fn an_idle_engine_does_not_spin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"p","version":"1.0.0"}"#,
    )
    .unwrap();
    let mut config = oj_js::EngineConfig::new(dir.path());
    // The backstop's own tick is not what this measures.
    config.idle_shrink_after = None;
    let engine = oj_js::JsEngine::spawn(config).unwrap();
    // Allocate enough to arm V8's reducer, so its delayed tasks do wake the
    // engine during the idle window below.
    engine
        .eval(oj_js::EvalInput::Source(
            "globalThis.x = []; for (let i = 0; i < 20; i++) globalThis.x.push(new Array(1024 * 1024).fill(1.5)); \
             globalThis.x = null; export default 1;"
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = cpu_seconds();
    // Measured: 0.000s for the engine, 0.89s with a drained poll that
    // re-wakes itself.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let used = cpu_seconds() - before;
    assert!(
        used < 0.25,
        "an idle engine used {used:.2}s of CPU over 3s: the drained-loop poll spins"
    );
    drop(engine);
}
