// Manual RSS probe (#[ignore]d: wall-clock and RSS assertions are machine
// dependent): does the idle shrink actually return memory after a
// burst-then-idle, versus an identical engine without it? Run each in its own
// process and compare the printed numbers:
//   cargo test -p oj_js --test shrink_probe probe_with_shrink -- --ignored --nocapture
//   cargo test -p oj_js --test shrink_probe probe_without_shrink -- --ignored --nocapture
// Measured (M-series, 2026-09-28): with shrink peak=618MB settled=56MB
// (562MB returned); without: settled=618MB forever.
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
async fn probe_with_shrink() {
    run(Some(Duration::from_millis(800))).await;
}

#[tokio::test]
#[ignore]
async fn probe_without_shrink() {
    run(None).await;
}

async fn run(shrink: Option<Duration>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"p","version":"1.0.0"}"#,
    )
    .unwrap();
    let mut config = oj_js::EngineConfig::new(dir.path());
    config.idle_shrink_after = shrink;
    let engine = oj_js::JsEngine::spawn(config).unwrap();
    let base = rss_mb();
    engine
        .eval(oj_js::EvalInput::Source(
            "globalThis.big = []; for (let i = 0; i < 70; i++) globalThis.big.push(new Array(1024 * 1024).fill(1.5)); \
             globalThis.big = null; export default 1;"
                .into(),
        ))
        .await
        .unwrap();
    let peak = rss_mb();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let settled = rss_mb();
    eprintln!(
        "[probe shrink={:?}] base={}MB peak={}MB settled={}MB returned={}MB shrinks={}",
        shrink,
        base,
        peak,
        settled,
        peak.saturating_sub(settled),
        engine.idle_shrinks()
    );
}
