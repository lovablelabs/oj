//! Opt-in creation-site counters (`-p tsrs_checker --features tsrs_checker/site-counts`, compiled to no-ops otherwise).
//!
//! `hit(kind, label)` counts one event per (kind, label, caller location); the checker's constructors
//! (`new_symbol`, `new_type`, type mappers, link slots, signatures) are `#[track_caller]` under the feature, so
//! the location is the code that asked for the object. `dump()` prints the per-kind totals and the top rows to
//! stderr; `TSRS_SITE_COUNTS_TOP` sets the number of rows per kind (default 40).

#[cfg(feature = "site-counts")]
mod imp {
    use rustc_hash::FxHashMap;
    use std::panic::Location;
    use std::sync::Mutex;

    type Key = (&'static str, &'static str, &'static Location<'static>);
    static COUNTS: Mutex<Option<FxHashMap<Key, u64>>> = Mutex::new(None);

    #[track_caller]
    pub(crate) fn hit(kind: &'static str, label: &'static str) {
        let loc = Location::caller();
        let mut guard = COUNTS.lock().unwrap();
        *guard.get_or_insert_with(FxHashMap::default).entry((kind, label, loc)).or_insert(0) += 1;
    }

    pub(crate) fn hit_at(kind: &'static str, label: &'static str, loc: &'static Location<'static>) {
        let mut guard = COUNTS.lock().unwrap();
        *guard.get_or_insert_with(FxHashMap::default).entry((kind, label, loc)).or_insert(0) += 1;
    }

    pub fn dump() {
        let top: usize =
            std::env::var("TSRS_SITE_COUNTS_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        let guard = COUNTS.lock().unwrap();
        let Some(counts) = guard.as_ref() else { return };
        let mut kinds: FxHashMap<&str, Vec<(String, u64)>> = FxHashMap::default();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "Aggregates independent integer counters; the report sorts the collected rows."
        )]
        for (&(kind, label, loc), &n) in counts {
            let file = loc.file().rsplit('/').next().unwrap_or(loc.file());
            kinds
                .entry(kind)
                .or_default()
                .push((format!("{:<24} {}:{}", label, file, loc.line()), n));
        }
        let mut names: Vec<_> = kinds.keys().copied().collect();
        names.sort_unstable();
        for kind in names {
            let rows = kinds.get_mut(kind).unwrap();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let total: u64 = rows.iter().map(|r| r.1).sum();
            eprintln!("== site counts: {kind} (total {total}, {} sites)", rows.len());
            // Per-label totals.
            let mut labels: FxHashMap<&str, u64> = FxHashMap::default();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "Aggregates independent integer counters; the report sorts the collected rows."
            )]
            for (&(k, label, _), &n) in counts {
                if k == kind {
                    *labels.entry(label).or_insert(0) += n;
                }
            }
            let mut labels: Vec<_> = labels.into_iter().collect();
            labels.sort_by_key(|a| std::cmp::Reverse(a.1));
            if labels.len() > 1 {
                for (label, n) in labels.iter().take(top) {
                    eprintln!(
                        "   label {:>12} {:5.1}%  {}",
                        n,
                        100.0 * *n as f64 / total as f64,
                        label
                    );
                }
            }
            for (row, n) in rows.iter().take(top) {
                eprintln!("  {:>12} {:5.1}%  {}", n, 100.0 * *n as f64 / total as f64, row);
            }
        }
    }
}

#[cfg(feature = "site-counts")]
pub use imp::dump;
#[cfg(feature = "site-counts")]
pub(crate) use imp::{hit, hit_at};

#[cfg(not(feature = "site-counts"))]
#[inline(always)]
pub(crate) fn hit(_kind: &'static str, _label: &'static str) {}

#[cfg(not(feature = "site-counts"))]
pub fn dump() {}
