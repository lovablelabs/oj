// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::Path;

use super::{build_app, scratch, write_files};

/// An app whose entry imports a bare specifier nothing can resolve.
fn unresolved_app(root: &Path) {
    write_files(
        root,
        &[
            ("package.json", r#"{"type":"module"}"#),
            (
                "index.html",
                r#"<html><body><script type="module" src="/main.js"></script></body></html>"#,
            ),
            (
                "main.js",
                r#"import "missing-example-dependency"; document.body.textContent = "ready";"#,
            ),
        ],
    );
}

/// Vite fails the build on an unresolved import (rolldown only warns
/// UNRESOLVED_IMPORT), names the specifier, and points at
/// `build.rollupOptions.external` as the escape hatch for a deliberate
/// external. Vite writes nothing on failure, so oj must not leave the partial
/// bundle rolldown already wrote in dist/.
#[tokio::test]
async fn an_unresolved_import_fails_the_build_names_external_and_leaves_no_dist() {
    let root = scratch("unresolved-missing");
    unresolved_app(&root);
    let err = build_app(&root)
        .await
        .expect_err("an unresolved dependency must fail the build");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing-example-dependency"),
        "the failure names the unresolved specifier:\n{msg}"
    );
    assert!(
        msg.contains("build.rollupOptions.external"),
        "the failure must point at the externalization option:\n{msg}"
    );
    assert!(
        !root.join("dist").exists(),
        "a failed build must not leave a partial dist/ behind"
    );
}

/// An outDir outside the project root was never oj's to empty (Vite's
/// emptyOutDir rule), so the failed-build cleanup must not start clearing it.
#[tokio::test]
async fn a_failed_build_never_wipes_an_out_dir_outside_the_root() {
    let root = scratch("unresolved-outdir-root");
    let outside = scratch("unresolved-outdir-outside");
    unresolved_app(&root);
    fs::write(outside.join("keep.txt"), "not yours to delete").unwrap();
    let config = serde_json::json!({ "build": { "outDir": outside.display().to_string() } });
    fs::write(root.join("oj.config.json"), config.to_string()).unwrap();

    build_app(&root)
        .await
        .expect_err("the unresolved import must still fail the build with an outside outDir");
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).ok().as_deref(),
        Some("not yours to delete"),
        "a failed build must not wipe an outDir outside the project root"
    );
}

/// A specifier listed in `build.rollupOptions.external` is a deliberate
/// external, so the same import builds fine.
#[tokio::test]
async fn an_explicitly_external_unresolved_import_still_builds() {
    let root = scratch("unresolved-external");
    unresolved_app(&root);
    write_files(
        &root,
        &[(
            "oj.config.json",
            r#"{"build":{"rollupOptions":{"external":["missing-example-dependency"]}}}"#,
        )],
    );
    if let Err(err) = build_app(&root).await {
        panic!("an explicitly external dependency must remain allowed:\n{err:#}");
    }
}
