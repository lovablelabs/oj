//! Packages oj answers for itself when engine code imports them: a module
//! file inside one is served oj's stand-in, never the package's own code.

const STYLEX_UNPLUGIN: &str = include_str!("stand_ins/stylex-unplugin.js");

/// `@stylexjs/unplugin`'s bundler adapters, each a module of its own
/// (`@stylexjs/unplugin/vite`) whose default export is the adapter.
const STYLEX_ADAPTERS: &[&str] = &[
    "bun", "esbuild", "farm", "rolldown", "rollup", "rspack", "unloader", "vite", "webpack",
];

/// How the engine is loading the file.
#[derive(Clone, Copy)]
pub(crate) enum Format {
    /// `import`: the module loader.
    Esm,
    /// `require()`: Node's CommonJS loader.
    CommonJs,
}

/// The stand-in for the module file at `path`, when its package is one oj
/// replaces.
pub(crate) fn source_for(path: &str, format: Format) -> Option<String> {
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
    let names = "bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack";
    let exports = match format {
        Format::Esm => format!(
            "export {{ {names} }};\n\
             export {{ stylexPlugin as createStylexBunPlugin, stylexPlugin as unpluginFactory, stylex as unplugin }};\n\
             export default {default};\n"
        ),
        // The package's CommonJS build: named exports beside `default`,
        // flagged `__esModule` for bundlers' interop.
        Format::CommonJs => format!(
            "module.exports = {{ __esModule: true, {names}, createStylexBunPlugin: stylexPlugin, \
             unpluginFactory: stylexPlugin, unplugin: stylex, default: {default} }};\n"
        ),
    };
    Some(format!("{STYLEX_UNPLUGIN}\n{exports}"))
}

#[cfg(test)]
mod tests {
    use super::{source_for, Format};

    #[test]
    fn every_module_of_the_package_is_the_stand_in() {
        let entry = source_for(
            "/app/node_modules/@stylexjs/unplugin/lib/es/index.mjs",
            Format::Esm,
        )
        .unwrap();
        assert!(entry.ends_with("export default stylex;\n"));
        let vite = source_for(
            "/app/node_modules/.pnpm/@stylexjs+unplugin@0.19.0/node_modules/@stylexjs/unplugin/lib/vite.js",
            Format::CommonJs,
        )
        .unwrap();
        assert!(vite.contains("__esModule: true") && vite.contains("default: vite }"));
        assert!(source_for(
            "/app/node_modules/@stylexjs/unplugin/package.json",
            Format::Esm
        )
        .is_none());
        assert!(source_for(
            "/app/node_modules/@stylexjs/stylex/lib/index.js",
            Format::Esm
        )
        .is_none());
    }
}
