//! `oj dev --config <file>` adopts values only Vite knows from that file:
//! `envPrefix`, `resolve.extensions`, and Sass `additionalData` reach the dev
//! compiler. The build half (config-file `mode` as default, `--mode` winning,
//! outDir) is crates/oj/src/build/plugin_api_tests/build_config_flag_build.rs.

use crate::{serve, App, INDEX_HTML};

const ALT_CONFIG: &str = r#"export default ({ mode }) => ({
  mode: "staging",
  envPrefix: ["VITE_", "APP_"],
  resolve: { extensions: [".custom.js", ".js"] },
  css: { preprocessorOptions: { scss: { additionalData: "$brand: rgb(1, 2, 3);" } } },
  build: { outDir: "alt-out-" + mode },
});
"#;

#[tokio::test(flavor = "multi_thread")]
async fn dev_config_flag_applies_vite_only_values() {
    let app = App::new(&[
        ("package.json", r#"{"name":"cfgflag","version":"1.0.0"}"#),
        (".env", "APP_TOKEN=from-env\nVITE_SEEN=vite\n"),
        (".env.staging", "APP_TOKEN=staging-token\n"),
        ("src/styles.scss", ".box { color: $brand; }\n"),
        ("src/util.custom.js", "export const U = \"custom-ext\";\n"),
        (
            "src/main.js",
            "import { U } from \"./util\";\nwindow.__MODE = import.meta.env.MODE; window.__TOKEN = import.meta.env.APP_TOKEN; window.__U = U;\n",
        ),
        ("index.html", INDEX_HTML),
        // A decoy the --config must override.
        ("vite.config.js", "export default { build: { outDir: \"decoy-out\" } };\n"),
        ("configs/vite.alt.js", ALT_CONFIG),
    ]);
    let mut dev = app.dev();
    dev.config = Some("configs/vite.alt.js".into());
    let server = serve(dev).await;
    server.text("/").await;

    let main = server.text("/src/main.js").await;
    // from-env, not staging-token: dev keeps the default mode even though
    // the config file names `mode: "staging"`.
    assert!(
        main.contains("from-env"),
        "dev honors the APP_ envPrefix from vite.config:\n{main}"
    );
    assert!(
        main.contains("util.custom.js"),
        "dev honors resolve.extensions from vite.config:\n{main}"
    );

    let css = server.text("/src/styles.scss?import").await;
    let re = regex::Regex::new(r"rgb\(1,\s*2,\s*3\)|#010203").unwrap();
    assert!(
        re.is_match(&css),
        "scss additionalData from vite.config applied in dev:\n{css}"
    );
}
