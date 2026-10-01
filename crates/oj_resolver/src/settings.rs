use std::path::Path;

use oxc_resolver::{
    AliasValue, ResolveOptions, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};

use crate::defaults::{
    default_extension_alias, default_extensions, default_main_fields, default_server_main_fields,
    with_main_fallback,
};

#[derive(Clone, Default)]
pub struct ResolveSettings {
    pub conditions: Vec<String>,
    pub alias: Vec<(String, String)>,
    pub dedupe: Vec<String>,
    pub extensions: Option<Vec<String>>,
    pub main_fields: Option<Vec<String>>,
    pub preserve_symlinks: bool,
    /// Resolving for SSR: the default mainFields drop `browser`, and the
    /// package.json `browser` object only applies when mainFields name it.
    pub server: bool,
}

/// The oxc options for `settings` under `root`.
pub(crate) fn resolve_options(root: &Path, settings: &ResolveSettings) -> ResolveOptions {
    // oxc has no pkg.main fallback, so the list must end with "main".
    let main_fields = with_main_fallback(settings.main_fields.clone().unwrap_or_else(|| {
        if settings.server {
            default_server_main_fields()
        } else {
            default_main_fields()
        }
    }));
    let alias_fields = if main_fields.iter().any(|f| f == "browser") {
        vec![vec!["browser".to_string()]]
    } else {
        Vec::new()
    };
    let tsconfig = root.join("tsconfig.json");
    ResolveOptions {
        extensions: settings
            .extensions
            .clone()
            .unwrap_or_else(default_extensions),
        main_fields,
        alias_fields,
        condition_names: settings.conditions.clone(),
        alias: aliases(root, &settings.alias),
        extension_alias: default_extension_alias(),
        // oxc's exports-in-directory support runs after mainFields and index,
        // the last-resort half of Vite's behavior; `exports::directory_entry`
        // adds the exports-first half on top.
        allow_package_exports_in_directory_resolve: true,
        symlinks: !settings.preserve_symlinks,
        tsconfig: tsconfig
            .is_file()
            .then_some(TsconfigDiscovery::Manual(TsconfigOptions {
                config_file: tsconfig,
                references: TsconfigReferences::Auto,
            })),
        ..ResolveOptions::default()
    }
}

/// `(find, replacement)` pairs as oxc aliases; a `.`-led replacement is
/// root-relative.
fn aliases(root: &Path, alias: &[(String, String)]) -> Vec<(String, Vec<AliasValue>)> {
    alias
        .iter()
        .map(|(find, replacement)| {
            let target = if replacement.starts_with('.') {
                root.join(replacement).to_string_lossy().into_owned()
            } else {
                replacement.clone()
            };
            (find.clone(), vec![AliasValue::Path(target)])
        })
        .collect()
}
