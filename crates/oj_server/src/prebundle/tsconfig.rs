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

pub(super) struct Aliases(Vec<Alias>);

impl Aliases {
    /// The root tsconfig's paths (`tsconfig.json`, else `tsconfig.app.json`),
    /// then each `(find, replacement)` of the config, which matches `find`
    /// itself and `find/…`.
    pub fn load(root: &Path, config: &[(String, String)]) -> Aliases {
        let mut out = tsconfig_paths(root);
        for (find, replacement) in config {
            out.push(Alias::Exact {
                find: find.clone(),
                target: PathBuf::from(replacement),
            });
            out.push(Alias::Prefix {
                prefix: format!("{find}/"),
                target: PathBuf::from(replacement),
            });
        }
        Aliases(out)
    }

    /// The first alias's rewrite of `id`, as a path.
    pub fn apply(&self, id: &str) -> Option<String> {
        self.0.iter().find_map(|a| match a {
            Alias::Exact { find, target } if id == find => {
                Some(target.to_string_lossy().into_owned())
            }
            Alias::Prefix { prefix, target } => id
                .strip_prefix(prefix.as_str())
                .map(|rest| target.join(rest).to_string_lossy().into_owned()),
            _ => None,
        })
    }
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
