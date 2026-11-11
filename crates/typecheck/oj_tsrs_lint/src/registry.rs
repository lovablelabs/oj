//! Rule inventory from the pinned tsgolint registry.

pub const REFERENCE_COMMIT: &str = "4ebf80fd2b166cba4890c6896220fec4a1462b29";
pub const REFERENCE_COMPILER_COMMIT: &str = "2bd066d87f5bafd315be9f40889d0a60b9e58e0b";

pub const BASELINE_RULES: &[&str] = &[
    "await-thenable",
    "consistent-return",
    "consistent-type-exports",
    "dot-notation",
    "no-array-delete",
    "no-base-to-string",
    "no-confusing-void-expression",
    "no-deprecated",
    "no-duplicate-type-constituents",
    "no-floating-promises",
    "no-for-in-array",
    "no-generated-empty-object-type",
    "no-implied-eval",
    "no-meaningless-void-operator",
    "no-misused-promises",
    "no-misused-spread",
    "no-mixed-enums",
    "no-redundant-type-constituents",
    "no-unnecessary-boolean-literal-compare",
    "no-unnecessary-condition",
    "no-unnecessary-qualifier",
    "no-unnecessary-template-expression",
    "no-unnecessary-type-conversion",
    "no-unnecessary-type-arguments",
    "no-unnecessary-type-parameters",
    "no-unnecessary-type-assertion",
    "no-useless-default-assignment",
    "no-unsafe-argument",
    "no-unsafe-assignment",
    "no-unsafe-call",
    "no-unsafe-enum-assignment",
    "no-unsafe-enum-comparison",
    "no-unsafe-member-access",
    "no-unsafe-return",
    "no-unsafe-type-assertion",
    "no-unsafe-unary-minus",
    "non-nullable-type-assertion-style",
    "only-throw-error",
    "prefer-find",
    "prefer-includes",
    "prefer-optional-chain",
    "prefer-nullish-coalescing",
    "prefer-promise-reject-errors",
    "prefer-readonly-parameter-types",
    "prefer-regexp-exec",
    "prefer-readonly",
    "prefer-reduce-type-parameter",
    "prefer-return-this-type",
    "prefer-string-starts-ends-with",
    "promise-function-async",
    "related-getter-setter-pairs",
    "require-array-sort-compare",
    "require-await",
    "restrict-plus-operands",
    "restrict-template-expressions",
    "return-await",
    "strict-boolean-expressions",
    "strict-void-return",
    "switch-exhaustiveness-check",
    "unbound-method",
    "use-unknown-in-catch-callback-variable",
];

pub fn require_implemented(name: &str) -> Result<(), String> {
    if matches!(
        name,
        "await-thenable"
            | "consistent-return"
            | "consistent-type-exports"
            | "dot-notation"
            | "no-array-delete"
            | "no-base-to-string"
            | "no-confusing-void-expression"
            | "no-deprecated"
            | "no-duplicate-type-constituents"
            | "no-floating-promises"
            | "no-for-in-array"
            | "no-generated-empty-object-type"
            | "no-implied-eval"
            | "no-meaningless-void-operator"
            | "no-misused-promises"
            | "no-misused-spread"
            | "no-mixed-enums"
            | "no-redundant-type-constituents"
            | "no-unnecessary-boolean-literal-compare"
            | "no-unnecessary-condition"
            | "no-unnecessary-qualifier"
            | "no-unnecessary-template-expression"
            | "no-unnecessary-type-conversion"
            | "no-unnecessary-type-arguments"
            | "no-unnecessary-type-parameters"
            | "no-unnecessary-type-assertion"
            | "no-useless-default-assignment"
            | "no-unsafe-argument"
            | "no-unsafe-assignment"
            | "no-unsafe-call"
            | "no-unsafe-enum-assignment"
            | "no-unsafe-enum-comparison"
            | "no-unsafe-member-access"
            | "no-unsafe-return"
            | "no-unsafe-type-assertion"
            | "no-unsafe-unary-minus"
            | "non-nullable-type-assertion-style"
            | "only-throw-error"
            | "prefer-find"
            | "prefer-includes"
            | "prefer-optional-chain"
            | "prefer-nullish-coalescing"
            | "prefer-promise-reject-errors"
            | "prefer-readonly-parameter-types"
            | "prefer-regexp-exec"
            | "prefer-readonly"
            | "prefer-reduce-type-parameter"
            | "prefer-return-this-type"
            | "prefer-string-starts-ends-with"
            | "promise-function-async"
            | "related-getter-setter-pairs"
            | "require-array-sort-compare"
            | "require-await"
            | "restrict-plus-operands"
            | "restrict-template-expressions"
            | "return-await"
            | "strict-boolean-expressions"
            | "strict-void-return"
            | "switch-exhaustiveness-check"
            | "unbound-method"
            | "use-unknown-in-catch-callback-variable"
    ) {
        return Ok(());
    }
    if BASELINE_RULES.contains(&name) {
        Err(format!("rule not yet implemented: {name}"))
    } else {
        Err(format!("unknown rule: {name}"))
    }
}

pub fn create_rule(
    config: &crate::payload::ConfiguredRule,
) -> Result<Box<dyn crate::rule::Rule>, String> {
    match config.name.as_str() {
        "await-thenable" => crate::rules::await_thenable::AwaitThenable::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "consistent-return" => {
            crate::rules::consistent_return::ConsistentReturn::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "consistent-type-exports" => {
            crate::rules::consistent_type_exports::ConsistentTypeExports::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "dot-notation" => crate::rules::dot_notation::DotNotation::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "no-array-delete" => Ok(Box::new(crate::rules::no_array_delete::NoArrayDelete)),
        "no-base-to-string" => {
            crate::rules::no_base_to_string::NoBaseToString::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-confusing-void-expression" => {
            crate::rules::no_confusing_void_expression::NoConfusingVoidExpression::new(
                &config.options,
            )
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-deprecated" => crate::rules::no_deprecated::NoDeprecated::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "no-duplicate-type-constituents" => {
            crate::rules::no_duplicate_type_constituents::NoDuplicateTypeConstituents::new(
                &config.options,
            )
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-floating-promises" => {
            crate::rules::no_floating_promises::NoFloatingPromises::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-for-in-array" => Ok(Box::new(crate::rules::no_for_in_array::NoForInArray)),
        "no-generated-empty-object-type" => Ok(Box::new(crate::rules::no_generated_empty_object_type::NoGeneratedEmptyObjectType)),
        "no-implied-eval" => Ok(Box::new(crate::rules::no_implied_eval::NoImpliedEval)),
        "no-meaningless-void-operator" => {
            crate::rules::no_meaningless_void_operator::NoMeaninglessVoidOperator::new(
                &config.options,
            )
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-misused-promises" => {
            crate::rules::no_misused_promises::NoMisusedPromises::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-misused-spread" => {
            crate::rules::no_misused_spread::NoMisusedSpread::new(&config.options)
                .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>)
        }
        "no-mixed-enums" => Ok(Box::new(crate::rules::no_mixed_enums::NoMixedEnums)),
        "no-redundant-type-constituents" => Ok(Box::new(
            crate::rules::no_redundant_type_constituents::NoRedundantTypeConstituents,
        )),
        "no-unnecessary-boolean-literal-compare" => crate::rules::no_unnecessary_boolean_literal_compare::NoUnnecessaryBooleanLiteralCompare::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "no-unnecessary-condition" => crate::rules::no_unnecessary_condition::NoUnnecessaryCondition::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "no-unnecessary-qualifier" => Ok(Box::new(crate::rules::no_unnecessary_qualifier::NoUnnecessaryQualifier::default())),
        "no-unnecessary-template-expression" => Ok(Box::new(crate::rules::no_unnecessary_template_expression::NoUnnecessaryTemplateExpression)),
        "no-unnecessary-type-conversion" => Ok(Box::new(crate::rules::no_unnecessary_type_conversion::NoUnnecessaryTypeConversion)),
        "no-unnecessary-type-arguments" => Ok(Box::new(crate::rules::no_unnecessary_type_arguments::NoUnnecessaryTypeArguments)),
        "no-unnecessary-type-parameters" => Ok(Box::new(crate::rules::no_unnecessary_type_parameters::NoUnnecessaryTypeParameters)),
        "no-unnecessary-type-assertion" => crate::rules::no_unnecessary_type_assertion::NoUnnecessaryTypeAssertion::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "no-useless-default-assignment" => Ok(Box::new(crate::rules::no_useless_default_assignment::NoUselessDefaultAssignment)),
        "no-unsafe-argument" => Ok(Box::new(crate::rules::no_unsafe_argument::NoUnsafeArgument)),
        "no-unsafe-assignment" => Ok(Box::new(crate::rules::no_unsafe_assignment::NoUnsafeAssignment)),
        "no-unsafe-call" => Ok(Box::new(crate::rules::no_unsafe_call::NoUnsafeCall)),
        "no-unsafe-enum-assignment" => Ok(Box::new(crate::rules::no_unsafe_enum_assignment::NoUnsafeEnumAssignment::default())),
        "no-unsafe-enum-comparison" => Ok(Box::new(crate::rules::no_unsafe_enum_comparison::NoUnsafeEnumComparison)),
        "no-unsafe-member-access" => Ok(Box::new(crate::rules::no_unsafe_member_access::NoUnsafeMemberAccess::new(&config.options)?)),
        "no-unsafe-return" => Ok(Box::new(crate::rules::no_unsafe_return::NoUnsafeReturn)),
        "no-unsafe-type-assertion" => Ok(Box::new(crate::rules::no_unsafe_type_assertion::NoUnsafeTypeAssertion)),
        "no-unsafe-unary-minus" => Ok(Box::new(crate::rules::no_unsafe_unary_minus::NoUnsafeUnaryMinus)),
        "non-nullable-type-assertion-style" => Ok(Box::new(crate::rules::non_nullable_type_assertion_style::NonNullableTypeAssertionStyle)),
        "only-throw-error" => crate::rules::only_throw_error::OnlyThrowError::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-find" => Ok(Box::new(crate::rules::prefer_find::PreferFind)),
        "prefer-includes" => Ok(Box::new(crate::rules::prefer_includes::PreferIncludes)),
        "prefer-optional-chain" => crate::rules::prefer_optional_chain::PreferOptionalChain::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-nullish-coalescing" => crate::rules::prefer_nullish_coalescing::PreferNullishCoalescing::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-promise-reject-errors" => crate::rules::prefer_promise_reject_errors::PreferPromiseRejectErrors::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-readonly-parameter-types" => crate::rules::prefer_readonly_parameter_types::PreferReadonlyParameterTypes::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-regexp-exec" => Ok(Box::new(crate::rules::prefer_regexp_exec::PreferRegexpExec)),
        "prefer-readonly" => crate::rules::prefer_readonly::PreferReadonly::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "prefer-reduce-type-parameter" => Ok(Box::new(crate::rules::prefer_reduce_type_parameter::PreferReduceTypeParameter)),
        "prefer-return-this-type" => Ok(Box::new(crate::rules::prefer_return_this_type::PreferReturnThisType)),
        "prefer-string-starts-ends-with" => crate::rules::prefer_string_starts_ends_with::PreferStringStartsEndsWith::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "promise-function-async" => crate::rules::promise_function_async::PromiseFunctionAsync::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "related-getter-setter-pairs" => Ok(Box::new(crate::rules::related_getter_setter_pairs::RelatedGetterSetterPairs)),
        "require-array-sort-compare" => crate::rules::require_array_sort_compare::RequireArraySortCompare::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "require-await" => Ok(Box::new(crate::rules::require_await::RequireAwait::default())),
        "restrict-plus-operands" => crate::rules::restrict_plus_operands::RestrictPlusOperands::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "restrict-template-expressions" => crate::rules::restrict_template_expressions::RestrictTemplateExpressions::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "return-await" => crate::rules::return_await::ReturnAwait::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "strict-boolean-expressions" => crate::rules::strict_boolean_expressions::StrictBooleanExpressions::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "strict-void-return" => crate::rules::strict_void_return::StrictVoidReturn::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "switch-exhaustiveness-check" => crate::rules::switch_exhaustiveness_check::SwitchExhaustivenessCheck::new(&config.options).map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "unbound-method" => crate::rules::unbound_method::UnboundMethod::new(&config.options)
            .map(|rule| Box::new(rule) as Box<dyn crate::rule::Rule>),
        "use-unknown-in-catch-callback-variable" => Ok(Box::new(crate::rules::use_unknown_in_catch_callback_variable::UseUnknownInCatchCallbackVariable)),
        name => {
            require_implemented(name)?;
            unreachable!("registered rule factory")
        }
    }
}
