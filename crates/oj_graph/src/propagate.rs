//! HMR propagation (Vite's `propagateUpdate`): from a changed module up its
//! importers to the accepting boundaries, or a full reload.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ModuleGraph;

/// One boundary an update stops at (Vite's `PropagationBoundary`): the module
/// that accepts, the module it accepts (itself, or a declared dependency), and
/// whether the boundary sits inside an import cycle with the changed chain, in
/// which case a failed re-import must reset the page instead of surfacing an
/// error (Vite's `isWithinCircularImport`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UpdateTarget {
    pub boundary: PathBuf,
    pub accepted: PathBuf,
    pub within_circular_import: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HmrDecision {
    Update { boundaries: Vec<PathBuf> },
    FullReload { reason: String },
}

impl ModuleGraph {
    /// Whether every binding `importer` uses from `dep` falls inside `dep`'s
    /// accepted exports (Vite's `areAllImportsAccepted`): the update then
    /// never climbs through this importer.
    fn all_imports_accepted(
        &self,
        importer: &Path,
        dep: &Path,
        accepted: &HashSet<String>,
    ) -> bool {
        self.modules
            .get(importer)
            .and_then(|n| n.imported_bindings.get(dep))
            .is_some_and(|bindings| bindings.iter().all(|b| accepted.contains(b)))
    }

    fn accepts_dep(&self, importer: &Path, dep: &Path) -> bool {
        self.modules
            .get(importer)
            .is_some_and(|n| n.accepted_hmr_deps.contains(dep))
    }

    /// The update targets for a change. A self-accepting boundary accepts
    /// itself; an importer that declared the changed module (or a module on the
    /// way up) in `hot.accept(deps)` is the boundary for that dependency. `Err`
    /// means a full reload.
    pub fn update_targets(&self, changed: &Path) -> Result<Vec<UpdateTarget>, String> {
        if !self.modules.contains_key(changed) {
            return Err(format!("{} is not in the module graph", changed.display()));
        }
        self.collect_boundaries(&[changed], &[]).map(sorted)
    }

    /// The update targets when a module calls `import.meta.hot.invalidate()`:
    /// its own acceptance is skipped and the walk starts at its importers, as
    /// Vite's `updateModules(..., [...mod.importers])` does. A module nothing
    /// imports is an entry, so the page must reload.
    pub fn update_targets_from_importers(
        &self,
        changed: &Path,
    ) -> Result<Vec<UpdateTarget>, String> {
        let Some(node) = self.modules.get(changed) else {
            return Err(format!("{} is not in the module graph", changed.display()));
        };
        if node.importers.is_empty() {
            return Err(format!("{} invalidated at an entry", changed.display()));
        }
        let seeds: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
        self.collect_boundaries(&seeds, &[changed]).map(sorted)
    }

    pub fn propagate_update(&self, changed: &Path) -> HmrDecision {
        self.propagate_from_seeds(&[changed])
    }

    pub fn propagate_from_seeds(&self, seeds: &[&Path]) -> HmrDecision {
        for seed in seeds {
            if !self.modules.contains_key(*seed) {
                return HmrDecision::FullReload {
                    reason: format!("{} is not in the module graph", seed.display()),
                };
            }
        }
        match self.collect_boundaries(seeds, &[]) {
            Ok(targets) => HmrDecision::Update {
                boundaries: sorted(targets.into_iter().map(|t| t.boundary).collect()),
            },
            Err(reason) => HmrDecision::FullReload { reason },
        }
    }

    /// The dirty set an update of `changed` would stamp (its importers up to
    /// and including the accepting boundaries), without mutating anything.
    pub fn dirty_closure(&self, changed: &Path) -> Vec<PathBuf> {
        let mut dirty: Vec<PathBuf> = vec![changed.to_path_buf()];
        let mut queue = vec![changed.to_path_buf()];
        let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
        while let Some(current) = queue.pop() {
            let Some(node) = self.modules.get(&current) else {
                continue;
            };
            if node.is_self_accepting && current != changed {
                continue;
            }
            for importer in &node.importers {
                // A dep-accepting importer is not re-fetched: its callback receives
                // the new dependency module instead.
                if self.accepts_dep(importer, &current) {
                    continue;
                }
                if seen.insert(importer.clone()) {
                    dirty.push(importer.clone());
                    queue.push(importer.clone());
                }
            }
        }
        dirty.sort();
        dirty
    }

    fn collect_boundaries<'a>(
        &'a self,
        seeds: &[&'a Path],
        pre_stack: &[&'a Path],
    ) -> Result<Vec<UpdateTarget>, String> {
        let mut colors: HashMap<&'a Path, Color> = HashMap::new();
        for module in pre_stack {
            colors.insert(module, Color::Gray);
        }
        let mut boundaries: Vec<UpdateTarget> = Vec::new();
        for seed in seeds {
            self.climb(seed, &mut colors, &mut boundaries)?;
        }
        Ok(boundaries)
    }

    /// Whether `boundary` is imported, directly or through other modules, by a
    /// module on the current change chain (the gray nodes of the walk, plus the
    /// boundary itself): Vite's `isNodeWithinCircularImports`. Stylesheet
    /// importers are skipped as there, and a module's direct self-import is not
    /// a cycle.
    fn is_within_circular_imports(&self, boundary: &Path, colors: &HashMap<&Path, Color>) -> bool {
        let mut stack = vec![boundary];
        let mut seen: HashSet<&Path> = HashSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            let Some(node) = self.modules.get(current) else {
                continue;
            };
            for importer in &node.importers {
                let importer = importer.as_path();
                if importer == current || is_css_path(importer) {
                    continue;
                }
                if importer == boundary || colors.get(importer) == Some(&Color::Gray) {
                    return true;
                }
                stack.push(importer);
            }
        }
        false
    }

    fn target(
        &self,
        boundary: &Path,
        accepted: &Path,
        colors: &HashMap<&Path, Color>,
    ) -> UpdateTarget {
        UpdateTarget {
            boundary: boundary.to_path_buf(),
            accepted: accepted.to_path_buf(),
            within_circular_import: self.is_within_circular_imports(boundary, colors),
        }
    }

    fn climb<'a>(
        &'a self,
        seed: &'a Path,
        colors: &mut HashMap<&'a Path, Color>,
        boundaries: &mut Vec<UpdateTarget>,
    ) -> Result<(), String> {
        enum Step<'s> {
            Enter(&'s Path),
            Blacken(&'s Path),
        }

        let mut stack = vec![Step::Enter(seed)];
        while let Some(step) = stack.pop() {
            let current = match step {
                Step::Blacken(path) => {
                    colors.insert(path, Color::Black);
                    continue;
                }
                Step::Enter(path) => path,
            };
            match colors.get(current) {
                Some(Color::Black) => continue,
                // A circular import: skipped like Vite does, so barrel-file cycles
                // don't turn every edit into a reload; only an entry with no
                // accepting module does.
                Some(Color::Gray) => continue,
                None => {}
            }
            let Some(node) = self.modules.get(current) else {
                return Err(format!("{} is not in the module graph", current.display()));
            };
            if node.is_self_accepting {
                boundaries.push(self.target(current, current, colors));
                colors.insert(current, Color::Black);
                continue;
            }
            // A partially accepting module (`acceptExports`) is a boundary, and
            // each importer is then gated on the bindings it uses (Vite's
            // acceptedHmrExports + importedBindings). Without importers it is
            // self-accepting, never a dead end.
            let accepted_exports = node.accepted_exports.as_ref();
            if accepted_exports.is_some() {
                boundaries.push(self.target(current, current, colors));
            } else if node.importers.is_empty() {
                return Err(format!(
                    "update reached entry {} with no accepting boundary",
                    current.display()
                ));
            }
            colors.insert(current, Color::Gray);
            stack.push(Step::Blacken(current));
            let mut importers: Vec<&Path> = node.importers.iter().map(PathBuf::as_path).collect();
            importers.sort_unstable();
            for importer in importers.into_iter().rev() {
                // An importer that accepts `current` via hot.accept(deps) is the
                // boundary for this change; the walk does not continue above it.
                if self.accepts_dep(importer, current) {
                    boundaries.push(self.target(importer, current, colors));
                    continue;
                }
                // An importer using only accepted exports is already covered by
                // the partial boundary above (Vite's areAllImportsAccepted).
                if let Some(accepted) = accepted_exports {
                    if self.all_imports_accepted(importer, current, accepted) {
                        continue;
                    }
                }
                stack.push(Step::Enter(importer));
            }
        }
        Ok(())
    }
}

/// Sorted, without duplicates.
fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items.dedup();
    items
}

fn is_css_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" | "sss")
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Color {
    Gray,
    Black,
}
