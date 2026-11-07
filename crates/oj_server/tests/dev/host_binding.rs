//! The dev server's bind address: loopback by default, all interfaces for a
//! bare `--host` (the CLI passes "true") and for `server.host: "::"`, which
//! oj normalizes to the dual-stack wildcard like Vite. e2e/host-binding.mjs
//! keeps the LAN reachability probes and the `oj preview` cases.

use std::net::{IpAddr, Ipv4Addr};

use crate::{serve, App, INDEX_HTML};

const ALL: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"host-app","version":"1.0.0"}"#),
        ("src/main.js", "window.__READY = true;\n"),
        ("index.html", INDEX_HTML),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn default_dev_binds_loopback() {
    let app = app();
    let server = app.serve().await;
    assert_eq!(server.built.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
}

#[tokio::test(flavor = "multi_thread")]
async fn host_flag_binds_all_interfaces() {
    let app = app();
    let mut dev = app.dev();
    dev.host = Some("true".into());
    let server = serve(dev).await;
    assert_eq!(server.built.host, ALL, "dev --host binds all interfaces");
}

#[tokio::test(flavor = "multi_thread")]
async fn config_host_ipv6_wildcard_binds_all_interfaces() {
    let app = app();
    app.write(&[(
        "oj.config.js",
        "export default { server: { host: \"::\" } };\n",
    )]);
    let server = app.serve().await;
    assert_eq!(
        server.built.host, ALL,
        "server.host \"::\" binds all interfaces (dual-stack normalized)"
    );
}
