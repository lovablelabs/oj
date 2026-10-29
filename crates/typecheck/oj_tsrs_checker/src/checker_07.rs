use crate::*;
use rustc_hash::FxHashSet;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:13069-15238 (hand-ported by checker-foundation):
//   type PredicateSemantics (checker.go:13085)
//   const (checker.go:13087): PredicateSemanticsNone, PredicateSemanticsAlways, PredicateSemanticsNever,
//     PredicateSemanticsSometimes

// Mutable locals of Go's checkObjectLiteral that its nested createObjectLiteralType closure reads.
#[derive(Clone)]
struct ObjectLiteralState {
    in_destructuring_pattern: bool,
    contextual_type: Option<P<Type>>,
    object_flags: ObjectFlags,
    pattern_with_computed_properties: bool,
    has_computed_string_property: bool,
    has_computed_number_property: bool,
    has_computed_symbol_property: bool,
    // Go's `propertiesTable`, made on first use: Go makes a new one after every spread, and the last one is
    // usually never used (an object literal ending in a spread); an empty table is unobservable until it is used.
    properties_table: Cell<Option<P<SymbolTable>>>,
    properties_array: Vec<P<Symbol>>,
    offset: usize,
}

impl ObjectLiteralState {
    fn properties_table(&self) -> P<SymbolTable> {
        self.properties_table_for(0)
    }

    /// `properties_table`, made with room for `capacity` members (the literal's members still to come) if new.
    fn properties_table_for(&self, capacity: usize) -> P<SymbolTable> {
        if let Some(table) = self.properties_table.get() {
            table
        } else {
            let table = SymbolTable::with_capacity(capacity);
            self.properties_table.set(Some(table));
            table
        }
    }
}

fn create_object_literal_type(c: &mut Checker, node: P<Node>, st: &ObjectLiteralState) -> P<Type> {
    let mut index_infos: Vec<P<IndexInfo>> = Vec::new();
    let is_readonly = c.is_const_context(node);
    if st.has_computed_string_property {
        let string_type = c.string_type;
        index_infos.push(c.get_object_literal_index_info(
            is_readonly,
            &st.properties_array[st.offset..],
            string_type,
        ));
    }
    if st.has_computed_number_property {
        let number_type = c.number_type;
        index_infos.push(c.get_object_literal_index_info(
            is_readonly,
            &st.properties_array[st.offset..],
            number_type,
        ));
    }
    if st.has_computed_symbol_property {
        let es_symbol_type = c.es_symbol_type;
        index_infos.push(c.get_object_literal_index_info(
            is_readonly,
            &st.properties_array[st.offset..],
            es_symbol_type,
        ));
    }
    let result =
        c.new_anonymous_type(node.symbol(), Some(st.properties_table()), &[], &[], &index_infos);
    result.object_flags.set(
        result.object_flags_lazy()
            | st.object_flags
            | ObjectFlags::ObjectLiteral
            | ObjectFlags::ContainsObjectOrArrayLiteral,
    );
    if st.contextual_type.is_none() && ast::is_in_js_file(node) && !ast::is_in_json_file(node) {
        result.object_flags.set(result.object_flags_lazy() | ObjectFlags::JSLiteral);
    }
    if st.pattern_with_computed_properties {
        result.object_flags.set(
            result.object_flags_lazy() | ObjectFlags::ObjectLiteralPatternWithComputedProperties,
        );
    }
    if st.in_destructuring_pattern {
        c.pattern_for_type.insert(result, node);
    }
    result
}

impl Checker {
    // checker.go:13069
    pub(crate) fn is_type_equality_comparable_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
    ) -> bool {
        target.flags().intersects(TypeFlags::Nullable) || self.is_type_comparable_to(source, target)
    }

    // checker.go:13073
    pub(crate) fn check_truthiness_of_type(&mut self, t: P<Type>, node: P<Node>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Void) {
            self.error(
                Some(node),
                &diagnostics::AN_EXPRESSION_OF_TYPE_VOID_CANNOT_BE_TESTED_FOR_TRUTHINESS,
                &[],
            );
            return t;
        }
        let semantics = self.get_syntactic_truthy_semantics(node);
        if semantics != PredicateSemantics::Sometimes {
            self.error(
                Some(node),
                if semantics == PredicateSemantics::Always {
                    &diagnostics::THIS_KIND_OF_EXPRESSION_IS_ALWAYS_TRUTHY
                } else {
                    &diagnostics::THIS_KIND_OF_EXPRESSION_IS_ALWAYS_FALSY
                },
                &[],
            );
        }
        t
    }

    // checker.go:13094
    fn get_syntactic_truthy_semantics(&mut self, node: P<Node>) -> PredicateSemantics {
        let node = ast::skip_outer_expressions(node, OuterExpressionKinds::All);
        match node.kind() {
            Kind::NumericLiteral => {
                // Allow `while(0)` or `while(1)`
                if node.text() == "0" || node.text() == "1" {
                    return PredicateSemantics::Sometimes;
                }
                return PredicateSemantics::Always;
            }
            Kind::ArrayLiteralExpression
            | Kind::ArrowFunction
            | Kind::BigIntLiteral
            | Kind::ClassExpression
            | Kind::FunctionExpression
            | Kind::JsxElement
            | Kind::JsxSelfClosingElement
            | Kind::ObjectLiteralExpression
            | Kind::RegularExpressionLiteral => {
                return PredicateSemantics::Always;
            }
            Kind::VoidExpression | Kind::NullKeyword => {
                return PredicateSemantics::Never;
            }
            Kind::NoSubstitutionTemplateLiteral | Kind::StringLiteral => {
                if !node.text().is_empty() {
                    return PredicateSemantics::Always;
                }
                return PredicateSemantics::Never;
            }
            Kind::ConditionalExpression => {
                let cond = node.as_conditional_expression();
                return self.get_syntactic_truthy_semantics(cond.when_true)
                    | self.get_syntactic_truthy_semantics(cond.when_false);
            }
            Kind::Identifier if self.get_resolved_symbol(node) == self.undefined_symbol => {
                return PredicateSemantics::Never;
            }
            _ => {}
        }
        PredicateSemantics::Sometimes
    }

    // checker.go:13123
    pub(crate) fn check_nullish_coalesce_operands(&mut self, left: P<Node>, right: P<Node>) {
        let grandparent = left.parent().unwrap().parent().unwrap();
        if ast::is_binary_expression(grandparent) {
            let grandparent_left = grandparent.as_binary_expression().left;
            let grandparent_operator_token = grandparent.as_binary_expression().operator_token;
            if ast::is_binary_expression(grandparent_left)
                && grandparent_operator_token.kind() == Kind::BarBarToken
            {
                self.grammar_error_on_node(
                    grandparent_left,
                    &diagnostics::X_0_AND_1_OPERATIONS_CANNOT_BE_MIXED_WITHOUT_PARENTHESES,
                    &[
                        &tsrs_scanner::token_to_string(Kind::QuestionQuestionToken),
                        &tsrs_scanner::token_to_string(grandparent_operator_token.kind()),
                    ],
                );
            }
        } else if ast::is_binary_expression(left) {
            let operator_token = left.as_binary_expression().operator_token;
            if operator_token.kind() == Kind::BarBarToken
                || operator_token.kind() == Kind::AmpersandAmpersandToken
            {
                self.grammar_error_on_node(
                    left,
                    &diagnostics::X_0_AND_1_OPERATIONS_CANNOT_BE_MIXED_WITHOUT_PARENTHESES,
                    &[
                        &tsrs_scanner::token_to_string(operator_token.kind()),
                        &tsrs_scanner::token_to_string(Kind::QuestionQuestionToken),
                    ],
                );
            }
        } else if ast::is_binary_expression(right) {
            let operator_token = right.as_binary_expression().operator_token;
            if operator_token.kind() == Kind::AmpersandAmpersandToken {
                self.grammar_error_on_node(
                    right,
                    &diagnostics::X_0_AND_1_OPERATIONS_CANNOT_BE_MIXED_WITHOUT_PARENTHESES,
                    &[
                        &tsrs_scanner::token_to_string(Kind::QuestionQuestionToken),
                        &tsrs_scanner::token_to_string(operator_token.kind()),
                    ],
                );
            }
        }
        self.check_nullish_coalesce_operand_left(left);
    }

    // checker.go:13144
    fn check_nullish_coalesce_operand_left(&mut self, left: P<Node>) {
        let left_target = ast::skip_outer_expressions(left, OuterExpressionKinds::All);
        let nullish_semantics = self.get_syntactic_nullishness_semantics(left_target);
        if nullish_semantics != PredicateSemantics::Sometimes {
            if nullish_semantics == PredicateSemantics::Always {
                self.error(Some(left_target), &diagnostics::THIS_EXPRESSION_IS_ALWAYS_NULLISH, &[]);
            } else {
                self.error(Some(left_target), &diagnostics::RIGHT_OPERAND_OF_IS_UNREACHABLE_BECAUSE_THE_LEFT_OPERAND_IS_NEVER_NULLISH, &[]);
            }
        }
    }

    // checker.go:13156
    fn get_syntactic_nullishness_semantics(&mut self, node: P<Node>) -> PredicateSemantics {
        let node = ast::skip_outer_expressions(node, OuterExpressionKinds::All);
        match node.kind() {
            Kind::AwaitExpression
            | Kind::CallExpression
            | Kind::TaggedTemplateExpression
            | Kind::ElementAccessExpression
            | Kind::MetaProperty
            | Kind::NewExpression
            | Kind::PropertyAccessExpression
            | Kind::YieldExpression
            | Kind::ThisKeyword => {
                return PredicateSemantics::Sometimes;
            }
            Kind::BinaryExpression => {
                let bin = node.as_binary_expression();
                // List of operators that can produce null/undefined:
                // || ||= && &&= ?? ??=
                match bin.operator_token.kind() {
                    Kind::BarBarToken
                    | Kind::BarBarEqualsToken
                    | Kind::AmpersandAmpersandToken
                    | Kind::AmpersandAmpersandEqualsToken => {
                        return PredicateSemantics::Sometimes;
                    }
                    // For these operator kinds, the right operand is effectively controlling
                    Kind::CommaToken | Kind::EqualsToken => {
                        return self.get_syntactic_nullishness_semantics(bin.right());
                    }
                    // For nullish coalescing: result is the left operand when left is non-null,
                    // or the right operand when left is null/undefined. The nullishness of the
                    // result combines both paths: the left's non-null path contributes Never,
                    // and when left can be null, the right's semantics are also included.
                    Kind::QuestionQuestionToken | Kind::QuestionQuestionEqualsToken => {
                        let left_semantics = self.get_syntactic_nullishness_semantics(bin.left);
                        // The non-null path (left is non-null): left branch taken, result has Never bit
                        let mut result = left_semantics & PredicateSemantics::Never;
                        // The null path (left is null/undefined): right branch taken, result inherits right's semantics
                        if left_semantics.intersects(PredicateSemantics::Always) {
                            result |= self.get_syntactic_nullishness_semantics(bin.right());
                        }
                        return result;
                    }
                    _ => {}
                }
                return PredicateSemantics::Never;
            }
            Kind::ConditionalExpression => {
                let cond = node.as_conditional_expression();
                return self.get_syntactic_nullishness_semantics(cond.when_true)
                    | self.get_syntactic_nullishness_semantics(cond.when_false);
            }
            Kind::NullKeyword => {
                return PredicateSemantics::Always;
            }
            Kind::Identifier => {
                if self.get_resolved_symbol(node) == self.undefined_symbol {
                    return PredicateSemantics::Always;
                }
                return PredicateSemantics::Sometimes;
            }
            _ => {}
        }
        PredicateSemantics::Never
    }

    /**
     * This is a *shallow* check: An expression is side-effect-free if the
     * evaluation of the expression *itself* cannot produce side effects.
     * For example, x++ / 3 is side-effect free because the / operator
     * does not have side effects.
     * The intent is to "smell test" an expression for correctness in positions where
     * its value is discarded (e.g. the left side of the comma operator).
     */
    // checker.go:13219
    pub(crate) fn is_side_effect_free(node: P<Node>) -> bool {
        let node = ast::skip_parentheses(node);
        match node.kind() {
            Kind::Identifier
            | Kind::StringLiteral
            | Kind::RegularExpressionLiteral
            | Kind::TaggedTemplateExpression
            | Kind::TemplateExpression
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NullKeyword
            | Kind::UndefinedKeyword
            | Kind::FunctionExpression
            | Kind::ClassExpression
            | Kind::ArrowFunction
            | Kind::ArrayLiteralExpression
            | Kind::ObjectLiteralExpression
            | Kind::TypeOfExpression
            | Kind::NonNullExpression
            | Kind::JsxSelfClosingElement
            | Kind::JsxElement => {
                return true;
            }
            Kind::ConditionalExpression => {
                let cond = node.as_conditional_expression();
                return Self::is_side_effect_free(cond.when_true)
                    && Self::is_side_effect_free(cond.when_false);
            }
            Kind::BinaryExpression => {
                let bin = node.as_binary_expression();
                if ast::is_assignment_operator(bin.operator_token.kind()) {
                    return false;
                }
                return Self::is_side_effect_free(bin.left)
                    && Self::is_side_effect_free(bin.right());
            }
            Kind::PrefixUnaryExpression => {
                // Unary operators ~, !, +, and - have no side effects.
                // The rest do.
                match node.as_prefix_unary_expression().operator {
                    Kind::ExclamationToken
                    | Kind::PlusToken
                    | Kind::MinusToken
                    | Kind::TildeToken => {
                        return true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        false
    }

    // Return true for "indirect calls", (i.e. `(0, x.f)(...)` or `(0, eval)(...)`), which prevents passing `this`.
    // checker.go:13247
    pub(crate) fn is_indirect_call(node: P<Node>) -> bool {
        let left = node.as_binary_expression().left;
        let right = node.as_binary_expression().right();
        let parent = node.parent().unwrap();
        ast::is_parenthesized_expression(parent)
            && ast::is_numeric_literal(left)
            && left.text() == "0"
            && (ast::is_call_expression(parent.parent().unwrap())
                && parent.parent().unwrap().expression() == Some(parent)
                || ast::is_tagged_template_expression(parent.parent().unwrap()))
            && (ast::is_access_expression(right)
                || ast::is_identifier(right) && right.text() == "eval")
    }

    // checker.go:13255
    pub(crate) fn check_instance_of_expression(
        &mut self,
        left: P<Node>,
        right: P<Node>,
        left_type: P<Type>,
        right_type: P<Type>,
        check_mode: CheckMode,
    ) -> P<Type> {
        if left_type == self.silent_never_type || right_type == self.silent_never_type {
            return self.silent_never_type;
        }
        // TypeScript 1.0 spec (April 2014): 4.15.4
        // The instanceof operator requires the left operand to be of type Any, an object type, or a type parameter type,
        // and the right operand to be of type Any, a subtype of the 'Function' interface type, or have a call or construct signature.
        // The result is always of the Boolean primitive type.
        // NOTE: do not raise error if leftType is unknown as related error was already reported
        if !is_type_any(Some(left_type))
            && self.all_types_assignable_to_kind(left_type, TypeFlags::Primitive)
        {
            self.error(Some(left), &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_INSTANCEOF_EXPRESSION_MUST_BE_OF_TYPE_ANY_AN_OBJECT_TYPE_OR_A_TYPE_PARAMETER, &[]);
        }
        let signature = self.get_resolved_signature(
            left.parent().unwrap(),
            None, /*candidatesOutArray*/
            check_mode,
        );
        if signature == self.resolving_signature {
            // CheckMode.SkipGenericFunctions is enabled and this is a call to a generic function that
            // returns a function type. We defer checking and return silentNeverType.
            return self.silent_never_type;
        }
        // If rightType has a `[Symbol.hasInstance]` method that is not `(value: unknown) => boolean`, we
        // must check the expression as if it were a call to `right[Symbol.hasInstance](left)`. The call to
        // `getResolvedSignature`, below, will check that leftType is assignable to the type of the first
        // parameter.
        let return_type = self.get_return_type_of_signature(signature);
        // We also verify that the return type of the `[Symbol.hasInstance]` method is assignable to
        // `boolean`. According to the spec, the runtime will actually perform `ToBoolean` on the result,
        // but this is more type-safe.
        let boolean_type = self.boolean_type;
        self.check_type_assignable_to(
            return_type,
            boolean_type,
            Some(right),
            Some(&diagnostics::AN_OBJECT_S_SYMBOL_HAS_INSTANCE_METHOD_MUST_RETURN_A_BOOLEAN_VALUE_FOR_IT_TO_BE_USED_ON_THE_RIGHT_HAND_SIDE_OF_AN_INSTANCEOF_EXPRESSION),
        );
        self.boolean_type
    }

    // checker.go:13285
    pub(crate) fn check_in_expression(
        &mut self,
        left: P<Node>,
        right: P<Node>,
        left_type: P<Type>,
        right_type: P<Type>,
    ) -> P<Type> {
        if left_type == self.silent_never_type || right_type == self.silent_never_type {
            return self.silent_never_type;
        }
        if ast::is_private_identifier(left) {
            if self.language_version
                < LANGUAGE_FEATURE_MINIMUM_TARGET.private_names_and_class_static_blocks
                || self.language_version
                    < LANGUAGE_FEATURE_MINIMUM_TARGET.class_and_class_element_decorators
                || !self.compiler_options.get_use_define_for_class_fields()
            {
                self.check_external_emit_helpers(left, ExternalEmitHelpers::ClassPrivateFieldIn);
            }
            // Unlike in 'checkPrivateIdentifierExpression' we now have access to the RHS type
            // which provides us with the opportunity to emit more detailed errors
            if self.symbol_node_links.get(left).resolved_symbol.get().is_none()
                && ast::get_containing_class(left).is_some()
            {
                let is_unchecked_js = self.is_unchecked_js_suggestion(
                    Some(left),
                    right_type.symbol(),
                    true, /*excludeClasses*/
                );
                self.report_nonexistent_property(left, right_type, is_unchecked_js);
            }
        } else {
            // The type of the left operand must be assignable to string, number, or symbol.
            let t = self.check_non_null_type(left_type, left);
            let string_number_symbol_type = self.string_number_symbol_type;
            self.check_type_assignable_to(t, string_number_symbol_type, Some(left), None);
        }
        // The type of the right operand must be assignable to 'object'.
        let t = self.check_non_null_type(right_type, right);
        let non_primitive_type = self.non_primitive_type;
        if self.check_type_assignable_to(t, non_primitive_type, Some(right), None) {
            // The {} type is assignable to the object type, yet {} might represent a primitive type. Here we
            // detect and error on {} that results from narrowing the unknown type, as well as intersections
            // that include {} (we know that the other types in such intersections are assignable to object
            // since we already checked for that).
            if self.has_empty_object_intersection(right_type) {
                let s = self.type_to_string_exported(right_type);
                self.error(Some(right), &diagnostics::TYPE_0_MAY_REPRESENT_A_PRIMITIVE_VALUE_WHICH_IS_NOT_PERMITTED_AS_THE_RIGHT_OPERAND_OF_THE_IN_OPERATOR, &[&s]);
            }
        }
        // The result is always of the Boolean primitive type.
        self.boolean_type
    }

    // checker.go:13319
    fn has_empty_object_intersection(&mut self, t: P<Type>) -> bool {
        some_type(self, t, |c, t| {
            t == c.unknown_empty_object_type
                || t.flags().intersects(TypeFlags::Intersection) && {
                    let base = c.get_base_constraint_or_type(t);
                    c.is_empty_anonymous_object_type(base)
                }
        })
    }

    // checker.go:13325
    pub(crate) fn get_exact_optional_unassignable_properties(
        &mut self,
        source: P<Type>,
        target: P<Type>,
    ) -> Vec<P<Symbol>> {
        if is_tuple_type(source) && is_tuple_type(target) {
            return Vec::new();
        }
        let mut result = Vec::new();
        result.extend(self.get_properties_of_type(target).iter().copied().filter(|&target_prop| {
            let s = self.get_type_of_property_of_type(source, target_prop.name());
            let t = self.get_type_of_symbol(target_prop);
            self.is_exact_optional_property_mismatch(s, Some(t))
        }));
        result
    }

    // checker.go:13334
    pub(crate) fn is_exact_optional_property_mismatch(
        &mut self,
        source: Option<P<Type>>,
        target: Option<P<Type>>,
    ) -> bool {
        match (source, target) {
            (Some(source), Some(target)) => {
                self.maybe_type_of_kind(source, TypeFlags::Undefined)
                    && self.contains_missing_type(target)
            }
            _ => false,
        }
    }

    // checker.go:13338
    pub(crate) fn check_reference_expression(
        &mut self,
        expr: P<Node>,
        invalid_reference_message: &'static Message,
        invalid_optional_chain_message: &'static Message,
    ) -> bool {
        // References are combinations of identifiers, parentheses, and property accesses.
        let node = ast::skip_outer_expressions(
            expr,
            OuterExpressionKinds::Assertions | OuterExpressionKinds::Parentheses,
        );
        if node.kind() != Kind::Identifier && !ast::is_access_expression(node) {
            self.error(Some(expr), invalid_reference_message, &[]);
            return false;
        }
        if node.flags().intersects(NodeFlags::OptionalChain) {
            self.error(Some(expr), invalid_optional_chain_message, &[]);
            return false;
        }
        true
    }

    // checker.go:13352
    pub(crate) fn check_object_literal(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        // Expando object literals have empty properties but filled exports
        if node.properties().is_empty()
            && node.symbol().is_some()
            && node.symbol().unwrap().exports().map_or(0, |e| e.len()) != 0
        {
            let symbol = node.symbol().unwrap();
            let result = self.new_anonymous_type(Some(symbol), symbol.exports(), &[], &[], &[]);
            if ast::is_in_js_file(node) && !ast::is_in_json_file(node) {
                result.object_flags.set(result.object_flags_lazy() | ObjectFlags::JSLiteral);
            }
            // An expando object literal has no property children (len == 0), so there
            // is nothing to check here.
            return result;
        }
        self.check_node_deferred(node);
        let in_destructuring_pattern = ast::is_assignment_target(node);
        // Grammar checking
        self.check_grammar_object_literal_expression(node, in_destructuring_pattern);
        // Local to this call (Go's GC collects it): owned here, not allocated in the arena.
        let all_properties_table: Option<SymbolTable> =
            self.strict_null_checks.then(SymbolTable::default);
        let mut spread = self.empty_object_type;
        self.push_cached_contextual_type(node);
        let contextual_type = self.get_apparent_type_of_contextual_type(node, ContextFlags::None);
        let mut contextual_type_has_pattern = false;
        if let Some(contextual_type) = contextual_type
            && let Some(pattern) = self.pattern_for_type.get(&contextual_type).copied()
            && (ast::is_object_binding_pattern(pattern)
                || ast::is_object_literal_expression(pattern))
        {
            contextual_type_has_pattern = true;
        }
        let in_const_context = self.is_const_context(node);
        let mut check_flags = CheckFlags::empty();
        if in_const_context {
            check_flags = CheckFlags::Readonly;
        }
        let mut st = ObjectLiteralState {
            in_destructuring_pattern,
            contextual_type,
            object_flags: ObjectFlags::FreshLiteral,
            pattern_with_computed_properties: false,
            has_computed_string_property: false,
            has_computed_number_property: false,
            has_computed_symbol_property: false,
            properties_table: Cell::new(None),
            properties_array: Vec::new(),
            offset: 0,
        };
        // Spreads may cause an early bail; ensure computed names are always checked (this is cached)
        // As otherwise they may not be checked until exports for the type at this position are retrieved,
        // which may never occur.
        for &elem in node.properties() {
            if let Some(name) = elem.name()
                && ast::is_computed_property_name(name)
            {
                self.check_computed_property_name(name);
            }
        }
        for (member_index, &member_decl) in node.properties().iter().enumerate() {
            let mut member = self.get_symbol_of_declaration(member_decl);
            let mut computed_name_type: Option<P<Type>> = None;
            if let Some(name) = member_decl.name()
                && name.kind() == Kind::ComputedPropertyName
            {
                computed_name_type = Some(self.check_computed_property_name(name));
            }
            if ast::is_property_assignment(member_decl)
                || ast::is_shorthand_property_assignment(member_decl)
                || ast::is_object_literal_method(member_decl)
            {
                let t = match member_decl.kind() {
                    Kind::PropertyAssignment => {
                        self.check_property_assignment(member_decl, check_mode)
                    }
                    Kind::ShorthandPropertyAssignment => self.check_shorthand_property_assignment(
                        member_decl,
                        in_destructuring_pattern,
                        check_mode,
                    ),
                    _ => self.check_object_literal_method(member_decl, check_mode),
                };
                st.object_flags |= t.object_flags() & ObjectFlags::PropagatingFlags;
                let mut name_type: Option<P<Type>> = None;
                if let Some(computed_name_type) = computed_name_type
                    && is_type_usable_as_property_name(computed_name_type)
                {
                    name_type = Some(computed_name_type);
                }
                let member_symbol = member.unwrap();
                let prop = if let Some(name_type) = name_type {
                    self.new_symbol_ex(
                        SymbolFlags::Property | member_symbol.flags(),
                        alloc_str(&get_property_name_from_type(name_type)),
                        check_flags | CheckFlags::Late,
                    )
                } else {
                    self.new_symbol_ex(
                        SymbolFlags::Property | member_symbol.flags(),
                        member_symbol.name(),
                        check_flags,
                    )
                };
                let links = self.value_symbol_links.get(prop);
                if name_type.is_some() {
                    links.set_name_type(name_type);
                }
                if in_destructuring_pattern && Self::has_default_value(member_decl) {
                    // If object literal is an assignment pattern and if the assignment pattern specifies a default value
                    // for the property, make the property optional.
                    prop.flags.set(prop.flags() | SymbolFlags::Optional);
                } else if contextual_type_has_pattern
                    && !contextual_type
                        .unwrap()
                        .object_flags()
                        .intersects(ObjectFlags::ObjectLiteralPatternWithComputedProperties)
                {
                    // If object literal is contextually typed by the implied type of a binding pattern, and if the
                    // binding pattern specifies a default value for the property, make the property optional.
                    let contextual_type = contextual_type.unwrap();
                    let implied_prop =
                        self.get_property_of_type(contextual_type, member_symbol.name());
                    if let Some(implied_prop) = implied_prop {
                        prop.flags
                            .set(prop.flags() | (implied_prop.flags() & SymbolFlags::Optional));
                    } else {
                        let string_type = self.string_type;
                        if self.get_index_info_of_type(contextual_type, string_type).is_none() {
                            let a = self.symbol_to_string(member_symbol);
                            let b = self.type_to_string_exported(contextual_type);
                            self.error(member_decl.name(), &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_AND_0_DOES_NOT_EXIST_IN_TYPE_1, &[&a, &b]);
                        }
                    }
                }
                prop.set_declarations_static(member_symbol.declarations());
                prop.set_parent(member_symbol.parent());
                prop.set_value_declaration(member_symbol.value_declaration());
                links.resolved_type.set(Some(t));
                links.set_target(Some(member_symbol));
                member = Some(prop);
                if let Some(all_properties_table) = &all_properties_table {
                    all_properties_table.set(prop.name(), prop);
                }
                if contextual_type.is_some()
                    && check_mode.intersects(CheckMode::Inferential)
                    && !check_mode.intersects(CheckMode::SkipContextSensitive)
                    && (ast::is_property_assignment(member_decl)
                        || ast::is_method_declaration(member_decl))
                    && self.is_context_sensitive(member_decl)
                {
                    let inference_context = self.get_inference_context(node);
                    // In CheckMode.Inferential we should always have an inference context
                    let mut inference_node = member_decl;
                    if ast::is_property_assignment(member_decl) {
                        inference_node = member_decl.initializer().unwrap();
                    }
                    Self::add_intra_expression_inference_site(
                        inference_context.unwrap(),
                        inference_node,
                        t,
                    );
                }
            } else if member_decl.kind() == Kind::SpreadAssignment {
                if !st.properties_array.is_empty() {
                    let object_literal_type = create_object_literal_type(self, node, &st);
                    spread = self.get_spread_type(
                        spread,
                        object_literal_type,
                        node.symbol(),
                        st.object_flags,
                        in_const_context,
                    );
                    st.properties_array = Vec::new();
                    st.properties_table = Cell::new(None);
                    st.has_computed_string_property = false;
                    st.has_computed_number_property = false;
                    st.has_computed_symbol_property = false;
                }
                let checked = self.check_expression_ex(
                    member_decl.expression().unwrap(),
                    check_mode & CheckMode::Inferential,
                );
                let t = self.get_reduced_type(checked);
                if self.is_valid_spread_type(t) {
                    let merged_type =
                        self.try_merge_union_of_object_type_and_empty_object(t, in_const_context);
                    if all_properties_table.is_some() {
                        self.check_spread_prop_overrides(
                            merged_type,
                            all_properties_table.as_ref(),
                            member_decl,
                        );
                    }
                    st.offset = st.properties_array.len();
                    if self.is_error_type(spread) {
                        continue;
                    }
                    spread = self.get_spread_type(
                        spread,
                        merged_type,
                        node.symbol(),
                        st.object_flags,
                        in_const_context,
                    );
                } else {
                    self.error(
                        Some(member_decl),
                        &diagnostics::SPREAD_TYPES_MAY_ONLY_BE_CREATED_FROM_OBJECT_TYPES,
                        &[],
                    );
                    spread = self.error_type;
                }
                continue;
            } else {
                // TypeScript 1.0 spec (April 2014)
                // A get accessor declaration is processed in the same manner as
                // an ordinary function declaration(section 6.1) with no parameters.
                // A set accessor declaration is processed in the same manner
                // as an ordinary function declaration with a single parameter and a Void return type.
                assert!(
                    member_decl.kind() == Kind::GetAccessor
                        || member_decl.kind() == Kind::SetAccessor
                );
                self.check_node_deferred(member_decl);
            }
            let member = member.unwrap();
            if let Some(computed_name_type) = computed_name_type
                && !computed_name_type.flags().intersects(TypeFlags::StringOrNumberLiteralOrUnique)
            {
                let string_number_symbol_type = self.string_number_symbol_type;
                if self.is_type_assignable_to(computed_name_type, string_number_symbol_type) {
                    let number_type = self.number_type;
                    let es_symbol_type = self.es_symbol_type;
                    if self.is_type_assignable_to(computed_name_type, number_type) {
                        st.has_computed_number_property = true;
                    } else if self.is_type_assignable_to(computed_name_type, es_symbol_type) {
                        st.has_computed_symbol_property = true;
                    } else {
                        st.has_computed_string_property = true;
                    }
                    if in_destructuring_pattern {
                        st.pattern_with_computed_properties = true;
                    }
                }
            } else {
                st.properties_table_for(node.properties().len() - member_index)
                    .set(member.name(), member);
            }
            st.properties_array.push(member);
        }
        self.pop_contextual_type();
        if self.is_error_type(spread) {
            return self.error_type;
        }
        if spread != self.empty_object_type {
            if !st.properties_array.is_empty() {
                let object_literal_type = create_object_literal_type(self, node, &st);
                spread = self.get_spread_type(
                    spread,
                    object_literal_type,
                    node.symbol(),
                    st.object_flags,
                    in_const_context,
                );
                st.properties_array = Vec::new();
                st.properties_table = Cell::new(None);
                st.has_computed_string_property = false;
                st.has_computed_number_property = false;
            }
            // remap the raw emptyObjectType fed in at the top into a fresh empty object literal type, unique to this use site
            return self
                .map_type(spread, |c, t| {
                    if t == c.empty_object_type {
                        return Some(create_object_literal_type(c, node, &st));
                    }
                    Some(t)
                })
                .unwrap();
        }
        create_object_literal_type(self, node, &st)
    }

    // checker.go:13564
    pub(crate) fn check_contextual_deprecations(&mut self, node: P<Node>) {
        let contextual_type = self.get_apparent_type_of_contextual_type(node, ContextFlags::None);
        for &property in node.properties() {
            if self.is_canceled() {
                return;
            }
            if let Some(name) = property.name()
                && !ast::is_computed_property_name(name)
            {
                self.check_deprecated_property(name, contextual_type);
            }
        }
    }

    // checker.go:13576
    fn check_deprecated_property(&mut self, name: P<Node>, contextual_type: Option<P<Type>>) {
        let Some(contextual_type) = contextual_type else {
            return;
        };
        let prop = self.get_property_of_type(contextual_type, name.text());
        let Some(prop) = prop else {
            return;
        };
        if prop.declarations().is_empty() {
            return;
        }
        if self.is_deprecated_symbol(prop) {
            let declarations = prop.declarations();
            self.add_deprecated_suggestion(name, declarations, name.text());
        }
    }

    // checker.go:13589
    pub(crate) fn check_spread_prop_overrides(
        &mut self,
        t: P<Type>,
        props: Option<&SymbolTable>,
        spread: P<Node>,
    ) {
        for &right in self.get_properties_of_type(t) {
            if !right.flags().intersects(SymbolFlags::Optional)
                && !right.check_flags().intersects(CheckFlags::Partial)
                && let Some(left) = props.and_then(|props| props.lookup(right.name()))
            {
                let diagnostic = self.error(
                    left.value_declaration(),
                    &diagnostics::X_0_IS_SPECIFIED_MORE_THAN_ONCE_SO_THIS_USAGE_WILL_BE_OVERWRITTEN,
                    &[&left.name()],
                );
                diagnostic.add_related_info(new_diagnostic_for_node(
                    Some(spread),
                    Some(&diagnostics::THIS_SPREAD_ALWAYS_OVERWRITES_THIS_PROPERTY),
                    &[],
                ));
            }
        }
    }

    /**
     * Since the source of spread types are object literals, which are not binary,
     * this function should be called in a left folding style, with left = previous result of getSpreadType
     * and right = the new element to be spread.
     */
    // checker.go:13605
    pub(crate) fn get_spread_type(
        &mut self,
        left: P<Type>,
        right: P<Type>,
        symbol: Option<P<Symbol>>,
        object_flags: ObjectFlags,
        readonly: bool,
    ) -> P<Type> {
        if left.flags().intersects(TypeFlags::Any) || right.flags().intersects(TypeFlags::Any) {
            return self.any_type;
        }
        if left.flags().intersects(TypeFlags::Unknown)
            || right.flags().intersects(TypeFlags::Unknown)
        {
            return self.unknown_type;
        }
        if left.flags().intersects(TypeFlags::Never) {
            return right;
        }
        if right.flags().intersects(TypeFlags::Never) {
            return left;
        }
        let left = self.try_merge_union_of_object_type_and_empty_object(left, readonly);
        if left.flags().intersects(TypeFlags::Union) {
            if self.check_cross_product_union(&[left, right], Self::too_complex_key(&[left, right]))
            {
                return self
                    .map_type(left, |c, t| {
                        Some(c.get_spread_type(t, right, symbol, object_flags, readonly))
                    })
                    .unwrap();
            }
            return self.error_type;
        }
        let right = self.try_merge_union_of_object_type_and_empty_object(right, readonly);
        if right.flags().intersects(TypeFlags::Union) {
            if self.check_cross_product_union(&[left, right], Self::too_complex_key(&[left, right]))
            {
                return self
                    .map_type(right, |c, t| {
                        Some(c.get_spread_type(left, t, symbol, object_flags, readonly))
                    })
                    .unwrap();
            }
            return self.error_type;
        }
        if right.flags().intersects(
            TypeFlags::BooleanLike
                | TypeFlags::NumberLike
                | TypeFlags::BigIntLike
                | TypeFlags::StringLike
                | TypeFlags::EnumLike
                | TypeFlags::NonPrimitive
                | TypeFlags::Index,
        ) {
            return left;
        }
        if self.is_generic_object_type(left) || self.is_generic_object_type(right) {
            if self.is_empty_object_type(left) {
                return right;
            }
            // When the left type is an intersection, we may need to merge the last constituent of the
            // intersection with the right type. For example when the left type is 'T & { a: string }'
            // and the right type is '{ b: string }' we produce 'T & { a: string, b: string }'.
            if left.flags().intersects(TypeFlags::Intersection) {
                let types = left.types();
                let last_left = types[types.len() - 1];
                if self.is_non_generic_object_type(last_left)
                    && self.is_non_generic_object_type(right)
                {
                    let mut new_types = types.to_vec();
                    let n = new_types.len();
                    new_types[n - 1] =
                        self.get_spread_type(last_left, right, symbol, object_flags, readonly);
                    return self.get_intersection_type(&new_types);
                }
            }
            return self.get_intersection_type(&[left, right]);
        }
        let mut skipped_private_members: FxHashSet<&'static str> = FxHashSet::default();
        let index_infos = if left == self.empty_object_type {
            self.get_index_infos_of_type(right).to_vec()
        } else {
            self.get_union_index_infos(&[left, right])
        };
        let right_props = self.get_properties_of_type(right);
        // tsrs: sized for the larger side (the spread nearly always has that many members): spreading a large object
        // grew the table, and rebuilt its index, step by step. The left side counts only when already resolved, so
        // that nothing is resolved earlier than below.
        let left_len = if left.flags().intersects(TypeFlags::Object)
            && left.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
        {
            left.as_structured_type().properties().len()
        } else {
            0
        };
        let members = SymbolTable::with_capacity(right_props.len().max(left_len));
        for right_prop in right_props.iter().copied() {
            if get_declaration_modifier_flags_from_symbol(right_prop)
                .intersects(ModifierFlags::Private | ModifierFlags::Protected)
            {
                skipped_private_members.insert(right_prop.name());
            } else if Self::is_spreadable_property(right_prop) {
                let s = self.get_spread_symbol(right_prop, readonly);
                members.set(right_prop.name(), s);
            }
        }

        for left_prop in self.get_properties_of_type(left).iter().copied() {
            if skipped_private_members.contains(left_prop.name())
                || !Self::is_spreadable_property(left_prop)
            {
                continue;
            }
            if let Some(right_prop) = members.lookup(left_prop.name()) {
                let right_type = self.get_type_of_symbol(right_prop);
                if right_prop.flags().intersects(SymbolFlags::Optional) {
                    let mut declarations = left_prop.declarations().to_vec();
                    declarations.extend(right_prop.declarations().iter().copied());
                    let flags = SymbolFlags::Property | (left_prop.flags() & SymbolFlags::Optional);
                    let result = self.new_symbol(flags, left_prop.name());
                    let links = self.value_symbol_links.get(result);
                    // Optimization: avoid calculating the union type if spreading into the exact same type.
                    // This is common, e.g. spreading one options bag into another where the bags have the
                    // same type, or have properties which overlap. If the unions are large, it may turn out
                    // to be expensive to perform subtype reduction.
                    let left_type = self.get_type_of_symbol(left_prop);
                    let left_type_without_undefined =
                        self.remove_missing_or_undefined_type(left_type);
                    let right_type_without_undefined =
                        self.remove_missing_or_undefined_type(right_type);
                    if left_type_without_undefined == right_type_without_undefined {
                        links.resolved_type.set(Some(left_type));
                    } else {
                        let u = self.get_union_type_ex(
                            &[left_type, right_type_without_undefined],
                            UnionReduction::Subtype,
                            AliasArg::None,
                            None,
                        );
                        links.resolved_type.set(Some(u));
                    }
                    self.spread_links.get(result).left_spread.set(Some(left_prop));
                    self.spread_links.get(result).right_spread.set(Some(right_prop));
                    result.set_declarations(&declarations);
                    links.set_name_type(self.value_symbol_links.get(left_prop).name_type());
                    members.set(left_prop.name(), result);
                }
            } else {
                let s = self.get_spread_symbol(left_prop, readonly);
                members.set(left_prop.name(), s);
            }
        }
        let spread_index_infos: Vec<P<IndexInfo>> = index_infos
            .iter()
            .map(|&info| Self::get_index_info_with_readonly(info, readonly))
            .collect();
        let spread = self.new_anonymous_type(symbol, Some(members), &[], &[], &spread_index_infos);
        spread.object_flags.set(
            spread.object_flags()
                | ObjectFlags::ObjectLiteral
                | ObjectFlags::ContainsObjectOrArrayLiteral
                | ObjectFlags::ContainsSpread
                | object_flags,
        );
        spread
    }

    // checker.go:13715
    fn get_index_info_with_readonly(info: P<IndexInfo>, readonly: bool) -> P<IndexInfo> {
        if info.is_readonly.get() != readonly {
            return Self::new_index_info(
                info.key_type.get().unwrap(),
                info.value_type.get().unwrap(),
                readonly,
                info.declaration.get(),
                info.components.get(),
            );
        }
        info
    }

    // checker.go:13722
    pub(crate) fn is_valid_spread_type(&mut self, t: P<Type>) -> bool {
        let mapped = self.map_type(t, |c, t| Some(c.get_base_constraint_or_type(t))).unwrap();
        let s = self.remove_definitely_falsy_types(mapped);
        s.flags().intersects(
            TypeFlags::Any
                | TypeFlags::NonPrimitive
                | TypeFlags::Object
                | TypeFlags::InstantiableNonPrimitive,
        ) || s.flags().intersects(TypeFlags::UnionOrIntersection)
            && s.types().iter().all(|&t| self.is_valid_spread_type(t))
    }

    // checker.go:13728
    pub(crate) fn get_union_index_infos(&mut self, types: &[P<Type>]) -> Vec<P<IndexInfo>> {
        let source_infos = self.get_index_infos_of_type(types[0]);
        let mut result = Vec::new();
        result.extend(source_infos.iter().filter_map(|info| {
            let index_type = info.key_type.get().unwrap();
            if types.iter().all(|&t| self.get_index_info_of_type(t, index_type).is_some()) {
                let value_types: Vec<P<Type>> = types
                    .iter()
                    .map(|&t| self.get_index_type_of_type(t, index_type).unwrap())
                    .collect();
                let value_type = self.get_union_type(&value_types);
                let is_readonly = types.iter().any(|&t| {
                    self.get_index_info_of_type(t, index_type).unwrap().is_readonly.get()
                });
                Some(Self::new_index_info(index_type, value_type, is_readonly, None, &[]))
            } else {
                None
            }
        }));
        result
    }

    // checker.go:13744
    pub(crate) fn is_non_generic_object_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Object) && !self.is_generic_mapped_type(t)
    }

    // checker.go:13748
    fn try_merge_union_of_object_type_and_empty_object(
        &mut self,
        t: P<Type>,
        readonly: bool,
    ) -> P<Type> {
        if !t.flags().intersects(TypeFlags::Union) {
            return t;
        }
        if t.types().iter().all(|&t| self.is_empty_object_type_or_spreads_into_empty_object(t)) {
            let empty = t.types().iter().copied().find(|&t| self.is_empty_object_type(t));
            if let Some(empty) = empty {
                return empty;
            }
            return self.empty_object_type;
        }
        let first_type = t
            .types()
            .iter()
            .copied()
            .find(|&t| !self.is_empty_object_type_or_spreads_into_empty_object(t));
        let Some(first_type) = first_type else {
            return t;
        };
        let second_type = t.types().iter().copied().find(|&t| {
            t != first_type && !self.is_empty_object_type_or_spreads_into_empty_object(t)
        });
        if second_type.is_some() {
            return t;
        }
        // gets the type as if it had been spread, but where everything in the spread is made optional
        let members = SymbolTable::new();
        for prop in self.get_properties_of_type(first_type).iter().copied() {
            if get_declaration_modifier_flags_from_symbol(prop)
                .intersects(ModifierFlags::Private | ModifierFlags::Protected)
            {
                // do nothing, skip privates
            } else if Self::is_spreadable_property(prop) {
                let is_setonly_accessor = prop.flags().intersects(SymbolFlags::SetAccessor)
                    && !prop.flags().intersects(SymbolFlags::GetAccessor);
                let flags = SymbolFlags::Property | SymbolFlags::Optional;
                let result = self.new_symbol_ex(
                    flags,
                    prop.name(),
                    prop.check_flags() & CheckFlags::Late
                        | if readonly { CheckFlags::Readonly } else { CheckFlags::empty() },
                );
                let links = self.value_symbol_links.get(result);
                if is_setonly_accessor {
                    links.resolved_type.set(Some(self.undefined_type));
                } else {
                    let prop_type = self.get_type_of_symbol(prop);
                    links.resolved_type.set(Some(self.add_optionality_ex(
                        prop_type, true, /*isProperty*/
                        true, /*isOptional*/
                    )));
                }
                result.set_declarations_static(prop.declarations());
                links.set_name_type(self.value_symbol_links.get(prop).name_type());
                self.mapped_symbol_links.get(result).synthetic_origin.set(Some(prop));
                members.set(prop.name(), result);
            }
        }
        let index_infos = self.get_index_infos_of_type(first_type);
        let spread =
            self.new_anonymous_type(first_type.symbol(), Some(members), &[], &[], index_infos);
        spread.object_flags.set(
            spread.object_flags_lazy()
                | ObjectFlags::ObjectLiteral
                | ObjectFlags::ContainsObjectOrArrayLiteral,
        );
        spread
    }

    // We approximate own properties as non-methods plus methods that are inside the object literal
    // checker.go:13798
    pub(crate) fn is_spreadable_property(prop: P<Symbol>) -> bool {
        let declarations = prop.declarations();
        !declarations.iter().any(|&d| ast::is_private_identifier_class_element_declaration(d))
            && !prop.flags().intersects(
                SymbolFlags::Method | SymbolFlags::GetAccessor | SymbolFlags::SetAccessor,
            )
            || !declarations
                .iter()
                .any(|&d| d.parent().is_some() && ast::is_class_like(d.parent().unwrap()))
    }

    // checker.go:13803
    pub(crate) fn get_spread_symbol(&mut self, prop: P<Symbol>, readonly: bool) -> P<Symbol> {
        let is_setonly_accessor = prop.flags().intersects(SymbolFlags::SetAccessor)
            && !prop.flags().intersects(SymbolFlags::GetAccessor);
        if !is_setonly_accessor && readonly == self.is_readonly_symbol(prop) {
            return prop;
        }
        let flags = SymbolFlags::Property | (prop.flags() & SymbolFlags::Optional);
        let result = self.new_symbol_ex(
            flags,
            prop.name(),
            prop.check_flags() & CheckFlags::Late
                | if readonly { CheckFlags::Readonly } else { CheckFlags::empty() },
        );
        let links = self.value_symbol_links.get(result);
        if is_setonly_accessor {
            links.resolved_type.set(Some(self.undefined_type));
        } else {
            links.resolved_type.set(Some(self.get_type_of_symbol(prop)));
        }
        result.set_declarations_static(prop.declarations());
        links.set_name_type(self.value_symbol_links.get(prop).name_type());
        self.mapped_symbol_links.get(result).synthetic_origin.set(Some(prop));
        result
    }

    // checker.go:13822
    fn is_empty_object_type_or_spreads_into_empty_object(&mut self, t: P<Type>) -> bool {
        self.is_empty_object_type(t)
            || t.flags().intersects(
                TypeFlags::Null
                    | TypeFlags::Undefined
                    | TypeFlags::BooleanLike
                    | TypeFlags::NumberLike
                    | TypeFlags::BigIntLike
                    | TypeFlags::StringLike
                    | TypeFlags::EnumLike
                    | TypeFlags::NonPrimitive
                    | TypeFlags::Index,
            )
    }

    // checker.go:13826
    pub(crate) fn has_default_value(node: P<Node>) -> bool {
        ast::is_binding_element(node) && node.initializer().is_some()
            || ast::is_property_assignment(node)
                && Self::has_default_value(node.initializer().unwrap())
            || ast::is_shorthand_property_assignment(node)
                && node
                    .as_shorthand_property_assignment()
                    .object_assignment_initializer
                    .get()
                    .is_some()
            || ast::is_binary_expression(node)
                && node.as_binary_expression().operator_token.kind() == Kind::EqualsToken
    }

    // checker.go:13833
    pub(crate) fn is_const_context(&mut self, node: P<Node>) -> bool {
        self.is_const_context_ex(node).0
    }

    /// `is_const_context`, and `get_contextual_type(node, ContextFlags::None)` when it was computed on the way.
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn is_const_context_ex(&mut self, node: P<Node>) -> (bool, Option<Option<P<Type>>>) {
        let parent = node.parent().unwrap();
        if ast::is_const_assertion(parent) || Self::is_inline_import_attributes(node) {
            return (true, None);
        }
        let mut node_contextual_type = None;
        if self.is_valid_const_assertion_argument(node) {
            let contextual_type = self.get_contextual_type(node, ContextFlags::None);
            node_contextual_type = Some(contextual_type);
            if self.is_const_type_variable(contextual_type, 0) {
                return (true, node_contextual_type);
            }
        }
        let result = (ast::is_parenthesized_expression(parent)
            || ast::is_array_literal_expression(parent)
            || ast::is_spread_element(parent))
            && self.is_const_context(parent)
            || (ast::is_property_assignment(parent)
                || ast::is_shorthand_property_assignment(parent)
                || ast::is_template_span(parent))
                && self.is_const_context(parent.parent().unwrap());
        (result, node_contextual_type)
    }

    // checker.go:13842
    fn is_inline_import_attributes(node: P<Node>) -> bool {
        if !ast::is_object_literal_expression(node)
            || !ast::is_property_assignment(node.parent().unwrap())
            || node.parent().unwrap().initializer() != Some(node)
        {
            return false;
        }
        let property = node.parent().unwrap();
        let property_name = property.name().unwrap();
        if (!ast::is_identifier(property_name) && !ast::is_string_literal_like(property_name))
            || property_name.text() != "with"
        {
            return false;
        }
        let options = property.parent().unwrap();
        if !ast::is_object_literal_expression(options) {
            return false;
        }
        let import_call = ast::find_ancestor(options, ast::is_import_call);
        match import_call {
            Some(import_call) => {
                import_call.arguments().len() > 1
                    && ast::skip_parentheses(import_call.arguments()[1]) == options
            }
            None => false,
        }
    }

    // checker.go:13858
    pub(crate) fn is_valid_const_assertion_argument(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::ArrayLiteralExpression
            | Kind::ObjectLiteralExpression
            | Kind::TemplateExpression => {
                return true;
            }
            Kind::ParenthesizedExpression => {
                return self.is_valid_const_assertion_argument(node.expression().unwrap());
            }
            Kind::PrefixUnaryExpression => {
                let op = node.as_prefix_unary_expression().operator;
                let arg = node.as_prefix_unary_expression().operand;
                return op == Kind::MinusToken
                    && (arg.kind() == Kind::NumericLiteral || arg.kind() == Kind::BigIntLiteral)
                    || op == Kind::PlusToken && arg.kind() == Kind::NumericLiteral;
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                let expr = ast::skip_parentheses(node.expression().unwrap());
                let mut symbol: Option<P<Symbol>> = None;
                if ast::is_entity_name_expression(expr) {
                    symbol = self.resolve_entity_name(
                        expr,
                        SymbolFlags::Value,
                        true, /*ignoreErrors*/
                        false,
                        None,
                    );
                }
                return symbol.is_some_and(|symbol| symbol.flags().intersects(SymbolFlags::Enum));
            }
            _ => {}
        }
        false
    }

    // checker.go:13880
    pub(crate) fn is_const_type_variable(&mut self, t: Option<P<Type>>, depth: u32) -> bool {
        let Some(t) = t else {
            return false;
        };
        if depth >= 5 {
            return false;
        }
        // tsrs-only: every branch below that can answer true reaches a type variable, so a type already known not to
        // contain one (`couldContainTypeVariables` computed false; read only, never computed here) answers false.
        if t.object_flags()
            & (ObjectFlags::CouldContainTypeVariablesComputed
                | ObjectFlags::CouldContainTypeVariables)
            == ObjectFlags::CouldContainTypeVariablesComputed
        {
            return false;
        }
        if t.flags().intersects(TypeFlags::TypeParameter) {
            return t.symbol().is_some_and(|symbol| {
                symbol
                    .declarations()
                    .iter()
                    .any(|&d| ast::has_syntactic_modifier(d, ModifierFlags::Const))
            });
        } else if t.flags().intersects(TypeFlags::UnionOrIntersection) {
            // tsrs-only: a union of primitives (`PrimitiveUnion`: no object, intersection or instantiable constituent)
            // has no constituent any branch here answers true for; `isConstContext` asks this of large literal unions.
            if t.flags().intersects(TypeFlags::Union)
                && t.object_flags().intersects(ObjectFlags::PrimitiveUnion)
            {
                return false;
            }
            if t.types().iter().any(|&s| {
                Self::may_be_const_type_variable(s) && self.is_const_type_variable(Some(s), depth)
            }) {
                return true;
            }
            return false;
        } else if t.flags().intersects(TypeFlags::IndexedAccess) {
            return self
                .is_const_type_variable(t.as_indexed_access_type().object_type.get(), depth + 1);
        } else if t.flags().intersects(TypeFlags::Conditional) {
            let constraint = self.get_constraint_of_conditional_type(t);
            return self.is_const_type_variable(constraint, depth + 1);
        } else if t.flags().intersects(TypeFlags::Substitution) {
            return self.is_const_type_variable(t.as_substitution_type().base_type.get(), depth);
        } else if t.object_flags().intersects(ObjectFlags::Mapped) {
            let type_variable = self.get_homomorphic_type_variable(t);
            return type_variable.is_some() && self.is_const_type_variable(type_variable, depth);
        } else if Self::is_generic_tuple_type(t)
            && self.get_element_types(t).iter().copied().enumerate().any(|(i, s)| {
                t.target_tuple_type().element_infos.get()[i]
                    .flags
                    .intersects(ElementFlags::Variadic)
                    && self.is_const_type_variable(Some(s), depth)
            })
        {
            return true;
        }
        false
    }

    /// False for a type `is_const_type_variable` answers false for at every depth, tested without the call
    /// (a union's constituents: mostly object types and literals).
    #[inline]
    fn may_be_const_type_variable(t: P<Type>) -> bool {
        let flags = t.flags();
        if flags.intersects(
            TypeFlags::TypeParameter
                | TypeFlags::UnionOrIntersection
                | TypeFlags::IndexedAccess
                | TypeFlags::Conditional
                | TypeFlags::Substitution,
        ) {
            return true;
        }
        t.object_flags().intersects(ObjectFlags::Mapped) || Self::is_generic_tuple_type(t)
    }

    // checker.go:13908
    pub(crate) fn check_property_assignment(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        // Do not use hasDynamicName here, because that returns false for well known symbols.
        // We want to perform checkComputedPropertyName for all computed properties, including
        // well known symbols.
        if ast::is_computed_property_name(node.name().unwrap()) {
            self.check_computed_property_name(node.name().unwrap());
        }
        let initializer_type =
            self.check_expression_for_mutable_location(node.initializer().unwrap(), check_mode);
        if let Some(type_node) = node.type_node() {
            let t = self.get_type_from_type_node(type_node);
            self.check_type_assignable_to_and_optionally_elaborate(
                initializer_type,
                t,
                Some(node),
                Some(node.initializer().unwrap()),
                None, /*headMessage*/
                None,
            );
            return t;
        }
        initializer_type
    }

    // checker.go:13924
    pub(crate) fn check_shorthand_property_assignment(
        &mut self,
        node: P<Node>,
        in_destructuring_pattern: bool,
        check_mode: CheckMode,
    ) -> P<Type> {
        let mut expr: Option<P<Node>> = None;
        if !in_destructuring_pattern {
            expr = node.as_shorthand_property_assignment().object_assignment_initializer.get();
        }
        let expr = match expr {
            Some(expr) => expr,
            None => node.name().unwrap(),
        };
        let expression_type = self.check_expression_for_mutable_location(expr, check_mode);
        if let Some(type_node) = node.type_node() {
            let t = self.get_type_from_type_node(type_node);
            self.check_type_assignable_to_and_optionally_elaborate(
                expression_type,
                t,
                Some(node),
                Some(expr),
                None, /*headMessage*/
                None,
            );
            return t;
        }
        expression_type
    }

    // checker.go:13941
    pub(crate) fn is_in_property_initializer_or_class_static_block(
        node: P<Node>,
        ignore_arrow_functions: bool,
    ) -> bool {
        ast::find_ancestor_or_quit(node, |node| match node.kind() {
            Kind::PropertyDeclaration | Kind::ClassStaticBlockDeclaration => {
                FindAncestorResult::True
            }
            Kind::TypeQuery | Kind::JsxClosingElement => FindAncestorResult::Quit,
            Kind::ArrowFunction => {
                if ignore_arrow_functions {
                    FindAncestorResult::False
                } else {
                    FindAncestorResult::Quit
                }
            }
            Kind::Block => {
                if ast::is_function_like_declaration(node.parent())
                    && node.parent().unwrap().kind() != Kind::ArrowFunction
                {
                    FindAncestorResult::Quit
                } else {
                    FindAncestorResult::False
                }
            }
            _ => FindAncestorResult::False,
        })
        .is_some()
    }

    // checker.go:13958
    pub(crate) fn get_narrowed_type_of_symbol(
        &mut self,
        symbol: P<Symbol>,
        location: P<Node>,
    ) -> P<Type> {
        let mut t = self.get_type_of_symbol(symbol);
        let declaration = symbol.value_declaration();
        if let Some(declaration) = declaration {
            // If we have a non-rest binding element with no initializer declared as a const variable or a const-like
            // parameter (a parameter for which there are no assignments in the function body), and if the parent type
            // for the destructuring is a union type, one or more of the binding elements may represent discriminant
            // properties, and we want the effects of conditional checks on such discriminants to affect the types of
            // other binding elements from the same destructuring. Consider:
            //
            //   type Action =
            //       | { kind: 'A', payload: number }
            //       | { kind: 'B', payload: string };
            //
            //   function f({ kind, payload }: Action) {
            //       if (kind === 'A') {
            //           payload.toFixed();
            //       }
            //       if (kind === 'B') {
            //           payload.toUpperCase();
            //       }
            //   }
            //
            // Above, we want the conditional checks on 'kind' to affect the type of 'payload'. To facilitate this, we use
            // the binding pattern AST instance for '{ kind, payload }' as a pseudo-reference and narrow this reference
            // as if it occurred in the specified location. We then recompute the narrowed binding element type by
            // destructuring from the narrowed parent type.
            if ast::is_binding_element(declaration)
                && declaration.initializer().is_none()
                && !has_dot_dot_dot_token(declaration)
                && declaration.parent().unwrap().elements().len() >= 2
            {
                'case: {
                    let root_declaration = ast::get_root_declaration(declaration);
                    let root_initializer = root_declaration.initializer();
                    // Avoid declaration circularity without blocking binding defaults or nested callbacks.
                    if let Some(root_initializer) = root_initializer
                        && ast::is_node_descendant_of(location, root_initializer)
                        && Self::get_control_flow_container(declaration)
                            == Self::get_control_flow_container(location)
                    {
                        break 'case;
                    }
                    let parent = declaration.parent().unwrap().parent().unwrap();
                    if ast::is_variable_declaration(root_declaration)
                        && self
                            .get_combined_node_flags_cached(root_declaration)
                            .intersects(NodeFlags::Constant)
                        || ast::is_parameter_declaration(root_declaration)
                    {
                        let links = self.node_links.get(parent);
                        if !links.flags.get().intersects(NodeCheckFlags::InCheckIdentifier) {
                            links.flags.set(links.flags.get() | NodeCheckFlags::InCheckIdentifier);
                            let parent_type =
                                self.get_type_for_binding_element_parent(parent, CheckMode::Normal);
                            let mut parent_type_constraint: Option<P<Type>> = None;
                            if let Some(parent_type) = parent_type {
                                parent_type_constraint = self.map_type(parent_type, |c, t| {
                                    Some(c.get_base_constraint_or_type(t))
                                });
                            }
                            // Guard parent-type resolution only; flow analysis should allow re-entrant narrowing
                            links.flags.set(links.flags.get() & !NodeCheckFlags::InCheckIdentifier);
                            if let Some(parent_type_constraint) = parent_type_constraint
                                && parent_type_constraint.flags().intersects(TypeFlags::Union)
                                && !(ast::is_parameter_declaration(root_declaration)
                                    && self.is_some_symbol_assigned(root_declaration))
                            {
                                let pattern = declaration.parent().unwrap();
                                let narrowed_type = self.get_flow_type_of_reference_ex(
                                    pattern,
                                    parent_type_constraint,
                                    parent_type_constraint,
                                    None, /*flowContainer*/
                                    get_flow_node_of_node(location),
                                );
                                if narrowed_type.flags().intersects(TypeFlags::Never) {
                                    t = self.never_type;
                                } else {
                                    // Destructurings are validated against the parent type elsewhere. Here we disable tuple bounds
                                    // checks because the narrowed type may have lower arity than the full parent type. For example,
                                    // for the declaration [x, y]: [1, 2] | [3], we may have narrowed the parent type to just [3].
                                    t = self.get_binding_element_type_from_parent_type(
                                        declaration,
                                        narrowed_type,
                                        true, /*noTupleBoundsCheck*/
                                    );
                                }
                            }
                        }
                    }
                }
            } else if ast::is_parameter_declaration(declaration)
                && declaration.type_node().is_none()
                && declaration.initializer().is_none()
                && !has_dot_dot_dot_token(declaration)
            {
                // If we have a const-like parameter with no type annotation or initializer, and if the parameter is contextually
                // typed by a signature with a single rest parameter of a union of tuple types, one or more of the parameters may
                // represent discriminant tuple elements, and we want the effects of conditional checks on such discriminants to
                // affect the types of other parameters in the same parameter list. Consider:
                //
                //   type Action = [kind: 'A', payload: number] | [kind: 'B', payload: string];
                //
                //   const f: (...args: Action) => void = (kind, payload) => {
                //       if (kind === 'A') {
                //           payload.toFixed();
                //       }
                //       if (kind === 'B') {
                //           payload.toUpperCase();
                //       }
                //   }
                //
                // Above, we want the conditional checks on 'kind' to affect the type of 'payload'. To facilitate this, we use
                // the arrow function AST node for '(kind, payload) => ...' as a pseudo-reference and narrow this reference as
                // if it occurred in the specified location. We then recompute the narrowed parameter type by indexing into the
                // narrowed tuple type.
                let fn_ = declaration.parent().unwrap();
                if fn_.parameters().len() >= 2
                    && self.is_context_sensitive_function_or_object_literal_method(fn_)
                {
                    let contextual_signature = self.get_contextual_signature(fn_);
                    if let Some(contextual_signature) = contextual_signature
                        && contextual_signature.parameters.get().len() == 1
                        && signature_has_rest_parameter(contextual_signature)
                    {
                        let mut mapper: Option<P<TypeMapper>> = None;
                        let context = self.get_inference_context(fn_);
                        if let Some(context) = context {
                            mapper = context.non_fixing_mapper();
                        }
                        let rest_param_type =
                            self.get_type_of_symbol(contextual_signature.parameters.get()[0]);
                        let instantiated = self.instantiate_type(rest_param_type, mapper);
                        let rest_type = self.get_reduced_apparent_type(instantiated);
                        if rest_type.flags().intersects(TypeFlags::Union)
                            && every_type(self, rest_type, |_, t| is_tuple_type(t))
                            && !fn_.parameters().iter().any(|&p| self.is_some_symbol_assigned(p))
                        {
                            let narrowed_type = self.get_flow_type_of_reference_ex(
                                fn_,
                                rest_type,
                                rest_type,
                                None, /*flowContainer*/
                                get_flow_node_of_node(location),
                            );
                            let index = fn_
                                .parameters()
                                .iter()
                                .position(|&p| p == declaration)
                                .expect("parameter belongs to its declaration")
                                - usize::from(ast::get_this_parameter(fn_).is_some());
                            let index_type = self.get_number_literal_type(Number(index as f64));
                            t = self.get_indexed_access_type(narrowed_type, index_type);
                        }
                    }
                }
            }
        }
        t
    }

    // checker.go:14064
    fn is_readonly_assignment_declaration(&mut self, node: P<Node>) -> bool {
        if !ast::is_call_expression(node) {
            return false;
        }
        let property_descriptor_type = self.check_expression_cached(node.arguments()[2]);
        if self.get_type_of_property_of_type(property_descriptor_type, "value").is_some() {
            if let Some(writable_prop) =
                self.get_property_of_type(property_descriptor_type, "writable")
            {
                let writable_type = if writable_prop.value_declaration().is_some()
                    && ast::is_property_assignment(writable_prop.value_declaration().unwrap())
                {
                    self.check_expression(
                        writable_prop.value_declaration().unwrap().initializer().unwrap(),
                    )
                } else {
                    self.get_type_of_symbol(writable_prop)
                };
                return writable_type.flags().intersects(TypeFlags::BooleanLiteral)
                    && !get_boolean_literal_value(writable_type);
            }
            return true;
        }
        self.get_type_of_property_of_type(property_descriptor_type, "set").is_none()
    }

    // checker.go:14084
    pub(crate) fn is_readonly_symbol(&mut self, symbol: P<Symbol>) -> bool {
        // The following symbols are considered read-only:
        // Properties with a 'readonly' modifier
        // Variables declared with 'const'
        // Get accessors without matching set accessors
        // Enum members
        // Object.defineProperty assignments with writable false or no setter
        // Unions and intersections of the above (unions and intersections eagerly set isReadonly on creation)
        self.is_readonly_symbol_with_modifiers(symbol, || {
            get_declaration_modifier_flags_from_symbol(symbol)
        })
    }

    /// `is_readonly_symbol` for a caller that has `get_declaration_modifier_flags_from_symbol(symbol)` at hand.
    pub(crate) fn is_readonly_symbol_with_modifiers(
        &mut self,
        symbol: P<Symbol>,
        modifiers: impl FnOnce() -> ModifierFlags,
    ) -> bool {
        symbol.check_flags().intersects(CheckFlags::Readonly)
            || symbol.flags().intersects(SymbolFlags::Property)
                && modifiers().intersects(ModifierFlags::Readonly)
            || symbol.flags().intersects(SymbolFlags::Variable)
                && self
                    .get_declaration_node_flags_from_symbol(symbol)
                    .intersects(NodeFlags::Constant)
            || symbol.flags().intersects(SymbolFlags::Accessor)
                && !symbol.flags().intersects(SymbolFlags::SetAccessor)
            || symbol.flags().intersects(SymbolFlags::EnumMember)
            || {
                let declarations = symbol.declarations();
                declarations.iter().any(|&d| self.is_readonly_assignment_declaration(d))
            }
    }

    // checker.go:14100
    pub(crate) fn check_object_literal_method(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        // Grammar checking
        self.check_grammar_method(node);
        // Do not use hasDynamicName here, because that returns false for well known symbols.
        // We want to perform checkComputedPropertyName for all computed properties, including
        // well known symbols.
        if ast::is_computed_property_name(node.name().unwrap()) {
            self.check_computed_property_name(node.name().unwrap());
        }
        let uninstantiated_type =
            self.check_function_expression_or_object_literal_method(node, check_mode);
        self.instantiate_type_with_single_generic_call_signature(
            node,
            uninstantiated_type,
            check_mode,
        )
    }

    // checker.go:14113
    pub(crate) fn check_expression_for_mutable_location(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let t = self.check_expression_ex(node, check_mode);
        let (is_const_context, node_contextual_type) = self.is_const_context_ex(node);
        if is_const_context {
            self.get_regular_type_of_literal_type(t)
        } else if crate::is_type_assertion(node) {
            t
        } else {
            // tsrs-only: the contextual type isConstContext computed, if it did; nothing between changes it.
            let contextual_type = match node_contextual_type {
                Some(contextual_type) => contextual_type,
                None => self.get_contextual_type(node, ContextFlags::None),
            };
            let instantiated =
                self.instantiate_contextual_type(contextual_type, node, ContextFlags::None);
            self.get_widened_literal_like_type_for_contextual_type(t, instantiated)
        }
    }

    // checker.go:14125
    pub(crate) fn get_resolved_symbol(&mut self, node: P<Node>) -> P<Symbol> {
        let links = self.symbol_node_links.get(node);
        if links.resolved_symbol.get().is_none() {
            let mut symbol: Option<P<Symbol>> = None;
            if !ast::node_is_missing(node) {
                let message = self.get_cannot_find_name_diagnostic_for_name(node);
                symbol = self.resolve_name(
                    Some(node),
                    node.text(),
                    SymbolFlags::Value | SymbolFlags::ExportValue,
                    Some(message),
                    !ast::is_write_only_access(node),
                    false, /*excludeGlobals*/
                );
            }
            links.resolved_symbol.set(Some(symbol.unwrap_or(self.unknown_symbol)));
        }
        links.resolved_symbol.get().unwrap()
    }

    // checker.go:14138
    pub(crate) fn get_resolved_symbol_or_nil(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        self.symbol_node_links.get(node).resolved_symbol.get()
    }

    // checker.go:14142
    pub(crate) fn get_referenced_value_or_alias_symbol(
        &mut self,
        reference: P<Node>,
    ) -> Option<P<Symbol>> {
        let resolved_symbol = self.symbol_node_links.get(reference).resolved_symbol.get();
        if let Some(resolved_symbol) = resolved_symbol
            && resolved_symbol != self.unknown_symbol
        {
            return Some(resolved_symbol);
        }
        self.resolve_name(
            Some(reference),
            reference.text(),
            SymbolFlags::Value | SymbolFlags::ExportValue | SymbolFlags::Alias,
            None,
            false, /*isUse*/
            false, /*excludeGlobals*/
        )
    }

    // checker.go:14150
    pub(crate) fn get_cannot_find_name_diagnostic_for_name(
        &self,
        node: P<Node>,
    ) -> &'static Message {
        match node.text() {
            "document" | "console" => &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_INCLUDE_DOM,
            "$" => {
                if self.compiler_options.uses_wildcard_types() {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_J_QUERY_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJQUERY
                } else {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_J_QUERY_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJQUERY_AND_THEN_ADD_JQUERY_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG
                }
            }
            "beforeEach" | "describe" | "suite" | "it" | "test" => {
                if self.compiler_options.uses_wildcard_types() {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_A_TEST_RUNNER_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJEST_OR_NPM_I_SAVE_DEV_TYPES_SLASHMOCHA
                } else {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_A_TEST_RUNNER_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJEST_OR_NPM_I_SAVE_DEV_TYPES_SLASHMOCHA_AND_THEN_ADD_JEST_OR_MOCHA_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG
                }
            }
            "process" | "require" | "Buffer" | "module" | "NodeJS" => {
                if self.compiler_options.uses_wildcard_types() {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE
                } else {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE_AND_THEN_ADD_NODE_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG
                }
            }
            "Bun" => {
                if self.compiler_options.uses_wildcard_types() {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_BUN_TRY_NPM_I_SAVE_DEV_TYPES_SLASHBUN
                } else {
                    &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_BUN_TRY_NPM_I_SAVE_DEV_TYPES_SLASHBUN_AND_THEN_ADD_BUN_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG
                }
            }
            "Map" | "Set" | "Promise" | "ast.Symbol" | "WeakMap" | "WeakSet" | "Iterator" | "AsyncIterator" | "SharedArrayBuffer" | "Atomics" | "AsyncIterable"
            | "AsyncIterableIterator" | "AsyncGenerator" | "AsyncGeneratorFunction" | "BigInt" | "Reflect" | "BigInt64Array" | "BigUint64Array" => {
                &diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_1_OR_LATER
            }
            text => {
                if text == "await" && ast::is_call_expression(node.parent().unwrap()) {
                    return &diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_TO_WRITE_THIS_IN_AN_ASYNC_FUNCTION;
                }
                // fallthrough
                if node.parent().unwrap().kind() == Kind::ShorthandPropertyAssignment {
                    return &diagnostics::NO_VALUE_EXISTS_IN_SCOPE_FOR_THE_SHORTHAND_PROPERTY_0_EITHER_DECLARE_ONE_OR_PROVIDE_AN_INITIALIZER;
                }
                &diagnostics::CANNOT_FIND_NAME_0
            }
        }
    }

    // checker.go:14186
    pub fn get_diagnostics_exported(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        self.get_diagnostics(ctx, source_file, false)
    }

    // checker.go:14190
    pub fn get_suggestion_diagnostics(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        self.get_diagnostics(ctx, source_file, true)
    }

    // checker.go:14194
    // Go passes `&c.diagnostics` / `&c.suggestionDiagnostics`; the collections are plain Checker fields, so the
    // Rust port selects the collection with `suggestions` (true = `c.suggestionDiagnostics`).
    fn get_diagnostics(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
        suggestions: bool,
    ) -> Vec<P<Diagnostic>> {
        self.check_not_canceled();
        let check_unused = self.compiler_options.no_unused_locals.is_true()
            || self.compiler_options.no_unused_parameters.is_true()
            || suggestions;
        self.check_source_file(ctx, source_file, check_unused);
        if self.was_canceled {
            return Vec::new();
        }
        if suggestions {
            self.suggestion_diagnostics.get_diagnostics_for_file(source_file)
        } else {
            self.diagnostics.get_diagnostics_for_file(source_file)
        }
    }

    // checker.go:14204
    pub fn get_global_diagnostics(&mut self) -> Vec<P<Diagnostic>> {
        self.check_not_canceled();
        self.produce_deferred_diagnostics();
        self.diagnostics.get_global_diagnostics()
    }

    // checker.go:14210
    // the callback is stored in `deferred_diagnostic_callbacks: Vec<Box<dyn FnOnce(&mut Checker)>>`, so it must be
    // `FnOnce + 'static`.
    pub(crate) fn add_deferred_diagnostic(
        &mut self,
        callback: impl FnOnce(&mut Checker) + 'static,
    ) {
        self.deferred_diagnostic_callbacks.push(Box::new(callback));
    }

    // checker.go:14214
    pub(crate) fn produce_deferred_diagnostics(&mut self) {
        // Go ranges over the slice value it had on entry, then resets the field to nil (dropping any callback
        // appended while running).
        let callbacks = std::mem::take(&mut self.deferred_diagnostic_callbacks);
        for cb in callbacks {
            cb(self);
        }
        self.deferred_diagnostic_callbacks = Vec::new();
    }

    // checker.go:14221
    pub(crate) fn add_diagnostic(&mut self, diagnostic: P<Diagnostic>) -> P<Diagnostic> {
        self.diagnostic_adds = self.diagnostic_adds.wrapping_add(1);
        // Discard diagnostics created while at the maximum number of recursive TypeToString invocations.
        if self.serialization_level < MAX_SERIALIZATION_LEVEL {
            return self.diagnostics.add(diagnostic);
        }
        diagnostic
    }

    // checker.go:14229
    pub(crate) fn add_suggestion_diagnostic(&mut self, diagnostic: P<Diagnostic>) -> P<Diagnostic> {
        self.diagnostic_adds = self.diagnostic_adds.wrapping_add(1);
        // Discard diagnostics created while at the maximum number of recursive TypeToString invocations.
        if self.serialization_level < MAX_SERIALIZATION_LEVEL {
            return self.suggestion_diagnostics.add(diagnostic);
        }
        diagnostic
    }

    // checker.go:14237
    pub(crate) fn error(
        &mut self,
        location: Option<P<Node>>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> P<Diagnostic> {
        self.add_diagnostic(new_diagnostic_for_node(location, Some(message), args))
    }

    // checker.go:14241
    pub(crate) fn error_skipped_on_no_emit(
        &mut self,
        location: P<Node>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> P<Diagnostic> {
        let diagnostic = self.error(Some(location), message, args);
        diagnostic.set_skipped_on_no_emit();
        diagnostic
    }

    // checker.go:14247
    pub(crate) fn error_or_suggestion(
        &mut self,
        is_error: bool,
        location: Option<P<Node>>,
        message: &'static Message,
        args: &[&dyn Display],
    ) {
        self.add_error_or_suggestion(
            is_error,
            new_diagnostic_for_node(location, Some(message), args),
        );
    }

    // checker.go:14251
    pub(crate) fn error_and_maybe_suggest_await(
        &mut self,
        location: Option<P<Node>>,
        maybe_missing_await: bool,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> P<Diagnostic> {
        let diagnostic = self.error(location, message, args);
        if maybe_missing_await {
            diagnostic.add_related_info(create_diagnostic_for_node(
                location,
                &diagnostics::DID_YOU_FORGET_TO_USE_AWAIT,
                &[],
            ));
        }
        diagnostic
    }

    // checker.go:14259
    pub(crate) fn add_error_or_suggestion(&mut self, is_error: bool, diagnostic: P<Diagnostic>) {
        if is_error {
            self.add_diagnostic(diagnostic);
        } else {
            let suggestion = diagnostic.clone_diagnostic();
            suggestion.set_category(diagnostics::Category::Suggestion);
            self.add_suggestion_diagnostic(suggestion);
        }
    }

    // checker.go:14269
    pub fn is_deprecated_declaration(&mut self, declaration: P<Node>) -> bool {
        let flags = self.get_combined_node_flags_cached(declaration);
        ast::is_deprecated_declaration_with_cached_flags(declaration, flags)
    }

    // checker.go:14273
    pub(crate) fn add_deprecated_suggestion(
        &mut self,
        location: P<Node>,
        declarations: &[P<Node>],
        deprecated_entity: &str,
    ) -> P<Diagnostic> {
        let diagnostic = new_diagnostic_for_node(
            Some(location),
            Some(&diagnostics::X_0_IS_DEPRECATED),
            &[&deprecated_entity],
        );
        self.add_deprecated_suggestion_worker(declarations, diagnostic)
    }

    // checker.go:14278
    pub(crate) fn add_deprecated_suggestion_worker(
        &mut self,
        declarations: &[P<Node>],
        diagnostic: P<Diagnostic>,
    ) -> P<Diagnostic> {
        for &declaration in declarations {
            let deprecated_tag = ast::get_jsdoc_deprecated_tag(declaration);
            if let Some(deprecated_tag) = deprecated_tag {
                diagnostic.add_related_info(new_diagnostic_for_node(
                    Some(deprecated_tag),
                    Some(&diagnostics::THE_DECLARATION_WAS_MARKED_AS_DEPRECATED_HERE),
                    &[],
                ));
                break;
            }
        }
        self.add_suggestion_diagnostic(diagnostic)
    }

    // checker.go:14289
    pub(crate) fn is_deprecated_symbol(&mut self, symbol: P<Symbol>) -> bool {
        let parent_symbol = self.get_parent_of_symbol(symbol);
        let declarations = symbol.declarations();
        if let Some(parent_symbol) = parent_symbol
            && declarations.len() > 1
        {
            if parent_symbol.flags().intersects(SymbolFlags::Interface) {
                return declarations.iter().any(|&d| self.is_deprecated_declaration(d));
            }
            return declarations.iter().all(|&d| self.is_deprecated_declaration(d));
        }
        symbol.value_declaration().is_some()
            && self.is_deprecated_declaration(symbol.value_declaration().unwrap())
            || !declarations.is_empty()
                && declarations.iter().all(|&d| self.is_deprecated_declaration(d))
    }

    // checker.go:14301
    pub(crate) fn has_parse_diagnostics(source_file: P<SourceFile>) -> bool {
        !source_file.diagnostics().is_empty()
    }

    // checker.go:14305
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_symbol(&mut self, flags: SymbolFlags, name: &'static str) -> P<Symbol> {
        self.symbol_count += 1;
        crate::sitecount::hit("symbol", "");
        let s = Symbol::new(flags | SymbolFlags::Transient, name);
        if self.seed_mode {
            // Shared graph: a fork must never write an id into a frozen symbol.
            ast::get_symbol_id(s);
        }
        s
    }

    // checker.go:14348
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_shared_data_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        self.symbol_count += 1;
        crate::sitecount::hit("symbol", "");
        let result = Symbol::new_shared_data(symbol);
        if self.seed_mode {
            ast::get_symbol_id(result);
        }
        result
    }

    // checker.go:14313
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_symbol_ex(
        &mut self,
        flags: SymbolFlags,
        name: &'static str,
        check_flags: CheckFlags,
    ) -> P<Symbol> {
        let result = self.new_symbol(flags, name);
        result.set_check_flags(check_flags);
        result
    }

    // checker.go:14319
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_parameter(&mut self, name: &'static str, t: P<Type>) -> P<Symbol> {
        let symbol = self.new_symbol(SymbolFlags::FunctionScopedVariable, name);
        self.value_symbol_links.get(symbol).resolved_type.set(Some(t));
        symbol
    }

    // checker.go:14325
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_property(&mut self, name: &'static str, t: P<Type>) -> P<Symbol> {
        let symbol = self.new_symbol(SymbolFlags::Property, name);
        self.value_symbol_links.get(symbol).resolved_type.set(Some(t));
        symbol
    }

    // checker.go:14331
    pub(crate) fn combine_symbol_tables(
        &mut self,
        first: Option<P<SymbolTable>>,
        second: Option<P<SymbolTable>>,
    ) -> Option<P<SymbolTable>> {
        if first.is_none_or(|t| t.is_empty()) {
            return second;
        }
        if second.is_none_or(|t| t.is_empty()) {
            return first;
        }
        let combined = SymbolTable::new();
        self.merge_symbol_table(combined, first, false, None);
        self.merge_symbol_table(combined, second, false, None);
        Some(combined)
    }

    // checker.go:14344
    pub(crate) fn merge_symbol_table(
        &mut self,
        target: P<SymbolTable>,
        source: Option<P<SymbolTable>>,
        unidirectional: bool,
        merged_parent: Option<P<Symbol>>,
    ) {
        let Some(source) = source else {
            return;
        };
        for (id, source_symbol) in source.entries() {
            let target_symbol = target.lookup(id);
            let merged = if let Some(target_symbol) = target_symbol {
                self.merge_symbol(target_symbol, source_symbol, unidirectional)
            } else {
                self.get_merged_symbol(source_symbol)
            };
            if merged_parent.is_some() && target_symbol.is_some() {
                // If a merge was performed on the target symbol, set its parent to the merged parent that initiated the merge
                // of its exports. Otherwise, `merged` came only from `sourceSymbol` and can keep its parent:
                //
                // // a.ts
                // export interface A { x: number; }
                //
                // // b.ts
                // declare module "./a" {
                //   interface A { y: number; }
                //   interface B {}
                // }
                //
                // When merging the module augmentation into a.ts, the symbol for `A` will itself be merged, so its parent
                // should be the merged module symbol. But the symbol for `B` has only one declaration, so its parent should
                // be the module augmentation symbol, which contains its only declaration.
                if merged.flags().intersects(SymbolFlags::Transient) {
                    merged.set_parent(merged_parent);
                }
            }
            target.set(id, merged);
        }
    }

    /**
     * Note: if target is transient, then it is mutable, and mergeSymbol with both mutate and return it.
     * If target is not transient, mergeSymbol will produce a transient clone, mutate that and return it.
     */
    // checker.go:14381
    pub(crate) fn merge_symbol(
        &mut self,
        target: P<Symbol>,
        source: P<Symbol>,
        unidirectional: bool,
    ) -> P<Symbol> {
        let mut target = target;
        if !target.flags().intersects(get_excluded_symbol_flags(source.flags()))
            || (source.flags() | target.flags()).intersects(SymbolFlags::Assignment)
        {
            if source == target {
                // This can happen when an export assigned namespace exports something also erroneously exported at the top level
                // See `declarationFileNoCrashOnExtraExportModifier` for an example
                return target;
            }
            if !target.flags().intersects(SymbolFlags::Transient) {
                let resolved_target = self.resolve_symbol(target);
                if resolved_target == self.unknown_symbol {
                    return source;
                }
                if !resolved_target.flags().intersects(get_excluded_symbol_flags(source.flags()))
                    || (source.flags() | resolved_target.flags())
                        .intersects(SymbolFlags::Assignment)
                {
                    target = self.clone_symbol(resolved_target);
                } else {
                    self.report_merge_symbol_error(target, source);
                    return source;
                }
            }
            // Javascript static-property-assignment declarations always merge, even though they are also values
            if source.flags().intersects(SymbolFlags::ValueModule)
                && target.flags().intersects(SymbolFlags::ValueModule)
                && target.flags().intersects(SymbolFlags::ConstEnumOnlyModule)
                && !source.flags().intersects(SymbolFlags::ConstEnumOnlyModule)
            {
                // reset flag when merging instantiated module into value module that has only const enums
                target.flags.set(target.flags() & !SymbolFlags::ConstEnumOnlyModule);
            }
            let mut source_flags = source.flags();
            if !target.flags().intersects(SymbolFlags::ConstEnumOnlyModule) {
                source_flags &= !SymbolFlags::ConstEnumOnlyModule;
            }
            target.flags.set(target.flags() | source_flags);
            if let Some(value_declaration) = source.value_declaration() {
                tsrs_binder::set_value_declaration(target, value_declaration);
            }
            let source_declarations = source.declarations();
            target.append_declarations(source_declarations);
            if source.members().is_some() {
                let target_members = get_members(target);
                self.merge_symbol_table(target_members, source.members(), unidirectional, None);
            }
            if source.exports().is_some() {
                let target_exports = get_exports(target);
                self.merge_symbol_table(
                    target_exports,
                    source.exports(),
                    unidirectional,
                    Some(target),
                );
            }
            if !unidirectional {
                self.record_merged_symbol(target, source);
            }
        } else if target.flags().intersects(SymbolFlags::NamespaceModule) {
            // Do not report an error when merging `var globalThis` with the built-in `globalThis`,
            // as we will already report a "Declaration name conflicts..." error, and this error
            // won't make much sense.
            if target != self.global_this_symbol {
                let s = self.symbol_to_string(target);
                self.error(
                    ast::get_name_of_declaration(get_first_declaration(source)),
                    &diagnostics::CANNOT_AUGMENT_MODULE_0_WITH_VALUE_EXPORTS_BECAUSE_IT_RESOLVES_TO_A_NON_MODULE_ENTITY,
                    &[&s],
                );
            }
        } else {
            self.report_merge_symbol_error(target, source);
        }
        target
    }

    // checker.go:14436
    fn report_merge_symbol_error(&mut self, target: P<Symbol>, source: P<Symbol>) {
        let is_either_enum = target.flags().intersects(SymbolFlags::Enum)
            || source.flags().intersects(SymbolFlags::Enum);
        let is_either_block_scoped = target.flags().intersects(SymbolFlags::BlockScopedVariable)
            || source.flags().intersects(SymbolFlags::BlockScopedVariable);
        let message: &'static Message = if is_either_enum {
            &diagnostics::ENUM_DECLARATIONS_CAN_ONLY_MERGE_WITH_NAMESPACE_OR_OTHER_ENUM_DECLARATIONS
        } else if is_either_block_scoped {
            &diagnostics::CANNOT_REDECLARE_BLOCK_SCOPED_VARIABLE_0
        } else {
            &diagnostics::DUPLICATE_IDENTIFIER_0
        };
        let source_symbol_file = ast::get_source_file_of_node(get_first_declaration(source));
        let target_symbol_file = ast::get_source_file_of_node(get_first_declaration(target));
        let is_source_plain_js =
            ast::is_plain_js_file(source_symbol_file, self.compiler_options.check_js);
        let is_target_plain_js =
            ast::is_plain_js_file(target_symbol_file, self.compiler_options.check_js);
        let symbol_name = self.symbol_to_string(source);
        if !is_source_plain_js {
            self.add_duplicate_declaration_errors_for_symbols(
                source,
                message,
                &symbol_name,
                target,
            );
        }
        if !is_target_plain_js {
            self.add_duplicate_declaration_errors_for_symbols(
                target,
                message,
                &symbol_name,
                source,
            );
        }
    }

    // checker.go:14461
    fn add_duplicate_declaration_errors_for_symbols(
        &mut self,
        target: P<Symbol>,
        message: &'static Message,
        symbol_name: &str,
        source: P<Symbol>,
    ) {
        let target_declarations = target.declarations();
        for &node in target_declarations {
            let source_declarations = source.declarations();
            self.add_duplicate_declaration_error(node, message, symbol_name, source_declarations);
        }
    }

    // checker.go:14467
    fn add_duplicate_declaration_error(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        symbol_name: &str,
        related_nodes: &[P<Node>],
    ) {
        let error_node = get_adjusted_node_for_error(node);
        let err = self.lookup_or_issue_error(error_node, message, &[&symbol_name]);
        for &related_node in related_nodes {
            let adjusted_node = get_adjusted_node_for_error(related_node);
            if adjusted_node == error_node {
                continue;
            }
            let leading_message = create_diagnostic_for_node(
                Some(adjusted_node),
                &diagnostics::X_0_WAS_ALSO_DECLARED_HERE,
                &[&symbol_name],
            );
            let follow_on_message =
                create_diagnostic_for_node(Some(adjusted_node), &diagnostics::X_AND_HERE, &[]);
            if err.related_information().len() >= 5
                || err.related_information().iter().any(|&d| {
                    ast::compare_diagnostics(d, follow_on_message) == std::cmp::Ordering::Equal
                        || ast::compare_diagnostics(d, leading_message) == std::cmp::Ordering::Equal
                })
            {
                continue;
            }
            if err.related_information().is_empty() {
                err.add_related_info(leading_message);
            } else {
                err.add_related_info(follow_on_message);
            }
        }
    }
}

// checker.go:14493
pub(crate) fn create_diagnostic_for_node(
    node: Option<P<Node>>,
    message: &'static Message,
    args: &[&dyn Display],
) -> P<Diagnostic> {
    new_diagnostic_for_node(node, Some(message), args)
}

// checker.go:14497
fn get_adjusted_node_for_error(node: P<Node>) -> P<Node> {
    let name = ast::get_name_of_declaration(node);
    if let Some(name) = name {
        return name;
    }
    node
}

impl Checker {
    // checker.go:14505
    fn lookup_or_issue_error(
        &mut self,
        location: P<Node>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> P<Diagnostic> {
        self.add_diagnostic(new_diagnostic_for_node(Some(location), Some(message), args))
    }
}

// checker.go:14509
fn get_first_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
    symbol.declarations().first().copied()
}

// checker.go:14516
pub(crate) fn get_excluded_symbol_flags(flags: SymbolFlags) -> SymbolFlags {
    let mut result = SymbolFlags::empty();
    if flags.intersects(SymbolFlags::BlockScopedVariable) {
        result |= SymbolFlags::BlockScopedVariableExcludes;
    }
    if flags.intersects(SymbolFlags::FunctionScopedVariable) {
        result |= SymbolFlags::FunctionScopedVariableExcludes;
    }
    if flags.intersects(SymbolFlags::Property) {
        result |= SymbolFlags::PropertyExcludes;
    }
    if flags.intersects(SymbolFlags::EnumMember) {
        result |= SymbolFlags::EnumMemberExcludes;
    }
    if flags.intersects(SymbolFlags::Function) {
        result |= SymbolFlags::FunctionExcludes;
    }
    if flags.intersects(SymbolFlags::Class) {
        result |= SymbolFlags::ClassExcludes;
    }
    if flags.intersects(SymbolFlags::Interface) {
        result |= SymbolFlags::InterfaceExcludes;
    }
    if flags.intersects(SymbolFlags::RegularEnum) {
        result |= SymbolFlags::RegularEnumExcludes;
    }
    if flags.intersects(SymbolFlags::ConstEnum) {
        result |= SymbolFlags::ConstEnumExcludes;
    }
    if flags.intersects(SymbolFlags::ValueModule) {
        result |= SymbolFlags::ValueModuleExcludes;
    }
    if flags.intersects(SymbolFlags::Method) {
        result |= SymbolFlags::MethodExcludes;
    }
    if flags.intersects(SymbolFlags::GetAccessor) {
        result |= SymbolFlags::GetAccessorExcludes;
    }
    if flags.intersects(SymbolFlags::SetAccessor) {
        result |= SymbolFlags::SetAccessorExcludes;
    }
    if flags.intersects(SymbolFlags::TypeParameter) {
        result |= SymbolFlags::TypeParameterExcludes;
    }
    if flags.intersects(SymbolFlags::TypeAlias) {
        result |= SymbolFlags::TypeAliasExcludes;
    }
    if flags.intersects(SymbolFlags::Alias) {
        result |= SymbolFlags::AliasExcludes;
    }
    if flags.intersects(SymbolFlags::ReplaceableByMethod) {
        result &= !SymbolFlags::Method;
    }
    result
}

// A Go `ast.SymbolTable` field lookup, where a nil map yields nil.
fn lookup_export(symbol: P<Symbol>, name: &str) -> Option<P<Symbol>> {
    symbol.exports()?.lookup(name)
}

impl Checker {
    // checker.go:14572
    pub(crate) fn clone_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        let result = self.new_symbol(symbol.flags(), symbol.name());
        // Force reallocation if anything is ever appended to declarations
        result.set_declarations_static(symbol.declarations());
        result.set_parent(symbol.parent());
        result.set_value_declaration(symbol.value_declaration());
        result.set_members(symbol.members().map(|m| m.clone_table()));
        result.set_exports(symbol.exports().map(|e| e.clone_table()));
        self.record_merged_symbol(result, symbol);
        result
    }

    // checker.go:14584
    pub fn get_merged_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        if let Some(&merged) = self.merged_symbols.get(&symbol) {
            return merged;
        }
        symbol
    }

    // checker.go:14594
    pub(crate) fn get_parent_of_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        if let Some(parent) = symbol.parent() {
            let late_bound = self.get_late_bound_symbol(parent);
            return Some(self.get_merged_symbol(late_bound));
        }
        None
    }

    // checker.go:14601
    fn record_merged_symbol(&mut self, target: P<Symbol>, source: P<Symbol>) {
        self.merged_symbols.insert(source, target);
    }

    // checker.go:14605
    pub(crate) fn get_symbol_if_same_reference(
        &mut self,
        s1: P<Symbol>,
        s2: P<Symbol>,
    ) -> Option<P<Symbol>> {
        let m1 = self.get_merged_symbol(s1);
        let r1 = self.resolve_symbol(m1);
        let a = self.get_merged_symbol(r1);
        let m2 = self.get_merged_symbol(s2);
        let r2 = self.resolve_symbol(m2);
        let b = self.get_merged_symbol(r2);
        if a == b {
            return Some(s1);
        }
        None
    }

    // checker.go:14612
    // returns Option (Go returns nil for a nil symbol; callers test the result against nil).
    pub(crate) fn get_export_symbol_of_value_symbol_if_exported(
        &mut self,
        symbol: Option<P<Symbol>>,
    ) -> Option<P<Symbol>> {
        let mut symbol = symbol;
        if let Some(s) = symbol
            && s.flags().intersects(SymbolFlags::ExportValue)
            && s.export_symbol().is_some()
        {
            symbol = s.export_symbol();
        }
        symbol.map(|s| self.get_merged_symbol(s))
    }

    // checker.go:14619
    pub(crate) fn get_symbol_of_declaration(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let symbol = node.symbol();
        if let Some(symbol) = symbol {
            let late_bound = self.get_late_bound_symbol(symbol);
            return Some(self.get_merged_symbol(late_bound));
        }
        None
    }

    // Get the merged symbol for a node. If you know the node is a `Declaration`, it is more type safe to
    // use use `getSymbolOfDeclaration` instead.
    // checker.go:14629
    pub(crate) fn get_symbol_of_node(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let data = node.declaration_data();
        if let Some(data) = data
            && let Some(symbol) = data.symbol.get()
        {
            let late_bound = self.get_late_bound_symbol(symbol);
            return Some(self.get_merged_symbol(late_bound));
        }
        None
    }

    // checker.go:14637
    #[inline]
    pub(crate) fn get_late_bound_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        if !symbol.flags().intersects(SymbolFlags::ClassMember)
            || symbol.name() != INTERNAL_SYMBOL_NAME_COMPUTED
        {
            return symbol;
        }
        self.get_late_bound_symbol_worker(symbol)
    }

    #[inline(never)]
    fn get_late_bound_symbol_worker(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        let links = self.late_bound_links.get(symbol);
        if links.late_symbol.get().is_none() && {
            let declarations = symbol.declarations();
            declarations.iter().any(|&d| self.has_late_bindable_name(d))
        } {
            // force late binding of members/exports. This will set the late-bound symbol
            let parent = self.get_merged_symbol(symbol.parent().unwrap());
            let has_static = symbol.declarations().iter().any(|&d| ast::has_static_modifier(d));
            if has_static {
                self.get_exports_of_symbol(parent);
            } else {
                self.get_members_of_symbol(parent);
            }
        }
        if links.late_symbol.get().is_none() {
            links.late_symbol.set(Some(symbol));
        }
        links.late_symbol.get().unwrap()
    }

    // checker.go:14657
    pub(crate) fn resolve_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        self.resolve_symbol_ex(symbol, false /*dontResolveAlias*/)
    }

    // checker.go:14661
    pub(crate) fn resolve_symbol_ex(
        &mut self,
        symbol: P<Symbol>,
        dont_resolve_alias: bool,
    ) -> P<Symbol> {
        if !dont_resolve_alias
            && ast::is_non_local_alias(
                symbol,
                SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            )
        {
            return self.resolve_alias(symbol);
        }
        symbol
    }

    // checker.go:14668
    pub(crate) fn get_target_of_import_equals_declaration(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Symbol>> {
        // Node is ImportEqualsDeclaration | VariableDeclaration
        if ast::is_variable_declaration(node)
            || node.as_import_equals_declaration().module_reference.kind()
                == Kind::ExternalModuleReference
        {
            let mut module_reference = get_external_module_require_argument(node);
            if module_reference.is_none() {
                module_reference =
                    ast::get_external_module_import_equals_declaration_expression(node);
            }
            let immediate = self.resolve_external_module_name(
                node,
                module_reference.unwrap(),
                false, /*ignoreErrors*/
                None,  /*importAttributesType*/
            );
            let resolved = immediate.map(|immediate| {
                self.resolve_external_module_symbol(immediate, true /*dontResolveAlias*/)
            });
            if let Some(resolved) = resolved
                && ModuleKind::NODE20 <= self.module_kind
                && self.module_kind <= ModuleKind::NODE_NEXT
            {
                let module_exports = self.get_export_of_module(
                    resolved,
                    INTERNAL_SYMBOL_NAME_MODULE_EXPORTS,
                    node,
                    true, /*dontResolveAlias*/
                );
                if module_exports.is_some() {
                    return module_exports;
                }
            }
            self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
            return resolved;
        }
        let resolved = self.get_symbol_of_part_of_right_hand_side_of_import_equals(
            node.as_import_equals_declaration().module_reference,
        );
        self.check_and_report_error_for_resolving_import_alias_to_type_only_symbol(node, resolved);
        resolved
    }

    // checker.go:14691
    pub(crate) fn resolve_external_module_type_by_literal(&mut self, name: P<Node>) -> P<Type> {
        let module_sym = self.resolve_external_module_name(
            name, name, false, /*ignoreErrors*/
            None,  /*importAttributesType*/
        );
        if let Some(module_sym) = module_sym {
            let resolved_module_symbol = Some(
                self.resolve_external_module_symbol(module_sym, false /*dontResolveAlias*/),
            );
            if let Some(resolved_module_symbol) = resolved_module_symbol {
                return self.get_type_of_symbol(resolved_module_symbol);
            }
        }
        self.any_type
    }

    // This function is only for imports with entity names
    // checker.go:14703
    pub(crate) fn get_symbol_of_part_of_right_hand_side_of_import_equals(
        &mut self,
        entity_name: P<Node>,
    ) -> Option<P<Symbol>> {
        // There are three things we might try to look for. In the following examples,
        // the search term is enclosed in |...|:
        //
        //     import a = |b|; // Namespace
        //     import a = |b.c|; // Value, type, namespace
        //     import a = |b.c|.d; // Namespace
        let mut entity_name = entity_name;
        if entity_name.kind() == Kind::Identifier
            && ast::is_right_side_of_qualified_name_or_property_access(entity_name)
        {
            entity_name = entity_name.parent().unwrap(); // QualifiedName
        }
        // Check for case 1 and 3 in the above example
        if entity_name.kind() == Kind::Identifier
            || entity_name.parent().unwrap().kind() == Kind::QualifiedName
        {
            return self.resolve_entity_name(
                entity_name,
                SymbolFlags::Namespace,
                false, /*ignoreErrors*/
                true,  /*dontResolveAlias*/
                None,  /*location*/
            );
        }
        // Case 2 in above example
        // entityName.kind could be a QualifiedName or a Missing identifier
        assert_eq!(entity_name.parent().unwrap().kind(), Kind::ImportEqualsDeclaration);
        self.resolve_entity_name(
            entity_name,
            SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            false, /*ignoreErrors*/
            true,  /*dontResolveAlias*/
            None,  /*location*/
        )
    }

    // checker.go:14723
    // `resolved` is Option (Go passes the possibly-nil result of getSymbolOfPartOfRightHandSideOfImportEquals).
    // It is unused, as in Go.
    fn check_and_report_error_for_resolving_import_alias_to_type_only_symbol(
        &mut self,
        node: P<Node>,
        _resolved: Option<P<Symbol>>,
    ) {
        let decl = node.as_import_equals_declaration();
        let mut name = decl.module_reference;
        loop {
            if let Some(type_only_declaration) = self.get_type_only_declaration_of_entity_name(name)
            {
                let is_export = ast::node_kind_is(
                    type_only_declaration,
                    &[Kind::ExportSpecifier, Kind::ExportDeclaration],
                );
                let message: &'static Message = if is_export {
                    &diagnostics::AN_IMPORT_ALIAS_CANNOT_REFERENCE_A_DECLARATION_THAT_WAS_EXPORTED_USING_EXPORT_TYPE
                } else {
                    &diagnostics::AN_IMPORT_ALIAS_CANNOT_REFERENCE_A_DECLARATION_THAT_WAS_IMPORTED_USING_IMPORT_TYPE
                };
                let related_message: &'static Message = if is_export {
                    &diagnostics::X_0_WAS_EXPORTED_HERE
                } else {
                    &diagnostics::X_0_WAS_IMPORTED_HERE
                };
                // TODO: how to get name for export *?
                let mut name = "*";
                if !ast::is_export_declaration(type_only_declaration) {
                    name = type_only_declaration.name().unwrap().text();
                }
                self.error(Some(decl.module_reference), message, &[]).add_related_info(
                    create_diagnostic_for_node(
                        Some(type_only_declaration),
                        related_message,
                        &[&name],
                    ),
                );
                break;
            }
            if ast::is_identifier(name) {
                break;
            }
            name = name.as_qualified_name().left;
        }
    }

    // checker.go:14750
    fn get_type_only_declaration_of_entity_name(&mut self, name: P<Node>) -> Option<P<Node>> {
        if let Some(symbol) = self.resolve_entity_name(
            name,
            SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            true, /*ignoreErrors*/
            true, /*dontResolveAlias*/
            None, /*location*/
        ) {
            return self.get_type_only_alias_declaration(symbol);
        }
        None
    }

    // checker.go:14757
    pub(crate) fn get_target_of_import_clause(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        if node.as_import_clause().phase_modifier.get() == Kind::SourceKeyword {
            let alias = self.get_symbol_of_declaration(node)?;
            let links = self.alias_symbol_links.get(alias);
            if links.immediate_target.get().is_none() {
                let symbol = self
                    .new_symbol(SymbolFlags::FunctionScopedVariable, node.name().unwrap().text());
                symbol.set_declarations_static(alias.declarations());
                let source_type = self.get_global_abstract_module_source_type();
                self.value_symbol_links.get(symbol).resolved_type.set(Some(source_type));
                links.immediate_target.set(Some(symbol));
            }
            return links.immediate_target.get();
        }
        let module_specifier = get_module_specifier_from_node(node.parent().unwrap()).unwrap();
        let import_attributes_type = self
            .get_type_from_import_attributes(ast::get_import_attributes(node.parent().unwrap()));
        let module_symbol = self.resolve_external_module_name(
            node,
            module_specifier,
            false, /*ignoreErrors*/
            import_attributes_type,
        );
        if let Some(module_symbol) = module_symbol {
            return self.get_target_of_module_default(
                module_symbol,
                node,
                true, /*dontResolveAlias*/
            );
        }
        None
    }

    // checker.go:14765
    // returns Option (Go returns the possibly-nil exportDefaultSymbol).
    fn get_target_of_module_default(
        &mut self,
        module_symbol: P<Symbol>,
        node: P<Node>,
        dont_resolve_alias: bool,
    ) -> Option<P<Symbol>> {
        let file = module_symbol.declarations().iter().copied().find(|&d| ast::is_source_file(d));
        let specifier = Self::get_module_specifier_for_import_or_export(node);

        let mut export_module_dot_exports_symbol: Option<P<Symbol>> = None;
        if is_shorthand_ambient_module_symbol(module_symbol) {
            // !!! exportDefaultSymbol = moduleSymbol
            // Does nothing?
        } else if file.is_some()
            && specifier.is_some()
            && ModuleKind::NODE20 <= self.module_kind
            && self.module_kind <= ModuleKind::NODE_NEXT
            && self.get_emit_syntax_for_module_specifier_expression(specifier.unwrap())
                == ModuleKind::COMMON_JS
            && self.program.get_implied_node_format_for_emit(file.unwrap().as_source_file_p())
                == ModuleKind::ES_NEXT
        {
            export_module_dot_exports_symbol = self.resolve_export_by_name(
                module_symbol,
                INTERNAL_SYMBOL_NAME_MODULE_EXPORTS,
                Some(node),
                dont_resolve_alias,
            );
        }
        let export_default_symbol: Option<P<Symbol>> = if export_module_dot_exports_symbol.is_some()
        {
            // We have a transpiled default import where the `require` resolves to an ES module with a `module.exports` named
            // export. With `esModuleInterop` (always enabled), this will work:
            //
            // const dep_1 = __importDefault(require("./dep.mjs")); // wraps like { default: require("./dep.mjs") }
            // dep_1.default; // require("./dep.mjs") -> the `module.exports` export value
            self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
            return export_module_dot_exports_symbol;
        } else {
            self.resolve_export_by_name(
                module_symbol,
                INTERNAL_SYMBOL_NAME_DEFAULT,
                Some(node),
                dont_resolve_alias,
            )
        };
        let Some(specifier) = specifier else {
            return export_default_symbol;
        };
        // node is ImportClause | ImportSpecifier | ExportSpecifier
        let mut attributes: Option<P<Node>> = None;
        if ast::is_import_clause(node) {
            attributes = ast::get_import_attributes(node.parent().unwrap());
        } else if ast::is_import_specifier(node) {
            attributes = ast::get_import_attributes(
                node.parent().unwrap().parent().unwrap().parent().unwrap(),
            );
        } else if ast::is_export_specifier(node) {
            attributes = ast::get_import_attributes(node.parent().unwrap().parent().unwrap());
        }
        let attributes_type = self.get_type_from_import_attributes(attributes);
        let has_default_only =
            self.is_only_importable_as_default(specifier, Some(module_symbol), attributes_type);
        let has_synthetic_default =
            self.can_have_synthetic_default(file, module_symbol, dont_resolve_alias, specifier);
        if export_default_symbol.is_none() && !has_synthetic_default && !has_default_only {
            if ast::is_import_clause(node) {
                self.report_non_default_export(module_symbol, node);
            } else {
                let name = if ast::is_import_or_export_specifier(node) {
                    node.property_name_or_name()
                } else {
                    node.name()
                };
                self.error_no_module_member_symbol(
                    module_symbol,
                    module_symbol,
                    node,
                    name.unwrap(),
                );
            }
        } else if has_synthetic_default || has_default_only {
            // per emit behavior, a synthetic default overrides a "real" .default member if `__esModule` is not present
            let mut resolved =
                Some(self.resolve_external_module_symbol(module_symbol, dont_resolve_alias));
            if resolved.is_none() {
                resolved = Some(self.resolve_symbol_ex(module_symbol, dont_resolve_alias));
            }
            self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
            return resolved;
        }
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        export_default_symbol
    }

    // checker.go:14829
    fn report_non_default_export(&mut self, module_symbol: P<Symbol>, node: P<Node>) {
        if module_symbol.exports().is_some()
            && lookup_export(module_symbol, node.symbol().unwrap().name()).is_some()
        {
            let a = self.symbol_to_string(module_symbol);
            let b = self.symbol_to_string(node.symbol().unwrap());
            self.error(Some(node), &diagnostics::MODULE_0_HAS_NO_DEFAULT_EXPORT_DID_YOU_MEAN_TO_USE_IMPORT_1_FROM_0_INSTEAD, &[&a, &b]);
        } else {
            let a = self.symbol_to_string(module_symbol);
            let diagnostic =
                self.error(node.name(), &diagnostics::MODULE_0_HAS_NO_DEFAULT_EXPORT, &[&a]);
            let mut export_star: Option<P<Symbol>> = None;
            if module_symbol.exports().is_some() {
                export_star = lookup_export(module_symbol, INTERNAL_SYMBOL_NAME_EXPORT_STAR);
            }
            if let Some(export_star) = export_star {
                let declarations = export_star.declarations();
                let mut default_export: Option<P<Node>> = None;
                for &decl in declarations {
                    if !(ast::is_export_declaration(decl) && decl.module_specifier().is_some()) {
                        continue;
                    }
                    let attributes_type =
                        self.get_type_from_import_attributes(ast::get_import_attributes(decl));
                    let resolved_external_module_name = self.resolve_external_module_name(
                        decl,
                        decl.module_specifier().unwrap(),
                        false, /*ignoreErrors*/
                        attributes_type,
                    );
                    if resolved_external_module_name
                        .is_some_and(|m| lookup_export(m, INTERNAL_SYMBOL_NAME_DEFAULT).is_some())
                    {
                        default_export = Some(decl);
                        break;
                    }
                }
                if let Some(default_export) = default_export {
                    diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(default_export),
                        &diagnostics::X_EXPORT_ASTERISK_DOES_NOT_RE_EXPORT_A_DEFAULT,
                        &[],
                    ));
                }
            }
        }
    }

    // checker.go:14853
    // returns Option (Go returns nil when the export does not exist).
    fn resolve_export_by_name(
        &mut self,
        module_symbol: P<Symbol>,
        name: &str,
        source_node: Option<P<Node>>,
        dont_resolve_alias: bool,
    ) -> Option<P<Symbol>> {
        let export_value = lookup_export(module_symbol, INTERNAL_SYMBOL_NAME_EXPORT_EQUALS);
        let export_symbol = if let Some(export_value) = export_value {
            let t = self.get_type_of_symbol(export_value);
            self.get_property_of_type_ex(
                t, name, true,  /*skipObjectFunctionPropertyAugment*/
                false, /*includeTypeOnlyMembers*/
            )
        } else {
            lookup_export(module_symbol, name)
        };
        let resolved = export_symbol.map(|s| self.resolve_symbol_ex(s, dont_resolve_alias));
        self.mark_symbol_of_alias_declaration_if_type_only(source_node, None);
        resolved
    }

    // checker.go:14866
    // returns Option (Go returns nil when the module cannot be resolved).
    pub(crate) fn get_target_of_namespace_import(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let module_specifier = Self::get_module_specifier_for_import_or_export(node).unwrap();
        let attributes_type = self.get_type_from_import_attributes(ast::get_import_attributes(
            node.parent().unwrap().parent().unwrap(),
        ));
        let immediate = self.resolve_external_module_name(
            node,
            module_specifier,
            false, /*ignoreErrors*/
            attributes_type,
        );
        let resolved = immediate
            .map(|immediate| self.resolve_es_module_symbol(immediate, node, module_specifier));
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        resolved
    }

    // checker.go:14874
    pub(crate) fn get_target_of_namespace_export(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let module_specifier = Self::get_module_specifier_for_import_or_export(node);
        if let Some(module_specifier) = module_specifier {
            let attributes_type = self.get_type_from_import_attributes(ast::get_import_attributes(
                node.parent().unwrap(),
            ));
            let immediate = self.resolve_external_module_name(
                node,
                module_specifier,
                false, /*ignoreErrors*/
                attributes_type,
            );
            let resolved = immediate
                .map(|immediate| self.resolve_es_module_symbol(immediate, node, module_specifier));
            self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
            return resolved;
        }
        None
    }

    // checker.go:14885
    pub(crate) fn get_target_of_import_specifier(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let name = node.property_name_or_name().unwrap();
        if ast::is_import_specifier(node) && ast::module_export_name_is_default(name) {
            let specifier = Self::get_module_specifier_for_import_or_export(node);
            if let Some(specifier) = specifier {
                let attributes_type =
                    self.get_type_from_import_attributes(ast::get_import_attributes(
                        node.parent().unwrap().parent().unwrap().parent().unwrap(),
                    ));
                let module_symbol = self.resolve_external_module_name(
                    node,
                    specifier,
                    false, /*ignoreErrors*/
                    attributes_type,
                );
                if let Some(module_symbol) = module_symbol {
                    return self.get_target_of_module_default(
                        module_symbol,
                        node,
                        true, /*dontResolveAlias*/
                    );
                }
            }
        }
        let mut root = node.parent().and_then(|p| p.parent()).and_then(|p| p.parent()); // ImportDeclaration
        if ast::is_binding_element(node) {
            root = Some(ast::get_root_declaration(node));
        }
        let resolved =
            self.get_external_module_member(root.unwrap(), node, true /*dontResolveAlias*/);
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        resolved
    }

    // checker.go:14905
    pub(crate) fn get_external_module_member(
        &mut self,
        node: P<Node>,
        specifier: P<Node>,
        dont_resolve_alias: bool,
    ) -> Option<P<Symbol>> {
        // node is ImportDeclaration | ExportDeclaration | VariableDeclaration
        // specifier is ImportSpecifier | ExportSpecifier | BindingElement | PropertyAccessExpression
        let mut module_specifier = get_external_module_require_argument(node);
        if module_specifier.is_none() {
            module_specifier = ast::get_external_module_name(node);
        }
        let module_specifier = module_specifier.unwrap();
        let mut attributes: Option<P<Node>> = None;
        if ast::has_import_attributes(node) {
            attributes = ast::get_import_attributes(node);
        }
        let import_attributes_type = self.get_type_from_import_attributes(attributes);
        let module_symbol = self.resolve_external_module_name(
            node,
            module_specifier,
            false, /*ignoreErrors*/
            import_attributes_type,
        );
        let name = if ast::is_property_access_expression(specifier) {
            specifier.name()
        } else {
            specifier.property_name_or_name()
        };
        let name = name.unwrap();
        if !ast::is_identifier(name) && !ast::is_string_literal(name) {
            return None;
        }
        let name_text = name.text();
        let target_symbol =
            module_symbol.map(|m| self.resolve_es_module_symbol(m, specifier, module_specifier));
        if let Some(target_symbol) = target_symbol {
            // Note: The empty string is a valid module export name:
            //
            //   import { "" as foo } from "./foo";
            //   export { foo as "" };
            //
            if !name_text.is_empty() || name.kind() == Kind::StringLiteral {
                let module_symbol_ = module_symbol.unwrap();
                if is_shorthand_ambient_module_symbol(module_symbol_) {
                    return module_symbol;
                }
                // First check if module was specified with "export=". If so, get the member from the resolved type
                let mut symbol_from_variable = if module_symbol.is_some()
                    && lookup_export(module_symbol_, INTERNAL_SYMBOL_NAME_EXPORT_EQUALS).is_some()
                {
                    let t = self.get_type_of_symbol(target_symbol);
                    self.get_property_of_type_ex(
                        t, name_text, true,  /*skipObjectFunctionPropertyAugment*/
                        false, /*includeTypeOnlyMembers*/
                    )
                } else {
                    self.get_property_of_variable(target_symbol, name_text)
                };
                // if symbolFromVariable is export - get its final target
                symbol_from_variable =
                    symbol_from_variable.map(|s| self.resolve_symbol_ex(s, dont_resolve_alias));
                let mut export_container = target_symbol;
                if module_symbol.is_some()
                    && lookup_export(module_symbol_, INTERNAL_SYMBOL_NAME_EXPORT_EQUALS).is_some()
                {
                    // For `export =` modules, supplemental type/namespace exports live on the original module symbol.
                    export_container = module_symbol_;
                }
                let mut symbol_from_module = self.get_export_of_module(
                    export_container,
                    name_text,
                    specifier,
                    dont_resolve_alias,
                );
                if symbol_from_module.is_none() && name_text == INTERNAL_SYMBOL_NAME_DEFAULT {
                    let file = module_symbol_
                        .declarations()
                        .iter()
                        .copied()
                        .find(|&d| ast::is_source_file(d));
                    if self.is_only_importable_as_default(
                        module_specifier,
                        module_symbol,
                        import_attributes_type,
                    ) || self.can_have_synthetic_default(
                        file,
                        module_symbol_,
                        dont_resolve_alias,
                        module_specifier,
                    ) {
                        symbol_from_module = Some(
                            self.resolve_external_module_symbol(module_symbol_, dont_resolve_alias),
                        );
                        if symbol_from_module.is_none() {
                            symbol_from_module =
                                Some(self.resolve_symbol_ex(module_symbol_, dont_resolve_alias));
                        }
                    }
                }
                let mut symbol = symbol_from_variable;
                if let Some(symbol_from_module) = symbol_from_module {
                    symbol = Some(symbol_from_module);
                    if let Some(symbol_from_variable) = symbol_from_variable {
                        symbol = Some(self.combine_value_and_type_symbols(
                            symbol_from_variable,
                            symbol_from_module,
                        ));
                    }
                }
                if ast::is_import_or_export_specifier(specifier)
                    && self.is_only_importable_as_default(
                        module_specifier,
                        module_symbol,
                        import_attributes_type,
                    )
                    && name_text != INTERNAL_SYMBOL_NAME_DEFAULT
                {
                    let module_kind = self.module_kind.to_string();
                    self.error(Some(name), &diagnostics::NAMED_IMPORTS_FROM_A_JSON_FILE_INTO_AN_ECMA_SCRIPT_MODULE_ARE_NOT_ALLOWED_WHEN_MODULE_IS_SET_TO_0, &[&module_kind]);
                } else if symbol.is_none() {
                    self.error_no_module_member_symbol(module_symbol_, target_symbol, node, name);
                }
                return symbol;
            }
        }
        None
    }

    // checker.go:14981
    fn get_property_of_variable(&mut self, symbol: P<Symbol>, name: &str) -> Option<P<Symbol>> {
        if symbol.flags().intersects(SymbolFlags::Variable) {
            let type_annotation = symbol.value_declaration().unwrap().type_node();
            if let Some(type_annotation) = type_annotation {
                let t = self.get_type_from_type_node(type_annotation);
                let prop = self.get_property_of_type(t, name);
                return prop.map(|p| self.resolve_symbol(p));
            }
        }
        None
    }

    // This function creates a synthetic symbol that combines the value side of one symbol with the
    // type/namespace side of another symbol. Consider this example:
    //
    //	declare module graphics {
    //	    interface Point {
    //	        x: number;
    //	        y: number;
    //	    }
    //	}
    //	declare var graphics: {
    //	    Point: new (x: number, y: number) => graphics.Point;
    //	}
    //	declare module "graphics" {
    //	    export = graphics;
    //	}
    //
    // An 'import { Point } from "graphics"' needs to create a symbol that combines the value side 'Point'
    // property with the type/namespace side interface 'Point'.
    // checker.go:15009
    fn combine_value_and_type_symbols(
        &mut self,
        value_symbol: P<Symbol>,
        type_symbol: P<Symbol>,
    ) -> P<Symbol> {
        if value_symbol == self.unknown_symbol && type_symbol == self.unknown_symbol {
            return self.unknown_symbol;
        }
        if type_symbol.flags().intersects(SymbolFlags::Value) {
            return type_symbol;
        }
        if value_symbol.flags().intersects(SymbolFlags::Type | SymbolFlags::Namespace) {
            return value_symbol;
        }
        let result =
            self.new_symbol(value_symbol.flags() | type_symbol.flags(), value_symbol.name());
        assert!(!value_symbol.declarations().is_empty() || !type_symbol.declarations().is_empty());
        let mut declarations = value_symbol.declarations().to_vec();
        declarations.extend(type_symbol.declarations().iter().copied());
        declarations.dedup();
        result.set_declarations(&declarations);
        result.set_parent(value_symbol.parent());
        if result.parent().is_none() {
            result.set_parent(type_symbol.parent());
        }
        result.set_value_declaration(value_symbol.value_declaration());
        result.set_members(type_symbol.members().map(|m| m.clone_table()));
        result.set_exports(value_symbol.exports().map(|e| e.clone_table()));
        result
    }

    // checker.go:15032
    pub(crate) fn get_export_of_module(
        &mut self,
        symbol: P<Symbol>,
        name_text: &str,
        specifier: P<Node>,
        dont_resolve_alias: bool,
    ) -> Option<P<Symbol>> {
        if symbol.flags().intersects(SymbolFlags::Module) {
            let export_symbol =
                self.get_exports_of_symbol(symbol).and_then(|exports| exports.lookup(name_text));
            let resolved = export_symbol.map(|s| self.resolve_symbol_ex(s, dont_resolve_alias));
            let export_star_declaration =
                self.module_symbol_links.get(symbol).type_only_export_star_map.get(name_text);
            self.mark_symbol_of_alias_declaration_if_type_only(
                Some(specifier),
                export_star_declaration,
            );
            return resolved;
        }
        None
    }

    // checker.go:15043
    pub(crate) fn is_only_importable_as_default(
        &mut self,
        usage: P<Node>,
        resolved_module: Option<P<Symbol>>,
        import_attributes_type: Option<P<Type>>,
    ) -> bool {
        // In Node.js, JSON modules don't get named exports
        if ModuleKind::NODE16 <= self.module_kind && self.module_kind <= ModuleKind::NODE_NEXT {
            let usage_mode = self.get_emit_syntax_for_module_specifier_expression(usage);
            if usage_mode == ModuleKind::ES_NEXT {
                let mut resolved_module = resolved_module;
                if resolved_module.is_none() {
                    resolved_module = self.resolve_external_module_name(
                        usage,
                        usage,
                        true, /*ignoreErrors*/
                        import_attributes_type,
                    );
                }
                let mut target_file: Option<P<SourceFile>> = None;
                if let Some(resolved_module) = resolved_module {
                    target_file = ast::get_source_file_of_module(resolved_module);
                }
                return target_file.is_some_and(|target_file| {
                    ast::is_json_source_file(target_file)
                        || tspath::get_declaration_file_extension(target_file.file_name())
                            == ".d.json.ts"
                });
            }
        }
        false
    }

    // checker.go:15061
    pub(crate) fn can_have_synthetic_default(
        &mut self,
        file: Option<P<Node>>,
        module_symbol: P<Symbol>,
        dont_resolve_alias: bool,
        usage: P<Node>,
    ) -> bool {
        let mut usage_mode = ModuleKind::NONE;
        if file.is_some() {
            usage_mode = self.get_emit_syntax_for_module_specifier_expression(usage);
        }
        if let Some(file) = file
            && usage_mode != ModuleKind::NONE
        {
            let target_mode =
                self.program.get_implied_node_format_for_emit(file.as_source_file_p());
            if usage_mode == ModuleKind::ES_NEXT
                && target_mode == ModuleKind::COMMON_JS
                && ModuleKind::NODE16 <= self.module_kind
                && self.module_kind <= ModuleKind::NODE_NEXT
            {
                // In Node.js, CommonJS modules always have a synthetic default when imported into ESM
                return true;
            }
            if usage_mode == ModuleKind::ES_NEXT && target_mode == ModuleKind::ES_NEXT {
                // No matter what the `module` setting is, if we're confident that both files
                // are ESM, there cannot be a synthetic default.
                return false;
            }
            // For other files (not node16/nodenext with impliedNodeFormat), check if we can determine
            // the module format from project references
            if target_mode == ModuleKind::NONE && file.as_source_file().is_declaration_file() {
                // Try to get the project reference - try both source file mapping and output file mapping
                // since declaration files can be mapped either way depending on how they're resolved
                if self.program.get_redirect_for_resolution(file.as_source_file_p()).is_some()
                    || self
                        .program
                        .get_project_reference_from_output_dts(file.as_source_file().path())
                        .is_some()
                {
                    // This is a declaration file from a project reference, so we can determine
                    // its module format from the referenced project's options
                    let target_module_kind =
                        self.emit_module_format_of_file(file.as_source_file_p());
                    if usage_mode == ModuleKind::ES_NEXT
                        && ModuleKind::ES2015 <= target_module_kind
                        && target_module_kind <= ModuleKind::ES_NEXT
                    {
                        return false;
                    }
                }
            }
        }
        // Declaration files (and ambient modules)
        if file.is_none() || file.unwrap().as_source_file().is_declaration_file() {
            // Definitely cannot have a synthetic default if they have a syntactic default member specified
            let default_export_symbol = self.resolve_export_by_name(
                module_symbol,
                INTERNAL_SYMBOL_NAME_DEFAULT,
                None, /*sourceNode*/
                true, /*dontResolveAlias*/
            ); // Dont resolve alias because we want the immediately exported symbol's declaration
            if default_export_symbol
                .is_some_and(|s| s.declarations().iter().any(|&d| is_syntactic_default(d)))
            {
                return false;
            }
            // It _might_ still be incorrect to assume there is no __esModule marker on the import at runtime, even if there is no `default` member
            // So we check a bit more,
            if self
                .resolve_export_by_name(
                    module_symbol,
                    "__esModule",
                    None, /*sourceNode*/
                    dont_resolve_alias,
                )
                .is_some()
            {
                // If there is an `__esModule` specified in the declaration (meaning someone explicitly added it or wrote it in their code),
                // it definitely is a module and does not have a synthetic default
                return false;
            }
            // There are _many_ declaration files not written with esmodules in mind that still get compiled into a format with __esModule set
            // Meaning there may be no default at runtime - however to be on the permissive side, we allow access to a synthetic default member
            // as there is no marker to indicate if the accompanying JS has `__esModule` or not, or is even native esm
            return true;
        }
        let file = file.unwrap();
        // TypeScript files never have a synthetic default (as they are always emitted with an __esModule marker) _unless_ they contain an export= statement
        if !ast::is_in_js_file(file) {
            return has_export_assignment_symbol(module_symbol);
        }

        // JS files have a synthetic default if they do not contain ES2015+ module syntax (export = is not valid in js) _and_ do not have an __esModule marker
        let external_module_indicator = file.as_source_file().external_module_indicator();
        (external_module_indicator.is_none() || external_module_indicator == Some(file))
            && self
                .resolve_export_by_name(
                    module_symbol,
                    "__esModule",
                    None, /*sourceNode*/
                    dont_resolve_alias,
                )
                .is_none()
    }

    // checker.go:15120
    pub(crate) fn get_emit_syntax_for_module_specifier_expression(
        &self,
        usage: P<Node>,
    ) -> ModuleKind {
        if ast::is_string_literal_like(usage) {
            return self.program.get_emit_syntax_for_usage_location(
                ast::get_source_file_of_node(usage).unwrap(),
                usage,
            );
        }
        ModuleKind::NONE
    }

    // checker.go:15127
    fn error_no_module_member_symbol(
        &mut self,
        module_symbol: P<Symbol>,
        target_symbol: P<Symbol>,
        node: P<Node>,
        name: P<Node>,
    ) {
        if self.compiler_options.no_check.is_true() {
            return;
        }
        let module_name = self.get_fully_qualified_name(module_symbol, Some(node));
        let declaration_name = tsrs_scanner::declaration_name_to_string(Some(name));
        let mut suggestion: Option<P<Symbol>> = None;
        if ast::is_identifier(name) {
            suggestion = self.get_suggested_symbol_for_nonexistent_module(name, target_symbol);
        }
        if let Some(suggestion) = suggestion {
            let suggestion_name = self.symbol_to_string(suggestion);
            let diagnostic = self.error(
                Some(name),
                &diagnostics::X_0_HAS_NO_EXPORTED_MEMBER_NAMED_1_DID_YOU_MEAN_2,
                &[&module_name, &declaration_name, &suggestion_name],
            );
            if let Some(value_declaration) = suggestion.value_declaration() {
                diagnostic.add_related_info(create_diagnostic_for_node(
                    Some(value_declaration),
                    &diagnostics::X_0_IS_DECLARED_HERE,
                    &[&suggestion_name],
                ));
            }
        } else if lookup_export(module_symbol, INTERNAL_SYMBOL_NAME_DEFAULT).is_some() {
            self.error(Some(name), &diagnostics::MODULE_0_HAS_NO_EXPORTED_MEMBER_1_DID_YOU_MEAN_TO_USE_IMPORT_1_FROM_0_INSTEAD, &[&module_name, &declaration_name]);
        } else {
            self.report_non_exported_member(name, &declaration_name, module_symbol, &module_name);
        }
    }

    // checker.go:15152
    fn report_non_exported_member(
        &mut self,
        name: P<Node>,
        declaration_name: &str,
        module_symbol: P<Symbol>,
        module_name: &str,
    ) {
        let mut local_symbol: Option<P<Symbol>> = None;
        if let Some(locals) = module_symbol.value_declaration().unwrap().locals() {
            local_symbol = locals.lookup(name.text());
        }
        let exports = module_symbol.exports();
        if let Some(local_symbol) = local_symbol {
            if let Some(exported_equals_symbol) =
                exports.and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS))
            {
                if self.get_symbol_if_same_reference(exported_equals_symbol, local_symbol).is_some()
                {
                    self.report_invalid_import_equals_export_member(
                        name,
                        declaration_name,
                        module_name,
                    );
                } else {
                    self.error(
                        Some(name),
                        &diagnostics::MODULE_0_HAS_NO_EXPORTED_MEMBER_1,
                        &[&module_name, &declaration_name],
                    );
                }
            } else {
                // Go's findInMap over the exports map (iteration order is unspecified in Go).
                let mut exported_symbol: Option<P<Symbol>> = None;
                if let Some(exports) = exports
                    && let Some(symbol) = exports.entries().into_iter().find_map(|(_, symbol)| {
                        self.get_symbol_if_same_reference(symbol, local_symbol).map(|_| symbol)
                    })
                {
                    exported_symbol = Some(symbol);
                }
                let diagnostic = if let Some(exported_symbol) = exported_symbol {
                    let s = self.symbol_to_string(exported_symbol);
                    self.error(
                        Some(name),
                        &diagnostics::MODULE_0_DECLARES_1_LOCALLY_BUT_IT_IS_EXPORTED_AS_2,
                        &[&module_name, &declaration_name, &s],
                    )
                } else {
                    self.error(
                        Some(name),
                        &diagnostics::MODULE_0_DECLARES_1_LOCALLY_BUT_IT_IS_NOT_EXPORTED,
                        &[&module_name, &declaration_name],
                    )
                };
                let declarations = local_symbol.declarations();
                for (i, &decl) in declarations.iter().enumerate() {
                    diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(decl),
                        if i == 0 {
                            &diagnostics::X_0_IS_DECLARED_HERE
                        } else {
                            &diagnostics::X_AND_HERE
                        },
                        &[&declaration_name],
                    ));
                }
            }
        } else {
            self.error(
                Some(name),
                &diagnostics::MODULE_0_HAS_NO_EXPORTED_MEMBER_1,
                &[&module_name, &declaration_name],
            );
        }
    }

    // checker.go:15184
    fn report_invalid_import_equals_export_member(
        &mut self,
        name: P<Node>,
        declaration_name: &str,
        module_name: &str,
    ) {
        if self.module_kind >= ModuleKind::ES2015 {
            self.error(
                Some(name),
                &diagnostics::X_0_CAN_ONLY_BE_IMPORTED_BY_USING_A_DEFAULT_IMPORT,
                &[&declaration_name],
            );
        } else if ast::is_in_js_file(name) {
            self.error(Some(name), &diagnostics::X_0_CAN_ONLY_BE_IMPORTED_BY_USING_A_REQUIRE_CALL_OR_BY_USING_A_DEFAULT_IMPORT, &[&declaration_name]);
        } else {
            self.error(
                Some(name),
                &diagnostics::X_0_CAN_ONLY_BE_IMPORTED_BY_USING_IMPORT_1_REQUIRE_2_OR_A_DEFAULT_IMPORT,
                &[&declaration_name, &declaration_name, &module_name],
            );
        }
    }

    // checker.go:15194
    pub(crate) fn get_target_of_export_specifier(
        &mut self,
        node: P<Node>,
        meaning: SymbolFlags,
        dont_resolve_alias: bool,
    ) -> Option<P<Symbol>> {
        let name = node.property_name_or_name().unwrap();
        if ast::module_export_name_is_default(name) {
            let specifier = Self::get_module_specifier_for_import_or_export(node);
            if let Some(specifier) = specifier {
                let attributes_type = self.get_type_from_import_attributes(
                    ast::get_import_attributes(node.parent().unwrap().parent().unwrap()),
                );
                let module_symbol = self.resolve_external_module_name(
                    node,
                    specifier,
                    false, /*ignoreErrors*/
                    attributes_type,
                );
                if let Some(module_symbol) = module_symbol {
                    return self.get_target_of_module_default(
                        module_symbol,
                        node,
                        dont_resolve_alias,
                    );
                }
            }
        }
        let export_declaration = node.parent().unwrap().parent().unwrap();
        let resolved = if export_declaration.module_specifier().is_some() {
            self.get_external_module_member(export_declaration, node, dont_resolve_alias)
        } else if ast::is_string_literal(name) {
            None
        } else {
            self.resolve_entity_name(
                name,
                meaning,
                false, /*ignoreErrors*/
                dont_resolve_alias,
                None, /*location*/
            )
        };
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        resolved
    }

    // checker.go:15219
    pub(crate) fn get_target_of_export_assignment(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        // An `export =` / `export default` inside a namespace/module block is a grammar error;
        // checkExportAssignment reports it and returns without resolving the expression. Mirror that
        // bail-out here (using the same container computation) so that alias resolution triggered by
        // the emit resolver does not resolve — and report "Cannot find name" diagnostics on — the
        // expression, which would produce diagnostics inconsistent with checking.
        if crate::is_contained_by_namespace(node) {
            return None;
        }
        let resolved = self.get_target_of_alias_like_expression(node.expression().unwrap());
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        resolved
    }

    // checker.go:15233
    pub(crate) fn get_target_of_binary_expression(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let resolved =
            self.get_target_of_alias_like_expression(node.as_binary_expression().right());
        self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
        resolved
    }
}
