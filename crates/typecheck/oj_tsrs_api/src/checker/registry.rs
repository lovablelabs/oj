// Port of the registries in Go `snapshotData` / `projectRegistryData` (tsc/internal/api/session.go).
//
// Symbol ids come from `tsrs_ast::get_symbol_id` (a process-wide counter, like Go `ast.GetSymbolId`), so
// snapshot-owned symbols are registered snapshot-wide. Type and signature ids are per-checker sequential
// ids, so they are registered per project, and additionally pinned to the id of the API checker that
// produced them: if the project's persistent API checker is replaced (e.g. after cancellation), every
// handle produced by the old checker becomes a stale-handle client error instead of silently resolving
// against a different checker.
//
// Raw pointers in here are only valid while the owning snapshot is retained. `release()` runs when the
// snapshot's `SnapshotData` is dropped (`CheckerSnapshotState::drop`, right after `SnapshotData::drop` derefs the
// project snapshot); no request can reach the registry by then, and after release every lookup fails.

use std::collections::hash_map::Entry;
use std::sync::Mutex;

use rustc_hash::FxHashMap;
use tsrs_ast::Symbol;
use tsrs_checker::{Signature, Type};
use tsrs_core::P;

use super::host::{CheckerError, CheckerResult};

#[derive(Default)]
struct ProjectRegistry {
    checker_id: u32,
    types: FxHashMap<u32, P<Type>>,
    signatures: FxHashMap<u64, P<Signature>>,
}

#[derive(Default)]
struct RegistryState {
    released: bool,
    symbols: FxHashMap<u64, P<Symbol>>,
    // Go `symbolCanonicalProjects`: first writer wins.
    symbol_projects: FxHashMap<u64, String>,
    projects: FxHashMap<String, ProjectRegistry>,
    // Snapshots derived during a request (auto-import preparation) whose symbols were handed out;
    // dereferenced on release so those pointers stay valid exactly as long as the registry.
    retained: Vec<std::sync::Arc<tsrs_project::Snapshot>>,
}

#[derive(Default)]
pub struct CheckerRegistry {
    state: Mutex<RegistryState>,
}

impl CheckerRegistry {
    /// Drops every registered pointer. Runs when the snapshot's `SnapshotData` is dropped.
    pub fn release(&self) {
        let mut st = self.lock();
        st.released = true;
        st.symbols = FxHashMap::default();
        st.symbol_projects = FxHashMap::default();
        st.projects = FxHashMap::default();
        let retained = std::mem::take(&mut st.retained);
        drop(st);
        for snapshot in retained {
            snapshot.deref();
        }
    }

    /// Takes over one reference of `snapshot` until `release` (see `retained`).
    pub(crate) fn retain_snapshot(
        &self,
        snapshot: std::sync::Arc<tsrs_project::Snapshot>,
    ) -> CheckerResult<()> {
        let mut st = self.lock();
        if st.released {
            drop(st);
            snapshot.deref();
            return Err(CheckerError::client("snapshot has been released"));
        }
        st.retained.push(snapshot);
        Ok(())
    }

    pub fn is_released(&self) -> bool {
        self.lock().released
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        // A panic while holding this short-lived lock cannot leave the maps half-updated in a way that
        // matters (every operation is a single insert/lookup), so recover from poisoning.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn check_live(st: &RegistryState) -> CheckerResult<()> {
        if st.released {
            return Err(CheckerError::client("snapshot has been released"));
        }
        Ok(())
    }

    /// Go `registerSymbol`. Returns the symbol id and its canonical project.
    pub(crate) fn register_symbol(
        &self,
        symbol: P<Symbol>,
        canonical_project: &str,
    ) -> CheckerResult<(u64, String)> {
        assert!(
            !canonical_project.is_empty(),
            "registerSymbol requires a non-empty canonical project"
        );
        let id = tsrs_ast::get_symbol_id(symbol).0;
        let mut st = self.lock();
        Self::check_live(&st)?;
        match st.symbols.entry(id) {
            Entry::Occupied(entry) => assert!(*entry.get() == symbol, "duplicate symbol"),
            Entry::Vacant(entry) => {
                entry.insert(symbol);
            }
        }
        let project =
            st.symbol_projects.entry(id).or_insert_with(|| canonical_project.to_string()).clone();
        Ok((id, project))
    }

    /// Go `resolveSymbolHandle`.
    pub(crate) fn resolve_symbol(&self, id: u64) -> CheckerResult<P<Symbol>> {
        if id == 0 {
            return Err(CheckerError::client("empty symbol handle"));
        }
        let st = self.lock();
        Self::check_live(&st)?;
        st.symbols.get(&id).copied().ok_or_else(|| {
            CheckerError::client(format!("symbol handle {id} not found in snapshot registry"))
        })
    }

    fn project_for_checker<'a>(
        st: &'a mut RegistryState,
        project: &str,
        checker_id: u32,
    ) -> &'a mut ProjectRegistry {
        let reg = st.projects.entry(project.to_string()).or_default();
        if reg.checker_id != checker_id {
            // The project's API checker changed: handles from the previous checker are stale.
            *reg = ProjectRegistry { checker_id, ..Default::default() };
        }
        reg
    }

    /// Go `registerType`. `checker_id` is the id of the API checker that produced `t`.
    pub(crate) fn register_type(
        &self,
        project: &str,
        checker_id: u32,
        t: P<Type>,
    ) -> CheckerResult<u32> {
        assert!(!project.is_empty(), "registerType: empty project ID");
        let id = t.id().0;
        let mut st = self.lock();
        Self::check_live(&st)?;
        let reg = Self::project_for_checker(&mut st, project, checker_id);
        match reg.types.entry(id) {
            Entry::Occupied(entry) => assert!(*entry.get() == t, "duplicate type"),
            Entry::Vacant(entry) => {
                entry.insert(t);
            }
        }
        Ok(id)
    }

    /// Go `resolveTypeHandle`. With `checker_id = Some(id)` the handle must come from that checker.
    pub fn resolve_type(
        &self,
        project: &str,
        id: u32,
        checker_id: Option<u32>,
    ) -> CheckerResult<P<Type>> {
        if id == 0 {
            return Err(CheckerError::client("empty type handle"));
        }
        if project.is_empty() {
            return Err(CheckerError::client(format!("empty project ID for type handle {id}")));
        }
        let st = self.lock();
        Self::check_live(&st)?;
        let Some(reg) = st.projects.get(project) else {
            return Err(CheckerError::client(format!(
                "type handle {id} not found (no registry for project {project})"
            )));
        };
        if checker_id.is_some_and(|c| c != reg.checker_id) {
            return Err(CheckerError::client(format!(
                "type handle {id} is stale (the project's API checker was replaced)"
            )));
        }
        reg.types.get(&id).copied().ok_or_else(|| {
            CheckerError::client(format!("type handle {id} not found in project registry"))
        })
    }

    /// Go `registerSignature`.
    pub(crate) fn register_signature(
        &self,
        project: &str,
        checker_id: u32,
        sig: P<Signature>,
    ) -> CheckerResult<u64> {
        assert!(!project.is_empty(), "registerSignature: empty project ID");
        let id = sig.id().0 as u64;
        let mut st = self.lock();
        Self::check_live(&st)?;
        let reg = Self::project_for_checker(&mut st, project, checker_id);
        match reg.signatures.entry(id) {
            Entry::Occupied(entry) => assert!(*entry.get() == sig, "duplicate signature"),
            Entry::Vacant(entry) => {
                entry.insert(sig);
            }
        }
        Ok(id)
    }

    /// Go `resolveSignatureHandle`.
    pub(crate) fn resolve_signature(
        &self,
        project: &str,
        id: u64,
        checker_id: Option<u32>,
    ) -> CheckerResult<P<Signature>> {
        if id == 0 {
            return Err(CheckerError::client("empty signature handle"));
        }
        if project.is_empty() {
            return Err(CheckerError::client(format!(
                "empty project ID for signature handle {id}"
            )));
        }
        let st = self.lock();
        Self::check_live(&st)?;
        let Some(reg) = st.projects.get(project) else {
            return Err(CheckerError::client(format!(
                "signature handle {id} not found (no registry for project {project})"
            )));
        };
        if checker_id.is_some_and(|c| c != reg.checker_id) {
            return Err(CheckerError::client(format!(
                "signature handle {id} is stale (the project's API checker was replaced)"
            )));
        }
        reg.signatures.get(&id).copied().ok_or_else(|| {
            CheckerError::client(format!("signature handle {id} not found in project registry"))
        })
    }

    /// Checker that produced the project's registered types/signatures (0 if none yet).
    pub fn project_checker_id(&self, project: &str) -> u32 {
        self.lock().projects.get(project).map_or(0, |r| r.checker_id)
    }
}
