// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Graph shapes a real app produces and a naive HMR walk cannot survive:
//! very deep import chains, wide fan-in, cycles of every length, and
//! repeated re-linking of the same module.
//!
//! Every case here must terminate with a decision. `FullReload` is always a
//! correct answer; a panic, an overflow or a hang is never one.

use std::path::{Path, PathBuf};

use oj_graph::{HmrDecision, ModuleGraph};

fn p(i: usize) -> PathBuf {
    PathBuf::from(format!("/src/m{i}.tsx"))
}

/// Runs `body` on a thread with the stack a tokio worker gets (2 MiB), so a
/// recursive walk shows up here instead of in someone's dev server.
fn on_worker_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(body)
        .expect("spawn")
        .join()
        .expect("no panic or overflow on a worker-sized stack")
}

/// chain: m0 <- m1 <- ... <- mN, with the top module accepting.
fn chain(depth: usize) -> ModuleGraph {
    let mut g = ModuleGraph::new();
    for i in 0..depth {
        g.add_import(&p(i + 1), &p(i));
    }
    g.set_self_accepting(&p(depth), true);
    g
}

#[test]
fn a_deep_import_chain_does_not_exhaust_the_stack() {
    let decision = on_worker_stack(|| chain(200_000).propagate_update(&p(0)));
    match decision {
        HmrDecision::Update { boundaries } => assert_eq!(boundaries, vec![p(200_000)]),
        HmrDecision::FullReload { reason } => panic!("unexpected full reload: {reason}"),
    }
}

#[test]
fn a_deep_chain_of_accepting_modules_stops_at_the_first_boundary() {
    let targets = on_worker_stack(|| {
        let mut g = chain(100_000);
        // Every module accepts: the walk must stop at the immediate importer.
        for i in 0..=100_000 {
            g.set_self_accepting(&p(i), true);
        }
        g.update_targets_from_importers(&p(0))
    });
    match targets {
        Ok(targets) => assert_eq!(
            targets
                .iter()
                .map(|t| t.boundary.clone())
                .collect::<Vec<_>>(),
            vec![p(1)]
        ),
        Err(reason) => panic!("unexpected full reload: {reason}"),
    }
}

#[test]
fn a_deep_cycle_is_reported_as_a_cycle_not_a_hang() {
    let decision = on_worker_stack(|| {
        let mut g = chain(50_000);
        // Close the loop: the top module imports the bottom one.
        g.add_import(&p(0), &p(50_000));
        g.set_self_accepting(&p(50_000), false);
        g.propagate_update(&p(0))
    });
    // A closed cycle with no entry has nothing loaded to update and nothing to
    // reload; the point is that the walk terminates.
    assert_eq!(decision, HmrDecision::Update { boundaries: vec![] });
}

#[test]
fn a_deep_chain_with_no_boundary_falls_back_to_a_full_reload() {
    let decision = on_worker_stack(|| {
        let mut g = chain(100_000);
        g.set_self_accepting(&p(100_000), false);
        g.propagate_update(&p(0))
    });
    match decision {
        HmrDecision::FullReload { reason } => assert!(reason.contains("entry"), "{reason}"),
        HmrDecision::Update { boundaries } => panic!("no boundary exists: {boundaries:?}"),
    }
}

#[test]
fn the_dirty_closure_of_a_deep_chain_is_the_whole_chain() {
    let dirty = on_worker_stack(|| chain(50_000).stamp_update(&p(0), 1));
    assert_eq!(dirty.len(), 50_001);
}

#[test]
fn wide_fan_in_visits_every_importer_once() {
    let mut g = ModuleGraph::new();
    let shared = PathBuf::from("/src/shared.ts");
    for i in 0..20_000 {
        g.add_import(&p(i), &shared);
        g.set_self_accepting(&p(i), true);
    }
    let HmrDecision::Update { boundaries } = g.propagate_update(&shared) else {
        panic!("expected an update");
    };
    assert_eq!(boundaries.len(), 20_000);
    let mut sorted = boundaries.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), boundaries.len(), "boundaries must be unique");
}

#[test]
fn a_diamond_reaches_each_boundary_exactly_once() {
    let mut g = ModuleGraph::new();
    let (leaf, left, right, top) = (p(0), p(1), p(2), p(3));
    g.add_import(&left, &leaf);
    g.add_import(&right, &leaf);
    g.add_import(&top, &left);
    g.add_import(&top, &right);
    g.set_self_accepting(&top, true);

    let HmrDecision::Update { boundaries } = g.propagate_update(&leaf) else {
        panic!("expected an update");
    };
    assert_eq!(boundaries, vec![top]);
}

#[test]
fn self_import_is_a_cycle() {
    let mut g = ModuleGraph::new();
    let a = p(0);
    g.add_import(&a, &a);
    assert_eq!(
        g.propagate_update(&a),
        HmrDecision::Update { boundaries: vec![] }
    );
}

#[test]
fn a_self_importing_module_that_accepts_is_its_own_boundary() {
    let mut g = ModuleGraph::new();
    let a = p(0);
    g.add_import(&a, &a);
    g.set_self_accepting(&a, true);
    assert_eq!(
        g.propagate_update(&a),
        HmrDecision::Update {
            boundaries: vec![a]
        }
    );
}

#[test]
fn cycles_of_every_small_length_are_detected() {
    for len in 2..12 {
        let mut g = ModuleGraph::new();
        for i in 0..len {
            g.add_import(&p((i + 1) % len), &p(i));
        }
        let decision = g.propagate_update(&p(0));
        assert_eq!(
            decision,
            HmrDecision::Update { boundaries: vec![] },
            "cycle of {len} must terminate with nothing to do: {decision:?}"
        );
    }
}

#[test]
fn a_cycle_below_an_accepting_boundary_still_updates() {
    // leaf <- a <-> b, and a is also imported by an accepting boundary.
    let mut g = ModuleGraph::new();
    let (leaf, a, b, boundary) = (p(0), p(1), p(2), p(3));
    g.add_import(&a, &leaf);
    g.add_import(&a, &b);
    g.add_import(&b, &a);
    g.add_import(&boundary, &a);
    g.set_self_accepting(&boundary, true);
    // The cycle is skipped and the accepting importer above it is the boundary.
    assert_eq!(
        g.propagate_update(&leaf),
        HmrDecision::Update {
            boundaries: vec![boundary]
        }
    );
}

#[test]
fn unknown_modules_never_produce_a_partial_update() {
    let g = chain(3);
    let decision = g.propagate_update(Path::new("/src/never-seen.tsx"));
    let HmrDecision::FullReload { reason } = decision else {
        panic!("an unknown module must force a reload");
    };
    assert!(reason.contains("not in the module graph"), "{reason}");
    assert!(g.update_targets(Path::new("/src/never-seen.tsx")).is_err());
}

#[test]
fn relinking_a_module_leaves_no_stale_edges() {
    let mut g = ModuleGraph::new();
    let importer = p(0);
    g.set_self_accepting(&importer, true);
    // Re-link the same importer many times over a rotating dependency set.
    let deps =
        |round: usize| -> Vec<PathBuf> { (0..10).map(|i| p(1 + (round + i) % 50)).collect() };
    for round in 0..500 {
        g.set_imports(&importer, &deps(round));
    }

    let live = deps(499);
    for dep in &live {
        assert_eq!(
            g.propagate_update(dep),
            HmrDecision::Update {
                boundaries: vec![importer.clone()]
            },
            "{dep:?} is still imported"
        );
    }
    for i in 1..51 {
        let dep = p(i);
        if live.contains(&dep) {
            continue;
        }
        // A dropped dependency keeps its node but must have lost the edge, so
        // it now looks like an entry with no boundary above it.
        let HmrDecision::FullReload { reason } = g.propagate_update(&dep) else {
            panic!("{dep:?} still reaches the importer: stale edge");
        };
        assert!(reason.contains("no accepting boundary"), "{reason}");
    }
}

#[test]
fn paths_that_are_not_utf8_shaped_are_still_keys() {
    let mut g = ModuleGraph::new();
    let weird = PathBuf::from("/src/../src/./wei rd%20\u{202e}name.tsx");
    let importer = PathBuf::from("/src/importer.tsx");
    g.add_import(&importer, &weird);
    g.set_self_accepting(&importer, true);
    assert!(g.contains(&weird));
    assert_eq!(
        g.propagate_update(&weird),
        HmrDecision::Update {
            boundaries: vec![importer]
        }
    );
}
