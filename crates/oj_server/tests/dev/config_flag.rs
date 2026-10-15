//! `oj dev --config <file>` loads plugins from that file instead of the root
//! vite.config, and a `--config` naming a missing file fails the boot (Vite:
//! "failed to load config from ..."), never a silent fallback to the root.

use crate::{serve, App};

const ROOT_CONFIG: &str = "export default { plugins: [] };\n";
const OVERRIDE_CONFIG: &str = r#"import base from "../vite.config.mjs";
export default {
  ...base,
  plugins: [
    ...(base.plugins ?? []),
    { name: "ovr", transformIndexHtml(h) { return h.replace("</head>", '<script>window.__OVR = 1;</script></head>'); } },
  ],
};
"#;
const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>"#;

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"cfg-app","version":"1.0.0"}"#),
        ("vite.config.mjs", ROOT_CONFIG),
        (".config/vite.config.mjs", OVERRIDE_CONFIG),
        ("src.js", "window.__READY = true;\n"),
        ("index.html", INDEX),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn root_config_has_no_override_plugin() {
    let app = app();
    let server = app.serve().await;
    let html = server.text("/").await;
    assert!(
        !html.contains("window.__OVR = 1"),
        "root config has no override plugin:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn config_flag_loads_plugins_from_the_named_file() {
    let app = app();
    let mut dev = app.dev();
    dev.config = Some(".config/vite.config.mjs".into());
    let server = serve(dev).await;
    let html = server.text("/").await;
    assert!(
        html.contains("window.__OVR = 1"),
        "--config loads plugins from the override config:\n{html}"
    );
}

// The dev boot shares the verdict with `oj build` (no_values_verdict), so
// the dev side is pinned here and the build side belongs with
// crates/oj/src/build/plugin_api_tests/.
#[tokio::test(flavor = "multi_thread")]
async fn missing_config_flag_fails_the_boot() {
    let app = app();
    let mut dev = app.dev();
    dev.config = Some(".config/nope.config.mjs".into());
    let err = match dev.build_app().await {
        Ok(_) => panic!("dev with a missing --config must fail"),
        Err(err) => format!("{err:#}"),
    };
    let re = regex::Regex::new(r"failed to load config from .*nope\.config\.mjs").unwrap();
    assert!(re.is_match(&err), "missing --config error:\n{err}");
}
