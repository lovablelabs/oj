use super::*;

fn app(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("vite.config.mjs"), config).unwrap();
    dir
}

// The deps pre-seed stamp survives a restart whose fresh evaluation matches
// the cached one (a plugin's server.restart(), an .env edit), and is dropped
// when the config's values moved.
#[test]
fn a_restart_drops_the_preseed_stamp_only_when_the_config_moved() {
    let dir = app(r#"export default { define: { __A__: "1" } };"#);
    let root = dir.path();
    crate::plugins::extract_vite_values(root, None, "serve", "development")
        .expect("the config extracts");
    crate::preseed::write_stamp(root, &[]);
    assert!(crate::preseed::stamp_is_warm(root));
    let restarter = Restarter::new(root.to_path_buf(), None, "development".into(), false);
    assert_eq!(restarter.failure(), None);
    assert!(
        crate::preseed::stamp_is_warm(root),
        "an unchanged config dropped the stamp"
    );
    std::fs::write(
        root.join("vite.config.mjs"),
        r#"export default { define: { __A__: "2" } };"#,
    )
    .unwrap();
    assert_eq!(restarter.failure(), None);
    assert!(
        !crate::preseed::stamp_is_warm(root),
        "a changed config kept the stamp"
    );
}

// An evaluation a signal interrupted runs again after each delay of the
// schedule, and the error of the last attempt is the one reported; one that
// failed on its own (the config threw) is reported at once, Vite's shape
// (restartServer logs the load error and keeps the running server).
#[test]
fn an_interrupted_evaluation_runs_again_per_the_schedule() {
    use crate::plugins::ExtractionError;
    let interrupted = || {
        ExtractionError::Interrupted("the engine job child was killed by SIGKILL (signal 9)".into())
    };

    // Interrupted twice, then through: two waits, the schedule's first two.
    let mut runs = 0;
    let mut slept = Vec::new();
    let out = evaluate_with_retries(
        || {
            runs += 1;
            if runs <= 2 {
                Err(interrupted())
            } else {
                Ok(runs)
            }
        },
        |d| slept.push(d),
        || false,
    );
    assert_eq!(out, Ok(3));
    assert_eq!(slept, INTERRUPTED_RETRY_DELAYS[..2].to_vec());

    // Interrupted every time: every delay waited, then the interruption reported.
    let mut runs = 0;
    let mut slept = Vec::new();
    let out: Result<(), _> = evaluate_with_retries(
        || {
            runs += 1;
            Err(interrupted())
        },
        |d| slept.push(d),
        || false,
    );
    assert_eq!(out, Err(interrupted()));
    assert_eq!(runs, INTERRUPTED_RETRY_DELAYS.len() + 1);
    assert_eq!(slept, INTERRUPTED_RETRY_DELAYS.to_vec());

    // A config that failed is not run again: its cause is printed and nothing
    // about a second run would differ.
    let mut runs = 0;
    let out: Result<(), _> = evaluate_with_retries(
        || {
            runs += 1;
            Err(ExtractionError::Failed)
        },
        |_| panic!("a failed config waits for nothing"),
        || false,
    );
    assert_eq!(out, Err(ExtractionError::Failed));
    assert_eq!(runs, 1);

    // The server stopping (its shutdown sweep killed the child): no second run.
    let mut runs = 0;
    let out: Result<(), _> = evaluate_with_retries(
        || {
            runs += 1;
            Err(interrupted())
        },
        |_| panic!("a stopping server waits for nothing"),
        || true,
    );
    assert_eq!(out, Err(interrupted()));
    assert_eq!(runs, 1);

    // The server began stopping during the wait: no second run either, its
    // sweep would kill that evaluation's child or miss it.
    let mut runs = 0;
    let stopping = std::cell::Cell::new(false);
    let out: Result<(), _> = evaluate_with_retries(
        || {
            runs += 1;
            Err(interrupted())
        },
        |_| stopping.set(true),
        || stopping.get(),
    );
    assert_eq!(out, Err(interrupted()));
    assert_eq!(runs, 1);
}

// A check waiting to evaluate an interrupted config again stops waiting when
// a restart is requested: the request is for a config edited meanwhile, which
// is what the next evaluation reads. Vite's restart has no wait to cut short
// (it never retries); this keeps oj's from delaying a fixed config.
#[test]
fn a_request_ends_the_wait_before_evaluating_again() {
    let long = Duration::from_secs(60);

    // No request: the wait lasts its delay.
    let asked = Asked::default();
    let started = std::time::Instant::now();
    asked.wait(Duration::from_millis(50));
    assert!(started.elapsed() >= Duration::from_millis(50));

    // A request during the wait ends it.
    let asked = std::sync::Arc::new(Asked::default());
    let waiter = {
        let asked = std::sync::Arc::clone(&asked);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            asked.wait(long);
            started.elapsed()
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    asked.note();
    assert!(
        waiter.join().unwrap() < long / 2,
        "the request did not end the wait"
    );

    // One made before the wait (during the interrupted evaluation) ends it at
    // once, and only once: the next wait lasts its delay.
    let started = std::time::Instant::now();
    asked.note();
    asked.wait(long);
    assert!(started.elapsed() < long / 2);
    let started = std::time::Instant::now();
    asked.wait(Duration::from_millis(50));
    assert!(started.elapsed() >= Duration::from_millis(50));

    // The request that started the check is taken up, so it ends no wait.
    asked.note();
    asked.take_up();
    let started = std::time::Instant::now();
    asked.wait(Duration::from_millis(50));
    assert!(started.elapsed() >= Duration::from_millis(50));
}
