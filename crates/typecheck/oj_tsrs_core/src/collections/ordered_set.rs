use rustc_hash::FxBuildHasher;

pub type OrderedSet<T> = indexmap::IndexSet<T, FxBuildHasher>;
