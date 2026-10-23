//! The splice map must place generated symbols on their true source lines,
//! and must not perturb the emitted bytes.

use std::path::Path;

use oj_stylex::api::{FileContext, transform_source_in, transform_source_mapped_in};
use oj_stylex::module_resolution::StdFs;
use oj_stylex::options::CompilerOptions;
use oxc_allocator::Allocator;

fn fixture_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("stylex-rs-splicemap-fixture");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("package.json"), r#"{"name":"fx"}"#).expect("package.json");
    dir
}

/// A file long enough that a line-count change is unmistakable, with the
/// stylex work at the very bottom.
fn long_source() -> String {
    let filler: String = (0..300).map(|i| format!("const pad{i} = {i};\n")).collect();
    format!(
        "import * as stylex from '@stylexjs/stylex';\n{filler}\
         export const styles = stylex.create({{\n  \
         box: {{ color: 'red', ':hover': {{ color: 'blue' }} }},\n}});\n\
         export const canary = 1;\n"
    )
}

struct Decoded {
    tokens: Vec<(u32, u32, u32, u32)>,
}

/// Minimal v3 mappings decoder: enough to check where generated lines land.
fn decode(map_json: &str) -> Decoded {
    let v: serde_json::Value = serde_json::from_str(map_json).expect("valid json");
    let mappings = v["mappings"].as_str().expect("mappings");
    let (mut gen_line, mut src_line, mut src_col) = (0i64, 0i64, 0i64);
    let mut tokens = Vec::new();
    for segment_line in mappings.split(';') {
        let mut gen_col = 0i64;
        for seg in segment_line.split(',').filter(|s| !s.is_empty()) {
            let fields = vlq(seg);
            if fields.is_empty() {
                continue;
            }
            gen_col += fields[0];
            if fields.len() >= 4 {
                src_line += fields[2];
                src_col += fields[3];
                tokens.push((
                    gen_line as u32,
                    gen_col as u32,
                    src_line as u32,
                    src_col as u32,
                ));
            }
        }
        gen_line += 1;
    }
    Decoded { tokens }
}

fn vlq(seg: &str) -> Vec<i64> {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut value, mut shift) = (0i64, 0u32);
    for ch in seg.bytes() {
        let digit = B64.iter().position(|&c| c == ch).expect("base64 digit") as i64;
        value += (digit & 31) << shift;
        if digit & 32 != 0 {
            shift += 5;
            continue;
        }
        let negative = value & 1 == 1;
        value >>= 1;
        out.push(if negative { -value } else { value });
        value = 0;
        shift = 0;
    }
    out
}

fn compile(source: &str, want_map: bool) -> (String, Option<String>) {
    let root = fixture_root();
    let filename = root.join("src/input.tsx");
    let ctx = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: &root,
    };
    let options = CompilerOptions::from_json(&serde_json::json!({}))
        .expect("options")
        .resolve()
        .expect("resolve");
    let allocator = Allocator::default();
    let out = if want_map {
        transform_source_mapped_in(&allocator, &ctx, &options, &StdFs, true)
    } else {
        transform_source_in(&allocator, &ctx, &options, &StdFs)
    }
    .expect("compiles")
    .expect("not pre-gated");
    (out.code, out.map)
}

#[test]
fn mapping_does_not_change_the_emitted_bytes() {
    let source = long_source();
    let (unmapped, none) = compile(&source, false);
    let (mapped, some) = compile(&source, true);
    assert!(none.is_none(), "unmapped call must not build a map");
    assert!(some.is_some(), "mapped call must build a map");
    assert_eq!(unmapped, mapped, "the map must not perturb output bytes");
}

#[test]
fn trailing_code_maps_to_its_true_source_line() {
    let source = long_source();
    let (code, map) = compile(&source, true);
    let map = map.expect("map");
    let decoded = decode(&map);
    assert!(!decoded.tokens.is_empty(), "map has no tokens");

    // `canary` sits on the last line of both source and output, but the
    // compiled object between them changes the line count.
    let src_line = source
        .lines()
        .position(|l| l.contains("canary"))
        .expect("canary in source");
    let gen_line = code
        .lines()
        .position(|l| l.contains("canary"))
        .expect("canary in output");
    assert_ne!(
        src_line, gen_line,
        "fixture must actually shift lines, else the test proves nothing"
    );
    let mapped = decoded
        .tokens
        .iter()
        .find(|(g, _, _, _)| *g as usize == gen_line)
        .unwrap_or_else(|| panic!("no token for generated line {gen_line}"));
    assert_eq!(
        mapped.2 as usize, src_line,
        "canary mapped to source line {} but really lives on {src_line}",
        mapped.2
    );
}

#[test]
fn every_generated_line_is_mapped() {
    let source = long_source();
    let (code, map) = compile(&source, true);
    let decoded = decode(&map.expect("map"));
    let mapped: std::collections::HashSet<u32> = decoded.tokens.iter().map(|t| t.0).collect();
    let unmapped: Vec<usize> = code
        .lines()
        .enumerate()
        .filter(|(i, l)| !l.trim().is_empty() && !mapped.contains(&(*i as u32)))
        .map(|(i, _)| i)
        .collect();
    assert!(
        unmapped.is_empty(),
        "{} non-blank generated lines have no mapping: {:?}",
        unmapped.len(),
        &unmapped[..unmapped.len().min(10)]
    );
}

#[test]
fn untouched_leading_code_maps_one_to_one() {
    let source = long_source();
    let (_, map) = compile(&source, true);
    let decoded = decode(&map.expect("map"));
    // Everything before the create call is copied verbatim, so generated and
    // source positions must agree exactly.
    for &(gen_line, gen_col, src_line, src_col) in decoded.tokens.iter().take(50) {
        assert_eq!(
            (gen_line, gen_col),
            (src_line, src_col),
            "verbatim run drifted"
        );
    }
}
