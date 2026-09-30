// Manual RSS probe (#[ignore]d: wall-clock and RSS assertions are machine
// dependent): an idle engine returns memory the way Node does, through V8's
// own MemoryReducer. The heap shape is the one the reducer is built for: data
// RETAINED through the burst's collections (a mark-compact that grows
// committed memory arms the reducer), then dropped by a tiny job that triggers
// no collection. Pass `--trace-gc` to V8 to see the `(reduce)` collection:
//   cargo test -p oj_js --test shrink_probe probe_reducer -- --ignored --nocapture
// Measured (M-series, 2026-09-29), 983MB retained then dropped:
// - Node 24, the same JS: `Mark-Compact (reduce)` at ~106s, settles at 91MB.
// - this engine: `Incremental Mark-Compact (reduce)` at ~104s, settles at
//   91MB. Before the engine polled its drained event loop, the reducer's
//   delayed tasks never ran and it stayed at 983MB.
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
async fn probe_reducer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"p","version":"1.0.0"}"#,
    )
    .unwrap();
    let engine = oj_js::JsEngine::spawn(oj_js::EngineConfig::new(dir.path()), None, None).unwrap();
    engine
        .eval(oj_js::EvalInput::Source(
            "globalThis.keep = []; for (let i = 0; i < 22; i++) globalThis.keep.push(Array.from({ length: 1 << 20 }, (_, j) => ({ j }))); export default 1;"
                .into(),
        ), None)
        .await
        .unwrap();
    let retained = rss_mb();
    engine
        .eval(
            oj_js::EvalInput::Source("globalThis.keep = null; export default 2;".into()),
            None,
        )
        .await
        .unwrap();
    // Past the reducer's 100s watchdog, the path Node takes here too.
    tokio::time::sleep(Duration::from_secs(130)).await;
    let settled = rss_mb();
    eprintln!(
        "[probe reducer] retained={retained}MB settled={settled}MB returned={}MB",
        retained.saturating_sub(settled)
    );
}
