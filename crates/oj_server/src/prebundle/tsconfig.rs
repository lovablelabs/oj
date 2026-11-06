// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The aliases the scan and the pre-bundle apply to bare ids ahead of
//! rolldown's resolver: the config's `resolve.alias`, then tsconfig `paths`
//! the way the app's Vite applies them (see `oj_resolver::TsconfigPaths`), so
//! a dep reached through an alias resolves to the file the dev server serves.
//! rolldown's own resolver applies `paths` natively (oxc's order, ahead of
//! packages): that is the answer for `resolve.tsconfigPaths`, and the one
//! the other modes override.

use std::path::Path;

use crate::optimize::OptimizeInput;

/// An id nothing answers.
pub(super) struct Unresolved;

pub(super) struct Aliases {
    config: Vec<(String, String)>,
    /// The aliases a glob pattern goes through: the config's, then the
    /// tsconfig `paths` Vite's resolve plugin maps it by.
    glob: Vec<(String, String)>,
    resolver: oj_resolver::OjResolver,
}

impl Aliases {
    /// The config's `(find, replacement)` list, in the written order, and a
    /// resolver with the dev server's settings for tsconfig `paths`.
    pub fn load(root: &Path, input: &OptimizeInput) -> Aliases {
        let resolver = oj_resolver::OjResolver::with_settings(
            root,
            oj_resolver::ResolveSettings {
                conditions: input.conditions.clone(),
                alias: input.alias.clone(),
                dedupe: input.dedupe.clone(),
                extensions: (!input.extensions.is_empty()).then(|| input.extensions.clone()),
                main_fields: (!input.main_fields.is_empty()).then(|| input.main_fields.clone()),
                preserve_symlinks: input.preserve_symlinks,
                server: false,
                tsconfig_paths: input.tsconfig_paths,
                tsconfig_file: input.tsconfig_file.clone(),
            },
        );
        let glob = input
            .alias
            .iter()
            .cloned()
            .chain(resolver.glob_aliases())
            .collect();
        Aliases {
            config: input.alias.clone(),
            glob,
            resolver,
        }
    }

    /// What `import.meta.glob` patterns go through: the aliases, then, with
    /// `resolve.tsconfigPaths`, the tsconfig `paths` (Vite's resolve plugin
    /// maps a glob by them; vite-tsconfig-paths has no answer for a glob,
    /// which names no file).
    pub fn glob(&self, id: &str) -> Option<String> {
        first_rewrite(&self.glob, id)
    }

    /// `id` after the first `resolve.alias` entry that takes it, alone.
    pub fn alias(&self, id: &str) -> Option<String> {
        first_rewrite(&self.config, id)
    }

    /// The rewrite of `id` from `importer`, as a path: the first
    /// `resolve.alias` entry that takes it (Vite's alias plugin runs ahead of
    /// every resolver), then tsconfig `paths` on what it gave, as the app's
    /// Vite applies them. No importer, no `paths` (the plugin's rule).
    /// `Err` when nothing answers the id in Vite (only a declaration file).
    pub fn apply(&self, id: &str, importer: Option<&str>) -> Result<Option<String>, Unresolved> {
        let aliased = first_rewrite(&self.config, id);
        let target = aliased.as_deref().unwrap_or(id);
        let Some(importer) = importer else {
            return Ok(aliased);
        };
        let file = importer.split(['?', '#']).next().unwrap_or(importer);
        match self
            .resolver
            .tsconfig_answer(Path::new(file), target, false)
        {
            oj_resolver::TsconfigAnswer::File(p) => Ok(Some(p.to_string_lossy().into_owned())),
            oj_resolver::TsconfigAnswer::Bundler => Ok(aliased),
            oj_resolver::TsconfigAnswer::Unresolved => Err(Unresolved),
        }
    }
}

/// @rollup/plugin-alias: a `find` takes the id itself or `find/…`.
fn first_rewrite(aliases: &[(String, String)], id: &str) -> Option<String> {
    aliases.iter().find_map(|(find, replacement)| {
        let rest = id.strip_prefix(find.as_str())?;
        (rest.is_empty() || rest.starts_with('/')).then(|| format!("{replacement}{rest}"))
    })
}
