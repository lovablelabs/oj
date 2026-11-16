// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `@vitejs/plugin-basic-ssl` in the config: oj makes the plugin's
//! certificate itself and serves TLS with it, with no plugin host.

use crate::App;

fn install_basic_ssl(app: &App) {
    let pkg = app.root.join("node_modules/@vitejs/plugin-basic-ssl");
    std::fs::create_dir_all(pkg.join("dist")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"@vitejs/plugin-basic-ssl","version":"2.3.0","type":"module","exports":{".":{"import":"./dist/index.mjs","require":"./dist/index.cjs"}}}"#,
    )
    .unwrap();
    // The package's own code never runs: the engine serves oj's stand-in.
    std::fs::write(
        pkg.join("dist/index.mjs"),
        "throw new Error(\"the real @vitejs/plugin-basic-ssl ran\");\n",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_ssl_in_the_config_serves_tls_from_a_kept_certificate_without_a_host() {
    let app = App::new(&[
        ("package.json", r#"{"name":"ssl","type":"module"}"#),
        ("index.html", r#"<script type="module" src="/src/main.js"></script>"#),
        ("src/main.js", "document.body.textContent = 'hi';\n"),
        (
            "vite.config.mjs",
            "import basicSsl from \"@vitejs/plugin-basic-ssl\";\nexport default { plugins: [basicSsl({ domains: [\"dev.local\"] })] };\n",
        ),
    ]);
    install_basic_ssl(&app);
    let server = app.serve().await;
    let https = server
        .built
        .https
        .clone()
        .expect("the plugin's certificate turns TLS on");
    assert!(
        https.key.contains("-----BEGIN PRIVATE KEY-----")
            && https.key.contains("-----BEGIN CERTIFICATE-----"),
        "key and certificate in one PEM, as the plugin keeps them"
    );
    assert_eq!(https.key, https.cert);
    assert!(
        server.built.plugin_host.is_none(),
        "the plugin is oj's: no host boots for it"
    );
    // The plugin's own cache file, under Vite's cacheDir, holds it.
    let kept = app.root.join("node_modules/.vite/basic-ssl/_cert.pem");
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), https.cert);
    // A page still serves over the router (the TLS acceptor is the
    // listener's; the router does not change).
    assert!(server.text("/").await.contains("/src/main.js"));
    drop(server);

    // The next boot serves the kept certificate, not a new one.
    let again = app.serve().await;
    assert_eq!(
        again.built.https.as_ref().map(|h| &h.cert),
        Some(&https.cert)
    );
}
