//! Port of `internal/modulespecifiers`: computes the module specifiers (`"./foo"`, `"pkg/sub"`) the checker's node
//! builder prints in `import("…")` type names. Non-function declarations are hand-ported in types.rs (including the
//! host trait); the functions of compare.go, preferences.go, specifiers.go and util.go are ported by hand.
//! docs/sigs/modulespecifiers.txt indexes the Rust signatures.
//! `ProcessEntrypointEnding` (util.go, language-service auto-imports only) is hand-ported in util.rs
//! (`process_entrypoint_ending`).

mod compare;
mod preferences;
pub mod specifiers;
mod types;
mod util;

pub use compare::*;
pub use preferences::*;
pub use specifiers::*;
pub use types::*;
pub use util::*;

pub(crate) use tsrs_core::collections::OrderedMap;
pub(crate) use tsrs_module::ResolvedModule;
pub(crate) use tsrs_module::symlinks::KnownSymlinks;
pub(crate) use tsrs_tsoptions::SourceOutputAndProjectReference;
pub(crate) use tsrs_tsoptions::outputpaths::OutputPathsHost;
