// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Module resolution for oj: oxc_resolver configured the way Vite resolves
//! (extensions, mainFields, conditions, aliases, tsconfig paths), plus the Vite
//! behaviors oxc does not have (dedupe, exports-first directory entries).

pub mod alias;
mod defaults;
mod exports;
mod settings;
mod spec;
mod tsconfig_paths;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};

pub use defaults::{
    default_extension_alias, default_extensions, default_main_fields, default_server_main_fields,
    rolldown_conditions, with_main_fallback,
};
use exports::{directory_entry, DirectoryEntry};
pub use settings::ResolveSettings;
use spec::package_name;
pub use tsconfig_paths::TsconfigPaths;

pub struct OjResolver {
    /// oxc with tsconfig `paths`, without `resolve.alias` (applied first,
    /// by `resolve`).
    inner: Resolver,
    /// `inner` without tsconfig `paths`: plain resolution (Vite's dev
    /// server with no `paths` step, `tryNodeResolve`).
    no_paths: Resolver,
    /// Finds and parses the importer's nearest tsconfig, references
    /// included, for the mapping rolldown's own resolver makes.
    tsconfigs: Resolver,
    /// Vite 8's own resolver with `resolve.tsconfigPaths`: the tsconfig that
    /// owns each importer (or the one Vite's `tsconfig` option names).
    native: Resolver,
    /// How the app's Vite applies tsconfig `paths`.
    tsconfig_paths: TsconfigPaths,
    /// `resolve.alias` as `(find, replacement)`, in the written order. They
    /// go before tsconfig `paths`: Vite's alias plugin runs ahead of every
    /// resolver plugin.
    aliases: Vec<(String, String)>,
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
        let options = settings::resolve_options(root, &settings);
        let inner = Resolver::new(options.clone());
        let no_paths = inner.clone_with_options(ResolveOptions {
            tsconfig: None,
            ..options.clone()
        });
        let tsconfigs = inner.clone_with_options(ResolveOptions {
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..options.clone()
        });
        let native = inner.clone_with_options(ResolveOptions {
            tsconfig: Some(settings::native_tsconfig(&settings)),
            ..options
        });
        Self {
            inner,
            no_paths,
            tsconfigs,
            native,
            aliases: settings::aliases(root, &settings.alias),
            tsconfig_paths: settings.tsconfig_paths,
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
        let no_paths = self.inner.clone_with_options(ResolveOptions {
            tsconfig: None,
            ..options.clone()
        });
        let tsconfigs = self.tsconfigs.clone_with_options(ResolveOptions {
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..options.clone()
        });
        let native = self.native.clone_with_options(ResolveOptions {
            tsconfig: self.native.options().tsconfig.clone(),
            ..options.clone()
        });
        Self {
            inner: self.inner.clone_with_options(options),
            no_paths,
            tsconfigs,
            native,
            aliases: self.aliases.clone(),
            tsconfig_paths: self.tsconfig_paths,
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

    /// The first `resolve.alias` entry's rewrite of `specifier`, in the
    /// written order. A find takes the whole specifier or a prefix up to a
    /// `/`, as Vite's alias plugin matches (`matches` in @rollup/plugin-alias).
    ///
    /// An entry skips a specifier that already is its replacement or lies
    /// under it, as oxc's alias does: the plugin host applies the alias
    /// before asking this resolver, so `{ process: "process/browser" }` would
    /// otherwise turn its `process/browser` into `process/browser/browser`
    /// (Vite's alias plugin resolves its rewrite with `skipSelf`).
    fn alias(&self, specifier: &str) -> Option<String> {
        self.aliases.iter().find_map(|(find, replacement)| {
            let under = |base: &str| {
                specifier
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            };
            if alias::regex_parts(find).is_none() && under(replacement.trim_end_matches('/')) {
                return None;
            }
            alias::rewrite(find, replacement, specifier)
        })
    }

    /// `specifier` from `importer_dir`. A `resolve.alias` entry rewrites it
    /// first, as Vite's alias plugin runs ahead of tsconfig `paths`; the
    /// rewritten id then resolves with `paths` on, as the alias plugin hands
    /// it to the rest of the pipeline (`this.resolve(updatedId, …, { skipSelf })`).
    /// tsconfig `paths` apply as Vite's own resolver applies them
    /// ([`TsconfigPaths`]); vite-tsconfig-paths answers in the plugin host
    /// before an id reaches oj.
    pub fn resolve(&self, importer_dir: &Path, specifier: &str) -> Result<PathBuf, ResolveFailure> {
        self.resolve_at(importer_dir, None, specifier)
    }

    /// [`Self::resolve`] from the importing file, whose owning tsconfig
    /// gives the `paths` with `resolve.tsconfigPaths`, as in Vite.
    pub fn resolve_from(
        &self,
        importer: &Path,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        let file = clean_importer(importer);
        let dir = file.parent().unwrap_or(&self.root);
        self.resolve_at(dir, Some(file), specifier)
    }

    /// `specifier` as Node resolution finds it: no `resolve.alias`, no
    /// tsconfig `paths` (Vite's `tryNodeResolve`, what its SSR
    /// externalization check uses).
    pub fn resolve_plain(
        &self,
        importer_dir: &Path,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        self.resolve_with(&self.no_paths, importer_dir, specifier)
    }

    fn resolve_at(
        &self,
        importer_dir: &Path,
        importer: Option<&Path>,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        let Some(aliased) = self.alias(specifier) else {
            return self.resolve_unaliased(importer_dir, importer, specifier);
        };
        self.resolve_unaliased(importer_dir, importer, &aliased)
            .map_err(|failure| ResolveFailure {
                specifier: specifier.to_string(),
                reason: format!("aliased to '{aliased}': {}", failure.reason),
                ..failure
            })
    }

    /// `specifier` past the alias step, with tsconfig `paths` as the app's
    /// Vite applies them ([`TsconfigPaths`]).
    fn resolve_unaliased(
        &self,
        importer_dir: &Path,
        importer: Option<&Path>,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        // A root tsconfig that does not parse leaves the `paths` unknown:
        // oxc's resolver fails with an error that names it, not a guess.
        if self.inner.options().tsconfig.is_some()
            && self.tsconfig_paths != TsconfigPaths::Off
            && self
                .inner
                .resolve_tsconfig(self.root.join("tsconfig.json"))
                .is_err()
        {
            return self.resolve_with(&self.inner, importer_dir, specifier);
        }
        let plain = || self.resolve_with(&self.no_paths, importer_dir, specifier);
        let native = || self.resolve_native(importer_dir, importer, specifier);
        match self.tsconfig_paths {
            // vite-tsconfig-paths runs in the plugin host, ahead of oj's
            // resolver, with its own options: what it passed on reaches
            // vite:resolve, which applies no `paths` (or Vite's own resolver's,
            // with `resolve.tsconfigPaths` too).
            TsconfigPaths::Plugin | TsconfigPaths::Neither | TsconfigPaths::Off => plain(),
            TsconfigPaths::Native | TsconfigPaths::PluginNative => native(),
        }
    }

    /// Vite's own resolver with `resolve.tsconfigPaths`: oxc with the
    /// tsconfig that owns the importer (`resolve_file`), or the one Vite's
    /// `tsconfig` option names.
    fn resolve_native(
        &self,
        importer_dir: &Path,
        importer: Option<&Path>,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        // Vite's resolver resolves from the file, Vite's `tsconfig` option
        // or not (`resolve_file`: the reference whose `include` holds it).
        if self.should_dedupe(specifier) {
            return self.resolve_with(&self.native, importer_dir, specifier);
        }
        // With only a directory, a TypeScript file in it stands in.
        let stand_in;
        let file = match importer {
            Some(file) => file,
            None => {
                stand_in = importer_dir.join("__oj_importer__.ts");
                &stand_in
            }
        };
        match self.native.resolve_file(file, specifier) {
            Ok(resolution) => {
                match directory_entry(&self.native, importer_dir, specifier, resolution.path()) {
                    DirectoryEntry::Resolved(entry) => Ok(entry),
                    DirectoryEntry::NotApplicable => Ok(resolution.full_path()),
                    DirectoryEntry::Unresolvable(_) => {
                        self.resolve_with(&self.native, importer_dir, specifier)
                    }
                }
            }
            Err(err) => Err(ResolveFailure {
                specifier: specifier.to_string(),
                importer: importer_dir.to_path_buf(),
                reason: err.to_string(),
                ignored: err.is_ignore(),
            }),
        }
    }

    /// What a bundler whose own resolver applies tsconfig `paths` natively
    /// (rolldown: the importer's nearest tsconfig, oxc's order, ahead of
    /// packages) should do instead, for the app's [`TsconfigPaths`]. It
    /// steps in only where that native mapping would answer and Vite would
    /// not take it: everything else stays the bundler's, with its own
    /// conditions and import kinds. `native_after` is whether the bundler,
    /// like `vite build`, lets its native `paths` take what Vite's resolve
    /// plugin leaves (a dev server never does). For a `require`, ask the
    /// [`Self::require_variant`].
    pub fn tsconfig_answer(
        &self,
        importer: &Path,
        specifier: &str,
        native_after: bool,
    ) -> TsconfigAnswer {
        let file = clean_importer(importer);
        let dir = file.parent().unwrap_or(&self.root);
        let id = specifier.split('?').next().unwrap_or(specifier);
        let bare = !(id.starts_with('.') || id.starts_with('/') || id.contains('\0'));
        // Natively mapped (`resolve.tsconfigPaths`): the bundler's own
        // resolver answers as Vite's. vite-tsconfig-paths answered upstream
        // (it runs in the plugin host, ahead of this), so what reaches here
        // it passed on, as with neither.
        if !bare
            || matches!(
                self.tsconfig_paths,
                TsconfigPaths::Native | TsconfigPaths::PluginNative | TsconfigPaths::Off
            )
        {
            return TsconfigAnswer::Bundler;
        }
        let native_hit = || self.native_hit(file, dir, id);
        let plain = || self.resolve_with(&self.no_paths, dir, specifier).ok();
        let leftover = || {
            if native_after {
                TsconfigAnswer::Bundler
            } else {
                TsconfigAnswer::Unresolved
            }
        };
        if native_hit().is_none() {
            return TsconfigAnswer::Bundler;
        }
        plain().map_or_else(leftover, TsconfigAnswer::File)
    }

    /// The file the bundler's own resolver would map `id` to through
    /// tsconfig `paths` or `baseUrl`: the importer's nearest tsconfig
    /// (references followed), its targets in oxc's order.
    fn native_hit(&self, file: &Path, dir: &Path, id: &str) -> Option<PathBuf> {
        let tsconfig = self.tsconfigs.find_tsconfig(file).ok()??;
        tsconfig
            .resolve_path_alias_or_base_url(id)
            .iter()
            .find_map(|c| {
                self.resolve_with(&self.no_paths, dir, &c.to_string_lossy())
                    .ok()
            })
    }

    /// An `import.meta.glob` pattern through tsconfig `paths`, as Vite's
    /// resolve plugin maps one with `resolve.tsconfigPaths` (after the
    /// aliases): the `paths` or `baseUrl` of the tsconfig that owns
    /// `importer` (or the one Vite's `tsconfig` option names), unchecked on
    /// disk, the first target (`resolve_tsconfig_path_alias`). Else none:
    /// vite-tsconfig-paths has no answer for a glob, which names no file.
    pub fn glob_mapped(&self, importer: &Path, pattern: &str) -> Option<String> {
        if !matches!(
            self.tsconfig_paths,
            TsconfigPaths::Native | TsconfigPaths::PluginNative
        ) {
            return None;
        }
        let importer = clean_importer(importer);
        let tsconfig = self.native.find_tsconfig(importer).ok()??;
        let first = tsconfig
            .resolve_path_alias_or_base_url(pattern)
            .into_iter()
            .next()?;
        Some(first.to_string_lossy().into_owned())
    }

    fn resolve_with(
        &self,
        resolver: &Resolver,
        importer_dir: &Path,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
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
        match resolver.resolve(base, specifier) {
            Ok(resolution) => {
                match directory_entry(resolver, base, specifier, resolution.path()) {
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
                    if let Ok(resolution) = resolver.resolve(importer_dir, specifier) {
                        return Ok(resolution.full_path());
                    }
                }
                Err(failure(err.to_string(), err.is_ignore()))
            }
        }
    }
}

/// An importer id as a file path: its query and hash dropped.
fn clean_importer(importer: &Path) -> &Path {
    Path::new(
        importer
            .to_str()
            .and_then(|p| p.split(['?', '#']).next())
            .unwrap_or_default(),
    )
}

/// [`OjResolver::tsconfig_answer`]'s verdict for a bundler.
#[derive(Debug, PartialEq, Eq)]
pub enum TsconfigAnswer {
    /// Resolve to this file.
    File(PathBuf),
    /// Leave the id to the bundler's own resolver.
    Bundler,
    /// Nothing answers the id, as in Vite.
    Unresolved,
}

/// Vite's searchForWorkspaceRoot: the first workspace marker walking up
/// (`pnpm-workspace.yaml`, `lerna.json`, a `package.json` with `workspaces`,
/// a `deno.json(c)` with `workspace`), else the nearest ancestor with a
/// `package.json`, else `root`. `.git` is no marker.
pub fn workspace_root(root: &Path) -> PathBuf {
    // Found once per root: each resolver a server or a build constructs asks.
    static FOUND: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
    > = std::sync::LazyLock::new(Default::default);
    if let Some(found) = FOUND.lock().unwrap_or_else(|e| e.into_inner()).get(root) {
        return found.clone();
    }
    let found = search_workspace_root(root);
    FOUND
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.to_path_buf(), found.clone());
    found
}

/// [`workspace_root`] searched again, without the per-root memo.
pub fn search_workspace_root(root: &Path) -> PathBuf {
    let mut pkg_root: Option<PathBuf> = None;
    let mut dir = root;
    loop {
        if dir.join("pnpm-workspace.yaml").exists() || dir.join("lerna.json").exists() {
            return dir.to_path_buf();
        }
        if dir.join("package.json").exists() {
            if json_field_truthy(&dir.join("package.json"), "workspaces") {
                return dir.to_path_buf();
            }
            if pkg_root.is_none() {
                pkg_root = Some(dir.to_path_buf());
            }
        }
        if json_field_truthy(&dir.join("deno.json"), "workspace")
            || json_field_truthy(&dir.join("deno.jsonc"), "workspace")
        {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return pkg_root.unwrap_or_else(|| root.to_path_buf()),
        }
    }
}

/// The parsed `field` of a JSON file, truthy by JavaScript's rules.
fn json_field_truthy(file: &Path, field: &str) -> bool {
    let Ok(txt) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return false;
    };
    match v.get(field) {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OjResolver::new` with `resolve.tsconfigPaths`: Vite's own resolver,
    /// what these tests check the `paths` against.
    fn native_with_options(
        root: &Path,
        conditions: &[String],
        alias: &[(String, String)],
        dedupe: &[String],
    ) -> OjResolver {
        OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: conditions.to_vec(),
                alias: alias.to_vec(),
                dedupe: dedupe.to_vec(),
                tsconfig_paths: TsconfigPaths::Native,
                ..ResolveSettings::default()
            },
        )
    }

    fn native_resolver(root: &Path) -> OjResolver {
        OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: ["browser", "import", "module", "development", "default"]
                    .map(String::from)
                    .to_vec(),
                tsconfig_paths: TsconfigPaths::Native,
                ..ResolveSettings::default()
            },
        )
    }

    fn playground_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../playground")
    }
    fn playground_src() -> PathBuf {
        playground_root().join("src")
    }

    #[test]
    fn resolves_relative_import_with_tsx_extension_probing() {
        let resolver = native_resolver(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "./App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "got {resolved:?}");
    }

    /// A project whose tsconfig `paths` name a declaration file for the
    /// client module, and `@/*` for the rest.
    fn supabase_project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (file, body) in [
            (
                "src/integrations/supabase/client.ts",
                "export const who = 1;",
            ),
            ("src/appBackend.ts", "export const who = 2;"),
            (
                "src/types/existingSupabaseClient.d.ts",
                "export declare const supabase: 1;",
            ),
            ("src/lib/helper.ts", "export const helper = 1;"),
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/integrations/supabase/client": ["./src/types/existingSupabaseClient.d.ts"], "@/*": ["./src/*"] } } }"#,
            ),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        tmp
    }

    fn aliased(root: &Path, alias: &[(&str, &str)]) -> OjResolver {
        let alias: Vec<(String, String)> = alias
            .iter()
            .map(|(f, r)| (f.to_string(), root.join(r).to_string_lossy().into_owned()))
            .collect();
        OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: vec!["import".to_string()],
                alias,
                tsconfig_paths: TsconfigPaths::Native,
                ..ResolveSettings::default()
            },
        )
    }

    // Vite's alias plugin runs ahead of tsconfig `paths`, and takes the first
    // entry that matches in the written order.
    #[test]
    fn an_alias_goes_before_tsconfig_paths_in_written_order() {
        let tmp = supabase_project();
        let root = tmp.path().canonicalize().unwrap();
        let src = root.join("src");
        let client = "@/integrations/supabase/client";
        let specific_first = aliased(
            &root,
            &[
                ("@/integrations/supabase/client", "src/appBackend.ts"),
                ("@", "src"),
            ],
        );
        assert_eq!(
            specific_first.resolve(&src, client).unwrap(),
            src.join("appBackend.ts")
        );
        let at_first = aliased(
            &root,
            &[
                ("@", "src"),
                ("@/integrations/supabase/client", "src/appBackend.ts"),
            ],
        );
        assert_eq!(
            at_first.resolve(&src, client).unwrap(),
            src.join("integrations/supabase/client.ts")
        );
        // A find takes the whole specifier or a prefix up to a `/`, nothing
        // else; `$` is no anchor (@rollup/plugin-alias has no such rule).
        let prefix = aliased(&root, &[("@/li", "src/appBackend.ts")]);
        assert_eq!(
            prefix.resolve(&src, "@/lib/helper").unwrap(),
            src.join("lib/helper.ts"),
            "the paths answer what no find takes"
        );
        let dollar = aliased(&root, &[("@/lib$", "src/appBackend.ts")]);
        assert_eq!(
            dollar.resolve(&src, "@/lib/helper").unwrap(),
            src.join("lib/helper.ts")
        );
    }

    // The alias plugin hands the rewritten id to the rest of the pipeline,
    // where tsconfig `paths` still map it: an alias to a `paths` name works.
    #[test]
    fn an_aliased_id_still_goes_through_tsconfig_paths() {
        let tmp = supabase_project();
        let root = tmp.path().canonicalize().unwrap();
        let src = root.join("src");
        let resolver = native_with_options(
            &root,
            &["import".to_string()],
            &[("~lib".to_string(), "@/lib".to_string())],
            &[],
        );
        assert_eq!(
            resolver.resolve(&src, "~lib/helper").unwrap(),
            src.join("lib/helper.ts")
        );
        // The rewritten id is not aliased again (`skipSelf`).
        let looped = native_with_options(
            &root,
            &["import".to_string()],
            &[
                ("~a".to_string(), "~b/helper".to_string()),
                (
                    "~b".to_string(),
                    root.join("src/lib").to_string_lossy().into_owned(),
                ),
            ],
            &[],
        );
        let failure = looped.resolve(&src, "~a").unwrap_err();
        assert_eq!(failure.specifier, "~a");
        assert!(
            failure.reason.contains("aliased to '~b/helper'"),
            "{failure}"
        );
    }

    // The plugin host applies the alias, then asks this resolver for the
    // rewritten id: an entry whose replacement lies under its own find must
    // not apply again (oxc's alias guard; Vite resolves with `skipSelf`).
    #[test]
    fn an_alias_skips_an_id_already_under_its_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let pkg = root.join("node_modules/process");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"process","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "").unwrap();
        std::fs::write(pkg.join("browser.js"), "").unwrap();
        let resolver = native_with_options(
            &root,
            &["import".to_string()],
            &[("process".to_string(), "process/browser".to_string())],
            &[],
        );
        assert_eq!(
            resolver.resolve(&root, "process").unwrap(),
            pkg.join("browser.js")
        );
        assert_eq!(
            resolver.resolve(&root, "process/browser").unwrap(),
            pkg.join("browser.js"),
            "the rewritten id resolves as is"
        );
    }

    /// A project tree from `(file, body)` pairs.
    fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (file, body) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        (tmp, root)
    }

    fn moded(root: &Path, mode: TsconfigPaths, alias: &[(String, String)]) -> OjResolver {
        OjResolver::with_settings(
            root,
            ResolveSettings {
                conditions: vec!["import".to_string()],
                alias: alias.to_vec(),
                tsconfig_paths: mode,
                ..ResolveSettings::default()
            },
        )
    }

    const EXACT_AND_STAR: &[(&str, &str)] = &[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/x": ["./src/a.ts"], "@/client": ["./src/client.d.ts"], "@/*": ["./src/*"], "events": ["./src/events.ts"] } } }"#,
        ),
        ("src/a.ts", ""),
        ("src/x.ts", ""),
        ("src/client.d.ts", ""),
        ("src/client.ts", ""),
        ("src/events.ts", ""),
        (
            "node_modules/events/package.json",
            r#"{"name":"events","main":"events.js"}"#,
        ),
        ("node_modules/events/events.js", ""),
    ];

    // How the app's Vite applies tsconfig `paths`: vite-tsconfig-paths in the
    // host (none left for oj), Vite 8's own (`resolve.tsconfigPaths`: the
    // exact key first, a declaration file served), or none in dev.
    #[test]
    fn tsconfig_paths_follow_how_the_app_turns_them_on() {
        let (_tmp, root) = tree(EXACT_AND_STAR);
        let src = root.join("src");
        // vite-tsconfig-paths answers in the plugin host, ahead of this: what
        // reaches oj it passed on, and vite:resolve applies no `paths`.
        let plugin = moded(&root, TsconfigPaths::Plugin, &[]);
        assert!(plugin.resolve(&src, "@/x").is_err());
        assert_eq!(
            plugin.resolve(&src, "events").unwrap(),
            root.join("node_modules/events/events.js")
        );

        let native = moded(&root, TsconfigPaths::Native, &[]);
        assert_eq!(native.resolve(&src, "@/x").unwrap(), src.join("a.ts"));
        assert_eq!(
            native.resolve(&src, "@/client").unwrap(),
            src.join("client.d.ts")
        );

        let neither = moded(&root, TsconfigPaths::Neither, &[]);
        assert_eq!(
            neither.resolve(&src, "events").unwrap(),
            root.join("node_modules/events/events.js"),
        );
        assert!(
            neither.resolve(&src, "@/x").is_err(),
            "Vite's dev server applies no paths"
        );

        let off = moded(&root, TsconfigPaths::Off, &[]);
        assert!(off.resolve(&src, "@/x").is_err());
    }

    // A bundler whose own resolver applies `paths` natively (oxc's order,
    // ahead of packages) takes the plain answer first with neither, or with
    // vite-tsconfig-paths (which answered upstream, in the plugin host), and
    // keeps its own with `resolve.tsconfigPaths`.
    #[test]
    fn a_bundler_takes_the_answer_the_mode_gives() {
        let (_tmp, root) = tree(EXACT_AND_STAR);
        let m = root.join("src/m.ts");
        let plugin = moded(&root, TsconfigPaths::Plugin, &[]);
        use TsconfigAnswer::*;
        assert_eq!(plugin.tsconfig_answer(&m, "@/x", false), Unresolved);
        assert_eq!(
            plugin.tsconfig_answer(&m, "events", false),
            File(root.join("node_modules/events/events.js"))
        );
        let fallback = moded(&root, TsconfigPaths::Neither, &[]);
        assert_eq!(
            fallback.tsconfig_answer(&m, "events", false),
            File(root.join("node_modules/events/events.js"))
        );
        assert_eq!(
            fallback.tsconfig_answer(&m, "@/x", false),
            Unresolved,
            "Vite's dev server applies no paths"
        );
        assert_eq!(
            fallback.tsconfig_answer(&m, "react", false),
            Bundler,
            "no pattern: the bundler's"
        );
        let native = moded(&root, TsconfigPaths::Native, &[]);
        assert_eq!(native.tsconfig_answer(&m, "@/x", false), Bundler);
        let both = moded(&root, TsconfigPaths::PluginNative, &[]);
        assert_eq!(both.tsconfig_answer(&m, "@/x", false), Bundler);
    }

    // Vite's own resolver (`resolve.tsconfigPaths`) uses the tsconfig that
    // owns the importer: Vite's template root with no files hands `src` to
    // the app project; a file no project includes gets no `paths`; a nested
    // package's own tsconfig maps its imports. Vite's `tsconfig` option
    // names the one file instead.
    #[test]
    fn native_paths_come_from_the_tsconfig_that_owns_the_importer() {
        let (_tmp, root) = tree(&[
            (
                "tsconfig.json",
                r#"{ "files": [], "references": [{ "path": "./tsconfig.node.json" }, { "path": "./tsconfig.app.json" }] }"#,
            ),
            ("tsconfig.node.json", r#"{ "include": ["vite.config.ts"] }"#),
            (
                "tsconfig.app.json",
                r#"{ "include": ["src"], "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
            ),
            ("src/a.ts", ""),
            ("src/m.ts", ""),
            ("other/m.ts", ""),
            (
                "pkg/tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "~/*": ["./lib/*"] } } }"#,
            ),
            ("pkg/lib/b.ts", ""),
            ("pkg/m.ts", ""),
        ]);
        let native = moded(&root, TsconfigPaths::Native, &[]);
        assert_eq!(
            native.resolve_from(&root.join("src/m.ts"), "@/a").unwrap(),
            root.join("src/a.ts")
        );
        assert!(native
            .resolve_from(&root.join("other/m.ts"), "@/a")
            .is_err());
        assert_eq!(
            native.resolve_from(&root.join("pkg/m.ts"), "~/b").unwrap(),
            root.join("pkg/lib/b.ts")
        );
        let named = OjResolver::with_settings(
            &root,
            ResolveSettings {
                conditions: vec!["import".to_string()],
                tsconfig_paths: TsconfigPaths::Native,
                tsconfig_file: Some(root.join("tsconfig.app.json")),
                ..ResolveSettings::default()
            },
        );
        assert_eq!(
            named.resolve_from(&root.join("pkg/m.ts"), "@/a").unwrap(),
            root.join("src/a.ts")
        );
        // A glob maps through the importer's own tsconfig, its first target.
        assert_eq!(
            native.glob_mapped(&root.join("src/m.ts"), "@/pages/*.ts"),
            Some(root.join("src/pages/*.ts").to_string_lossy().into_owned())
        );
        assert_eq!(
            native.glob_mapped(&root.join("pkg/m.ts"), "~/views/*.ts"),
            Some(
                root.join("pkg/lib/views/*.ts")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(
            native.glob_mapped(&root.join("pkg/m.ts"), "@/pages/*.ts"),
            None
        );
        assert_eq!(
            named.glob_mapped(&root.join("pkg/m.ts"), "@/pages/*.ts"),
            Some(root.join("src/pages/*.ts").to_string_lossy().into_owned()),
            "Vite's `tsconfig` option names the one file"
        );
    }

    // A bundler that lets its native `paths` take Vite's leftovers (a build)
    // gets `Bundler` where a dev server's gets `Unresolved`; a `paths` key
    // named like a package leaves the package in every mode but Native.
    #[test]
    fn a_build_bundler_keeps_its_native_paths_for_leftovers() {
        let (_tmp, root) = tree(EXACT_AND_STAR);
        let m = root.join("src/m.ts");
        let fallback = moded(&root, TsconfigPaths::Neither, &[]);
        assert_eq!(
            fallback.tsconfig_answer(&m, "events", true),
            TsconfigAnswer::File(root.join("node_modules/events/events.js"))
        );
        assert_eq!(
            fallback.tsconfig_answer(&root.join("src/m.js"), "@/nothing", true),
            TsconfigAnswer::Bundler
        );
        assert_eq!(
            fallback.tsconfig_answer(&root.join("src/m.js"), "@/nothing", false),
            TsconfigAnswer::Bundler
        );
        assert_eq!(
            fallback.tsconfig_answer(&m, "@/x", true),
            TsconfigAnswer::Bundler,
            "what plain resolution leaves goes to the build's native order"
        );
    }

    // With `baseUrl` (the shadcn template's `"."`), a package import and a
    // Node builtin stay the bundler's: only an id the native mapping or the
    // plugin actually resolves is taken, so the bundler keeps its own
    // conditions and import kinds.
    #[test]
    fn a_base_url_leaves_packages_and_builtins_to_the_bundler() {
        let (_tmp, root) = tree(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#,
            ),
            ("src/a.ts", ""),
            ("src/m.ts", ""),
            (
                "node_modules/clsx/package.json",
                r#"{"name":"clsx","exports":{".":{"import":"./clsx.mjs","default":"./clsx.js"}}}"#,
            ),
            ("node_modules/clsx/clsx.mjs", ""),
            ("node_modules/clsx/clsx.js", ""),
        ]);
        let m = root.join("src/m.ts");
        for mode in [TsconfigPaths::Plugin, TsconfigPaths::Neither] {
            let resolver = moded(&root, mode, &[]);
            for native_after in [false, true] {
                assert_eq!(
                    resolver.tsconfig_answer(&m, "clsx", native_after),
                    TsconfigAnswer::Bundler,
                    "{mode:?}"
                );
                assert_eq!(
                    resolver.tsconfig_answer(&m, "path", native_after),
                    TsconfigAnswer::Bundler,
                    "{mode:?}"
                );
            }
        }
    }

    // Vite's resolver resolves from the importing file with Vite's `tsconfig`
    // option too: the reference whose `include` holds it maps it.
    #[test]
    fn the_tsconfig_option_still_picks_the_reference_that_owns_the_importer() {
        let (_tmp, root) = tree(&[
            (
                "tsconfig.json",
                r#"{ "files": [], "references": [{ "path": "./tsconfig.node.json" }, { "path": "./tsconfig.app.json" }] }"#,
            ),
            ("tsconfig.node.json", r#"{ "include": ["vite.config.ts"] }"#),
            (
                "tsconfig.app.json",
                r#"{ "include": ["src"], "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
            ),
            ("src/a.ts", ""),
            ("src/m.ts", ""),
        ]);
        let resolver = OjResolver::with_settings(
            &root,
            ResolveSettings {
                conditions: vec!["import".to_string()],
                tsconfig_paths: TsconfigPaths::Native,
                tsconfig_file: Some(root.join("tsconfig.json")),
                ..ResolveSettings::default()
            },
        );
        assert_eq!(
            resolver
                .resolve_from(&root.join("src/m.ts"), "@/a")
                .unwrap(),
            root.join("src/a.ts")
        );
    }

    #[test]
    fn resolves_tsconfig_paths_alias() {
        let resolver = native_resolver(&playground_root());
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
        let r = native_with_options(
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
        let resolver = native_resolver(&playground_root());
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
    fn solid_condition_selects_transformable_source_over_compiled_import() {
        // vite-plugin-solid puts `solid` first in resolve.conditions (pinned
        // at the config-vector level by merge_keeps_solid_first_resolve_conditions
        // in oj_server): a dep with `solid` -> transformable source and
        // `import` -> precompiled output must resolve to the source under the
        // observed solid-first list, and to the compiled output under generic
        // import conditions. The resolved files — not vector order — are the
        // claim (Node matches exports-map keys in map source order against
        // the condition set, so the fixture lists `solid` first like real
        // Solid packages do).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/solid-conditions");
        let solid = OjResolver::with_conditions(
            &dir,
            &[
                "solid",
                "development",
                "module",
                "browser",
                "import",
                "default",
            ]
            .map(String::from),
        );
        let hit = solid.resolve(&dir, "solid-pkg").unwrap();
        assert!(
            hit.ends_with("source.jsx"),
            "solid conditions must pick the transformable source, got {hit:?}"
        );
        // A queried Solid import keeps its plugin identity on the Solid file.
        let queried = solid
            .resolve(&dir, "solid-pkg?tsr-split=component")
            .unwrap();
        let queried = queried.to_string_lossy();
        assert!(
            queried.contains("source.jsx") && queried.contains("?tsr-split=component"),
            "queried solid import must keep source + query, got {queried:?}"
        );
        // Generic client conditions (no `solid`) take the precompiled output.
        let generic = OjResolver::with_conditions(
            &dir,
            &["browser", "import", "module", "development", "default"].map(String::from),
        );
        let hit = generic.resolve(&dir, "solid-pkg").unwrap();
        assert!(
            hit.ends_with("compiled.js"),
            "generic import conditions must pick the compiled output, got {hit:?}"
        );
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
        let r = native_resolver(&dir);
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
        let resolver = native_resolver(&dir);
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
        let resolver = native_resolver(&playground_root());
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
        let resolver = native_resolver(root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let resolver = native_resolver(&root);
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
        let default = native_resolver(&dir);
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
        let default = native_resolver(&dir);
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
