// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The aliases the scan and the pre-bundle apply to bare ids ahead of the
//! resolver: the root tsconfig's `paths` (its `extends` chain merged the way
//! tsc merges it) and the config's `resolve.alias`, so a dep reached through
//! an alias resolves to the file the dev server serves.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::normalize;

enum Alias {
    Exact { find: String, target: PathBuf },
    Prefix { prefix: String, target: PathBuf },
}

/// `resolve.alias`, then tsconfig `paths`, as the dev server orders them.
pub(super) struct Aliases {
    config: Vec<Alias>,
    paths: Vec<Alias>,
}

impl Aliases {
    /// Each `(find, replacement)` of the config, in the written order, which
    /// matches `find` itself and `find/…`; then the root tsconfig's paths
    /// (`tsconfig.json`, else `tsconfig.app.json`).
    pub fn load(root: &Path, config: &[(String, String)]) -> Aliases {
        let config = config
            .iter()
            .flat_map(|(find, replacement)| {
                [
                    Alias::Exact {
                        find: find.clone(),
                        target: PathBuf::from(replacement),
                    },
                    Alias::Prefix {
                        prefix: format!("{find}/"),
                        target: PathBuf::from(replacement),
                    },
                ]
            })
            .collect();
        Aliases {
            config,
            paths: tsconfig_paths(root),
        }
    }

    /// The rewrite of `id`, as a path: the first `resolve.alias` entry that
    /// takes it (Vite's alias plugin runs ahead of every resolver), then
    /// tsconfig `paths` on what it gave, as the alias plugin hands the
    /// rewritten id to the rest of the pipeline.
    pub fn apply(&self, id: &str) -> Option<String> {
        let Some(aliased) = first_rewrite(&self.config, id) else {
            return self.mapped(id);
        };
        Some(self.mapped(&aliased).unwrap_or(aliased))
    }

    /// `id` through tsconfig `paths` as vite-tsconfig-paths maps it: each
    /// matching pattern's targets in order, the first that resolves wins,
    /// and an answer that is a declaration file is none.
    fn mapped(&self, id: &str) -> Option<String> {
        let found = self.paths.iter().find_map(|a| {
            let target = match a {
                Alias::Exact { find, target } if id == find => target.clone(),
                Alias::Prefix { prefix, target } => target.join(id.strip_prefix(prefix.as_str())?),
                _ => return None,
            };
            module_file(&target)
        })?;
        let found = found.to_string_lossy().into_owned();
        (!found.ends_with(".d.ts")).then_some(found)
    }
}

fn first_rewrite(aliases: &[Alias], id: &str) -> Option<String> {
    aliases.iter().find_map(|a| match a {
        Alias::Exact { find, target } if id == find => Some(target.to_string_lossy().into_owned()),
        Alias::Prefix { prefix, target } => id
            .strip_prefix(prefix.as_str())
            .map(|rest| target.join(rest).to_string_lossy().into_owned()),
        _ => None,
    })
}

/// The file a mapped `paths` target loads, the way the dev resolver finds
/// it: the path itself, with an extension, or a directory's index.
fn module_file(target: &Path) -> Option<PathBuf> {
    const EXTENSIONS: [&str; 8] = ["mjs", "js", "mts", "ts", "jsx", "tsx", "json", "cjs"];
    if target.is_file() {
        return Some(target.to_path_buf());
    }
    let with_ext = |base: &Path| {
        EXTENSIONS.iter().find_map(|ext| {
            let file = PathBuf::from(format!("{}.{ext}", base.display()));
            file.is_file().then_some(file)
        })
    };
    with_ext(target).or_else(|| {
        if target.join("package.json").is_file() {
            return Some(target.to_path_buf());
        }
        with_ext(&target.join("index"))
    })
}

/// `paths` and the directory its targets are relative to.
struct Paths {
    paths: serde_json::Map<String, serde_json::Value>,
    base: PathBuf,
}

fn tsconfig_paths(root: &Path) -> Vec<Alias> {
    for name in ["tsconfig.json", "tsconfig.app.json"] {
        let Some(chain) = read_chain(&root.join(name), &mut HashSet::new()) else {
            continue;
        };
        let mut out = Vec::new();
        for (key, targets) in &chain.paths {
            let Some(targets) = targets.as_array() else {
                continue;
            };
            // Every target, in order: the first that resolves wins (tsc's
            // fallback order).
            for t in targets.iter().filter_map(|t| t.as_str()) {
                match (key.strip_suffix('*'), t.strip_suffix('*')) {
                    (Some(prefix), Some(dir)) if key.ends_with("/*") && t.ends_with("/*") => out
                        .push(Alias::Prefix {
                            prefix: prefix.to_string(),
                            target: normalize(&chain.base.join(dir)),
                        }),
                    (None, None) => out.push(Alias::Exact {
                        find: key.clone(),
                        target: normalize(&chain.base.join(t)),
                    }),
                    _ => {}
                }
            }
        }
        if !out.is_empty() {
            // The longest prefix first, as vite-tsconfig-paths orders them: a
            // pattern with no `*` counts as no prefix (`getPrefixLength`), so
            // `@/*` goes before an exact key. The sort is stable, so a key's
            // targets keep their order.
            out.sort_by_key(|a| {
                std::cmp::Reverse(match a {
                    Alias::Exact { .. } => 0,
                    Alias::Prefix { prefix, .. } => prefix.len(),
                })
            });
            return out;
        }
    }
    Vec::new()
}

/// A tsconfig with its `extends` chain (relative, or a package such as
/// `@tsconfig/node20/tsconfig.json`): `paths` from the nearest file that
/// declares them, `baseUrl` relative to the file that declares it.
fn read_chain(file: &Path, seen: &mut HashSet<PathBuf>) -> Option<Paths> {
    if !seen.insert(file.to_path_buf()) || !file.is_file() {
        return None;
    }
    let mut text = std::fs::read_to_string(file).ok()?;
    json_strip_comments::strip(&mut text).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let dir = file.parent().unwrap_or(file);
    let own = json.get("compilerOptions");
    let base_url = own.and_then(|o| o.get("baseUrl")).and_then(|b| b.as_str());
    let parents: Vec<&str> = match json.get("extends") {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    };
    let mut merged = None;
    for ext in parents {
        let parent_file = if ext.starts_with('.') || Path::new(ext).is_absolute() {
            let p = dir.join(ext);
            let with_json = PathBuf::from(format!("{}.json", p.display()));
            Some(if !p.exists() && with_json.exists() {
                with_json
            } else {
                p
            })
        } else {
            let spec = if ext.ends_with(".json") {
                ext.to_string()
            } else {
                format!("{ext}/tsconfig.json")
            };
            oj_resolver::OjResolver::new(dir)
                .require_variant()
                .resolve(dir, &spec)
                .ok()
        };
        if let Some(parent) = parent_file.and_then(|f| read_chain(&f, seen)) {
            merged = Some(parent);
        }
    }
    if let Some(paths) = own.and_then(|o| o.get("paths")).and_then(|p| p.as_object()) {
        return Some(Paths {
            paths: paths.clone(),
            base: normalize(&dir.join(base_url.unwrap_or("."))),
        });
    }
    match (merged, base_url) {
        (Some(parent), Some(base)) => Some(Paths {
            paths: parent.paths,
            base: normalize(&dir.join(base)),
        }),
        (merged, _) => merged,
    }
}
