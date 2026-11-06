// Port of microsoft/TypeScript#64475 (lazy member tables of instantiated class/interface references) and #64526
// (lazy members of `{ [P in keyof T]: X }` mapped types). Enabled by `--fast`; `--noLazyMembers` or `TSRS_LAZY_MEMBERS=0`
// turns it off, which restores the reference (tsgo) behavior exactly. See notes/lazy-members.md.
//
// Follow-up candidates (notes/mem-lazy.md) each have their own switch below. A candidate only takes effect when the
// master switch is on too, so `TSRS_LAZY_MEMBERS=0` stays reference-identical whatever the candidate switches say.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

// 0: not given on the command line, 1: `--lazyMembers`, 2: `--noLazyMembers`.
static CLI: AtomicU8 = AtomicU8::new(0);

pub fn set_from_cli(on: bool) {
    CLI.store(if on { 1 } else { 2 }, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    static ENV: OnceLock<Option<bool>> = OnceLock::new();
    match CLI.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => ENV
            .get_or_init(|| std::env::var("TSRS_LAZY_MEMBERS").ok().map(|v| v != "0"))
            .unwrap_or_else(crate::fast::enabled),
    }
}

fn env_flag(cell: &'static OnceLock<bool>, name: &str, default: bool) -> bool {
    enabled() && *cell.get_or_init(|| std::env::var(name).map_or(default, |v| v != "0"))
}

/// Candidate L1 (notes/mem-lazy.md): tuple references get lazy member tables like class/interface references.
/// `TSRS_LAZY_TUPLES=0|1`.
pub fn lazy_tuples() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_TUPLES", true)
}

/// Defers creating a composite mapper for conditional-type instantiation.
///
/// Candidate L5 (notes/mem-lazy.md): instantiating a conditional type applies `t.mapper` and `m` to the outer
/// type parameters the way the composite mapper would, instead of first allocating
/// `combineTypeMappers(t.mapper, m)`, which is only needed for that.
///
/// `TSRS_LAZY_COND_MAPPER=0|1`.
pub fn lazy_cond_mapper() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_COND_MAPPER", true)
}

/// Allocates property caches only when a value is stored.
///
/// Candidate L6 (notes/mem-lazy.md): union/intersection property caches are allocated on first store, and a
/// non-partial property found without the function-property augment is not copied into the augmented cache;
/// augmented lookups read it from the other cache instead.
///
/// `TSRS_LAZY_PROP_CACHE=0|1`.
pub fn lazy_prop_cache() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_PROP_CACHE", true)
}

/// Candidate L9 (notes/mem-lazy.md): `getUnmatchedProperties` without discriminant matching only needs to
/// know whether the source has each property, so it asks without instantiating lazy members.
///
/// `TSRS_LAZY_HAS_PROP=0|1`.
pub fn lazy_has_prop() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_HAS_PROP", false)
}

/// Checks unmatched properties while preserving lazy member order.
///
/// Candidate L10 (notes/mem-lazy.md): `getUnmatchedProperties` on a target with a lazy member table walks its
/// properties in `getPropertiesOfType` order with declared members standing in for uninstantiated ones, asks
/// the source only whether it has each property, and looks up the real target property only to return it or
/// to compare discriminant types.
///
/// `TSRS_LAZY_UNMATCHED=0|1`.
pub fn lazy_unmatched() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_UNMATCHED", true)
}

/// Candidate L11 (notes/mem-lazy.md): "is this an empty object type" (`isEmptyObjectType`, the empty-object
/// test in `removeSubtypes`) answers from a lazy member table instead of resolving it.
///
/// `TSRS_LAZY_EMPTY=0|1`.
pub fn lazy_empty() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_EMPTY", true)
}

/// Candidate B2 (notes/mem-round3.md): an inference context's fixing and non-fixing mappers are created on first
/// use instead of with the context. `TSRS_LAZY_INFERENCE_MAPPERS=0|1`.
pub fn lazy_inference_mappers() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    env_flag(&F, "TSRS_LAZY_INFERENCE_MAPPERS", true)
}

macro_rules! lazy_member_stats {
    ($($field:ident: $label:literal,)*) => {
        /// Per-checker counts of the lazy paths (only incremented when the flag is on).
        #[derive(Clone, Copy, Default, Debug)]
        pub struct LazyMemberStats {
            $(pub $field: u64,)*
        }

        impl LazyMemberStats {
            pub fn add(&mut self, o: &LazyMemberStats) {
                $(self.$field += o.$field;)*
            }

            pub fn rows(&self) -> Vec<(&'static str, u64)> {
                vec![$(($label, self.$field),)*]
            }
        }
    };
}

lazy_member_stats! {
    member_tables_created: "Lazy member tables",
    member_tables_resolved_in_full: "Lazy member tables resolved in full",
    member_table_declared_members: "Lazy table declared members",
    member_table_declared_instantiated: "Lazy table members instantiated",
    member_lookups: "Lazy member lookups",
    member_signature_queries: "Lazy signature queries",
    member_index_info_queries: "Lazy index info queries",
    member_every_property_queries: "Lazy every-property queries",
    mapped_tables_created: "Lazy mapped tables",
    mapped_tables_resolved_in_full: "Lazy mapped tables resolved in full",
    mapped_members_created: "Lazy mapped members created",
    mapped_member_lookups: "Lazy mapped member lookups",
    mapped_index_info_queries: "Lazy mapped index info queries",
    mapped_signature_early_returns: "Mapped signature early returns",
    some_property_skipped_constituents: "somePropertyReducesToNever skips",
    tuple_tables_created: "Lazy tuple tables",
    tuple_tables_resolved_in_full: "Lazy tuple tables resolved in full",
    cond_mappers_avoided: "Conditional composite mappers avoided",
    prop_cache_copies_avoided: "Augmented property cache copies avoided",
    prop_cache_shared_hits: "Augmented lookups served by the other cache",
    has_prop_queries: "Existence-only property queries",
    has_prop_uninstantiated: "Existence answers from an uninstantiated member",
    unmatched_lazy_walks: "Lazy property-order lists requested",
    empty_lazy_queries: "Empty-object queries answered by lazy tables",
}
