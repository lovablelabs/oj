use std::path::{Path, PathBuf};

use crate::path::normalize;

/// How specifiers inside a stylesheet resolve beyond plain relative paths, as
/// Vite's CSS resolvers do: `resolve.alias` applies to `@import`, `@use` and
/// `url()`, and a root-absolute `/src/x` resolves against the root, with the
/// public directory taking precedence.
#[derive(Debug, Clone, Copy)]
pub struct CssResolve<'a> {
    pub root: Option<&'a Path>,
    pub public_dir: Option<&'a Path>,
    /// `(find, replacement)`; a replacement starting with `.` is root-relative.
    pub alias: &'a [(String, String)],
    /// `build.cssTarget` as esbuild-style names; empty means Vite's baseline.
    pub targets: &'a [String],
    /// `build.cssMinify` (dev never minifies).
    pub minify: bool,
    pub modules: &'a CssModulesOptions,
}

static DEFAULT_MODULES: CssModulesOptions = CssModulesOptions {
    locals_convention: None,
    generate_scoped_name: None,
    global_scope: false,
    global_module_paths: Vec::new(),
};

impl Default for CssResolve<'_> {
    fn default() -> Self {
        CssResolve {
            root: None,
            public_dir: None,
            alias: &[],
            targets: &[],
            minify: false,
            modules: &DEFAULT_MODULES,
        }
    }
}

/// Vite's `css.modules` (postcss-modules) options oj applies.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CssModulesOptions {
    /// `camelCase`, `camelCaseOnly`, `dashes` or `dashesOnly`.
    pub locals_convention: Option<String>,
    /// Default `[name]_[local]_[hash]`.
    pub generate_scoped_name: Option<String>,
    /// `scopeBehaviour: "global"`.
    pub global_scope: bool,
    /// Regex sources; a matching module file compiles unscoped.
    pub global_module_paths: Vec<String>,
}

/// Owned `CssResolve` for holders that outlive a borrow.
#[derive(Debug, Default, Clone)]
pub struct CssResolveConfig {
    pub root: PathBuf,
    pub public_dir: PathBuf,
    pub alias: Vec<(String, String)>,
    pub targets: Vec<String>,
    pub minify: bool,
    pub modules: CssModulesOptions,
}

impl CssResolveConfig {
    pub fn as_ref(&self) -> CssResolve<'_> {
        fn set(p: &Path) -> Option<&Path> {
            (!p.as_os_str().is_empty()).then_some(p)
        }
        CssResolve {
            root: set(&self.root),
            public_dir: set(&self.public_dir),
            alias: &self.alias,
            targets: &self.targets,
            minify: self.minify,
            modules: &self.modules,
        }
    }
}

impl From<CssResolve<'_>> for CssResolveConfig {
    fn from(r: CssResolve<'_>) -> Self {
        CssResolveConfig {
            root: r.root.map(Path::to_path_buf).unwrap_or_default(),
            public_dir: r.public_dir.map(Path::to_path_buf).unwrap_or_default(),
            alias: r.alias.to_vec(),
            targets: r.targets.to_vec(),
            minify: r.minify,
            modules: r.modules.clone(),
        }
    }
}

impl CssResolve<'_> {
    /// The specifier after `resolve.alias`, matched like @rollup/plugin-alias:
    /// the whole specifier or a `find/` prefix. Yields an absolute path for a
    /// path alias, a bare specifier for a package alias.
    pub fn alias_spec(&self, spec: &str) -> Option<String> {
        self.alias.iter().find_map(|(find, replacement)| {
            if find.is_empty() {
                return None;
            }
            let rest = if spec == find {
                ""
            } else {
                spec.strip_prefix(find.as_str())
                    .filter(|rest| rest.starts_with('/'))?
            };
            let target = match (replacement.starts_with('.'), self.root) {
                (true, Some(root)) => normalize(&root.join(replacement))
                    .to_string_lossy()
                    .into_owned(),
                _ => replacement.clone(),
            };
            Some(format!("{target}{rest}"))
        })
    }

    /// The absolute path an alias maps `spec` to (None for a package alias).
    pub fn alias_path(&self, spec: &str) -> Option<PathBuf> {
        let aliased = self.alias_spec(spec)?;
        Path::new(&aliased)
            .is_absolute()
            .then(|| PathBuf::from(aliased))
    }

    /// The public-directory file a root-absolute `/x` names, if it exists.
    pub fn public_file(&self, spec: &str) -> Option<PathBuf> {
        let p = self.public_dir?.join(root_absolute_rel(spec)?);
        p.is_file().then_some(p)
    }

    /// The path under the root a root-absolute `/x` names, not checked for
    /// existence so callers can probe extensions.
    pub fn root_path(&self, spec: &str) -> Option<PathBuf> {
        Some(self.root?.join(root_absolute_rel(spec)?))
    }

    /// The dev-server url of `file`: `/rel` inside the root, `/@fs` outside.
    pub(crate) fn dev_url(&self, file: &Path) -> String {
        match self.root.and_then(|r| file.strip_prefix(r).ok()) {
            Some(rel) => format!("/{}", rel.to_string_lossy().replace('\\', "/")),
            None => format!("/@fs{}", file.display()),
        }
    }
}

/// `/src/x` -> `src/x`; None for `//cdn`, `/` alone, or a relative spec.
fn root_absolute_rel(spec: &str) -> Option<&str> {
    let rel = spec.strip_prefix('/')?;
    (!rel.is_empty() && !rel.starts_with('/')).then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_matches_whole_specifier_or_slash_prefix_like_rollup_alias() {
        let alias = vec![
            ("@".to_string(), "./src".to_string()),
            ("@components".to_string(), "/abs/components".to_string()),
            ("react".to_string(), "preact/compat".to_string()),
        ];
        let r = CssResolve {
            root: Some(Path::new("/proj")),
            public_dir: None,
            alias: &alias,
            ..CssResolve::default()
        };
        assert_eq!(
            r.alias_spec("@/img.png").as_deref(),
            Some("/proj/src/img.png")
        );
        assert_eq!(r.alias_spec("@").as_deref(), Some("/proj/src"));
        // `@components/x` must not be eaten by the shorter `@` alias.
        assert_eq!(
            r.alias_spec("@components/btn.css").as_deref(),
            Some("/abs/components/btn.css")
        );
        assert_eq!(
            r.alias_spec("@scope/pkg/x.css"),
            None,
            "not a `find/` prefix match"
        );
        // A package alias stays a bare specifier (no path).
        assert_eq!(
            r.alias_spec("react/x.css").as_deref(),
            Some("preact/compat/x.css")
        );
        assert!(r.alias_path("react/x.css").is_none());
        assert_eq!(
            r.alias_path("@/a.css"),
            Some(PathBuf::from("/proj/src/a.css"))
        );
    }
}
