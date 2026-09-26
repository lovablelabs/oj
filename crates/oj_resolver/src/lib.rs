// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oxc_resolver::{
    AliasValue, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};

pub struct OjResolver {
    inner: Resolver,
    root: PathBuf,
    dedupe: Vec<String>,
}

/// The package id of a bare specifier: `react-dom/client` -> `react-dom`,
/// `@radix-ui/react-slot/x` -> `@radix-ui/react-slot`.
fn package_name(spec: &str) -> String {
    let mut it = spec.split('/');
    let first = it.next().unwrap_or("");
    if first.starts_with('@') {
        match it.next() {
            Some(second) => format!("{first}/{second}"),
            None => first.to_string(),
        }
    } else {
        first.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cannot resolve '{specifier}' from '{importer}': {reason}")]
pub struct ResolveFailure {
    pub specifier: String,
    pub importer: PathBuf,
    pub reason: String,
    /// The package deliberately maps this specifier to `false` (a package.json
    /// `browser` field entry or a `{ find, replacement: false }` alias). Not a
    /// real failure: the module is meant to be empty in this target, so it
    /// should be served as an empty stub rather than 404'd.
    pub ignored: bool,
}

#[derive(Clone, Default)]
pub struct ResolveSettings {
    pub conditions: Vec<String>,
    pub alias: Vec<(String, String)>,
    pub dedupe: Vec<String>,
    pub extensions: Option<Vec<String>>,
    pub main_fields: Option<Vec<String>>,
    pub preserve_symlinks: bool,
    /// Resolving for a server (SSR) environment: Vite's DEFAULT_SERVER_MAIN_FIELDS
    /// drop `browser`, and the package.json `browser` object remap only applies
    /// when the effective mainFields include `browser` (resolve.ts
    /// tryResolveBrowserMapping), so Node-side code gets the Node build.
    pub server: bool,
}

/// Extensions probed for an extensionless import, in priority order: Vite's
/// DEFAULT_EXTENSIONS (constants.ts), so `./foo` with both `foo.js` and `foo.ts`
/// on disk picks `foo.js` like Vite, and `./foo` reaches `foo.mts`. Shared with
/// the dependency optimizer so it resolves exactly like the dev server.
pub fn default_extensions() -> Vec<String> {
    [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".json"]
        .map(String::from)
        .to_vec()
}

/// Package entry fields for legacy deps without an `exports` map. `module` leads
/// so a dep shipping both an ESM build and a `browser` STRING that points at a
/// UMD/CJS bundle (e.g. transliteration) serves its ESM: real named exports, no
/// CJS-interop guessing. The `browser` OBJECT remap (node-shim swaps) is
/// unaffected: it runs through alias_fields, not here. Shared with the optimizer.
pub fn default_main_fields() -> Vec<String> {
    ["module", "browser", "jsnext:main", "jsnext", "main"]
        .map(String::from)
        .to_vec()
}

/// Vite's DEFAULT_SERVER_MAIN_FIELDS: the client list without `browser`.
pub fn default_server_main_fields() -> Vec<String> {
    default_main_fields()
        .into_iter()
        .filter(|f| f != "browser")
        .collect()
}

/// Vite's `pkg.main` fallback as a list transform: resolvePackageEntry ALWAYS
/// falls back to `pkg.main` after the mainFields walk (`entryPoint ||=
/// data.main`; its resolve plugin spells it `mainFields.concat(["main"])`), so
/// its DEFAULT_MAIN_FIELDS deliberately omit "main". A resolver without that
/// fallback (oxc_resolver, rolldown, the dep optimizer's sidecar) must take
/// the list with "main" appended LAST — never outranking the configured
/// fields — or a package whose only entry is `main` resolves in one consumer
/// and fails in the next.
pub fn with_main_fallback(mut fields: Vec<String>) -> Vec<String> {
    if !fields.iter().any(|f| f == "main") {
        fields.push("main".to_string());
    }
    fields
}

/// Vite's TS-output remap (resolve.ts `tryCleanFsResolve` / `isPossibleTsOutput`):
/// an import ending in `.js`/`.jsx`/`.mjs`/`.cjs` with no such file on disk
/// resolves to its TypeScript source (`.ts` then `.tsx`, `.tsx`, `.mts`, `.cts`),
/// so a NodeNext-style `import "./x.js"` works from source. The real extension
/// leads so an existing `.js` wins over a sibling `.ts`, as Vite tries the exact
/// file first. It applies to every filesystem path, aliased and tsconfig-paths
/// imports included, not only relative ones. Shared with the build.
pub fn default_extension_alias() -> Vec<(String, Vec<String>)> {
    [
        (".js", &[".js", ".ts", ".tsx"][..]),
        (".jsx", &[".jsx", ".tsx"][..]),
        (".mjs", &[".mjs", ".mts"][..]),
        (".cjs", &[".cjs", ".cts"][..]),
    ]
    .iter()
    .map(|(ext, alts)| {
        (
            ext.to_string(),
            alts.iter().map(|s| s.to_string()).collect(),
        )
    })
    .collect()
}

impl OjResolver {
    pub fn new(root: &Path) -> Self {
        Self::with_conditions(
            root,
            &["browser", "import", "module", "development", "default"].map(String::from),
        )
    }

    pub fn with_conditions(root: &Path, conditions: &[String]) -> Self {
        Self::with_options(root, conditions, &[], &[])
    }

    pub fn with_options(
        root: &Path,
        conditions: &[String],
        alias: &[(String, String)],
        dedupe: &[String],
    ) -> Self {
        Self::with_settings(
            root,
            ResolveSettings {
                conditions: conditions.to_vec(),
                alias: alias.to_vec(),
                dedupe: dedupe.to_vec(),
                ..ResolveSettings::default()
            },
        )
    }

    pub fn with_settings(root: &Path, settings: ResolveSettings) -> Self {
        let tsconfig = root.join("tsconfig.json");
        let alias = settings
            .alias
            .iter()
            .map(|(find, replacement)| {
                let target = if replacement.starts_with('.') {
                    root.join(replacement).to_string_lossy().into_owned()
                } else {
                    replacement.clone()
                };
                (find.clone(), vec![AliasValue::Path(target)])
            })
            .collect();
        // The `with_main_fallback` append: oxc_resolver has no pkg.main
        // fallback (an exhausted main_fields goes straight to the index
        // files), so the list must end with "main", or a package whose only
        // entry is `main` (a linked workspace package with a TS main, say)
        // stops resolving the moment a Vite-shaped mainFields list is adopted
        // from the config.
        let main_fields = with_main_fallback(settings.main_fields.unwrap_or_else(|| {
            if settings.server {
                default_server_main_fields()
            } else {
                default_main_fields()
            }
        }));
        // Vite applies the package.json `browser` object only when mainFields
        // include `browser` (the client default; a server list opts in by naming it).
        let alias_fields = if main_fields.iter().any(|f| f == "browser") {
            vec![vec!["browser".to_string()]]
        } else {
            Vec::new()
        };
        let options = ResolveOptions {
            extensions: settings.extensions.unwrap_or_else(default_extensions),
            main_fields,
            alias_fields,
            condition_names: settings.conditions,
            alias,
            extension_alias: default_extension_alias(),
            symlinks: !settings.preserve_symlinks,
            tsconfig: tsconfig.is_file().then_some({
                TsconfigDiscovery::Manual(TsconfigOptions {
                    config_file: tsconfig,
                    references: TsconfigReferences::Auto,
                })
            }),
            ..ResolveOptions::default()
        };
        Self {
            inner: Resolver::new(options),
            root: root.to_path_buf(),
            dedupe: settings.dedupe,
        }
    }

    /// The resolver for `require()` specifiers: Vite's `getConditions` pushes
    /// `require` instead of `import` when resolving for a requirer (`isRequire`),
    /// so a dual package's `exports` map hands its CommonJS build to a CJS dep
    /// that requires it and its ESM build to an importer. Everything else
    /// (aliases, extensions, dedupe, tsconfig paths) is the same, and the fs
    /// cache is shared.
    pub fn require_variant(&self) -> Self {
        let mut options = self.inner.options().clone();
        for c in options.condition_names.iter_mut() {
            if c == "import" {
                *c = "require".to_string();
            }
        }
        Self {
            inner: self.inner.clone_with_options(options),
            root: self.root.clone(),
            dedupe: self.dedupe.clone(),
        }
    }

    /// A bare import of a `resolve.dedupe` package resolves from the project
    /// root so nested / monorepo copies collapse to one instance (Vite parity).
    fn should_dedupe(&self, specifier: &str) -> bool {
        if self.dedupe.is_empty() || specifier.starts_with('.') || specifier.starts_with('/') {
            return false;
        }
        let pkg = package_name(specifier);
        self.dedupe.iter().any(|d| d == &pkg)
    }

    /// Drop the resolver's file system cache. A lookup that failed is cached
    /// like a hit, so a file or directory created after the miss (an import
    /// written before its module exists) stays unresolvable until this runs.
    pub fn clear_cache(&self) {
        self.inner.clear_cache();
    }

    pub fn resolve(&self, importer_dir: &Path, specifier: &str) -> Result<PathBuf, ResolveFailure> {
        let deduped = self.should_dedupe(specifier);
        let base = if deduped {
            self.root.as_path()
        } else {
            importer_dir
        };
        match self.inner.resolve(base, specifier) {
            Ok(resolution) => Ok(resolution.full_path()),
            Err(err) => {
                // Dedupe from root can miss a package only installed nested;
                // fall back to the importer's dir before failing.
                if deduped {
                    if let Ok(resolution) = self.inner.resolve(importer_dir, specifier) {
                        return Ok(resolution.full_path());
                    }
                }
                Err(ResolveFailure {
                    specifier: specifier.to_string(),
                    importer: importer_dir.to_path_buf(),
                    reason: err.to_string(),
                    ignored: err.is_ignore(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playground_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../playground")
    }
    fn playground_src() -> PathBuf {
        playground_root().join("src")
    }

    #[test]
    fn resolves_relative_import_with_tsx_extension_probing() {
        let resolver = OjResolver::new(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "./App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "got {resolved:?}");
    }

    #[test]
    fn resolves_tsconfig_paths_alias() {
        let resolver = OjResolver::new(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "@/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias @/App -> {resolved:?}");
    }

    #[test]
    fn resolves_config_alias() {
        let resolver = OjResolver::with_options(
            &playground_root(),
            &["browser", "import", "default"].map(String::from),
            &[("~".to_string(), "./src".to_string())],
            &[],
        );
        let resolved = resolver.resolve(&playground_root(), "~/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias ~/App -> {resolved:?}");
    }

    #[test]
    fn dedupe_collapses_nested_copy_to_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dedupe");
        let nested_dir = root.join("pkg");
        let conds = ["import", "default"].map(String::from);
        // Without dedupe: the importer's nested copy wins.
        let plain = OjResolver::with_options(&root, &conds, &[], &[]);
        let n = plain.resolve(&nested_dir, "dep").unwrap();
        assert!(
            n.to_string_lossy().contains("pkg/node_modules/dep"),
            "without dedupe expected nested copy, got {n:?}",
        );
        // With dedupe: collapses to the root copy.
        let deduped = OjResolver::with_options(&root, &conds, &[], &["dep".to_string()]);
        let r = deduped.resolve(&nested_dir, "dep").unwrap();
        assert!(
            !r.to_string_lossy().contains("pkg/node_modules/dep")
                && r.to_string_lossy().contains("dedupe/node_modules/dep"),
            "with dedupe expected the root copy, got {r:?}",
        );
        // Subpaths of a deduped package dedupe too.
        let deduped2 = OjResolver::with_options(&root, &conds, &[], &["dep".to_string()]);
        assert!(deduped2.resolve(&nested_dir, "dep").is_ok());
    }

    #[test]
    fn remaps_ts_output_extensions_for_every_fs_path() {
        // Vite's tryCleanFsResolve: a `.js`/`.jsx`/`.mjs`/`.cjs` import with no
        // file on disk resolves to its TS source, for relative, aliased and
        // tsconfig-paths imports alike; an existing `.js` still wins over `.ts`.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tsout");
        let src = root.join("src/lib");
        let r = OjResolver::with_options(
            &root,
            &["import", "default"].map(String::from),
            &[("~".to_string(), "./src".to_string())],
            &[],
        );
        let ends = |spec: &str, suffix: &str| {
            let p = r
                .resolve(&src, spec)
                .unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert!(p.ends_with(suffix), "{spec} -> {p:?}, want *{suffix}");
        };
        ends("../utils/a.js", "utils/a.ts");
        ends("@/utils/a.js", "utils/a.ts");
        ends("~/utils/a.js", "utils/a.ts");
        ends("@/utils/comp.js", "utils/comp.tsx");
        ends("@/utils/j.jsx", "utils/j.tsx");
        ends("@/utils/m.mjs", "utils/m.mts");
        ends("@/utils/c.cjs", "utils/c.cts");
        ends("@/utils/both.js", "utils/both.js");
        ends("@/utils/a", "utils/a.ts");
        assert!(r.resolve(&src, "@/utils/missing.js").is_err());
    }

    #[test]
    fn reports_unresolvable_specifier() {
        let resolver = OjResolver::new(&playground_root());
        let err = resolver
            .resolve(&playground_src(), "./does-not-exist")
            .unwrap_err();
        assert_eq!(err.specifier, "./does-not-exist");
    }

    #[test]
    fn resolves_exports_per_condition() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dual");
        let browser =
            OjResolver::with_conditions(&dir, &["browser", "import", "default"].map(String::from));
        let node =
            OjResolver::with_conditions(&dir, &["node", "import", "default"].map(String::from));
        assert!(browser
            .resolve(&dir, "dual-pkg")
            .unwrap()
            .ends_with("browser.js"));
        assert!(node.resolve(&dir, "dual-pkg").unwrap().ends_with("node.js"));
    }

    #[test]
    fn require_variant_swaps_the_import_condition_for_require() {
        // Vite getConditions: a require() resolves with `require`, not `import`,
        // so a dual package's exports map picks its CJS build for a requirer.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dual");
        let importer = OjResolver::with_conditions(
            &dir,
            &["browser", "import", "development", "default"].map(String::from),
        );
        let requirer = importer.require_variant();
        assert!(importer
            .resolve(&dir, "esm-cjs")
            .unwrap()
            .ends_with("esm.mjs"));
        assert!(requirer
            .resolve(&dir, "esm-cjs")
            .unwrap()
            .ends_with("cjs.cjs"));
        // Conditions other than import are untouched (browser still wins).
        assert!(requirer
            .resolve(&dir, "dual-pkg")
            .unwrap()
            .ends_with("browser.js"));
    }

    #[test]
    fn default_condition_resolves_the_fallback_export() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dual");
        let resolver = OjResolver::with_conditions(&dir, &["default".to_string()]);
        assert!(resolver
            .resolve(&dir, "dual-pkg")
            .unwrap()
            .ends_with("default.js"));
    }

    #[test]
    fn server_resolver_ignores_the_browser_field_like_vite_ssr() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainfields");
        let server = OjResolver::with_settings(
            &dir,
            ResolveSettings {
                conditions: ["node", "import", "default"].map(String::from).to_vec(),
                server: true,
                ..ResolveSettings::default()
            },
        );
        // DEFAULT_SERVER_MAIN_FIELDS has no `browser`, so the `browser` object
        // remap (./node.js -> ./browser.js) does not apply on the server.
        assert!(
            server.resolve(&dir, "br-pkg").unwrap().ends_with("node.js"),
            "ssr keeps the node build: {:?}",
            server.resolve(&dir, "br-pkg")
        );
        assert!(
            server.resolve(&dir, "mf-pkg").unwrap().ends_with("esm.js"),
            "module still leads"
        );
        // Naming `browser` in a server mainFields opts back in (Vite: mapping
        // applies whenever the effective mainFields include it).
        let opted_in = OjResolver::with_settings(
            &dir,
            ResolveSettings {
                conditions: ["node", "import", "default"].map(String::from).to_vec(),
                main_fields: Some(["browser", "module", "main"].map(String::from).to_vec()),
                server: true,
                ..ResolveSettings::default()
            },
        );
        assert!(opted_in
            .resolve(&dir, "br-pkg")
            .unwrap()
            .ends_with("browser.js"));
        assert_eq!(
            default_server_main_fields(),
            ["module", "jsnext:main", "jsnext", "main"].map(String::from)
        );
    }

    #[test]
    fn prefers_module_field_and_browser_object_remap() {
        // Vite default mainFields: an ESM-only package (module field, no exports)
        // must resolve to its module entry, not fall back to CJS main; and the
        // package.json `browser` object must remap the resolved file.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainfields");
        let r = OjResolver::new(&dir);
        assert!(
            r.resolve(&dir, "mf-pkg").unwrap().ends_with("esm.js"),
            "module field must win over main: {:?}",
            r.resolve(&dir, "mf-pkg")
        );
        assert!(
            r.resolve(&dir, "br-pkg").unwrap().ends_with("browser.js"),
            "browser object must remap main: {:?}",
            r.resolve(&dir, "br-pkg")
        );
    }

    #[test]
    fn resolves_package_subpath_exports_and_enforces_encapsulation() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/subpath");
        let resolver = OjResolver::new(&dir);
        assert!(resolver
            .resolve(&dir, "sub-pkg")
            .unwrap()
            .ends_with("index.js"));
        assert!(resolver
            .resolve(&dir, "sub-pkg/feature")
            .unwrap()
            .ends_with("feature.js"));
        assert!(
            resolver.resolve(&dir, "sub-pkg/internal").is_err(),
            "unlisted subpath must not resolve"
        );
    }

    #[test]
    fn resolves_json_css_and_explicit_extensions() {
        let resolver = OjResolver::new(&playground_root());
        let src = playground_src();
        assert!(resolver
            .resolve(&src, "./data.json")
            .unwrap()
            .ends_with("data.json"));
        assert!(resolver
            .resolve(&src, "./App.tsx")
            .unwrap()
            .ends_with("App.tsx"));
        assert!(
            resolver
                .resolve(&src, "./Counter.module.css")
                .unwrap()
                .ends_with("Counter.module.css"),
            "exact-path .css should resolve",
        );
    }

    #[test]
    fn default_extensions_probe_in_vite_order() {
        // Vite's DEFAULT_EXTENSIONS: .js before .ts, and .mts is probed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("both.js"), "export const js = 1;").unwrap();
        std::fs::write(root.join("both.ts"), "export const ts = 1;").unwrap();
        std::fs::write(root.join("modern.mts"), "export const m = 1;").unwrap();
        std::fs::write(root.join("main.ts"), "").unwrap();
        let resolver = OjResolver::new(root);
        assert!(
            resolver
                .resolve(root, "./both")
                .unwrap()
                .ends_with("both.js"),
            ".js wins over a sibling .ts as in Vite",
        );
        assert!(
            resolver
                .resolve(root, "./modern")
                .unwrap()
                .ends_with("modern.mts"),
            ".mts is in the default probe list",
        );
        assert_eq!(
            default_extensions(),
            [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".json"].map(String::from)
        );
    }

    #[test]
    fn honors_custom_resolve_extensions() {
        // resolve.extensions replaces the default probe list (Vite semantics):
        // a `.vue` file is only reachable once the caller supplies the extension.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/extensions");
        let default = OjResolver::new(&dir);
        assert!(
            default.resolve(&dir, "./Widget").is_err(),
            "default extensions must not resolve a .vue file",
        );
        let custom = OjResolver::with_settings(
            &dir,
            ResolveSettings {
                conditions: ["import", "default"].map(String::from).to_vec(),
                extensions: Some(vec![".vue".to_string()]),
                ..ResolveSettings::default()
            },
        );
        assert!(
            custom
                .resolve(&dir, "./Widget")
                .unwrap()
                .ends_with("Widget.vue"),
            "custom .vue extension should resolve",
        );
    }

    #[test]
    fn honors_main_fields_override() {
        // mf-pkg ships both `module` (esm.js) and `main` (cjs.js). The default
        // ordering prefers module; forcing mainFields:["main"] must pick main.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainfields");
        let default = OjResolver::new(&dir);
        assert!(
            default.resolve(&dir, "mf-pkg").unwrap().ends_with("esm.js"),
            "default mainFields prefers module",
        );
        let main_first = OjResolver::with_settings(
            &dir,
            ResolveSettings {
                conditions: ["import", "default"].map(String::from).to_vec(),
                main_fields: Some(vec!["main".to_string()]),
                ..ResolveSettings::default()
            },
        );
        assert!(
            main_first
                .resolve(&dir, "mf-pkg")
                .unwrap()
                .ends_with("cjs.js"),
            "mainFields:[main] must pick the main entry",
        );
    }

    // Vite's resolvePackageEntry always falls back to `pkg.main` after the
    // mainFields walk, so its DEFAULT_MAIN_FIELDS list omits "main" entirely.
    // Adopting that list verbatim into oxc_resolver (no such fallback) made a
    // package whose ONLY entry is `main` unresolvable — e.g. a linked
    // workspace package with `"main": "src/x.ts"` and no index file.
    #[test]
    fn a_vite_shaped_main_fields_list_still_falls_back_to_main() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pkg = root.join("node_modules/@acme/parser");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@acme/parser","main":"src/parse.ts"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("src/parse.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        // Vite's DEFAULT_MAIN_FIELDS, as the extractor adopts them.
        let vite_shaped = OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: ["import", "default"].map(String::from).to_vec(),
                main_fields: Some(
                    ["browser", "module", "jsnext:main", "jsnext"]
                        .map(String::from)
                        .to_vec(),
                ),
                ..ResolveSettings::default()
            },
        );
        assert!(
            vite_shaped
                .resolve(root, "@acme/parser")
                .unwrap()
                .ends_with("parse.ts"),
            "a main-only package must resolve under Vite's main-less mainFields",
        );
        // The fallback is LAST: a list preferring `module` still picks it over main.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainfields");
        let module_first = OjResolver::with_settings(
            &dir,
            ResolveSettings {
                conditions: ["import", "default"].map(String::from).to_vec(),
                main_fields: Some(vec!["module".to_string()]),
                ..ResolveSettings::default()
            },
        );
        assert!(
            module_first
                .resolve(&dir, "mf-pkg")
                .unwrap()
                .ends_with("esm.js"),
            "the appended main fallback must not outrank the user's fields",
        );
    }

    // The shared transform every non-fallback resolver (oxc, rolldown, the
    // optimizer sidecar) applies to an adopted mainFields list.
    #[test]
    fn with_main_fallback_appends_main_last_and_dedups() {
        assert_eq!(
            with_main_fallback(["browser", "module"].map(String::from).to_vec()),
            ["browser", "module", "main"].map(String::from)
        );
        // Already present (anywhere): the list is untouched — the fallback
        // never outranks or duplicates a configured "main".
        assert_eq!(
            with_main_fallback(["main", "module"].map(String::from).to_vec()),
            ["main", "module"].map(String::from)
        );
        assert_eq!(with_main_fallback(Vec::new()), ["main".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn preserve_symlinks_controls_realpath() {
        use std::os::unix::fs::symlink;
        // A linked package dir: real files at `real/`, imported through `link/`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("index.js"), "export default 1;\n").unwrap();
        symlink(&real, root.join("link")).unwrap();
        let conds = ["import", "default"].map(String::from).to_vec();

        // Default follows symlinks: the resolved path lands in the real dir.
        let resolved = OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: conds.clone(),
                ..ResolveSettings::default()
            },
        )
        .resolve(root, "./link/index.js")
        .unwrap();
        assert!(
            resolved.starts_with(std::fs::canonicalize(&real).unwrap()),
            "default realpaths through the symlink: {resolved:?}",
        );

        // preserveSymlinks keeps the symlink location instead of realpathing.
        let preserved = OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: conds,
                preserve_symlinks: true,
                ..ResolveSettings::default()
            },
        )
        .resolve(root, "./link/index.js")
        .unwrap();
        assert!(
            preserved.to_string_lossy().contains("link"),
            "preserveSymlinks keeps the symlink path: {preserved:?}",
        );
    }
}
