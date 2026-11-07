use super::*;

fn app(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("vite.config.mjs"), config).unwrap();
    dir
}

// The deps pre-seed stamp survives a restart whose fresh evaluation matches
// the cached one (a plugin's server.restart(), an .env edit), and is dropped
// when the config's values moved.
#[test]
fn a_restart_drops_the_preseed_stamp_only_when_the_config_moved() {
    let dir = app(r#"export default { define: { __A__: "1" } };"#);
    let root = dir.path();
    crate::plugins::extract_vite_values(root, None, "serve", "development")
        .expect("the config extracts");
    crate::preseed::write_stamp(root, &[]);
    assert!(crate::preseed::stamp_is_warm(root));
    let restarter = Restarter::new(root.to_path_buf(), None, "development".into(), false);
    assert_eq!(restarter.failure(), None);
    assert!(
        crate::preseed::stamp_is_warm(root),
        "an unchanged config dropped the stamp"
    );
    std::fs::write(
        root.join("vite.config.mjs"),
        r#"export default { define: { __A__: "2" } };"#,
    )
    .unwrap();
    assert_eq!(restarter.failure(), None);
    assert!(
        !crate::preseed::stamp_is_warm(root),
        "a changed config kept the stamp"
    );
}
