// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build --mode`, as Vite resolves it: the mode picks `.env.<mode>` and
//! sets `import.meta.env.MODE`; the build is production (PROD, NODE_ENV
//! `production`) unless the mode's `.env` file sets `NODE_ENV=development`,
//! Vite's documented way to get a development build; a mode whose file sets
//! no NODE_ENV stays production; and the shell's NODE_ENV beats the file.
//! The shell case is pinned on the env resolution the build uses
//! (`build_app_env`), since a test cannot set the process env snapshot.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::build::{build, CliOptions};

use super::browser::Page;
use super::{preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize)]
struct Seen {
    mode: String,
    flavor: String,
    dev: bool,
    prod: bool,
    #[serde(rename = "nodeEnv")]
    node_env: String,
}

fn app(name: &str) -> std::path::PathBuf {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"mode-app","version":"1.0.0"}"#),
            (".env.production", "VITE_FLAVOR=prod-flavor\n"),
            (".env.development", "VITE_FLAVOR=dev-flavor\nNODE_ENV=development\n"),
            (".env.staging", "VITE_FLAVOR=staging-flavor\n"),
            (
                "src/main.js",
                "window.__MODE = import.meta.env.MODE;\nwindow.__FLAVOR = import.meta.env.VITE_FLAVOR;\n\
                 window.__DEV = import.meta.env.DEV;\nwindow.__PROD = import.meta.env.PROD;\n\
                 window.__NODE_ENV = process.env.NODE_ENV;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    root
}

/// Builds `root` in `mode` and reads what the built page sees.
async fn built(root: &Path, mode: Option<&str>) -> Seen {
    build(root.to_path_buf(), mode, CliOptions::default())
        .await
        .unwrap_or_else(|e| panic!("build --mode {mode:?} failed: {e:#}"));
    let server = preview(&root.join("dist"), "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__MODE !== undefined", WAIT).await;
    let seen: Seen = page
        .eval(
            "({ mode: window.__MODE, flavor: window.__FLAVOR, dev: window.__DEV, \
             prod: window.__PROD, nodeEnv: window.__NODE_ENV })",
        )
        .await;
    page.close().await;
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn build_mode_picks_env_file_and_node_env() {
    if super::browser::skip() {
        return;
    }
    // A NODE_ENV in this process's env would decide every case below.
    if oj_env::get().knobs.node_env.is_some() {
        eprintln!("skipping: NODE_ENV is set in the test environment");
        return;
    }
    let dev = built(&app("build-mode-development"), Some("development")).await;
    assert_eq!(
        dev.mode, "development",
        "--mode development sets import.meta.env.MODE"
    );
    assert_eq!(
        dev.flavor, "dev-flavor",
        "--mode development loads .env.development"
    );
    assert!(
        dev.dev,
        ".env.development NODE_ENV=development makes DEV true"
    );
    assert!(!dev.prod, "and PROD false");
    assert_eq!(
        dev.node_env, "development",
        "and process.env.NODE_ENV development"
    );

    let prod = built(&app("build-mode-default"), None).await;
    assert_eq!(prod.mode, "production", "default build mode is production");
    assert_eq!(
        prod.flavor, "prod-flavor",
        "default build loads .env.production"
    );
    assert!(!prod.dev, "default build is not DEV");
    assert!(prod.prod, "default build is PROD");
    assert_eq!(
        prod.node_env, "production",
        "default build NODE_ENV production"
    );

    let staging = built(&app("build-mode-staging"), Some("staging")).await;
    assert_eq!(staging.mode, "staging", "--mode staging sets MODE");
    assert_eq!(
        staging.flavor, "staging-flavor",
        "--mode staging loads .env.staging"
    );
    assert!(!staging.dev, "no NODE_ENV in .env.staging: PROD build");
    assert_eq!(staging.node_env, "production");
}

/// The shell's NODE_ENV=production beats `.env.development`'s
/// NODE_ENV=development, with MODE still following `--mode`. Pinned on the
/// resolution `build_app_env` runs, fed a shell env of its own.
#[test]
fn shell_node_env_beats_the_env_file() {
    let root = app("build-mode-shell-node-env");
    let shell = oj_env::Env::from_vars([("NODE_ENV", "production")]);
    let env = oj_env::AppEnv::resolve(
        &shell,
        &root,
        "development",
        vec!["VITE_".to_string()],
        "production",
    );
    assert_eq!(env.mode(), "development", "MODE still follows --mode");
    assert!(
        env.is_production(),
        "shell NODE_ENV=production beats .env.development"
    );
    assert_eq!(env.node_env(), "production");
}
