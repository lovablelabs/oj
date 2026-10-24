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
                        // A sheet's imports are the files it inlined, Vite's
                        // file-only entries: never compiled on their own.
                        Ok((_, module)) if module.kind == "css" => Vec::new(),
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
                        let import = crawl_url(&import);
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

        let paths = served_module_paths(&state);
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

/// The graph's modules minus the files only stylesheets pull in (Vite's
/// file-only entries): those are HMR edges, never served as modules, so a
/// modulepreload of one would compile a partial without its importer.
pub(crate) fn served_module_paths(state: &ServerState) -> Vec<PathBuf> {
    let graph = state.graph.lock().unwrap();
    graph
        .module_paths()
        .into_iter()
        .filter(|p| {
            !graph.node(p).is_some_and(|n| {
                !n.importers.is_empty()
                    && n.importers
                        .iter()
                        .all(|i| is_style_url(&i.to_string_lossy()))
            })
        })
        .collect()
}

/// The url the crawl compiles for an import: its file, or a stylesheet's
/// `?inline` request, a module of its own (see [`graph_url`]).
fn crawl_url(import: &str) -> String {
    let (path, query) = import.split_once('?').unwrap_or((import, ""));
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if is_style_ext(ext) && query.split('&').any(|kv| kv == "inline") {
        format!("{path}?inline")
    } else {
        path.to_string()
    }
}

/// The file behind a crawled url, when it is one the eager crawl compiles.
fn crawl_file(state: &ServerState, url: &str) -> Option<PathBuf> {
    let url = url.split('?').next().unwrap_or(url);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Vite warms the url a module imports: a stylesheet's `?inline` is a
    /// module of its own, any other query is dropped to the file.
    #[test]
    fn the_crawl_compiles_a_stylesheets_inline_request_as_imported() {
        assert_eq!(crawl_url("/src/b.scss?inline"), "/src/b.scss?inline");
        assert_eq!(crawl_url("/src/b.css?t=1&inline"), "/src/b.css?inline");
        assert_eq!(crawl_url("/src/b.css?t=1"), "/src/b.css");
        assert_eq!(crawl_url("/src/a.ts?inline"), "/src/a.ts");
        assert_eq!(crawl_url("/src/a.ts"), "/src/a.ts");
    }
}
