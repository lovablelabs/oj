//! Port of `internal/checker` (and `internal/evaluator`). See docs/CHECKER.md.
//!
//! Every module does `use crate::*;`, which brings the data model and the common imports below into scope.

// Common imports for all checker modules.
pub(crate) use std::cell::{Cell, RefCell};

pub(crate) use rustc_hash::{FxHashMap, FxHashSet};
pub(crate) use tsrs_ast as ast;
pub(crate) use tsrs_ast::{
    CheckFlags, Diagnostic, FlowNode, Kind, ModifierFlags, Node, NodeFlags, NodeId, SourceFile,
    Symbol, SymbolFlags, SymbolId, SymbolTable,
};
pub(crate) use tsrs_binder::{NameResolver, ReferenceResolver};
pub(crate) use tsrs_core::collections::{OrderedMap, OrderedSet, Set};
pub(crate) use tsrs_core::jsnum::{self, Number, PseudoBigInt};
pub(crate) use tsrs_core::{
    CompilerOptions, ModuleKind, ModuleResolutionKind, P, ResolutionMode, ScriptTarget, Tristate,
    alloc, alloc_slice, alloc_str, alloc_vec,
};
pub(crate) use tsrs_diagnostics::{self as diagnostics, Message};
pub(crate) use tsrs_module::ResolvedModule;
pub(crate) use tsrs_modulespecifiers as modulespecifiers;
pub(crate) use tsrs_modulespecifiers::ModuleSpecifierGenerationHost;
pub(crate) use tsrs_pseudochecker as pseudochecker;
pub(crate) use tsrs_pseudochecker::{
    PseudoChecker, PseudoParameter, PseudoType, new_pseudo_checker,
};
pub(crate) use tsrs_scanner::Scanner;

// The regular `bitflags!` form generates a public wrapper and a private flags type, duplicating
// methods, operators and iterators. The checker has enough flags types for that duplication to be
// measurable, and doesn't use the external-trait derives that need the private type. Use bitflags'
// custom-type form while keeping the declarations and their symbolic Debug output unchanged.
pub(crate) struct DirectBitflagsDebug<'a, F>(&'a F);

impl<F: bitflags::Flags> std::fmt::Debug for DirectBitflagsDebug<'_, F>
where
    F::Bits: bitflags::parser::WriteHex,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            std::fmt::Write::write_str(f, "0x0")
        } else {
            bitflags::parser::to_writer(self.0, f)
        }
    }
}

macro_rules! direct_bitflags {
    (
        $(#[doc = $doc:literal])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
        $(#[expect($($expect:tt)*)])*
        $vis:vis struct $flags:ident: $bits:ty {
            $($body:tt)*
        }
    ) => {
        $(#[doc = $doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        $(#[expect($($expect)*)])*
        #[repr(transparent)]
        $vis struct $flags($bits);

        ::bitflags::bitflags! {
            impl $flags: $bits {
                $($body)*
            }
        }

        impl std::fmt::Debug for $flags {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($flags))
                    .field(&$crate::DirectBitflagsDebug(self))
                    .finish()
            }
        }
    };
}

pub(crate) use direct_bitflags;

// Data model (checker-foundation).
pub mod checker;
pub mod evaluator;
pub mod flow_types;
pub mod inference_types;
pub mod jsx_types;
pub mod links;
pub mod mapper;
pub mod merged_symbols;
pub mod nodebuilder_types;
pub mod printer_types;
pub mod program;
pub mod relater_types;
pub mod sitecount;
pub mod types;
pub mod utilities_types;
pub mod workcensus;

pub use checker::*;
pub use flow_types::*;
pub use inference_types::*;
pub use jsx_types::*;
pub use links::*;
pub use mapper::*;
pub use nodebuilder_types::*;
pub use printer_types::*;
pub use program::*;
pub use relater_types::*;
pub use types::*;
pub use utilities_types::*;

// Hand-ported function bodies.
pub mod checker_01;
pub(crate) use checker_01::*;
mod checker_02;
pub(crate) mod checker_03;
pub(crate) use checker_03::*;
mod checker_04;
mod checker_05;
mod checker_06;
pub mod checker_07;
pub(crate) use checker_07::*;
pub mod checker_08;
pub(crate) use checker_08::*;
pub mod checker_09;
pub(crate) use checker_09::*;
pub mod checker_10;
pub(crate) use checker_10::*;
pub mod checker_11;
pub(crate) use checker_11::*;
pub(crate) mod checker_12;
pub(crate) use checker_12::*;
pub mod checker_13;
pub(crate) use checker_13::*;
pub mod checker_14;
pub(crate) use checker_14::*;
pub mod checker_15;
pub(crate) use checker_15::*;
pub mod relater_1;
pub(crate) use relater_1::*;
pub(crate) mod flow;
mod infermemo;
mod relater_2;
mod unioncache;
mod uniontrace;
pub(crate) use flow::*;
pub mod basedmap;
pub mod flowmemo;
pub mod packedmap;
pub use packedmap::{GoPackedMap, PackedMap, StringLiteralTypes};
pub mod inference;
pub(crate) use inference::*;
pub(crate) mod grammarchecks;
pub(crate) use grammarchecks::*;
pub mod utilities;
pub use utilities::new_diagnostic_for_node;
pub(crate) use utilities::*;
mod exports;
mod jsdoc;
mod jsx;
pub mod printer;
pub(crate) use printer::*;
mod nodebuilder;
pub mod nodebuilderimpl_1;
pub(crate) use nodebuilderimpl_1::*;
pub(crate) mod nodebuilderimpl_2;
pub(crate) use nodebuilderimpl_2::*;
pub mod emitresolver;
mod nodebuilderscopes;
mod nodecopy;
mod pseudotypenodebuilder;
pub(crate) use emitresolver::*;
pub(crate) mod nodebuilder_hover;
pub(crate) use nodebuilder_hover::*;
mod services;
pub use tsrs_astnav as astnav;

// Exported Go package-level functions (the baseline writer, the language service and the project system call them).
pub(crate) use checker_14::get_mapped_type_modifiers;
pub use exports::{
    get_declaration_modifier_flags_from_symbol_exported, get_property_name_from_type_exported,
    is_distributed_type_parameter, is_tuple_type_exported, is_tuple_type_target,
    is_type_usable_as_property_name_exported,
};
pub use nodebuilder::{new_node_builder, new_node_builder_ex};
pub use nodebuilderimpl_1::try_get_module_specifier_from_declaration;
pub(crate) use printer::new_symbol_tracker_impl;
pub use services::get_resolved_signature_for_signature_help;
pub(crate) use utilities::{
    compare_types, is_private_identifier_symbol, new_diagnostic_chain_for_node,
};
pub use utilities::{
    create_mode_mismatch_details, create_module_not_found_chain, get_set_accessor_value_parameter,
    is_external_module_symbol, is_in_type_query, is_known_symbol, is_type_any, skip_alias,
    value_to_string,
};
