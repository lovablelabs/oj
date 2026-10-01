use std::path::{Component, Path, PathBuf};

/// `base` + `spec` with `.`/`..` folded lexically (no fs), so the result
/// compares against resolver-returned paths.
pub(crate) fn lexical_join(base: &Path, spec: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in Path::new(spec).components() {
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

/// The package of a bare specifier: `react-dom/client` -> `react-dom`,
/// `@radix-ui/react-slot/x` -> `@radix-ui/react-slot`.
pub(crate) fn package_name(spec: &str) -> &str {
    let first = spec.find('/').unwrap_or(spec.len());
    if !spec.starts_with('@') || first == spec.len() {
        return &spec[..first];
    }
    let second = spec[first + 1..]
        .find('/')
        .map_or(spec.len(), |i| first + 1 + i);
    &spec[..second]
}

/// `spec` without its `?query` / `#hash`.
pub(crate) fn strip_query(spec: &str) -> &str {
    &spec[..spec.find(['?', '#']).unwrap_or(spec.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_handles_scopes_and_subpaths() {
        assert_eq!(package_name("react"), "react");
        assert_eq!(package_name("react-dom/client"), "react-dom");
        assert_eq!(package_name("@radix-ui/react-slot"), "@radix-ui/react-slot");
        assert_eq!(
            package_name("@radix-ui/react-slot/x/y"),
            "@radix-ui/react-slot"
        );
        assert_eq!(package_name("@scope"), "@scope");
    }
}
