// Manual RSS probes (#[ignore]d: wall-clock and RSS assertions are machine
// dependent) for how an idle engine returns memory. The heap shape is the one
// V8's MemoryReducer is built for: data RETAINED through the burst's
// collections (a mark-compact that grows committed memory arms the reducer),
// then dropped by a tiny job that triggers no collection. Run each in its own
// process, with `--trace-gc` passed to V8 to see the collections:
//   cargo test -p oj_js --test shrink_probe probe_reducer_arms -- --ignored --nocapture
//   cargo test -p oj_js --test shrink_probe probe_backstop_default -- --ignored --nocapture
// Measured (M-series, 2026-09-29), 983MB retained then dropped:
// - Node 24, the same JS: `Mark-Compact (reduce)` at ~106s, settles at 91MB.
// - reducer only (backstop off): `Incremental Mark-Compact (reduce)` at ~104s,
//   settles at 91MB. Before the engine kept polling its drained event loop,
//   no collection ever ran and it stayed at 983MB.
// - default backstop (moderate pressure after 10s): the same collection at
//   ~10.6s (17.7ms pause), settles at 91MB.
use std::time::Duration;

fn rss_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        / 1024
}

#[tokio::test]
#[ignore]
async fn probe_reducer_arms() {
    // Past the reducer's 100s watchdog, the path Node takes here too.
    retained_then_dropped(None, Duration::from_secs(130)).await;
}

#[tokio::test]
#[ignore]
async fn probe_backstop_default() {
    retained_then_dropped(
        Some(oj_js::DEFAULT_IDLE_SHRINK_AFTER),
        Duration::from_secs(25),
    )
    .await;
}

async fn retained_then_dropped(shrink: Option<Duration>, idle: Duration) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"p","version":"1.0.0"}"#,
    )
    .unwrap();
    let mut config = oj_js::EngineConfig::new(dir.path());
    config.idle_shrink_after = shrink;
    let engine = oj_js::JsEngine::spawn(config).unwrap();
    engine
        .eval(oj_js::EvalInput::Source(
            "globalThis.keep = []; for (let i = 0; i < 22; i++) globalThis.keep.push(Array.from({ length: 1 << 20 }, (_, j) => ({ j }))); export default 1;"
                .into(),
        ))
        .await
        .unwrap();
    let retained = rss_mb();
    engine
        .eval(oj_js::EvalInput::Source(
            "globalThis.keep = null; export default 2;".into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(idle).await;
    let settled = rss_mb();
    eprintln!(
        "[probe shrink={shrink:?} idle={idle:?}] retained={retained}MB settled={settled}MB returned={}MB shrinks={}",
        retained.saturating_sub(settled),
        engine.idle_shrinks()
    );
}
