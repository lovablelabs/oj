//! tsconfig `paths` the way the app's Vite resolves them, which depends on
//! how the app turns them on (see [`TsconfigPaths`]). The vite-tsconfig-paths
//! (6.1.1) model, which oj also uses when nothing turns them on:
//!
//! - Projects: the root `tsconfig.json` and the projects it references,
//!   tried from the importer's directory upward, by file name within a
//!   directory. A project whose `files` is empty with no `include` has no
//!   resolver; one with neither `paths` nor `baseUrl` neither.
//! - A project passes on an importer its `include`/`exclude` leave out
//!   (`**/node_modules` excluded by default, `outDir` too), or that is not a
//!   TypeScript file (any script with `allowJs`), and on an answer that is a
//!   declaration file: the next project tries.
//! - Patterns go by the length before their first `*`, longest first, a
//!   pattern with no `*` counting as 0 (`getPrefixLength`). Each `*` matches
//!   one or more characters, greedy as the plugin's RegExp; a target's n-th
//!   `*` takes the n-th capture (the last one past the end, the whole id for
//!   a pattern with none).
//! - A pattern's targets resolve in order, each through `resolve.alias`
//!   again (the plugin's `this.resolve` skips only itself); then `baseUrl`,
//!   whose answer is none when it is a `.json` the id did not name.
//! - The first project that matches nothing ends the search.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oxc_resolver::TsConfig;
use regex::Regex;

/// How the app's Vite applies tsconfig `paths`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TsconfigPaths {
    /// vite-tsconfig-paths runs.
    Plugin,
    /// vite-tsconfig-paths and `resolve.tsconfigPaths: true`: the plugin
    /// first, Vite's own resolver for whatever it passes on.
    PluginNative,
    /// `resolve.tsconfigPaths: true`: Vite 8's own resolver (oxc, the
    /// tsconfig that owns the importer), an exact key first, a declaration
    /// file served like any file.
    Native,
    /// Neither (or the plugins are unknown): Vite's dev server applies no
    /// `paths`. `vite build` lets rolldown's own resolver apply them to what
    /// its resolve plugin leaves (`TsconfigAnswer::Bundler` for a build).
    #[default]
    Neither,
    /// No `paths` at all (Vite's module runner resolving an external).
    Off,
}

/// vite-tsconfig-paths' own options, as the app's Vite config passes them
/// (`tsconfigPaths({ … })`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginOptions {
    /// `root`, resolved: the project root and workspace root both.
    pub root: Option<PathBuf>,
    /// `projects`, resolved: the tsconfig files loaded (with their
    /// references) in place of discovery.
    pub projects: Option<Vec<PathBuf>>,
    /// `projectDiscovery: "lazy"`: each importer's ancestors, found on demand.
    pub lazy: bool,
    /// `configNames`: the file names discovery finds.
    pub config_names: Option<Vec<String>>,
    /// `loose`, or an `importerFilter` function: any importer passes the
    /// extension check.
    pub loose: bool,
}

/// What the plugin's resolvers answered.
pub(crate) enum Mapped {
    File(PathBuf),
    /// Every answer was a declaration file.
    Declaration(PathBuf),
    None,
}

/// A project's `include`/`exclude`, compiled as the plugin's getIncluder,
/// as absolute globs: a pattern a config inherits through `extends` stays
/// relative to the config that declares it (tsconfck rebases it there),
/// where oxc rebases it onto the extending one.
struct Includer {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    /// `baseUrl`, relative to the config that declares it (oxc rebases an
    /// inherited one onto the extending config; tsconfck and tsc do not).
    base_url: Option<PathBuf>,
}

impl Includer {
    fn new(project: &TsConfig) -> Includer {
        let dir = project.path.parent().unwrap_or(Path::new("/"));
        // `${configDir}` is the final config's directory (tsconfck's
        // replaceTokens), wherever the pattern is declared.
        let globs = |base: &Path, list: &[String]| -> Vec<Regex> {
            list.iter()
                .flat_map(|p| {
                    let path = match p.strip_prefix("${configDir}") {
                        Some(rest) => dir.join(rest.trim_start_matches('/')),
                        None => base.join(p),
                    };
                    compiled_glob(&lexical(&path).to_string_lossy())
                })
                .collect()
        };
        let include = match declared(&project.path, "include", 0) {
            Some((base, list)) => globs(&base, &list),
            None => globs(dir, &["**/*".to_string()]),
        };
        let mut exclude = match declared(&project.path, "exclude", 0) {
            Some((base, list)) => globs(&base, &list),
            None => globs(
                dir,
                &["**/node_modules", "**/bower_components", "**/jspm_packages"].map(String::from),
            ),
        };
        if let Some(out) = &project.compiler_options.out_dir {
            exclude.extend(compiled_glob(&lexical(out).to_string_lossy()));
        }
        let base_url = declared(&project.path, "baseUrl", 0).map(|(base, list)| {
            let value = &list[0];
            match value.strip_prefix("${configDir}") {
                Some(rest) => lexical(&dir.join(rest.trim_start_matches('/'))),
                None => lexical(&base.join(value)),
            }
        });
        Includer {
            include,
            exclude,
            base_url,
        }
    }

    /// Whether `exclude` takes `importer`'s directory: a file there, whatever
    /// its name (an importer known only by its directory).
    fn excludes_dir(&self, dir: &Path) -> bool {
        let probe = dir.join("__oj_importer__");
        let probe = probe.to_string_lossy();
        self.exclude.iter().any(|g| g.is_match(&probe))
    }

    fn includes(&self, importer: &Path) -> bool {
        if self.include.is_empty() && self.exclude.is_empty() {
            return true;
        }
        let path = importer.to_string_lossy();
        let path = path.split(['?', '#']).next().unwrap_or("");
        self.include.iter().any(|g| g.is_match(path))
            && !self.exclude.iter().any(|g| g.is_match(path))
    }
}

/// `key` (`include`, `exclude`) as the config that declares it writes it,
/// and that config's directory: the file itself, else the configs it
/// `extends` (a later one over an earlier one, as tsc merges them).
fn declared(file: &Path, key: &str, depth: usize) -> Option<(PathBuf, Vec<String>)> {
    if depth > 16 {
        return None;
    }
    let mut text = std::fs::read_to_string(file).ok()?;
    json_strip_comments::strip(&mut text).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let dir = file.parent()?;
    let own: Option<Vec<String>> = match key {
        "baseUrl" => json
            .get("compilerOptions")
            .and_then(|o| o.get("baseUrl"))
            .and_then(|v| v.as_str())
            .map(|v| vec![v.to_string()]),
        _ => json.get(key).and_then(|v| v.as_array()).map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        }),
    };
    if let Some(list) = own {
        return Some((dir.to_path_buf(), list));
    }
    let parents: Vec<&str> = match json.get("extends") {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    };
    parents
        .into_iter()
        .rev()
        .find_map(|ext| declared(&extended_file(dir, ext)?, key, depth + 1))
}

/// The file an `extends` entry names: relative to `dir`, or a package's
/// (its `tsconfig.json`, or the `.json` it names) from the nearest
/// node_modules.
fn extended_file(dir: &Path, ext: &str) -> Option<PathBuf> {
    let with_json = |p: PathBuf| -> Option<PathBuf> {
        if p.is_file() {
            return Some(p);
        }
        let json = PathBuf::from(format!("{}.json", p.display()));
        if json.is_file() {
            return Some(json);
        }
        let inner = p.join("tsconfig.json");
        inner.is_file().then_some(inner)
    };
    if ext.starts_with('.') || Path::new(ext).is_absolute() {
        return with_json(dir.join(ext));
    }
    dir.ancestors()
        .find_map(|d| with_json(d.join("node_modules").join(ext)))
}

/// `path` with `.` and `..` folded, as `path.resolve` does.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// addCompiledGlob: a glob whose last segment has no `*` matches the
/// directory's contents, and itself too when it has an extension.
fn compiled_glob(glob: &str) -> Vec<Regex> {
    let last = glob.rsplit('/').next().unwrap_or("");
    if last.contains('*') {
        return glob_regex(glob).into_iter().collect();
    }
    let mut out: Vec<Regex> = glob_regex(&format!("{}/**", glob.trim_end_matches('/')))
        .into_iter()
        .collect();
    let has_ext = last.rsplit_once('.').is_some_and(|(_, ext)| {
        !ext.is_empty() && ext.chars().all(|c| c.is_alphanumeric() || c == '_')
    });
    if has_ext {
        out.extend(glob_regex(glob));
    }
    out
}

/// globrex with `globstar` and `extended`: `**` spans segments, `*` stays
/// within one, `?` is any one character, `{a,b}` alternates, `[...]` is a
/// class (`[!…]` negated), and the extglobs `?(…)`, `*(…)`, `+(…)`, `@(…)`
/// group alternatives (`!(…)` needs a lookahead the regex crate lacks, and
/// stays literal).
fn glob_regex(glob: &str) -> Option<Regex> {
    let segments: Vec<&str> = glob.split('/').collect();
    let mut re = String::from("^");
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        if *seg == "**" {
            re.push_str(if last { ".*" } else { "(?:[^/]*(?:/|$))*" });
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        let mut braces = 0usize;
        // The closing suffix of each open extglob group.
        let mut groups: Vec<&str> = Vec::new();
        let mut j = 0;
        while j < chars.len() {
            let c = chars[j];
            let next = chars.get(j + 1).copied();
            match c {
                '?' | '*' | '+' | '@' if next == Some('(') => {
                    groups.push(match c {
                        '?' => ")?",
                        '*' => ")*",
                        '+' => ")+",
                        _ => ")",
                    });
                    re.push_str("(?:");
                    j += 2;
                    continue;
                }
                ')' if !groups.is_empty() => re.push_str(groups.pop().unwrap_or(")")),
                '|' if !groups.is_empty() => re.push('|'),
                '*' => re.push_str("[^/]*"),
                '?' => re.push('.'),
                '{' => {
                    braces += 1;
                    re.push_str("(?:");
                }
                '}' if braces > 0 => {
                    braces -= 1;
                    re.push(')');
                }
                ',' if braces > 0 => re.push('|'),
                '[' => {
                    re.push('[');
                    j += 1;
                    if chars.get(j) == Some(&'!') {
                        re.push('^');
                        j += 1;
                    }
                    while j < chars.len() && chars[j] != ']' {
                        if chars[j] == '\\' || chars[j] == '[' {
                            re.push('\\');
                        }
                        re.push(chars[j]);
                        j += 1;
                    }
                    re.push(']');
                }
                c => re.push_str(&regex::escape(&c.to_string())),
            }
            j += 1;
        }
        if !last {
            re.push('/');
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// The captures of `id` against a `paths` pattern, each `*` one or more
/// characters, the first ones greedy (the plugin's `^…(.+)…$`); `None` when
/// it does not match.
pub(crate) fn star_match<'a>(pattern: &str, id: &'a str) -> Option<Vec<&'a str>> {
    let literals: Vec<&str> = pattern.split('*').collect();
    match_literals(&literals, id)
}

fn match_literals<'a>(literals: &[&str], s: &'a str) -> Option<Vec<&'a str>> {
    let rest = s.strip_prefix(literals[0])?;
    if literals.len() == 1 {
        return rest.is_empty().then(Vec::new);
    }
    (1..=rest.len())
        .rev()
        .filter(|&end| rest.is_char_boundary(end))
        .find_map(|end| {
            let mut tail = match_literals(&literals[1..], &rest[end..])?;
            tail.insert(0, &rest[..end]);
            Some(tail)
        })
}

/// The files `paths` map `id` to, in the plugin's order.
pub(crate) fn candidates(project: &TsConfig, id: &str) -> Vec<PathBuf> {
    let Some(paths) = &project.compiler_options.paths else {
        return Vec::new();
    };
    let mut patterns: Vec<(&String, &Vec<PathBuf>)> = paths.iter().collect();
    patterns.sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern.find('*').unwrap_or(0)));
    let mut out = Vec::new();
    for (pattern, targets) in patterns {
        let Some(captures) = star_match(pattern, id) else {
            continue;
        };
        for target in targets {
            let template = target.to_string_lossy();
            let mut n = 0;
            let mapped: String = template
                .split('*')
                .enumerate()
                .map(|(i, part)| {
                    if i == 0 {
                        return part.to_string();
                    }
                    n += 1;
                    let star = match captures.len() {
                        0 => id,
                        len => captures[n.min(len) - 1],
                    };
                    format!("{star}{part}")
                })
                .collect();
            out.push(PathBuf::from(mapped));
        }
    }
    out
}

/// `localeCompare` as the plugin sorts a directory's projects (ICU root
/// collation for file names: punctuation first, `_` before `-` before `.`,
/// then digits, then letters without regard to case, lowercase first on a
/// tie).
fn locale_compare(a: &Path, b: &Path) -> std::cmp::Ordering {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let key = |p: &Path| -> Vec<(u32, u32)> {
        p.to_string_lossy()
            .chars()
            .map(|c| {
                let primary = if let Some(i) = PUNCT.find(c) {
                    i as u32 + 1
                } else if c.is_ascii_digit() {
                    100 + c as u32
                } else if c.is_alphabetic() {
                    1000 + c.to_lowercase().next().unwrap_or(c) as u32
                } else {
                    500 + c as u32
                };
                (primary, c.is_uppercase() as u32)
            })
            .collect()
    };
    let (ka, kb) = (key(a), key(b));
    let primary = |k: &[(u32, u32)]| k.iter().map(|x| x.0).collect::<Vec<_>>();
    primary(&ka).cmp(&primary(&kb)).then_with(|| ka.cmp(&kb))
}

/// The importers a project's resolver takes: TypeScript files, any script
/// with `allowJs` (the plugin's default importerFilter).
fn takes_importer(project: &TsConfig, importer: &Path, loose: bool) -> bool {
    if loose {
        return true;
    }
    let name = importer.to_string_lossy();
    let name = name.split(['?', '#']).next().unwrap_or("");
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let ts = matches!(ext, "ts" | "tsx" | "mts" | "cts" | "mtsx" | "ctsx");
    let jsconfig = project
        .path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("jsconfig."));
    if project.compiler_options.allow_js == Some(true) || jsconfig {
        ts || matches!(
            ext,
            "js" | "jsx" | "mjs" | "cjs" | "mjsx" | "cjsx" | "astro" | "mdx" | "svelte" | "vue"
        )
    } else {
        ts
    }
}

/// The projects and their compiled globs, cached until `clear`.
#[derive(Default)]
pub(crate) struct Projects {
    options: PluginOptions,
    includers: Mutex<HashMap<PathBuf, Arc<Includer>>>,
    /// The projects an importer directory sees, found once.
    by_dir: Mutex<HashMap<PathBuf, Arc<Vec<Arc<TsConfig>>>>>,
}

impl Projects {
    /// The projects from `dir` up to `workspace`, their references included.
    pub fn projects_for(
        &self,
        dir: &Path,
        load: &impl Fn(&Path) -> Option<Arc<TsConfig>>,
        workspace: &Path,
    ) -> Arc<Vec<Arc<TsConfig>>> {
        if let Some(found) = self
            .by_dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir)
        {
            return Arc::clone(found);
        }
        let mut all: Vec<Arc<TsConfig>> = Vec::new();
        let add = |file: PathBuf, all: &mut Vec<Arc<TsConfig>>| {
            if file.is_file() && !all.iter().any(|p| p.path == file) {
                if let Some(project) = load(&file) {
                    all.push(project);
                }
            }
        };
        let names: Vec<String> = self
            .options
            .config_names
            .clone()
            .unwrap_or_else(|| vec!["tsconfig.json".into(), "jsconfig.json".into()]);
        if let Some(projects) = &self.options.projects {
            // `projects`: those files alone (loadEagerProjects).
            for file in projects {
                add(file.clone(), &mut all);
            }
        } else if self.options.lazy {
            // Lazy discovery: every ancestor, one named node_modules or .git
            // skipped.
            let skipped = |d: &Path| {
                d.file_name()
                    .is_some_and(|n| n == "node_modules" || n == ".git")
            };
            for d in dir.ancestors().filter(|d| !skipped(d)) {
                for name in &names {
                    add(d.join(name), &mut all);
                }
            }
        } else {
            // Eager discovery (tsconfck.findAll under the workspace root)
            // never descends into node_modules or .git.
            let skipped = |d: &Path| {
                d.components()
                    .any(|c| c.as_os_str() == "node_modules" || c.as_os_str() == ".git")
            };
            for d in dir
                .ancestors()
                .take_while(|d| d.starts_with(workspace))
                .filter(|d| !skipped(d))
            {
                for name in &names {
                    add(d.join(name), &mut all);
                }
            }
        }
        let mut i = 0;
        while i < all.len() {
            for r in all[i].references_resolved.clone() {
                if !all.iter().any(|p| p.path == r.path) {
                    all.push(r);
                }
            }
            i += 1;
        }
        let all = Arc::new(all);
        self.by_dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_path_buf(), Arc::clone(&all));
        all
    }

    /// The same options, empty caches.
    pub fn fresh(&self) -> Projects {
        Projects::new(self.options.clone())
    }

    pub fn new(options: PluginOptions) -> Projects {
        Projects {
            options,
            ..Projects::default()
        }
    }

    pub fn clear(&self) {
        self.includers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.by_dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn includer(&self, project: &TsConfig) -> Arc<Includer> {
        let mut map = self.includers.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            map.entry(project.path.clone())
                .or_insert_with(|| Arc::new(Includer::new(project))),
        )
    }

    /// The plugin's answer for `id` from `importer`. The projects: each
    /// `tsconfig.json` and `jsconfig.json` from the importer's directory up to
    /// `workspace` (the plugin finds them all under Vite's
    /// searchForWorkspaceRoot), with the projects they reference; `load`
    /// parses one (None when it does not parse: the plugin logs and skips
    /// it). `resolve` loads a mapped path (through the aliases).
    /// `importer` is None when only its directory is known: no project then
    /// passes on it for its extension or its `include`.
    pub fn map(
        &self,
        load: impl Fn(&Path) -> Option<Arc<TsConfig>>,
        workspace: &Path,
        importer_dir: &Path,
        importer: Option<&Path>,
        id: &str,
        resolve: impl Fn(&str) -> Option<PathBuf>,
    ) -> Mapped {
        if id.starts_with("./")
            || id.starts_with("../")
            || id == "."
            || id == ".."
            || id.contains('\0')
        {
            return Mapped::None;
        }
        let all = self.projects_for(importer_dir, &load, workspace);
        let mut declaration = None;
        let mut dir = Some(importer_dir);
        while let Some(d) = dir {
            let mut here: Vec<&Arc<TsConfig>> =
                all.iter().filter(|p| p.path.parent() == Some(d)).collect();
            here.sort_by(|a, b| locale_compare(&a.path, &b.path));
            for project in here {
                let options = &project.compiler_options;
                let no_files = project.files.as_ref().is_some_and(|f| f.is_empty())
                    && project.include.as_ref().is_none_or(|i| i.is_empty());
                if no_files || (options.paths.is_none() && options.base_url.is_none()) {
                    continue;
                }
                let passes = match importer {
                    Some(importer) => {
                        !takes_importer(project, importer, self.options.loose)
                            || !self.includer(project).includes(importer)
                    }
                    None => self.includer(project).excludes_dir(importer_dir),
                };
                if passes {
                    continue;
                }
                let (id, query) = match id.split_once('?') {
                    Some((id, q)) => (id, Some(q)),
                    None => (id, None),
                };
                let found = candidates(project, id)
                    .iter()
                    .find_map(|c| resolve(&c.to_string_lossy()))
                    .or_else(|| {
                        options.base_url.as_ref()?;
                        if id.starts_with('/') {
                            return None;
                        }
                        let base = self.includer(project).base_url.clone()?;
                        let found = resolve(&base.join(id).to_string_lossy())?;
                        let json = found.extension().is_some_and(|e| e == "json");
                        (!json || id.ends_with(".json")).then_some(found)
                    });
                let Some(found) = found else {
                    return declaration.map_or(Mapped::None, Mapped::Declaration);
                };
                if found.to_string_lossy().ends_with(".d.ts") {
                    declaration = Some(found);
                    continue;
                }
                return Mapped::File(match query {
                    Some(q) => PathBuf::from(format!("{}?{q}", found.display())),
                    None => found,
                });
            }
            dir = d.parent();
        }
        declaration.map_or(Mapped::None, Mapped::Declaration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_match_as_the_plugins_regexp() {
        assert_eq!(star_match("@/*", "@/a/b"), Some(vec!["a/b"]));
        assert_eq!(star_match("x/*/y/*", "x/a/y/b"), Some(vec!["a", "b"]));
        assert_eq!(
            star_match("x/*/y/*", "x/a/y/c/y/b"),
            Some(vec!["a/y/c", "b"])
        );
        assert_eq!(
            star_match("@/*", "@/"),
            None,
            "a star takes at least one char"
        );
        assert_eq!(star_match("exact", "exact"), Some(vec![]));
        assert_eq!(star_match("exact", "exactly"), None);
    }

    #[test]
    fn globs_match_as_globrex() {
        let any = |glob: &str, path: &str| compiled_glob(glob).iter().any(|r| r.is_match(path));
        assert!(any("./src", "./src/a/b.ts"));
        assert!(!any("./src", "./srcx/a.ts"));
        assert!(any("./src/**/*", "./src/a.ts"));
        assert!(any("./src/**/*", "./src/a/b.ts"));
        assert!(any("./vite.config.ts", "./vite.config.ts"));
        assert!(!any("./vite.config.ts", "./src/a.ts"));
        assert!(any("./**/node_modules", "./a/node_modules/x/y.js"));
        assert!(any("./src/*.{ts,tsx}", "./src/a.tsx"));
        assert!(!any("./src/*.{ts,tsx}", "./src/a/b.tsx"));
        assert!(any("./src/[!x]*.ts", "./src/a.ts"));
        assert!(!any("./src/[!x]*.ts", "./src/x.ts"));
        assert!(any("./src/?.ts", "./src/a.ts"));
        assert!(any("./src/@(a|b).ts", "./src/b.ts"));
        assert!(!any("./src/@(a|b).ts", "./src/c.ts"));
        assert!(any("./src/+(a).ts", "./src/aa.ts"));
    }
}
