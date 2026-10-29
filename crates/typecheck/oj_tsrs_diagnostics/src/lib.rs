//! Diagnostic messages (Go `internal/diagnostics`).
//!
//! Every Go message variable is a `pub static` with a `SCREAMING_SNAKE_CASE` identifier, re-exported at the crate
//! root: Go `diagnostics.Type_0_is_not_assignable_to_type_1` is
//! `tsrs_diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1` (pass it as `&'static Message`).
//! `generated.rs` is produced by `tools/gen-diagnostics/gen.mjs`; do not edit it by hand.

mod diagnostics;
pub mod generated;

pub use diagnostics::*;
pub use generated::*;
