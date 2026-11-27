//! Stylesheet-assembly bench over a synthetic StyleX project: the one-shot
//! `assemble` and the `RuleRegistry` HMR paths (file edit, add, remove, const
//! edits, and the non-incremental fallback).
//!
//! ```text
//! cargo run --release -p oj_stylex --example assemble_bench -- \
//!     --rules 20000 --consts 300 --runs 9
//! ```
//!
//! One JSON line per scenario: the median and minimum of `runs` samples in
//! microseconds, as wall time and as on-CPU time of the thread (64-bit Linux;
//! equal to wall elsewhere), plus a hash of the CSS the scenario
//! ended on so two builds can be compared for identical output. `--dump`
//! prints the project as NDJSON metadata tuples (`fru assemble` input) instead.

use std::time::Instant;

use oj_stylex::assemble::{AssembleConfig, RuleRegistry, assemble};
use oj_stylex::rules::StylexRule;
use oj_stylex::shared::priorities::get_priority;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn hash(&mut self) -> String {
        const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut x = self.next();
        let mut out = String::from("x");
        for _ in 0..7 {
            out.push(DIGITS[(x % 36) as usize] as char);
            x /= 36;
        }
        out
    }
}

const LONGHANDS: &[&str] = &[
    "color",
    "background-color",
    "display",
    "position",
    "width",
    "height",
    "min-width",
    "max-height",
    "font-size",
    "font-weight",
    "line-height",
    "opacity",
    "z-index",
    "cursor",
    "align-items",
    "justify-content",
    "flex-direction",
    "flex-grow",
    "text-align",
    "overflow-x",
    "transform",
    "transition-duration",
    "border-top-color",
    "padding-top",
    "margin-bottom",
    "grid-template-columns",
    "box-shadow",
    "white-space",
];
const SHORTHANDS: &[&str] = &["margin", "padding", "gap", "inset", "border-radius", "flex"];
const VALUES: &[&str] = &[
    "red",
    "#1877f2",
    "rgba(0,0,0,.4)",
    "0",
    "1px",
    "4px",
    "8px",
    "12px",
    "16px",
    "1.5rem",
    "100%",
    "auto",
    "flex",
    "none",
    "center",
    "inherit",
    "calc(100% - 16px)",
    "translateX(-50%)",
    "\"Helvetica Neue\",Arial,sans-serif",
];
const PSEUDOS: &[&str] = &[
    ":hover",
    ":focus",
    ":active",
    ":focus-visible",
    "::before",
    "::after",
    ":first-child",
];
const MEDIA: &[&str] = &[
    "@media (min-width: 480px)",
    "@media (min-width: 768px)",
    "@media (min-width: 1024px)",
    "@media (min-width: 1280px)",
    "@media (max-width: 767.5px)",
    "@media (max-width: 479px)",
    "@media (prefers-color-scheme: dark)",
    "@media (hover: hover)",
    "@media print",
    "@supports (display: grid)",
    "@container (min-width: 400px)",
];
const CONST_AT_RULES: &[&str] = &[
    "@media (min-width: 640px)",
    "@media (min-width: 900px)",
    "@media (max-width: 1199.5px)",
    "@container (min-width: 320px)",
    "@supports (backdrop-filter: blur(1px))",
];

fn plain(class_name: &str, ltr: String, priority: f64) -> StylexRule {
    StylexRule {
        class_name: class_name.into(),
        ltr: ltr.into(),
        rtl: None,
        const_key: None,
        const_val: None,
        priority,
    }
}

struct Tokens {
    values: Vec<String>,
    at_rules: Vec<String>,
    vars: Vec<String>,
}

// defineConsts / defineVars output: const rules, a variable group and a theme.
fn token_file(rng: &mut Rng, consts: usize, tokens: &mut Tokens) -> Vec<StylexRule> {
    let mut rules = Vec::new();
    let group = rng.hash();
    let mut decls = String::new();
    for _ in 0..12 {
        let var = rng.hash();
        decls.push_str(&format!("--{var}:{};", rng.pick(VALUES)));
        tokens.vars.push(var);
    }
    rules.push(plain(&group, format!(":root, .{group}{{{decls}}}"), 0.0));
    let theme = rng.hash();
    rules.push(plain(
        &theme,
        format!(".{theme}, .{theme}:root{{{decls}}}"),
        0.5,
    ));
    for _ in 0..consts {
        let key = rng.hash();
        let roll = rng.below(100);
        let value = if roll < 10 {
            tokens.at_rules.push(key.clone());
            serde_json::json!(rng.pick(CONST_AT_RULES))
        } else if roll < 18 {
            tokens.values.push(key.clone());
            serde_json::json!(rng.below(64) as f64 / 4.0)
        } else if roll < 24 {
            tokens.values.push(key.clone());
            let var = &tokens.vars[rng.below(tokens.vars.len())];
            serde_json::json!(format!("var(--{var})"))
        } else if roll < 30 && !tokens.values.is_empty() {
            let other = tokens.values[rng.below(tokens.values.len())].clone();
            tokens.values.push(key.clone());
            serde_json::json!(format!("calc(var(--{other}) * 2)"))
        } else {
            tokens.values.push(key.clone());
            serde_json::json!(rng.pick(VALUES))
        };
        rules.push(StylexRule {
            class_name: key.as_str().into(),
            ltr: "".into(),
            rtl: None,
            const_key: Some(key.as_str().into()),
            const_val: Some(Box::new(value)),
            priority: 0.0,
        });
    }
    rules
}

fn component_rule(rng: &mut Rng, tokens: &Tokens) -> StylexRule {
    let class = rng.hash();
    let has_consts = !tokens.values.is_empty();
    let roll = rng.below(100);
    let (prop, base) = if rng.below(6) == 0 {
        (rng.pick(SHORTHANDS), 2000.0)
    } else {
        (rng.pick(LONGHANDS), 3000.0)
    };
    let value = rng.pick(VALUES);
    if roll < 12 {
        let pseudo = rng.pick(PSEUDOS);
        let ltr = format!(".{class}{pseudo}{{{prop}:{value}}}");
        return plain(&class, ltr, base + get_priority(pseudo));
    }
    if roll < 22 {
        let media = rng.pick(MEDIA);
        let ltr = format!("{media}{{.{class}.{class}{{{prop}:{value}}}}}");
        return plain(&class, ltr, base + get_priority(media));
    }
    if roll < 25 {
        let (media, pseudo) = (rng.pick(MEDIA), rng.pick(PSEUDOS));
        let ltr = format!("{media}{{.{class}.{class}{pseudo}{{{prop}:{value}}}}}");
        return plain(
            &class,
            ltr,
            base + get_priority(media) + get_priority(pseudo),
        );
    }
    if roll < 38 {
        let var = &tokens.vars[rng.below(tokens.vars.len())];
        return plain(&class, format!(".{class}{{{prop}:var(--{var})}}"), base);
    }
    if roll < 41 {
        return StylexRule {
            rtl: Some(format!(".{class}{{margin-right:{value}}}").into()),
            ..plain(&class, format!(".{class}{{margin-left:{value}}}"), 4000.0)
        };
    }
    if roll < 42 {
        let name = format!("{class}-B");
        let ltr = format!("@keyframes {name}{{from{{opacity:0;}}to{{opacity:1;}}}}");
        return plain(&name, ltr, 0.0);
    }
    if roll < 43 {
        let ltr = format!(".{class}{{float:var(--stylex-logical-start)}}");
        return plain(&class, ltr, 3000.0);
    }
    if roll < 55 && has_consts {
        let key = &tokens.values[rng.below(tokens.values.len())];
        return plain(&class, format!(".{class}{{{prop}:var(--{key})}}"), base);
    }
    if roll < 60 && !tokens.at_rules.is_empty() {
        let key = &tokens.at_rules[rng.below(tokens.at_rules.len())];
        let wrapper = format!("var(--{key})");
        let ltr = format!("{wrapper}{{.{class}.{class}{{{prop}:{value}}}}}");
        return plain(&class, ltr, base + get_priority(&wrapper));
    }
    plain(&class, format!(".{class}{{{prop}:{value}}}"), base)
}

struct Project {
    files: Vec<(String, Vec<StylexRule>)>,
    token_paths: Vec<String>,
    component_paths: Vec<String>,
    tokens: Tokens,
}

impl Project {
    fn rules_of(&self, path: &str) -> &[StylexRule] {
        let at = self
            .files
            .binary_search_by(|(p, _)| p.as_str().cmp(path))
            .expect("a project file");
        &self.files[at].1
    }

    // The first constant that is a plain value (not an at-rule, a number or
    // a reference to another variable): its token file and position there.
    fn value_constant(&self) -> Option<(&str, usize)> {
        let is_plain_value = |rule: &StylexRule| {
            rule.const_key.is_some()
                && rule
                    .const_val
                    .as_deref()
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.starts_with('@') && !value.contains("var("))
        };
        self.token_paths.iter().find_map(|path| {
            let at = self.rules_of(path).iter().position(is_plain_value)?;
            Some((path.as_str(), at))
        })
    }
}

// `dup` percent of a component's rules come from a shared pool of common
// atomic rules, the rest are its own.
fn project(rule_count: usize, const_count: usize, dup: usize, seed: u64) -> Project {
    let mut rng = Rng(seed);
    let mut tokens = Tokens {
        values: Vec::new(),
        at_rules: Vec::new(),
        vars: Vec::new(),
    };
    let mut files = Vec::new();
    let mut token_paths = Vec::new();
    let token_files = const_count.div_ceil(60).max(1);
    let mut left = const_count;
    for i in 0..token_files {
        let take = left.min(60);
        left -= take;
        let path = format!("src/tokens/t{i:04}.stylex.ts");
        files.push((path.clone(), token_file(&mut rng, take, &mut tokens)));
        token_paths.push(path);
    }
    let pool: Vec<StylexRule> = (0..(rule_count / 20).max(8))
        .map(|_| component_rule(&mut rng, &tokens))
        .collect();
    let mut component_paths = Vec::new();
    let mut made = 0;
    let mut index = 0;
    while made < rule_count {
        let len = (10 + rng.below(31)).min(rule_count - made);
        let rules = (0..len)
            .map(|_| {
                if rng.below(100) < dup {
                    // Squaring skews the draw towards the head of the pool.
                    let at = rng.below(pool.len()) * rng.below(pool.len()) / pool.len();
                    pool[at].clone()
                } else {
                    component_rule(&mut rng, &tokens)
                }
            })
            .collect();
        let path = format!("src/components/c{index:05}.tsx");
        files.push((path.clone(), rules));
        component_paths.push(path);
        made += len;
        index += 1;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Project {
        files,
        token_paths,
        component_paths,
        tokens,
    }
}

// On-CPU time of this thread: unlike wall time it does not count the time
// spent descheduled on a busy machine.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn cpu_ns() -> Option<u64> {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clock_id: i32, ts: *mut Timespec) -> i32;
    }
    const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `struct timespec` (two 64-bit fields
    // on 64-bit Linux) for the duration of the call.
    let status = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (status == 0).then(|| ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn cpu_ns() -> Option<u64> {
    None
}

#[derive(Default)]
struct Samples {
    wall_us: Vec<f64>,
    cpu_us: Vec<f64>,
}

impl Samples {
    fn time<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let cpu = cpu_ns();
        let wall = Instant::now();
        let out = std::hint::black_box(work());
        let wall_us = wall.elapsed().as_secs_f64() * 1e6;
        let cpu_us = match (cpu, cpu_ns()) {
            (Some(start), Some(end)) => (end - start) as f64 / 1e3,
            _ => wall_us,
        };
        self.wall_us.push(wall_us);
        self.cpu_us.push(cpu_us);
        out
    }
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

struct Bench {
    rules: usize,
    consts: usize,
    dup: usize,
    runs: usize,
    cfg: AssembleConfig,
    only: Option<Vec<String>>,
}

impl Bench {
    fn wants(&self, scenario: &str) -> bool {
        self.only
            .as_ref()
            .is_none_or(|names| names.iter().any(|n| n == scenario))
    }

    fn report(&self, scenario: &str, samples: &Samples, css: &str) {
        let min = |xs: &[f64]| xs.iter().copied().fold(f64::INFINITY, f64::min);
        println!(
            "{}",
            serde_json::json!({
                "scenario": scenario,
                "rules": self.rules,
                "consts": self.consts,
                "dup": self.dup,
                "runs": samples.wall_us.len(),
                "wall_median_us": median(&samples.wall_us),
                "wall_min_us": min(&samples.wall_us),
                "cpu_median_us": median(&samples.cpu_us),
                "cpu_min_us": min(&samples.cpu_us),
                "css_bytes": css.len(),
                "css_hash": format!("{:016x}", fnv(css)),
            })
        );
    }

    fn registry(&self, project: &Project) -> RuleRegistry {
        let mut registry = RuleRegistry::new();
        for (path, rules) in &project.files {
            registry.set_file_rules(path, rules.clone());
        }
        registry
    }

    // The registry's answer must be the one-shot answer over the same rules.
    fn check(&self, scenario: &str, registry: &mut RuleRegistry) -> String {
        let css = registry.emit(&self.cfg).expect("emit");
        let one_shot = assemble(&registry.all_rules(), &self.cfg).expect("one-shot");
        assert!(css == one_shot, "{scenario}: emit diverged from one-shot");
        css
    }

    // One sample: `change` the registry, then emit. A change the registry
    // sees nothing new in is answered from its cache, so a run that left the
    // generation where it was would time no assembly at all.
    fn timed(
        &self,
        scenario: &str,
        samples: &mut Samples,
        registry: &mut RuleRegistry,
        change: impl FnOnce(&mut RuleRegistry),
    ) -> String {
        let generation = registry.generation();
        let css = samples.time(|| {
            change(registry);
            registry.emit(&self.cfg).expect("emit")
        });
        assert!(
            registry.generation() > generation,
            "{scenario}: a timed run left the registry unchanged"
        );
        css
    }

    // `runs` edits over `paths`, visited `stride` apart: an even run adds a
    // rule to its file, the odd run after it takes that rule out of the same
    // file again, so every run is an edit the registry has to assemble.
    fn edit_and_revert(
        &self,
        scenario: &str,
        registry: &mut RuleRegistry,
        project: &Project,
        runs: usize,
        paths: &[String],
        stride: usize,
    ) -> Samples {
        let mut samples = Samples::default();
        for run in 0..runs {
            let path = &paths[(run / 2 * stride) % paths.len()];
            let mut rules = project.rules_of(path).to_vec();
            if run % 2 == 0 {
                let class = format!("xedit{run}");
                rules.push(plain(&class, format!(".{class}{{color:red}}"), 3e3));
            }
            self.timed(scenario, &mut samples, registry, |registry| {
                registry.set_file_rules(path, rules)
            });
        }
        samples
    }

    fn run(&self, project: &Project) {
        if self.wants("oneshot") {
            let all: Vec<StylexRule> = project
                .files
                .iter()
                .flat_map(|(_, rules)| rules.iter().cloned())
                .collect();
            let mut samples = Samples::default();
            let mut css = String::new();
            for _ in 0..self.runs {
                css = samples.time(|| assemble(&all, &self.cfg).expect("assemble"));
            }
            self.report("oneshot", &samples, &css);
        }

        if self.wants("cold_emit") {
            let mut samples = Samples::default();
            let mut css = String::new();
            for _ in 0..self.runs {
                let mut registry = self.registry(project);
                css = samples.time(|| registry.emit(&self.cfg).expect("emit"));
            }
            self.report("cold_emit", &samples, &css);
        }

        let mut registry = self.registry(project);
        registry.emit(&self.cfg).expect("warm-up emit");
        let components = &project.component_paths;

        if self.wants("edit") {
            let runs = self.runs.max(21);
            let samples =
                self.edit_and_revert("edit", &mut registry, project, runs, components, 7919);
            let css = self.check("edit", &mut registry);
            self.report("edit", &samples, &css);
        }

        if self.wants("edit_tokens") && self.consts > 0 {
            // A token file changes around its constants, which stay put.
            let runs = self.runs.max(21);
            let paths = &project.token_paths;
            let samples =
                self.edit_and_revert("edit_tokens", &mut registry, project, runs, paths, 1);
            let css = self.check("edit_tokens", &mut registry);
            self.report("edit_tokens", &samples, &css);
        }

        if self.wants("add_file") || self.wants("remove_file") {
            let mut rng = Rng(0x9e3779b97f4a7c15);
            let (mut added, mut removed) = (Samples::default(), Samples::default());
            let mut added_css = String::new();
            for run in 0..self.runs {
                // Sorts between existing component files.
                let after = &components[(run * 7919 + 13) % components.len()];
                let path = format!("{after}.new{run}.tsx");
                let rules: Vec<StylexRule> = (0..25)
                    .map(|_| component_rule(&mut rng, &project.tokens))
                    .collect();
                self.timed("add_file", &mut added, &mut registry, |registry| {
                    registry.set_file_rules(&path, rules)
                });
                if run + 1 == self.runs {
                    added_css = self.check("add_file", &mut registry);
                }
                self.timed("remove_file", &mut removed, &mut registry, |registry| {
                    registry.remove_file(&path);
                });
            }
            let removed_css = self.check("remove_file", &mut registry);
            self.report("add_file", &added, &added_css);
            self.report("remove_file", &removed, &removed_css);
        }

        if self.wants("const_change") && self.consts > 0 {
            // One value constant's value changes: every rule may read it.
            match project.value_constant() {
                Some((path, at)) => {
                    let base = project.rules_of(path);
                    let mut samples = Samples::default();
                    for run in 0..self.runs {
                        // A length no generated constant has, new on every run.
                        let value = format!("{}px", 1000 + run);
                        let mut rules = base.to_vec();
                        rules[at].const_val = Some(Box::new(serde_json::json!(value)));
                        self.timed("const_change", &mut samples, &mut registry, |registry| {
                            registry.set_file_rules(path, rules)
                        });
                    }
                    let css = self.check("const_change", &mut registry);
                    self.report("const_change", &samples, &css);
                }
                None => eprintln!(
                    "assemble_bench: const_change skipped: no plain value constant among {} consts",
                    self.consts
                ),
            }
        }

        if self.wants("fallback_edit") {
            // One rule with a non-finite priority keeps the whole registry off
            // the splice path.
            let mut registry = self.registry(project);
            registry.set_file_rules(
                "src/zz-unsafe.tsx",
                vec![plain("xinf", ".xinf{color:red}".into(), f64::INFINITY)],
            );
            registry.emit(&self.cfg).expect("warm-up emit");
            let samples = self.edit_and_revert(
                "fallback_edit",
                &mut registry,
                project,
                self.runs,
                components,
                7919,
            );
            let css = self.check("fallback_edit", &mut registry);
            self.report("fallback_edit", &samples, &css);
        }
    }
}

fn main() {
    let mut bench = Bench {
        rules: 20_000,
        consts: 300,
        dup: 35,
        runs: 9,
        cfg: AssembleConfig::default(),
        only: None,
    };
    let mut dump = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| panic!("{flag} takes a value"))
        };
        match flag.as_str() {
            "--rules" => bench.rules = value().parse().expect("--rules N"),
            "--consts" => bench.consts = value().parse().expect("--consts N"),
            "--dup" => bench.dup = value().parse().expect("--dup PERCENT"),
            "--runs" => bench.runs = value().parse().expect("--runs N"),
            "--scenarios" => {
                bench.only = Some(value().split(',').map(str::to_string).collect());
            }
            "--dump" => dump = true,
            "--config" => {
                let raw = serde_json::from_str(&value()).expect("--config JSON");
                bench.cfg = AssembleConfig::from_json(&raw).expect("processStylexRules config");
            }
            other => panic!("unknown flag {other}"),
        }
    }
    let project = project(bench.rules, bench.consts, bench.dup, 0x05ee_d0f5_7a1e);
    if dump {
        // The project as `fru assemble` input: one metadata tuple per line.
        for rule in project.files.iter().flat_map(|(_, rules)| rules) {
            println!("{}", rule.to_metadata_tuple());
        }
        return;
    }
    // No rules leaves no component file to edit, no runs no sample to report.
    if bench.rules == 0 || bench.runs == 0 {
        eprintln!("assemble_bench: --rules and --runs must both be at least 1");
        std::process::exit(2);
    }
    if cfg!(debug_assertions) {
        eprintln!(
            "assemble_bench: warning: built with debug assertions, where every emit also \
             cross-checks itself against the one-shot assembly; time a --release build"
        );
    }
    bench.run(&project);
}
