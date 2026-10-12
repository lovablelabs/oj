//! The per-file pipeline is oj's only contact with source it did not write: it
//! must never panic, and anything it accepts must come back out as parseable
//! JavaScript with a module graph that matches what it emitted.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;
use oj_compiler::{compile, exports, CompileOptions};

/// Bracket nesting, over-approximated over raw bytes (brackets inside strings
/// count). Used only to skip inputs, never to assert: nesting depth is bounded
/// by the stack oj gives a compile thread, which `COMPILE_STACK_SIZE` and the
/// `adverse` suite cover directly. Without this the fuzzer spends all its time
/// rediscovering that a parser recurses.
fn bracket_depth(data: &[u8]) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    for byte in data {
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Unclosed `<` depth, same skip-only role as [`bracket_depth`]: oxc resolves
/// each TSX `<` ambiguously (type arguments vs JSX vs less-than) by trying,
/// and adversarial runs of unclosed `<` backtrack exponentially -- about 20 of
/// them allocated 3.5GB in CI. Real code closes its angles (JSX tags, generic
/// argument lists), so a path this deep is soup, not coverage.
fn angle_depth(data: &[u8]) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    for byte in data {
        match byte {
            b'<' => {
                depth += 1;
                max = max.max(depth);
            }
            b'>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

const EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if bracket_depth(data) > 200 || angle_depth(data) > 12 {
        return;
    }
    // The first byte picks the extension, so one corpus entry can explore every
    // source type without the fuzzer having to guess filenames.
    let extension = EXTENSIONS[data.first().copied().unwrap_or(0) as usize % EXTENSIONS.len()];
    let path = std::path::PathBuf::from(format!("/src/fuzz.{extension}"));

    // Names are names, whatever the input.
    for name in exports(source, &path) {
        assert!(!name.is_empty(), "empty export name");
    }

    for options in [CompileOptions::dev(), CompileOptions::prod()] {
        let Ok(out) = compile(&path, source, &options) else {
            continue;
        };
        // TypeScript/JSX output parses as what the browser will consume
        // (`validate_emitted` guarantees it; this reparse is the belt). Plain
        // JavaScript round-trips under its own leniency instead: an importless
        // `.js` file resolves to sloppy script, and sloppy-only code in it
        // (`--eval`) passes through byte-identical, exactly as under Vite,
        // whose esbuild transform does not strict-check plain js either -- the
        // browser reports it.
        let verify_ext = match extension {
            "js" | "cjs" => extension,
            _ => "mjs",
        };
        let verify = std::path::PathBuf::from(format!("/src/verify.{verify_ext}"));
        let reparsed = compile(&verify, &out.code, &CompileOptions::prod());
        assert!(
            reparsed.is_ok(),
            "emitted invalid code for {extension}: {:?}\n---\n{}",
            source,
            out.code
        );
        // Every import reported is a real specifier, and the map, if any, is a
        // data URL.
        for import in out.imports.iter().chain(out.dynamic_imports.iter()) {
            assert!(
                !import.contains('\n'),
                "specifier with a newline: {import:?}"
            );
        }
        if let Some(url) = &out.map_data_url() {
            assert!(url.starts_with("data:application/json;"), "{url}");
        }
        // Compilation is a pure function of its inputs.
        let again = compile(&path, source, &options).expect("second compile");
        assert_eq!(out.code, again.code, "nondeterministic output");
    }
});
