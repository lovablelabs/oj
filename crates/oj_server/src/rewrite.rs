use super::*;

pub(crate) type DirCache = std::collections::HashMap<
    PathBuf,
    std::sync::Arc<std::collections::HashMap<std::ffi::OsString, bool>>,
>;

// Whether a cached module imports a plugin-served virtual: a filesystem-path import
// with no file on disk, checked both root-relative and absolute. Such a module's
// transform produced in-memory plugin state a warm start loses, so re-transform it.
pub(crate) fn imports_a_plugin_virtual(
    imports: &[String],
    root: &Path,
    dir_cache: &Mutex<DirCache>,
) -> bool {
    imports.iter().any(|imp| {
        let p = imp.split('?').next().unwrap_or(imp);
        if !p.starts_with('/') || p.starts_with("/@") || p.contains("://") {
            return false;
        }
        let as_root = root.join(p.trim_start_matches('/'));
        if is_file_cached(dir_cache, &as_root) {
            return false;
        }
        let as_abs = Path::new(p);
        !is_file_cached(dir_cache, as_abs)
    })
}

pub(crate) fn is_file_cached(cache: &Mutex<DirCache>, path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return path.is_file();
    };
    if let Some(entries) = cache.lock().unwrap().get(dir) {
        return entries.get(name).copied().unwrap_or(false);
    }
    let mut map = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let is_file = match e.file_type() {
                Ok(ft) if ft.is_file() => true,
                Ok(ft) if ft.is_symlink() => e.path().is_file(),
                _ => false,
            };
            map.insert(e.file_name(), is_file);
        }
    }
    let arc = std::sync::Arc::new(map);
    let result = arc.get(name).copied().unwrap_or(false);
    cache.lock().unwrap().insert(dir.to_path_buf(), arc);
    result
}

/// Asset queries that pass through to the resolved URL unchanged.
const PASSTHROUGH_QUERIES: &[&str] = &[
    "url",
    "raw",
    "inline",
    "worker",
    "sharedworker",
    "init",
    "react",
    "no-inline",
];

pub(crate) fn rewrite_specifier(
    root: &Path,
    dir: &Path,
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    dir_cache: &Mutex<DirCache>,
    spec: &str,
    css_import_marker: bool,
) -> Option<String> {
    if spec.starts_with('/') {
        return rewrite_absolute(root, fs_allow, dir_cache, spec);
    }
    if spec.contains("://") {
        return None;
    }

    if is_lingui_macro_specifier(spec) {
        warn_lingui_macro_shim_once();
        return Some("/@oj/lingui-macro-shim.js".to_string());
    }

    if let Some((base, query)) = spec.split_once('?') {
        if PASSTHROUGH_QUERIES.contains(&query) {
            let resolved = rewrite_specifier(root, dir, resolver, fs_allow, dir_cache, base, false)
                .or_else(|| {
                    resolver.resolve(dir, base).ok().map(|p| {
                        allow_package(fs_allow, &p);
                        url_of(root, &p)
                    })
                })?;
            return Some(format!("{resolved}?{query}"));
        }
    }

    // A package.json `browser` object can remap a dependency's own relative files
    // (or map one to `false`), and only the resolver applies that map: the
    // on-disk fast path is for app source.
    if (spec.starts_with("./") || spec.starts_with("../")) && !in_node_modules(dir) {
        if let Some(p) = find_relative_on_disk(dir, dir_cache, spec) {
            return Some(mark_css_import(url_of(root, &p), &p, css_import_marker));
        }
    }

    match resolver.resolve(dir, spec) {
        // An alias (`@/assets/logo.svg`) or root-absolute import of a style
        // or asset gets the same `?import` / `?url` marks a relative one does.
        Ok(resolved) if !in_node_modules(&resolved) && resolved.starts_with(root) => Some(
            mark_css_import(url_of(root, &resolved), &resolved, css_import_marker),
        ),
        // A node_modules dep routes through `dep_serve_url` even under the app root
        // so partial bundling can collapse it (per-file URL no-op when off).
        Ok(resolved) => {
            allow_package(fs_allow, &resolved);
            Some(dep_serve_url(&resolved, root))
        }
        Err(err) if err.ignored => {
            // The package's `browser` field maps this specifier to false: serve an
            // empty module like Vite so the importing dep still loads.
            Some("/@oj-empty".to_string())
        }
        Err(err) => {
            // A missing OPTIONAL peer resolves to a stub that errors when evaluated,
            // naming both sides (Vite's optionalPeerDepId), not a link-time failure.
            if let Some(url) = optional_peer_dep_url(root, dir, spec) {
                return Some(url);
            }
            // virtual:/\0 ids are expected to miss the disk resolver (the plugin
            // fallback serves them), so a "cannot resolve" line is noise.
            let plugin_virtual = spec.starts_with("virtual:") || spec.starts_with('\0');
            if !(spec.starts_with("./")
                || spec.starts_with("../")
                || plugin_virtual
                || is_node_builtin(spec))
            {
                eprintln!("oj: cannot resolve '{spec}': {err}");
            }
            None
        }
    }
}

// A root-relative URL is already servable, but a plugin can emit an absolute fs
// path under root: rewrite it to its URL, preserving the query (Vite parity).
fn rewrite_absolute(
    root: &Path,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    dir_cache: &Mutex<DirCache>,
    spec: &str,
) -> Option<String> {
    let (base, query) = match spec.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (spec, None),
    };
    let p = Path::new(base);
    if !is_file_cached(dir_cache, p) {
        return None;
    }
    let url = if p.starts_with(root) {
        url_of(root, p)
    } else {
        // An absolute path OUTSIDE root: serve through /@fs (Vite's
        // FS_PREFIX) and allow its package for the fs guard.
        allow_package(fs_allow, p);
        dep_serve_url(p, root)
    };
    Some(match query {
        Some(q) => format!("{url}?{q}"),
        None => url,
    })
}

/// The file a relative import names, trying `.ts`/`.tsx` for a missing `.js`/`.jsx`
/// and the compilable extensions for an extensionless path.
fn find_relative_on_disk(dir: &Path, dir_cache: &Mutex<DirCache>, spec: &str) -> Option<PathBuf> {
    let mut joined = normalize(&dir.join(spec));
    if !is_file_cached(dir_cache, &joined) {
        if let Some("js" | "jsx") = joined.extension().and_then(|e| e.to_str()) {
            if let Some(alt) = ["ts", "tsx"]
                .iter()
                .map(|ext| joined.with_extension(ext))
                .find(|alt| is_file_cached(dir_cache, alt))
            {
                joined = alt;
            }
        }
    }
    if is_file_cached(dir_cache, &joined) {
        Some(joined)
    } else if joined.extension().is_none() {
        COMPILABLE
            .iter()
            .map(|ext| joined.with_extension(ext))
            .find(|c| is_file_cached(dir_cache, c))
    } else {
        None
    }
}

/// Marks a style import `?import` and an asset import `?url`, when requested.
fn mark_css_import(url: String, file: &Path, css_import_marker: bool) -> String {
    if css_import_marker && is_style_url(&url) {
        format!("{url}?import")
    } else if css_import_marker && is_asset_path(file) {
        format!("{url}?url")
    } else {
        url
    }
}

/// Lets the fs guard serve files from the package containing `file`.
fn allow_package(fs_allow: &Mutex<std::collections::HashSet<PathBuf>>, file: &Path) {
    allow_root(fs_allow, package_root(file));
}

pub(crate) fn in_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

pub(crate) fn url_of(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => format!("/@fs{}", file.display()),
    }
}

/// A `./` or `../` import that neither exists nor resolves. The query is dropped
/// first: an unknown query on an existing file is not a missing module.
pub(crate) fn relative_import_missing(dir: &Path, resolver: &OjResolver, spec: &str) -> bool {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return false;
    }
    let base = spec.split('?').next().unwrap_or(spec);
    !normalize(&dir.join(base)).is_file() && resolver.resolve(dir, base).is_err_and(|e| !e.ignored)
}

/// A bare import the resolver cannot find and nothing else answers (builtin,
/// virtual id, data:, browser-false mapping): Vite's importAnalysis fails these.
pub(crate) fn bare_import_unresolved(dir: &Path, resolver: &OjResolver, spec: &str) -> bool {
    if !is_bare_specifier(spec)
        || spec.is_empty()
        || spec.starts_with("virtual:")
        || spec.starts_with('\0')
        || spec.starts_with("data:")
        || is_node_builtin(spec)
        || is_lingui_macro_specifier(spec)
    {
        return false;
    }
    let base = spec.split('?').next().unwrap_or(spec);
    resolver.resolve(dir, base).is_err_and(|e| !e.ignored)
}

pub(crate) const UNRESOLVED_IMPORT_MARK: &str = "Failed to resolve import \"";

pub(crate) fn is_unresolved_import_error(err: &str) -> bool {
    err.contains(UNRESOLVED_IMPORT_MARK)
}

/// Vite's missing-import error in oj's `title\nfile:line:col message\nframe` shape;
/// the position is where the specifier is quoted in the (plugin-transformed) source.
pub(crate) fn unresolved_import_error(
    root: &Path,
    file: &Path,
    source: &str,
    spec: &str,
) -> String {
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string();
    let quoted = ['"', '\'', '`']
        .iter()
        .find_map(|q| source.find(&format!("{q}{spec}{q}")).map(|p| p + 1));
    let (line, col) = match quoted {
        Some(pos) => {
            let before = &source[..pos];
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            (line, col)
        }
        None => (1, 1),
    };
    let frame = source.lines().nth(line - 1).unwrap_or("");
    format!(
        "compile error:\n{rel}:{line}:{col} {UNRESOLVED_IMPORT_MARK}{spec}\" from \"{rel}\". Does the file exist?\n{line:>4} | {frame}\n"
    )
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn locate(root: &Path, public_dir: Option<&Path>, rel: &str) -> Option<PathBuf> {
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let base = root.join(rel);
    if base.is_file() {
        return Some(base);
    }
    if base.extension().is_none() {
        for ext in COMPILABLE {
            let candidate = base.with_extension(ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let public = public_dir?.join(rel);
    if public.is_file() {
        return Some(public);
    }
    None
}
