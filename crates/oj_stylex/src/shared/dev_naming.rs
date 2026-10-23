//! Dev class names, test-mode replacement, and `$$css` debug source strings.
// parity: babel-plugin src/utils/{dev-classname,add-sourcemap-data}.js

use crate::eval::value::{EvalValue, JsObjectMap};
use crate::shared::dashify::sanitize_dev_class_name;
use std::sync::Arc;

/// Filesystem-derived inputs wave 3 computes; upstream's package.json walk
/// starts at dirname, so a package.json in cwd itself is skipped.
#[derive(Debug, Clone, Default)]
pub struct DebugPathInfo {
    /// Nearest package.json walking up from dirname(filename): (name, dir).
    pub file_package: Option<(String, String)>,
    /// Name of the nearest package.json walking up from dirname(cwd).
    pub cwd_package_name: Option<String>,
    /// unstable_moduleResolution commonJS rootDir, when configured.
    pub root_dir: Option<String>,
    pub is_haste: bool,
}

/// The `{basename}__{varName.}` prefix shared by every namespace of one call.
pub(crate) fn dev_class_prefix(var_name: Option<&str>, filename: &str) -> String {
    let basename = path_basename(filename)
        .split('.')
        .next()
        .unwrap_or_default();
    match var_name {
        Some(v) => format!("{basename}__{v}."),
        None => format!("{basename}__"),
    }
}

// parity: dev-classname.js namespaceToDevClassName
pub fn namespace_to_dev_class_name(
    namespace: &str,
    var_name: Option<&str>,
    filename: &str,
) -> String {
    sanitize_dev_class_name(&format!(
        "{}{namespace}",
        dev_class_prefix(var_name, filename)
    ))
}

// parity: dev-classname.js convertToTestStyles
pub fn convert_to_test_styles(
    compiled: &Arc<JsObjectMap>,
    var_name: Option<&str>,
    filename: Option<&str>,
) -> Arc<JsObjectMap> {
    let prefix = dev_class_prefix(var_name, filename.unwrap_or("UnknownFile"));
    let mut result = JsObjectMap::new();
    for (namespace, _) in compiled.entries() {
        let dev_class = sanitize_dev_class_name(&format!("{prefix}{namespace}"));
        let mut obj = JsObjectMap::new();
        obj.insert(dev_class.clone(), EvalValue::Str(dev_class));
        obj.insert("$$css", EvalValue::Bool(true));
        result.insert(namespace.to_string(), EvalValue::Obj(Arc::new(obj)));
    }
    Arc::new(result)
}

// parity: add-sourcemap-data.js createShortFilename (debugFilePath unsupported)
pub fn create_short_filename(filename: &str, info: &DebugPathInfo) -> String {
    if let Some((name, dir)) = &info.file_package {
        let relative = path_relative(dir, filename);
        if info.cwd_package_name.as_deref() == Some(name.as_str()) {
            return relative;
        }
        return format!("{name}:{relative}");
    }
    if let Some(prefix) = package_prefix(filename) {
        return format!("{prefix}:{}", short_path(filename, info));
    }
    if info.is_haste {
        return path_basename(filename).to_string();
    }
    short_path(filename, info)
}

// parity: add-sourcemap-data.js getShortPath
fn short_path(filename: &str, info: &DebugPathInfo) -> String {
    if let Some(root_dir) = &info.root_dir {
        return path_relative(root_dir, filename);
    }
    let segments: Vec<&str> = filename.split('/').collect();
    let start = segments.len().saturating_sub(2);
    segments[start..].join("/")
}

// parity: add-sourcemap-data.js getPackagePrefix ('' prefix is falsy upstream)
fn package_prefix(filename: &str) -> Option<String> {
    let idx = filename.find("node_modules")?;
    let rest = filename.get(idx + "node_modules".len() + 1..)?;
    let prefix = rest.split('/').next().unwrap_or_default();
    (!prefix.is_empty()).then(|| prefix.to_string())
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `path.relative` over already-normalized absolute '/'-separated paths.
fn path_relative(from: &str, to: &str) -> String {
    let from_parts: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; from_parts.len() - common];
    parts.extend(&to_parts[common..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_class_names() {
        assert_eq!(
            namespace_to_dev_class_name("root", Some("styles"), "/fake/root/pins.ts"),
            "pins__styles.root"
        );
        assert_eq!(
            namespace_to_dev_class_name("a", None, "/fake/root/pins.ts"),
            "pins__a"
        );
        assert_eq!(
            namespace_to_dev_class_name("weird ns$!name", Some("styles"), "/fake/root/pins.ts"),
            "pins__styles.weirdnsname"
        );
        assert_eq!(
            namespace_to_dev_class_name("0", Some("styles"), "/fake/root/pins.ts"),
            "pins__styles.0"
        );
    }

    #[test]
    fn short_filename_branches() {
        // Pinned via probes 2026-08-27 against @stylexjs/babel-plugin 0.19.0.
        let plain = DebugPathInfo::default();
        assert_eq!(
            create_short_filename("/fake/root/pins.ts", &plain),
            "root/pins.ts"
        );
        assert_eq!(create_short_filename("pins.ts", &plain), "pins.ts");

        let rooted = DebugPathInfo {
            root_dir: Some("/fake".to_string()),
            ..DebugPathInfo::default()
        };
        assert_eq!(
            create_short_filename("/fake/deep/nested/dir/pins.ts", &rooted),
            "deep/nested/dir/pins.ts"
        );
        let rooted_outside = DebugPathInfo {
            root_dir: Some("/fake/other".to_string()),
            ..DebugPathInfo::default()
        };
        assert_eq!(
            create_short_filename("/fake/deep/pins.ts", &rooted_outside),
            "../deep/pins.ts"
        );

        let pkg = DebugPathInfo {
            file_package: Some(("fixture-pkg".to_string(), "/tmp/x/fixture-pkg".to_string())),
            ..DebugPathInfo::default()
        };
        assert_eq!(
            create_short_filename("/tmp/x/fixture-pkg/src/thing.ts", &pkg),
            "fixture-pkg:src/thing.ts"
        );
        let same_pkg = DebugPathInfo {
            cwd_package_name: Some("fixture-pkg".to_string()),
            ..pkg
        };
        assert_eq!(
            create_short_filename("/tmp/x/fixture-pkg/src/thing.ts", &same_pkg),
            "src/thing.ts"
        );

        assert_eq!(
            create_short_filename("/fake/proj/node_modules/some-lib/dist/x.ts", &plain),
            "some-lib:dist/x.ts"
        );
        let rooted = DebugPathInfo {
            root_dir: Some("/fake".to_string()),
            ..DebugPathInfo::default()
        };
        assert_eq!(
            create_short_filename("/fake/proj/node_modules/some-lib/dist/x.ts", &rooted),
            "some-lib:proj/node_modules/some-lib/dist/x.ts"
        );
    }

    #[test]
    fn short_filename_haste_arm() {
        // Last resort only: a package or node_modules ancestor still wins.
        let haste = DebugPathInfo {
            is_haste: true,
            ..DebugPathInfo::default()
        };
        assert_eq!(
            create_short_filename("/html/js/components/Foo.react.js", &haste),
            "Foo.react.js"
        );
        let in_package = DebugPathInfo {
            file_package: Some(("fixture-pkg".to_string(), "/tmp/x/fixture-pkg".to_string())),
            ..haste
        };
        assert_eq!(
            create_short_filename("/tmp/x/fixture-pkg/src/Foo.js", &in_package),
            "fixture-pkg:src/Foo.js"
        );
    }
}
