// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The live config values a Start bundle reads off the app's resolved Vite
//! config, as the plugin host serves them (plugin-host.mjs `startFacts`). A
//! RegExp arrives as `{ "$regex", "flags" }` and a function as a
//! `{ "$fn": n }` handle the host calls back.

use rolldown_utils::js_regex::HybridRegex;
use serde_json::Value;

/// A config RegExp, compiled with its own flags (the stateful `g`, `y` and
/// `d` change nothing about a single test).
pub(crate) fn js_regex(v: &Value) -> Option<HybridRegex> {
    let source = v.get("$regex")?.as_str()?;
    let flags: String = v
        .get("flags")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !matches!(c, 'g' | 'y' | 'd'))
        .collect();
    HybridRegex::with_flags(source, &flags).ok()
}

/// The handle of a config function, when `v` is one.
pub(crate) fn fn_handle(v: &Value) -> Option<u64> {
    v.get("$fn")?.as_u64()
}

/// The facts of one plugin host.
#[derive(Debug, Default, Clone)]
pub(crate) struct Facts {
    pub alias: Vec<AliasEntry>,
    pub external: Option<Value>,
    /// `output.codeSplitting` (or the older `advancedChunks`), as sent.
    pub code_splitting: Option<Value>,
    pub chunk_optimization: Option<Value>,
    pub preserve_entry_signatures: Option<Value>,
    /// `server.hmr !== false`.
    pub server_hmr: bool,
    /// `server.watch`: `None` when the app turned watching off (`null`).
    pub server_watch: Option<Value>,
    /// Whether the app configures `@vitejs/plugin-react` (`vite:react-refresh`).
    pub react_refresh: bool,
    pub cloudflare: Option<CloudflareEnv>,
    pub worker: WorkerOptions,
    /// How many resolveId hooks run before Vite's core resolver, and after.
    pub resolve_id_pre: u64,
    pub resolve_id_post: u64,
}

impl Facts {
    pub fn from_json(v: &Value) -> Facts {
        Facts {
            alias: normalize_alias(v.get("alias").unwrap_or(&Value::Null)),
            external: v.get("external").filter(|e| !e.is_null()).cloned(),
            code_splitting: v.get("codeSplitting").filter(|e| !e.is_null()).cloned(),
            chunk_optimization: v.get("chunkOptimization").filter(|e| !e.is_null()).cloned(),
            preserve_entry_signatures: v
                .get("preserveEntrySignatures")
                .filter(|e| !e.is_null())
                .cloned(),
            server_hmr: v.get("serverHmr").and_then(Value::as_bool).unwrap_or(true),
            server_watch: match v.get("serverWatch") {
                Some(Value::Null) => None,
                Some(w) => Some(w.clone()),
                None => Some(Value::Object(Default::default())),
            },
            react_refresh: v
                .get("reactRefresh")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            cloudflare: v.get("cloudflare").and_then(CloudflareEnv::from_json),
            worker: WorkerOptions::from_json(v.get("worker").unwrap_or(&Value::Null)),
            resolve_id_pre: v["resolveIdPhases"]["pre"].as_u64().unwrap_or(0),
            resolve_id_post: v["resolveIdPhases"]["post"].as_u64().unwrap_or(0),
        }
    }
}

/// Vite's resolved `worker` options a worker bundle takes in place of the
/// app's build options: `format` and `rolldownOptions`' `external` and
/// output file names.
#[derive(Debug, Clone)]
pub(crate) struct WorkerOptions {
    /// `iife` (Vite's default) or `es`.
    pub format: String,
    pub external: Option<Value>,
    pub entry_file_names: Option<String>,
    pub chunk_file_names: Option<String>,
    pub asset_file_names: Option<String>,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            format: "iife".into(),
            external: None,
            entry_file_names: None,
            chunk_file_names: None,
            asset_file_names: None,
        }
    }
}

impl WorkerOptions {
    fn from_json(v: &Value) -> Self {
        let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            format: text("format").unwrap_or_else(|| "iife".into()),
            external: v.get("external").filter(|e| !e.is_null()).cloned(),
            entry_file_names: text("entryFileNames"),
            chunk_file_names: text("chunkFileNames"),
            asset_file_names: text("assetFileNames"),
        }
    }

    /// An `es` worker runs as a module; any other format as a classic script.
    pub fn is_module(&self) -> bool {
        matches!(self.format.as_str(), "es" | "esm" | "module")
    }
}

/// One `resolve.alias` entry after Vite's normalizeAlias.
#[derive(Debug, Clone)]
pub(crate) struct AliasEntry {
    pub find: AliasFind,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub(crate) enum AliasFind {
    /// Matches the id itself or the id plus `/...` (@rollup/plugin-alias).
    Exact(String),
    Regex(HybridRegex),
}

impl AliasEntry {
    /// The rewritten specifier when this entry matches `id`.
    pub fn apply(&self, id: &str) -> Option<String> {
        match &self.find {
            AliasFind::Exact(find) => {
                let rest = id.strip_prefix(find.as_str())?;
                (rest.is_empty() || rest.starts_with('/'))
                    .then(|| format!("{}{rest}", self.replacement))
            }
            AliasFind::Regex(re) => js_replace(re, id, &self.replacement),
        }
    }
}

/// `id.replace(re, replacement)` with JavaScript's replacement patterns
/// (`$1`, `$&`, `` $` ``, `$'`, `$$`), as Vite's alias plugin applies a
/// RegExp `find`: `$1_web` is group 1 followed by `_web`, where the regex
/// crate would read a group named `1_web`. `None` when `re` does not match.
pub(crate) fn js_replace(
    re: &rolldown_utils::js_regex::HybridRegex,
    id: &str,
    replacement: &str,
) -> Option<String> {
    let groups: Vec<Option<std::ops::Range<usize>>> = match re.as_optimized() {
        Some(re) => {
            let caps = re.captures(id)?;
            (0..caps.len())
                .map(|i| caps.get(i).map(|m| m.range()))
                .collect()
        }
        None => {
            let m = re.find_from(id, 0)?.next()?;
            (0..m.group_count()).map(|i| m.group(i)).collect()
        }
    };
    let whole = groups.first()?.clone()?;
    let bytes = replacement.as_bytes();
    let mut out = String::with_capacity(id.len() + replacement.len());
    out.push_str(&id[..whole.start]);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || i + 1 >= bytes.len() {
            let next = replacement[i..].chars().next().expect("a char");
            out.push(next);
            i += next.len_utf8();
            continue;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'&' => {
                out.push_str(&id[whole.clone()]);
                i += 2;
            }
            b'`' => {
                out.push_str(&id[..whole.start]);
                i += 2;
            }
            b'\'' => {
                out.push_str(&id[whole.end..]);
                i += 2;
            }
            d if d.is_ascii_digit() => {
                // Two digits when they name a group, else one (JS's rule).
                let one = (d - b'0') as usize;
                let two = bytes
                    .get(i + 2)
                    .filter(|c| c.is_ascii_digit())
                    .map(|c| one * 10 + (c - b'0') as usize)
                    .filter(|n| *n > 0 && *n < groups.len());
                let (n, len) = match two {
                    Some(n) => (n, 3),
                    None => (one, 2),
                };
                if n > 0 && n < groups.len() {
                    if let Some(range) = &groups[n] {
                        out.push_str(&id[range.clone()]);
                    }
                    i += len;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    out.push_str(&id[whole.end..]);
    Some(out)
}

/// Vite's normalizeAlias: an object or array `resolve.alias` as an ordered
/// list, a trailing `/` dropped from both sides when both carry it. Entries
/// whose replacement is not a string (a custom resolver) are skipped.
pub(crate) fn normalize_alias(alias: &Value) -> Vec<AliasEntry> {
    let entries: Vec<(Value, Value)> = match alias {
        Value::Array(items) => items
            .iter()
            .filter_map(|e| Some((e.get("find")?.clone(), e.get("replacement")?.clone())))
            .collect(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (Value::String(k.clone()), v.clone()))
            .collect(),
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter_map(|(find, replacement)| {
            let replacement = replacement.as_str()?.to_string();
            match &find {
                Value::String(find) => {
                    let (find, replacement) = if find.ends_with('/') && replacement.ends_with('/') {
                        (
                            find.trim_end_matches('/').to_string(),
                            replacement[..replacement.len() - 1].to_string(),
                        )
                    } else {
                        (find.clone(), replacement)
                    };
                    Some(AliasEntry {
                        find: AliasFind::Exact(find),
                        replacement,
                    })
                }
                other => Some(AliasEntry {
                    find: AliasFind::Regex(js_regex(other)?),
                    replacement,
                }),
            }
        })
        .collect()
}

/// The Worker environment @cloudflare/vite-plugin declared, and what its
/// bundle needs from it.
#[derive(Debug, Clone)]
pub(crate) struct CloudflareEnv {
    pub name: String,
    /// `resolve.builtins`: the runtime's own modules, left external.
    pub builtins: Vec<Builtin>,
    pub conditions: Vec<String>,
    /// The environment's `resolve.mainFields`, when it sets them.
    pub main_fields: Option<Vec<String>>,
    pub target: Option<String>,
    /// Rolldown builtin plugins the environment attached, by name, with
    /// their options (Vite's esmExternalRequirePlugin).
    pub rolldown_plugins: Vec<(String, Value)>,
    pub out_dir: Option<String>,
    /// The top-level `build.outDir`, which the environment's default nests in.
    pub root_out_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum Builtin {
    Name(String),
    Regex(HybridRegex),
}

impl CloudflareEnv {
    fn from_json(v: &Value) -> Option<CloudflareEnv> {
        let strings = |key: &str| -> Vec<String> {
            v.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(CloudflareEnv {
            name: v.get("name")?.as_str()?.to_string(),
            builtins: v
                .get("builtins")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|b| match b {
                            Value::String(s) => Some(Builtin::Name(s.clone())),
                            other => js_regex(other).map(Builtin::Regex),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            conditions: strings("conditions"),
            main_fields: v.get("mainFields").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            }),
            target: v.get("target").and_then(Value::as_str).map(str::to_string),
            rolldown_plugins: v
                .get("rolldownPlugins")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            Some((
                                p.get("name")?.as_str()?.to_string(),
                                p.get("options").cloned().unwrap_or(Value::Null),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            out_dir: v.get("outDir").and_then(Value::as_str).map(str::to_string),
            root_out_dir: v
                .get("rootOutDir")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    pub fn is_builtin(&self, id: &str) -> bool {
        self.builtins.iter().any(|b| match b {
            Builtin::Name(n) => n == id,
            Builtin::Regex(re) => re.matches(id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_options_default_to_vites_iife() {
        let none = Facts::from_json(&serde_json::json!({})).worker;
        assert_eq!(none.format, "iife");
        assert!(!none.is_module());
        assert!(none.external.is_none() && none.entry_file_names.is_none());
        let es = Facts::from_json(&serde_json::json!({
            "worker": {
                "format": "es",
                "external": ["left-pad"],
                "entryFileNames": "assets/wk-[name]-[hash].js",
                "chunkFileNames": null,
            }
        }))
        .worker;
        assert!(es.is_module());
        assert_eq!(es.external, Some(serde_json::json!(["left-pad"])));
        assert_eq!(
            es.entry_file_names.as_deref(),
            Some("assets/wk-[name]-[hash].js")
        );
        assert!(es.chunk_file_names.is_none());
    }

    #[test]
    fn a_regex_alias_replaces_with_javascripts_patterns() {
        let entries = normalize_alias(&serde_json::json!([
            { "find": { "$regex": "^(.*)\\.native$", "flags": "" }, "replacement": "$1_web" },
        ]));
        assert_eq!(
            entries[0].apply("button.native").as_deref(),
            Some("button_web")
        );
        let re = rolldown_utils::js_regex::HybridRegex::new("^@app/(.*)$").unwrap();
        assert_eq!(
            js_replace(&re, "@app/x", "/src/$1").as_deref(),
            Some("/src/x")
        );
        assert_eq!(
            js_replace(&re, "@app/x", "$$1-$&").as_deref(),
            Some("$1-@app/x")
        );
        assert_eq!(
            js_replace(&re, "@app/x", "$9").as_deref(),
            Some("$9"),
            "no such group"
        );
        assert_eq!(js_replace(&re, "other", "$1"), None);
        // regress (lookarounds) takes the same patterns.
        let re = rolldown_utils::js_regex::HybridRegex::new(r"^@app(?!/excluded)(/.*)?$").unwrap();
        assert_eq!(
            js_replace(&re, "@app/utils", "/abs$1_x").as_deref(),
            Some("/abs/utils_x")
        );
    }
    use serde_json::json;

    #[test]
    fn aliases_normalize_like_vite() {
        let object = normalize_alias(&json!({ "@/": "/app/src/", "~lib": "/app/lib" }));
        assert_eq!(object.len(), 2);
        assert_eq!(object[0].apply("@/a").as_deref(), Some("/app/src/a"));
        assert_eq!(
            object[0].apply("@"),
            Some("/app/src".into()),
            "both trailing slashes dropped"
        );
        assert_eq!(object[1].apply("~lib/x"), Some("/app/lib/x".into()));
        assert_eq!(
            object[1].apply("~library"),
            None,
            "only the id or the id plus /..."
        );

        let array = normalize_alias(&json!([
            { "find": { "$regex": "^#/(.*)$", "flags": "g" }, "replacement": "/app/$1" },
            { "find": "react", "replacement": "preact/compat" },
            { "find": "skip", "replacement": { "$fn": 0 } },
        ]));
        assert_eq!(array.len(), 2, "a non-string replacement is skipped");
        assert_eq!(array[0].apply("#/x/y").as_deref(), Some("/app/x/y"));
        assert_eq!(array[0].apply("x"), None);
        assert_eq!(
            array[1].apply("react/jsx-runtime").as_deref(),
            Some("preact/compat/jsx-runtime")
        );
    }

    #[test]
    fn facts_default_watching_on_and_honor_null() {
        let on = Facts::from_json(&json!({ "serverWatch": { "usePolling": true } }));
        assert!(on.server_watch.is_some());
        assert!(on.server_hmr);
        let off = Facts::from_json(&json!({ "serverWatch": null, "serverHmr": false }));
        assert!(off.server_watch.is_none());
        assert!(!off.server_hmr);
    }

    #[test]
    fn the_cloudflare_environment_reads_its_builtins() {
        let env = CloudflareEnv::from_json(&json!({
            "name": "my_worker",
            "builtins": ["cloudflare:workers", "node:buffer", { "$regex": "^node:", "flags": "" }],
            "conditions": ["workerd", "worker"],
            "rolldownPlugins": [{ "name": "builtin:esm-external-require", "options": { "external": ["x"] } }],
        }))
        .unwrap();
        assert!(env.is_builtin("cloudflare:workers"));
        assert!(env.is_builtin("node:fs"));
        assert!(!env.is_builtin("fs"));
        assert_eq!(env.rolldown_plugins[0].0, "builtin:esm-external-require");
    }
}
