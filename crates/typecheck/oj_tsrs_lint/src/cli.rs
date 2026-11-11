//! The headless command line shared by `tsrs-lint` and `tsrs --lint`: oxlint runs `<binary> headless [flags]` with the
//! payload on stdin and reads protocol frames from stdout.

use std::io::{self, Read, Write};

use crate::{engine, payload, protocol};

/// Runs [`run`] on stdin and stdout on a thread with a large stack (checking recurses deeply), as both binaries do.
/// `args` excludes the program name (and `--lint`). Returns the exit status.
pub fn main(args: Vec<String>) -> i32 {
    let result = std::thread::Builder::new()
        .name("tsrs-lint".into())
        .stack_size(512 * 1024 * 1024)
        .spawn(move || {
            run(args, &mut io::stdin().lock(), &mut io::BufWriter::new(io::stdout().lock()))
        });
    match result {
        Ok(thread) => {
            if let Ok(code) = thread.join() {
                code
            } else {
                eprintln!("tsrs-lint panicked");
                1
            }
        }
        Err(error) => {
            eprintln!("could not start tsrs-lint: {error}");
            1
        }
    }
}

/// Parses `headless [--fix] [--fix-suggestions] [--runExternalCode] [--debug timings]`.
///
/// Reads the payload from `input`, lints it from the current directory and writes the frames to `output`. Returns
/// the exit status.
pub fn run(
    args: impl IntoIterator<Item = String>,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> i32 {
    let mut options = engine::Options {
        suppress_program_diagnostics: std::env::var(
            "OXLINT_TSGOLINT_DANGEROUSLY_SUPPRESS_PROGRAM_DIAGNOSTICS",
        )
        .is_ok_and(|value| value == "true"),
        // oxlint runs `<path> headless` with flags of its own, so `--runExternalCode` has an environment form.
        run_external_code: std::env::var("TSRS_LINT_RUN_EXTERNAL_CODE")
            .is_ok_and(|value| value == "1"),
        ..Default::default()
    };
    let mut headless = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "headless" | "--headless" => headless = true,
            "-fix" | "--fix" => options.fix = true,
            "-fix-suggestions" | "--fix-suggestions" => options.fix_suggestions = true,
            "-runExternalCode" | "--runExternalCode" => options.run_external_code = true,
            "-debug" | "--debug" => {
                if args.next().as_deref() == Some("timings") {
                    options.debug_timings = true;
                } else {
                    eprintln!("--debug requires timings");
                    return 1;
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: tsrs-lint headless [--fix] [--fix-suggestions] [--runExternalCode] [--debug timings]"
                );
                return 0;
            }
            _ => {
                eprintln!("unknown option: {arg}");
                return 1;
            }
        }
    }
    if !headless {
        eprintln!("tsrs-lint requires headless");
        return 1;
    }
    let result = (|| {
        let mut text = String::new();
        input.read_to_string(&mut text).map_err(|e| format!("error reading from stdin: {e}"))?;
        let payload = payload::deserialize_payload(&text)
            .map_err(|e| format!("error parsing config: {e}"))?;
        let cwd =
            std::env::current_dir().map_err(|e| format!("error getting current directory: {e}"))?;
        engine::run(&payload, options, &cwd.to_string_lossy(), output)
            .map_err(|e| format!("error running linter: {e}"))
    })();
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            if let Err(write_error) = protocol::write_error(output, &error) {
                eprintln!("could not write error: {write_error}");
            }
            1
        }
    };
    if let Err(error) = output.flush() {
        eprintln!("could not flush output: {error}");
        return 1;
    }
    code
}
