//! `assemble`: NDJSON rule metadata tuples on stdin, assembled CSS on stdout.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Instant;

use crate::assemble::{AssembleConfig, RuleRegistry, assemble};
use crate::rules::StylexRule;
use serde::{Deserialize, Deserializer};
use serde_json::json;

#[derive(Deserialize)]
struct RuleBody {
    ltr: Arc<str>,
    #[serde(default)]
    rtl: Option<Arc<str>>,
    #[serde(default, rename = "constKey")]
    const_key: Option<Arc<str>>,
    #[serde(default, rename = "constVal", deserialize_with = "present_value")]
    const_val: Option<Box<serde_json::Value>>,
}

// A present constVal is kept even when null, as from_metadata_tuple does.
fn present_value<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Box<serde_json::Value>>, D::Error> {
    serde_json::Value::deserialize(d).map(|v| Some(Box::new(v)))
}

// The typed form skips the per-line Value tree; whatever it rejects re-parses
// through the Value path so the reported error text is unchanged.
fn parse_rule_line(line: &str) -> Result<StylexRule, String> {
    if let Ok((class_name, body, priority)) =
        serde_json::from_str::<(Arc<str>, RuleBody, f64)>(line)
    {
        return Ok(StylexRule {
            class_name,
            ltr: body.ltr,
            rtl: body.rtl,
            const_key: body.const_key,
            const_val: body.const_val,
            priority,
        });
    }
    let value: serde_json::Value = serde_json::from_str(line).map_err(|err| err.to_string())?;
    StylexRule::from_metadata_tuple(&value)
}

pub fn run(
    config: Option<&str>,
    timings: bool,
    bench_incremental: Option<u32>,
    bench_files: usize,
) {
    let raw_cfg: serde_json::Value = match config {
        Some(text) => match serde_json::from_str(text) {
            Ok(v) => v,
            Err(err) => {
                eprintln!(
                    "{}",
                    json!({ "error": "BAD_CONFIG", "detail": err.to_string() })
                );
                std::process::exit(64);
            }
        },
        None => serde_json::Value::Null,
    };
    let cfg = match AssembleConfig::from_json(&raw_cfg) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!(
                "{}",
                json!({ "error": "BAD_CONFIG", "detail": err.to_string() })
            );
            std::process::exit(64);
        }
    };

    let mut input = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
        std::process::exit(1);
    }

    let parse_start = Instant::now();
    let mut rules = Vec::new();
    for (i, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_rule_line(line) {
            Ok(rule) => rules.push(rule),
            Err(detail) => {
                eprintln!(
                    "{}",
                    json!({ "error": "BAD_RULE", "detail": format!("line {}: {detail}", i + 1) })
                );
                std::process::exit(65);
            }
        }
    }
    let parse_ns = parse_start.elapsed().as_nanos();

    if let Some(deltas) = bench_incremental {
        run_incremental_bench(rules, &cfg, deltas, bench_files);
        return;
    }

    let assemble_start = Instant::now();
    let css = match assemble(&rules, &cfg) {
        Ok(css) => css,
        Err(err) => {
            eprintln!(
                "{}",
                json!({ "error": "ASSEMBLE", "detail": err.to_string() })
            );
            std::process::exit(1);
        }
    };
    let assemble_ns = assemble_start.elapsed().as_nanos();

    let mut out = std::io::stdout().lock();
    out.write_all(css.as_bytes()).expect("stdout write");
    crate::timings::report_to_stderr();
    if timings {
        eprintln!(
            "{}",
            json!({
                "rules": rules.len(),
                "parse_ns": parse_ns as u64,
                "assemble_ns": assemble_ns as u64,
                "bytes": css.len(),
            })
        );
    }
}

// HMR-path bench: partition the rules into files, then time N single-file
// swaps (set_file_rules + emit) through the RuleRegistry incremental path.
fn run_incremental_bench(rules: Vec<StylexRule>, cfg: &AssembleConfig, deltas: u32, files: usize) {
    let file_count = files.clamp(1, rules.len().max(1));
    let chunk_len = rules.len().div_ceil(file_count).max(1);
    let mut chunks: Vec<Vec<StylexRule>> = rules
        .chunks(chunk_len)
        .map(<[StylexRule]>::to_vec)
        .collect();
    if chunks.is_empty() {
        // Empty stdin: one empty file keeps the delta loop's modulo alive.
        chunks.push(Vec::new());
    }
    let keys: Vec<String> = (0..chunks.len()).map(|i| format!("file{i:05}")).collect();

    let mut registry = RuleRegistry::new();
    for (i, chunk) in chunks.iter().enumerate() {
        registry.set_file_rules(&keys[i], chunk.clone());
    }
    let one_shot = assemble(&rules, cfg).expect("one-shot assemble");
    let warm_start = Instant::now();
    let initial = registry.emit(cfg).expect("initial emit");
    let warmup_ms = warm_start.elapsed().as_secs_f64() * 1e3;
    let initial_matches = initial == one_shot;

    let mut per_delta_us: Vec<f64> = Vec::with_capacity(deltas as usize);
    let mut toggled = vec![false; chunks.len()];
    let mut target = 0usize;
    for _ in 0..deltas {
        target = (target + 7919) % chunks.len();
        let mut swapped = chunks[target].clone();
        if !toggled[target] {
            swapped.push(StylexRule {
                class_name: format!("xbench{target}").into(),
                ltr: format!(".xbench{target}{{color:red}}").into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: 3000.0,
            });
        }
        toggled[target] = !toggled[target];
        let t = Instant::now();
        registry.set_file_rules(&keys[target], swapped);
        let css = registry.emit(cfg).expect("delta emit");
        per_delta_us.push(t.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(css.len());
    }

    let final_matches = registry.emit(cfg).expect("final emit")
        == assemble(&registry.all_rules(), cfg).expect("final one-shot");

    let mut ranked = per_delta_us.clone();
    ranked.sort_by(|a, b| a.total_cmp(b));
    let quantile = |q: f64| -> serde_json::Value {
        match ranked.is_empty() {
            true => serde_json::Value::Null,
            false => json!(ranked[((ranked.len() - 1) as f64 * q).round() as usize]),
        }
    };
    println!(
        "{}",
        json!({
            "rules": rules.len(),
            "files": chunks.len(),
            "deltas": deltas,
            "warmup_emit_ms": warmup_ms,
            "initial_css_matches_one_shot": initial_matches,
            "final_css_matches_one_shot": final_matches,
            "per_delta_us": per_delta_us,
            "p50_us": quantile(0.5),
            "p95_us": quantile(0.95),
            "max_us": quantile(1.0),
        })
    );
    if !initial_matches || !final_matches {
        std::process::exit(70);
    }
}
