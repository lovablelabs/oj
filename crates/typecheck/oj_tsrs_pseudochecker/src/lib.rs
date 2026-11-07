//! Port of `internal/pseudochecker`: a limited "checker" that returns pseudo-"types" of expressions (mostly those
//! which trivially have type nodes). It has no dependency on the checker; the checker's node builder consumes it
//! (pseudotypenodebuilder.rs). `checker.go`, `type.go` (data model) and `lookup.go` are hand-ported;
//! docs/sigs/pseudochecker.txt indexes the Rust signatures.

mod checker;
mod lookup;
#[expect(
    clippy::redundant_pub_crate,
    reason = "The root re-exports the public types; helper functions and singleton values stay crate-private."
)]
mod type_;

pub use checker::*;
pub use lookup::*;
pub use type_::*;
