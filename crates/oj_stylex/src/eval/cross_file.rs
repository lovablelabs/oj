//! `.stylex` theme-file import proxies: hash-only, no file contents are read.
// parity: evaluate-path.js createVarGroupProxy/resolveVarGroupKey/:595-654

use std::rc::Rc;

use crate::hash::hash;
use crate::module_resolution::gen_file_based_identifier;
use crate::options::ResolvedOptions;

// parity: evaluation-errors.js — exact user-facing texts (first line is what
// pins compare after babel's code-frame formatting).
pub const IMPORT_PATH_RESOLUTION_ERROR: &str = "Could not resolve the path to the imported file.\nPlease ensure that the theme file has a .stylex.js or .stylex.ts extension and follows the\nrules for defining variables:\n\nhttps://stylexjs.com/docs/learn/theming/defining-variables/#rules-when-defining-variables\n";

pub const IMPORT_FILE_EVAL_ERROR: &str = "There was an error when attempting to evaluate the imported file.\nPlease ensure that the imported file is self-contained and does not rely on dynamic behavior.\n";

pub const NON_CONSTANT: &str = "Referenced value is not a constant.\n\n";

pub const USED_BEFORE_DECLARATION: &str = "Referenced value is used before declaration.\n\n";

pub const UNINITIALIZED_CONST: &str = "Referenced constant is not initialized.\n\n";

pub const UNDEFINED_CONST: &str = "Referenced constant is not defined.";

pub const PATH_WITHOUT_NODE: &str =
    "Unexpected error:\nCould not resolve the code being evaluated.\n";

pub const UNEXPECTED_MEMBER_LOOKUP: &str =
    "Unexpected error:\nCould not determine the property being accessed.\n";

pub const OBJECT_METHOD: &str = "Unsupported object method.\n\n";

pub fn unsupported_expression(node_type: &str) -> String {
    format!("Unsupported expression: {node_type}\n\n")
}

pub fn unsupported_operator(op: &str) -> String {
    format!("Unsupported operator: {op}\n\n")
}

/// Stand-in for the upstream JS Proxy over a theme-file export: every member
/// access resolves to a `var(--…)` string derived from hashes alone.
#[derive(Debug, Clone)]
pub struct VarGroupProxy {
    group: Rc<VarGroup>,
}

#[derive(Debug, PartialEq, Eq)]
struct VarGroup {
    /// Canonical theme-file name (`pkg:relPath` form).
    file_name: String,
    export_name: String,
    var_group_hash: String,
    class_name_prefix: String,
    debug_class_names: bool,
}

impl PartialEq for VarGroupProxy {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.group, &other.group) || self.group == other.group
    }
}

impl Eq for VarGroupProxy {}

impl VarGroupProxy {
    pub fn new(file_name: String, export_name: String, options: &ResolvedOptions) -> Self {
        let var_group_hash = format!(
            "{}{}",
            options.class_name_prefix,
            hash(&gen_file_based_identifier(&file_name, &export_name, None))
        );
        VarGroupProxy {
            group: Rc::new(VarGroup {
                file_name,
                export_name,
                var_group_hash,
                class_name_prefix: options.class_name_prefix.clone(),
                debug_class_names: options.debug && options.enable_debug_class_names,
            }),
        }
    }

    pub fn var_group_hash(&self) -> &str {
        &self.group.var_group_hash
    }

    // parity: evaluate-path.js resolveVarGroupKey.
    pub fn resolve_key(&self, key: &str) -> String {
        if key.starts_with("--") {
            return format!("var({key})");
        }
        let group = &*self.group;
        let hashed = hash(&gen_file_based_identifier(
            &group.file_name,
            &group.export_name,
            Some(key),
        ));
        let var_name = if group.debug_class_names {
            let mut safe: String = key
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            if key.starts_with(|c: char| c.is_ascii_digit()) {
                safe.insert(0, '_');
            }
            format!("{safe}-{}{hashed}", group.class_name_prefix)
        } else {
            format!("{}{hashed}", group.class_name_prefix)
        };
        format!("var(--{var_name})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(options: &ResolvedOptions) -> VarGroupProxy {
        VarGroupProxy::new(
            "probe-pkg:src/tokens.stylex.ts".to_string(),
            "colors".to_string(),
            options,
        )
    }

    #[test]
    fn oracle_pinned_hashes() {
        // Pinned live against @stylexjs/babel-plugin 0.19.0 (probe 2026-08-27).
        let options = ResolvedOptions::default();
        let p = proxy(&options);
        assert_eq!(p.var_group_hash(), "x73pkp7");
        assert_eq!(p.resolve_key("accent"), "var(--x8fssgy)");
        assert_eq!(p.resolve_key("--raw"), "var(--raw)");
    }

    #[test]
    fn debug_class_names_var_safe_keys() {
        let options = crate::options::CompilerOptions::from_json(
            &serde_json::json!({ "debug": true, "enableDebugClassNames": true }),
        )
        .unwrap()
        .resolve()
        .unwrap();
        let p = proxy(&options);
        assert_eq!(p.resolve_key("2fast"), "var(--_2fast-x1xncp22)");
        assert_eq!(p.resolve_key("weird key!"), "var(--weird_key_-x1ixwx2f)");
    }
}
