// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Module resolution for oj: oxc_resolver configured the way Vite resolves
//! (extensions, mainFields, conditions, aliases, tsconfig paths), plus the Vite
//! behaviors oxc does not have (dedupe, exports-first directory entries).

mod defaults;
mod exports;
mod settings;
mod spec;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oxc_resolver::Resolver;

pub use defaults::{
    default_extension_alias, default_extensions, default_main_fields, default_server_main_fields,
    with_main_fallback,
};
use exports::{directory_entry, DirectoryEntry};
pub use settings::ResolveSettings;
use spec::package_name;

pub struct OjResolver {
    inner: Resolver,
    root: PathBuf,
    /// `resolve.dedupe` package names.
    dedupe: HashSet<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("cannot resolve '{specifier}' from '{importer}': {reason}")]
pub struct ResolveFailure {
    pub specifier: String,
    pub importer: PathBuf,
    pub reason: String,
    /// The package maps this specifier to `false` (a `browser` field entry or a
    /// `replacement: false` alias): serve an empty stub, not a 404.
    pub ignored: bool,
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
        Self {
            inner: Resolver::new(settings::resolve_options(root, &settings)),
            root: root.to_path_buf(),
            dedupe: settings.dedupe.into_iter().collect(),
        }
    }

    /// The resolver for `require()` specifiers: Vite's `getConditions` swaps
    /// `import` for `require` for a requirer, so a dual package hands its CJS
    /// build to a CJS dep. Everything else, the fs cache included, is shared.
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

    /// A bare import of a `resolve.dedupe` package resolves from the root so
    /// nested copies collapse to one instance (Vite parity).
    fn should_dedupe(&self, specifier: &str) -> bool {
        !self.dedupe.is_empty()
            && !specifier.starts_with('.')
            && !specifier.starts_with('/')
            && self.dedupe.contains(package_name(specifier))
    }

    /// Drops the fs cache. Misses are cached like hits, so a file created after
    /// a failed lookup stays unresolvable until this runs.
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
        let failure = |reason: String, ignored: bool| ResolveFailure {
            specifier: specifier.to_string(),
            importer: importer_dir.to_path_buf(),
            reason,
            ignored,
        };
        match self.inner.resolve(base, specifier) {
            Ok(resolution) => {
                match directory_entry(&self.inner, base, specifier, resolution.path()) {
                    DirectoryEntry::Resolved(entry) => Ok(entry),
                    DirectoryEntry::NotApplicable => Ok(resolution.full_path()),
                    DirectoryEntry::Unresolvable(dir) => Err(failure(
                        format!(
                            "failed to resolve entry for package '{}': its exports name a missing file and no index exists",
                            dir.display()
                        ),
                        false,
                    )),
                }
            }
            Err(err) => {
                // A deduped package installed only nested: retry from the importer.
                if deduped {
                    if let Ok(resolution) = self.inner.resolve(importer_dir, specifier) {
                        return Ok(resolution.full_path());
                    }
                }
                Err(failure(err.to_string(), err.is_ignore()))
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
    fn dep_relative_extensionless_prefers_js_over_stray_ts() {
        // The Start host finishes a dependency's extensionless relative import
        // through this resolver (crates/oj/src/start_host.rs, node_modules
        // branch): a published .js build must outrank a stray .ts source
        // shipped next to it, per the default extension order.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./greet").unwrap();
        assert!(
            hit.ends_with("greet.js"),
            "expected the .js build, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_resolves_nested_package_entry() {
        // Same seam: a relative directory import inside a dependency whose
        // subdir carries its own package.json (no index.*) resolves through
        // that manifest, module over main — Vite's tryCleanFsResolve consults
        // the directory package before probing index files.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./helpers").unwrap();
        assert!(
            hit.ends_with("helpers/m.js"),
            "expected the module entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_map_beats_main_fields() {
        // Vite's resolvePackageEntry consults `exports["."]` before the
        // mainFields walk even for a path-reached directory, where Node binds
        // `exports` only at the package-name boundary; directory_exports_entry
        // carries the Vite rule over the inner resolver's Node behavior.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./exports-dir").unwrap();
        assert!(
            hit.ends_with("exports-dir/e.js"),
            "expected the exports entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_without_entry_fields_falls_back_to_index() {
        // Vite's resolvePackageEntry defaults to index.js/json/node when the
        // manifest names no entry; Node rejects the directory import outright.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./bare-dir").unwrap();
        assert!(
            hit.ends_with("bare-dir/index.js"),
            "expected index fallback, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_under_server_settings() {
        // Same exports-beats-mainFields contract, but through the exact
        // settings shape the SSR server builds (server list, symlinks
        // followed), so a server-only regression cannot hide behind the
        // default-constructor test above.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::with_settings(
            &root,
            ResolveSettings {
                conditions: ["module", "node", "development", "import", "default"]
                    .map(String::from)
                    .to_vec(),
                server: true,
                ..ResolveSettings::default()
            },
        );
        let hit = resolver.resolve(&dir, "./exports-dir").unwrap();
        assert!(
            hit.ends_with("exports-dir/e.js"),
            "expected the exports entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_broken_exports_falls_to_index_not_main_fields() {
        // Vite: once exports names an entry, mainFields never run — a target
        // missing on disk throws in resolvePackageEntry and tryCleanFsResolve
        // falls to index probing.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./broken-exports-idx").unwrap();
        assert!(
            hit.ends_with("broken-exports-idx/index.js"),
            "expected the index fallback, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_broken_exports_without_index_fails() {
        // ... and with no index either, the resolution fails as under Vite,
        // instead of quietly using the module/main pick exports superseded.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let err = resolver
            .resolve(&dir, "./broken-exports-noidx")
            .unwrap_err();
        assert!(
            err.reason.contains("exports name a missing file"),
            "expected the package-entry failure, got {err:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_only_manifest_resolves() {
        // A manifest with exports and neither entry fields nor an index fails
        // the inner resolver's Node algorithm outright; Vite still enters it
        // through exports["."].
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./exports-only").unwrap();
        assert!(
            hit.ends_with("exports-only/e.js"),
            "expected the exports entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_string_sugar() {
        // Node's exports sugar: a bare string is the "." target
        // ("exports": "./s.js"), the commonest shape in small packages; it
        // outranks the module field like any exports entry.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./sugar-string").unwrap();
        assert!(
            hit.ends_with("sugar-string/s.js"),
            "expected the string-sugar entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_conditions_sugar() {
        // The other sugar: a top-level conditions object with no "." key is
        // itself the "." target; source order picks "import" before "default".
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./sugar-conditions").unwrap();
        assert!(
            hit.ends_with("sugar-conditions/i.js"),
            "expected the import-condition entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_without_matching_condition_falls_to_index() {
        // resolve.exports THROWS on a truthy exports field with no derivable
        // "." target ('No known conditions'), and resolvePackageEntry turns
        // any throw into packageEntryFailure — mainFields never run, index
        // probing does. "require" is absent from the import-side conditions.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./nomatch-idx").unwrap();
        assert!(
            hit.ends_with("nomatch-idx/index.js"),
            "expected the index fallback, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_exports_without_matching_condition_or_index_fails() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let err = resolver.resolve(&dir, "./nomatch-noidx").unwrap_err();
        assert!(
            err.reason.contains("exports name a missing file"),
            "expected the package-entry failure, got {err:?}"
        );
    }

    #[test]
    fn dep_relative_directory_subpath_only_exports_falls_to_index_not_main_fields() {
        // resolve.exports' OTHER throw family: an exports map with only
        // subpath keys and no "." at all ('Missing "." specifier') is still a
        // truthy exports field, so mainFields never run — index probing does.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./subpath-only").unwrap();
        assert!(
            hit.ends_with("subpath-only/index.js"),
            "expected the index fallback, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_exact_extension_hit_stays_a_file_resolution() {
        // The equality gate: `./greet.js` resolves to the file it names even
        // with a same-named sibling story around it — the exact hit must never
        // be treated as a directory candidate (and skips the manifest read).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./greet.js").unwrap();
        assert!(
            hit.ends_with("dist/greet.js"),
            "expected the exact file, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_falsy_exports_uses_main_fields() {
        // Vite gates on JS truthiness (`if (data.exports)`): exports: false
        // behaves like no exports field at all.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./falsy-exports").unwrap();
        assert!(
            hit.ends_with("falsy-exports/m.js"),
            "expected the module entry, got {hit:?}"
        );
    }

    #[test]
    fn dep_relative_directory_bare_string_export_target_resolves() {
        // resolve.exports accepts any string target and Vite path.joins it,
        // so a sloppy '"exports": "lib.js"' resolves the file — it must not
        // be read as a bare package name.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deprelative/node_modules/dep/dist");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deprelative");
        let resolver = OjResolver::new(&root);
        let hit = resolver.resolve(&dir, "./bare-target").unwrap();
        assert!(
            hit.ends_with("bare-target/lib.js"),
            "expected the bare-string entry, got {hit:?}"
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
