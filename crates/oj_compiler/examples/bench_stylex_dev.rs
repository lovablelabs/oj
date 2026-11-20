//! Dev compile throughput of StyleX TSX modules: what the dev server does to a
//! module the `@stylexjs/unplugin` gate admits, from its source to the served
//! code and sourcemap (the StyleX pass, the CJS-interop rewrite, the compile).
//!
//! The corpus is a generated app: presentational components (no value import
//! of a CommonJS dep, so the interop pass leaves them as they are) and
//! stateful ones (hooks off `react`, which the interop pass rewrites), half
//! and half, each with a `stylex.create` of a dozen namespaces over theme
//! tokens, pseudo-classes and media queries. `react` stands here for any
//! dependency whose imports need interop: the stateful half is the worst
//! case, a module the dev server cannot compile in one parse.
//!
//! `--pipeline dev` (the default) is the compile of a module no app plugin
//! transforms; `--pipeline steps` the one where a plugin could, StyleX
//! running ahead of the plugin chain.
//!
//!   cargo run --release -p oj_compiler --example bench_stylex_dev [-- --modules 200 --runs 7 --pipeline dev]

use std::path::{Path, PathBuf};
use std::time::Instant;

use oj_compiler::interop::{
    rewrite_cjs_interop_logged, ImporterFormat, InteropKind, InteropTarget, LaterTransforms,
};
use oj_compiler::{
    compile_module_with_input_maps, compile_stylex_module, CompileOptions, CompileOutput, InputMap,
    InteropPass,
};
use oj_stylex::plugin::StylexPlugin;

const TOKENS: &str = r#"import * as stylex from "@stylexjs/stylex";

export const colors = stylex.defineVars({
  accent: "rgb(0, 128, 0)",
  surface: "rgb(240, 240, 240)",
  text: "rgb(20, 20, 20)",
  muted: "rgb(120, 120, 120)",
  border: "rgba(0, 0, 0, 0.12)",
});

export const spacing = stylex.defineVars({
  xs: "4px",
  sm: "8px",
  md: "16px",
  lg: "24px",
  xl: "40px",
});
"#;

const PROPERTIES: &[(&str, &[&str])] = &[
    (
        "display",
        &["\"flex\"", "\"grid\"", "\"block\"", "\"inline-flex\""],
    ),
    (
        "alignItems",
        &["\"center\"", "\"flex-start\"", "\"stretch\""],
    ),
    (
        "justifyContent",
        &["\"space-between\"", "\"center\"", "\"flex-end\""],
    ),
    (
        "padding",
        &["spacing.sm", "spacing.md", "12", "\"2px 6px\""],
    ),
    ("marginBlock", &["spacing.xs", "spacing.lg", "0"]),
    ("gap", &["spacing.sm", "spacing.md", "6"]),
    ("color", &["colors.text", "colors.muted", "\"white\""]),
    (
        "backgroundColor",
        &["colors.surface", "colors.accent", "\"transparent\""],
    ),
    ("borderRadius", &["4", "8", "\"50%\""]),
    ("borderColor", &["colors.border", "\"currentColor\""]),
    ("fontSize", &["\"0.875rem\"", "\"1rem\"", "\"1.25rem\""]),
    ("fontWeight", &["400", "600", "700"]),
    ("lineHeight", &["1.2", "1.5"]),
    ("opacity", &["0.6", "1"]),
    ("transitionDuration", &["\"120ms\"", "\"240ms\""]),
    ("width", &["\"100%\"", "\"auto\"", "320"]),
];

const NAMESPACES: &[&str] = &[
    "root", "header", "title", "subtitle", "body", "row", "cell", "footer", "action", "icon",
    "badge", "label", "divider", "overlay", "panel", "caption",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next() % items.len()]
    }
}

fn create_call(rng: &mut Rng, namespaces: &[&str]) -> String {
    let mut out = String::from("const styles = stylex.create({\n");
    for ns in namespaces {
        out.push_str(&format!("  {ns}: {{\n"));
        let count = 4 + rng.next() % 5;
        let start = rng.next() % PROPERTIES.len();
        for i in 0..count {
            let (prop, values) = PROPERTIES[(start + i) % PROPERTIES.len()];
            let value = rng.pick(values);
            match rng.next() % 6 {
                0 => out.push_str(&format!(
                    "    {prop}: {{ default: {value}, \":hover\": {} }},\n",
                    rng.pick(values)
                )),
                1 => out.push_str(&format!(
                    "    {prop}: {{ default: {value}, \"@media (min-width: 768px)\": {} }},\n",
                    rng.pick(values)
                )),
                _ => out.push_str(&format!("    {prop}: {value},\n")),
            }
        }
        out.push_str("  },\n");
    }
    out.push_str("});\n");
    out
}

/// One generated component; `stateful` ones take hooks off `react`.
fn module(index: usize, stateful: bool) -> String {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((index as u64 + 1) * 0x1000_0000_01B3));
    let count = 10 + rng.next() % 5;
    let start = rng.next() % NAMESPACES.len();
    let namespaces: Vec<&str> = (0..count)
        .map(|i| NAMESPACES[(start + i) % NAMESPACES.len()])
        .collect();
    let name = format!("Widget{index}");
    let mut out = String::new();
    out.push_str("import * as stylex from \"@stylexjs/stylex\";\n");
    if stateful {
        out.push_str("import { useCallback, useMemo, useState } from \"react\";\n");
    }
    out.push_str("import type { ReactNode } from \"react\";\n");
    out.push_str("import { colors, spacing } from \"./tokens.stylex\";\n\n");
    out.push_str(&format!(
        "const fade{index} = stylex.keyframes({{\n  from: {{ opacity: 0 }},\n  to: {{ opacity: 1 }},\n}});\n\n"
    ));
    out.push_str(&create_call(&mut rng, &namespaces));
    out.push_str(&format!(
        "\ninterface {name}Props {{\n  title: string;\n  items: ReadonlyArray<{{ id: string; label: string; done?: boolean }}>;\n  active?: boolean;\n  compact?: boolean;\n  children?: ReactNode;\n  onSelect?: (id: string) => void;\n}}\n\n"
    ));
    out.push_str(&format!(
        "function Row{index}({{ label, done, onPick }}: {{ label: string; done?: boolean; onPick?: () => void }}) {{\n  return (\n    <li {{...stylex.props(styles.{a}, done && styles.{b})}} onClick={{onPick}}>\n      <span {{...stylex.props(styles.{c})}}>{{label}}</span>\n      {{done ? <em {{...stylex.props(styles.{d})}}>done</em> : null}}\n    </li>\n  );\n}}\n\n",
        a = namespaces[1],
        b = namespaces[2],
        c = namespaces[3],
        d = namespaces[4],
    ));
    out.push_str(&format!(
        "export function {name}({{ title, items, active = false, compact, children, onSelect }}: {name}Props) {{\n"
    ));
    if stateful {
        out.push_str(
            "  const [selected, setSelected] = useState<string | null>(null);\n  const [open, setOpen] = useState(true);\n  const visible = useMemo(() => items.filter((item) => open || item.done !== true), [items, open]);\n  const pick = useCallback(\n    (id: string) => {\n      setSelected(id);\n      onSelect?.(id);\n    },\n    [onSelect],\n  );\n",
        );
    } else {
        out.push_str(
            "  const selected: string | null = null;\n  const open = !compact;\n  const visible = items;\n  const pick = (id: string) => onSelect?.(id);\n",
        );
    }
    out.push_str(&format!(
        "  return (\n    <section {{...stylex.props(styles.{root}, active && styles.{active}, compact && styles.{compact})}}>\n      <header {{...stylex.props(styles.{header})}}>\n        <h2 {{...stylex.props(styles.{title})}}>{{title}}</h2>\n        <button type=\"button\" {{...stylex.props(styles.{action})}}{toggle}>\n          {{open ? \"Collapse\" : \"Expand\"}}\n        </button>\n      </header>\n      <ul {{...stylex.props(styles.{body})}}>\n        {{visible.map((item) => (\n          <Row{index}\n            key={{item.id}}\n            label={{item.id === selected ? `${{item.label}} *` : item.label}}\n            done={{item.done}}\n            onPick={{() => pick(item.id)}}\n          />\n        ))}}\n      </ul>\n      <footer {{...stylex.props(styles.{footer})}}>\n        <span {{...stylex.props(styles.{label}, selected != null && styles.{badge})}}>\n          {{visible.length}} of {{items.length}}\n        </span>\n        {{children}}\n      </footer>\n    </section>\n  );\n}}\n",
        root = namespaces[0],
        active = namespaces[5],
        compact = namespaces[6],
        header = namespaces[7],
        title = namespaces[8],
        action = namespaces[9],
        body = namespaces[1],
        footer = namespaces[2],
        label = namespaces[3],
        badge = namespaces[4],
        toggle = if stateful {
            " onClick={() => setOpen((value) => !value)}"
        } else {
            ""
        },
    ));
    out
}

struct Corpus {
    root: PathBuf,
    modules: Vec<(PathBuf, String)>,
}

fn corpus(count: usize) -> Corpus {
    let root = std::env::temp_dir().join(format!("oj-bench-stylex-dev-{}", std::process::id()));
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("corpus dir");
    std::fs::write(
        root.join("package.json"),
        "{\"name\":\"bench\",\"type\":\"module\"}\n",
    )
    .expect("package.json");
    std::fs::write(src.join("tokens.stylex.ts"), TOKENS).expect("tokens");
    let modules = (0..count)
        .map(|index| {
            let file = src.join(format!("Widget{index}.tsx"));
            let source = module(index, index % 2 == 1);
            std::fs::write(&file, &source).expect("module");
            (file, source)
        })
        .collect();
    Corpus { root, modules }
}

/// The pre-bundle of `react`, a CommonJS dep: the one import the interop
/// pass rewrites.
fn interop_target(spec: &str) -> Option<InteropTarget> {
    (spec == "react").then(|| {
        InteropTarget::new(
            "/node_modules/.vite/deps/react.js?v=1a2b3c4d",
            InteropKind::PreBundle,
        )
    })
}

fn served_url(spec: &str) -> Option<String> {
    Some(match spec {
        "react" => "/node_modules/.vite/deps/react.js?v=1a2b3c4d".into(),
        "react/jsx-dev-runtime" => {
            "/node_modules/.vite/deps/react_jsx-dev-runtime.js?v=1a2b3c4d".into()
        }
        "@stylexjs/stylex" => "/node_modules/.vite/deps/@stylexjs_stylex.js?v=1a2b3c4d".into(),
        relative if relative.starts_with("./") => format!("/src/{}.ts", &relative[2..]),
        _ => return None,
    })
}

fn compile_options() -> CompileOptions {
    let mut opts = CompileOptions::dev();
    opts.class_field_set_semantics = Some(false);
    opts.verbatim_module_syntax = Some(false);
    opts
}

/// The dev server's client compile of an app module the StyleX gate admits
/// and no app plugin transforms.
fn dev_compile(plugin: &StylexPlugin, file: &Path, source: &str) -> CompileOutput {
    compile_stylex_module(
        file,
        source,
        &compile_options(),
        plugin,
        InteropPass {
            format: ImporterFormat::ESM,
            later: &LaterTransforms::default(),
            may_need_interop: &|spec: &str| spec == "react",
            interop: &interop_target,
            warn: &mut |_| {},
        },
        &mut |spec: &str| served_url(spec),
    )
    .expect("compile")
}

/// The same module where an app plugin's transform could take it: the StyleX
/// transform ahead of the plugin chain as a string, then the interop rewrite
/// and the compile, its map folded through the transform's.
fn dev_compile_in_steps(plugin: &StylexPlugin, file: &Path, source: &str) -> CompileOutput {
    let pre = plugin
        .transform_source_mapped(file, source, true)
        .expect("stylex compile");
    let (code, mappings) = match pre {
        Some(out) => (out.code, out.mappings),
        None => (source.to_string(), None),
    };
    let interopped = rewrite_cjs_interop_logged(
        &code,
        file,
        ImporterFormat::ESM,
        &LaterTransforms::default(),
        &|spec: &str| spec == "react",
        &interop_target,
        &mut |_| {},
    );
    let input_maps: Vec<InputMap> = mappings
        .into_iter()
        .map(|mappings| InputMap::Parsed(mappings.into_sourcemap(Some(source))))
        .collect();
    compile_module_with_input_maps(
        file,
        interopped.as_deref().unwrap_or(&code),
        &compile_options(),
        Some(&mut |spec: &str| served_url(spec)),
        &input_maps,
    )
    .expect("compile")
}

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn count_arg(name: &str, default: usize) -> usize {
    arg(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn main() {
    let (count, runs) = (count_arg("--modules", 200), count_arg("--runs", 7).max(5));
    let compile: fn(&StylexPlugin, &Path, &str) -> CompileOutput =
        match arg("--pipeline").as_deref() {
            None | Some("dev") => dev_compile,
            Some("steps") => dev_compile_in_steps,
            Some(other) => panic!("--pipeline is `dev` or `steps`, not `{other}`"),
        };
    let corpus = corpus(count);
    let plugin =
        StylexPlugin::new(&serde_json::json!({}), &corpus.root, true, true).expect("plugin");
    let bytes: usize = corpus.modules.iter().map(|(_, s)| s.len()).sum();
    println!(
        "{count} modules, {:.1} KB of source ({:.2} KB each)",
        bytes as f64 / 1024.0,
        bytes as f64 / 1024.0 / count as f64
    );

    // Each group with the parity of its modules' indices (see `corpus`).
    let groups = [
        ("all", None),
        ("presentational", Some(0)),
        ("stateful", Some(1)),
    ];
    for (label, parity) in groups {
        let modules: Vec<&(PathBuf, String)> = corpus
            .modules
            .iter()
            .enumerate()
            .filter(|(i, _)| parity.is_none_or(|parity| i % 2 == parity))
            .map(|(_, m)| m)
            .collect();
        let mut digest = 0usize;
        let mut samples = Vec::with_capacity(runs);
        for run in 0..=runs {
            let started = Instant::now();
            for (file, source) in &modules {
                let out = compile(&plugin, file, source);
                digest = digest
                    .wrapping_add(out.code.len())
                    .wrapping_add(out.map_json.map_or(0, |m| m.len()));
            }
            // The first pass warms the plugin's fs memo and the allocator.
            if run > 0 {
                samples.push(started.elapsed().as_secs_f64() * 1e3);
            }
        }
        let total = median(samples.clone());
        let (min, max) = samples
            .iter()
            .fold((f64::MAX, 0f64), |(lo, hi), &s| (lo.min(s), hi.max(s)));
        println!(
            "{label:>15}: median {total:8.2} ms over {runs} runs (min {min:.2}, max {max:.2}), {:7.1} us/module, {} modules, digest {digest}",
            total * 1e3 / modules.len() as f64,
            modules.len(),
        );
    }
    let _ = std::fs::remove_dir_all(&corpus.root);
}
