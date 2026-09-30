use super::*;

fn tmp(label: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("oj-srv-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn write_start_assets_writes_every_module_the_start_host_aliases() {
    let dir = tmp("assets");
    write_start_assets(&dir).unwrap();
    for name in [
        "injected-head-scripts.ts",
        "manifest-dev.ts",
        "server-entry.tsx",
        "start-entry.ts",
        "cf-server.mjs",
        "cf-workers.mjs",
    ] {
        assert!(dir.join(name).is_file(), "{name} was not written");
    }
}

#[test]
fn is_tanstack_start_app_requires_routes_and_dep() {
    let base = tmp("ts");
    let app = base.join("app");
    std::fs::create_dir_all(app.join("src").join("routes")).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies":{"react":"19"}}"#,
    )
    .unwrap();
    assert!(!is_tanstack_start_app(&app));
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
    )
    .unwrap();
    assert!(is_tanstack_start_app(&app));
    let app2 = base.join("app2");
    std::fs::create_dir_all(app2.join("src")).unwrap();
    std::fs::write(
        app2.join("package.json"),
        r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
    )
    .unwrap();
    assert!(!is_tanstack_start_app(&app2));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_ssr_module_endpoint_only_reads_the_project_and_its_dependencies() {
    let base = tmp("ssr-allow");
    let root = base.join("app");
    let outside = base.join("elsewhere");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/react")).unwrap();
    std::fs::create_dir_all(outside.join("secrets")).unwrap();
    std::fs::create_dir_all(base.join("linked/node_modules/dep")).unwrap();
    std::fs::create_dir_all(base.join("allowed")).unwrap();
    std::fs::write(root.join("src/App.tsx"), "x").unwrap();
    std::fs::write(root.join("node_modules/react/index.js"), "x").unwrap();
    std::fs::write(outside.join("secrets/id_rsa"), "x").unwrap();
    std::fs::write(base.join("linked/node_modules/dep/index.js"), "x").unwrap();
    std::fs::write(base.join("allowed/shared.ts"), "x").unwrap();

    let mut allow = std::collections::HashSet::new();
    allow.insert(base.join("allowed"));

    // The project, its dependencies, and what `server.fs.allow` named.
    for ok in [
        root.join("src/App.tsx"),
        root.join("node_modules/react/index.js"),
        base.join("linked/node_modules/dep/index.js"),
        base.join("allowed/shared.ts"),
    ] {
        assert!(module_read_allowed(&root, &allow, &ok), "{ok:?} denied");
    }

    // Everything else, however it is spelled.
    for denied in [
        outside.join("secrets/id_rsa"),
        root.join("../elsewhere/secrets/id_rsa"),
        root.join("src/../../elsewhere/secrets/id_rsa"),
    ] {
        assert!(
            !module_read_allowed(&root, &allow, &denied),
            "{denied:?} allowed"
        );
    }

    // A virtual id is not a filesystem read: the plugin host is the only
    // thing that can resolve it, so it passes through.
    for virtual_id in ["virtual:oj-routes", "\0virtual:x", "plugin:generated"] {
        assert!(module_read_allowed(&root, &allow, Path::new(virtual_id)));
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_project_does_not_widen_what_can_be_read() {
    let base = tmp("ssr-symlink");
    let root = base.join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(base.join("secrets")).unwrap();
    std::fs::write(base.join("secrets/id_rsa"), "x").unwrap();
    let link = root.join("src/escape.ts");
    if std::os::unix::fs::symlink(base.join("secrets/id_rsa"), &link).is_ok() {
        assert!(
            !module_read_allowed(&root, &std::collections::HashSet::new(), &link),
            "a symlink inside the project must not expose its target"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn locate_prefers_root_then_public_dir() {
    let base = tmp("locate");
    let root = base.join("root");
    let public = base.join("shared-public");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(public.join("img")).unwrap();
    std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();
    std::fs::write(public.join("img").join("logo.webp"), "y").unwrap();

    assert_eq!(
        locate(&root, Some(&public), "src/App"),
        Some(root.join("src/App.tsx"))
    );
    assert_eq!(
        locate(&root, Some(&public), "img/logo.webp"),
        Some(public.join("img/logo.webp"))
    );
    assert_eq!(locate(&root, Some(&public), "img/missing.webp"), None);
    assert_eq!(locate(&root, Some(&public), "../secret"), None);
    // `publicDir: false`: only the root is searched.
    assert_eq!(locate(&root, None, "img/logo.webp"), None);
    assert_eq!(
        locate(&root, None, "src/App"),
        Some(root.join("src/App.tsx"))
    );
    let _ = std::fs::remove_dir_all(&base);
}

// --- adverse: request paths are attacker-shaped strings ---

#[test]
fn urldecode_handles_every_malformed_escape() {
    assert_eq!(urldecode("plain/path.tsx"), "plain/path.tsx");
    assert_eq!(urldecode("a%20b"), "a b");
    assert_eq!(urldecode("a%2Fb"), "a/b");
    assert_eq!(urldecode("caf%C3%A9.css"), "café.css");
    assert_eq!(urldecode("100%25.css"), "100%.css");
    // Truncated and non-hex escapes are left alone rather than dropped.
    assert_eq!(urldecode("a%"), "a%");
    assert_eq!(urldecode("a%2"), "a%2");
    assert_eq!(urldecode("a%zz"), "a%zz");
    assert_eq!(urldecode("a%%20"), "a% ");
    assert_eq!(urldecode(""), "");
    // A single pass only: an encoded escape stays encoded once decoded.
    assert_eq!(urldecode("a%2520b"), "a%20b");
    // Invalid UTF-8 becomes replacement characters instead of panicking.
    assert!(!urldecode("%ff%fe").is_empty());
}

#[test]
fn locate_rejects_traversal_in_every_spelling_after_decoding() {
    let base = tmp("traversal");
    let root = base.join("root");
    let public = base.join("public");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&public).unwrap();
    std::fs::write(base.join("secret.txt"), "s3cret").unwrap();
    std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();

    for hostile in [
        "../secret.txt",
        "src/../../secret.txt",
        "./../secret.txt",
        "src/../../../etc/passwd",
    ] {
        assert_eq!(locate(&root, Some(&public), hostile), None, "{hostile}");
        // ...and through the decoding the request handler applies first.
        let encoded = hostile.replace("..", "%2e%2e");
        assert_eq!(
            locate(&root, Some(&public), &urldecode(&encoded)),
            None,
            "{encoded}"
        );
    }
    // A name that merely contains dots is not traversal.
    std::fs::write(root.join("src").join("..dotted.tsx"), "x").unwrap();
    assert!(locate(&root, Some(&public), "src/..dotted.tsx").is_some());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn locate_finds_files_whose_names_need_encoding() {
    let base = tmp("encoded-names");
    let root = base.join("root");
    let public = base.join("public");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&public).unwrap();
    for name in ["Cool Button.tsx", "café.css", "100%.css", "a+b.tsx"] {
        std::fs::write(root.join("src").join(name), "x").unwrap();
    }

    // What a browser actually requests for each of those files.
    for (requested, name) in [
        ("src/Cool%20Button.tsx", "Cool Button.tsx"),
        ("src/caf%C3%A9.css", "café.css"),
        ("src/100%25.css", "100%.css"),
        ("src/a+b.tsx", "a+b.tsx"),
    ] {
        let decoded = urldecode(requested);
        assert_eq!(
            locate(&root, Some(&public), &decoded),
            Some(root.join("src").join(name)),
            "{requested}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn preview_rel_decodes_before_guarding_traversal() {
    assert_eq!(
        preview_rel("/assets/Cool%20Button.js", "/").as_deref(),
        Some("assets/Cool Button.js")
    );
    assert_eq!(preview_rel("/%2e%2e/etc/passwd", "/"), None);
    assert_eq!(preview_rel("/a/%2e%2e/%2e%2e/etc/passwd", "/"), None);
    assert_eq!(preview_rel("/%2E%2E/etc/passwd", "/"), None);
    // Not traversal: `..` inside a segment.
    assert_eq!(preview_rel("/a..b/x.js", "/").as_deref(), Some("a..b/x.js"));
}

#[test]
fn html_entry_src_rejects_everything_that_is_not_a_local_path() {
    for external in [
        "",
        "   ",
        "http://cdn.example/x.js",
        "https://cdn.example/x.js",
        "//cdn.example/x.js",
        "data:text/javascript,alert(1)",
    ] {
        assert_eq!(html_entry_src(external), None, "{external:?}");
    }
    assert_eq!(
        html_entry_src("./src/main.tsx").as_deref(),
        Some("/src/main.tsx")
    );
    assert_eq!(
        html_entry_src("src/main.tsx").as_deref(),
        Some("/src/main.tsx")
    );
    assert_eq!(
        html_entry_src("/src/main.tsx").as_deref(),
        Some("/src/main.tsx")
    );
    assert_eq!(
        html_entry_src("  src/main.tsx  ").as_deref(),
        Some("/src/main.tsx")
    );
    // A traversal in an entry src stays a path; `locate` is what refuses it.
    assert_eq!(
        html_entry_src("../../etc/passwd").as_deref(),
        Some("/../../etc/passwd")
    );
}

#[test]
fn gate_relevance_ignores_generated_directories() {
    assert!(gate_relevant(Path::new("/app/src/App.tsx")));
    assert!(!gate_relevant(Path::new(
        "/app/node_modules/react/index.js"
    )));
    assert!(!gate_relevant(Path::new("/app/.oj-cache/ab/cd.json")));
    assert!(!gate_relevant(Path::new("/app/dist/assets/x.js")));
    // Only a whole path component counts.
    assert!(gate_relevant(Path::new("/app/src/dist-helper.ts")));
    assert!(gate_relevant(Path::new(
        "/app/src/my-node_modules-thing.ts"
    )));
}

#[test]
fn query_classification_only_matches_whole_flags() {
    assert!(is_worker_query("/w.ts?worker"));
    assert!(is_worker_query("/w.ts?sharedworker"));
    assert!(!is_worker_query("/w.ts?workerish"));
    assert!(!is_worker_query("/w.ts?x=worker"));
    assert!(!is_worker_query("/w.ts"));
    assert!(!is_worker_query(""));
}

#[test]
fn worker_beats_inline_in_asset_kind_classification() {
    // ?worker&inline must classify as a worker (inline is a modifier), not as
    // a generic inline asset that would 404 as a base64 data URI of the file.
    assert_eq!(query_asset_kind(Some("worker&inline")), Some("worker"));
    assert_eq!(
        query_asset_kind(Some("sharedworker&inline")),
        Some("sharedworker")
    );
    assert_eq!(query_asset_kind(Some("inline")), Some("inline"));
    assert_eq!(query_asset_kind(Some("worker")), Some("worker"));
}

#[test]
fn is_spa_navigation_rules() {
    let empty = HeaderMap::new();
    assert!(is_spa_navigation("dashboard", &empty));
    assert!(is_spa_navigation("projects/abc", &empty));
    assert!(!is_spa_navigation("main.js", &empty));
    assert!(!is_spa_navigation("@vite/client", &empty));
    assert!(!is_spa_navigation("src/App.tsx", &empty));
    assert!(!is_spa_navigation("node_modules/react/index.js", &empty));
    let mut html = HeaderMap::new();
    html.insert(header::ACCEPT, "text/html,*/*".parse().unwrap());
    assert!(is_spa_navigation("some.thing", &html));
}

#[test]
fn loopback_headers_carry_host_as_x_oj_host_and_pass_forwarded_host_through() {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, "localhost:8080".parse().unwrap());
    h.append("x-forwarded-host", "app.example.com".parse().unwrap());
    h.append("x-forwarded-host", "edge.example.com".parse().unwrap());
    h.insert("x-oj-host", "spoofed.example.com".parse().unwrap());
    h.insert(header::ACCEPT, "text/html".parse().unwrap());
    let out = loopback_request_headers(&h);
    let get = |n: &str| {
        out.iter()
            .filter(|(k, _)| k.as_str() == n)
            .map(|(_, v)| v.to_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("x-oj-host"), ["localhost:8080"]);
    assert_eq!(
        get("x-forwarded-host"),
        ["app.example.com", "edge.example.com"]
    );
    assert!(
        get("host").is_empty(),
        "hyper writes the loopback Host itself"
    );
    assert_eq!(get("accept"), ["text/html"]);
    assert!(loopback_request_headers(&HeaderMap::new()).is_empty());
}

#[test]
fn content_changes_ignore_attribute_only_events_unless_the_mtime_moved() {
    use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind};
    let dir = std::env::temp_dir().join(format!("oj-content-changes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.ts");
    std::fs::write(&file, "export const a = 1;").unwrap();
    let meta = || {
        notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        )))
        .add_path(file.clone())
    };
    let data = || {
        notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        )))
        .add_path(file.clone())
    };
    let access = || {
        notify::Event::new(notify::EventKind::Access(AccessKind::Read)).add_path(file.clone())
    };
    // A never-seen file with an OLD mtime: the relatime atime storm shape.
    let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();
    let mut changes = ContentChanges::new();
    assert!(
        changes.changed_paths(&meta()).is_empty(),
        "an atime update on a never-seen old file is not a change"
    );
    assert!(
        changes.changed_paths(&meta()).is_empty(),
        "nor is a repeat with the same mtime"
    );
    assert!(changes.changed_paths(&access()).is_empty());
    // A never-seen file whose mtime is fresh: a touch, which must count once.
    let touched = dir.join("touched.ts");
    std::fs::write(&touched, "export const t = 1;").unwrap();
    let meta_touched = notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
        MetadataKind::Any,
    )))
    .add_path(touched.clone());
    assert_eq!(
        changes.changed_paths(&meta_touched),
        vec![touched.clone()],
        "a touch on a never-seen file is a change"
    );
    let meta_touched = notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
        MetadataKind::Any,
    )))
    .add_path(touched.clone());
    assert!(
        changes.changed_paths(&meta_touched).is_empty(),
        "and is then the baseline"
    );
    assert_eq!(
        changes.changed_paths(&data()),
        vec![file.clone()],
        "a data change always counts"
    );
    assert!(
        changes.changed_paths(&meta()).is_empty(),
        "the mtime the data change recorded has not moved"
    );
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(
        changes.changed_paths(&meta()),
        vec![file.clone()],
        "a moved mtime (touch) is a change, as in chokidar"
    );
    assert!(
        changes.changed_paths(&meta()).is_empty(),
        "and is then the new baseline"
    );
    std::fs::remove_file(&file).unwrap();
    assert_eq!(
        changes.changed_paths(&meta()),
        vec![file.clone()],
        "a vanished file is a change"
    );
    let removed =
        notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
            .add_path(file.clone());
    assert_eq!(changes.changed_paths(&removed), vec![file.clone()]);
    let _ = std::fs::remove_dir_all(&dir);
}
