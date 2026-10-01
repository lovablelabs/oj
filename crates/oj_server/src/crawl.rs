use super::*;

pub(crate) fn spawn_crawl(state: Arc<ServerState>, done_tx: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = html_entries(&state.root);
        let mut tasks = tokio::task::JoinSet::new();

        loop {
            for url in queue.drain(..) {
                if !visited.insert(url.clone()) {
                    continue;
                }
                let Some(file) = crawl_file(&state, &url) else {
                    continue;
                };
                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    match ensure_module(&state, &file, &url).await {
                        Ok((_, module)) => module.imports.clone(),
                        Err(err) => {
                            eprintln!("oj: crawl: {err}");
                            Vec::new()
                        }
                    }
                });
            }
            match tasks.join_next().await {
                None => break,
                Some(imports) => {
                    for import in imports.unwrap_or_default() {
                        let import = import.split('?').next().unwrap_or(&import).to_string();
                        if import.starts_with('/')
                            && !import.starts_with("/@oj/")
                            && !visited.contains(&import)
                        {
                            queue.push(import);
                        }
                    }
                }
            }
        }

        let paths = state.graph.lock().unwrap().module_paths();
        println!(
            "{} eager graph ready: {} modules in {:?}",
            oj_tag(),
            paths.len(),
            started.elapsed()
        );
        save_graph_snapshot(&state.root, &paths);
        let _ = done_tx.send(true);
    });
}

/// The file behind a crawled url, when it is one the eager crawl compiles.
fn crawl_file(state: &ServerState, url: &str) -> Option<PathBuf> {
    let file = if let Some(abs) = url.strip_prefix("/@fs") {
        let f = PathBuf::from(abs);
        let allowed = state
            .fs_allow
            .lock()
            .unwrap()
            .iter()
            .any(|r| f.starts_with(r));
        if !allowed {
            return None;
        }
        f
    } else {
        locate(
            &state.root,
            state.public_dir.as_deref(),
            url.trim_start_matches('/'),
        )?
    };
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !(COMPILABLE.contains(&ext) || is_style_ext(ext) || ext == "json") {
        return None;
    }
    // A sass partial is not an entry: it reaches the graph via its
    // importer, and a standalone compile lacks importer-provided mixins.
    let partial = matches!(ext, "scss" | "sass")
        && file
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('_'));
    (!partial).then_some(file)
}

pub(crate) fn snapshot_path(root: &Path) -> PathBuf {
    oj_cache::cache_root(root).join("graph-snapshot.json")
}

pub(crate) fn load_graph_snapshot(root: &Path) -> Vec<String> {
    std::fs::read(snapshot_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(crate) fn save_graph_snapshot(root: &Path, paths: &[PathBuf]) {
    let urls: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let path = snapshot_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_vec(&urls).unwrap_or_default());
}
