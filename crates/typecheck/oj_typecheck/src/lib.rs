// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TypeScript type checking for an app root, on the tsrs checker (vendored
//! under `vendor/tsrs`).
//!
//! A [`Checker`] keeps one tsrs API session open: the programs of every
//! project `tsc -b` would build stay loaded, and [`Checker::changed`] moves
//! them to the next snapshot, so a check after an edit re-parses only the
//! files that changed. Old snapshots are released, which frees their memory
//! (tsrs keeps each program version in a region of its own).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tsrs_api::{Handler, Response, Session, SessionOptions};
use tsrs_vfs::{FS, bundled, osvfs};

/// The file a check starts from, as `tsc -b` with no arguments.
pub const ROOT_CONFIG: &str = "tsconfig.json";

/// Extensions whose changes can affect a program: sources, declarations,
/// configs and `package.json` (module resolution).
const WATCHED_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "json"];

/// Whether a change to `path` can affect a check.
pub fn is_relevant(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| WATCHED_EXTENSIONS.contains(&e))
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

pub struct Checker {
    session: Arc<Session>,
    root: PathBuf,
    snapshot: u64,
    projects: Vec<String>,
    generation: u64,
}

impl Checker {
    /// Opens the projects `tsc -b` builds from `root/tsconfig.json`: the
    /// root config when it lists files, and every config it references,
    /// transitively. `Ok(None)` when the root has no `tsconfig.json`.
    pub fn open(root: &Path) -> Result<Option<Checker>, String> {
        let root = &root
            .canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?;
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
        let configs = build_configs(&session, &slash(&root_config))?;
        let opened = request(
            &session,
            "createSnapshot",
            &json!({ "openProjects": configs }),
        )?;
        let snapshot = snapshot_id(&opened)?;
        let projects = opened["projects"]
            .as_array()
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| p["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(Checker {
            session,
            root: root.to_path_buf(),
            snapshot,
            projects,
            generation: 1,
        }))
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves to a snapshot with `paths` re-read from disk. Each path counts
    /// as created, changed or deleted by whether it exists now and whether
    /// a program already holds it.
    pub fn changed<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> Result<(), String> {
        let (mut changed, mut created, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
        let mut seen = BTreeSet::new();
        for path in paths {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.root.join(path)
            };
            if !is_relevant(&path) || !seen.insert(path.clone()) {
                continue;
            }
            let name = slash(&path);
            if !path.exists() {
                deleted.push(name);
            } else if self.known(&name) {
                changed.push(name);
            } else {
                created.push(name);
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

    /// Whether the current programs contain `file` (an absolute path).
    fn known(&self, file: &str) -> bool {
        let found = request(
            &self.session,
            "getDefaultProjectForFile",
            &json!({ "snapshot": self.snapshot, "file": file }),
        );
        matches!(found, Ok(v) if !v.is_null())
    }

    /// Every error and warning of the current snapshot, sorted by file and
    /// position, each reported once.
    pub fn check(&mut self) -> Result<Report, String> {
        let started = Instant::now();
        let mut found = BTreeSet::new();
        for project in &self.projects {
            let params = json!({ "snapshot": self.snapshot, "project": project });
            for method in [
                "getConfigFileParsingDiagnostics",
                "getProgramDiagnostics",
                "getSyntacticDiagnostics",
                "getGlobalDiagnostics",
                "getSemanticDiagnostics",
            ] {
                let diagnostics = request(&self.session, method, &params)?;
                for d in diagnostics.as_array().into_iter().flatten() {
                    if let Some(d) = self.diagnostic(d) {
                        found.insert(d);
                    }
                }
            }
        }
        Ok(Report {
            diagnostics: found.into_iter().collect(),
            generation: self.generation,
            duration: started.elapsed(),
        })
    }

    fn diagnostic(&self, d: &Value) -> Option<Diagnostic> {
        let category = match d["category"].as_u64()? {
            0 => Category::Warning,
            1 => Category::Error,
            _ => return None,
        };
        let file = d["fileName"].as_str().map(|f| {
            Path::new(f)
                .strip_prefix(&self.root)
                .map(slash)
                .unwrap_or_else(|_| f.to_string())
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
}

impl Drop for Checker {
    fn drop(&mut self) {
        self.session.close();
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

/// The configs `tsc -b root_config` builds, references before the configs
/// that reference them. A solution config (references and no files of its
/// own) is left out: its program is empty.
fn build_configs(session: &Session, root_config: &str) -> Result<Vec<String>, String> {
    let mut order = Vec::new();
    let mut visited = BTreeSet::new();
    visit_config(session, root_config, &mut visited, &mut order)?;
    Ok(order)
}

fn visit_config(
    session: &Session,
    config: &str,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if !visited.insert(config.to_string()) {
        return Ok(());
    }
    let parsed = request(session, "parseConfigFile", &json!({ "file": config }))?;
    let references = parsed["projectReferences"].as_array();
    for reference in references.into_iter().flatten() {
        let Some(path) = reference["path"].as_str() else {
            continue;
        };
        let referenced = if Path::new(path).is_dir() {
            format!("{path}/{ROOT_CONFIG}")
        } else {
            path.to_string()
        };
        visit_config(session, &referenced, visited, order)?;
    }
    let lists_files = parsed["fileNames"]
        .as_array()
        .is_some_and(|f| !f.is_empty());
    if lists_files || references.is_none_or(|r| r.is_empty()) {
        order.push(config.to_string());
    }
    Ok(())
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

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
