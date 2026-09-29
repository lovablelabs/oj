// An idle engine polls its drained event loop on every wake, so V8's own
// delayed tasks (the MemoryReducer) run between jobs. That must not turn into
// a busy loop: a drained poll only registers the waker, and the engine wakes
// again only for a job, a due V8 task, or a timer. Its own binary, so no other
// test's CPU lands in the measurement.
use std::time::Duration;

/// This process's CPU time (user + system) in seconds. Linux `ps` prints
/// whole seconds (`[[dd-]hh:]mm:ss`), too coarse to catch a spin in a 3s
/// window, so there it reads `/proc/self/stat`'s utime and stime (fields 14
/// and 15, in clock ticks: 100 per second on Linux).
#[cfg(target_os = "linux")]
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // The command name (field 2) is parenthesized and may contain spaces.
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // `after` starts at field 3, so utime (14) and stime (15) are 11 and 12.
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    ticks as f64 / 100.0
}

/// This process's CPU time (user + system), from `ps`, which on macOS prints
/// hundredths (`[[dd-]hh:]mm:ss.cc`).
#[cfg(not(target_os = "linux"))]
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
    let engine = oj_js::JsEngine::spawn(oj_js::EngineConfig::new(dir.path())).unwrap();
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
