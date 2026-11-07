//! Port of `internal/transformers/declarations` (TypeScript 7). The transformer base it builds on
//! (`internal/transformers`) lives in `tsrs_transformers` and is re-exported.
//!
//! Used both for declaration emit (`tsrs_compiler` `run_declaration_transformers` in emitter.rs) and for declaration
//! diagnostics (`tsc` reports them whenever `declaration`/`composite` is on). The transform is ported faithfully:
//! which nodes are visited, in what order and with which node-builder flags decides which diagnostics fire.
//! Diagnostics entry point: `get_declaration_diagnostics` (Go `compiler.getDeclarationDiagnostics`) is in
//! `tsrs_compiler`; it builds a `DeclarationTransformer` with `new_declaration_transformer` and runs
//! `transform_source_file` while lending the file's checker to the transformer (see `Resolver`).
//!
//! Layout (one Rust file per Go file; transform.go is split by line range):
//! - types.rs: the hand-ported data model of all Go files (structs, host trait, constants).
//! - `Resolver` and `Transformer` come from `tsrs_transformers` (re-exported).
//! - transform_{1,2,3}.rs (transform.go 1–1339, 1340–2327, 2328–end), diagnostics_.rs, tracker.rs, util.rs,
//!   supplementalreferences.rs: hand-ported function bodies.

pub(crate) use std::cell::{Cell, OnceCell, RefCell};
pub(crate) use std::fmt::Display;
pub(crate) use std::rc::Rc;

pub(crate) use rustc_hash::FxHashMap;
pub(crate) use tsrs_ast::{self as ast, *};
pub(crate) use tsrs_checker::{
    self as checker, Checker, SymbolAccessibility, SymbolAccessibilityResult, SymbolTracker,
};
pub(crate) use tsrs_core::collections::Set;
pub(crate) use tsrs_core::tspath;
pub(crate) use tsrs_core::{CompilerOptions, P, alloc_str, alloc_vec};
pub(crate) use tsrs_diagnostics::{self as diagnostics, Message};
pub(crate) use tsrs_modulespecifiers::ModuleSpecifierGenerationHost;
pub(crate) use tsrs_printer::{self as printer, EmitContext};
pub(crate) use tsrs_scanner as scanner;
pub(crate) use tsrs_transformers as transformers;
pub(crate) use tsrs_tsoptions::outputpaths::OutputPaths;

/// Go package `nodebuilder` (flags only).
pub(crate) mod nodebuilder {
    pub use tsrs_checker::{Flags, InternalFlags};
}

mod types;

// Go: `declarations` imports `transformers`; the base types moved to tsrs_transformers and are re-exported here so
// existing paths keep compiling.
pub use tsrs_transformers::{
    Resolver, Transformer, is_original_node_single_line, is_simple_copiable_expression,
    is_simple_inlineable_expression,
};
pub use types::*;

mod diagnostics_;
mod supplementalreferences;
mod tracker;
mod transform_1;
mod transform_2;
mod transform_3;
mod util;

pub(crate) use diagnostics_::*;
pub use supplementalreferences::*;
pub(crate) use tracker::*;
pub use transform_1::*;
pub(crate) use util::*;
