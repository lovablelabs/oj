//! Native type-aware linting on the tsrs AST and checker.

pub mod cli;
mod go_quote;
pub mod payload;
pub mod protocol;
pub mod registry;

pub mod engine;

pub mod rule;

pub mod utils;

mod type_matches_specifier;

mod rules {
    pub mod await_thenable;
    pub mod consistent_return;
    pub mod consistent_type_exports;
    pub mod dot_notation;
    pub mod no_array_delete;
    pub mod no_base_to_string;
    pub mod no_confusing_void_expression;
    pub mod no_deprecated;
    pub mod no_duplicate_type_constituents;
    pub mod no_floating_promises;
    pub mod no_for_in_array;
    pub mod no_generated_empty_object_type;
    pub mod no_implied_eval;
    pub mod no_meaningless_void_operator;
    pub mod no_misused_promises;
    pub mod no_misused_spread;
    pub mod no_mixed_enums;
    pub mod no_redundant_type_constituents;
    pub mod no_unnecessary_boolean_literal_compare;
    pub mod no_unnecessary_condition;
    pub mod no_unnecessary_qualifier;
    pub mod no_unnecessary_template_expression;
    pub mod no_unnecessary_type_arguments;
    pub mod no_unnecessary_type_assertion;
    pub mod no_unnecessary_type_conversion;
    pub mod no_unnecessary_type_parameters;
    pub mod no_unsafe_argument;
    pub mod no_unsafe_assignment;
    pub mod no_unsafe_call;
    pub mod no_unsafe_enum_assignment;
    pub mod no_unsafe_enum_comparison;
    pub mod no_unsafe_member_access;
    pub mod no_unsafe_return;
    pub mod no_unsafe_type_assertion;
    pub mod no_unsafe_unary_minus;
    pub mod no_useless_default_assignment;
    pub mod non_nullable_type_assertion_style;
    pub mod only_throw_error;
    pub mod prefer_find;
    pub mod prefer_includes;
    pub mod prefer_nullish_coalescing;
    pub mod prefer_optional_chain;
    pub mod prefer_promise_reject_errors;
    pub mod prefer_readonly;
    pub mod prefer_readonly_parameter_types;
    pub mod prefer_reduce_type_parameter;
    pub mod prefer_regexp_exec;
    pub mod prefer_return_this_type;
    pub mod prefer_string_starts_ends_with;
    pub mod promise_function_async;
    mod regexp_unicode;
    pub mod related_getter_setter_pairs;
    pub mod require_array_sort_compare;
    pub mod require_await;
    pub mod restrict_plus_operands;
    pub mod restrict_template_expressions;
    pub mod return_await;
    pub mod strict_boolean_expressions;
    pub mod strict_void_return;
    pub mod switch_exhaustiveness_check;
    pub mod unbound_method;
    pub mod use_unknown_in_catch_callback_variable;
}
