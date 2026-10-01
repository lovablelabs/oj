//! Path and url helpers shared by the resolvers.

use std::path::{Component, Path, PathBuf};

/// `p` with `.ext` appended (not replacing an existing extension).
pub(crate) fn with_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// `url` without its `?query` / `#hash`.
pub(crate) fn strip_query(url: &str) -> &str {
    split_query(url).0
}

/// `url` split into its path and its `?query` / `#hash` suffix.
pub(crate) fn split_query(url: &str) -> (&str, &str) {
    url.split_at(url.find(['?', '#']).unwrap_or(url.len()))
}

/// Lexical normalization: `.` dropped, `..` pops. No filesystem access.
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `target` relative to `from_dir`, `/`-separated, `./`-led unless it starts
/// with `..`.
pub(crate) fn relative_path(from_dir: &Path, target: &Path) -> String {
    let segments = |p: &Path| -> Vec<String> {
        normalize(p)
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    let from = segments(from_dir);
    let to = segments(target);
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; from.len() - common];
    parts.extend(to[common..].iter().map(String::as_str));
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// The relative path `rel` joined onto the relative path `base` (a
/// `relative_path` result), in the same `./` / `../` form.
pub(crate) fn join_relative(base: &str, rel: &str) -> String {
    let mut segments: Vec<&str> = base
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." if segments.last().is_some_and(|s| *s != "..") => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// `rel` joined onto the url directory `base_dir`, resolving `.` and `..`.
pub(crate) fn posix_join(base_dir: &str, rel: &str) -> String {
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("/{}", segments.join("/"))
}

/// Every `node_modules` directory from `dir` up to the filesystem root,
/// nearest first.
pub fn node_modules_load_paths(dir: &Path) -> Vec<PathBuf> {
    dir.ancestors()
        .filter(|d| d.file_name().is_none_or(|n| n != "node_modules"))
        .map(|d| d.join("node_modules"))
        .filter(|nm| nm.is_dir())
        .collect()
}

/// `dir/package.json`, parsed.
pub(crate) fn read_package_json(dir: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// A url that points off the local filesystem: `data:`, `//host`, `scheme://`.
pub(crate) fn is_external(spec: &str) -> bool {
    spec.starts_with("data:") || spec.starts_with("//") || spec.contains("://")
}
