//! Per-shape compile throughput. Default: the small four-define env the server
//! sets for a bare project. `--big-env` (or `BENCH_BIG_ENV=1`): a 40-`VITE_*`
//! env with a ~2 KB `import.meta.env` blob, the shape of a real dotenv-heavy
//! app, where the per-compile define handling used to dominate small modules.
//!
//!   cargo run --release -p oj_compiler --example bench_compile [-- --big-env]

use std::path::Path;
use std::time::Instant;

use oj_compiler::{compile, set_import_meta_env, CompileOptions};

/// App module reading `import.meta.env.*` (gated by the SIMD finder).
const SRC_ENV: &str = r#"
import { useState, useEffect } from "react";

const API = import.meta.env.VITE_API_URL;
const MODE = import.meta.env.MODE;

interface Props { label: string; count?: number }

export function Widget({ label, count = 0 }: Props) {
  const [n, setN] = useState<number>(count);
  const dev = import.meta.env.DEV;
  useEffect(() => {
    if (dev) console.log("mounted", label, API, MODE);
  }, [label]);
  return (
    <button className="widget" onClick={() => setN((v) => v + 1)} data-api={API}>
      {label}: {n} {dev ? "(dev)" : "(prod)"}
    </button>
  );
}

export function List({ items }: { items: string[] }) {
  return <ul>{items.map((it, i) => <li key={i}><Widget label={it} /></li>)}</ul>;
}
"#;

/// ESM dependency shape: no `import.meta.env`, but `process.env.NODE_ENV`
/// trips the gate through a plain (non-`import.meta`) key.
const SRC_NODE_ENV: &str = r#"
import { jsx } from "react/jsx-runtime";
export function warn(msg) { if (process.env.NODE_ENV !== "production") { console.warn(msg); } }
export const Box = (p) => jsx("div", { ...p, "data-env": process.env.NODE_ENV });
export default { warn, Box };
"#;

/// Plain component module: mentions no define at all.
const SRC_PLAIN: &str = r#"
import { useState } from "react";

export function Counter({ a, b }: { a: number; b: string }) {
  const [x, setX] = useState<number>(a);
  return <div className="counter" onClick={() => setX(x + 1)}>{b} {x}</div>;
}

export function Label({ text }: { text: string }) {
  return <span className="label">{text}</span>;
}
"#;

fn small_env() -> Vec<(String, String)> {
    vec![
        ("import.meta.env.VITE_API_URL".into(), "\"https://api.example.com\"".into()),
        ("import.meta.env.MODE".into(), "\"development\"".into()),
        ("import.meta.env.DEV".into(), "true".into()),
        (
            "import.meta.env".into(),
            "({\"VITE_API_URL\":\"https://api.example.com\",\"MODE\":\"development\",\"DEV\":true})".into(),
        ),
        ("process.env.NODE_ENV".into(), "\"development\"".into()),
    ]
}

/// What the server sets for a project with 40 `VITE_*` vars: the per-key
/// defines, the full `import.meta.env` object, and the three NODE_ENV spellings.
fn big_env() -> Vec<(String, String)> {
    let mut obj = String::from(
        "{\"BASE_URL\":\"/\",\"MODE\":\"development\",\"DEV\":true,\"PROD\":false,\"SSR\":false",
    );
    let mut defines: Vec<(String, String)> = vec![
        ("import.meta.env.BASE_URL".into(), "\"/\"".into()),
        ("import.meta.env.MODE".into(), "\"development\"".into()),
        ("import.meta.env.DEV".into(), "true".into()),
        ("import.meta.env.PROD".into(), "false".into()),
        ("import.meta.env.SSR".into(), "false".into()),
    ];
    for i in 0..40 {
        let key = if i == 0 {
            "VITE_API_URL".to_string()
        } else {
            format!("VITE_VAR_{i}")
        };
        let value = format!("\"https://service-{i}.example.com/api/v1\"");
        obj.push_str(&format!(",\"{key}\":{value}"));
        defines.push((format!("import.meta.env.{key}"), value));
    }
    obj.push('}');
    defines.push(("import.meta.env".into(), format!("({obj})")));
    for key in [
        "process.env.NODE_ENV",
        "global.process.env.NODE_ENV",
        "globalThis.process.env.NODE_ENV",
    ] {
        defines.push((key.into(), "\"development\"".into()));
    }
    defines
}

fn bench_shape(name: &str, path: &Path, src: &str, opts: &CompileOptions, iters: usize) {
    for _ in 0..(iters / 20).max(1) {
        let _ = compile(path, src, opts).unwrap();
    }
    let t = Instant::now();
    for _ in 0..iters {
        let out = compile(path, src, opts).unwrap();
        std::hint::black_box(out);
    }
    let el = t.elapsed();
    println!(
        "{name:<28} {iters:>6} compiles in {el:>12?}  =  {:>8.2} us/compile  ({:>7.0} compiles/sec)",
        el.as_nanos() as f64 / iters as f64 / 1000.0,
        iters as f64 / el.as_secs_f64(),
    );
}

fn main() {
    let big = std::env::args().any(|a| a == "--big-env")
        || std::env::var_os("BENCH_BIG_ENV").is_some_and(|v| v != "0" && !v.is_empty());
    let env = if big { big_env() } else { small_env() };
    let blob = env
        .iter()
        .find(|(k, _)| k == "import.meta.env")
        .map(|(_, v)| v.len())
        .unwrap_or(0);
    println!(
        "env: {} define pairs, import.meta.env blob {blob} bytes ({})",
        env.len(),
        if big {
            "--big-env"
        } else {
            "default; pass --big-env for 40 VITE vars"
        }
    );
    set_import_meta_env(env);

    let opts = CompileOptions::dev();

    let out = compile(Path::new("Widget.tsx"), SRC_ENV, &opts).expect("compile");
    assert!(
        out.code.contains("\"development\""),
        "define must be replaced"
    );
    assert!(
        !out.code.contains("import.meta.env"),
        "no bare import.meta.env left"
    );
    let dep = compile(Path::new("dep.js"), SRC_NODE_ENV, &opts).expect("compile");
    assert!(
        !dep.code.contains("process.env.NODE_ENV"),
        "NODE_ENV must be replaced"
    );

    bench_shape(
        "env-using module",
        Path::new("Widget.tsx"),
        SRC_ENV,
        &opts,
        40_000,
    );
    bench_shape(
        "NODE_ENV dep module",
        Path::new("dep.js"),
        SRC_NODE_ENV,
        &opts,
        40_000,
    );
    bench_shape(
        "plain module (no env)",
        Path::new("Counter.tsx"),
        SRC_PLAIN,
        &opts,
        40_000,
    );
}
