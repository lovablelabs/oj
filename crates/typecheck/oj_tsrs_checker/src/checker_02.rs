use crate::*;
use rustc_hash::FxHashMap;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

impl Checker {
    // Return the type-only declaration node (if any) for the given alias symbol (non-transitively)
    // checker.go:2174
    pub fn get_type_only_alias_declaration(&mut self, symbol: P<Symbol>) -> Option<P<Node>> {
        if symbol.flags().intersects(SymbolFlags::Alias) {
            self.resolve_alias(symbol);
            return self.alias_symbol_links.get(symbol).type_only_declaration.get();
        }
        None
    }

    // Return the first type-only alias declaration node (if any) in the resolution chain that affects
    // the symbol for the given meaning
    // checker.go:2184
    pub(crate) fn get_type_only_alias_declaration_ex(
        &mut self,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> Option<P<Node>> {
        let mut symbol = symbol;
        while symbol.flags().intersects(SymbolFlags::Alias) && !symbol.flags().intersects(meaning) {
            let resolved = self.resolve_alias(symbol);
            let links = self.alias_symbol_links.get(symbol);
            if let Some(type_only_declaration) = links.type_only_declaration.get() {
                return Some(type_only_declaration);
            }
            symbol = resolved;
        }
        None
    }

    // checker.go:2196
    pub(crate) fn get_immediate_aliased_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        assert!(symbol.flags().intersects(SymbolFlags::Alias), "Should only get Alias here.");
        let links = self.alias_symbol_links.get(symbol);
        if links.immediate_target.get().is_none() {
            let node = Self::get_declaration_of_alias_symbol(symbol);
            let Some(node) = node else {
                panic!("Unexpected nil in getImmediateAliasedSymbol");
            };
            let target = self.get_target_of_alias_declaration(Some(node));
            links.immediate_target.set(target);
        }
        links.immediate_target.get()
    }

    // checker.go:2209
    pub(crate) fn add_type_only_declaration_related_info(
        diagnostic: P<Diagnostic>,
        type_only_declaration: Option<P<Node>>,
        name: &str,
    ) -> P<Diagnostic> {
        let Some(type_only_declaration) = type_only_declaration else {
            return diagnostic;
        };
        let is_export = ast::is_export_specifier(type_only_declaration)
            || ast::is_export_declaration(type_only_declaration)
            || ast::is_namespace_export(type_only_declaration);
        diagnostic.add_related_info(new_diagnostic_for_node(
            Some(type_only_declaration),
            Some(if_else(
                is_export,
                &diagnostics::X_0_WAS_EXPORTED_HERE,
                &diagnostics::X_0_WAS_IMPORTED_HERE,
            )),
            &[&name],
        ))
    }

    // checker.go:2217
    pub(crate) fn get_symbol(
        &mut self,
        symbols: Option<P<SymbolTable>>,
        name: &str,
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        if meaning.intersects(SymbolFlags::All) {
            let symbol =
                symbols.and_then(|symbols| symbols.lookup(name)).map(|s| self.get_merged_symbol(s));
            if let Some(symbol) = symbol {
                if symbol.flags().intersects(meaning) {
                    return Some(symbol);
                }
                if symbol.flags().intersects(SymbolFlags::Alias) {
                    let target_flags = self.get_symbol_flags(symbol);
                    // `targetFlags` will be `SymbolFlags.All` if an error occurred in alias resolution; this avoids cascading errors
                    if target_flags.intersects(meaning) {
                        return Some(symbol);
                    }
                }
            }
        }
        // return nil if we can't find a symbol
        None
    }

    // checker.go:2237
    pub(crate) fn check_source_file(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
        check_unused: bool,
    ) {
        let saved_checking_file = self.checking_file.replace(source_file);
        if self.census_on() {
            let span =
                self.census_begin(crate::workcensus::Cat::File, || crate::workcensus::CKey::None);
            self.check_source_file_worker(ctx, source_file, check_unused);
            let t = self.census_end(span).unwrap();
            self.census.as_mut().unwrap().file_wall_ns += t.incl_ns;
        } else {
            self.check_source_file_worker(ctx, source_file, check_unused);
        }
        self.flush_too_complex_reports();
        self.checking_file = saved_checking_file;
    }

    fn check_source_file_worker(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
        check_unused: bool,
    ) {
        self.ctx = Some(ctx.clone());
        let links = self.source_file_links.get(source_file);
        if !links.type_checked.get() {
            // tsrs-only: TSRS_TRACE_UNION_REDUCTION (uniontrace.rs) prints each checker's file order.
            if crate::uniontrace::enabled() {
                crate::uniontrace::report_check_file(self, source_file, None);
            }
            self.run_deferred_type_argument_checks(source_file);
            // Grammar checking
            self.check_grammar_source_file(source_file);
            self.renamed_binding_elements_in_types = Vec::new();
            if self.statements_checked_in_pieces.take() != Some(source_file) {
                self.check_source_elements(source_file.statements.nodes());
            }
            self.check_deferred_nodes(source_file);
            if ast::is_external_or_common_js_module(source_file) {
                self.check_external_module_exports(source_file.as_node());
                self.register_for_unused_identifiers_check(source_file.as_node());
            }
            if !source_file.is_declaration_file() && !self.is_canceled() {
                self.check_unused_renamed_binding_elements();
            }
            self.produce_deferred_diagnostics();
            self.reported_unreachable_nodes.clear();
            links.type_checked.set(true);
        }
        if check_unused && !links.unused_checked.get() {
            // The unused identifiers check relies on a full type check having first been performed
            if !source_file.is_declaration_file() && !self.is_canceled() {
                let identifier_check_nodes = links.identifier_check_nodes.borrow().clone();
                self.check_unused_identifiers(&identifier_check_nodes);
            }
            links.unused_checked.set(true);
        }
        if self.is_canceled() {
            self.was_canceled = true;
        }
        self.ctx = None;
    }

    /// tsrs-only (notes/perf-next-heavy-files.md): checks `statements` of a declaration file as `check_source_file` would
    /// (the statements, then the nodes they deferred and the deferred diagnostics) without the file-level steps, and
    /// returns this checker's diagnostics and suggestions for the file. The compiler's checker pool splits a heavy
    /// declaration file over several checkers this way; the checker that owns the file merges the other pieces'
    /// diagnostics (`add_piece_diagnostics`), then runs `check_source_file`, which skips the statements.
    pub fn check_source_file_piece(
        &mut self,
        ctx: &Context,
        source_file: P<SourceFile>,
        statements: std::ops::Range<usize>,
    ) -> (Vec<P<Diagnostic>>, Vec<P<Diagnostic>>) {
        debug_assert!(source_file.is_declaration_file());
        self.check_not_canceled();
        let saved_checking_file = self.checking_file.replace(source_file);
        self.ctx = Some(ctx.clone());
        // Only what this piece adds is handed back: this checker may already hold diagnostics located in the file from
        // checking its own files (a symbol-level check reports at every declaration of the symbol); those the owner
        // finds for itself when it checks the file, exactly as it does for a file that is not split.
        let before: rustc_hash::FxHashSet<P<Diagnostic>> =
            self.diagnostics.get_diagnostics_for_file(source_file).into_iter().collect();
        let suggestions_before: rustc_hash::FxHashSet<P<Diagnostic>> =
            self.suggestion_diagnostics.get_diagnostics_for_file(source_file).into_iter().collect();
        if !self.source_file_links.get(source_file).type_checked.get() {
            if crate::uniontrace::enabled() {
                crate::uniontrace::report_check_file(self, source_file, Some(&statements));
            }
            self.check_source_elements(&source_file.statements.nodes()[statements]);
            self.check_deferred_nodes(source_file);
            self.produce_deferred_diagnostics();
            self.reported_unreachable_nodes.clear();
        }
        if self.is_canceled() {
            self.was_canceled = true;
        }
        self.flush_too_complex_reports();
        self.ctx = None;
        self.checking_file = saved_checking_file;
        let added = |all: Vec<P<Diagnostic>>, before: &rustc_hash::FxHashSet<P<Diagnostic>>| {
            all.into_iter().filter(|d| !before.contains(d)).collect::<Vec<_>>()
        };
        (
            added(self.diagnostics.get_diagnostics_for_file(source_file), &before),
            added(
                self.suggestion_diagnostics.get_diagnostics_for_file(source_file),
                &suggestions_before,
            ),
        )
    }

    /// tsrs-only: adds the diagnostics another checker found while checking pieces of `source_file`
    /// (`check_source_file_piece`), and marks the file's statements as checked: the next `check_source_file` of it
    /// runs only the file-level steps. The collections deduplicate and sort, so the file's diagnostics are the union.
    pub fn add_piece_diagnostics(
        &mut self,
        source_file: P<SourceFile>,
        diagnostics: &[P<Diagnostic>],
        suggestions: &[P<Diagnostic>],
    ) {
        for &d in diagnostics {
            self.diagnostics.add(d);
        }
        for &d in suggestions {
            self.suggestion_diagnostics.add(d);
        }
        self.statements_checked_in_pieces = Some(source_file);
    }

    /// tsrs-only: the diagnostics this checker holds for `source_file`, without checking anything (the split check's
    /// shadow mode compares the owner's whole-file result with the merged pieces).
    pub fn file_diagnostics_so_far(&mut self, source_file: P<SourceFile>) -> Vec<P<Diagnostic>> {
        self.diagnostics.get_diagnostics_for_file(source_file)
    }

    // checker.go:2273
    pub(crate) fn check_source_elements(&mut self, nodes: &[P<Node>]) {
        for &node in nodes {
            if self.is_canceled() {
                break;
            }
            self.check_source_element(Some(node));
        }
    }

    // checker.go:2282
    pub(crate) fn check_source_element(&mut self, node: Option<P<Node>>) -> bool {
        if let Some(node) = node {
            let save_current_node = self.current_node;
            let save_within_unreachable_code = self.within_unreachable_code;
            self.current_node = Some(node);
            self.instantiation_count = 0;
            self.check_source_element_worker(node);
            self.current_node = save_current_node;
            self.within_unreachable_code = save_within_unreachable_code;
            self.end_instantiation_count();
        }
        false
    }

    // checker.go:2295
    fn check_source_element_worker(&mut self, node: P<Node>) {
        for &jsdoc in node.eager_jsdoc(None) {
            self.check_jsdoc_comments(jsdoc);
            if let Some(tags) = jsdoc.as_jsdoc().tags {
                for &tag in tags.nodes() {
                    self.check_jsdoc_comments(tag);
                }
            }
        }

        if !self.within_unreachable_code
            && self.compiler_options.allow_unreachable_code != Tristate::True
            && self.check_source_element_unreachable(node)
        {
            self.within_unreachable_code = true;
        }

        match node.kind() {
            Kind::TypeParameter => self.check_type_parameter(node),
            Kind::Parameter => self.check_parameter(node),
            Kind::PropertyDeclaration => self.check_property_declaration(node),
            Kind::PropertySignature => self.check_property_signature(node),
            Kind::ConstructorType
            | Kind::FunctionType
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::IndexSignature => {
                self.check_signature_declaration(node);
            }
            Kind::MethodDeclaration | Kind::MethodSignature => self.check_method_declaration(node),
            Kind::ClassStaticBlockDeclaration => self.check_class_static_block_declaration(node),
            Kind::Constructor => self.check_constructor_declaration(node),
            Kind::GetAccessor | Kind::SetAccessor => self.check_accessor_declaration(node),
            Kind::TypeReference => self.check_type_reference_node(node),
            Kind::TypePredicate => self.check_type_predicate(node),
            Kind::TypeQuery => self.check_type_query(node),
            Kind::TypeLiteral => self.check_type_literal(node),
            Kind::ArrayType => self.check_array_type(node),
            Kind::TupleType => self.check_tuple_type(node),
            Kind::UnionType | Kind::IntersectionType => self.check_union_or_intersection_type(node),
            Kind::ParenthesizedType | Kind::OptionalType | Kind::RestType => {
                node.for_each_child(&mut |n| self.check_source_element(Some(n)));
            }
            Kind::ThisType => self.check_this_type(node),
            Kind::TypeOperator => self.check_type_operator(node),
            Kind::ConditionalType => self.check_conditional_type(node),
            Kind::InferType => self.check_infer_type(node),
            Kind::TemplateLiteralType => self.check_template_literal_type(node),
            Kind::ImportType => self.check_import_type(node),
            Kind::NamedTupleMember => self.check_named_tuple_member(node),
            Kind::IndexedAccessType => self.check_indexed_access_type(node),
            Kind::MappedType => self.check_mapped_type(node),
            Kind::FunctionDeclaration => self.check_function_declaration(node),
            Kind::Block | Kind::ModuleBlock => self.check_block(node),
            Kind::VariableStatement => self.check_variable_statement(node),
            Kind::ExpressionStatement => self.check_expression_statement(node),
            Kind::IfStatement => self.check_if_statement(node),
            Kind::DoStatement => self.check_do_statement(node),
            Kind::WhileStatement => self.check_while_statement(node),
            Kind::ForStatement => self.check_for_statement(node),
            Kind::ForInStatement => self.check_for_in_statement(node),
            Kind::ForOfStatement => self.check_for_of_statement(node),
            Kind::ContinueStatement | Kind::BreakStatement => {
                self.check_break_or_continue_statement(node);
            }
            Kind::ReturnStatement => self.check_return_statement(node),
            Kind::WithStatement => self.check_with_statement(node),
            Kind::SwitchStatement => self.check_switch_statement(node),
            Kind::LabeledStatement => self.check_labeled_statement(node),
            Kind::ThrowStatement => self.check_throw_statement(node),
            Kind::TryStatement => self.check_try_statement(node),
            Kind::VariableDeclaration => self.check_variable_declaration(node),
            Kind::BindingElement => self.check_binding_element(node),
            Kind::ClassDeclaration => self.check_class_declaration(node),
            Kind::InterfaceDeclaration => self.check_interface_declaration(node),
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
                self.check_type_alias_declaration(node);
            }
            Kind::EnumDeclaration => self.check_enum_declaration(node),
            Kind::EnumMember => self.check_enum_member(node),
            Kind::ModuleDeclaration => self.check_module_declaration(node),
            Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                self.check_import_declaration(node);
            }
            Kind::ImportEqualsDeclaration => self.check_import_equals_declaration(node),
            Kind::ExportDeclaration => self.check_export_declaration(node),
            Kind::ExportAssignment => self.check_export_assignment(node),
            Kind::EmptyStatement | Kind::DebuggerStatement => {
                self.check_grammar_statement_in_ambient_context(node);
            }
            Kind::MissingDeclaration => self.check_missing_declaration(node),
            Kind::JSDocNonNullableType
            | Kind::JSDocNullableType
            | Kind::JSDocAllType
            | Kind::JSDocTypeLiteral => self.check_jsdoc_type(node),
            _ => {}
        }
    }

    // checker.go:2433
    fn check_source_element_unreachable(&mut self, node: P<Node>) -> bool {
        if !ast::is_potentially_executable_node(node) {
            return false;
        }

        if self.reported_unreachable_nodes.has(&node) {
            return true;
        }

        if !self.is_source_element_unreachable(node) {
            return false;
        }

        self.reported_unreachable_nodes.add(node);

        let source_file = ast::get_source_file_of_node(node).unwrap();

        let mut start_node = node;
        let mut end_node = node;

        let parent = node.parent().unwrap();
        if parent.can_have_statements() {
            let statements = parent.statements();
            if let Some(offset) = statements.iter().position(|&s| s == node) {
                // Scan backwards to find the first unreachable unreported node;
                // this may happen when producing region diagnostics where not all nodes
                // will have been visited.
                // TODO: enable this code once we support region diagnostics again.
                let first = offset;

                let mut last = offset;
                for (i, element) in statements.iter().enumerate().skip(offset + 1) {
                    let next_node = *element;
                    if !ast::is_potentially_executable_node(next_node)
                        || !self.is_source_element_unreachable(next_node)
                    {
                        break;
                    }
                    last = i;
                    self.reported_unreachable_nodes.add(next_node);
                }

                start_node = statements[first];
                end_node = statements[last];
            }
        }

        let start = tsrs_scanner::get_token_pos_of_node(
            start_node,
            source_file,
            false, /*includeJSDoc*/
        );

        let diagnostic = ast::new_diagnostic(
            Some(source_file),
            new_text_range(
                start,
                end_node.end().expect_source("parsed nodes have source positions"),
            ),
            &diagnostics::UNREACHABLE_CODE_DETECTED,
            &[],
        );
        self.add_error_or_suggestion(
            self.compiler_options.allow_unreachable_code == Tristate::False,
            diagnostic,
        );

        true
    }

    // checker.go:2494
    fn is_source_element_unreachable(&mut self, node: P<Node>) -> bool {
        // Precondition: ast.IsPotentiallyExecutableNode is true
        if node.flags().intersects(NodeFlags::Unreachable) {
            // The binder has determined that this code is unreachable.
            // Ignore const enums unless preserveConstEnums is set.
            match node.kind() {
                Kind::EnumDeclaration => {
                    return !ast::is_enum_const(node)
                        || self.compiler_options.should_preserve_const_enums();
                }
                Kind::ModuleDeclaration => {
                    return ast::is_instantiated_module(
                        node,
                        self.compiler_options.should_preserve_const_enums(),
                    );
                }
                _ => return true,
            }
        } else if let Some(flow_node) = {
            assert!(node.has_flow_node_data());
            node.flow_node()
        } {
            // For code the binder doesn't know is unreachable, use control flow / types.
            return !self.is_reachable_flow_node(flow_node);
        }
        false
    }

    // Function and class expression bodies are checked after all statements in the enclosing body. This is
    // to ensure constructs like the following are permitted:
    //
    //	const foo = function () {
    //	   const s = foo();
    //	   return "hello";
    //	}
    //
    // Here, performing a full type check of the body of the function expression whilst in the process of
    // determining the type of foo would cause foo to be given type any because of the recursive reference.
    // Delaying the type check of the body ensures foo has been assigned a type.
    // checker.go:2525
    pub(crate) fn check_node_deferred(&mut self, node: P<Node>) {
        let enclosing_file = ast::get_source_file_of_node(node).unwrap();
        let links = self.source_file_links.get(enclosing_file);
        if !links.type_checked.get() {
            links.deferred_nodes.borrow_mut().insert(node);
        }
    }

    // checker.go:2533
    fn check_deferred_nodes(&mut self, context: P<SourceFile>) {
        let links = self.source_file_links.get(context);
        // Go's OrderedSet.Values() enumerates items added during iteration.
        let mut i = 0;
        loop {
            let Some(&node) = links.deferred_nodes.borrow().get_index(i) else {
                break;
            };
            if self.is_canceled() {
                break;
            }
            self.check_deferred_node(node);
            i += 1;
        }
        *links.deferred_nodes.borrow_mut() = OrderedSet::default();
    }

    // checker.go:2544
    fn check_deferred_node(&mut self, node: P<Node>) {
        let save_current_node = self.current_node;
        self.current_node = Some(node);
        self.instantiation_count = 0;
        match node.kind() {
            Kind::CallExpression
            | Kind::NewExpression
            | Kind::TaggedTemplateExpression
            | Kind::Decorator
            | Kind::JsxOpeningElement => {
                // These node kinds are deferred checked when overload resolution fails. To save on work,
                // we ensure the arguments are checked just once in a deferred way.
                self.resolve_untyped_call(node);
            }
            Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::MethodDeclaration
            | Kind::MethodSignature => {
                self.check_function_expression_or_object_literal_method_deferred(node);
            }
            Kind::GetAccessor | Kind::SetAccessor => self.check_accessor_declaration(node),
            Kind::ClassExpression => self.check_class_expression_deferred(node),
            Kind::TypeParameter => self.check_type_parameter_deferred(node),
            Kind::JsxSelfClosingElement => self.check_jsx_self_closing_element_deferred(node),
            Kind::JsxElement => self.check_jsx_element_deferred(node),
            Kind::TypeAssertionExpression | Kind::AsExpression => {
                self.check_assertion_deferred(node);
            }
            Kind::VoidExpression => {
                self.check_expression(node.expression().unwrap());
            }
            Kind::BinaryExpression => {
                if ast::is_instance_of_expression(node) {
                    self.resolve_untyped_call(node);
                }
            }
            Kind::ObjectLiteralExpression | Kind::JsxAttributes => {
                self.check_contextual_deprecations(node);
            }
            _ => {}
        }
        self.current_node = save_current_node;
    }

    // checker.go:2582
    fn check_jsdoc_comments(&mut self, node: P<Node>) {
        for &comment in node.comments() {
            self.check_jsdoc_comment(comment);
        }
    }

    // checker.go:2588
    fn check_jsdoc_comment(&mut self, node: P<Node>) {
        // This performs minimal checking of JSDoc nodes to ensure that @link references to entities are recorded
        // for purposes of checking unused identifiers.
        match node.kind() {
            Kind::JSDocLink | Kind::JSDocLinkCode | Kind::JSDocLinkPlain => {
                self.resolve_jsdoc_member_name(node.name());
            }
            _ => {}
        }
    }

    // checker.go:2597
    pub(crate) fn resolve_jsdoc_member_name(&mut self, name: Option<P<Node>>) -> Option<P<Symbol>> {
        if let Some(name) = name
            && ast::is_entity_name(name)
        {
            let meaning = SymbolFlags::Type | SymbolFlags::Namespace | SymbolFlags::Value;
            if let Some(symbol) = self.resolve_entity_name(
                name,
                meaning,
                true, /*ignoreErrors*/
                true, /*dontResolveAlias*/
                ast::get_host_signature_from_jsdoc(name),
            ) {
                return Some(symbol);
            }
            if ast::is_qualified_name(name)
                && let Some(symbol) =
                    self.resolve_jsdoc_member_name(Some(name.as_qualified_name().left))
            {
                let mut t: Option<P<Type>> = None;
                if symbol.flags().intersects(SymbolFlags::Value) {
                    let symbol_type = self.get_type_of_symbol(symbol);
                    let proto = self.get_property_of_type(symbol_type, "prototype");
                    if let Some(proto) = proto {
                        t = Some(self.get_type_of_symbol(proto));
                    }
                }
                let t = match t {
                    Some(t) => t,
                    None => self.get_declared_type_of_symbol(symbol),
                };
                return self.get_property_of_type(t, name.as_qualified_name().right.text());
            }
        }
        None
    }

    // checker.go:2622
    fn check_jsdoc_type(&mut self, node: P<Node>) {
        self.check_jsdoc_type_is_in_js_file(node);
        node.for_each_child(&mut |n| self.check_source_element(Some(n)));
    }

    // checker.go:2627
    fn check_jsdoc_type_is_in_js_file(&mut self, node: P<Node>) {
        if !ast::is_in_js_file(node) {
            if ast::is_jsdoc_non_nullable_type(node) || ast::is_jsdoc_nullable_type(node) {
                let token = if_else(ast::is_jsdoc_non_nullable_type(node), "!", "?");
                let postfix = node.pos() == node.type_node().unwrap().pos();
                let message = if_else(
                    postfix,
                    &diagnostics::X_0_AT_THE_END_OF_A_TYPE_IS_NOT_VALID_TYPE_SCRIPT_SYNTAX_DID_YOU_MEAN_TO_WRITE_1,
                    &diagnostics::X_0_AT_THE_START_OF_A_TYPE_IS_NOT_VALID_TYPE_SCRIPT_SYNTAX_DID_YOU_MEAN_TO_WRITE_1,
                );
                let mut t = self.get_type_from_type_node(node.type_node().unwrap());
                if ast::is_jsdoc_nullable_type(node) && t != self.never_type && t != self.void_type
                {
                    t = self.get_nullable_type(
                        t,
                        if_else(postfix, TypeFlags::Undefined, TypeFlags::Nullable),
                    );
                }
                let t_str = self.type_to_string_exported(t);
                self.grammar_error_on_node(node, message, &[&token, &t_str]);
            } else {
                self.grammar_error_on_node(
                    node,
                    &diagnostics::JSDOC_TYPES_CAN_ONLY_BE_USED_INSIDE_DOCUMENTATION_COMMENTS,
                    &[],
                );
            }
        }
    }

    // checker.go:2646
    pub(crate) fn check_type_parameter(&mut self, node: P<Node>) {
        // Grammar Checking
        self.check_grammar_modifiers(node);
        if let Some(expr) = node.as_type_parameter_declaration().expression {
            self.grammar_error_on_first_token(expr, &diagnostics::TYPE_EXPECTED, &[]);
        }
        let tp_node = node.as_type_parameter_declaration();
        self.check_source_element(tp_node.constraint);
        self.check_source_element(tp_node.default_type);
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let type_parameter = self.get_declared_type_of_type_parameter(symbol);
        // Resolve base constraint to reveal circularity errors
        self.get_base_constraint_of_type(type_parameter);
        if self.get_resolved_type_parameter_default(type_parameter) == self.circular_constraint_type
        {
            let s = self.type_to_string_exported(type_parameter);
            self.error(
                tp_node.default_type,
                &diagnostics::TYPE_PARAMETER_0_HAS_A_CIRCULAR_DEFAULT,
                &[&s],
            );
        }
        let constraint_type = self.get_constraint_of_type_parameter(type_parameter);
        let default_type = self.get_default_from_type_parameter(type_parameter);
        if let (Some(constraint_type), Some(default_type)) = (constraint_type, default_type) {
            let instantiated = self.instantiate_type(
                constraint_type,
                Some(new_simple_type_mapper(type_parameter, default_type)),
            );
            let target = self.get_type_with_this_argument(instantiated, Some(default_type), false);
            self.check_type_assignable_to(
                default_type,
                target,
                tp_node.default_type,
                Some(&diagnostics::TYPE_0_DOES_NOT_SATISFY_THE_CONSTRAINT_1),
            );
        }
        self.check_type_name_is_reserved(
            node.name().unwrap(),
            &diagnostics::TYPE_PARAMETER_NAME_CANNOT_BE_0,
        );
        self.check_node_deferred(node);
    }

    // checker.go:2670
    fn check_type_parameter_deferred(&mut self, node: P<Node>) {
        let parent = node.parent().unwrap();
        if ast::is_interface_declaration(parent)
            || ast::is_class_like(parent)
            || ast::is_type_or_js_type_alias_declaration(parent)
        {
            let symbol = self.get_symbol_of_declaration(node).unwrap();
            let type_parameter = self.get_declared_type_of_type_parameter(symbol);
            let modifiers = Self::get_type_parameter_modifiers(type_parameter)
                & (ModifierFlags::In | ModifierFlags::Out);
            if !modifiers.is_empty() {
                let symbol = self.get_symbol_of_declaration(parent).unwrap();
                if ast::is_type_or_js_type_alias_declaration(parent)
                    && !self
                        .get_declared_type_of_symbol(symbol)
                        .object_flags()
                        .intersects(ObjectFlags::Anonymous | ObjectFlags::Mapped)
                {
                    self.error(Some(node), &diagnostics::VARIANCE_ANNOTATIONS_ARE_ONLY_SUPPORTED_IN_TYPE_ALIASES_FOR_OBJECT_FUNCTION_CONSTRUCTOR_AND_MAPPED_TYPES, &[]);
                } else if modifiers == ModifierFlags::In || modifiers == ModifierFlags::Out {
                    let source = self.create_marker_type(
                        symbol,
                        type_parameter,
                        if_else(
                            modifiers == ModifierFlags::Out,
                            self.marker_sub_type_for_check,
                            self.marker_super_type_for_check,
                        ),
                    );
                    let target = self.create_marker_type(
                        symbol,
                        type_parameter,
                        if_else(
                            modifiers == ModifierFlags::Out,
                            self.marker_super_type_for_check,
                            self.marker_sub_type_for_check,
                        ),
                    );
                    let save_variance_type_parameter = type_parameter;
                    self.variance_type_parameter = Some(type_parameter);
                    self.check_type_assignable_to(source, target, Some(node), Some(&diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_AS_IMPLIED_BY_VARIANCE_ANNOTATION));
                    self.variance_type_parameter = Some(save_variance_type_parameter);
                }
            }
        }
    }

    // checker.go:2693
    pub(crate) fn should_check_erasable_syntax(&self, node: P<Node>) -> bool {
        self.compiler_options.erasable_syntax_only.is_true() && !ast::is_in_js_file(node)
    }

    // checker.go:2697
    fn check_parameter(&mut self, node: P<Node>) {
        // Grammar checking
        // It is a SyntaxError if the Identifier "eval" or the Identifier "arguments" occurs as the
        // Identifier in a PropertySetParameterList of a PropertyAssignment that is contained in strict code
        // or if its FunctionBody is strict code(11.1.5).
        self.check_grammar_modifiers(node);
        self.check_variable_like_declaration(node);
        let fn_ = ast::get_containing_function(node);
        let mut param_name = "";
        if let Some(name) = node.name()
            && ast::is_identifier(name)
        {
            param_name = name.text();
        }
        if ast::has_syntactic_modifier(node, ModifierFlags::ParameterPropertyModifier) {
            if self.should_check_erasable_syntax(node) {
                self.error(
                    Some(node),
                    &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                    &[],
                );
            }
            if !(ast::is_constructor_declaration(fn_.unwrap())
                && ast::node_is_present(fn_.unwrap().body()))
            {
                self.error(Some(node), &diagnostics::A_PARAMETER_PROPERTY_IS_ONLY_ALLOWED_IN_A_CONSTRUCTOR_IMPLEMENTATION, &[]);
            }
            if ast::is_constructor_declaration(fn_.unwrap()) && param_name == "constructor" {
                self.error(
                    node.name(),
                    &diagnostics::X_CONSTRUCTOR_CANNOT_BE_USED_AS_A_PARAMETER_PROPERTY_NAME,
                    &[],
                );
            }
        }
        if node.initializer().is_none()
            && is_optional_declaration(node)
            && ast::is_binding_pattern(node.name().unwrap())
            && fn_.unwrap().body().is_some()
        {
            self.error(Some(node), &diagnostics::A_BINDING_PATTERN_PARAMETER_CANNOT_BE_OPTIONAL_IN_AN_IMPLEMENTATION_SIGNATURE, &[]);
        }
        if param_name == "this" || param_name == "new" {
            let fn_ = fn_.unwrap();
            if fn_.parameters().iter().position(|&p| p == node) != Some(0) {
                self.error(
                    Some(node),
                    &diagnostics::A_0_PARAMETER_MUST_BE_THE_FIRST_PARAMETER,
                    &[&param_name],
                );
            }
            if ast::is_constructor_declaration(fn_)
                || ast::is_construct_signature_declaration(fn_)
                || ast::is_constructor_type_node(fn_)
            {
                self.error(
                    Some(node),
                    &diagnostics::A_CONSTRUCTOR_CANNOT_HAVE_A_THIS_PARAMETER,
                    &[],
                );
            }
            if ast::is_arrow_function(fn_) {
                self.error(
                    Some(node),
                    &diagnostics::AN_ARROW_FUNCTION_CANNOT_HAVE_A_THIS_PARAMETER,
                    &[],
                );
            }
            if ast::is_accessor(fn_) {
                self.error(
                    Some(node),
                    &diagnostics::X_GET_AND_SET_ACCESSORS_CANNOT_DECLARE_THIS_PARAMETERS,
                    &[],
                );
            }
        }
        // Only check rest parameter type if it's not a binding pattern. Since binding patterns are
        // not allowed in a rest parameter, we already have an error from checkGrammarParameterList.
        if has_dot_dot_dot_token(node) && !ast::is_binding_pattern(node.name().unwrap()) && {
            let symbol_type = self.get_type_of_symbol(node.symbol().unwrap());
            let reduced = self.get_reduced_type(symbol_type);
            !self.is_type_assignable_to(reduced, self.any_readonly_array_type)
        } {
            self.error(Some(node), &diagnostics::A_REST_PARAMETER_MUST_BE_OF_AN_ARRAY_TYPE, &[]);
        }
    }

    // checker.go:2744
    fn check_property_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        if !self.check_grammar_modifiers(node) && !self.check_grammar_property(node) {
            self.check_grammar_computed_property_name(node.name().unwrap());
        }
        self.check_variable_like_declaration(node);
        self.set_node_links_for_private_identifier_scope(node);
        // property signatures already report "initializer not allowed in ambient context" elsewhere
        if ast::has_syntactic_modifier(node, ModifierFlags::Abstract)
            && ast::is_property_declaration(node)
            && node.initializer().is_some()
        {
            let name = tsrs_scanner::declaration_name_to_string(node.name());
            self.error(
                Some(node),
                &diagnostics::PROPERTY_0_CANNOT_HAVE_AN_INITIALIZER_BECAUSE_IT_IS_MARKED_ABSTRACT,
                &[&name],
            );
        }
    }

    // checker.go:2759
    fn check_property_signature(&mut self, node: P<Node>) {
        if ast::is_private_identifier(node.name().unwrap()) {
            self.error(
                Some(node),
                &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                &[],
            );
        }
        self.check_property_declaration(node);
    }

    // checker.go:2766
    pub(crate) fn check_signature_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        match node.kind() {
            Kind::IndexSignature => {
                self.check_grammar_index_signature(node);
            }
            Kind::FunctionType
            | Kind::FunctionDeclaration
            | Kind::ConstructorType
            | Kind::CallSignature
            | Kind::Constructor
            | Kind::ConstructSignature => {
                self.check_grammar_function_like_declaration(node);
            }
            _ => {}
        }
        let function_flags = ast::get_function_flags(Some(node));
        if !function_flags.intersects(FunctionFlags::Invalid) {
            // Async generators prior to ES2018 require the __await and __asyncGenerator helpers
            if (function_flags & FunctionFlags::AsyncGenerator) == FunctionFlags::AsyncGenerator
                && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.async_generators
            {
                self.check_external_emit_helpers(node, ExternalEmitHelpers::AsyncGeneratorIncludes);
            }
            if (function_flags & FunctionFlags::AsyncGenerator) == FunctionFlags::Async
                && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.async_functions
            {
                self.check_external_emit_helpers(node, ExternalEmitHelpers::Awaiter);
            }
        }
        self.check_type_parameters(node.type_parameters());
        self.check_unmatched_jsdoc_parameters(node);
        self.check_source_elements(node.parameters());
        let return_type_node = node.type_node();
        if let Some(return_type_node) = return_type_node {
            self.check_source_element(Some(return_type_node));
        }
        if self.no_implicit_any && return_type_node.is_none() {
            match node.kind() {
                Kind::ConstructSignature => {
                    self.error(Some(node), &diagnostics::CONSTRUCT_SIGNATURE_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_ANY_RETURN_TYPE, &[]);
                }
                Kind::CallSignature => {
                    self.error(Some(node), &diagnostics::CALL_SIGNATURE_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_ANY_RETURN_TYPE, &[]);
                }
                _ => {}
            }
        }
        if let Some(return_type_node) = return_type_node {
            if (function_flags & (FunctionFlags::Invalid | FunctionFlags::Generator))
                == FunctionFlags::Generator
            {
                let return_type = self.get_type_from_type_node(return_type_node);
                if return_type == self.void_type {
                    self.error(
                        Some(return_type_node),
                        &diagnostics::A_GENERATOR_CANNOT_HAVE_A_VOID_TYPE_ANNOTATION,
                        &[],
                    );
                } else {
                    self.check_generator_instantiation_assignability_to_return_type(
                        return_type,
                        function_flags,
                        Some(return_type_node),
                    );
                }
            } else if (function_flags & FunctionFlags::AsyncGenerator) == FunctionFlags::Async {
                self.check_async_function_return_type(node, return_type_node);
            }
        }
        if !ast::is_index_signature_declaration(node) {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // Checks the return type of an async function to ensure it is a compatible
    // Promise implementation.
    //
    // This checks that an async function has a valid Promise-compatible return type.
    // An async function has a valid Promise-compatible return type if the resolved value
    // of the return type has a construct signature that takes in an `initializer` function
    // that in turn supplies a `resolve` function as one of its arguments and results in an
    // object with a callable `then` signature.
    // checker.go:2825
    fn check_async_function_return_type(&mut self, node: P<Node>, return_type_node: P<Node>) {
        let return_type = self.get_type_from_type_node(return_type_node);
        if self.is_error_type(return_type) {
            return;
        }
        let global_promise_type = self.get_global_promise_type_checked();
        if global_promise_type != self.empty_generic_type
            && !Self::is_reference_to_type(Some(return_type), global_promise_type)
        {
            // The promise type was not a valid type reference to the global promise type, so we
            // report an error and return the unknown type.
            let awaited = self.get_awaited_type_no_alias(return_type).unwrap_or(self.void_type);
            let s = self.type_to_string_exported(awaited);
            self.error(
                Some(return_type_node),
                &diagnostics::THE_RETURN_TYPE_OF_AN_ASYNC_FUNCTION_OR_METHOD_MUST_BE_THE_GLOBAL_PROMISE_T_TYPE_DID_YOU_MEAN_TO_WRITE_PROMISE_0,
                &[&s],
            );
            return;
        }
        self.check_awaited_type(
            return_type,
            false, /*withAlias*/
            node,
            &diagnostics::THE_RETURN_TYPE_OF_AN_ASYNC_FUNCTION_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER,
        );
    }

    // checker.go:2840
    fn check_method_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        if !self.check_grammar_method(node) {
            self.check_grammar_computed_property_name(node.name().unwrap());
            if ast::is_method_declaration(node)
                && node.as_method_declaration().asterisk_token().is_some()
                && ast::is_identifier(node.name().unwrap())
                && node.name().unwrap().text() == "constructor"
            {
                self.error(
                    node.name(),
                    &diagnostics::CLASS_CONSTRUCTOR_MAY_NOT_BE_A_GENERATOR,
                    &[],
                );
            }
        }
        // Grammar checking for modifiers is done inside the function checkGrammarFunctionLikeDeclaration
        self.check_function_or_method_declaration(node);
        // method signatures already report "implementation not allowed in ambient context" elsewhere
        if ast::has_syntactic_modifier(node, ModifierFlags::Abstract)
            && ast::is_method_declaration(node)
            && node.body().is_some()
        {
            let name = tsrs_scanner::declaration_name_to_string(node.name());
            self.error(
                Some(node),
                &diagnostics::METHOD_0_CANNOT_HAVE_AN_IMPLEMENTATION_BECAUSE_IT_IS_MARKED_ABSTRACT,
                &[&name],
            );
        }
        // Private named methods are only allowed in class declarations
        if ast::is_private_identifier(node.name().unwrap())
            && ast::get_containing_class(node).is_none()
        {
            self.error(
                Some(node),
                &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                &[],
            );
        }
        self.set_node_links_for_private_identifier_scope(node);
    }

    // checker.go:2861
    fn check_class_static_block_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        self.check_grammar_modifiers(node);
        node.for_each_child(&mut |n| self.check_source_element(Some(n)));
        if node.locals().map_or(0, |locals| locals.len()) != 0 {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // checker.go:2870
    fn check_constructor_declaration(&mut self, node: P<Node>) {
        // Grammar check on signature of constructor and modifier of the constructor is done in checkSignatureDeclaration function.
        self.check_signature_declaration(node);
        // Grammar check for checking only related to constructorDeclaration
        if !self.check_grammar_constructor_type_parameters(node) {
            self.check_grammar_constructor_type_annotation(node);
        }
        self.check_source_element(node.body());
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        self.check_function_or_constructor_symbol(symbol);
        // exit early in the case of signature - super checks are not relevant to them
        if ast::node_is_missing(node.body()) {
            return;
        }
        // TS 1.0 spec (April 2014): 8.3.2
        // Constructors of classes with no extends clause may not contain super calls, whereas
        // constructors of derived classes must contain at least one super call somewhere in their function body.
        let containing_class_decl = node.parent().unwrap();
        if ast::get_class_extends_heritage_element(containing_class_decl).is_none() {
            return;
        }
        let class_extends_null = self.class_declaration_extends_null(containing_class_decl);
        let super_call = Self::find_first_super_call(node.body().unwrap());
        if let Some(super_call) = super_call {
            if class_extends_null {
                self.error(Some(super_call), &diagnostics::A_CONSTRUCTOR_CANNOT_CONTAIN_A_SUPER_CALL_WHEN_ITS_CLASS_EXTENDS_NULL, &[]);
            }
            // A super call must be root-level in a constructor if both of the following are true:
            // - The containing class is a derived class.
            // - The constructor declares parameter properties
            //   or the containing class declares instance member variables with initializers.
            let super_call_should_be_root_level = !self.emit_standard_class_fields
                && (node.parent().unwrap().members().iter().any(|&m| {
                    is_instance_property_with_initializer_or_private_identifier_property(m)
                }) || node.parameters().iter().any(|&p| {
                    ast::has_syntactic_modifier(p, ModifierFlags::ParameterPropertyModifier)
                }));
            if super_call_should_be_root_level {
                // Until we have better flow analysis, it is an error to place the super call within any kind of block or conditional
                // See GH #8277
                if super_call_is_root_level_in_constructor(super_call, node.body().unwrap()) {
                    let mut super_call_statement: Option<P<Node>> = None;
                    for &statement in node.body().unwrap().statements() {
                        if ast::is_expression_statement(statement)
                            && is_super_call(ast::skip_outer_expressions(
                                statement.expression().unwrap(),
                                OuterExpressionKinds::All,
                            ))
                        {
                            super_call_statement = Some(statement);
                            break;
                        }
                        if node_immediately_references_super_or_this(statement) {
                            break;
                        }
                    }
                    // Until we have better flow analysis, it is an error to place the super call within any kind of block or conditional
                    // See GH #8277
                    if super_call_statement.is_none() {
                        self.error(
                            Some(node),
                            &diagnostics::A_SUPER_CALL_MUST_BE_THE_FIRST_STATEMENT_IN_THE_CONSTRUCTOR_TO_REFER_TO_SUPER_OR_THIS_WHEN_A_DERIVED_CLASS_CONTAINS_INITIALIZED_PROPERTIES_PARAMETER_PROPERTIES_OR_PRIVATE_IDENTIFIERS,
                            &[],
                        );
                    }
                } else {
                    self.error(
                        Some(super_call),
                        &diagnostics::A_SUPER_CALL_MUST_BE_A_ROOT_LEVEL_STATEMENT_WITHIN_A_CONSTRUCTOR_OF_A_DERIVED_CLASS_THAT_CONTAINS_INITIALIZED_PROPERTIES_PARAMETER_PROPERTIES_OR_PRIVATE_IDENTIFIERS,
                        &[],
                    );
                }
            }
        } else if !class_extends_null {
            self.error(
                Some(node),
                &diagnostics::CONSTRUCTORS_FOR_DERIVED_CLASSES_MUST_CONTAIN_A_SUPER_CALL,
                &[],
            );
        }
    }

    // checker.go:2935
    fn find_first_super_call(node: P<Node>) -> Option<P<Node>> {
        fn visit(node: P<Node>, super_call: &mut Option<P<Node>>) -> bool {
            if is_super_call(node) {
                *super_call = Some(node);
                return true;
            } else if ast::is_function_like(node) {
                return false;
            }
            node.for_each_child(&mut |n| visit(n, super_call))
        }
        let mut super_call: Option<P<Node>> = None;
        visit(node, &mut super_call);
        super_call
    }
}

// checker.go:2952
pub fn is_instance_property_with_initializer_or_private_identifier_property(n: P<Node>) -> bool {
    ast::is_private_identifier_class_element_declaration(n)
        || ast::is_property_declaration(n) && !ast::is_static(n) && n.initializer().is_some()
}

// checker.go:2956
pub fn super_call_is_root_level_in_constructor(super_call: P<Node>, body: P<Node>) -> bool {
    let super_call_parent = ast::walk_up_parenthesized_expressions(super_call.parent()).unwrap();
    ast::is_expression_statement(super_call_parent) && super_call_parent.parent() == Some(body)
}

// checker.go:2961
pub fn node_immediately_references_super_or_this(node: P<Node>) -> bool {
    match node.kind() {
        Kind::SuperKeyword | Kind::ThisKeyword => return true,
        Kind::ArrowFunction
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::PropertyDeclaration => return false,
        Kind::Block => match node.parent().unwrap().kind() {
            Kind::Constructor | Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => {
                return false;
            }
            _ => {}
        },
        _ => {}
    }
    node.for_each_child(&mut |n| node_immediately_references_super_or_this(n))
}

impl Checker {
    // checker.go:2976
    fn check_accessor_declaration(&mut self, node: P<Node>) {
        // Grammar checking accessors
        if !self.check_grammar_function_like_declaration(node) && !self.check_grammar_accessor(node)
        {
            self.check_grammar_computed_property_name(node.name().unwrap());
        }
        let name = node.name().unwrap();
        if ast::is_identifier(name)
            && name.text() == "constructor"
            && ast::is_class_like(node.parent().unwrap())
        {
            self.error(node.name(), &diagnostics::CLASS_CONSTRUCTOR_MAY_NOT_BE_AN_ACCESSOR, &[]);
        }
        self.check_decorators(node);
        self.check_signature_declaration(node);
        if ast::is_get_accessor_declaration(node)
            && !node.flags().intersects(NodeFlags::Ambient)
            && ast::node_is_present(node.body())
            && node.flags().intersects(NodeFlags::HasImplicitReturn)
            && !node.flags().intersects(NodeFlags::HasExplicitReturn)
        {
            self.error(Some(name), &diagnostics::A_GET_ACCESSOR_MUST_RETURN_A_VALUE, &[]);
        }
        // Do not use hasDynamicName here, because that returns false for well known symbols.
        // We want to perform checkComputedPropertyName for all computed properties, including
        // well known symbols.
        if ast::is_computed_property_name(name) {
            self.check_computed_property_name(name);
        }
        if self.has_bindable_name(node) {
            // TypeScript 1.0 spec (April 2014): 8.4.3
            // Accessors for the same member name must specify the same accessibility.
            let symbol = self.get_symbol_of_declaration(node).unwrap();
            let getter = ast::get_declaration_of_kind(symbol, Kind::GetAccessor);
            let setter = ast::get_declaration_of_kind(symbol, Kind::SetAccessor);
            if let (Some(getter), Some(setter)) = (getter, setter)
                && !self.node_links.get(getter).flags.get().intersects(NodeCheckFlags::TypeChecked)
            {
                let getter_links = self.node_links.get(getter);
                getter_links.flags.set(getter_links.flags.get() | NodeCheckFlags::TypeChecked);
                let getter_flags = getter.modifier_flags();
                let setter_flags = setter.modifier_flags();
                if (getter_flags & ModifierFlags::Abstract)
                    != (setter_flags & ModifierFlags::Abstract)
                {
                    self.error(
                        getter.name(),
                        &diagnostics::ACCESSORS_MUST_BOTH_BE_ABSTRACT_OR_NON_ABSTRACT,
                        &[],
                    );
                    self.error(
                        setter.name(),
                        &diagnostics::ACCESSORS_MUST_BOTH_BE_ABSTRACT_OR_NON_ABSTRACT,
                        &[],
                    );
                }
                if (getter_flags.intersects(ModifierFlags::Protected)
                    && !setter_flags.intersects(ModifierFlags::Protected | ModifierFlags::Private))
                    || (getter_flags.intersects(ModifierFlags::Private)
                        && !setter_flags.intersects(ModifierFlags::Private))
                {
                    self.error(
                        getter.name(),
                        &diagnostics::A_GET_ACCESSOR_MUST_BE_AT_LEAST_AS_ACCESSIBLE_AS_THE_SETTER,
                        &[],
                    );
                    self.error(
                        setter.name(),
                        &diagnostics::A_GET_ACCESSOR_MUST_BE_AT_LEAST_AS_ACCESSIBLE_AS_THE_SETTER,
                        &[],
                    );
                }
            }
        }
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let return_type = self.get_type_of_accessors(symbol);
        if node.kind() == Kind::GetAccessor {
            self.check_all_code_paths_in_non_void_function_return_or_throw(node, Some(return_type));
        }
        self.check_source_element(node.body());
        self.set_node_links_for_private_identifier_scope(node);
    }

    // checker.go:3028
    pub(crate) fn check_type_reference_node(&mut self, node: P<Node>) {
        self.check_grammar_type_arguments(node, node.type_argument_list());
        if ast::is_type_reference_node(node) && !node.flags().intersects(NodeFlags::JSDoc) {
            let data = node.as_type_reference_node();
            if let Some(type_arguments) = node.type_argument_list()
                && data.type_name.end() != type_arguments.pos()
            {
                // If there was a token between the type name and the type arguments, check if it was a DotToken
                let source_file = ast::get_source_file_of_node(node).unwrap();
                let type_name_end =
                    data.type_name.end().expect_source("parsed nodes have source positions");
                if tsrs_scanner::scan_token_at_position(source_file, type_name_end)
                    == Kind::DotToken
                {
                    self.grammar_error_at_pos(
                        node,
                        tsrs_scanner::skip_trivia(source_file.text(), type_name_end),
                        1,
                        &diagnostics::JSDOC_TYPES_CAN_ONLY_BE_USED_INSIDE_DOCUMENTATION_COMMENTS,
                        &[],
                    );
                }
            }
        }
        self.check_source_elements(node.type_arguments());
        if !(crate::is_const_type_reference(node)
            && ast::is_assertion_expression(node.parent().unwrap()))
        {
            self.check_type_reference_or_import(node);
        }
    }

    // checker.go:3046
    fn check_type_reference_or_import(&mut self, node: P<Node>) {
        let t = self.get_type_from_type_node(node);
        if !self.is_error_type(t) {
            if !node.type_arguments().is_empty() && !self.defer_type_argument_constraints(node) {
                let type_parameters = self.get_type_parameters_for_type_reference_or_import(node);
                if !type_parameters.is_empty() {
                    self.check_type_argument_constraints(node, &type_parameters);
                }
            }
            let symbol = self.get_resolved_symbol_or_nil(node);
            if let Some(symbol) = symbol {
                let declarations = symbol.declarations();
                if declarations
                    .iter()
                    .any(|&d| ast::is_type_declaration(d) && self.is_deprecated_declaration(d))
                {
                    let suggestion_node = Self::get_deprecated_suggestion_node(node).unwrap();
                    self.add_deprecated_suggestion(suggestion_node, declarations, symbol.name());
                }
            }
        }
    }

    /// tsrs-only: whether to leave the type-argument constraint check of `node` to the check of its file. A checker
    /// reaches the check while computing a type in a file it has not started checking (a function expression's
    /// signature is checked when its type is first needed, from any file that uses it). The check only reports
    /// diagnostics, and a checker's diagnostics in a file it does not check are never read, so every checker but the
    /// one that checks the file repeated it for nothing: in excalidraw one such check costs each of 9 checkers 0.3 s
    /// and 120 MiB of discarded error elaboration (notes/perf-excalidraw-typefest.md). TypeScript defers the same
    /// check (`addLazyDiagnostic` in `checkTypeReferenceOrImport`); typescript-go runs it at once, as Go-compatible
    /// history mode still does.
    fn defer_type_argument_constraints(&mut self, node: P<Node>) -> bool {
        if tsrs_core::compat::go_compatible_history() {
            return false;
        }
        let Some(file) = ast::get_source_file_of_node(Some(node)) else {
            return false;
        };
        if self.checking_file == Some(file)
            || self.source_file_links.try_get(file).is_some_and(|links| links.type_checked.get())
        {
            return false;
        }
        self.deferred_type_argument_checks.entry(file).or_default().push(node);
        true
    }

    /// tsrs-only: the checks `defer_type_argument_constraints` left to the check of `source_file`, each as checking the
    /// type reference itself would run it.
    fn run_deferred_type_argument_checks(&mut self, source_file: P<SourceFile>) {
        let Some(nodes) = self.deferred_type_argument_checks.remove(&source_file) else {
            return;
        };
        for node in nodes {
            let save_current_node = self.current_node.replace(node);
            self.instantiation_count = 0;
            let type_parameters = self.get_type_parameters_for_type_reference_or_import(node);
            if !type_parameters.is_empty() {
                self.check_type_argument_constraints(node, &type_parameters);
            }
            self.current_node = save_current_node;
        }
    }

    // checker.go:3064
    pub(crate) fn check_type_argument_constraints(
        &mut self,
        node: P<Node>,
        type_parameters: &[P<Type>],
    ) -> bool {
        let mut type_arguments: Option<&'static [P<Type>]> = None;
        let mut mapper: Option<P<TypeMapper>> = None;
        let mut result = true;
        for (i, &type_parameter) in type_parameters.iter().enumerate() {
            let constraint = self.get_constraint_of_type_parameter(type_parameter);
            if let Some(constraint) = constraint {
                if type_arguments.is_none() {
                    let args = alloc_vec(self.get_effective_type_arguments(node, type_parameters));
                    type_arguments = Some(args);
                    mapper = Some(new_type_mapper(alloc_slice(type_parameters), args));
                }
                result = result && {
                    let instantiated = self.instantiate_type(constraint, mapper);
                    self.check_type_assignable_to(
                        type_arguments.unwrap()[i],
                        instantiated,
                        element_or_nil(node.type_arguments(), i),
                        Some(&diagnostics::TYPE_0_DOES_NOT_SATISFY_THE_CONSTRAINT_1),
                    )
                };
            }
        }
        result
    }

    // checker.go:3081
    pub(crate) fn get_deprecated_suggestion_node(node: P<Node>) -> Option<P<Node>> {
        let node = ast::skip_parentheses(node);
        match node.kind() {
            Kind::CallExpression | Kind::Decorator | Kind::NewExpression => {
                return Self::get_deprecated_suggestion_node(node.expression().unwrap());
            }
            Kind::TaggedTemplateExpression => {
                return Self::get_deprecated_suggestion_node(
                    node.as_tagged_template_expression().tag,
                );
            }
            Kind::JsxOpeningElement | Kind::JsxSelfClosingElement => {
                return Self::get_deprecated_suggestion_node(node.tag_name());
            }
            Kind::ElementAccessExpression => {
                return Some(node.as_element_access_expression().argument_expression);
            }
            Kind::PropertyAccessExpression => return node.name(),
            Kind::TypeReference => {
                let type_name = node.as_type_reference_node().type_name;
                if ast::is_qualified_name(type_name) {
                    return Some(type_name.as_qualified_name().right);
                }
            }
            _ => {}
        }
        Some(node)
    }

    // checker.go:3103
    fn check_type_predicate(&mut self, node: P<Node>) {
        // Always check the predicate's type so nested type errors are reported even when the
        // predicate is in an invalid position, keeping diagnostics stable.
        self.check_source_element(node.type_node());
        let parent = Self::get_type_predicate_parent(node);
        let Some(parent) = parent else {
            // The parent must not be valid.
            self.error(Some(node), &diagnostics::A_TYPE_PREDICATE_IS_ONLY_ALLOWED_IN_RETURN_TYPE_POSITION_FOR_FUNCTIONS_AND_METHODS, &[]);
            return;
        };
        let signature = self.get_signature_from_declaration(parent);
        let type_predicate = self.get_type_predicate_of_signature(signature);
        let Some(type_predicate) = type_predicate else {
            return;
        };
        let parameter_name = node.as_type_predicate_node().parameter_name;
        if type_predicate.kind.get() != TypePredicateKind::This
            && type_predicate.kind.get() != TypePredicateKind::AssertsThis
        {
            if let Some(parameter_index) = type_predicate.parameter_index.get() {
                if signature_has_rest_parameter(signature)
                    && parameter_index == signature.parameters.get().len() - 1
                {
                    self.error(
                        Some(parameter_name),
                        &diagnostics::A_TYPE_PREDICATE_CANNOT_REFERENCE_A_REST_PARAMETER,
                        &[],
                    );
                } else if let Some(predicate_type) = type_predicate.t.get() {
                    let mut diags: Vec<P<Diagnostic>> = Vec::new();
                    let parameter_type =
                        self.get_type_of_symbol(signature.parameters.get()[parameter_index]);
                    if !self.check_type_assignable_to_ex(
                        predicate_type,
                        parameter_type,
                        node.type_node(),
                        None, /*headMessage*/
                        &mut diags,
                    ) {
                        self.add_diagnostic(ast::new_diagnostic_chain(diags[0], &diagnostics::A_TYPE_PREDICATE_S_TYPE_MUST_BE_ASSIGNABLE_TO_ITS_PARAMETER_S_TYPE, &[]));
                    }
                }
            } else {
                // Go checks `parameterName != nil`; TypePredicateNode.parameter_name is never nil in the port.
                let mut has_reported_error = false;
                for &param in parent.parameters() {
                    let name = param.name().unwrap();
                    if ast::is_binding_pattern(name)
                        && self.check_if_type_predicate_variable_is_declared_in_binding_pattern(
                            name,
                            parameter_name,
                            type_predicate.parameter_name.get(),
                        )
                    {
                        has_reported_error = true;
                        break;
                    }
                }
                if !has_reported_error {
                    self.error(
                        Some(parameter_name),
                        &diagnostics::CANNOT_FIND_PARAMETER_0,
                        &[&type_predicate.parameter_name.get()],
                    );
                }
            }
        }
    }

    // checker.go:3147
    fn get_type_predicate_parent(node: P<Node>) -> Option<P<Node>> {
        let parent = node.parent().unwrap();
        match parent.kind() {
            Kind::ArrowFunction
            | Kind::CallSignature
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::FunctionType
            | Kind::MethodDeclaration
            | Kind::MethodSignature
                if Some(node) == parent.type_node() =>
            {
                return Some(parent);
            }
            _ => {}
        }
        None
    }

    // checker.go:3159
    fn check_if_type_predicate_variable_is_declared_in_binding_pattern(
        &mut self,
        pattern: P<Node>,
        predicate_variable_node: P<Node>,
        predicate_variable_name: &str,
    ) -> bool {
        if pattern.elements().iter().any(|&element| {
            let Some(name) = element.name() else {
                return false;
            };
            if ast::is_identifier(name) && name.text() == predicate_variable_name {
                self.error(
                    Some(predicate_variable_node),
                    &diagnostics::A_TYPE_PREDICATE_CANNOT_REFERENCE_ELEMENT_0_IN_A_BINDING_PATTERN,
                    &[&predicate_variable_name],
                );
                return true;
            }
            if (ast::is_array_binding_pattern(name) || ast::is_object_binding_pattern(name))
                && self.check_if_type_predicate_variable_is_declared_in_binding_pattern(
                    name,
                    predicate_variable_node,
                    predicate_variable_name,
                )
            {
                return true;
            }

            false
        }) {
            return true;
        }
        false
    }

    // checker.go:3178
    fn check_type_query(&mut self, node: P<Node>) {
        self.get_type_from_type_query_node(node);
    }

    // checker.go:3182
    fn check_type_literal(&mut self, node: P<Node>) {
        self.check_source_elements(node.members());
        let t = self.get_type_from_type_literal_or_function_or_constructor_type_node(node);
        self.check_index_constraints(t, t.symbol().unwrap(), false /*isStaticIndex*/);
        self.check_type_for_duplicate_index_signatures(node);
        self.check_object_type_for_duplicate_declarations(node, false /*checkPrivateNames*/);
    }

    // checker.go:3190
    pub(crate) fn check_object_type_for_duplicate_declarations(
        &mut self,
        node: P<Node>,
        check_private_names: bool,
    ) {
        fn check_property_or_accessor(
            c: &mut Checker,
            node: P<Node>,
            instance_names: &mut FxHashMap<String, u8>,
            static_names: &mut FxHashMap<String, u8>,
            symbol: P<Symbol>,
            kind: u8,
            is_static: bool,
        ) {
            if symbol.declarations().len() > 1 {
                let names = if is_static { static_names } else { instance_names };
                let state = names.get(symbol.name()).copied().unwrap_or(0);
                if state == 0 {
                    // On first occurrence just record the kind
                    names.insert(symbol.name().to_string(), kind);
                } else if state == 1 || state == 2 && kind != 2 {
                    // Error on second property or combination of property and accessor
                    c.report_duplicate_member_errors(
                        node,
                        symbol.name(),
                        true,
                        is_static,
                        &diagnostics::DUPLICATE_IDENTIFIER_0,
                    );
                    // Record that errors have been reported
                    names.insert(symbol.name().to_string(), 3);
                }
            }
        }

        let mut instance_names: FxHashMap<String, u8> = FxHashMap::default();
        let mut static_names: FxHashMap<String, u8> = FxHashMap::default();
        let mut private_names: FxHashMap<String, u8> = FxHashMap::default();
        let node_in_ambient_context = node.flags().intersects(NodeFlags::Ambient);
        for &member in node.members() {
            if ast::is_constructor_declaration(member) {
                for &param in member.parameters() {
                    if ast::is_parameter_property_declaration(param, member)
                        && !ast::is_binding_pattern(param.name().unwrap())
                    {
                        let symbol = self.get_symbol_of_declaration(param).unwrap();
                        check_property_or_accessor(
                            self,
                            node,
                            &mut instance_names,
                            &mut static_names,
                            symbol,
                            1,
                            false, /*isStatic*/
                        );
                    }
                }
            } else {
                let symbol = self.get_symbol_of_declaration(member);
                let is_static = ast::has_static_modifier(member);
                // In non-ambient contexts, check that static members are not named 'prototype'.
                if !node_in_ambient_context
                    && is_static
                    && symbol.is_some_and(|s| s.name() == "prototype")
                {
                    let symbol = symbol.unwrap();
                    let node_symbol = self.get_symbol_of_declaration(node).unwrap();
                    let node_symbol_str = self.symbol_to_string(node_symbol);
                    self.error(
                        member.name(),
                        &diagnostics::STATIC_PROPERTY_0_CONFLICTS_WITH_BUILT_IN_PROPERTY_FUNCTION_0_OF_CONSTRUCTOR_FUNCTION_1,
                        &[&symbol.name(), &node_symbol_str],
                    );
                }
                // Check that this object type declaration doesn't contain multiple declarations of the same property,
                // or accessor and property declarations with the same name.
                if ast::is_property_declaration(member) && !ast::has_accessor_modifier(member)
                    || ast::is_property_signature_declaration(member)
                {
                    check_property_or_accessor(
                        self,
                        node,
                        &mut instance_names,
                        &mut static_names,
                        symbol.unwrap(),
                        1,
                        is_static,
                    );
                } else if ast::is_accessor(member)
                    || ast::is_property_declaration(member) && ast::has_accessor_modifier(member)
                {
                    check_property_or_accessor(
                        self,
                        node,
                        &mut instance_names,
                        &mut static_names,
                        symbol.unwrap(),
                        2,
                        is_static,
                    );
                }
                // Check that each private identifier is used only for instance members or only for static members. It is an
                // error for an instance and a static member to have the same private identifier.
                if check_private_names && member.name().is_some_and(ast::is_private_identifier) {
                    let symbol = symbol.unwrap();
                    let mut flags = private_names.get(symbol.name()).copied().unwrap_or(0);
                    if flags != 3 {
                        flags |= if_else(ast::is_static(member), 2, 1);
                        private_names.insert(symbol.name().to_string(), flags);
                        if flags == 3 {
                            self.report_duplicate_member_errors(
                                node,
                                symbol.name(),
                                false,
                                false,
                                &diagnostics::DUPLICATE_IDENTIFIER_0_STATIC_AND_INSTANCE_ELEMENTS_CANNOT_SHARE_THE_SAME_PRIVATE_NAME,
                            );
                        }
                    }
                }
            }
        }
    }

    // checker.go:3261
    fn report_duplicate_member_errors(
        &mut self,
        node: P<Node>,
        name: &str,
        check_static: bool,
        is_static: bool,
        message: &'static Message,
    ) {
        for &member in node.members() {
            if ast::is_constructor_declaration(member) {
                for &param in member.parameters() {
                    if ast::is_parameter_property_declaration(param, member)
                        && !ast::is_binding_pattern(param.name().unwrap())
                    {
                        let symbol = self.get_symbol_of_declaration(param).unwrap();
                        if symbol.name() == name {
                            let s = self.symbol_to_string(symbol);
                            self.error(param.name(), message, &[&s]);
                        }
                    }
                }
            } else if let Some(symbol) = self.get_symbol_of_declaration(member)
                && symbol.name() == name
                && (!check_static || is_static == ast::is_static(member))
            {
                let s = self.symbol_to_string(symbol);
                self.error(member.name(), message, &[&s]);
            }
        }
    }

    // checker.go:3277
    fn check_array_type(&mut self, node: P<Node>) {
        self.check_source_element(Some(node.as_array_type_node().element_type));
    }

    // checker.go:3281
    fn check_tuple_type(&mut self, node: P<Node>) {
        let mut seen_optional_element = false;
        let mut seen_rest_element = false;
        let elements = node.elements();
        for &e in elements {
            let mut flags = Self::get_tuple_element_flags(e);
            if flags.intersects(ElementFlags::Variadic) {
                let t = self.get_type_from_type_node(e.type_node().unwrap());
                if !self.is_array_like_type(t) {
                    self.error(
                        Some(e),
                        &diagnostics::A_REST_ELEMENT_TYPE_MUST_BE_AN_ARRAY_TYPE,
                        &[],
                    );
                    break;
                }
                if self.is_array_type(t)
                    || is_tuple_type(t)
                        && t.target_tuple_type().combined_flags.get().intersects(ElementFlags::Rest)
                {
                    flags |= ElementFlags::Rest;
                }
            }
            if flags.intersects(ElementFlags::Rest) {
                if seen_rest_element {
                    self.grammar_error_on_node(
                        e,
                        &diagnostics::A_REST_ELEMENT_CANNOT_FOLLOW_ANOTHER_REST_ELEMENT,
                        &[],
                    );
                    break;
                }
                seen_rest_element = true;
            } else if flags.intersects(ElementFlags::Optional) {
                if seen_rest_element {
                    self.grammar_error_on_node(
                        e,
                        &diagnostics::AN_OPTIONAL_ELEMENT_CANNOT_FOLLOW_A_REST_ELEMENT,
                        &[],
                    );
                    break;
                }
                seen_optional_element = true;
            } else if flags.intersects(ElementFlags::Required) && seen_optional_element {
                self.grammar_error_on_node(
                    e,
                    &diagnostics::A_REQUIRED_ELEMENT_CANNOT_FOLLOW_AN_OPTIONAL_ELEMENT,
                    &[],
                );
                break;
            }
        }
        self.check_source_elements(elements);
        self.get_type_from_type_node(node);
    }

    // checker.go:3318
    fn check_union_or_intersection_type(&mut self, node: P<Node>) {
        node.for_each_child(&mut |n| self.check_source_element(Some(n)));
        self.get_type_from_type_node(node);
    }

    // checker.go:3323
    fn check_this_type(&mut self, node: P<Node>) {
        self.get_type_from_this_type_node(node);
    }

    // checker.go:3327
    fn check_type_operator(&mut self, node: P<Node>) {
        self.check_grammar_type_operator_node(node);
        self.check_source_element(node.type_node());
    }

    // checker.go:3332
    fn check_conditional_type(&mut self, node: P<Node>) {
        node.for_each_child(&mut |n| self.check_source_element(Some(n)));
    }

    // checker.go:3336
    fn check_infer_type(&mut self, node: P<Node>) {
        if ast::find_ancestor(node, |n| {
            n.parent().is_some_and(|parent| {
                parent.kind() == Kind::ConditionalType
                    && parent.as_conditional_type_node().extends_type == n
            })
        })
        .is_none()
        {
            self.grammar_error_on_node(node, &diagnostics::X_INFER_DECLARATIONS_ARE_ONLY_PERMITTED_IN_THE_EXTENDS_CLAUSE_OF_A_CONDITIONAL_TYPE, &[]);
        }
        let type_parameter_declaration_node = node.as_infer_type_node().type_parameter;
        self.check_source_element(Some(type_parameter_declaration_node));
        let symbol = self.get_symbol_of_declaration(type_parameter_declaration_node).unwrap();
        if symbol.declarations().len() > 1 {
            let links = self.declared_type_links.get(symbol);
            if !links.type_parameters_checked.get() {
                links.type_parameters_checked.set(true);
                let type_parameter = self.get_declared_type_of_type_parameter(symbol);
                let declarations = get_declarations_of_kind(symbol, Kind::TypeParameter);
                if !self.are_type_parameters_identical(
                    &declarations,
                    &[type_parameter],
                    |_, decl| vec![decl],
                ) {
                    // Report an error on every conflicting declaration.
                    let name = self.symbol_to_string(symbol);
                    for &declaration in &declarations {
                        self.error(
                            declaration.name(),
                            &diagnostics::ALL_DECLARATIONS_OF_0_MUST_HAVE_IDENTICAL_CONSTRAINTS,
                            &[&name],
                        );
                    }
                }
            }
        }
        self.register_for_unused_identifiers_check(node);
    }

    // checker.go:3363
    fn check_template_literal_type(&mut self, node: P<Node>) {
        for &span in node.as_template_literal_type_node().template_spans.nodes() {
            self.check_source_element(span.type_node());
            let t = self.get_type_from_type_node(span.type_node().unwrap());
            self.check_type_assignable_to(t, self.template_constraint_type, span.type_node(), None);
        }
        self.get_type_from_type_node(node);
    }

    // checker.go:3372
    fn check_import_type(&mut self, node: P<Node>) {
        self.check_source_element(Some(node.as_import_type_node().argument));
        if let Some(attributes) = node.as_import_type_node().attributes {
            let import_attributes = attributes;
            self.check_grammar_import_attribute_values(import_attributes);
            self.get_resolution_mode_override(import_attributes, true /*reportErrors*/);
        }
        self.check_type_reference_or_import(node);
        self.check_import_attributes(node);
    }

    // checker.go:3383
    pub(crate) fn get_resolution_mode_override(
        &mut self,
        node: P<Node>,
        report_errors: bool,
    ) -> ModuleKind {
        let mode = if report_errors {
            let mut grammar_error_on_node =
                |n: P<Node>, message: &'static Message, args: &[&dyn Display]| {
                    self.grammar_error_on_node(n, message, args)
                };
            ast::get_resolution_mode_override(Some(node), Some(&mut grammar_error_on_node))
        } else {
            ast::get_resolution_mode_override(Some(node), None)
        };
        mode.unwrap_or(RESOLUTION_MODE_NONE)
    }

    // checker.go:3392
    fn check_named_tuple_member(&mut self, node: P<Node>) {
        let tuple_member = node.as_named_tuple_member();
        if tuple_member.dot_dot_dot_token.is_some() && tuple_member.question_token.is_some() {
            self.grammar_error_on_node(
                node,
                &diagnostics::A_TUPLE_MEMBER_CANNOT_BE_BOTH_OPTIONAL_AND_REST,
                &[],
            );
        }
        if tuple_member.type_.kind() == Kind::OptionalType {
            self.grammar_error_on_node(
                tuple_member.type_,
                &diagnostics::A_LABELED_TUPLE_ELEMENT_IS_DECLARED_AS_OPTIONAL_WITH_A_QUESTION_MARK_AFTER_THE_NAME_AND_BEFORE_THE_COLON_RATHER_THAN_AFTER_THE_TYPE,
                &[],
            );
        }
        if tuple_member.type_.kind() == Kind::RestType {
            self.grammar_error_on_node(
                tuple_member.type_,
                &diagnostics::A_LABELED_TUPLE_ELEMENT_IS_DECLARED_AS_REST_WITH_A_BEFORE_THE_NAME_RATHER_THAN_BEFORE_THE_TYPE,
                &[],
            );
        }
        self.check_source_element(node.type_node());
        self.get_type_from_type_node(node);
    }

    // checker.go:3407
    fn check_indexed_access_type(&mut self, node: P<Node>) {
        node.for_each_child(&mut |n| self.check_source_element(Some(n)));
        let t = self.get_type_from_indexed_access_type_node(node);
        self.check_indexed_access_index_type(t, node);
    }

    // checker.go:3412
    fn check_mapped_type(&mut self, node: P<Node>) {
        let mapped_type_node = node.as_mapped_type_node();
        self.check_grammar_mapped_type(node);
        self.check_source_element(Some(mapped_type_node.type_parameter));
        self.check_source_element(mapped_type_node.name_type);
        self.check_source_element(mapped_type_node.type_);
        if mapped_type_node.type_.is_none() {
            self.report_implicit_any(node, self.any_type, WideningKind::Normal);
        }
        let t = self.get_type_from_mapped_type_node(node);
        let name_type = self.get_name_type_from_mapped_type(t);
        if let Some(name_type) = name_type {
            self.check_type_assignable_to(
                name_type,
                self.string_number_symbol_type,
                mapped_type_node.name_type,
                None,
            );
        } else {
            let constraint_type = self.get_constraint_type_from_mapped_type(t);
            self.check_type_assignable_to(
                constraint_type,
                self.string_number_symbol_type,
                mapped_type_node.type_parameter.as_type_parameter_declaration().constraint,
                None,
            );
        }
    }

    // checker.go:3431
    fn check_function_declaration(&mut self, node: P<Node>) {
        self.check_function_or_method_declaration(node);
        self.check_grammar_for_generator(node);
        self.check_collisions_for_declaration_name(node, node.name());
    }

    // checker.go:3437
    fn check_function_or_method_declaration(&mut self, node: P<Node>) {
        self.check_decorators(node);
        self.check_signature_declaration(node);
        let function_flags = ast::get_function_flags(Some(node));
        // Do not use hasDynamicName here, because that returns false for well known symbols.
        // We want to perform checkComputedPropertyName for all computed properties, including
        // well known symbols.
        if let Some(name) = node.name()
            && ast::is_computed_property_name(name)
        {
            // This check will account for methods in class/interface declarations,
            // as well as accessors in classes/object literals
            self.check_computed_property_name(name);
        }
        if self.has_bindable_name(node) {
            // first we want to check the local symbol that contain this declaration
            // - if node.localSymbol !== undefined - this is current declaration is exported and localSymbol points to the local symbol
            // - if node.localSymbol === undefined - this node is non-exported so we can just pick the result of getSymbolOfNode
            let symbol = self.get_symbol_of_declaration(node);
            let local_symbol = node.local_symbol().or(symbol).unwrap();
            // Since the javascript won't do semantic analysis like typescript, ignore javascript function
            // declarations so that redeclaring a function in a JS file is not reported as a duplicate.
            if !node.flags().intersects(NodeFlags::JavaScriptFile) {
                self.check_function_or_constructor_symbol(local_symbol);
            }
            let symbol = symbol.unwrap();
            if symbol.parent().is_some() {
                // run check on export symbol to check that modifiers agree across all exported declarations
                self.check_function_or_constructor_symbol(symbol);
            }
        }
        let body = node.body();
        self.check_source_element(body);
        let return_type = self.get_return_type_from_annotation(node);
        self.check_all_code_paths_in_non_void_function_return_or_throw(node, return_type);
        if let Some(full_signature) = node.function_like_data().unwrap().full_signature() {
            self.check_source_element(Some(full_signature));
            let full_signature_type = self.get_type_from_type_node(full_signature);
            if self.get_contextual_call_signature(full_signature_type, node).is_none() {
                self.error(Some(full_signature), &diagnostics::A_JSDOC_TYPE_TAG_ON_A_FUNCTION_MUST_HAVE_A_SIGNATURE_WITH_THE_CORRECT_NUMBER_OF_ARGUMENTS, &[]);
            }
        }
        if node.type_node().is_none() {
            // Report an implicit any error if there is no body, no explicit return type, and node is not a private method
            // in an ambient context
            if ast::node_is_missing(body) && !is_private_within_ambient(node) {
                self.report_implicit_any(node, self.any_type, WideningKind::Normal);
            }
            if function_flags.intersects(FunctionFlags::Generator) && ast::node_is_present(body) {
                // A generator with a body and no type annotation can still cause errors. It can error if the
                // yielded values have no common supertype, or it can give an implicit any error if it has no
                // yielded values. The only way to trigger these errors is to try checking its return type.
                let signature = self.get_signature_from_declaration(node);
                self.get_return_type_of_signature(signature);
            }
        }
    }

    // checker.go:3489
    pub(crate) fn check_function_or_constructor_symbol(&mut self, symbol: P<Symbol>) {
        // Only check the symbol once
        if self.constructor_or_function_checked.add_if_absent(symbol) {
            self.check_function_or_constructor_symbol_worker(symbol);
        }
    }

    // checker.go:3497
    fn check_function_or_constructor_symbol_worker(&mut self, symbol: P<Symbol>) {
        fn get_canonical_overload(
            overloads: &[P<Node>],
            implementation: Option<P<Node>>,
        ) -> P<Node> {
            // Consider the canonical set of flags to be the flags of the bodyDeclaration or the first declaration
            // Error on all deviations from this canonical set of flags
            // The caveat is that if some overloads are defined in lib.d.ts, we don't want to
            // report the errors on those. To achieve this, we will say that the implementation is
            // the canonical signature only if it is in the same container as the first overload
            let implementation_shares_container_with_first_overload = implementation
                .is_some_and(|implementation| implementation.parent() == overloads[0].parent());
            if implementation_shares_container_with_first_overload {
                return implementation.unwrap();
            }
            overloads[0]
        }
        fn check_flag_agreement_between_overloads(
            c: &mut Checker,
            overloads: &[P<Node>],
            implementation: Option<P<Node>>,
            flags_to_check: ModifierFlags,
            some_overload_flags: ModifierFlags,
            all_overload_flags: ModifierFlags,
        ) {
            // Error if some overloads have a flag that is not shared by all overloads. To find the
            // deviations, we XOR someOverloadFlags with allOverloadFlags
            let some_but_not_all_overload_flags = some_overload_flags ^ all_overload_flags;
            if !some_but_not_all_overload_flags.is_empty() {
                let canonical_flags = c.get_effective_declaration_flags(
                    get_canonical_overload(overloads, implementation),
                    flags_to_check,
                );
                // Go iterates a map here (random order); an insertion-ordered map keeps the port deterministic.
                let mut groups: OrderedMap<P<SourceFile>, Vec<P<Node>>> = OrderedMap::default();
                for &overload in overloads {
                    let source_file = ast::get_source_file_of_node(overload).unwrap();
                    groups.entry(source_file).or_default().push(overload);
                }
                let groups: Vec<Vec<P<Node>>> = groups.into_values().collect();
                for overloads_in_file in &groups {
                    let canonical_flags_for_file = c.get_effective_declaration_flags(
                        get_canonical_overload(overloads_in_file, implementation),
                        flags_to_check,
                    );
                    for &overload in overloads_in_file {
                        let deviation = c.get_effective_declaration_flags(overload, flags_to_check)
                            ^ canonical_flags;
                        let deviation_in_file = c
                            .get_effective_declaration_flags(overload, flags_to_check)
                            ^ canonical_flags_for_file;
                        if deviation_in_file.intersects(ModifierFlags::Export) {
                            // Overloads in different files need not all have export modifiers. This is ok:
                            //   // lib.d.ts
                            //   declare function foo(s: number): string;
                            //   declare function foo(s: string): number;
                            //   export { foo };
                            //
                            //   // app.ts
                            //   declare module "lib" {
                            //     export function foo(s: boolean): boolean;
                            //   }
                            c.error(ast::get_name_of_declaration(overload), &diagnostics::OVERLOAD_SIGNATURES_MUST_ALL_BE_EXPORTED_OR_NON_EXPORTED, &[]);
                        } else if deviation_in_file.intersects(ModifierFlags::Ambient) {
                            // Though rare, a module augmentation (necessarily ambient) is allowed to add overloads
                            // to a non-ambient function in an implementation file.
                            c.error(ast::get_name_of_declaration(overload), &diagnostics::OVERLOAD_SIGNATURES_MUST_ALL_BE_AMBIENT_OR_NON_AMBIENT, &[]);
                        } else if deviation
                            .intersects(ModifierFlags::Private | ModifierFlags::Protected)
                        {
                            c.error(
                                ast::get_name_of_declaration(overload).or(Some(overload)),
                                &diagnostics::OVERLOAD_SIGNATURES_MUST_ALL_BE_PUBLIC_PRIVATE_OR_PROTECTED,
                                &[],
                            );
                        } else if deviation.intersects(ModifierFlags::Abstract) {
                            c.error(ast::get_name_of_declaration(overload), &diagnostics::OVERLOAD_SIGNATURES_MUST_ALL_BE_ABSTRACT_OR_NON_ABSTRACT, &[]);
                        }
                    }
                }
            }
        }
        fn check_question_token_agreement_between_overloads(
            c: &mut Checker,
            overloads: &[P<Node>],
            implementation: Option<P<Node>>,
            some_have_question_token: bool,
            all_have_question_token: bool,
        ) {
            if some_have_question_token != all_have_question_token {
                let canonical_has_question_token =
                    is_optional_declaration(get_canonical_overload(overloads, implementation));
                for &o in overloads {
                    if is_optional_declaration(o) != canonical_has_question_token {
                        c.error(
                            ast::get_name_of_declaration(o),
                            &diagnostics::OVERLOAD_SIGNATURES_MUST_ALL_BE_OPTIONAL_OR_REQUIRED,
                            &[],
                        );
                    }
                }
            }
        }
        fn report_implementation_expected_error(
            c: &mut Checker,
            node: P<Node>,
            is_constructor: bool,
        ) {
            let name = node.name();
            if name.is_some() && ast::node_is_missing(name) {
                return;
            }
            let mut seen = false;
            let mut subsequent_node: Option<P<Node>> = None;
            node.parent().unwrap().for_each_child(&mut |child| {
                if seen {
                    subsequent_node = Some(child);
                    return true;
                }
                seen = child == node;
                false
            });
            // We may be here because of some extra nodes between overloads that could not be parsed into a valid node.
            // In this case the subsequent node is not really consecutive (.pos !== node.end), and we must ignore it here.
            if let Some(subsequent_node) = subsequent_node {
                let is_consecutive = subsequent_node.pos() == node.end();
                let is_same_kind = subsequent_node.kind() == node.kind();
                if is_consecutive && is_same_kind {
                    let subsequent_name = subsequent_node.name();
                    let error_node = subsequent_name.or(Some(subsequent_node));
                    if let (Some(name), Some(subsequent_name)) = (name, subsequent_name)
                        && (ast::is_private_identifier(name)
                            && ast::is_private_identifier(subsequent_name)
                            && name.text() == subsequent_name.text()
                            || ast::is_computed_property_name(name)
                                && ast::is_computed_property_name(subsequent_name)
                                && {
                                    let t1 = c.check_computed_property_name(name);
                                    let t2 = c.check_computed_property_name(subsequent_name);
                                    c.is_type_identical_to(t1, t2)
                                }
                            || ast::is_property_name_literal(name)
                                && ast::is_property_name_literal(subsequent_name)
                                && name.text() == subsequent_name.text())
                    {
                        let report_error = (ast::is_method_declaration(node)
                            || ast::is_method_signature_declaration(node))
                            && ast::is_static(node) != ast::is_static(subsequent_node);
                        // we can get here in two cases
                        // 1. mixed static and instance class members
                        // 2. something with the same name was defined before the set of overloads that prevents them from merging
                        // here we'll report error only for the first case since for second we should already report error in binder
                        if report_error {
                            let diagnostic = if_else(
                                ast::is_static(node),
                                &diagnostics::FUNCTION_OVERLOAD_MUST_BE_STATIC,
                                &diagnostics::FUNCTION_OVERLOAD_MUST_NOT_BE_STATIC,
                            );
                            c.error(error_node, diagnostic, &[]);
                        }
                        return;
                    }
                    if ast::node_is_present(subsequent_node.body()) {
                        let name_str = tsrs_scanner::declaration_name_to_string(name);
                        c.error(
                            error_node,
                            &diagnostics::FUNCTION_IMPLEMENTATION_NAME_MUST_BE_0,
                            &[&name_str],
                        );
                        return;
                    }
                }
            }
            let error_node = name.or(Some(node));
            if is_constructor {
                c.error(error_node, &diagnostics::CONSTRUCTOR_IMPLEMENTATION_IS_MISSING, &[]);
            } else {
                // Report different errors regarding non-consecutive blocks of declarations depending on whether
                // the node in question is abstract.
                if ast::has_syntactic_modifier(node, ModifierFlags::Abstract) {
                    c.error(
                        error_node,
                        &diagnostics::ALL_DECLARATIONS_OF_AN_ABSTRACT_METHOD_MUST_BE_CONSECUTIVE,
                        &[],
                    );
                } else {
                    c.error(error_node, &diagnostics::FUNCTION_IMPLEMENTATION_IS_MISSING_OR_NOT_IMMEDIATELY_FOLLOWING_THE_DECLARATION, &[]);
                }
            }
        }

        let flags_to_check = ModifierFlags::Export
            | ModifierFlags::Ambient
            | ModifierFlags::Private
            | ModifierFlags::Protected
            | ModifierFlags::Abstract;
        let mut some_node_flags = ModifierFlags::None;
        let mut all_node_flags = flags_to_check;
        let mut some_have_question_token = false;
        let mut all_have_question_token = true;
        let mut has_overloads = false;
        let mut body_declaration: Option<P<Node>> = None;
        let mut last_seen_non_ambient_declaration: Option<P<Node>> = None;
        let mut previous_declaration: Option<P<Node>> = None;
        let declarations: Vec<P<Node>> = symbol.declarations().to_vec();
        let is_constructor = symbol.flags().intersects(SymbolFlags::Constructor);
        let mut duplicate_function_declaration = false;
        let mut multiple_constructor_implementation = false;
        let mut has_non_ambient_class = false;
        let mut function_declarations: Vec<P<Node>> = Vec::new();
        for &node in &declarations {
            let in_ambient_context = node.flags().intersects(NodeFlags::Ambient);
            let in_ambient_context_or_interface = in_ambient_context
                || node.parent().is_some_and(|parent| {
                    ast::is_interface_declaration(parent) || ast::is_type_literal_node(parent)
                });
            if in_ambient_context_or_interface {
                // check if declarations are consecutive only if they are non-ambient
                // 1. ambient declarations can be interleaved
                // i.e. this is legal
                //     declare function foo();
                //     declare function bar();
                //     declare function foo();
                // 2. mixing ambient and non-ambient declarations is a separate error that will be reported - do not want to report an extra one
                previous_declaration = None;
            }
            if ast::is_class_like(node) && !in_ambient_context {
                has_non_ambient_class = true;
            }
            if ast::is_function_declaration(node)
                || ast::is_method_declaration(node)
                || ast::is_method_signature_declaration(node)
                || ast::is_constructor_declaration(node)
            {
                function_declarations.push(node);
                let current_node_flags = self.get_effective_declaration_flags(node, flags_to_check);
                some_node_flags |= current_node_flags;
                all_node_flags &= current_node_flags;
                some_have_question_token =
                    some_have_question_token || is_optional_declaration(node);
                all_have_question_token = all_have_question_token && is_optional_declaration(node);
                let body_is_present = ast::node_is_present(node.body());
                if body_is_present && body_declaration.is_some() {
                    if is_constructor {
                        multiple_constructor_implementation = true;
                    } else {
                        duplicate_function_declaration = true;
                    }
                } else if let Some(previous) = previous_declaration {
                    let is_same_parent = previous.parent() == node.parent();
                    let is_consecutive = previous.end() == node.pos();
                    if is_same_parent
                        && !is_consecutive
                        && !previous.flags().intersects(NodeFlags::Reparsed)
                    {
                        report_implementation_expected_error(self, previous, is_constructor);
                    }
                }
                if body_is_present {
                    if body_declaration.is_none() {
                        body_declaration = Some(node);
                    }
                } else {
                    has_overloads = true;
                }
                previous_declaration = Some(node);
                if !in_ambient_context_or_interface {
                    last_seen_non_ambient_declaration = Some(node);
                }
            }
        }
        if multiple_constructor_implementation {
            for &declaration in &function_declarations {
                self.error(
                    Some(declaration),
                    &diagnostics::MULTIPLE_CONSTRUCTOR_IMPLEMENTATIONS_ARE_NOT_ALLOWED,
                    &[],
                );
            }
        }
        if duplicate_function_declaration {
            for &declaration in &function_declarations {
                self.error(
                    ast::get_name_of_declaration(declaration).or(Some(declaration)),
                    &diagnostics::DUPLICATE_FUNCTION_IMPLEMENTATION,
                    &[],
                );
            }
        }
        if has_non_ambient_class
            && !is_constructor
            && symbol.flags().intersects(SymbolFlags::Function)
            && !declarations.is_empty()
        {
            let mut related_diagnostics: Vec<P<Diagnostic>> = Vec::new();
            related_diagnostics.extend(declarations.iter().filter_map(|&declaration| {
                if ast::is_class_declaration(declaration) {
                    Some(create_diagnostic_for_node(
                        Some(declaration),
                        &diagnostics::CONSIDER_ADDING_A_DECLARE_MODIFIER_TO_THIS_CLASS,
                        &[],
                    ))
                } else {
                    None
                }
            }));
            for &declaration in &declarations {
                let diagnostic: Option<&'static Message> = match declaration.kind() {
                    Kind::ClassDeclaration => Some(&diagnostics::CLASS_DECLARATION_CANNOT_IMPLEMENT_OVERLOAD_LIST_FOR_0),
                    Kind::FunctionDeclaration => Some(&diagnostics::FUNCTION_WITH_BODIES_CAN_ONLY_MERGE_WITH_CLASSES_THAT_ARE_AMBIENT),
                    _ => None,
                };
                if let Some(diagnostic) = diagnostic {
                    self.error(
                        ast::get_name_of_declaration(declaration).or(Some(declaration)),
                        diagnostic,
                        &[&symbol.name()],
                    )
                    .set_related_info(&related_diagnostics);
                }
            }
        }
        // Abstract methods can't have an implementation -- in particular, they don't need one.
        if let Some(last) = last_seen_non_ambient_declaration
            && last.body().is_none()
            && !ast::has_syntactic_modifier(last, ModifierFlags::Abstract)
            && !is_optional_declaration(last)
        {
            report_implementation_expected_error(self, last, is_constructor);
        }
        if has_overloads {
            check_flag_agreement_between_overloads(
                self,
                &declarations,
                body_declaration,
                flags_to_check,
                some_node_flags,
                all_node_flags,
            );
            check_question_token_agreement_between_overloads(
                self,
                &declarations,
                body_declaration,
                some_have_question_token,
                all_have_question_token,
            );
            if let Some(body_declaration) = body_declaration {
                let signatures = self.get_signatures_of_symbol(Some(symbol));
                let body_signature = self.get_signature_from_declaration(body_declaration);
                if let Some(signature) = signatures.iter().find_map(|&signature| {
                    if self.is_implementation_compatible_with_overload(body_signature, signature) {
                        None
                    } else {
                        Some(signature)
                    }
                }) {
                    let error_node = signature.declaration.get();
                    self.error(error_node, &diagnostics::THIS_OVERLOAD_SIGNATURE_IS_NOT_COMPATIBLE_WITH_ITS_IMPLEMENTATION_SIGNATURE, &[])
                        .add_related_info(create_diagnostic_for_node(Some(body_declaration), &diagnostics::THE_IMPLEMENTATION_SIGNATURE_IS_DECLARED_HERE, &[]));
                }
            }
        }
    }

    // checker.go:3729
    pub(crate) fn get_effective_declaration_flags(
        &mut self,
        n: P<Node>,
        flags_to_check: ModifierFlags,
    ) -> ModifierFlags {
        let mut flags = self.get_combined_modifier_flags_cached(n);
        // children of classes (even ambient classes) should not be marked as ambient or export
        // because those flags have no useful semantics there.
        let parent = n.parent().unwrap();
        if !ast::is_interface_declaration(parent)
            && !ast::is_class_declaration(parent)
            && !ast::is_class_expression(parent)
            && n.flags().intersects(NodeFlags::Ambient)
        {
            let container = get_enclosing_container(n);
            if container
                .is_some_and(|container| container.flags().intersects(NodeFlags::ExportContext))
                && !flags.intersects(ModifierFlags::Ambient)
                && !(ast::is_module_block(parent)
                    && ast::is_global_scope_augmentation(parent.parent().unwrap()))
            {
                // It is nested in an ambient export context, which means it is automatically exported
                flags |= ModifierFlags::Export;
            }
            flags |= ModifierFlags::Ambient;
        }
        flags & flags_to_check
    }

    // checker.go:3744
    fn is_implementation_compatible_with_overload(
        &mut self,
        implementation: P<Signature>,
        overload: P<Signature>,
    ) -> bool {
        let erased_source = self.get_erased_signature(implementation);
        let erased_target = self.get_erased_signature(overload);
        // First see if the return types are compatible in either direction.
        let source_return_type = self.get_return_type_of_signature(erased_source);
        let target_return_type = self.get_return_type_of_signature(erased_target);
        if target_return_type == self.void_type
            || self.is_type_related_to(
                target_return_type,
                source_return_type,
                self.assignable_relation,
            )
            || self.is_type_related_to(
                source_return_type,
                target_return_type,
                self.assignable_relation,
            )
        {
            return self.is_signature_assignable_to(
                erased_source,
                erased_target,
                true, /*ignoreReturnTypes*/
            );
        }
        false
    }

    // checker.go:3756
    pub(crate) fn check_all_code_paths_in_non_void_function_return_or_throw(
        &mut self,
        fn_: P<Node>,
        return_type: Option<P<Type>>,
    ) {
        let function_flags = ast::get_function_flags(Some(fn_));
        let mut t: Option<P<Type>> = None;
        if let Some(return_type) = return_type {
            t = self.unwrap_return_type(return_type, function_flags);
        }
        // Functions with an explicitly specified return type that includes `void` or is exactly `any` or `undefined` don't
        // need any return statements.
        if let Some(t) = t
            && (self.maybe_type_of_kind(t, TypeFlags::Void)
                || t.flags().intersects(TypeFlags::Any | TypeFlags::Undefined))
        {
            return;
        }
        // If all we have is a function signature, or an arrow function with an expression body, then there is nothing to check.
        // also if HasImplicitReturn flag is not set this means that all codepaths in function body end with return or throw
        if ast::is_method_signature_declaration(fn_)
            || ast::node_is_missing(fn_.body())
            || !ast::is_block(fn_.body().unwrap())
            || !self.function_has_implicit_return(fn_)
        {
            return;
        }
        let has_explicit_return = fn_.flags().intersects(NodeFlags::HasExplicitReturn);
        let mut error_node = fn_.type_node();
        if error_node.is_none()
            && let Some(data) = fn_.function_like_data()
            && let Some(full_signature) = data.full_signature()
        {
            error_node = Some(full_signature);
        }
        if error_node.is_none() {
            error_node = Some(fn_);
        }
        if t.is_some_and(|t| t.flags().intersects(TypeFlags::Never)) {
            self.error(
                error_node,
                &diagnostics::A_FUNCTION_RETURNING_NEVER_CANNOT_HAVE_A_REACHABLE_END_POINT,
                &[],
            );
        } else if t.is_some() && !has_explicit_return {
            // minimal check: function has syntactic return type annotation and no explicit return statements in the body
            // this function does not conform to the specification.
            self.error(error_node, &diagnostics::A_FUNCTION_WHOSE_DECLARED_TYPE_IS_NEITHER_UNDEFINED_VOID_NOR_ANY_MUST_RETURN_A_VALUE, &[]);
        } else if t.is_some()
            && self.strict_null_checks
            && !self.is_type_assignable_to(self.undefined_type, t.unwrap())
        {
            self.error(error_node, &diagnostics::FUNCTION_LACKS_ENDING_RETURN_STATEMENT_AND_RETURN_TYPE_DOES_NOT_INCLUDE_UNDEFINED, &[]);
        } else if self.compiler_options.no_implicit_returns == Tristate::True {
            if t.is_none() {
                // If return type annotation is omitted check if function has any explicit return statements.
                // If it does not have any - its inferred return type is void - don't do any checks.
                // Otherwise get inferred return type from function body and report error only if it is not void / anytype
                if !has_explicit_return {
                    return;
                }
                let signature = self.get_signature_from_declaration(fn_);
                let inferred_return_type = self.get_return_type_of_signature(signature);
                if self.is_unwrapped_return_type_undefined_void_or_any(fn_, inferred_return_type) {
                    return;
                }
            }
            self.error(error_node, &diagnostics::NOT_ALL_CODE_PATHS_RETURN_A_VALUE, &[]);
        }
    }

    // checker.go:3808
    fn is_unwrapped_return_type_undefined_void_or_any(
        &mut self,
        fn_: P<Node>,
        return_type: P<Type>,
    ) -> bool {
        let t = self.unwrap_return_type(return_type, ast::get_function_flags(Some(fn_)));
        match t {
            Some(t) => {
                self.maybe_type_of_kind(t, TypeFlags::Void)
                    || t.flags().intersects(TypeFlags::Any | TypeFlags::Undefined)
            }
            None => false,
        }
    }

    // checker.go:3813
    fn check_block(&mut self, node: P<Node>) {
        // Grammar checking for SyntaxKind.Block
        if node.kind() == Kind::Block {
            self.check_grammar_statement_in_ambient_context(node);
        }
        if ast::is_function_or_module_block(node) {
            let save_flow_analysis_disabled = self.flow_analysis_disabled;
            self.check_source_elements(node.statements());
            self.flow_analysis_disabled = save_flow_analysis_disabled;
        } else {
            self.check_source_elements(node.statements());
        }
        if node.locals().map_or(0, |locals| locals.len()) != 0 {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // checker.go:3830
    fn check_if_statement(&mut self, node: P<Node>) {
        self.check_grammar_statement_in_ambient_context(node);
        let t = self.check_truthiness_expression(node.expression().unwrap(), CheckMode::Normal);
        let data = node.as_if_statement();
        self.check_testing_known_truthy_callable_or_awaitable_or_enum_member_type(
            node.expression().unwrap(),
            t,
            Some(data.then_statement),
        );
        self.check_source_element(Some(data.then_statement));
        if ast::is_empty_statement(data.then_statement) {
            self.error(
                Some(data.then_statement),
                &diagnostics::THE_BODY_OF_AN_IF_STATEMENT_CANNOT_BE_THE_EMPTY_STATEMENT,
                &[],
            );
        }
        self.check_source_element(data.else_statement);
    }

    // checker.go:3842
    pub(crate) fn check_testing_known_truthy_callable_or_awaitable_or_enum_member_type(
        &mut self,
        cond_expr: P<Node>,
        cond_type: P<Type>,
        body: Option<P<Node>>,
    ) {
        if !self.strict_null_checks {
            return;
        }
        self.check_testing_known_truthy_types(cond_expr, cond_type, body);
    }

    // checker.go:3849
    fn check_testing_known_truthy_types(
        &mut self,
        cond_expr: P<Node>,
        cond_type: P<Type>,
        body: Option<P<Node>>,
    ) {
        let mut cond_expr = ast::skip_parentheses(cond_expr);
        self.check_testing_known_truthy_type(cond_expr, cond_type, body);
        while ast::is_binary_expression(cond_expr)
            && (cond_expr.as_binary_expression().operator_token.kind() == Kind::BarBarToken
                || cond_expr.as_binary_expression().operator_token.kind()
                    == Kind::QuestionQuestionToken)
        {
            cond_expr = ast::skip_parentheses(cond_expr.as_binary_expression().left);
            self.check_testing_known_truthy_type(cond_expr, cond_type, body);
        }
    }

    // checker.go:3858
    fn check_testing_known_truthy_type(
        &mut self,
        cond_expr: P<Node>,
        cond_type: P<Type>,
        body: Option<P<Node>>,
    ) {
        let mut location = cond_expr;
        if ast::is_logical_or_coalescing_binary_expression(cond_expr) {
            location = ast::skip_parentheses(cond_expr.as_binary_expression().right());
        }
        if ast::is_module_exports_access_expression(location) {
            return;
        }
        if ast::is_logical_or_coalescing_binary_expression(location) {
            self.check_testing_known_truthy_types(location, cond_type, body);
            return;
        }
        let mut t = cond_type;
        if location != cond_expr {
            t = self.check_expression(location);
        }
        if t.flags().intersects(TypeFlags::EnumLiteral)
            && ast::is_property_access_expression(location)
            && self
                .get_resolved_symbol_or_nil(location.expression().unwrap())
                .unwrap_or(self.unknown_symbol)
                .flags()
                .intersects(SymbolFlags::Enum)
        {
            // EnumLiteral type at condition with known value is always truthy or always falsy, likely an error
            let value = if_else(
                evaluator::is_truthy(t.as_literal_type().value.get().unwrap()),
                "true",
                "false",
            );
            self.error(
                Some(location),
                &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_0,
                &[&value],
            );
            return;
        }
        let is_property_expression_cast = ast::is_property_access_expression(location)
            && crate::is_type_assertion(location.expression().unwrap());
        if !self.has_type_facts(t, TypeFacts::Truthy) || is_property_expression_cast {
            return;
        }
        // While it technically should be invalid for any known-truthy value
        // to be tested, we de-scope to functions and Promises unreferenced in
        // the block as a heuristic to identify the most common bugs. There
        // are too many false positives for values sourced from type
        // definitions without strictNullChecks otherwise.
        let call_signatures = self.get_signatures_of_type(t, SignatureKind::Call);
        let is_promise = self.get_awaited_type_of_promise(t).is_some();
        if call_signatures.is_empty() && !is_promise {
            return;
        }
        let mut tested_node: Option<P<Node>> = None;
        if ast::is_identifier(location) {
            tested_node = Some(location);
        } else if ast::is_property_access_expression(location) {
            tested_node = location.name();
        }
        let mut tested_symbol: Option<P<Symbol>> = None;
        if let Some(tested_node) = tested_node {
            tested_symbol = self.get_symbol_at_location(tested_node, false);
        }
        if tested_symbol.is_none() && !is_promise {
            return;
        }
        let is_used = tested_symbol.is_some_and(|tested_symbol| {
            let parent = cond_expr.parent().unwrap();
            ast::is_binary_expression(parent)
                && self.is_symbol_used_in_binary_expression_chain(parent, tested_symbol)
        }) || tested_symbol.is_some_and(|tested_symbol| {
            body.is_some_and(|body| {
                self.is_symbol_used_in_condition_body(cond_expr, body, tested_node, tested_symbol)
            })
        });
        if !is_used {
            if is_promise {
                let s = self.get_type_name_for_error_display(t);
                self.error_and_maybe_suggest_await(Some(location), true, &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_TRUE_SINCE_THIS_0_IS_ALWAYS_DEFINED, &[&s]);
            } else {
                self.error(
                    Some(location),
                    &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_TRUE_SINCE_THIS_FUNCTION_IS_ALWAYS_DEFINED_DID_YOU_MEAN_TO_CALL_IT_INSTEAD,
                    &[],
                );
            }
        }
    }

    // checker.go:3918
    fn is_symbol_used_in_binary_expression_chain(
        &mut self,
        node: P<Node>,
        tested_symbol: P<Symbol>,
    ) -> bool {
        fn visit(c: &mut Checker, child: P<Node>, tested_symbol: P<Symbol>) -> bool {
            if ast::is_identifier(child) {
                let symbol = c.get_symbol_at_location(child, false);
                if symbol.is_some() && symbol == Some(tested_symbol) {
                    return true;
                }
            }
            child.for_each_child(&mut |n| visit(c, n, tested_symbol))
        }
        let mut node = node;
        while ast::is_binary_expression(node)
            && node.as_binary_expression().operator_token.kind() == Kind::AmpersandAmpersandToken
        {
            let is_used = node
                .as_binary_expression()
                .right()
                .for_each_child(&mut |n| visit(self, n, tested_symbol));
            if is_used {
                return true;
            }
            node = node.parent().unwrap();
        }
        false
    }

    // checker.go:3939
    fn is_symbol_used_in_condition_body(
        &mut self,
        expr: P<Node>,
        body: P<Node>,
        tested_node: Option<P<Node>>,
        tested_symbol: P<Symbol>,
    ) -> bool {
        fn visit(
            c: &mut Checker,
            child_node: P<Node>,
            expr: P<Node>,
            tested_node: Option<P<Node>>,
            tested_symbol: P<Symbol>,
        ) -> bool {
            if ast::is_identifier(child_node) {
                let child_symbol = c.get_symbol_at_location(child_node, false);
                if child_symbol.is_some() && child_symbol == Some(tested_symbol) {
                    // If the test was a simple identifier, the above check is sufficient
                    let tested = tested_node.unwrap();
                    if ast::is_identifier(expr)
                        || ast::is_identifier(tested)
                            && ast::is_binary_expression(tested.parent().unwrap())
                    {
                        return true;
                    }
                    // Otherwise we need to ensure the symbol is called on the same target
                    let mut tested_expression = tested.parent();
                    let mut child_expression = child_node.parent();
                    while let (Some(te), Some(ce)) = (tested_expression, child_expression) {
                        if ast::is_identifier(te) && ast::is_identifier(ce)
                            || te.kind() == Kind::ThisKeyword && ce.kind() == Kind::ThisKeyword
                        {
                            return c.get_symbol_at_location(te, false)
                                == c.get_symbol_at_location(ce, false);
                        } else if ast::is_property_access_expression(te)
                            && ast::is_property_access_expression(ce)
                        {
                            if c.get_symbol_at_location(te.name().unwrap(), false)
                                != c.get_symbol_at_location(ce.name().unwrap(), false)
                            {
                                return false;
                            }
                            child_expression = ce.expression();
                            tested_expression = te.expression();
                        } else if ast::is_call_expression(te) && ast::is_call_expression(ce) {
                            child_expression = ce.expression();
                            tested_expression = te.expression();
                        } else {
                            return false;
                        }
                    }
                }
            }
            child_node.for_each_child(&mut |n| visit(c, n, expr, tested_node, tested_symbol))
        }
        body.for_each_child(&mut |n| visit(self, n, expr, tested_node, tested_symbol))
    }

    // checker.go:3975
    fn check_do_statement(&mut self, node: P<Node>) {
        self.check_grammar_statement_in_ambient_context(node);
        self.check_source_element(Some(node.statement()));
        self.check_truthiness_expression(node.expression().unwrap(), CheckMode::Normal);
    }

    // checker.go:3981
    fn check_while_statement(&mut self, node: P<Node>) {
        self.check_grammar_statement_in_ambient_context(node);
        self.check_truthiness_expression(node.expression().unwrap(), CheckMode::Normal);
        self.check_source_element(Some(node.statement()));
    }

    // checker.go:3987
    fn check_for_statement(&mut self, node: P<Node>) {
        if !self.check_grammar_statement_in_ambient_context(node)
            && let Some(init) = node.initializer()
            && init.kind() == Kind::VariableDeclarationList
        {
            self.check_grammar_variable_declaration_list(init);
        }
        let data = node.as_for_statement();
        if let Some(initializer) = data.initializer {
            if ast::is_variable_declaration_list(initializer) {
                self.check_variable_declaration_list(initializer);
            } else {
                self.check_expression(initializer);
            }
        }
        if let Some(condition) = data.condition {
            self.check_truthiness_expression(condition, CheckMode::Normal);
        }
        if let Some(incrementor) = data.incrementor {
            self.check_expression(incrementor);
        }
        self.check_source_element(Some(node.statement()));
        if node.locals().is_some() {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // checker.go:4013
    fn check_for_in_statement(&mut self, node: P<Node>) {
        let data = node.as_for_in_or_of_statement();
        self.check_grammar_for_in_or_for_of_statement(node);
        let expression_type = self.check_expression(data.expression);
        let right_type = self.get_non_nullable_type_if_needed(expression_type);
        // TypeScript 1.0 spec (April 2014): 5.4
        // In a 'for-in' statement of the form
        // for (let VarDecl in Expr) Statement
        //   VarDecl must be a variable declaration without a type annotation that declares a variable of type Any,
        //   and Expr must be an expression of type Any, an object type, or a type parameter type.
        if ast::is_variable_declaration_list(data.initializer) {
            let declarations = data.initializer.as_variable_declaration_list().declarations.nodes();
            if !declarations.is_empty() && ast::is_binding_pattern(declarations[0].name().unwrap())
            {
                self.error(declarations[0].name(), &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_CANNOT_BE_A_DESTRUCTURING_PATTERN, &[]);
            }
            self.check_variable_declaration_list(data.initializer);
        } else {
            // In a 'for-in' statement of the form
            // for (Var in Expr) Statement
            //   Var must be an expression classified as a reference of type Any or the String primitive type,
            //   and Expr must be an expression of type Any, an object type, or a type parameter type.
            let var_expr = data.initializer;
            let left_type = self.check_expression(var_expr);
            if ast::is_array_literal_expression(var_expr)
                || ast::is_object_literal_expression(var_expr)
            {
                self.error(Some(var_expr), &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_CANNOT_BE_A_DESTRUCTURING_PATTERN, &[]);
            } else {
                let index_type = self.get_index_type_or_string(right_type);
                if self.is_type_assignable_to(index_type, left_type) {
                    // run check only former check succeeded to avoid cascading errors
                    self.check_reference_expression(
                        var_expr,
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS,
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS,
                    );
                } else {
                    self.error(Some(var_expr), &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_MUST_BE_OF_TYPE_STRING_OR_ANY, &[]);
                }
            }
        }
        // unknownType is returned i.e. if node.expression is identifier whose name cannot be resolved
        // in this case error about missing name is already reported - do not report extra one
        if right_type == self.never_type
            || !self.is_type_assignable_to_kind(
                right_type,
                TypeFlags::NonPrimitive | TypeFlags::InstantiableNonPrimitive,
            )
        {
            let s = self.type_to_string_exported(right_type);
            self.error(
                Some(data.expression),
                &diagnostics::THE_RIGHT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_MUST_BE_OF_TYPE_ANY_AN_OBJECT_TYPE_OR_A_TYPE_PARAMETER_BUT_HERE_HAS_TYPE_0,
                &[&s],
            );
        }
        self.check_source_element(Some(data.statement));
        if node.locals().is_some() {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // checker.go:4055
    fn get_index_type_or_string(&mut self, t: P<Type>) -> P<Type> {
        let index_type = self.get_index_type(t);
        let index_type = self.get_extract_string_type(index_type);
        if_else(index_type.flags().intersects(TypeFlags::Never), self.string_type, index_type)
    }

    // checker.go:4060
    fn check_for_of_statement(&mut self, node: P<Node>) {
        let data = node.as_for_in_or_of_statement();
        self.check_grammar_for_in_or_for_of_statement(node);
        let container = get_containing_function_or_class_static_block(node);
        if let Some(await_modifier) = data.await_modifier {
            if container.is_some_and(ast::is_class_static_block_declaration) {
                self.grammar_error_on_node(
                    await_modifier,
                    &diagnostics::X_FOR_AWAIT_LOOPS_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK,
                    &[],
                );
            } else {
                let function_flags = ast::get_function_flags(container);
                if (function_flags & (FunctionFlags::Invalid | FunctionFlags::Async))
                    == FunctionFlags::Async
                    && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.for_await_of
                {
                    // for..await..of in an async function or async generator function prior to ESNext requires the __asyncValues helper
                    self.check_external_emit_helpers(node, ExternalEmitHelpers::ForAwaitOfIncludes);
                }
            }
        } // Check the LHS and RHS
        // If the LHS is a declaration, just check it as a variable declaration, which will in turn check the RHS
        // via checkRightHandSideOfForOf.
        // If the LHS is an expression, check the LHS, as a destructuring assignment or as a reference.
        // Then check that the RHS is assignable to it.
        if ast::is_variable_declaration_list(data.initializer) {
            self.check_variable_declaration_list(data.initializer);
        } else {
            let var_expr = data.initializer;
            let iterated_type = self.check_right_hand_side_of_for_of(node);
            // There may be a destructuring assignment on the left side
            if ast::is_array_literal_expression(var_expr)
                || ast::is_object_literal_expression(var_expr)
            {
                // iteratedType may be undefined. In this case, we still want to check the structure of
                // varExpr, in particular making sure it's a valid LeftHandSideExpression. But we'd like
                // to short circuit the type relation checking as much as possible, so we pass the unknownType.
                // checkRightHandSideOfForOf never returns nil (checkIteratedTypeOrElementType falls back to
                // anyType), so Go's `core.OrElse(iteratedType, c.errorType)` is the identity here.
                self.check_destructuring_assignment(
                    var_expr,
                    iterated_type,
                    CheckMode::Normal,
                    false,
                );
            } else {
                let left_type = self.check_expression(var_expr);
                self.check_reference_expression(
                    var_expr,
                    &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_OF_STATEMENT_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS,
                    &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_OF_STATEMENT_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS,
                );
                // iteratedType will be undefined if the rightType was missing properties/signatures
                // required to get its iteratedType (like [Symbol.iterator] or next). This may be
                // because we accessed properties from anyType, or it may have led to an error inside
                // getElementTypeOfIterable.
                // (Go's `iteratedType != nil` guard is always true, see above.)
                self.check_type_assignable_to_and_optionally_elaborate(
                    iterated_type,
                    left_type,
                    Some(var_expr),
                    Some(data.expression),
                    None,
                    None,
                );
            }
        }
        self.check_source_element(Some(data.statement));
        if node.locals().is_some() {
            self.register_for_unused_identifiers_check(node);
        }
    }

    // checker.go:4108
    fn check_break_or_continue_statement(&mut self, node: P<Node>) {
        if !self.check_grammar_statement_in_ambient_context(node) {
            self.check_grammar_break_or_continue_statement(node);
        }
    }

    // checker.go:4114
    fn check_return_statement(&mut self, node: P<Node>) {
        // Always check the return expression so its identifiers are resolved even when the
        // return statement is misplaced (grammar error), keeping diagnostics stable
        // regardless of traversal order.
        let expr_node = node.expression();
        let mut expr_type = self.undefined_type;
        if let Some(expr_node) = expr_node {
            expr_type = self.check_expression_cached(expr_node);
        }
        if self.check_grammar_statement_in_ambient_context(node) {
            return;
        }
        let container = get_containing_function_or_class_static_block(node);
        if container.is_some_and(ast::is_class_static_block_declaration) {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::A_RETURN_STATEMENT_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK,
                &[],
            );
            return;
        }
        let Some(container) = container else {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::A_RETURN_STATEMENT_CAN_ONLY_BE_USED_WITHIN_A_FUNCTION_BODY,
                &[],
            );
            return;
        };
        let signature = self.get_signature_from_declaration(container);
        let return_type = self.get_return_type_of_signature(signature);
        let function_flags = ast::get_function_flags(Some(container));
        if self.strict_null_checks
            || expr_node.is_some()
            || return_type.flags().intersects(TypeFlags::Never)
        {
            if ast::is_set_accessor_declaration(container) {
                if expr_node.is_some() {
                    self.error(Some(node), &diagnostics::SETTERS_CANNOT_RETURN_A_VALUE, &[]);
                }
            } else if ast::is_constructor_declaration(container) {
                if let Some(expr_node) = expr_node
                    && !self.check_type_assignable_to_and_optionally_elaborate(
                        expr_type,
                        return_type,
                        Some(node),
                        Some(expr_node),
                        None,
                        None,
                    )
                {
                    self.error(Some(node), &diagnostics::RETURN_TYPE_OF_CONSTRUCTOR_SIGNATURE_MUST_BE_ASSIGNABLE_TO_THE_INSTANCE_TYPE_OF_THE_CLASS, &[]);
                }
            } else if self.get_return_type_from_annotation(container).is_some() {
                let unwrapped_return_type =
                    self.unwrap_return_type(return_type, function_flags).unwrap_or(return_type);
                self.check_return_expression(
                    container,
                    unwrapped_return_type,
                    node,
                    node.expression(),
                    expr_type,
                    false,
                );
            }
        } else if !ast::is_constructor_declaration(container)
            && self.compiler_options.no_implicit_returns.is_true()
            && !self.is_unwrapped_return_type_undefined_void_or_any(container, return_type)
        {
            // The function has a return type, but the return statement doesn't have an expression.
            self.error(Some(node), &diagnostics::NOT_ALL_CODE_PATHS_RETURN_A_VALUE, &[]);
        }
    }

    // When checking an arrow expression such as `(x) => exp`, then `node` is the expression `exp`.
    // Otherwise, `node` is a return statement.
    // checker.go:4159
    pub(crate) fn check_return_expression(
        &mut self,
        container: P<Node>,
        unwrapped_return_type: P<Type>,
        node: P<Node>,
        expr: Option<P<Node>>,
        expr_type: P<Type>,
        in_conditional_expression: bool,
    ) {
        let mut unwrapped_expr_type = expr_type;
        let function_flags = ast::get_function_flags(Some(container));
        if let Some(expr) = expr {
            let unwrapped_expr = ast::skip_parentheses(expr);
            if ast::is_conditional_expression(unwrapped_expr) {
                let when_true = unwrapped_expr.as_conditional_expression().when_true;
                let when_false = unwrapped_expr.as_conditional_expression().when_false;
                let when_true_type = self.check_expression(when_true);
                self.check_return_expression(
                    container,
                    unwrapped_return_type,
                    node,
                    Some(when_true),
                    when_true_type,
                    true, /*inConditionalExpression*/
                );
                let when_false_type = self.check_expression(when_false);
                self.check_return_expression(
                    container,
                    unwrapped_return_type,
                    node,
                    Some(when_false),
                    when_false_type,
                    true, /*inConditionalExpression*/
                );
                return;
            }
        }
        let in_return_statement = node.kind() == Kind::ReturnStatement;
        if function_flags.intersects(FunctionFlags::Async) {
            unwrapped_expr_type = self.check_awaited_type(
                expr_type,
                false, /*withAlias*/
                node,
                &diagnostics::THE_RETURN_TYPE_OF_AN_ASYNC_FUNCTION_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER,
            );
        }
        let mut effective_expr = expr; // The effective expression for diagnostics purposes.
        if let Some(expr) = expr {
            effective_expr = Some(Self::get_effective_check_node(expr));
        }
        let error_node =
            if_else(in_return_statement && !in_conditional_expression, Some(node), effective_expr);
        self.check_type_assignable_to_and_optionally_elaborate(
            unwrapped_expr_type,
            unwrapped_return_type,
            error_node,
            effective_expr,
            None,
            None,
        );
    }

    // checker.go:4184
    fn check_with_statement(&mut self, node: P<Node>) {
        if !self.check_grammar_statement_in_ambient_context(node)
            && node.flags().intersects(NodeFlags::AwaitContext)
        {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::X_WITH_STATEMENTS_ARE_NOT_ALLOWED_IN_AN_ASYNC_FUNCTION_BLOCK,
                &[],
            );
        }
        self.check_expression(node.expression().unwrap());
        let source_file = ast::get_source_file_of_node(node).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            let start = tsrs_scanner::skip_trivia(
                source_file.text(),
                node.pos().expect_source("parsed nodes have source positions"),
            );
            let end = node.statement().pos().expect_source("parsed nodes have source positions");
            self.grammar_error_at_pos(
                source_file.as_node(),
                start,
                end - start,
                &diagnostics::THE_WITH_STATEMENT_IS_NOT_SUPPORTED_ALL_SYMBOLS_IN_A_WITH_BLOCK_WILL_HAVE_TYPE_ANY,
                &[],
            );
        }
    }

    // checker.go:4199
    fn check_switch_statement(&mut self, node: P<Node>) {
        // Grammar checking
        self.check_grammar_statement_in_ambient_context(node);
        let mut first_default_clause: Option<P<Node>> = None;
        let mut has_duplicate_default_clause = false;
        let expression_type = self.check_expression(node.expression().unwrap());
        let case_block = node.as_switch_statement().case_block;
        for &clause in case_block.as_case_block().clauses.nodes() {
            // Grammar check for duplicate default clauses, skip if we already report duplicate default clause
            if ast::is_default_clause(clause) && !has_duplicate_default_clause {
                if first_default_clause.is_none() {
                    first_default_clause = Some(clause);
                } else {
                    self.grammar_error_on_node(clause, &diagnostics::A_DEFAULT_CLAUSE_CANNOT_APPEAR_MORE_THAN_ONCE_IN_A_SWITCH_STATEMENT, &[]);
                    has_duplicate_default_clause = true;
                }
            }
            if ast::is_case_clause(clause) {
                let case_type = self.check_expression(clause.expression().unwrap());
                if !self.is_type_equality_comparable_to(expression_type, case_type) {
                    // expressionType is not comparable to caseType, try the reversed check and report errors if it fails
                    self.check_type_comparable_to(
                        case_type,
                        expression_type,
                        clause.expression().unwrap(),
                        None, /*headMessage*/
                    );
                }
            }
            self.check_source_elements(clause.statements());
            if self.compiler_options.no_fallthrough_cases_in_switch.is_true()
                && let Some(flow_node) =
                    clause.as_case_or_default_clause().fallthrough_flow_node.get()
                && self.is_reachable_flow_node(flow_node)
            {
                self.error(Some(clause), &diagnostics::FALLTHROUGH_CASE_IN_SWITCH, &[]);
            }
        }
        if case_block.locals().is_some() {
            self.register_for_unused_identifiers_check(case_block);
        }
    }

    // checker.go:4235
    fn check_labeled_statement(&mut self, node: P<Node>) {
        let labeled_statement = node.as_labeled_statement();
        let label_node = labeled_statement.label;
        let label_text = label_node.text();
        if !self.check_grammar_statement_in_ambient_context(node) {
            let mut current = node.parent();
            while let Some(cur) = current {
                if ast::is_function_like(cur) {
                    break;
                }
                if ast::is_labeled_statement(cur) && cur.label().unwrap().text() == label_text {
                    self.grammar_error_on_node(
                        label_node,
                        &diagnostics::DUPLICATE_LABEL_0,
                        &[&label_text],
                    );
                    break;
                }
                current = cur.parent();
            }
        }
        if label_node.flags().intersects(NodeFlags::Unreachable)
            && self.compiler_options.allow_unused_labels != Tristate::True
        {
            self.error_or_suggestion(
                self.compiler_options.allow_unused_labels == Tristate::False,
                Some(label_node),
                &diagnostics::UNUSED_LABEL,
                &[],
            );
        }
        self.check_source_element(Some(labeled_statement.statement));
    }

    // checker.go:4253
    fn check_throw_statement(&mut self, node: P<Node>) {
        let throw_expr = node.expression().unwrap();
        if !self.check_grammar_statement_in_ambient_context(node)
            && ast::is_identifier(throw_expr)
            && throw_expr.text().is_empty()
        {
            self.grammar_error_at_pos(
                node,
                throw_expr.pos().expect_source("parsed nodes have source positions"),
                0, /*length*/
                &diagnostics::LINE_BREAK_NOT_PERMITTED_HERE,
                &[],
            );
        }
        self.check_expression(throw_expr);
    }

    // checker.go:4263
    fn check_try_statement(&mut self, node: P<Node>) {
        self.check_grammar_statement_in_ambient_context(node);
        let data = node.as_try_statement();
        self.check_block(data.try_block);
        if let Some(catch_clause) = data.catch_clause {
            self.check_catch_clause(catch_clause);
        }
        if let Some(finally_block) = data.finally_block {
            self.check_block(finally_block);
        }
    }

    // checker.go:4275
    fn check_catch_clause(&mut self, node: P<Node>) {
        let declaration = node.as_catch_clause().variable_declaration;
        if let Some(declaration) = declaration {
            self.check_variable_like_declaration(declaration);
            let type_node = declaration.type_node();
            if let Some(type_node) = type_node {
                let t = self.get_type_from_type_node(type_node);
                if !t.flags().intersects(TypeFlags::AnyOrUnknown) {
                    self.grammar_error_on_first_token(type_node, &diagnostics::CATCH_CLAUSE_VARIABLE_TYPE_ANNOTATION_MUST_BE_ANY_OR_UNKNOWN_IF_SPECIFIED, &[]);
                }
            } else if let Some(initializer) = declaration.initializer() {
                self.grammar_error_on_first_token(
                    initializer,
                    &diagnostics::CATCH_CLAUSE_VARIABLE_CANNOT_HAVE_AN_INITIALIZER,
                    &[],
                );
            } else {
                let block_locals = node.as_catch_clause().block.locals();
                if let Some(block_locals) = block_locals {
                    let caught_names: Vec<&'static str> =
                        node.locals().map(|locals| locals.keys()).unwrap_or_default();
                    for caught_name in caught_names {
                        if let Some(block_local) = block_locals.lookup(caught_name)
                            && let Some(value_declaration) = block_local.value_declaration()
                            && block_local.flags().intersects(SymbolFlags::BlockScopedVariable)
                        {
                            self.grammar_error_on_node(
                                value_declaration,
                                &diagnostics::CANNOT_REDECLARE_IDENTIFIER_0_IN_CATCH_CLAUSE,
                                &[&caught_name],
                            );
                        }
                    }
                }
            }
        }
        self.check_block(node.as_catch_clause().block);
    }

    // checker.go:4301
    fn check_binding_element(&mut self, node: P<Node>) {
        self.check_grammar_binding_element(node);
        self.check_variable_like_declaration(node);
    }

    // checker.go:4306
    fn check_class_declaration(&mut self, node: P<Node>) {
        let first_decorator = node.modifier_nodes().iter().copied().find(|&m| ast::is_decorator(m));
        if self.legacy_decorators
            && let Some(first_decorator) = first_decorator
            && node.members().iter().any(|&p| {
                ast::has_static_modifier(p)
                    && ast::is_private_identifier_class_element_declaration(p)
            })
        {
            self.grammar_error_on_node(
                first_decorator,
                &diagnostics::CLASS_DECORATORS_CAN_T_BE_USED_WITH_STATIC_PRIVATE_IDENTIFIER_CONSIDER_REMOVING_THE_EXPERIMENTAL_DECORATOR,
                &[],
            );
        }
        if node.name().is_none() && !ast::has_syntactic_modifier(node, ModifierFlags::Default) {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::A_CLASS_DECLARATION_WITHOUT_THE_DEFAULT_MODIFIER_MUST_HAVE_A_NAME,
                &[],
            );
        }
        self.check_class_like_declaration(node);
        self.check_source_elements(node.members());
        self.check_constructor_declared_properties(node);
        self.register_for_unused_identifiers_check(node);
    }

    // checker.go:4324
    pub(crate) fn check_constructor_declared_properties(&mut self, node: P<Node>) {
        if !ast::is_in_js_file(node) {
            return;
        }
        let class_symbol = self.get_symbol_of_declaration(node).unwrap();
        let class_type = self.get_declared_type_of_symbol(class_symbol);
        for &property in self.get_properties_of_type(class_type) {
            let (kind, constructor) = self.is_constructor_declared_this_property(property);
            if kind == ThisAssignmentDeclarationKind::Constructor
                && constructor.is_some_and(|constructor| constructor.parent() == Some(node))
            {
                self.get_type_of_symbol(property);
            }
        }
    }
}
