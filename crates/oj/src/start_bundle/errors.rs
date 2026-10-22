// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How a Start bundle fails, the way Vite's bundles fail: an import nothing
//! resolved is an error (Vite's onRollupLog, where rolldown would warn and
//! leave it external, shipping a bare specifier the browser cannot load),
//! and a failure carries Vite's ErrorPayload fields (`message`, `id`,
//! `frame`, `loc`, `plugin`) for the dev server's error overlay.

use std::path::Path;

/// A failed Start bundle, as Vite's prepareError presents one.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BundleError {
    pub message: String,
    pub id: Option<String>,
    pub frame: String,
    pub plugin: Option<String>,
    /// `(file, line, column)`, 1-based line and 0-based column. Boxed: the
    /// error rides in every bundle `Result`, and is rarely set.
    pub loc: Option<Box<(String, usize, usize)>>,
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if !self.frame.is_empty() {
            write!(f, "\n{}", self.frame)?;
        }
        Ok(())
    }
}

impl std::error::Error for BundleError {}

impl BundleError {
    /// Vite's ErrorPayload `err` (middlewares/error.ts prepareError), what
    /// client-error.json holds and the overlay renders.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "message": strip_ansi(&self.message),
            "stack": "",
            "id": self.id,
            "frame": strip_ansi(&self.frame),
            "plugin": self.plugin,
            "loc": self.loc.as_deref().map(|(file, line, column)| serde_json::json!({
                "file": file,
                "line": line,
                "column": column,
            })),
        })
    }

    /// A bundle failure from anything else (a plugin hook's error text).
    pub fn from_message(message: impl Into<String>) -> BundleError {
        BundleError {
            message: message.into(),
            ..Default::default()
        }
    }

    /// The first of rolldown's diagnostics, rendered as Vite renders it.
    pub fn from_diagnostics(errs: &[rolldown_error::BuildDiagnostic]) -> BundleError {
        let Some(first) = errs.first() else {
            return BundleError::from_message("the bundle failed");
        };
        if matches!(first.kind(), rolldown_error::EventKind::UnresolvedImport) {
            if let (Some(spec), Some(importer)) = (first.exporter(), first.id()) {
                return unresolved_import(&spec, &importer);
            }
        }
        let message = errs
            .iter()
            .map(|e| e.to_diagnostic().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Where it failed (a syntax error's file:line:column), and that spot
        // of the file, for the overlay's link and frame.
        let loc = first
            .to_diagnostic()
            .get_primary_location()
            .map(|(file, line, column, _)| (file, line, column));
        let frame = loc
            .as_ref()
            .and_then(|(file, line, column)| source_frame(Path::new(file), *line, *column))
            .unwrap_or_default();
        BundleError {
            message,
            id: first.id().or_else(|| loc.as_ref().map(|l| l.0.clone())),
            plugin: first.plugin(),
            frame,
            loc: loc.map(Box::new),
        }
    }
}

/// The unresolved import a bundle's warnings report, if any (rolldown warns
/// and keeps the import external; Vite fails on it). `?commonjs-external`
/// importers are exempt, as there.
pub(crate) fn unresolved_import_in(
    warnings: &[rolldown_error::BuildDiagnostic],
) -> Option<BundleError> {
    warnings.iter().find_map(|w| {
        if !matches!(w.kind(), rolldown_error::EventKind::UnresolvedImport) {
            return None;
        }
        let importer = w.id()?;
        if importer.ends_with("?commonjs-external") {
            return None;
        }
        Some(unresolved_import(&w.exporter()?, &importer))
    })
}

/// Vite's error for an import nothing resolved, with the importer's line
/// framed (the frame of rolldown's own warning says it is "treating it as
/// an external dependency", which this error is not).
pub(crate) fn unresolved_import(spec: &str, importer: &str) -> BundleError {
    let message = format!(
        "[vite]: Rolldown failed to resolve import \"{spec}\" from \"{importer}\".\n\
         This is most likely unintended because it can break your application at runtime.\n\
         If you do want to externalize this module explicitly add it to\n\
         `build.rolldownOptions.external`"
    );
    let file = importer.split('?').next().unwrap_or(importer);
    let loc = locate(file, spec);
    let frame = loc
        .as_ref()
        .and_then(|(file, line, column)| source_frame(Path::new(file), *line, *column))
        .unwrap_or_default();
    BundleError {
        message,
        id: Some(importer.to_string()),
        frame,
        plugin: None,
        loc: loc.map(Box::new),
    }
}

/// Where `spec` is imported in `file`: the first line quoting it, the column
/// of its opening quote.
fn locate(file: &str, spec: &str) -> Option<(String, usize, usize)> {
    let source = std::fs::read_to_string(file).ok()?;
    let quoted = [format!("\"{spec}\""), format!("'{spec}'")];
    source.lines().enumerate().find_map(|(i, line)| {
        quoted.iter().find_map(|q| {
            line.find(q.as_str())
                .map(|at| (file.to_string(), i + 1, line[..at].chars().count()))
        })
    })
}

/// Vite's generateCodeFrame layout for one position: two lines of context
/// either side and a caret under the column.
pub(crate) fn source_frame(file: &Path, line: usize, column: usize) -> Option<String> {
    let source = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    if line == 0 || line > lines.len() {
        return None;
    }
    let at = line - 1;
    let last = (line + 2).min(lines.len());
    let width = (last.to_string().len() + 1).max(3);
    let mut out = Vec::new();
    for (i, text) in lines
        .iter()
        .enumerate()
        .take(lines.len().min(at + 3))
        .skip(at.saturating_sub(2))
    {
        let n = (i + 1).to_string();
        out.push(format!("{}{n}|  {text}", " ".repeat(width - n.len())));
        if i == at {
            out.push(format!("{}|  {}^", " ".repeat(width), " ".repeat(column)));
        }
    }
    Some(out.join("\n"))
}

fn strip_ansi(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new("\x1b\\[[0-9;]*m").unwrap());
    RE.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A syntax error carries its location and the frame around it, as
    // rolldown's own errors do under Vite.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_syntax_error_carries_its_location_and_frame() {
        let dir = std::env::temp_dir().join(format!("oj-bundle-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.canonicalize().unwrap();
        let file = root.join("broken.js");
        std::fs::write(&file, "const ok = 1;\nconst = ;\n").unwrap();
        let mut bundler = rolldown::BundlerBuilder::default()
            .with_options(rolldown::BundlerOptions {
                input: Some(vec![rolldown::InputItem {
                    name: None,
                    import: file.to_string_lossy().into_owned(),
                }]),
                cwd: Some(root.clone()),
                ..Default::default()
            })
            .build()
            .unwrap();
        let Err(errs) = bundler.generate().await else {
            panic!("a syntax error fails the bundle");
        };
        let errs = errs.into_vec();
        let err = BundleError::from_diagnostics(&errs);
        let (at, line, _) = *err.loc.clone().expect("a location");
        assert!(at.ends_with("broken.js"), "{at}");
        assert_eq!(line, 2);
        assert!(err.frame.contains("2|  const = ;"), "{}", err.frame);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolved_import_is_framed_from_the_source() {
        let dir = std::env::temp_dir().join(format!("oj-start-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("typo.js");
        std::fs::write(
            &file,
            "import { thing } from \"no-such-dep-typo\";\nconsole.log(thing);\n",
        )
        .unwrap();
        let err = unresolved_import("no-such-dep-typo", &file.to_string_lossy());
        assert!(err
            .message
            .contains("Rolldown failed to resolve import \"no-such-dep-typo\""));
        assert!(err.message.contains("build.rolldownOptions.external"));
        assert_eq!(err.loc.as_ref().map(|l| (l.1, l.2)), Some((1, 22)));
        assert!(
            err.frame
                .starts_with("  1|  import { thing } from \"no-such-dep-typo\";"),
            "{}",
            err.frame
        );
        assert!(
            err.frame.contains("\n   |  ") && err.frame.contains('^'),
            "{}",
            err.frame
        );
        assert!(!err.frame.contains("external dependency"));
        let payload = err.payload();
        assert_eq!(payload["stack"], "");
        assert_eq!(payload["loc"]["line"], 1);
        assert!(payload["id"].as_str().unwrap().ends_with("typo.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frames_clip_at_the_file_edges() {
        let dir = std::env::temp_dir().join(format!("oj-start-frame-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.ts");
        std::fs::write(&file, "a\nb\nc\nd\ne\nf\n").unwrap();
        let frame = source_frame(&file, 5, 1).unwrap();
        assert_eq!(
            frame,
            "  3|  c\n  4|  d\n  5|  e\n   |   ^\n  6|  f\n  7|  "
        );
        assert!(source_frame(&file, 0, 0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ansi_codes_leave_the_payload() {
        let err = BundleError::from_message("\x1b[31mboom\x1b[0m");
        assert_eq!(err.payload()["message"], "boom");
    }
}
