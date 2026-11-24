//! Multi-file app benchmark for the plugin's filesystem layer: N components,
//! each importing one shared `tokens.stylex.ts` and one shared
//! `consts.stylex.const.ts`, compiled through [`StylexPlugin`] in build mode
//! (`live_fs = false`) and dev mode (`live_fs = true`).
//!
//! ```text
//! cargo run --release -p oj_stylex --example plugin_fs_bench -- \
//!     [--components 1000] [--runs 9] [--threads 1] [--layout nested|flat] \
//!     [--imports relative|alias|package] [--mode all|build|dev] \
//!     [--dir <fixture parent>] [--settle-ms 2100]
//! ```
//!
//! Prints the median and the minimum over `--runs` of: a build with a fresh
//! plugin, a dev server's first pass over every module (fresh plugin), and a
//! dev server's later pass over every module (the same plugin, as on a reload
//! or an edit). On Linux a single-threaded run also prints the time the
//! thread spent on a CPU, which a busy machine inflates far less than the
//! wall clock.
//!
//! The dev passes start `--settle-ms` after the fixture is written: a live fs
//! takes a directory's timestamp at its word only once it has stood a while,
//! as a project's directories have by the time anyone compiles in them. The
//! fixture's parent (`--dir`, by default the temp dir) counts as well, and a
//! temp dir other processes keep writing to never comes to rest.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oj_stylex::plugin::StylexPlugin;
use serde_json::json;

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// Every component in a directory of its own, five levels under the root.
    Nested,
    /// Fifty components per directory.
    Flat,
}

#[derive(Clone, Copy, PartialEq)]
enum Imports {
    Relative,
    /// `@tokens/*`, an alias onto `src/theme/*`.
    Alias,
    /// `@acme/design/*`, a package under `node_modules` with an exports map.
    Package,
}

struct Config {
    components: usize,
    runs: usize,
    threads: usize,
    layout: Layout,
    imports: Imports,
    mode: String,
    dir: PathBuf,
    settle: Duration,
}

fn parse_args() -> Config {
    let mut config = Config {
        components: 1000,
        runs: 9,
        threads: 1,
        layout: Layout::Nested,
        imports: Imports::Relative,
        mode: "all".to_string(),
        dir: std::env::temp_dir(),
        settle: Duration::from_millis(2100),
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--components" => config.components = value.parse().expect("--components"),
            "--runs" => config.runs = value.parse().expect("--runs"),
            "--threads" => config.threads = value.parse().expect("--threads"),
            "--layout" => {
                config.layout = match value.as_str() {
                    "nested" => Layout::Nested,
                    "flat" => Layout::Flat,
                    other => panic!("unknown layout {other}"),
                }
            }
            "--imports" => {
                config.imports = match value.as_str() {
                    "relative" => Imports::Relative,
                    "alias" => Imports::Alias,
                    "package" => Imports::Package,
                    other => panic!("unknown imports {other}"),
                }
            }
            "--mode" => config.mode = value,
            "--dir" => config.dir = PathBuf::from(value),
            "--settle-ms" => {
                config.settle = Duration::from_millis(value.parse().expect("--settle-ms"));
            }
            other => panic!("unknown flag {other}"),
        }
    }
    config
}

const TOKENS: &str = "import * as stylex from '@stylexjs/stylex';\n\
export const colors = stylex.defineVars({\n  text: '#111', bg: '#fff', accent: 'rebeccapurple', border: '#ddd',\n});\n\
export const spacing = stylex.defineVars({\n  xs: '2px', sm: '4px', md: '8px', lg: '16px',\n});\n";

const CONSTS: &str = "import * as stylex from '@stylexjs/stylex';\n\
export const radii = stylex.defineConsts({ sm: '2px', md: '4px', lg: '8px' });\n\
export const sizes = stylex.defineConsts({ icon: '16px', row: '32px' });\n";

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn component_source(i: usize, tokens: &str, consts: &str) -> String {
    format!(
        "import * as stylex from '@stylexjs/stylex';\n\
import {{ colors, spacing }} from '{tokens}';\n\
import {{ radii, sizes }} from '{consts}';\n\
\n\
const styles = stylex.create({{\n\
  root: {{\n\
    color: colors.text,\n\
    backgroundColor: colors.bg,\n\
    padding: spacing.md,\n\
    borderRadius: radii.md,\n\
    minHeight: sizes.row,\n\
    marginTop: '{margin}px',\n\
  }},\n\
  accent: {{\n\
    color: colors.accent,\n\
    borderColor: colors.border,\n\
    gap: spacing.sm,\n\
    width: sizes.icon,\n\
  }},\n\
}});\n\
\n\
export function Comp{i}(props: {{ accent?: boolean }}) {{\n\
  return <div {{...stylex.props(styles.root, props.accent && styles.accent)}} />;\n\
}}\n",
        margin = i % 97,
    )
}

/// The fixture app: its root and every component's `(path, source)`.
fn generate(config: &Config) -> (PathBuf, Vec<(PathBuf, String)>) {
    let root = config
        .dir
        .join(format!("oj-stylex-fs-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write(
        &root.join("package.json"),
        r#"{"name":"bench-app","dependencies":{"@acme/design":"1.0.0"}}"#,
    );
    write(&root.join("src/theme/tokens.stylex.ts"), TOKENS);
    write(&root.join("src/theme/consts.stylex.const.ts"), CONSTS);
    write(
        &root.join("node_modules/@acme/design/package.json"),
        r#"{"name":"@acme/design","exports":{"./*.stylex":"./dist/*.stylex.js","./*.stylex.const":"./dist/*.stylex.const.js"}}"#,
    );
    write(
        &root.join("node_modules/@acme/design/dist/tokens.stylex.js"),
        TOKENS,
    );
    write(
        &root.join("node_modules/@acme/design/dist/consts.stylex.const.js"),
        CONSTS,
    );
    let root = std::fs::canonicalize(&root).unwrap();
    let mut files = Vec::with_capacity(config.components);
    for i in 0..config.components {
        let (dir, up) = match config.layout {
            Layout::Nested => (
                format!("src/features/f{}/components/c{i}", i / 50),
                "../../../..",
            ),
            Layout::Flat => (format!("src/features/f{}", i / 50), "../.."),
        };
        let (tokens, consts) = match config.imports {
            Imports::Relative => (
                format!("{up}/theme/tokens.stylex"),
                format!("{up}/theme/consts.stylex.const"),
            ),
            Imports::Alias => (
                "@tokens/tokens.stylex".to_string(),
                "@tokens/consts.stylex.const".to_string(),
            ),
            Imports::Package => (
                "@acme/design/tokens.stylex".to_string(),
                "@acme/design/consts.stylex.const".to_string(),
            ),
        };
        let path = root.join(&dir).join(format!("Comp{i}.tsx"));
        let source = component_source(i, &tokens, &consts);
        write(&path, &source);
        files.push((path, source));
    }
    (root, files)
}

fn plugin(root: &Path, live_fs: bool, node_env_dev: bool) -> StylexPlugin {
    let options = json!({
        "aliases": { "@tokens/*": [root.join("src/theme/*").to_string_lossy()] },
    });
    StylexPlugin::new(&options, root, node_env_dev, live_fs).unwrap()
}

/// One pass over every module; the number of rules recorded.
fn pass(plugin: &StylexPlugin, files: &[(PathBuf, String)], threads: usize) -> usize {
    let compile = |(path, source): &(PathBuf, String)| {
        let out = plugin
            .transform_source(path, source)
            .unwrap()
            .expect("every component compiles");
        let rules = out.rules.len();
        plugin.set_module_rules(&path.to_string_lossy(), out.rules);
        rules
    };
    if threads <= 1 {
        return files.iter().map(compile).sum();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut rules = 0;
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(file) = files.get(i) else { break };
                        rules += compile(file);
                    }
                    rules
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).sum()
    })
}

/// Wall clock and, where the kernel reports it, on-CPU time of this thread.
struct Stopwatch {
    wall: Instant,
    cpu: Option<Duration>,
}

fn on_cpu() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let nanos = stat.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

impl Stopwatch {
    fn start() -> Self {
        Self {
            cpu: on_cpu(),
            wall: Instant::now(),
        }
    }

    fn stop(self) -> (Duration, Option<Duration>) {
        let wall = self.wall.elapsed();
        let cpu = self.cpu.zip(on_cpu()).map(|(from, to)| to - from);
        (wall, cpu)
    }
}

#[derive(Default)]
struct Samples {
    wall: Vec<Duration>,
    cpu: Vec<Duration>,
}

impl Samples {
    fn push(&mut self, (wall, cpu): (Duration, Option<Duration>)) {
        self.wall.push(wall);
        self.cpu.extend(cpu);
    }

    fn report(mut self, label: &str, modules: usize, threads: usize) {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        self.wall.sort_unstable();
        let median = self.wall[self.wall.len() / 2];
        print!(
            "{label:<30} wall median {:>8.3} ms  min {:>8.3} ms  ({:>6.2} us/module)",
            ms(median),
            ms(self.wall[0]),
            median.as_secs_f64() * 1e6 / modules as f64,
        );
        if threads <= 1 && self.cpu.len() == self.wall.len() {
            self.cpu.sort_unstable();
            print!(
                "  cpu median {:>8.3} ms  min {:>8.3} ms",
                ms(self.cpu[self.cpu.len() / 2]),
                ms(self.cpu[0]),
            );
        }
        println!("  [{} runs]", self.wall.len());
    }
}

fn main() {
    let config = parse_args();
    let (root, files) = generate(&config);
    let modules = files.len();
    let want = |mode: &str| config.mode == "all" || config.mode == mode;

    // One unmeasured pass: page cache, dentry cache, allocator.
    let warmup = plugin(&root, false, false);
    let rules = pass(&warmup, &files, config.threads);
    let sheet = warmup.stylesheet().unwrap().unwrap_or_default();
    println!(
        "{modules} modules, {rules} rules, sheet {} bytes, threads {}",
        sheet.len(),
        config.threads
    );
    drop(warmup);

    if want("build") {
        let mut samples = Samples::default();
        for _ in 0..config.runs {
            let plugin = plugin(&root, false, false);
            let watch = Stopwatch::start();
            let got = pass(&plugin, &files, config.threads);
            samples.push(watch.stop());
            assert_eq!(got, rules);
        }
        samples.report("build (live_fs=false)", modules, config.threads);
    }
    if want("dev") {
        std::thread::sleep(config.settle);
        let (mut first, mut later) = (Samples::default(), Samples::default());
        for _ in 0..config.runs {
            let plugin = plugin(&root, true, true);
            let watch = Stopwatch::start();
            let got = pass(&plugin, &files, config.threads);
            first.push(watch.stop());
            assert_eq!(got, rules);
            let watch = Stopwatch::start();
            let got = pass(&plugin, &files, config.threads);
            later.push(watch.stop());
            assert_eq!(got, rules);
        }
        first.report("dev first pass (live_fs=true)", modules, config.threads);
        later.report("dev later pass (live_fs=true)", modules, config.threads);
    }
    let _ = std::fs::remove_dir_all(&root);
}
