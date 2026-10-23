//! Packages oj answers for itself when engine code imports them: a module
//! file inside one is served oj's stand-in, never the package's own code.

const STYLEX_UNPLUGIN: &str = include_str!("stand_ins/stylex-unplugin.mjs");

/// `@stylexjs/unplugin`'s bundler adapters, each a module of its own
/// (`@stylexjs/unplugin/vite`) whose default export is the adapter.
const STYLEX_ADAPTERS: &[&str] = &[
    "bun", "esbuild", "farm", "rolldown", "rollup", "rspack", "unloader", "vite", "webpack",
];

/// The stand-in for the module file at `path`, when its package is one oj
/// replaces.
pub(crate) fn source_for(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let (_, file) = path.split_once("/node_modules/@stylexjs/unplugin/")?;
    let name = file.rsplit('/').next()?;
    let (stem, ext) = name.split_once('.')?;
    if !matches!(ext, "js" | "mjs" | "cjs") {
        return None;
    }
    let default = if STYLEX_ADAPTERS.contains(&stem) {
        stem
    } else {
        "stylex"
    };
    Some(format!("{STYLEX_UNPLUGIN}\nexport default {default};\n"))
}

#[cfg(test)]
mod tests {
    use super::source_for;

    #[test]
    fn every_module_of_the_package_is_the_stand_in() {
        let entry = source_for("/app/node_modules/@stylexjs/unplugin/lib/es/index.mjs").unwrap();
        assert!(entry.ends_with("export default stylex;\n"));
        let vite = source_for("/app/node_modules/.pnpm/@stylexjs+unplugin@0.19.0/node_modules/@stylexjs/unplugin/lib/vite.js")
            .unwrap();
        assert!(vite.ends_with("export default vite;\n"));
        assert!(source_for("/app/node_modules/@stylexjs/unplugin/package.json").is_none());
        assert!(source_for("/app/node_modules/@stylexjs/stylex/lib/index.js").is_none());
    }
}
