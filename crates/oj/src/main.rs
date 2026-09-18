// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

mod build;
mod ssr_dev;
mod ssr_host;
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
    about = "A Rust-native build tool for React apps"
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
        bundle: bool,
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

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // One-shot engine jobs (config extraction) run in `oj engine-job`
    // children: a native addon a job loads can crash the process when it is
    // re-initialized after an earlier engine died (napi-rs before 3.10).
    if let Ok(exe) = std::env::current_exe() {
        oj_server::plugins::engine_jobs_via_subprocess(exe);
    }
    match Cli::parse().command {
        Command::Dev {
            root,
            port,
            bundle,
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
                    bundle,
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
            let engine = oj_js::JsEngine::spawn(config).map_err(|e| anyhow::anyhow!("{e}"))?;
            let value = engine
                .eval(oj_js::EvalInput::Path(file))
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
            set_config_override(&root, config);
            if oj_server::is_tanstack_start_app(&root) {
                let mode = mode.unwrap_or_else(|| "production".to_string());
                start_dev::start_build(root, &mode, out).await
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
            config,
            strict_port,
            open,
            base,
        } => {
            set_config_override(&PathBuf::from("."), config);
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
        assert!(matches!(o.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &["a.test"]));
        assert_eq!(o.headers, vec![("x-b".to_string(), "2".to_string())], "preview.headers wins over server.headers");
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

/// `--config <file>`: use that vite.config instead of the one found in the root
/// (Vite's `--config`). Relative paths resolve against the app root.
fn set_config_override(root: &std::path::Path, config: Option<PathBuf>) {
    if let Some(cfg) = config {
        let cfg = if cfg.is_absolute() { cfg } else { root.join(cfg) };
        oj_server::plugins::set_vite_config_override(cfg);
    }
}
