// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Model-based property test of the module graph. Random sequences of the
//! operations the dev server performs (link a module's imports, mark it a
//! refresh boundary, propagate a change) are applied to `ModuleGraph` and to a
//! plain adjacency-list model, and the graph's structure and every HMR
//! decision are checked against that model after each step.
//!
//! The model deliberately recomputes everything from scratch: the point is to
//! catch incremental bookkeeping (stale importer edges, colors left behind by
//! a previous walk) drifting away from the truth.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use oj_graph::{HmrDecision, ModuleGraph};
use proptest::prelude::*;

const MODULES: u8 = 8;

fn path(slot: u8) -> PathBuf {
    PathBuf::from(format!("/src/m{slot}.tsx"))
}

#[derive(Clone, Debug)]
enum Op {
    /// `set_imports`: the dev server re-links a module after compiling it.
    Link { importer: u8, imports: Vec<u8> },
    /// A single edge, as the crawler adds them.
    AddImport { importer: u8, imported: u8 },
    /// The compile output said whether the module self-accepts.
    Accept { module: u8, accepting: bool },
    /// A file changed.
    Change { module: u8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..MODULES, proptest::collection::vec(0..MODULES, 0..4))
            .prop_map(|(importer, imports)| Op::Link { importer, imports }),
        (0..MODULES, 0..MODULES).prop_map(|(importer, imported)| Op::AddImport {
            importer,
            imported
        }),
        (0..MODULES, any::<bool>()).prop_map(|(module, accepting)| Op::Accept { module, accepting }),
        (0..MODULES).prop_map(|module| Op::Change { module }),
    ]
}

/// The reference: who imports whom, and who accepts.
#[derive(Default)]
struct Model {
    /// importer -> imports
    imports: BTreeMap<u8, BTreeSet<u8>>,
    accepting: BTreeSet<u8>,
    known: BTreeSet<u8>,
}

impl Model {
    fn importers_of(&self, module: u8) -> BTreeSet<u8> {
        self.imports
            .iter()
            .filter(|(_, deps)| deps.contains(&module))
            .map(|(importer, _)| *importer)
            .collect()
    }

    fn link(&mut self, importer: u8, imports: &[u8]) {
        self.known.insert(importer);
        self.known.extend(imports.iter().copied());
        self.imports
            .insert(importer, imports.iter().copied().collect());
    }

    fn add_import(&mut self, importer: u8, imported: u8) {
        self.known.insert(importer);
        self.known.insert(imported);
        self.imports.entry(importer).or_default().insert(imported);
    }

    /// The reference climb, recomputed from scratch: walk up from each seed to
    /// the nearest accepting module. `Err` means "full reload", carrying the
    /// same distinction the graph makes.
    fn climb(&self, seeds: &[u8], pre_gray: &[u8]) -> Result<BTreeSet<u8>, Reload> {
        let mut boundaries = BTreeSet::new();
        let mut gray: BTreeSet<u8> = pre_gray.iter().copied().collect();
        let mut black: BTreeSet<u8> = BTreeSet::new();
        for seed in seeds {
            self.visit(*seed, &mut gray, &mut black, &mut boundaries)?;
        }
        Ok(boundaries)
    }

    fn visit(
        &self,
        module: u8,
        gray: &mut BTreeSet<u8>,
        black: &mut BTreeSet<u8>,
        boundaries: &mut BTreeSet<u8>,
    ) -> Result<(), Reload> {
        if black.contains(&module) {
            return Ok(());
        }
        if gray.contains(&module) {
            // A circular import on the current path is skipped, like Vite's
            // propagateUpdate; other importer paths may still find a boundary.
            return Ok(());
        }
        if self.accepting.contains(&module) {
            boundaries.insert(module);
            black.insert(module);
            return Ok(());
        }
        let importers = self.importers_of(module);
        if importers.is_empty() {
            return Err(Reload::Entry);
        }
        gray.insert(module);
        for importer in importers {
            self.visit(importer, gray, black, boundaries)?;
        }
        gray.remove(&module);
        black.insert(module);
        Ok(())
    }

    /// Everything that has to be re-fetched: the change plus its importers,
    /// stopping at accepting modules.
    fn dirty(&self, changed: u8) -> BTreeSet<u8> {
        let mut dirty = BTreeSet::from([changed]);
        let mut queue = vec![changed];
        while let Some(current) = queue.pop() {
            if self.accepting.contains(&current) && current != changed {
                continue;
            }
            for importer in self.importers_of(current) {
                if dirty.insert(importer) {
                    queue.push(importer);
                }
            }
        }
        dirty
    }
}

#[derive(Debug, PartialEq)]
enum Reload {
    Entry,
}

fn paths(slots: &BTreeSet<u8>) -> Vec<PathBuf> {
    slots.iter().copied().map(path).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn graph_matches_the_model(ops in proptest::collection::vec(op_strategy(), 1..40)) {
        let mut graph = ModuleGraph::new();
        let mut model = Model::default();

        for (step, op) in ops.iter().enumerate() {
            match op {
                Op::Link { importer, imports } => {
                    let as_paths: Vec<PathBuf> = imports.iter().copied().map(path).collect();
                    graph.set_imports(&path(*importer), &as_paths);
                    model.link(*importer, imports);
                }
                Op::AddImport { importer, imported } => {
                    graph.add_import(&path(*importer), &path(*imported));
                    model.add_import(*importer, *imported);
                }
                Op::Accept { module, accepting } => {
                    graph.set_self_accepting(&path(*module), *accepting);
                    model.known.insert(*module);
                    if *accepting {
                        model.accepting.insert(*module);
                    } else {
                        model.accepting.remove(module);
                    }
                }
                Op::Change { module } => {
                    let changed = path(*module);
                    let decision = graph.propagate_update(&changed);
                    let expected = if model.known.contains(module) {
                        model.climb(&[*module], &[])
                    } else {
                        Err(Reload::Entry)
                    };
                    match (&decision, &expected) {
                        (HmrDecision::Update { boundaries }, Ok(expected)) => {
                            prop_assert_eq!(boundaries, &paths(expected), "step {}: {:?}", step, op);
                        }
                        (HmrDecision::FullReload { .. }, Err(_)) => {}
                        (got, want) => prop_assert!(
                            false,
                            "step {step}: {op:?}: graph said {got:?}, model said {want:?}"
                        ),
                    }

                    // The dirty closure is only defined when an update is possible.
                    // Read-only on purpose: stamping here would corrupt the
                    // hmr timestamps mid-property-run with values production
                    // never produces.
                    if matches!(&decision, HmrDecision::Update { .. }) {
                        prop_assert_eq!(
                            graph.dirty_closure(&changed),
                            paths(&model.dirty(*module)),
                            "step {}: dirty closure", step
                        );
                    }
                }
            }

            // Structural invariants, after every operation.
            let known = paths(&model.known);
            prop_assert_eq!(graph.module_paths(), known.clone(), "step {}: known modules", step);
            for slot in 0..MODULES {
                let module = path(slot);
                prop_assert_eq!(
                    graph.contains(&module),
                    model.known.contains(&slot),
                    "step {}: contains {:?}", step, module
                );
            }

            // Every module the graph knows must be reachable as an importer
            // exactly where the model says it is: probe through the public
            // surface by asking for an update from each module's importers.
            for slot in 0..MODULES {
                if !model.known.contains(&slot) {
                    prop_assert!(
                        graph.update_targets_from_importers(&path(slot)).is_err(),
                        "step {}: unknown module {} must reload", step, slot
                    );
                    continue;
                }
                let importers = model.importers_of(slot);
                let result = graph.update_targets_from_importers(&path(slot));
                if importers.is_empty() {
                    prop_assert!(result.is_err(),
                        "step {}: {} has no importers but got {:?}", step, slot, result);
                    continue;
                }
                let seeds: Vec<u8> = importers.iter().copied().collect();
                match (&result, model.climb(&seeds, &[slot])) {
                    (Ok(targets), Ok(expected)) => {
                        let mut boundaries: Vec<PathBuf> =
                            targets.iter().map(|t| t.boundary.clone()).collect();
                        boundaries.sort();
                        boundaries.dedup();
                        prop_assert_eq!(boundaries, paths(&expected),
                            "step {}: importer-seeded boundaries for {}", step, slot);
                    }
                    (Err(_), Err(_)) => {}
                    (got, want) => prop_assert!(
                        false,
                        "step {step}: importer-seeded {slot}: graph said {got:?}, model said {want:?}"
                    ),
                }
            }
        }
    }

    /// Whatever the ops, a decision comes back and boundaries are always
    /// modules that actually accept.
    #[test]
    fn boundaries_are_always_accepting_modules(
        ops in proptest::collection::vec(op_strategy(), 1..30),
        changed in 0..MODULES,
    ) {
        let mut graph = ModuleGraph::new();
        let mut accepting = BTreeSet::new();
        for op in &ops {
            match op {
                Op::Link { importer, imports } => {
                    let as_paths: Vec<PathBuf> = imports.iter().copied().map(path).collect();
                    graph.set_imports(&path(*importer), &as_paths);
                }
                Op::AddImport { importer, imported } => {
                    graph.add_import(&path(*importer), &path(*imported));
                }
                Op::Accept { module, accepting: on } => {
                    graph.set_self_accepting(&path(*module), *on);
                    if *on { accepting.insert(*module); } else { accepting.remove(module); }
                }
                Op::Change { .. } => {}
            }
        }
        if let HmrDecision::Update { boundaries } = graph.propagate_update(&path(changed)) {
            for boundary in &boundaries {
                let slot = (0..MODULES).find(|s| path(*s) == *boundary).expect("known path");
                prop_assert!(accepting.contains(&slot), "{boundary:?} does not accept");
            }
            prop_assert!(boundaries.windows(2).all(|w| w[0] < w[1]), "sorted and deduped");
        }
    }

    /// Repeating a propagation is free of side effects: the same change always
    /// yields the same decision.
    #[test]
    fn propagation_is_idempotent(
        ops in proptest::collection::vec(op_strategy(), 1..30),
        changed in 0..MODULES,
    ) {
        let mut graph = ModuleGraph::new();
        for op in &ops {
            match op {
                Op::Link { importer, imports } => {
                    let as_paths: Vec<PathBuf> = imports.iter().copied().map(path).collect();
                    graph.set_imports(&path(*importer), &as_paths);
                }
                Op::AddImport { importer, imported } => {
                    graph.add_import(&path(*importer), &path(*imported));
                }
                Op::Accept { module, accepting } => {
                    graph.set_self_accepting(&path(*module), *accepting);
                }
                Op::Change { .. } => {}
            }
        }
        let first = graph.propagate_update(&path(changed));
        for _ in 0..3 {
            prop_assert_eq!(&graph.propagate_update(&path(changed)), &first);
        }
        let targets = graph.update_targets(&path(changed));
        prop_assert_eq!(graph.update_targets(&path(changed)).is_ok(), targets.is_ok());
    }
}
