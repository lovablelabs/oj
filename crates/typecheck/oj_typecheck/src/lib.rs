// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TypeScript type checking for an app root, on the tsrs checker (vendored
//! as the `oj_tsrs_*` crates).
//!
//! A [`Checker`] keeps one tsrs API session open: the programs of every
//! project `tsc -b` would build stay loaded, and [`Checker::changed`] moves
//! them to the next snapshot, so a check after an edit re-parses only the
//! files that changed. Old snapshots are released, which frees their memory
//! (tsrs keeps each program version in a region of its own). A change to a
//! config, `package.json` or a lockfile reopens the session instead: it can
//! change the set of projects and how every import resolves.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tsrs_api::{Handler, Response, Session, SessionOptions};
use tsrs_vfs::{FS, bundled, osvfs};

/// The file a check starts from, as `tsc -b` with no arguments.
pub const ROOT_CONFIG: &str = "tsconfig.json";

/// Extensions whose changes can affect a program: sources, declarations and
/// configs.
const WATCHED_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "json"];

/// Package manager lockfiles: an install rewrites one even when
/// `package.json` stays the same.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/// Whether a change to `path` can affect a check. A path without an
/// extension may be a directory that moved or was removed.
pub fn is_relevant(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => WATCHED_EXTENSIONS.contains(&ext) || is_lockfile(path),
        None => true,
    }
}

fn is_lockfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| LOCKFILES.contains(&n))
}

/// A config or manifest whose change reopens the projects.
fn reopens(path: &Path, configs: &BTreeSet<String>) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    configs.contains(&slash(path))
        || ((name.starts_with("tsconfig") || name.starts_with("jsconfig"))
            && name.ends_with(".json"))
        || name == "package.json"
        || is_lockfile(path)
}

/// One reported problem, positions 1-based as tsc prints them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Diagnostic {
    /// Relative to the app root; `None` for global and options diagnostics.
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
    pub code: u32,
    pub category: Category,
    /// The message, with chained detail lines indented below it as tsc does.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Error,
    Warning,
}

/// The outcome of one check.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
    /// Counts the snapshots this checker has seen; a report answers for one.
    pub generation: u64,
    #[serde(rename = "durationMs", serialize_with = "millis")]
    pub duration: Duration,
}

fn millis<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_millis() as u64)
}

impl Report {
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.category == Category::Error)
            .count()
    }

    /// tsc's `--pretty false` output: one `file(line,col): error TSn: message`
    /// block per diagnostic.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for d in &self.diagnostics {
            let category = match d.category {
                Category::Error => "error",
                Category::Warning => "warning",
            };
            if let Some(file) = &d.file {
                out.push_str(&format!("{file}({},{}): ", d.line, d.column));
            }
            out.push_str(&format!("{category} TS{}: {}\n", d.code, d.message));
        }
        out
    }
}

struct Project {
    id: String,
    /// `declaration` or `composite`: `tsc --noEmit` then also reports
    /// declaration diagnostics.
    emits_declarations: bool,
}

pub struct Checker {
    session: Arc<Session>,
    root: PathBuf,
    snapshot: u64,
    /// The opened projects, in `tsc -b` build order.
    projects: Vec<Project>,
    /// Every config the reference walk visited, opened or not.
    configs: BTreeSet<String>,
    /// Problems of the configs no program reports: a solution config's own
    /// errors and references to configs that do not exist.
    solution: Vec<Diagnostic>,
    generation: u64,
}

/// What a signature job did (`Checker::wait_for_signatures`).
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct SignatureJob {
    /// The files it emitted, in order, with each one's emit time.
    pub emitted: Vec<(String, Duration)>,
    /// Every file a job of this checker found slow, which later jobs skip.
    pub slow: Vec<String>,
}

impl Checker {
    /// Opens the projects `tsc -b` builds from `root/tsconfig.json`: the
    /// root config when it lists files, and every config it references,
    /// transitively. `Ok(None)` when the root has no `tsconfig.json`.
    pub fn open(root: &Path) -> Result<Option<Checker>, String> {
        let root = &root
            .canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?;
        Checker::load(root, 1)
    }

    fn load(root: &Path, generation: u64) -> Result<Option<Checker>, String> {
        let root_config = root.join(ROOT_CONFIG);
        if !root_config.is_file() {
            return Ok(None);
        }
        let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(osvfs::fs()));
        let session = Session::new(SessionOptions::new(
            slash(root),
            bundled::default_lib_path(),
            fs,
            false,
        ));
        let mut checker = Checker {
            session,
            root: root.to_path_buf(),
            snapshot: 0,
            projects: Vec::new(),
            configs: BTreeSet::new(),
            solution: Vec::new(),
            generation,
        };
        let mut walk = Walk::default();
        checker.visit_config(&slash(&root_config), &mut walk)?;
        checker.configs = walk.visited;
        checker.solution = walk.problems;
        let opened = request(
            &checker.session,
            "createSnapshot",
            &json!({ "openProjects": walk.order }),
        )?;
        checker.snapshot = snapshot_id(&opened)?;
        let mut by_config: BTreeMap<String, Project> = BTreeMap::new();
        for p in opened["projects"].as_array().into_iter().flatten() {
            let (Some(id), Some(config)) = (p["id"].as_str(), p["configFileName"].as_str()) else {
                continue;
            };
            let options = &p["parsedCommandLine"]["options"];
            let emits_declarations = options["declaration"].as_bool() == Some(true)
                || options["composite"].as_bool() == Some(true);
            by_config.insert(
                config.to_string(),
                Project {
                    id: id.to_string(),
                    emits_declarations,
                },
            );
        }
        checker.projects = walk
            .order
            .iter()
            .filter_map(|config| by_config.remove(config))
            .collect();
        Ok(Some(checker))
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves to a snapshot with `paths` re-read from disk. A path that no
    /// longer exists is deleted, with every program file under it when it
    /// was a directory. A config, `package.json` or lockfile among them
    /// reopens the projects.
    pub fn changed<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> Result<(), String> {
        let paths: BTreeSet<PathBuf> = paths
            .into_iter()
            .map(|p| {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    self.root.join(p)
                }
            })
            .filter(|p| is_relevant(p))
            .collect();
        if paths.is_empty() {
            return Ok(());
        }
        if paths.iter().any(|p| reopens(p, &self.configs)) {
            return self.reopen();
        }
        let (mut changed, mut created, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
        let mut program_files = None;
        for path in &paths {
            let name = slash(path);
            if path.is_file() {
                if !self.known(&name) {
                    created.push(name.clone());
                }
                changed.push(name);
            } else if !path.exists() {
                if path.extension().is_some() {
                    deleted.push(name);
                } else {
                    let files = program_files.get_or_insert_with(|| self.program_files());
                    let prefix = format!("{name}/");
                    deleted.extend(files.iter().filter(|f| f.starts_with(&prefix)).cloned());
                }
            }
        }
        if changed.is_empty() && created.is_empty() && deleted.is_empty() {
            return Ok(());
        }
        let next = request(
            &self.session,
            "updateSnapshot",
            &json!({
                "snapshot": self.snapshot,
                "changes": {
                    "ensurePrograms": true,
                    "fileNotifications": { "changed": changed, "created": created, "deleted": deleted },
                },
            }),
        )?;
        let snapshot = snapshot_id(&next)?;
        let _ = request(
            &self.session,
            "release",
            &json!({ "snapshot": self.snapshot }),
        );
        self.snapshot = snapshot;
        self.generation += 1;
        Ok(())
    }

    fn reopen(&mut self) -> Result<(), String> {
        let generation = self.generation + 1;
        match Checker::load(&self.root, generation)? {
            Some(fresh) => {
                *self = fresh;
                Ok(())
            }
            None => Err(format!("{ROOT_CONFIG} was removed from the app root")),
        }
    }

    /// Whether the current programs contain `file` (an absolute path).
    fn known(&self, file: &str) -> bool {
        let found = request(
            &self.session,
            "getDefaultProjectForFile",
            &json!({ "snapshot": self.snapshot, "file": file }),
        );
        matches!(found, Ok(v) if !v.is_null())
    }

    /// Every source file of the current programs.
    fn program_files(&self) -> BTreeSet<String> {
        let mut files = BTreeSet::new();
        for project in &self.projects {
            let names = request(
                &self.session,
                "getSourceFileNames",
                &json!({ "snapshot": self.snapshot, "project": project.id }),
            );
            if let Ok(names) = names {
                files.extend(
                    names
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|n| n.as_str().map(str::to_owned)),
                );
            }
        }
        files
    }

    /// Every error and warning of the current snapshot: each project's in
    /// tsc's order, a diagnostic two projects share once, then the problems
    /// of configs no program reports.
    pub fn check(&mut self) -> Result<Report, String> {
        let started = Instant::now();
        let mut seen = BTreeSet::new();
        let mut diagnostics = Vec::new();
        for project in &self.projects {
            let mut found = self.project_diagnostics(project)?;
            found.sort();
            found.dedup();
            for d in found {
                if seen.insert(d.clone()) {
                    diagnostics.push(d);
                }
            }
        }
        for d in &self.solution {
            if seen.insert(d.clone()) {
                diagnostics.push(d.clone());
            }
        }
        Ok(Report {
            diagnostics,
            generation: self.generation,
            duration: started.elapsed(),
        })
    }

    /// Records, in the background, the declaration signatures of the files the last check did not emit, so the
    /// next edit of a file whose exported types stay the same checks only that file. For a checker that waits
    /// for the next change: the next request stops it, but not in the middle of one file, so `skip` names the
    /// files known to be slow to emit (a generated route tree). Those, and the ones an earlier job found slow,
    /// keep no signature: a change to one re-checks the files that import it.
    pub fn record_signatures(&self, skip: &[PathBuf]) {
        self.start_signature_job(skip, None);
    }

    /// `record_signatures`, counting a file as slow once its emit takes
    /// `slow` (1 s otherwise). For tests.
    #[doc(hidden)]
    pub fn record_signatures_slow_after(&self, skip: &[PathBuf], slow: Duration) {
        self.start_signature_job(skip, Some(slow));
    }

    fn start_signature_job(&self, skip: &[PathBuf], slow: Option<Duration>) {
        let skip: Vec<String> = skip.iter().map(|p| slash(p)).collect();
        let mut params = json!({ "skip": skip });
        if let Some(slow) = slow {
            params["slowMs"] = json!(slow.as_millis() as u64);
        }
        let _ = request(&self.session, "ojStartSignatureJob", &params);
    }

    /// Waits for the signature job `record_signatures` started to finish
    /// instead of stopping it as any other request does. For tests.
    #[doc(hidden)]
    pub fn wait_for_signatures(&self) -> SignatureJob {
        let report = request(&self.session, "ojWaitSignatureJob", &json!({})).unwrap_or_default();
        let names = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        };
        SignatureJob {
            emitted: report["emitted"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| {
                    let took = Duration::from_secs_f64(e["ms"].as_f64()? / 1000.0);
                    Some((e["file"].as_str()?.to_string(), took))
                })
                .collect(),
            slow: names(&report["slow"]),
        }
    }

    /// Each project with an incremental program, and whether that program
    /// holds the current snapshot (not an older one). For tests.
    #[doc(hidden)]
    pub fn incremental_programs(&self) -> Vec<(String, bool)> {
        let report = request(
            &self.session,
            "ojIncrementalPrograms",
            &json!({ "snapshot": self.snapshot }),
        )
        .unwrap_or_default();
        report
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["project"].as_str()?.to_string(), p["current"].as_bool()?)))
            .collect()
    }

    /// tsc's `getDiagnosticsOfAnyProgram`: config parsing, then syntactic;
    /// only without syntax errors the program, global and semantic ones; and
    /// only when those found nothing, the declaration ones.
    fn project_diagnostics(&self, project: &Project) -> Result<Vec<Diagnostic>, String> {
        let params = json!({ "snapshot": self.snapshot, "project": project.id });
        let get = |method: &str| -> Result<Vec<Diagnostic>, String> {
            let found = request(&self.session, method, &params)?;
            Ok(found
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| self.diagnostic(d))
                .collect())
        };
        let mut all = get("getConfigFileParsingDiagnostics")?;
        let config_count = all.len();
        all.extend(get("getSyntacticDiagnostics")?);
        if all.len() != config_count {
            return Ok(all);
        }
        all.extend(get("getProgramDiagnostics")?);
        all.extend(get("ojGetGlobalDiagnostics")?);
        if all.len() == config_count {
            all.extend(get("ojGetIncrementalSemanticDiagnostics")?);
            all.extend(get("ojGetGlobalDiagnostics")?);
        }
        if project.emits_declarations && all.len() == config_count {
            all.extend(get("getDeclarationDiagnostics")?);
        }
        Ok(all)
    }

    fn diagnostic(&self, d: &Value) -> Option<Diagnostic> {
        let category = match d["category"].as_u64()? {
            0 => Category::Warning,
            1 => Category::Error,
            _ => return None,
        };
        let file = d["fileName"].as_str().map(|f| {
            let prefix = format!("{}/", slash(&self.root));
            f.strip_prefix(&prefix).unwrap_or(f).to_string()
        });
        let at = |k: &str| d["startPosition"][k].as_u64().unwrap_or(0) as u32 + 1;
        let mut message = d["text"].as_str().unwrap_or_default().to_string();
        flatten_chain(&d["messageChain"], 1, &mut message);
        Some(Diagnostic {
            line: if file.is_some() { at("line") } else { 0 },
            column: if file.is_some() { at("character") } else { 0 },
            file,
            code: d["code"].as_u64().unwrap_or(0) as u32,
            category,
            message,
        })
    }

    /// Walks `config` and its references depth first, references before the
    /// configs that reference them, as `tsc -b` orders its builds. A
    /// solution config (references and no files of its own) is not opened:
    /// its program is empty, so its own errors are kept in `walk.problems`.
    fn visit_config(&self, config: &str, walk: &mut Walk) -> Result<(), String> {
        if !walk.visited.insert(config.to_string()) {
            return Ok(());
        }
        if !Path::new(config).is_file() {
            // tsc's TS6053, which `tsc -b` reports without a location.
            walk.problems.push(Diagnostic {
                file: None,
                line: 0,
                column: 0,
                code: 6053,
                category: Category::Error,
                message: format!("File '{config}' not found."),
            });
            return Ok(());
        }
        let parsed = request(&self.session, "parseConfigFile", &json!({ "file": config }))?;
        let references = parsed["projectReferences"].as_array();
        for reference in references.into_iter().flatten() {
            if let Some(path) = reference["path"].as_str() {
                self.visit_config(&reference_config(path), walk)?;
            }
        }
        let lists_files = parsed["fileNames"]
            .as_array()
            .is_some_and(|f| !f.is_empty());
        if lists_files || references.is_none_or(|r| r.is_empty()) {
            walk.order.push(config.to_string());
        } else {
            walk.problems.extend(
                parsed["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| self.diagnostic(d)),
            );
        }
        Ok(())
    }
}

impl Drop for Checker {
    fn drop(&mut self) {
        self.session.close();
    }
}

#[derive(Default)]
struct Walk {
    visited: BTreeSet<String>,
    order: Vec<String>,
    problems: Vec<Diagnostic>,
}

/// tsc's `resolveConfigFileProjectName`: a reference that does not name a
/// `.json` file names a directory holding a `tsconfig.json`.
fn reference_config(path: &str) -> String {
    if path.ends_with(".json") {
        path.to_string()
    } else {
        format!("{}/{ROOT_CONFIG}", path.trim_end_matches('/'))
    }
}

/// tsc's `flattenDiagnosticMessageText`: each chain level on its own line,
/// two spaces deeper than its parent.
fn flatten_chain(chain: &Value, depth: usize, out: &mut String) {
    for link in chain.as_array().into_iter().flatten() {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
        out.push_str(link["text"].as_str().unwrap_or_default());
        flatten_chain(&link["messageChain"], depth + 1, out);
    }
}

fn request(session: &Session, method: &str, params: &Value) -> Result<Value, String> {
    let params = params.to_string();
    match session.handle_request(method, params.as_bytes()) {
        Ok(Response::Json(text)) => {
            serde_json::from_str(&text).map_err(|e| format!("tsrs {method}: {e}"))
        }
        Ok(Response::Binary(_)) => Err(format!("tsrs {method}: unexpected binary response")),
        Err(e) => Err(format!("tsrs {method}: {e}")),
    }
}

fn snapshot_id(response: &Value) -> Result<u64, String> {
    response["snapshot"]
        .as_u64()
        .ok_or_else(|| format!("tsrs snapshot response without an id: {response}"))
}

/// The path as tsrs spells it: forward slashes, without Windows' verbatim
/// `\\?\` prefix that `canonicalize` adds.
fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('\\', "/")
}
