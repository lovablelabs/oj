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
    /// vite-tsconfig-paths runs (or the plugins are unknown).
    #[default]
    Plugin,
    /// vite-tsconfig-paths and `resolve.tsconfigPaths: true`: the plugin
    /// first, Vite's own resolver for whatever it passes on.
    PluginNative,
    /// `resolve.tsconfigPaths: true`: Vite 8's own resolver (oxc, the
    /// tsconfig that owns the importer), an exact key first, a declaration
    /// file served like any file.
    Native,
    /// Neither: Vite applies no `paths`. oj keeps them for what plain
    /// resolution does not find, in the plugin's model, so it agrees with
    /// Vite wherever Vite resolves.
    Fallback,
    /// No `paths` at all (Vite's module runner resolving an external).
    Off,
}

/// What the plugin's resolvers answered.
pub(crate) enum Mapped {
    File(PathBuf),
    /// Every answer was a declaration file.
    Declaration(PathBuf),
    None,
}

/// A project's `include`/`exclude`, compiled as the plugin's getIncluder.
struct Includer {
    dir: PathBuf,
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl Includer {
    fn new(project: &TsConfig, dir: &Path) -> Includer {
        let globs = |list: &[PathBuf]| -> Vec<Regex> {
            list.iter()
                .flat_map(|p| compiled_glob(&relative_glob(dir, p)))
                .collect()
        };
        let include = match &project.include {
            Some(list) => globs(list),
            None => globs(&[PathBuf::from("**/*")]),
        };
        let mut exclude = match &project.exclude {
            Some(list) => globs(list),
            None => globs(
                &["**/node_modules", "**/bower_components", "**/jspm_packages"].map(PathBuf::from),
            ),
        };
        if let Some(out) = &project.compiler_options.out_dir {
            exclude.extend(compiled_glob(&relative_glob(dir, out)));
        }
        Includer {
            dir: dir.to_path_buf(),
            include,
            exclude,
        }
    }

    /// Whether `exclude` takes `importer`'s directory: a file there, whatever
    /// its name (an importer known only by its directory).
    fn excludes_dir(&self, dir: &Path) -> bool {
        let probe = dir.join("__oj_importer__");
        let rel = probe
            .strip_prefix(&self.dir)
            .map(|r| format!("./{}", r.to_string_lossy()))
            .unwrap_or_else(|_| probe.to_string_lossy().into_owned());
        self.exclude.iter().any(|g| g.is_match(&rel))
    }

    fn includes(&self, importer: &Path) -> bool {
        if self.include.is_empty() && self.exclude.is_empty() {
            return true;
        }
        let rel = importer
            .strip_prefix(&self.dir)
            .map(|r| format!("./{}", r.to_string_lossy()))
            .unwrap_or_else(|_| importer.to_string_lossy().into_owned());
        self.include.iter().any(|g| g.is_match(&rel))
            && !self.exclude.iter().any(|g| g.is_match(&rel))
    }
}

/// `p` as the plugin writes a glob: relative to the project, `./`-led.
fn relative_glob(dir: &Path, p: &Path) -> String {
    let rel = p
        .strip_prefix(dir)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned();
    if rel.is_empty() {
        ".".to_string()
    } else if rel.starts_with("./") || rel.starts_with("../") || rel.starts_with('/') {
        rel
    } else {
        format!("./{rel}")
    }
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

/// Whether any project from `importer_dir` up has a `paths` pattern that
/// takes `id`, or a `baseUrl`: what a bundler applying `paths` natively could
/// map, whatever the importer.
pub(crate) fn could_map(projects: &[Arc<TsConfig>], id: &str) -> bool {
    let bare = !(id.starts_with('.') || id.starts_with('/') || id.contains('\0'));
    bare && projects.iter().any(|p| {
        p.compiler_options.base_url.is_some()
            || p.compiler_options
                .paths
                .as_ref()
                .is_some_and(|paths| paths.keys().any(|k| star_match(k, id).is_some()))
    })
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
fn takes_importer(project: &TsConfig, importer: &Path) -> bool {
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
        // The plugin's discovery never descends into node_modules or .git.
        let skipped = |d: &Path| {
            d.components()
                .any(|c| c.as_os_str() == "node_modules" || c.as_os_str() == ".git")
        };
        for d in dir
            .ancestors()
            .take_while(|d| d.starts_with(workspace))
            .filter(|d| !skipped(d))
        {
            for name in ["tsconfig.json", "jsconfig.json"] {
                let file = d.join(name);
                if !file.is_file() || all.iter().any(|p| p.path == file) {
                    continue;
                }
                if let Some(project) = load(&file) {
                    all.push(project);
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
        Arc::clone(map.entry(project.path.clone()).or_insert_with(|| {
            let dir = project.path.parent().unwrap_or(Path::new("/"));
            Arc::new(Includer::new(project, dir))
        }))
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
                        !takes_importer(project, importer)
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
                        let base = options.base_url.as_ref().filter(|_| !id.starts_with('/'))?;
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
