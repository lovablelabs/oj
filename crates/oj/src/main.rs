// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

mod build;
mod ssr_dev;
mod ssr_host;
mod start_chunks;
mod start_dev;
mod start_host;

// Linking-only for now: build.rs exports the Node-API symbols these crates
// define, so they must be part of the binary (rustc drops unused crates from
// the link line otherwise). The JS engine that calls into them lands next.
use deno_core as _;
use deno_napi as _;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "oj",
    version,
    about = "Experimental next generation frontend tooling"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Dev {
        root: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        ssr: Option<String>,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        host: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Vite's `--mode` for the dev server (default `development`).
        #[arg(long)]
        mode: Option<String>,
        /// Enable the experimental on-disk module cache (also OJ_ENABLE_CACHE=1).
        /// Off by default; warm restarts then re-serve compiled modules from disk.
        #[arg(long)]
        enable_cache: bool,
        /// Force the on-disk module cache off even if enabled (also OJ_NO_CACHE=1).
        #[arg(long)]
        no_cache: bool,
        /// Compile modules on demand instead of eagerly crawling the whole graph
        /// on boot. Opt-in: the eager crawl pre-compiles in parallel and warms
        /// HMR, which is usually the faster default; --lazy suits apps that
        /// code-split so the first route needs only a fraction of the graph.
        #[arg(long)]
        lazy: bool,
    },
    Compile {
        file: PathBuf,
        #[arg(long)]
        prod: bool,
    },
    /// Internal: run an ES module on the embedded JS engine and print its
    /// default export as JSON. Used by oj's own tests (the napi symbols the
    /// engine needs are only exported from this binary, so a cargo test
    /// binary cannot host the probe).
    #[command(name = "js-eval", hide = true)]
    JsEval {
        file: PathBuf,
        /// The engine root (bare imports resolve from its node_modules).
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Internal: run one Start one-shot script (route-tree regen, the client
    /// rebundle) to completion on a fresh embedded engine, in a process of its
    /// own, then exit. The env pairs the script's `run(env)` receives arrive
    /// as JSON on stdin (they used to be spawn env; argv would print values
    /// in `ps`). A process per run on purpose: rolldown's binding retains
    /// native memory per `build()` that neither closing the bundler nor
    /// tearing the isolate down releases — only process exit reclaims it,
    /// exactly as the retired per-run `node` spawns did.
    #[command(name = "start-script", hide = true)]
    StartScript {
        script: PathBuf,
        /// The engine root (bare imports resolve from its node_modules).
        #[arg(long)]
        root: PathBuf,
    },
    /// Internal: run one export of an oj-owned module (config extraction) on a
    /// fresh embedded engine, in a process of its own, then exit. The JSON
    /// payload arrives on stdin; the outcome envelope lands in `--result` (a
    /// file, so job code that prints cannot corrupt the channel). A process
    /// per job on purpose: a native addon the job loads (rolldown, under any
    /// vite 8 config) can crash the host when it is re-initialized after a
    /// previous engine in the same process was torn down.
    #[command(name = "engine-job", hide = true)]
    EngineJob {
        module: PathBuf,
        /// The engine root (bare imports resolve from its node_modules).
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        export: String,
        #[arg(long)]
        timeout_secs: u64,
        #[arg(long)]
        result: PathBuf,
    },
    Build {
        root: Option<PathBuf>,
        /// Output directory (default: dist). `--out` is accepted as an alias.
        #[arg(long = "outDir", alias = "out")]
        out: Option<PathBuf>,
        /// Build the given entry for server-side rendering.
        #[arg(long)]
        ssr: Option<String>,
        /// Set env mode (Vite's -m/--mode).
        #[arg(short = 'm', long)]
        mode: Option<String>,
        /// Use this vite.config instead of the one found in the root.
        #[arg(short = 'c', long)]
        config: Option<PathBuf>,
        /// Empty outDir even when it is outside the project root (Vite's --emptyOutDir).
        #[arg(long = "emptyOutDir")]
        empty_out_dir: bool,
        /// Public base path (default: /).
        #[arg(long)]
        base: Option<String>,
        /// Directory under outDir to place assets in (default: assets).
        #[arg(long = "assetsDir")]
        assets_dir: Option<String>,
        /// Static asset base64 inline threshold in bytes (default: 4096).
        #[arg(long = "assetsInlineLimit")]
        assets_inline_limit: Option<u64>,
        /// Transpile target (default: baseline-widely-available).
        #[arg(long)]
        target: Option<String>,
        /// Output source maps: true | false | inline | hidden (default: false).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        sourcemap: Option<String>,
        /// Enable/disable minification, or name the minifier (default: oxc).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        minify: Option<String>,
        /// Emit the build manifest json (optionally under this file name).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        manifest: Option<String>,
        /// Emit the ssr manifest json (optionally under this file name).
        #[arg(long = "ssrManifest", num_args = 0..=1, default_missing_value = "true")]
        ssr_manifest: Option<String>,
        /// Rebuild on changes (Vite's -w); not supported by oj yet.
        #[arg(short = 'w', long)]
        watch: bool,
        /// Vite's `--app` (builder mode). oj's build already covers every
        /// configured environment, so this is accepted as a no-op.
        #[arg(long)]
        app: bool,
    },
    Preview {
        root: Option<PathBuf>,
        /// The build output to serve (default: build.outDir). `--out` is an alias.
        #[arg(long = "outDir", alias = "out")]
        out: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        host: Option<String>,
        /// Use this vite.config instead of the one found in the root.
        #[arg(short = 'c', long)]
        config: Option<PathBuf>,
        /// Exit if the port is already in use (Vite's --strictPort).
        #[arg(long = "strictPort")]
        strict_port: bool,
        /// Open the browser on startup, optionally at a path (Vite's --open).
        #[arg(long, num_args = 0..=1, default_missing_value = "/")]
        open: Option<String>,
        /// Public base path (default: the config's `base`).
        #[arg(long)]
        base: Option<String>,
    },
}

/// Self-reap when the parent dev server dies. The one-shot child subcommands
/// (`start-script`, `engine-job`) only ever run as children of an `oj`
/// process; a SIGKILLed or crashed parent runs no drop (`kill_on_drop` never
/// fires), and an orphan would keep running its job — writing code caches and
/// reports into the app's `.oj-cache` — until the job ends, minutes later.
/// Same net as the plugin host's ppid watchdog: poll for a reparent (the ppid
/// CHANGING, not `== 1` — in a container the live parent can BE pid 1) and
/// exit. The job's output is worthless with the parent gone.
///
/// The spawner names itself in [`PARENT_PID_ENV`] so the comparison does not
/// depend on a snapshot taken here: a parent that dies while this process is
/// still loading (a debug binary on a slow CI disk takes over a second to
/// reach `main`) has already reparented it, and a snapshot would point at
/// the subreaper and never change — the child then ran its whole job, the
/// recurring ENOTEMPTY in the e2e teardowns. With the declared pid the first
/// poll catches that case before an engine boots or a code cache is written.
/// Without the variable (a hand-run child) the snapshot is the fallback.
#[cfg(unix)]
fn reap_on_parent_death() {
    let declared = std::env::var(PARENT_PID_ENV)
        .ok()
        .and_then(|v| v.parse::<libc::pid_t>().ok());
    let parent = declared.unwrap_or_else(|| unsafe { libc::getppid() });
    // Born an orphan: exit synchronously, BEFORE any work (an engine boot or a
    // code-cache write racing the poll thread is the ENOTEMPTY teardown class
    // this exists to close).
    if unsafe { libc::getppid() } != parent {
        std::process::exit(0);
    }
    std::thread::spawn(move || loop {
        if unsafe { libc::getppid() } != parent {
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
}

/// Set by every spawner of a one-shot child (`oj_server::plugins`,
/// `oj_server::preseed`, `start_dev`) to its own pid; read by
/// `reap_on_parent_death`.
#[cfg(unix)]
const PARENT_PID_ENV: &str = oj_server::plugins::PARENT_PID_ENV;
#[cfg(not(unix))]
fn reap_on_parent_death() {}

/// Raise the soft fd limit to the hard limit at startup, mirroring what the
/// process embedding this engine always had: Node bumps it in
/// `PlatformInit` (src/node.cc) and Deno's CLI in `cli/util/unix.rs`, and
/// the Node tooling the engine hosts is written against that raised limit —
/// TanStack's route generator alone opens hundreds of files concurrently,
/// while macOS hands a plain binary a soft cap of 256, so big-app boots
/// died in EMFILE storms Vite users never saw. Deno's shape exactly: an
/// infinite hard limit (macOS refuses soft = RLIM_INFINITY for NOFILE) is
/// binary-searched against the kernel's own rejections under a 1<<20
/// ceiling, a finite one is set directly, and failures leave the inherited
/// limit, as before.
fn raise_fd_limit() {
    #[cfg(unix)]
    // SAFETY: getrlimit/setrlimit with valid pointers to an owned rlimit.
    unsafe {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if 0 != libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) {
            return;
        }
        if limits.rlim_cur == libc::RLIM_INFINITY {
            return;
        }
        if limits.rlim_max == libc::RLIM_INFINITY {
            let mut min = limits.rlim_cur;
            let mut max = 1 << 20;
            while min + 1 < max {
                limits.rlim_cur = min + (max - min) / 2;
                match libc::setrlimit(libc::RLIMIT_NOFILE, &limits) {
                    0 => min = limits.rlim_cur,
                    _ => max = limits.rlim_cur,
                }
            }
            return;
        }
        if limits.rlim_cur < limits.rlim_max {
            limits.rlim_cur = limits.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limits);
        }
    }
}

/// The app root the agent detection should walk from, per subcommand: the
/// user-facing commands' parsed root (clap's own semantics, so flags can
/// never be misread as a root), and None for internal one-shot children
/// (`engine-job`, `start-script`, `js-eval`, `compile`), which always inherit
/// the parent oj's environment and must not pay a filesystem walk before the
/// orphan-reap gate.
fn agent_detection_root(command: &Command) -> Option<Option<&PathBuf>> {
    match command {
        Command::Dev { root, .. } | Command::Build { root, .. } | Command::Preview { root, .. } => {
            Some(root.as_ref())
        }
        _ => None,
    }
}

/// The app's package manager, as `npm_config_user_agent` would present it.
/// Vite prefix-matches the manager NAME, but the VERSION matters too:
/// ecosystem tools version-gate on the agent (yarn classic vs berry is
/// `version.startsWith('1.')`), so the `packageManager` field's real version
/// wins and lockfile-shape detection picks a representative one, sniffing the
/// lockfile where the major is knowable (Berry keeps a yarn.lock; pnpm 8
/// writes lockfileVersion 6). The tail is a parseable `tool/version` field
/// (`oj/<version>`), never prose, because consumers split the agent on
/// spaces. Detection walks up from the parsed app root and stops at a `.git`
/// DIRECTORY (a `.git` file is a submodule or worktree whose workspace
/// lockfile legitimately lives above) or at $HOME when the walk started below
/// it: this value is pinned into every child's environment, so a stray
/// lockfile in an unrelated ancestor (an accidental `npm i` in $HOME) must
/// never win. Best effort by design: returning None leaves the environment
/// untouched.
fn detect_package_manager_agent(root_arg: Option<&PathBuf>) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = match root_arg {
        Some(arg) => {
            let p = cwd.join(arg);
            if p.is_dir() {
                p
            } else {
                cwd.clone()
            }
        }
        // Mirror the CLI's default root: playground/ when it serves one.
        None => {
            let playground = cwd.join("playground");
            if playground.join("index.html").is_file() {
                playground
            } else {
                cwd.clone()
            }
        }
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let started_below_home = home
        .as_deref()
        .is_some_and(|h| dir.starts_with(h) && dir != h);
    loop {
        // $HOME itself is never scanned when the walk started below it: its
        // stray lockfiles are another project's (or nobody's).
        if started_below_home && home.as_deref() == Some(dir.as_path()) {
            return None;
        }
        if let Ok(pkg) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
                if let Some(pm) = v.get("packageManager").and_then(|p| p.as_str()) {
                    let mut parts = pm.splitn(2, '@');
                    let name = parts.next().unwrap_or_default();
                    // `pnpm@9.0.0+sha512...`: the hash suffix is not part of
                    // the version tools parse.
                    let version = parts
                        .next()
                        .map(|v| v.split('+').next().unwrap_or(v))
                        .filter(|v| !v.is_empty())
                        .unwrap_or("0.0.0");
                    if !name.is_empty() {
                        return Some(format!("{name}/{version} oj/{}", env!("CARGO_PKG_VERSION")));
                    }
                }
            }
        }
        // One shared manager mapping (oj_server::preseed keeps the
        // change-stamp superset next to it); the versions are representative,
        // not tracked: only the MAJOR gates anything downstream, and the two
        // knowable majors are sniffed from the lockfile itself.
        for (file, manager, version) in oj_server::PACKAGE_MANAGER_LOCKFILES {
            let path = dir.join(file);
            if path.exists() {
                let version = sniffed_manager_version(&path, manager, version);
                return Some(format!(
                    "{manager}/{version} oj/{}",
                    env!("CARGO_PKG_VERSION")
                ));
            }
        }
        // A repo boundary ends the walk: this dir was the app's repo and it
        // answered nothing; an ancestor's lockfile is another project's. A
        // `.git` FILE is a submodule or worktree, whose workspace lockfile
        // legitimately lives above -- only a `.git` directory stops.
        if dir.join(".git").is_dir() || !dir.pop() {
            return None;
        }
    }
}

/// The representative version, upgraded where the lockfile states its major:
/// Berry never dropped yarn.lock (its `__metadata:` header disambiguates
/// classic 1.x from 4.x, the split `version.startsWith('1.')` gates care
/// about), and pnpm 8 writes `lockfileVersion: '6`.
fn sniffed_manager_version(path: &std::path::Path, manager: &str, default_version: &str) -> String {
    let head = || -> Option<String> {
        use std::io::Read;
        let mut buf = vec![0u8; 4096];
        let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
        buf.truncate(n);
        Some(String::from_utf8_lossy(&buf).into_owned())
    };
    match (manager, path.file_name().and_then(|n| n.to_str())) {
        ("yarn", Some("yarn.lock")) => {
            if head().is_some_and(|h| h.contains("__metadata:")) {
                "4.0.0".to_string()
            } else {
                default_version.to_string()
            }
        }
        ("pnpm", Some("pnpm-lock.yaml" | "lock.yaml")) => {
            if head().is_some_and(|h| {
                h.contains("lockfileVersion: '6") || h.contains("lockfileVersion: 6")
            }) {
                "8.0.0".to_string()
            } else {
                default_version.to_string()
            }
        }
        _ => default_version.to_string(),
    }
}

fn main() -> anyhow::Result<()> {
    raise_fd_limit();
    // Every engine and one-shot child that loads the app's vite.config through
    // rolldown-vite's native loader inherits this: the migration warning it
    // suppresses (one line per incompatibility in the config graph, 200+ on
    // big monorepos) is advice for a Vite the app is not running: oj IS the
    // native-loader world. Set while single-threaded; user overrides win, and
    // an explicitly EMPTY value re-enables the warning (empty is falsy under
    // Vite's gate but survives this is_none check and the JS-side ??= belts).
    if std::env::var_os("VITE_CONFIG_NATIVE_IGNORE_WARNING").is_none() {
        std::env::set_var("VITE_CONFIG_NATIVE_IGNORE_WARNING", "true");
    }
    // Vite sorts its lockfile-format preference by `npm_config_user_agent`
    // (optimizer/index.ts), which every package-manager launch sets and a
    // bare binary launch does not. Agentless, the list REVERSES: a pnpm app
    // with both node_modules/.pnpm/lock.yaml (content-only hash) and a stray
    // node_modules/.package-lock.json gets the npm entry, whose hash appends
    // the patches-dir MTIME — and oj's embedded engine truncates mtimeMs to
    // whole milliseconds (deno_io::FsStat carries i64 ms) where Node keeps
    // the fraction, so the optimizer's lockfileHash never matches a
    // pnpm-launched Vite's and node_modules/.vite is re-bundled on every
    // launcher switch. Present the app's own package manager instead, the
    // way its `pnpm dev` would; a set agent (any PM wrapper) always wins.
    // Parsed before the runtime exists (still single-threaded, so set_var
    // below stays safe; --help/--version now exit without spinning it up).
    // The parsed CLI is the single source for the app root: a second argv
    // parser would drift from clap the moment a flag is added.
    let cli = Cli::parse();
    // An EMPTY agent counts as unset: Vite's lockfile-preference sort treats
    // "" exactly like a missing agent (falsy), so leaving it would keep the
    // reversal this preset exists to fix.
    let agent_unset = std::env::var_os("npm_config_user_agent")
        .map(|v| v.is_empty())
        .unwrap_or(true);
    if agent_unset {
        if let Some(root) = agent_detection_root(&cli.command) {
            if let Some(agent) = detect_package_manager_agent(root) {
                std::env::set_var("npm_config_user_agent", agent);
            }
        }
    }
    // Before any in-process engine boots (each really chdirs to its app root):
    // a restart must re-resolve relative CLI args against the directory oj was
    // launched from, not wherever a plugin host moved the process.
    oj_server::capture_startup_cwd();
    // One-shot children (`engine-job` / `start-script`) reap on parent death.
    // This must run while the process is single-threaded: the reaper reads
    // AND REMOVES the spawner's pid variable (anything the child spawns must
    // not inherit it and self-reap against the wrong ancestor), and mutating
    // environ with live threads is the POSIX getenv/setenv race.
    #[cfg(unix)]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("engine-job") | Some("start-script")
    ) {
        reap_on_parent_death();
        std::env::remove_var(PARENT_PID_ENV);
    }
    // The startup writes above are oj's only direct process-env access. Snapshot
    // the env now, still single-threaded: everything after reads `oj_env::get()`.
    oj_env::init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // One-shot engine jobs (config extraction) run in `oj engine-job`
    // children: a native addon a job loads can crash the process when it is
    // re-initialized after an earlier engine died (napi-rs before 3.10).
    if let Ok(exe) = std::env::current_exe() {
        oj_server::plugins::engine_jobs_via_subprocess(exe);
    }
    match cli.command {
        Command::Dev {
            root,
            port,
            ssr,
            host,
            config,
            mode,
            enable_cache,
            no_cache,
            lazy,
        } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() {
                    playground
                } else {
                    PathBuf::from(".")
                }
            });
            if let Some(entry) = ssr {
                ssr_dev::ssr_dev(root, entry, port, host).await
            } else if oj_server::is_tanstack_start_app(&root) {
                start_dev::start_dev(root, port, host, config, mode).await
            } else {
                oj_server::DevServer {
                    root,
                    port,
                    host,
                    config,
                    enable_cache,
                    no_cache,
                    lazy,
                    mode,
                }
                .run()
                .await
            }
        }
        Command::JsEval { file, root } => {
            let root = root
                .unwrap_or_else(|| PathBuf::from("."))
                .canonicalize()
                .context("engine root not found")?;
            let mut config = oj_js::EngineConfig::new(&root);
            config.code_cache_dir = Some(oj_server::engine_code_cache_dir(&root));
            let engine =
                oj_js::JsEngine::spawn(config, None, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            let value = engine
                .eval(oj_js::EvalInput::Path(file), None)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{value}");
            Ok(())
        }
        Command::StartScript { script, root } => {
            let root = root.canonicalize().context("engine root not found")?;
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buf)
                .context("start-script env on stdin")?;
            let env: Vec<(String, String)> =
                serde_json::from_str(&buf).context("start-script env json")?;
            let scripts = start_host::ScriptEngine::new(&root)?;
            scripts.run_async(&script, &env, "start script").await
        }
        Command::EngineJob {
            module,
            root,
            export,
            timeout_secs,
            result,
        } => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buf)
                .context("engine-job payload on stdin")?;
            let payload: serde_json::Value =
                serde_json::from_str(&buf).context("engine-job payload json")?;
            // A job failure travels inside the envelope; a nonzero exit is
            // reserved for the harness itself (or a native addon crash).
            let outcome = oj_server::plugins::run_engine_job_in_process(
                &root,
                &module,
                &export,
                payload,
                std::time::Duration::from_secs(timeout_secs),
            );
            let envelope = oj_server::plugins::engine_job_envelope(&outcome);
            std::fs::write(&result, serde_json::to_string(&envelope)?)
                .context("engine-job result write")?;
            Ok(())
        }
        Command::Compile { file, prod } => {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let opts = if prod {
                oj_compiler::CompileOptions::prod()
            } else {
                oj_compiler::CompileOptions::dev()
            };
            let output = oj_compiler::compile(&file, &source, &opts)?;
            println!("{}", output.code);
            Ok(())
        }
        Command::Build {
            root,
            out,
            ssr,
            mode,
            config,
            empty_out_dir,
            base,
            assets_dir,
            assets_inline_limit,
            target,
            sourcemap,
            minify,
            manifest,
            ssr_manifest,
            watch,
            app: _,
        } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() {
                    playground
                } else {
                    PathBuf::from(".")
                }
            });
            let config = resolve_config_arg(&root, config);
            if oj_server::is_tanstack_start_app(&root) {
                let mode = mode.unwrap_or_else(|| "production".to_string());
                start_dev::start_build(root, config, &mode, out).await
            } else {
                build::build(
                    root,
                    mode.as_deref(),
                    build::CliOptions {
                        out,
                        ssr,
                        empty_out_dir,
                        base,
                        assets_dir,
                        assets_inline_limit,
                        target,
                        sourcemap,
                        minify,
                        manifest,
                        ssr_manifest,
                        watch,
                        config,
                    },
                )
                .await
            }
        }
        Command::Preview {
            root,
            out,
            port,
            host,
            // Accepted for Vite CLI parity; nothing on the preview path reads
            // a config file (the old global was set here but never consulted).
            config: _,
            strict_port,
            open,
            base,
        } => {
            let root = root
                .unwrap_or_else(|| {
                    let playground = PathBuf::from("playground");
                    if playground.join("index.html").is_file() {
                        playground
                    } else {
                        PathBuf::from(".")
                    }
                })
                .canonicalize()
                .with_context(|| "app root not found")?;
            let config = oj_config::load(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            let out_dir = out
                .or_else(|| {
                    config
                        .build
                        .as_ref()
                        .and_then(|b| b.out_dir.as_ref())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from("dist"));
            let out_dir = if out_dir.is_absolute() {
                out_dir
            } else {
                root.join(out_dir)
            };
            let mut opts = preview_options(&config, out_dir);
            if let Some(p) = port {
                opts.port = p;
            }
            if let Some(h) = host {
                opts.host = Some(h);
            }
            if let Some(b) = base {
                opts.base = b;
            }
            if strict_port {
                opts.strict_port = true;
            }
            if let Some(o) = open {
                opts.open = Some(o);
            }
            oj_server::preview(opts).await
        }
    }
}

/// Vite's `resolvePreviewOptions`: every preview option falls back to the
/// `server` one except the port (4173), so dev and preview run side by side.
fn preview_options(config: &oj_config::OjConfig, out_dir: PathBuf) -> oj_server::PreviewOptions {
    let preview = config.preview.clone().unwrap_or_default();
    let server = config.server.clone().unwrap_or_default();
    if preview.proxy.as_ref().is_some_and(|p| !p.is_null()) || server.proxy.is_some() {
        eprintln!("oj preview: (!) preview.proxy / server.proxy is not applied by the preview server yet.");
    }
    let open = match preview.open.as_ref() {
        Some(serde_json::Value::Bool(true)) => Some("/".to_string()),
        Some(serde_json::Value::String(p)) => Some(p.clone()),
        Some(_) => None,
        None => server.open.filter(|o| *o).map(|_| "/".to_string()),
    };
    let base = config.base.clone().unwrap_or_else(|| "/".into());
    let base = if base.is_empty() || base.starts_with('.') {
        "/".to_string()
    } else {
        format!("/{}/", base.trim_matches('/')).replace("//", "/")
    };
    oj_server::PreviewOptions {
        dir: out_dir,
        port: preview.port.unwrap_or(4173),
        base,
        headers: preview
            .headers
            .or(server.headers)
            .map(|m| m.into_iter().collect())
            .unwrap_or_default(),
        host: preview.host.or(server.host),
        strict_port: preview.strict_port.or(server.strict_port).unwrap_or(false),
        open,
        cors: preview.cors.or(server.cors),
        allowed_hosts: preview.allowed_hosts.or(server.allowed_hosts),
        spa_fallback: config.app_type.as_deref().unwrap_or("spa") == "spa",
        assets_dir: oj_config::build_assets_dir(config),
    }
}

/// `--config <file>`: use that vite.config instead of the one found in the root
/// (Vite's `--config`). Relative paths resolve against the app root.
fn resolve_config_arg(root: &std::path::Path, config: Option<PathBuf>) -> Option<PathBuf> {
    config.map(|cfg| {
        if cfg.is_absolute() {
            cfg
        } else {
            root.join(cfg)
        }
    })
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn preview_options_inherit_from_server_except_port() {
        let config: oj_config::OjConfig = serde_json::from_str(
            r#"{"base":"app","appType":"mpa","build":{"assetsDir":"static"},
                "server":{"port":3000,"strictPort":true,"open":true,"host":"0.0.0.0","cors":false,"allowedHosts":["a.test"],"headers":{"x-a":"1"}},
                "preview":{"headers":{"x-b":"2"}}}"#,
        )
        .unwrap();
        let o = preview_options(&config, PathBuf::from("dist"));
        assert_eq!(o.port, 4173, "the port never inherits from server");
        assert_eq!(o.base, "/app/");
        assert!(o.strict_port);
        assert_eq!(o.open.as_deref(), Some("/"));
        assert_eq!(o.host.as_deref(), Some("0.0.0.0"));
        assert!(matches!(o.cors, Some(oj_config::CorsConfig::Toggle(false))));
        assert!(
            matches!(o.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &["a.test"])
        );
        assert_eq!(
            o.headers,
            vec![("x-b".to_string(), "2".to_string())],
            "preview.headers wins over server.headers"
        );
        assert!(!o.spa_fallback, "appType mpa has no index.html fallback");
        assert_eq!(o.assets_dir, "static");

        let config: oj_config::OjConfig =
            serde_json::from_str(r#"{"preview":{"port":5000,"open":"/docs","strictPort":false},"server":{"strictPort":true}}"#).unwrap();
        let o = preview_options(&config, PathBuf::from("dist"));
        assert_eq!(o.port, 5000);
        assert_eq!(o.open.as_deref(), Some("/docs"));
        assert!(!o.strict_port, "an explicit preview.strictPort wins");
        assert!(o.spa_fallback);
        assert_eq!(o.base, "/");
    }
}
