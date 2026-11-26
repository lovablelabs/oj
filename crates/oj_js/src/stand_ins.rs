//! Packages oj answers for itself when engine code imports them: a module
//! file inside one is served oj's stand-in, never the package's own code.

const STYLEX_UNPLUGIN: &str = include_str!("stand_ins/stylex-unplugin.js");

/// `@stylexjs/unplugin`'s bundler adapters, each a module of its own
/// (`@stylexjs/unplugin/vite`) whose default export is the adapter.
const STYLEX_ADAPTERS: &[&str] = &[
    "bun", "esbuild", "farm", "rolldown", "rollup", "rspack", "unloader", "vite", "webpack",
];

/// The package's modules served as they are: what its real Vite adapter
/// requires when the stand-in evaluates it (see the stand-in's
/// `realVitePlugin`).
const STYLEX_REAL_MODULES: &[&str] = &["core", "consts", "dev-inject-middleware"];

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
    let (prefix, file) = path.split_once("/node_modules/@stylexjs/unplugin/")?;
    let name = file.rsplit('/').next()?;
    let (stem, ext) = name.split_once('.')?;
    if !matches!(ext, "js" | "mjs" | "cjs") || STYLEX_REAL_MODULES.contains(&stem) {
        return None;
    }
    let default = if STYLEX_ADAPTERS.contains(&stem) {
        stem
    } else {
        "stylex"
    };
    let dir = serde_json::Value::String(format!("{prefix}/node_modules/@stylexjs/unplugin"));
    let names = "bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack";
    let (prelude, exports) = match format {
        Format::Esm => (
            format!(
                "import {{ createRequire as __ojCreateRequire }} from \"node:module\";\n\
                 import {{ readFileSync as __ojReadFile, realpathSync as __ojRealpath }} from \"node:fs\";\n\
                 import {{ dirname as __ojDirname }} from \"node:path\";\n\
                 const __ojUnpluginDir = {dir};\n"
            ),
            format!(
                "export {{ {names} }};\n\
                 export {{ stylexPlugin as createStylexBunPlugin, stylexPlugin as unpluginFactory, stylex as unplugin }};\n\
                 export default {default};\n"
            ),
        ),
        // The package's CommonJS build: named exports beside `default`,
        // flagged `__esModule` for bundlers' interop.
        Format::CommonJs => (
            format!(
                "const {{ createRequire: __ojCreateRequire }} = require(\"node:module\");\n\
                 const {{ readFileSync: __ojReadFile, realpathSync: __ojRealpath }} = require(\"node:fs\");\n\
                 const {{ dirname: __ojDirname }} = require(\"node:path\");\n\
                 const __ojUnpluginDir = {dir};\n"
            ),
            format!(
                "module.exports = {{ __esModule: true, {names}, createStylexBunPlugin: stylexPlugin, \
                 unpluginFactory: stylexPlugin, unplugin: stylex, default: {default} }};\n"
            ),
        ),
    };
    Some(format!("{prelude}{STYLEX_UNPLUGIN}\n{exports}"))
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
        assert!(entry.contains("const __ojUnpluginDir = \"/app/node_modules/@stylexjs/unplugin\";"));
        assert!(
            entry.contains("import { createRequire as __ojCreateRequire } from \"node:module\";")
        );
        let vite = source_for(
            "/app/node_modules/.pnpm/@stylexjs+unplugin@0.19.0/node_modules/@stylexjs/unplugin/lib/vite.js",
            Format::CommonJs,
        )
        .unwrap();
        assert!(vite.contains("__esModule: true") && vite.contains("default: vite }"));
        assert!(vite.contains(
            "const __ojUnpluginDir = \"/app/node_modules/.pnpm/@stylexjs+unplugin@0.19.0/node_modules/@stylexjs/unplugin\";"
        ));
        assert!(vite.contains("= require(\"node:module\");"));
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

    /// The modules the package's real Vite adapter requires load as they
    /// are, so the stand-in can evaluate the adapter from its file.
    #[test]
    fn the_adapters_dependencies_are_the_packages_own() {
        for file in [
            "lib/core.js",
            "lib/consts.js",
            "lib/dev-inject-middleware.js",
            "lib/es/core.mjs",
        ] {
            assert!(
                source_for(
                    &format!("/app/node_modules/@stylexjs/unplugin/{file}"),
                    Format::CommonJs
                )
                .is_none(),
                "{file}"
            );
        }
        assert!(source_for(
            "/app/node_modules/@stylexjs/unplugin/lib/rollup.js",
            Format::CommonJs
        )
        .is_some());
    }
}
