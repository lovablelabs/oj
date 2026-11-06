use std::cell::RefCell;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_ast::{
    self as ast, Kind, ModifierFlags, ModifierList, Node, NodeFactory, NodeFlags, SourceFile,
    Symbol, TokenFlags,
};
use tsrs_checker::{Checker, Flags, InternalFlags, ObjectFlags, Type, TypeFlags, UnionReduction};
use tsrs_core::context::Context;
use tsrs_core::{LanguageVariant, NewLineKind, P, TextRange, alloc_str};
use tsrs_diagnostics as diagnostics;
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, AutoGenerateOptions, EmitFlags, GeneratedIdentifierFlags};
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport::{self, IdToSymbol};
use crate::change::{self, LeadingTriviaOption, NodeOptions};
use crate::codeactions::{
    CodeAction, CodeFixContext, CodeFixProvider, CombinedCodeActions, is_fixable_diagnostic,
};
use crate::diagnostics::get_all_diagnostics;

// codeactions_fixmissingtypeannotation.go:21
fn isolated_declarations_fix_error_codes() -> &'static [i32] {
    static CODES: std::sync::LazyLock<Vec<i32>> = std::sync::LazyLock::new(|| {
        vec![
            diagnostics::FUNCTION_MUST_HAVE_AN_EXPLICIT_RETURN_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::METHOD_MUST_HAVE_AN_EXPLICIT_RETURN_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::AT_LEAST_ONE_ACCESSOR_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::VARIABLE_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::PARAMETER_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::PROPERTY_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::EXPRESSION_TYPE_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::BINDING_ELEMENTS_WITH_INITIALIZERS_CAN_T_BE_EXPORTED_DIRECTLY_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::COMPUTED_PROPERTY_NAMES_ON_CLASS_OR_OBJECT_LITERALS_CANNOT_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::COMPUTED_PROPERTIES_MUST_BE_NUMBER_OR_STRING_LITERALS_VARIABLES_OR_DOTTED_EXPRESSIONS_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::ENUM_MEMBER_INITIALIZERS_MUST_BE_COMPUTABLE_WITHOUT_REFERENCES_TO_EXTERNAL_SYMBOLS_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::EXTENDS_CLAUSE_CAN_T_CONTAIN_AN_EXPRESSION_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::OBJECTS_THAT_CONTAIN_SHORTHAND_PROPERTIES_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::OBJECTS_THAT_CONTAIN_SPREAD_ASSIGNMENTS_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::ARRAYS_WITH_SPREAD_ELEMENTS_CAN_T_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::DEFAULT_EXPORTS_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::ONLY_CONST_ARRAYS_CAN_BE_INFERRED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::ASSIGNING_PROPERTIES_TO_FUNCTIONS_WITHOUT_DECLARING_THEM_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS_ADD_AN_EXPLICIT_DECLARATION_FOR_THE_PROPERTIES_ASSIGNED_TO_THIS_FUNCTION.code(),
            diagnostics::DECLARATION_EMIT_FOR_THIS_PARAMETER_REQUIRES_IMPLICITLY_ADDING_UNDEFINED_TO_ITS_TYPE_THIS_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::TYPE_CONTAINING_PRIVATE_NAME_0_CAN_T_BE_USED_WITH_ISOLATED_DECLARATIONS.code(),
            diagnostics::ADD_SATISFIES_AND_A_TYPE_ASSERTION_TO_THIS_EXPRESSION_SATISFIES_T_AS_T_TO_MAKE_THE_TYPE_EXPLICIT.code(),
        ]
    });
    &CODES
}

// codeactions_fixmissingtypeannotation.go:45
const FIX_MISSING_TYPE_ANNOTATION_ON_EXPORTS_FIX_ID: &str = "fixMissingTypeAnnotationOnExports";

// codeactions_fixmissingtypeannotation.go:48
// IsolatedDeclarationsFixProvider is the CodeFixProvider for isolatedDeclarations-related type annotation fixes.
pub static ISOLATED_DECLARATIONS_FIX_PROVIDER: std::sync::LazyLock<CodeFixProvider> =
    std::sync::LazyLock::new(|| CodeFixProvider {
        error_codes: isolated_declarations_fix_error_codes(),
        get_code_actions: get_isolated_declarations_code_actions,
        get_all_code_actions: Some(get_all_isolated_declarations_code_actions),
    });

// codeactions_fixmissingtypeannotation.go:56
// canHaveTypeAnnotationKinds are the node kinds that can have type annotations added.
fn can_have_type_annotation_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::GetAccessor
            | Kind::MethodDeclaration
            | Kind::PropertyDeclaration
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::VariableDeclaration
            | Kind::Parameter
            | Kind::ExportAssignment
            | Kind::ClassDeclaration
            | Kind::ObjectBindingPattern
            | Kind::ArrayBindingPattern
    )
}

// codeactions_fixmissingtypeannotation.go:72
// declarationEmitNodeBuilderFlags are the node builder flags used for declaration emit.
fn declaration_emit_node_builder_flags() -> Flags {
    Flags::MultilineObjectLiterals
        | Flags::WriteClassExpressionAsTypeLiteral
        | Flags::UseTypeOfFunction
        | Flags::UseStructuralFallback
        | Flags::AllowEmptyTuple
        | Flags::GenerateNamesForShadowedTypeParams
        | Flags::NoTruncation
}

// codeactions_fixmissingtypeannotation.go:81
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum TypePrintMode {
    #[default]
    Full,
    Relative, // typeof X
    Widened,  // widened literal type
}

// codeactions_fixmissingtypeannotation.go:88
#[expect(
    clippy::unnecessary_wraps,
    reason = "CodeFixRegistration callbacks share a fallible result so the dispatcher propagates errors from other fixes."
)]
fn get_isolated_declarations_code_actions(
    ctx: &Context,
    fix_context: &CodeFixContext,
) -> Result<Vec<CodeAction>, lsproto::Error> {
    let mut ch = fix_context.program.get_type_checker_for_file(ctx, fix_context.source_file);

    let mut fixes: Vec<CodeAction> = Vec::new();

    let mut add_fix = |action: Option<CodeAction>| {
        if let Some(action) = action {
            fixes.push(action);
        }
    };

    // Match TS ordering: Full annotation, Relative annotation, Widened annotation,
    // Full inline, Relative inline, Widened inline, Full extract
    let modes = [TypePrintMode::Full, TypePrintMode::Relative, TypePrintMode::Widened];

    for mode in modes {
        add_fix(try_code_action(ctx, fix_context, &mut ch, &mut |f| {
            f.type_print_mode = mode;
            f.add_type_annotation(fix_context.span)
        }));
    }

    for mode in modes {
        add_fix(try_code_action(ctx, fix_context, &mut ch, &mut |f| {
            f.type_print_mode = mode;
            f.add_inline_assertion(fix_context.span)
        }));
    }

    // extractAsVariable only in Full mode
    add_fix(try_code_action(ctx, fix_context, &mut ch, &mut |f| {
        f.type_print_mode = TypePrintMode::Full;
        f.extract_as_variable(fix_context.span)
    }));

    Ok(fixes)
}

// codeactions_fixmissingtypeannotation.go:128
#[expect(
    clippy::unnecessary_wraps,
    reason = "CodeFixRegistration callbacks share a fallible result so the dispatcher propagates errors from other fixes."
)]
fn get_all_isolated_declarations_code_actions(
    ctx: &Context,
    fix_context: &CodeFixContext,
) -> Result<Option<CombinedCodeActions>, lsproto::Error> {
    let all_diags = get_all_diagnostics(ctx, fix_context.program, fix_context.source_file);

    let mut ch = fix_context.program.get_type_checker_for_file(ctx, fix_context.source_file);

    let mut change_tracker = change::new_tracker(
        ctx,
        &fix_context.program.options(),
        fix_context.ls.format_options(),
        Arc::clone(&fix_context.ls.converters),
    );

    let mut fixer = IsolatedDeclarationsFixer {
        source_file: fix_context.source_file,
        checker: &mut ch,
        change_tracker: &mut change_tracker,
        fixed_nodes: FxHashMap::default(),
        type_print_mode: TypePrintMode::Full,
        symbols_to_import: Vec::new(),
        mutated_target: false,
    };

    for diag in all_diags {
        if is_fixable_diagnostic(diag, isolated_declarations_fix_error_codes())
            && let Some(span) = diag.loc()
        {
            fixer.add_type_annotation(span);
        }
    }

    let symbols_to_import = fixer.symbols_to_import.clone();
    for sym in symbols_to_import {
        fixer.add_symbol_to_existing_import(Some(sym));
    }

    let (mut changes, _) = change_tracker.get_changes();
    let file_changes = changes
        .shift_remove(crate::lsconv::Script::original_file_name(&fix_context.source_file))
        .unwrap_or_default();
    if file_changes.is_empty() {
        return Ok(None);
    }

    Ok(Some(CombinedCodeActions {
        description: diagnostics::ADD_ALL_MISSING_TYPE_ANNOTATIONS.localize(&[]),
        changes: file_changes,
    }))
}

// codeactions_fixmissingtypeannotation.go:170
fn try_code_action(
    ctx: &Context,
    fix_context: &CodeFixContext,
    ch: &mut Checker,
    f: &mut dyn FnMut(&mut IsolatedDeclarationsFixer) -> String,
) -> Option<CodeAction> {
    let mut change_tracker = change::new_tracker(
        ctx,
        &fix_context.program.options(),
        fix_context.ls.format_options(),
        Arc::clone(&fix_context.ls.converters),
    );

    // importAdder may be nil if the auto-import registry is not available;
    // type node transformation still works without it, just without adding imports.
    // (Go never assigns the adder here, so it stays nil.)

    let mut fixer = IsolatedDeclarationsFixer {
        source_file: fix_context.source_file,
        checker: ch,
        change_tracker: &mut change_tracker,
        fixed_nodes: FxHashMap::default(),
        type_print_mode: TypePrintMode::Full,
        symbols_to_import: Vec::new(),
        mutated_target: false,
    };

    let description = f(&mut fixer);
    if description.is_empty() {
        return None;
    }

    // Add any symbols that need to be imported to existing import declarations
    let symbols_to_import = fixer.symbols_to_import.clone();
    for sym in symbols_to_import {
        fixer.add_symbol_to_existing_import(Some(sym));
    }

    let (mut changes, _) = change_tracker.get_changes();
    let file_changes = changes
        .shift_remove(crate::lsconv::Script::original_file_name(&fix_context.source_file))
        .unwrap_or_default();

    if file_changes.is_empty() {
        return None;
    }

    Some(CodeAction {
        description,
        changes: file_changes,
        fix_id: FIX_MISSING_TYPE_ANNOTATION_ON_EXPORTS_FIX_ID.to_string(),
    })
}

// codeactions_fixmissingtypeannotation.go:220
// isolatedDeclarationsFixer encapsulates the state for fixing isolated declarations errors.
// (Go's `importAdder` field is never set, its `program` field is never read and its `locale` is English here.)
struct IsolatedDeclarationsFixer<'a> {
    source_file: P<SourceFile>,
    checker: &'a mut Checker,
    change_tracker: &'a mut change::Tracker,
    fixed_nodes: FxHashMap<P<Node>, bool>,
    type_print_mode: TypePrintMode,
    symbols_to_import: Vec<P<Symbol>>,
    mutated_target: bool, // set by inferType/relativeType when the target was mutated (e.g., spread decomposition)
}

impl IsolatedDeclarationsFixer<'_> {
    fn factory(&self) -> NodeFactory {
        self.change_tracker.node_factory.clone()
    }

    // codeactions_fixmissingtypeannotation.go:233
    fn add_type_annotation(&mut self, span: TextRange) -> String {
        let node_with_diag = astnav::get_token_at_position(
            self.source_file,
            span.pos().expect_source("code-fix spans have source positions"),
        );

        if let Some(expando_function) = find_expando_function(self.checker, node_with_diag) {
            if ast::is_function_declaration(expando_function) {
                return self.create_namespace_for_expando_properties(expando_function);
            }
            return self.fix_isolated_declaration_error(expando_function);
        }

        if let Some(node_missing_type) = find_ancestor_with_missing_type(node_with_diag) {
            return self.fix_isolated_declaration_error(node_missing_type);
        }
        String::new()
    }

    // codeactions_fixmissingtypeannotation.go:250
    fn create_namespace_for_expando_properties(&mut self, expando_func: P<Node>) -> String {
        let Some(func_name) = expando_func.name() else {
            return String::new();
        };

        let t = self.checker.get_type_at_location(expando_func);
        let elements = self.checker.get_properties_of_type(t);
        if elements.is_empty() {
            return String::new();
        }

        let factory = self.factory();

        let mut new_properties: Vec<P<Node>> = Vec::new();
        new_properties.extend(elements.iter().filter_map(|&symbol| {
            if !scanner::is_identifier_text(symbol.name(), LanguageVariant::Standard) {
                return None;
            }
            // skip symbols that already have a variable declaration
            if symbol.value_declaration().is_some_and(ast::is_variable_declaration) {
                return None;
            }

            let sym_type = self.checker.get_type_of_symbol_exported(symbol);
            let type_node = self.type_to_minimized_reference_type(
                sym_type,
                expando_func,
                declaration_emit_node_builder_flags(),
            )?;

            let var_decl = factory.new_variable_declaration(
                factory.new_identifier(symbol.name()),
                None,
                Some(type_node),
                None,
            );
            let export_token = factory.new_token(Kind::ExportKeyword);
            let var_decl_list = factory.new_variable_declaration_list(
                factory.new_node_list(vec![var_decl]),
                NodeFlags::None,
            );
            let var_stmt = factory.new_variable_statement(
                Some(factory.new_modifier_list(vec![export_token])),
                var_decl_list,
            );
            Some(var_stmt)
        }));

        if new_properties.is_empty() {
            return String::new();
        }

        let mut modifiers: Vec<P<Node>> = Vec::new();
        if ast::has_syntactic_modifier(expando_func, ModifierFlags::Export) {
            modifiers.push(factory.new_token(Kind::ExportKeyword));
        }
        modifiers.push(factory.new_token(Kind::DeclareKeyword));

        let namespace = factory.new_module_declaration(
            Some(factory.new_modifier_list(modifiers)),
            Kind::NamespaceKeyword,
            factory.new_identifier(func_name.text()),
            None, /*attributes*/
            Some(factory.new_module_block(factory.new_node_list(new_properties))),
        );
        // Set the flags for namespace
        namespace
            .set_flags(NodeFlags::Ambient | NodeFlags::ExportContext | NodeFlags::ContextFlags);

        self.change_tracker.insert_node_after(self.source_file, expando_func, namespace);
        diagnostics::ANNOTATE_TYPES_OF_PROPERTIES_EXPANDO_FUNCTION_IN_A_NAMESPACE.localize(&[])
    }

    // codeactions_fixmissingtypeannotation.go:321
    fn add_inline_assertion(&mut self, span: TextRange) -> String {
        let node_with_diag = astnav::get_token_at_position(
            self.source_file,
            span.pos().expect_source("code-fix spans have source positions"),
        );

        // No inline assertions for expando members
        if find_expando_function(self.checker, node_with_diag).is_some() {
            return String::new();
        }

        let Some(target_node) = find_best_fitting_node(Some(node_with_diag), span) else {
            return String::new();
        };
        if is_value_signature_declaration(Some(target_node))
            || is_value_signature_declaration(target_node.parent())
        {
            return String::new();
        }

        let is_expression_target = ast::is_expression(target_node);
        let is_shorthand_property_assignment_target =
            ast::is_shorthand_property_assignment(target_node);

        // Go's IsDeclaration is broader than TS's isDeclaration (e.g. CallExpression has DeclarationData
        // in Go but is not a declaration kind in TS). Use isNamedDeclarationKind to match TS behavior.
        if !is_shorthand_property_assignment_target && is_named_declaration_kind(target_node) {
            return String::new();
        }
        // No inline assertions on binding patterns
        if ast::find_ancestor(target_node, ast::is_binding_pattern).is_some() {
            return String::new();
        }
        // No inline assertions on enum members
        if ast::find_ancestor(target_node, ast::is_enum_member).is_some() {
            return String::new();
        }
        // No support for typeof in extends clauses
        if is_expression_target
            && (ast::find_ancestor_kind(target_node, Kind::HeritageClause).is_some()
                || ast::find_ancestor(target_node, ast::is_type_node).is_some())
        {
            return String::new();
        }
        // Can't inline type spread elements
        if ast::is_spread_element(target_node) {
            return String::new();
        }

        let variable_declaration = ast::find_ancestor_kind(target_node, Kind::VariableDeclaration);
        let variable_type = variable_declaration.map(|d| self.checker.get_type_at_location(d));
        // Can't use typeof on unique symbols
        if variable_type.is_some_and(|t| t.flags().intersects(TypeFlags::UniqueESSymbol)) {
            return String::new();
        }

        if !is_expression_target && !is_shorthand_property_assignment_target {
            return String::new();
        }

        let type_node = self.infer_type(target_node, variable_type);
        let Some(type_node) = type_node.filter(|_| !self.mutated_target) else {
            return String::new();
        };

        let factory = self.factory();

        if is_shorthand_property_assignment_target {
            // Insert `: expr as Type` after the shorthand property name
            let cloned_name = factory.deep_clone_node(target_node.name()).unwrap();
            let as_expr = create_as_expression(&factory, cloned_name, type_node);
            self.change_tracker.insert_node_at(
                self.source_file,
                target_node.end().expect_source("parsed target nodes have source positions"),
                as_expr,
                NodeOptions { prefix: ": ".to_string(), ..Default::default() },
            );
        } else if is_expression_target {
            // Replace expression with `(expression) satisfies Type as Type` or `expression satisfies Type as Type`
            let mut cloned_target = factory.deep_clone_node(Some(target_node)).unwrap();
            if needs_parenthesized_expression_for_assertion(target_node) {
                cloned_target = factory.new_parenthesized_expression(cloned_target);
            }
            let cloned_type = factory.deep_clone_node(Some(type_node)).unwrap();
            let satisfies_as_expr = factory.new_as_expression(
                factory.new_satisfies_expression(cloned_target, cloned_type),
                type_node,
            );
            self.change_tracker.replace_node(
                self.source_file,
                target_node,
                satisfies_as_expr,
                None,
            );
        } else {
            return String::new();
        }

        diagnostics::ADD_SATISFIES_AND_AN_INLINE_TYPE_ASSERTION_WITH_0
            .localize(&[&type_to_string_for_diag(type_node, self.source_file, self.change_tracker)])
    }

    // codeactions_fixmissingtypeannotation.go:404
    fn extract_as_variable(&mut self, span: TextRange) -> String {
        let node_with_diag = astnav::get_token_at_position(
            self.source_file,
            span.pos().expect_source("code-fix spans have source positions"),
        );
        let Some(target_node) = find_best_fitting_node(Some(node_with_diag), span) else {
            return String::new();
        };
        if is_value_signature_declaration(Some(target_node))
            || is_value_signature_declaration(target_node.parent())
        {
            return String::new();
        }

        if !ast::is_expression(target_node) {
            return String::new();
        }

        let factory = self.factory();

        // Array literals should be marked as const
        if ast::is_array_literal_expression(target_node) {
            let const_ref = factory.new_type_reference_node(factory.new_identifier("const"), None);
            let cloned = factory.deep_clone_node(Some(target_node)).unwrap();
            let replacement = create_as_expression(&factory, cloned, const_ref);
            self.change_tracker.replace_node(self.source_file, target_node, replacement, None);
            return diagnostics::MARK_ARRAY_LITERAL_AS_CONST.localize(&[]);
        }

        if let Some(parent_property_assignment) =
            ast::find_ancestor_kind(target_node, Kind::PropertyAssignment)
        {
            // Identifiers or entity names can already be typeof-ed
            if Some(parent_property_assignment) == target_node.parent()
                && ast::is_entity_name_expression(target_node)
            {
                return String::new();
            }

            let temp_name = self.change_tracker.emit_context.factory.new_unique_name_ex(
                &get_identifier_name_for_node(target_node),
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::Optimistic,
                    ..Default::default()
                },
            );

            let mut replacement_target = target_node;
            let mut initialization_node = target_node;

            // Handle spread elements: walk up to the spread's parent and handle const assertions
            if ast::is_spread_element(replacement_target) {
                replacement_target =
                    ast::walk_up_parenthesized_expressions(replacement_target.parent()).unwrap();
                if is_const_assertion(replacement_target.parent()) {
                    replacement_target = replacement_target.parent().unwrap();
                    initialization_node = replacement_target;
                } else {
                    let const_ref =
                        factory.new_type_reference_node(factory.new_identifier("const"), None);
                    initialization_node = create_as_expression(
                        &factory,
                        factory.deep_clone_node(Some(replacement_target)).unwrap(),
                        const_ref,
                    );
                }
            }

            if ast::is_entity_name_expression(replacement_target) {
                return String::new();
            }

            let cloned_init = factory.deep_clone_node(Some(initialization_node)).unwrap();
            let var_decl =
                factory.new_variable_declaration(temp_name, None, None, Some(cloned_init));
            let var_decl_list = factory.new_variable_declaration_list(
                factory.new_node_list(vec![var_decl]),
                NodeFlags::Const,
            );
            let var_stmt = factory.new_variable_statement(None, var_decl_list);

            let Some(statement) = ast::find_ancestor(target_node, ast::is_statement) else {
                return String::new();
            };
            self.change_tracker.insert_node_before(
                self.source_file,
                statement,
                var_stmt,
                false,
                LeadingTriviaOption::None,
            );

            let type_query = factory.new_type_query_node(temp_name, None);
            let as_expr = factory.new_as_expression(temp_name, type_query);
            self.change_tracker.replace_node(self.source_file, replacement_target, as_expr, None);

            let id_text = type_to_string_for_diag(temp_name, self.source_file, self.change_tracker);
            return diagnostics::EXTRACT_TO_VARIABLE_AND_REPLACE_WITH_0_AS_TYPEOF_0
                .localize(&[&id_text]);
        }

        String::new()
    }

    // codeactions_fixmissingtypeannotation.go:568
    fn fix_isolated_declaration_error(&mut self, node: P<Node>) -> String {
        // Avoid creating duplicate fixes for the same node
        let fixed = self.fixed_nodes.entry(node).or_default();
        if *fixed {
            return String::new();
        }
        *fixed = true;

        match node.kind() {
            Kind::Parameter | Kind::PropertyDeclaration | Kind::VariableDeclaration => {
                self.add_type_to_variable_like(node)
            }
            Kind::ArrowFunction
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor => self.add_type_to_signature_declaration(node),
            Kind::ExportAssignment => self.transform_export_assignment(node),
            Kind::ClassDeclaration => self.transform_extends_clause_with_expression(node),
            Kind::ObjectBindingPattern | Kind::ArrayBindingPattern => {
                self.transform_destructuring_patterns(node)
            }
            _ => String::new(),
        }
    }

    // codeactions_fixmissingtypeannotation.go:592
    fn add_type_to_signature_declaration(&mut self, func_node: P<Node>) -> String {
        if func_node.type_node().is_some() {
            return String::new();
        }
        let Some(type_node) = self.infer_type(func_node, None) else {
            return String::new();
        };
        self.change_tracker.try_insert_type_annotation(self.source_file, func_node, type_node);
        diagnostics::ADD_RETURN_TYPE_0.localize(&[&type_to_string_for_diag(
            type_node,
            self.source_file,
            self.change_tracker,
        )])
    }

    // codeactions_fixmissingtypeannotation.go:604
    fn transform_export_assignment(&mut self, default_export: P<Node>) -> String {
        let export_assignment = default_export.as_export_assignment();
        if export_assignment.is_export_equals {
            return String::new();
        }

        let expression = export_assignment.expression();
        let Some(type_node) = self.infer_type(expression, None) else {
            return String::new();
        };

        let factory = self.factory();

        let default_identifier =
            self.change_tracker.emit_context.factory.new_unique_name("_default");

        // Deep clone the expression so synthesized nodes don't reference original source positions
        let cloned_expression = factory.deep_clone_node(Some(expression)).unwrap();

        let var_decl = factory.new_variable_declaration(
            default_identifier,
            None,
            Some(type_node),
            Some(cloned_expression),
        );
        let var_decl_list = factory
            .new_variable_declaration_list(factory.new_node_list(vec![var_decl]), NodeFlags::Const);
        let var_stmt = factory.new_variable_statement(None, var_decl_list);

        let new_export = factory.update_export_assignment(
            default_export,
            default_export.modifiers(),
            false,
            None,
            default_identifier,
        );

        self.change_tracker.replace_node_with_nodes(
            self.source_file,
            default_export,
            vec![var_stmt, new_export],
            None,
        );
        diagnostics::EXTRACT_DEFAULT_EXPORT_TO_VARIABLE.localize(&[])
    }

    // codeactions_fixmissingtypeannotation.go:633
    fn transform_extends_clause_with_expression(&mut self, class_decl: P<Node>) -> String {
        let mut extends_clause: Option<P<Node>> = None;
        if let Some(clause) = class_decl
            .as_class_declaration()
            .heritage_clauses()
            .map_or(&[][..], |h| h.nodes())
            .iter()
            .find_map(|&clause| {
                if clause.as_heritage_clause().token == Kind::ExtendsKeyword {
                    Some(clause)
                } else {
                    None
                }
            })
        {
            extends_clause = Some(clause);
        }
        let Some(extends_clause) = extends_clause else {
            return String::new();
        };

        let heritage_types = extends_clause.as_heritage_clause().types();
        if heritage_types.nodes().is_empty() {
            return String::new();
        }
        let heritage_expression = heritage_types.nodes()[0];
        let expression = heritage_expression.as_expression_with_type_arguments().expression;

        let Some(heritage_type_node) = self.infer_type(expression, None) else {
            return String::new();
        };

        let factory = self.factory();

        let mut base_name = "Anonymous".to_string();
        if let Some(name) = class_decl.name() {
            base_name = format!("{}Base", name.text());
        }
        let base_class_name = self.change_tracker.emit_context.factory.new_unique_name_ex(
            &base_name,
            AutoGenerateOptions {
                flags: GeneratedIdentifierFlags::Optimistic,
                ..Default::default()
            },
        );

        // Create: const <BaseName>: <type> = <expression>;
        let cloned_expression = factory.deep_clone_node(Some(expression)).unwrap();
        let var_decl = factory.new_variable_declaration(
            base_class_name,
            None,
            Some(heritage_type_node),
            Some(cloned_expression),
        );
        let var_decl_list = factory
            .new_variable_declaration_list(factory.new_node_list(vec![var_decl]), NodeFlags::Const);
        let var_stmt = factory.new_variable_statement(None, var_decl_list);

        self.change_tracker.insert_node_before(
            self.source_file,
            class_decl,
            var_stmt,
            false,
            LeadingTriviaOption::None,
        );

        // Replace the heritage expression with the base class name
        let replacement = factory.new_expression_with_type_arguments(base_class_name, None);
        self.change_tracker.replace_node(self.source_file, heritage_expression, replacement, None);

        diagnostics::EXTRACT_BASE_CLASS_TO_VARIABLE.localize(&[])
    }

    // codeactions_fixmissingtypeannotation.go:676
    fn transform_destructuring_patterns(&mut self, binding_pattern: P<Node>) -> String {
        let enclosing_variable_declaration = binding_pattern.parent().unwrap();
        if !ast::is_variable_declaration(enclosing_variable_declaration) {
            return String::new();
        }
        let enclosing_var_stmt = enclosing_variable_declaration.parent().unwrap().parent().unwrap();
        if !ast::is_variable_statement(enclosing_var_stmt) {
            return String::new();
        }

        let Some(initializer) = enclosing_variable_declaration.initializer() else {
            return String::new();
        };

        let factory = self.factory();
        let mut new_nodes: Vec<P<Node>> = Vec::new();

        let base_expr_node = if ast::is_identifier(initializer) {
            // Use a new identifier to avoid referencing original source positions
            factory.new_identifier(initializer.text())
        } else {
            // Create a temporary variable for complex expressions
            let temp_name = self.change_tracker.emit_context.factory.new_unique_name_ex(
                "dest",
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::Optimistic,
                    ..Default::default()
                },
            );
            let cloned_initializer = factory.deep_clone_node(Some(initializer)).unwrap();
            let var_decl =
                factory.new_variable_declaration(temp_name, None, None, Some(cloned_initializer));
            let var_decl_list = factory.new_variable_declaration_list(
                factory.new_node_list(vec![var_decl]),
                NodeFlags::Const,
            );
            let var_stmt = factory.new_variable_statement(None, var_decl_list);
            new_nodes.push(var_stmt);
            temp_name
        };

        // Extract each binding element as a separate variable with type annotation
        self.extract_binding_elements(
            binding_pattern,
            base_expr_node,
            &mut new_nodes,
            enclosing_var_stmt,
        );

        if new_nodes.is_empty() {
            return String::new();
        }

        // If the enclosing variable statement has multiple declarations, preserve the non-destructuring ones
        let decl_list_node = enclosing_var_stmt.as_variable_statement().declaration_list;
        let decl_list = decl_list_node.as_variable_declaration_list();
        if decl_list.declarations.nodes().len() > 1 {
            let remaining_decls: Vec<P<Node>> = decl_list
                .declarations
                .nodes()
                .iter()
                .copied()
                .filter(|&d| d != enclosing_variable_declaration)
                .collect();
            if !remaining_decls.is_empty() {
                new_nodes.push(factory.update_variable_statement(
                    enclosing_var_stmt,
                    enclosing_var_stmt.modifiers(),
                    factory.update_variable_declaration_list(
                        decl_list_node,
                        factory.new_node_list(remaining_decls),
                        decl_list_node.flags(),
                    ),
                ));
            }
        }

        self.change_tracker.replace_node_with_nodes(
            self.source_file,
            enclosing_var_stmt,
            new_nodes,
            None,
        );
        diagnostics::EXTRACT_BINDING_EXPRESSIONS_TO_VARIABLE.localize(&[])
    }

    // codeactions_fixmissingtypeannotation.go:729
    fn extract_binding_elements(
        &mut self,
        binding_pattern: P<Node>,
        base_expr: P<Node>,
        new_nodes: &mut Vec<P<Node>>,
        enclosing_var_stmt: P<Node>,
    ) {
        let factory = self.factory();

        if ast::is_object_binding_pattern(binding_pattern) {
            for &element in binding_pattern.as_binding_pattern().elements.nodes() {
                if ast::is_omitted_expression(element) {
                    continue;
                }
                let be = element.as_binding_element();
                let Some(name) = be.name() else {
                    continue;
                };

                // Build property access expression
                let access_expr: P<Node>;
                if let Some(property_name) =
                    be.property_name().filter(|p| ast::is_computed_property_name(*p))
                {
                    // Handle computed property names: create a temp variable for the computed expression
                    let computed_expression = property_name.as_computed_property_name().expression;
                    let identifier_for_computed_property = self
                        .change_tracker
                        .emit_context
                        .factory
                        .new_generated_name_for_node(computed_expression);
                    let comp_var_decl = factory.new_variable_declaration(
                        identifier_for_computed_property,
                        None,
                        None,
                        Some(computed_expression),
                    );
                    let comp_var_decl_list = factory.new_variable_declaration_list(
                        factory.new_node_list(vec![comp_var_decl]),
                        NodeFlags::Const,
                    );
                    let comp_var_stmt = factory.new_variable_statement(None, comp_var_decl_list);
                    new_nodes.push(comp_var_stmt);
                    access_expr = factory.new_element_access_expression(
                        base_expr,
                        None,
                        identifier_for_computed_property,
                        NodeFlags::None,
                    );
                } else if let Some(property_name) = be.property_name() {
                    // Use property name text (handles identifiers, string literals, numeric literals)
                    let prop_text = property_name.text();
                    access_expr = factory.new_property_access_expression(
                        base_expr,
                        None,
                        factory.new_identifier(prop_text),
                        NodeFlags::None,
                    );
                } else if ast::is_identifier(name) {
                    access_expr = factory.new_property_access_expression(
                        base_expr,
                        None,
                        factory.new_identifier(name.text()),
                        NodeFlags::None,
                    );
                } else {
                    continue;
                }

                if ast::is_binding_pattern(name) {
                    self.extract_binding_elements(name, access_expr, new_nodes, enclosing_var_stmt);
                } else {
                    self.emit_binding_element_variable(
                        &factory,
                        name,
                        element,
                        access_expr,
                        new_nodes,
                        enclosing_var_stmt,
                    );
                }
            }
        } else if ast::is_array_binding_pattern(binding_pattern) {
            for (i, &element) in
                binding_pattern.as_binding_pattern().elements.nodes().iter().enumerate()
            {
                if ast::is_omitted_expression(element) {
                    continue;
                }
                let be = element.as_binding_element();
                let Some(name) = be.name() else {
                    continue;
                };

                let access_expr = factory.new_element_access_expression(
                    base_expr,
                    None,
                    factory.new_numeric_literal(alloc_str(&i.to_string()), TokenFlags::None),
                    NodeFlags::None,
                );

                if ast::is_binding_pattern(name) {
                    self.extract_binding_elements(name, access_expr, new_nodes, enclosing_var_stmt);
                } else {
                    self.emit_binding_element_variable(
                        &factory,
                        name,
                        element,
                        access_expr,
                        new_nodes,
                        enclosing_var_stmt,
                    );
                }
            }
        }
    }

    // codeactions_fixmissingtypeannotation.go:799
    // emitBindingElementVariable creates a variable declaration for a single binding element,
    // handling default initializers by creating a ternary `temp === undefined ? default : temp`.
    fn emit_binding_element_variable(
        &mut self,
        factory: &NodeFactory,
        name: P<Node>,
        be_node: P<Node>,
        access_expr: P<Node>,
        new_nodes: &mut Vec<P<Node>>,
        enclosing_var_stmt: P<Node>,
    ) {
        let be = be_node.as_binding_element();
        let type_node = self.infer_type(name, None);
        let mut variable_initializer = access_expr;

        if let Some(be_initializer) = be.initializer() {
            // Create a temp variable to hold the accessed value, then use a conditional expression
            // to apply the default: temp === undefined ? defaultValue : temp
            let prop_name = be.property_name();
            let mut temp_base_name = "temp".to_string();
            if let Some(prop_name) = prop_name.filter(|p| ast::is_identifier(*p)) {
                temp_base_name = prop_name.text().to_string();
            }
            let temp_name = self.change_tracker.emit_context.factory.new_unique_name_ex(
                &temp_base_name,
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::Optimistic,
                    ..Default::default()
                },
            );
            let temp_var_decl =
                factory.new_variable_declaration(temp_name, None, None, Some(variable_initializer));
            let temp_var_decl_list = factory.new_variable_declaration_list(
                factory.new_node_list(vec![temp_var_decl]),
                NodeFlags::Const,
            );
            let temp_var_stmt = factory.new_variable_statement(None, temp_var_decl_list);
            new_nodes.push(temp_var_stmt);

            variable_initializer = factory.new_conditional_expression(
                factory.new_binary_expression(
                    None,
                    temp_name,
                    None,
                    factory.new_token(Kind::EqualsEqualsEqualsToken),
                    factory.new_identifier("undefined"),
                ),
                factory.new_token(Kind::QuestionToken),
                be_initializer,
                factory.new_token(Kind::ColonToken),
                variable_initializer,
            );
        }

        let export_modifier = self.get_export_modifier(enclosing_var_stmt);
        let var_decl = factory.new_variable_declaration(
            factory.new_identifier(name.text()),
            None,
            type_node,
            Some(variable_initializer),
        );
        let var_decl_list = factory
            .new_variable_declaration_list(factory.new_node_list(vec![var_decl]), NodeFlags::Const);
        let var_stmt = factory.new_variable_statement(export_modifier, var_decl_list);
        new_nodes.push(var_stmt);
    }

    // codeactions_fixmissingtypeannotation.go:851
    fn get_export_modifier(&self, enclosing_var_stmt: P<Node>) -> Option<P<ModifierList>> {
        if ast::has_syntactic_modifier(enclosing_var_stmt, ModifierFlags::Export) {
            let export_token = self.change_tracker.node_factory.new_token(Kind::ExportKeyword);
            return Some(self.change_tracker.node_factory.new_modifier_list(vec![export_token]));
        }
        None
    }

    // codeactions_fixmissingtypeannotation.go:859
    fn infer_type(&mut self, node: P<Node>, variable_type: Option<P<Type>>) -> Option<P<Node>> {
        self.mutated_target = false;

        // Handle Relative mode first: return typeof X for identifiers
        if self.type_print_mode == TypePrintMode::Relative {
            return self.relative_type(node);
        }

        let t: Option<P<Type>> = if is_value_signature_declaration(Some(node)) {
            let signature = self.checker.get_signature_from_declaration_exported(node);
            if let Some(type_predicate) =
                self.checker.get_type_predicate_of_signature_exported(signature)
            {
                let predicate_type = type_predicate.type_()?;
                let enclosing_decl = ast::find_ancestor(node, ast::is_declaration)
                    .unwrap_or(self.source_file.as_node());
                let mut flags = declaration_emit_node_builder_flags();
                if predicate_type.flags().intersects(TypeFlags::UniqueESSymbol) {
                    flags |= Flags::AllowUniqueESSymbolType;
                }
                return self.checker.type_predicate_to_type_predicate_node(
                    type_predicate,
                    Some(enclosing_decl),
                    flags,
                    None,
                );
            }
            Some(self.checker.get_return_type_of_signature_exported(signature))
        } else {
            Some(self.checker.get_type_at_location(node))
        };

        let mut t = t?;

        // Handle Widened mode: return widened literal type if different
        if self.type_print_mode == TypePrintMode::Widened {
            if let Some(variable_type) = variable_type {
                t = variable_type;
            }
            let widened_type = self.checker.get_widened_literal_type_exported(t);
            if self.checker.is_type_assignable_to_exported(widened_type, t) {
                return None; // widened type is same, no fix needed
            }
            t = widened_type;
        }

        let enclosing_decl =
            ast::find_ancestor(node, ast::is_declaration).unwrap_or(self.source_file.as_node());

        let flags = declaration_emit_node_builder_flags() | Self::get_extra_flags(node, t);

        // For parameters that require adding implicit undefined, add it to the type
        if ast::is_parameter_declaration(node)
            && self.checker.requires_adding_implicit_undefined(node)
        {
            let undefined_type = self.checker.get_undefined_type();
            t = self.checker.get_union_type_ex_exported(&[undefined_type, t], UnionReduction::None);
        }

        self.type_to_minimized_reference_type(t, enclosing_decl, flags)
    }

    // codeactions_fixmissingtypeannotation.go:922
    fn get_extra_flags(node: P<Node>, t: P<Type>) -> Flags {
        if (ast::is_variable_declaration(node)
            || (ast::is_property_declaration(node)
                && ast::has_syntactic_modifier(
                    node,
                    ModifierFlags::Static | ModifierFlags::Readonly,
                )))
            && t.flags().intersects(TypeFlags::UniqueESSymbol)
        {
            return Flags::AllowUniqueESSymbolType;
        }
        Flags::None
    }

    // codeactions_fixmissingtypeannotation.go:932
    // createTypeOfFromEntityNameExpression creates a `typeof X` type query node.
    fn create_type_of_from_entity_name_expression(&self, node: P<Node>) -> P<Node> {
        let factory = &self.change_tracker.node_factory;
        factory.new_type_query_node(factory.deep_clone_node(Some(node)).unwrap(), None)
    }

    // codeactions_fixmissingtypeannotation.go:940
    // typeFromArraySpreadElements decomposes an array literal with spread elements into
    // separate variables, returning a tuple type of typeof references.
    fn type_from_array_spread_elements(&mut self, node: P<Node>, name: &str) -> Option<P<Node>> {
        let is_in_const_context =
            ast::find_ancestor(node, |n| is_const_assertion(Some(n))).is_some();
        if !is_in_const_context {
            return None;
        }
        let name = if name.is_empty() { "temp" } else { name };
        let factory = self.factory();
        let f1 = factory.clone();
        let f2 = factory.clone();
        let f3 = factory;
        self.type_from_spreads(
            node,
            name,
            is_in_const_context,
            &|n| n.as_array_literal_expression().elements.nodes().to_vec(),
            &|n| ast::is_spread_element(n),
            &move |expr| f1.new_spread_element(expr),
            &move |elements| f2.new_array_literal_expression(f2.new_node_list(elements), true),
            &move |types| {
                let rest_types: Vec<P<Node>> =
                    types.into_iter().map(|t| f3.new_rest_type_node(t)).collect();
                f3.new_tuple_type_node(f3.new_node_list(rest_types))
            },
        )
    }

    // codeactions_fixmissingtypeannotation.go:974
    // typeFromObjectSpreadAssignment decomposes an object literal with spread assignments into
    // separate variables, returning an intersection type of typeof references.
    fn type_from_object_spread_assignment(&mut self, node: P<Node>, name: &str) -> Option<P<Node>> {
        let is_in_const_context =
            ast::find_ancestor(node, |n| is_const_assertion(Some(n))).is_some();
        let name = if name.is_empty() { "temp" } else { name };
        let factory = self.factory();
        let f1 = factory.clone();
        let f2 = factory.clone();
        let f3 = factory;
        self.type_from_spreads(
            node,
            name,
            is_in_const_context,
            &|n| n.as_object_literal_expression().properties.nodes().to_vec(),
            &|n| ast::is_spread_assignment(n),
            &move |expr| f1.new_spread_assignment(expr),
            &move |elements| f2.new_object_literal_expression(f2.new_node_list(elements), true),
            &move |types| f3.new_intersection_type_node(f3.new_node_list(types)),
        )
    }

    // codeactions_fixmissingtypeannotation.go:1006
    // typeFromSpreads is the generic spread decomposition function, ported from TS's typeFromSpreads.
    // It splits a literal with spread elements into separate const variables and returns a composed type.
    fn type_from_spreads(
        &mut self,
        node: P<Node>,
        name: &str,
        is_in_const_context: bool,
        get_children: &dyn Fn(P<Node>) -> Vec<P<Node>>,
        is_spread: &dyn Fn(P<Node>) -> bool,
        create_spread: &dyn Fn(P<Node>) -> P<Node>,
        make_node_of_kind: &dyn Fn(Vec<P<Node>>) -> P<Node>,
        final_type: &dyn Fn(Vec<P<Node>>) -> P<Node>,
    ) -> Option<P<Node>> {
        let factory = self.factory();
        let mut intersection_types: Vec<P<Node>> = Vec::new();
        let mut new_spreads: Vec<P<Node>> = Vec::new();
        let mut current_variable_properties: Vec<P<Node>> = Vec::new();

        let statement = ast::find_ancestor(node, ast::is_statement);

        let children = get_children(node);
        for prop in children {
            if is_spread(prop) {
                self.finalizes_variable_part(
                    &factory,
                    name,
                    is_in_const_context,
                    statement,
                    make_node_of_kind,
                    create_spread,
                    &mut current_variable_properties,
                    &mut intersection_types,
                    &mut new_spreads,
                );
                let expression = prop.expression().unwrap();
                if ast::is_entity_name_expression(expression) {
                    intersection_types
                        .push(self.create_type_of_from_entity_name_expression(expression));
                    new_spreads.push(prop);
                } else {
                    self.make_spread_variable(
                        &factory,
                        name,
                        is_in_const_context,
                        statement,
                        create_spread,
                        expression,
                        &mut intersection_types,
                        &mut new_spreads,
                    );
                }
            } else {
                current_variable_properties.push(prop);
            }
        }

        if new_spreads.is_empty() {
            return None;
        }

        self.finalizes_variable_part(
            &factory,
            name,
            is_in_const_context,
            statement,
            make_node_of_kind,
            create_spread,
            &mut current_variable_properties,
            &mut intersection_types,
            &mut new_spreads,
        );

        let replacement = make_node_of_kind(new_spreads);
        self.change_tracker.replace_node(self.source_file, node, replacement, None);
        self.mutated_target = true;

        Some(final_type(intersection_types))
    }

    // codeactions_fixmissingtypeannotation.go:1051
    // makeSpreadVariable creates a const variable for a spread expression and adds it to the decomposition.
    fn make_spread_variable(
        &mut self,
        factory: &NodeFactory,
        name: &str,
        is_in_const_context: bool,
        statement: Option<P<Node>>,
        create_spread: &dyn Fn(P<Node>) -> P<Node>,
        expression: P<Node>,
        intersection_types: &mut Vec<P<Node>>,
        new_spreads: &mut Vec<P<Node>>,
    ) {
        let temp_name = self.change_tracker.emit_context.factory.new_unique_name_ex(
            &format!("{}_Part{}", name, new_spreads.len() + 1),
            AutoGenerateOptions {
                flags: GeneratedIdentifierFlags::Optimistic,
                ..Default::default()
            },
        );

        let initializer = if is_in_const_context {
            let const_ref = factory.new_type_reference_node(factory.new_identifier("const"), None);
            factory.new_as_expression(factory.deep_clone_node(Some(expression)).unwrap(), const_ref)
        } else {
            factory.deep_clone_node(Some(expression)).unwrap()
        };

        let var_decl = factory.new_variable_declaration(temp_name, None, None, Some(initializer));
        let var_decl_list = factory
            .new_variable_declaration_list(factory.new_node_list(vec![var_decl]), NodeFlags::Const);
        let var_stmt = factory.new_variable_statement(None, var_decl_list);

        if let Some(statement) = statement {
            self.change_tracker.insert_node_before(
                self.source_file,
                statement,
                var_stmt,
                false,
                LeadingTriviaOption::None,
            );
        }

        intersection_types.push(self.create_type_of_from_entity_name_expression(temp_name));
        new_spreads.push(create_spread(temp_name));
    }

    // codeactions_fixmissingtypeannotation.go:1089
    // finalizesVariablePart finalizes accumulated non-spread properties into a variable.
    fn finalizes_variable_part(
        &mut self,
        factory: &NodeFactory,
        name: &str,
        is_in_const_context: bool,
        statement: Option<P<Node>>,
        make_node_of_kind: &dyn Fn(Vec<P<Node>>) -> P<Node>,
        create_spread: &dyn Fn(P<Node>) -> P<Node>,
        current_variable_properties: &mut Vec<P<Node>>,
        intersection_types: &mut Vec<P<Node>>,
        new_spreads: &mut Vec<P<Node>>,
    ) {
        if !current_variable_properties.is_empty() {
            let node = make_node_of_kind(std::mem::take(current_variable_properties));
            self.make_spread_variable(
                factory,
                name,
                is_in_const_context,
                statement,
                create_spread,
                node,
                intersection_types,
                new_spreads,
            );
        }
    }

    // codeactions_fixmissingtypeannotation.go:1120
    // relativeType creates a typeof expression for a node, used in TypePrintMode.Relative.
    // Instead of spelling out the full type, returns `typeof X` for identifiers.
    // For object/array literals with spreads, decomposes into separate variables.
    fn relative_type(&mut self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_parameter_declaration(node) {
            return None;
        }
        if ast::is_shorthand_property_assignment(node) {
            return Some(self.create_type_of_from_entity_name_expression(node.name().unwrap()));
        }
        if ast::is_entity_name_expression(node) {
            return Some(self.create_type_of_from_entity_name_expression(node));
        }
        if is_const_assertion(Some(node)) {
            return self.relative_type(node.expression().unwrap());
        }
        if ast::is_array_literal_expression(node) {
            let var_decl = ast::find_ancestor_kind(node, Kind::VariableDeclaration);
            let mut part_name = "";
            if let Some(var_decl) = var_decl
                && let Some(n) = var_decl.name().filter(|n| ast::is_identifier(*n))
            {
                part_name = n.text();
            }
            return self.type_from_array_spread_elements(node, part_name);
        }
        if ast::is_object_literal_expression(node) {
            let var_decl = ast::find_ancestor_kind(node, Kind::VariableDeclaration);
            let mut part_name = "";
            if let Some(var_decl) = var_decl
                && let Some(n) = var_decl.name().filter(|n| ast::is_identifier(*n))
            {
                part_name = n.text();
            }
            return self.type_from_object_spread_assignment(node, part_name);
        }
        if ast::is_variable_declaration(node)
            && let Some(initializer) = node.initializer()
        {
            return self.relative_type(initializer);
        }
        if ast::is_conditional_expression(node) {
            let cond = node.as_conditional_expression();
            let true_type = self.relative_type(cond.when_true)?;
            let true_mutated = self.mutated_target;
            let false_type = self.relative_type(cond.when_false)?;
            self.mutated_target = true_mutated || self.mutated_target;
            let factory = self.factory();
            return Some(
                factory.new_union_type_node(factory.new_node_list(vec![true_type, false_type])),
            );
        }
        None
    }

    // codeactions_fixmissingtypeannotation.go:1172
    // typeToMinimizedReferenceType converts a type to a type node, then trims trailing
    // type arguments that match their defaults. Ported from TS's
    // services/codefixes/helpers.ts typeToMinimizedReferenceType.
    fn type_to_minimized_reference_type(
        &mut self,
        t: P<Type>,
        enclosing_decl: P<Node>,
        flags: Flags,
    ) -> Option<P<Node>> {
        let id_to_symbol: IdToSymbol = P::new(RefCell::new(FxHashMap::default()));
        // !!! When truncation tracking is supported, check if the type was truncated
        // and return factory.NewKeywordTypeNode(ast.KindAnyKeyword) instead of the truncated node.
        let mut type_node = self.checker.type_to_type_node_ex(
            t,
            Some(enclosing_decl),
            flags,
            InternalFlags::WriteComputedProps,
            Some(id_to_symbol),
        )?;
        if ast::is_type_reference_node(type_node)
            && t.object_flags().intersects(ObjectFlags::Reference)
        {
            let type_args = self.checker.get_type_arguments_exported(t);
            let node_type_args = type_node.type_arguments();
            if !type_args.is_empty() && !node_type_args.is_empty() {
                let cutoff = end_of_required_type_parameters(self.checker, t);
                if cutoff < node_type_args.len() {
                    // Trim trailing default type arguments
                    let trimmed_args = self
                        .change_tracker
                        .node_factory
                        .new_node_list(node_type_args[..cutoff].to_vec());
                    type_node = self.change_tracker.node_factory.update_type_reference_node(
                        type_node,
                        type_node.as_type_reference_node().type_name,
                        Some(trimmed_args),
                    );
                }
            }
        }
        // Convert import type references (e.g. import("./path").Name) to simple type references
        // and collect symbols that need to be imported
        let (reference_type_node, importable_symbols) =
            autoimport::try_get_auto_importable_reference_from_type_node(
                Some(type_node),
                id_to_symbol,
            );
        if let Some(reference_type_node) = reference_type_node {
            type_node = reference_type_node;
            self.symbols_to_import.extend(importable_symbols);
        }
        Some(type_node)
    }

    // codeactions_fixmissingtypeannotation.go:1262
    fn add_type_to_variable_like(&mut self, decl: P<Node>) -> String {
        let Some(type_node) = self.infer_type(decl, None) else {
            return String::new();
        };
        if let Some(decl_type) = decl.type_node() {
            self.change_tracker.replace_node(self.source_file, decl_type, type_node, None);
        } else {
            self.change_tracker.try_insert_type_annotation(self.source_file, decl, type_node);
            // Parenthesize paren-less arrow function parameters (`x => ...`) so the inserted `: T`
            // produces `(x: T) => ...` instead of the invalid `x: T => ...`. Queued after the type
            // annotation so that the `)` edit at param.End() sorts after the annotation insertion.
            if ast::is_parameter_declaration(decl)
                && let Some(parent) = decl.parent().filter(|p| ast::is_arrow_function(*p))
            {
                self.change_tracker.parenthesize_arrow_parameters(self.source_file, parent);
            }
        }
        diagnostics::ADD_ANNOTATION_OF_TYPE_0.localize(&[&type_to_string_for_diag(
            type_node,
            self.source_file,
            self.change_tracker,
        )])
    }

    // codeactions_fixmissingtypeannotation.go:1370
    // addSymbolToExistingImport finds the existing import declaration for the symbol's module
    // and adds the symbol name to the named imports.
    fn add_symbol_to_existing_import(&mut self, sym: Option<P<Symbol>>) {
        let Some(sym) = sym else {
            return;
        };
        let Some(module_symbol) = sym.parent() else {
            return;
        };

        // Find the module specifier for this symbol
        let symbol_name = sym.name();

        // Walk the source file's import declarations to find the one importing from the same module
        for &stmt in self.source_file.statements.nodes() {
            if !ast::is_import_declaration(stmt) {
                continue;
            }
            let import_decl = stmt.as_import_declaration();
            let Some(import_clause_node) = import_decl.import_clause else {
                continue;
            };

            // Check if this import is from the same module
            let Some(import_module_symbol) =
                self.checker.get_symbol_at_location_exported(import_decl.module_specifier)
            else {
                continue;
            };
            let a = self.checker.get_merged_symbol(import_module_symbol);
            let b = self.checker.get_merged_symbol(module_symbol);
            if a != b {
                continue;
            }

            // Found the matching import - add the symbol to named imports
            let import_clause = import_clause_node.as_import_clause();
            if let Some(named_bindings) =
                import_clause.named_bindings.filter(|n| ast::is_named_imports(*n))
            {
                // Add to existing named imports
                let existing_elements = named_bindings.as_named_imports().elements.nodes();
                let factory = self.factory();
                let new_specifier =
                    factory.new_import_specifier(false, None, factory.new_identifier(symbol_name));
                let mut new_elements = existing_elements.to_vec();
                new_elements.push(new_specifier);
                let new_named_imports =
                    factory.new_named_imports(factory.new_node_list(new_elements));
                let new_import_clause = factory.update_import_clause(
                    import_clause_node,
                    import_clause.phase_modifier(),
                    import_clause.name(),
                    Some(new_named_imports),
                );
                let new_import_decl = factory.update_import_declaration(
                    stmt,
                    stmt.modifiers(),
                    Some(new_import_clause),
                    import_decl.module_specifier,
                    import_decl.attributes(),
                );
                self.change_tracker.replace_node(self.source_file, stmt, new_import_decl, None);
            }
            return;
        }
    }
}

// codeactions_fixmissingtypeannotation.go:307
// needsParenthesizedExpressionForAssertion checks if an expression needs parentheses for an assertion.
fn needs_parenthesized_expression_for_assertion(node: P<Node>) -> bool {
    !ast::is_entity_name_expression(node)
        && !ast::is_call_expression(node)
        && !ast::is_object_literal_expression(node)
        && !ast::is_array_literal_expression(node)
}

// codeactions_fixmissingtypeannotation.go:312
// createAsExpression creates an `expr as Type` expression, parenthesizing if needed.
fn create_as_expression(factory: &NodeFactory, node: P<Node>, type_node: P<Node>) -> P<Node> {
    let mut node = node;
    if needs_parenthesized_expression_for_assertion(node) {
        node = factory.new_parenthesized_expression(node);
    }
    factory.new_as_expression(node, type_node)
}

// codeactions_fixmissingtypeannotation.go:486
// findExpandoFunction finds the function declaration that has expando properties assigned to it.
// isExpandoPropertyDeclarationForFix matches TS's isExpandoPropertyDeclaration which includes
// PropertyAccessExpression, ElementAccessExpression, and BinaryExpression. The shared
// ast.IsExpandoPropertyDeclaration was narrowed to BinaryExpression only for checker purposes.
fn is_expando_property_declaration_for_fix(node: Option<P<Node>>) -> bool {
    node.is_some_and(|n| {
        ast::is_property_access_expression(n)
            || ast::is_element_access_expression(n)
            || ast::is_binary_expression(n)
    })
}

// codeactions_fixmissingtypeannotation.go:490
fn find_expando_function(ch: &mut Checker, node: P<Node>) -> Option<P<Node>> {
    let expando_declaration = ast::find_ancestor_or_quit(node, |n| {
        if ast::is_statement(n) {
            return ast::FindAncestorResult::Quit;
        }
        if is_expando_property_declaration_for_fix(Some(n)) {
            return ast::FindAncestorResult::True;
        }
        ast::FindAncestorResult::False
    });

    let expando_declaration =
        expando_declaration.filter(|&d| is_expando_property_declaration_for_fix(Some(d)))?;

    let mut assignment_target = expando_declaration;
    // Some late bound expando members use the whole expression as the declaration.
    if ast::is_binary_expression(assignment_target) {
        assignment_target = assignment_target.as_binary_expression().left;
        if !is_expando_property_declaration_for_fix(Some(assignment_target)) {
            return None;
        }
    }

    let expression = if ast::is_property_access_expression(assignment_target) {
        assignment_target.as_property_access_expression().expression
    } else if ast::is_element_access_expression(assignment_target) {
        assignment_target.as_element_access_expression().expression
    } else {
        return None;
    };

    let target_type = ch.get_type_at_location(expression);

    let properties = ch.get_properties_of_type(target_type);
    let found = properties.iter().any(|&p| {
        p.value_declaration() == Some(expando_declaration)
            || p.value_declaration() == expando_declaration.parent()
    });
    if !found {
        return None;
    }

    let symbol = target_type.symbol()?;
    let func = symbol.value_declaration()?;

    if (ast::is_function_expression(func) || ast::is_arrow_function(func))
        && ast::is_variable_declaration(func.parent().unwrap())
    {
        return func.parent();
    }
    if ast::is_function_declaration(func) {
        return Some(func);
    }

    None
}

// codeactions_fixmissingtypeannotation.go:1108
// isConstAssertion checks if a node is an `as const` or `<const>` assertion.
fn is_const_assertion(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if ast::is_assertion_expression(node) {
        let type_node = node.type_node();
        return type_node.is_some_and(ast::is_const_type_reference);
    }
    false
}

// codeactions_fixmissingtypeannotation.go:1216
// endOfRequiredTypeParameters finds the number of type arguments that are
// actually required (i.e., differ from their defaults). Ported from TS's
// services/codefixes/helpers.ts endOfRequiredTypeParameters.
fn end_of_required_type_parameters(ch: &mut Checker, t: P<Type>) -> usize {
    let type_args = ch.get_type_arguments_exported(t);
    if type_args.is_empty() {
        return 0;
    }
    let Some(interface_type) = t.target().and_then(|target| target.try_as_interface_type()) else {
        return type_args.len();
    };
    let type_params = interface_type.type_parameters();
    let local_type_params = interface_type.local_type_parameters();
    let outer_count = type_params.len().saturating_sub(local_type_params.len());
    for cutoff in 0..type_args.len() {
        // Skip cutoff positions where the local type parameter has no default.
        // This matches TS's check for constraint === undefined on localTypeParameters,
        // which in practice skips type parameters without defaults (e.g. Set<T>
        // where T has no default should not have <unknown> elided).
        let Some(local_idx) =
            cutoff.checked_sub(outer_count).filter(|&index| index < local_type_params.len())
        else {
            continue;
        };
        if !type_param_has_default(local_type_params[local_idx]) {
            continue;
        }
        let filled_in = ch.fill_missing_type_arguments_exported(
            &type_args[..cutoff],
            type_params,
            cutoff,
            false,
        );
        let all_match = !(filled_in.iter().enumerate().any(|(i, &fill)| fill != type_args[i]));
        if all_match {
            return cutoff;
        }
    }
    type_args.len()
}

// codeactions_fixmissingtypeannotation.go:1250
// typeParamHasDefault checks if a type parameter has a default type declaration.
fn type_param_has_default(tp: P<Type>) -> bool {
    let Some(sym) = tp.symbol() else {
        return false;
    };
    sym.declarations().iter().any(|&decl| {
        ast::is_type_parameter_declaration(decl)
            && decl.as_type_parameter_declaration().default_type.is_some()
    })
}

// codeactions_fixmissingtypeannotation.go:1283
// typeToStringForDiag converts a type node to a string for use in diagnostic descriptions.
// It reuses the change tracker's EmitContext so that generated identifier names are resolved
// consistently with the actual code edits, and passes the source file so that the printer's
// name generator can check for conflicts with existing file-level identifiers.
fn type_to_string_for_diag(
    type_node: P<Node>,
    source_file: P<SourceFile>,
    ct: &change::Tracker,
) -> String {
    let saved_flags = ct.emit_context.emit_flags(type_node);
    ct.emit_context.set_emit_flags(type_node, saved_flags | EmitFlags::SingleLine);
    let mut p = printer::new_printer(
        printer::PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
        printer::PrintHandlers::default(),
        Some(ct.emit_context),
    );
    let (mut writer, release) = printer::get_single_line_string_writer();
    p.write(type_node, Some(source_file), &mut writer, None);
    ct.emit_context.set_emit_flags(type_node, saved_flags);
    let result = writer.string();
    release();
    if result.len() > 160 {
        // Go slices bytes; a cut through a multi-byte character becomes U+FFFD per byte when the
        // description is serialized.
        let bytes = &result.as_bytes()[..157];
        return format!("{}...", go_lossy_utf8(bytes));
    }
    result
}

fn go_lossy_utf8(bytes: &[u8]) -> String {
    let mut out = String::new();
    out.extend(bytes.utf8_chunks().flat_map(|chunk| {
        chunk.valid().chars().chain(std::iter::repeat_n('\u{FFFD}', chunk.invalid().len()))
    }));
    out
}

// codeactions_fixmissingtypeannotation.go:1307
// findAncestorWithMissingType walks up the ancestor chain to find a node that
// can have a type annotation and is missing one.
fn find_ancestor_with_missing_type(node: P<Node>) -> Option<P<Node>> {
    ast::find_ancestor(node, |n| {
        if !can_have_type_annotation_kind(n.kind()) {
            return false;
        }
        if ast::is_object_binding_pattern(n) || ast::is_array_binding_pattern(n) {
            return ast::is_variable_declaration(n.parent().unwrap());
        }
        true
    })
}

// codeactions_fixmissingtypeannotation.go:1321
// findBestFittingNode walks up from the token to find the node that best fits the diagnostic span.
fn find_best_fitting_node(node: Option<P<Node>>, span: TextRange) -> Option<P<Node>> {
    let mut node = node?;
    while node.end() < span.end() {
        // Go walks to a nil parent and then dereferences it in the next loop.
        node = node.parent().expect("nil node");
    }
    while let Some(parent) = node.parent() {
        if parent.pos() == node.pos() && parent.end() == node.end() {
            node = parent;
        } else {
            break;
        }
    }
    if ast::is_identifier(node)
        && ast::has_initializer(node.parent().unwrap())
        && node.parent().unwrap().initializer().is_some()
    {
        return node.parent().unwrap().initializer();
    }
    if ast::is_identifier(node) && ast::is_shorthand_property_assignment(node.parent().unwrap()) {
        return node.parent();
    }
    Some(node)
}

// codeactions_fixmissingtypeannotation.go:1342
// isNamedDeclarationKind matches TS's isDeclarationKind, which is narrower than Go's IsDeclaration.
// Go's IsDeclaration returns true for any node with DeclarationData (including CallExpression),
// while TS's isDeclaration only returns true for specific named declaration kinds.
fn is_named_declaration_kind(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ArrowFunction
            | Kind::BindingElement
            | Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::ClassStaticBlockDeclaration
            | Kind::Constructor
            | Kind::EnumDeclaration
            | Kind::EnumMember
            | Kind::ExportSpecifier
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::GetAccessor
            | Kind::ImportClause
            | Kind::ImportEqualsDeclaration
            | Kind::ImportSpecifier
            | Kind::InterfaceDeclaration
            | Kind::JsxAttribute
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::ModuleDeclaration
            | Kind::NamespaceExportDeclaration
            | Kind::NamespaceImport
            | Kind::NamespaceExport
            | Kind::Parameter
            | Kind::PropertyAssignment
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::SetAccessor
            | Kind::ShorthandPropertyAssignment
            | Kind::TypeAliasDeclaration
            | Kind::TypeParameter
            | Kind::VariableDeclaration
            | Kind::JSDocTypedefTag
            | Kind::JSDocCallbackTag
            | Kind::JSDocPropertyTag
            | Kind::NamedTupleMember
    )
}

// codeactions_fixmissingtypeannotation.go:1362
// isValueSignatureDeclaration checks if a node is a function-like declaration that produces a value.
fn is_value_signature_declaration(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    ast::is_function_expression(node)
        || ast::is_arrow_function(node)
        || ast::is_method_declaration(node)
        || ast::is_accessor(node)
        || ast::is_function_declaration(node)
        || ast::is_constructor_declaration(node)
}

// codeactions_fixmissingtypeannotation.go:1369
// getIdentifierNameForNode derives a meaningful variable name from a node expression.
// For property access expressions like `obj.foo`, returns "foo". Otherwise returns "newLocal".
// Ported from TS's getIdentifierForNode in services/refactors/helpers.ts.
fn get_identifier_name_for_node(node: P<Node>) -> String {
    if ast::is_property_access_expression(node) {
        let name = node.as_property_access_expression().name();
        if ast::is_identifier(name)
            && !ast::is_private_identifier(name)
            && scanner::identifier_to_keyword_kind(name) == Kind::Unknown
        {
            return name.text().to_string();
        }
    }
    "newLocal".to_string()
}
