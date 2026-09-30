// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Manual probe: what tokio costs on the engine's hot paths.
//!
//!   cargo test -p oj_js --release --test latency_probe -- --ignored --nocapture
//!
//! Three layers: the raw cross-thread hop (tokio channels vs std channels in
//! the engine's exact shape), the real call round trip through V8, and the
//! per-parked-call cost of the timeout_at deadline wrapper.

use std::time::Duration;
use std::time::Instant;

fn engine(root: &std::path::Path) -> oj_js::JsEngine {
    oj_js::JsEngine::spawn(oj_js::EngineConfig::new(root), None, None).unwrap()
}

/// The engine hop, isolated: an async caller sends over an unbounded tokio
/// mpsc, a dedicated thread running a current-thread runtime replies over a
/// tokio oneshot — exactly the JsEngine request shape, minus V8. The std
/// variant is the same hop on std::sync::mpsc with a blocking caller.
#[test]
#[ignore]
fn probe_channel_hop() {
    const N: u32 = 200_000;

    // tokio shape
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<(u32, tokio::sync::oneshot::Sender<u32>)>();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                while let Some((v, reply)) = rx.recv().await {
                    let _ = reply.send(v);
                }
            });
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let t0 = Instant::now();
    rt.block_on(async {
        for i in 0..N {
            let (rtx, rrx) = tokio::sync::oneshot::channel();
            tx.send((i, rtx)).unwrap();
            assert_eq!(rrx.await.unwrap(), i);
        }
    });
    let tokio_ns = t0.elapsed().as_nanos() / u128::from(N);
    drop(tx);
    server.join().unwrap();

    // std shape
    let (tx, rx) = std::sync::mpsc::channel::<(u32, std::sync::mpsc::Sender<u32>)>();
    let server = std::thread::spawn(move || {
        while let Ok((v, reply)) = rx.recv() {
            let _ = reply.send(v);
        }
    });
    let t0 = Instant::now();
    for i in 0..N {
        let (rtx, rrx) = std::sync::mpsc::channel();
        tx.send((i, rtx)).unwrap();
        assert_eq!(rrx.recv().unwrap(), i);
    }
    let std_ns = t0.elapsed().as_nanos() / u128::from(N);
    drop(tx);
    server.join().unwrap();

    println!("cross-thread request/reply hop: tokio {tokio_ns}ns, std {std_ns}ns");
}

/// The real per-call cost: a trivial exported function, called sequentially
/// and concurrently. The channel hop from the probe above is the tokio share
/// of this number.
#[test]
#[ignore]
fn probe_call_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mod.mjs"),
        "export function ping(n) { return n; }\n\
         export function park(ms) { return new Promise((r) => setTimeout(() => r(ms), ms)); }\n",
    )
    .unwrap();
    let engine = engine(dir.path());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        for _ in 0..200 {
            engine
                .call("mod.mjs", "ping", vec![1.into()], None)
                .await
                .unwrap();
        }
        const N: u32 = 5_000;
        let t0 = Instant::now();
        for i in 0..N {
            engine
                .call("mod.mjs", "ping", vec![i.into()], None)
                .await
                .unwrap();
        }
        let per = t0.elapsed().as_micros() / u128::from(N);
        println!("sequential trivial call: {per}us/call");

        // Concurrency: 256 calls parked on a 10ms timer, with and without a
        // deadline (the timeout_at wrapper + watchdog arm per call).
        for deadline in [None, Some(Duration::from_secs(5))] {
            let t0 = Instant::now();
            let calls: Vec<_> = (0..256)
                .map(|_| engine.call("mod.mjs", "park", vec![10.into()], deadline))
                .collect();
            for call in calls {
                call.await.unwrap();
            }
            let wall = t0.elapsed().as_millis();
            println!("256 concurrent 10ms-parked calls (deadline {deadline:?}): {wall}ms wall");
        }
    });
}
