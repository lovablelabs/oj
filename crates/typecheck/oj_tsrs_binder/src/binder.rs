use bitflags::bitflags;
use rustc_hash::FxHashSet;
use tsrs_ast as ast;
use tsrs_ast::lazylist::{self, LazyBindContext, LazyNodeList};
use tsrs_ast::{
    Diagnostic, DiagnosticExt, FlowFlags, FlowList, FlowNode, Kind, ModifierFlags, Node, NodeFlags,
    NodeList, SourceFile, Symbol, SymbolFlags, SymbolTable,
};
use tsrs_core::{OwnedCell, P, PatternKind, alloc_str, tspath};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;
use tsrs_scanner as scanner;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ContainerFlags: u32 {
        // The current node is not a container, and no container manipulation should happen before
        // recursing into it.
        const None = 0;
        // The current node is a container.  It should be set as the current container (and block-
        // container) before recursing into it.  The current node does not have locals.  Examples:
        //
        //      Classes, ObjectLiterals, TypeLiterals, Interfaces...
        const IsContainer = 1 << 0;
        // The current node is a block-scoped-container.  It should be set as the current block-
        // container before recursing into it.  Examples:
        //
        //      Blocks (when not parented by functions), Catch clauses, For/For-in/For-of statements...
        const IsBlockScopedContainer = 1 << 1;
        // The current node is the container of a control flow path. The current control flow should
        // be saved and restored, and a new control flow initialized within the container.
        const IsControlFlowContainer = 1 << 2;
        const IsFunctionLike = 1 << 3;
        const IsFunctionExpression = 1 << 4;
        const HasLocals = 1 << 5;
        const IsInterface = 1 << 6;
        const IsObjectLiteralOrClassExpressionMethodOrAccessor = 1 << 7;
        const IsThisContainer = 1 << 8;
        const PropagatesThisKeyword = 1 << 9;
    }
}

#[derive(Clone, Copy)]
pub struct ExpandoAssignmentInfo {
    node: P<Node>,
    container: Option<P<Node>>,
    block_scope_container: Option<P<Node>>,
}

struct Binder {
    file: P<SourceFile>,
    unreachable_flow: P<FlowNode>,

    container: Option<P<Node>>,
    this_container: Option<P<Node>>,
    block_scope_container: Option<P<Node>>,
    last_container: Option<P<Node>>,
    current_flow: Option<P<FlowNode>>,
    current_break_target: Option<P<FlowNode>>,
    current_continue_target: Option<P<FlowNode>>,
    current_return_target: Option<P<FlowNode>>,
    current_true_target: Option<P<FlowNode>>,
    current_false_target: Option<P<FlowNode>>,
    current_exception_target: Option<P<FlowNode>>,
    pre_switch_case_flow: Option<P<FlowNode>>,
    // Go keeps a linked list of ActiveLabel; the head of the list is the last element here.
    active_label_list: Vec<ActiveLabel>,
    emit_flags: NodeFlags,
    seen_this_keyword: bool,
    has_explicit_return: bool,
    has_flow_effects: bool,
    in_assignment_pattern: bool,
    seen_parse_error: bool,
    symbol_count: usize,
    not_const_enum_only_modules: FxHashSet<P<Symbol>>,
    expando_assignments: Vec<ExpandoAssignmentInfo>,
    // Go appends to file.BindDiagnostics on every report; they are collected here and stored on the
    // file once binding completes (nothing reads them in between).
    bind_diagnostics: Vec<P<Diagnostic>>,
}

struct ActiveLabel {
    break_target: Option<P<FlowNode>>,
    continue_target: Option<P<FlowNode>>,
    name: &'static str,
    referenced: bool,
}

impl ActiveLabel {
    fn break_target(&self) -> Option<P<FlowNode>> {
        self.break_target
    }
    fn continue_target(&self) -> Option<P<FlowNode>> {
        self.continue_target
    }
}

pub fn bind_source_file(file: P<SourceFile>) {
    if !file.is_bound() {
        bind_source_file_worker(file);
    }
}

fn bind_source_file_worker(file: P<SourceFile>) {
    file.bind_once(|| {
        let mut b = Binder::new(file);
        b.bind(file.as_node());
        b.bind_deferred_expando_assignments();
        file.set_bind_diagnostics(&b.bind_diagnostics);
        file.symbol_count.set(b.symbol_count);
        for &lazy in file.lazy_lists.get() {
            lazy.mark_unbound();
        }
    });
}

/// tsrs-only: bind lazily parsed member lists of declaration files on first use (`tsrs_ast` `lazylist`); with
/// `tsrs_parser::enable_lazy_dts`.
pub fn enable_lazy_dts() {
    lazylist::set_bind_hook(bind_lazy_list);
}

/// The first reader of a deferred lazy list (any thread, after the file was bound): binds its members with the
/// binder state recorded at the list. Its bind diagnostics and symbol count are dropped: the CLI does not report bind
/// diagnostics of files it does not check (only lists of such files are lazy), and `--extendedDiagnostics` counts only
/// the symbols of lists bound while their file was bound (lists bound here, on first read, are not counted).
fn bind_lazy_list(record: &LazyNodeList, file: P<SourceFile>, nodes: &'static [P<Node>]) {
    let ctx = *record.bind_context().expect("a deferred list's binder state");
    let mut b = Binder::new_for_lazy_list(file, ctx.unreachable_flow);
    b.bind_lazy_members(ctx, nodes, false);
}

fn new_flow_node_value(
    flags: FlowFlags,
    node: Option<P<Node>>,
    antecedent: Option<P<FlowNode>>,
    text_index: u32,
) -> P<FlowNode> {
    P::new_recycled(FlowNode::new(flags, node, antecedent, text_index))
}

impl Binder {
    fn new_for_lazy_list(file: P<SourceFile>, unreachable_flow: P<FlowNode>) -> Binder {
        let mut b = Binder::new_with(file, unreachable_flow, Vec::new());
        b.symbol_count = 0;
        b
    }

    fn new(file: P<SourceFile>) -> Binder {
        let unreachable_flow =
            new_flow_node_value(FlowFlags::Unreachable, None, None, file.text_index.get());
        Binder::new_with(file, unreachable_flow, file.bind_diagnostics().to_vec())
    }

    fn new_with(
        file: P<SourceFile>,
        unreachable_flow: P<FlowNode>,
        bind_diagnostics: Vec<P<Diagnostic>>,
    ) -> Binder {
        Binder {
            file,
            unreachable_flow,
            container: None,
            this_container: None,
            block_scope_container: None,
            last_container: None,
            current_flow: None,
            current_break_target: None,
            current_continue_target: None,
            current_return_target: None,
            current_true_target: None,
            current_false_target: None,
            current_exception_target: None,
            pre_switch_case_flow: None,
            active_label_list: Vec::new(),
            emit_flags: NodeFlags::None,
            seen_this_keyword: false,
            has_explicit_return: false,
            has_flow_effects: false,
            in_assignment_pattern: false,
            seen_parse_error: false,
            symbol_count: 0,
            not_const_enum_only_modules: FxHashSet::default(),
            expando_assignments: Vec::new(),
            bind_diagnostics,
        }
    }

    fn container(&self) -> P<Node> {
        self.container.unwrap()
    }

    fn current_flow(&self) -> P<FlowNode> {
        self.current_flow.unwrap()
    }

    fn new_symbol(&mut self, flags: SymbolFlags, name: &'static str) -> P<Symbol> {
        self.symbol_count += 1;
        Symbol::new(flags, name)
    }

    /**
     * Declares a Symbol for the node and adds it to symbols. Reports errors for conflicting identifier names.
     * @param symbolTable - The symbol table which node will be added to.
     * @param parent - node's parent declaration.
     * @param node - The declaration to be added to the symbol table
     * @param includes - The `SymbolFlags` that node has in addition to its declaration type (eg: export, ambient, etc.)
     * @param excludes - The flags which node cannot be declared alongside in a symbol table. Used to report forbidden declarations.
     */
    fn declare_symbol(
        &mut self,
        symbol_table: P<SymbolTable>,
        parent: Option<P<Symbol>>,
        node: P<Node>,
        includes: SymbolFlags,
        excludes: SymbolFlags,
    ) -> P<Symbol> {
        self.declare_symbol_ex(
            symbol_table,
            parent,
            node,
            includes,
            excludes,
            false, /*isReplaceableByMethod*/
            false, /*isComputedName*/
        )
    }

    fn declare_symbol_ex(
        &mut self,
        symbol_table: P<SymbolTable>,
        parent: Option<P<Symbol>>,
        node: P<Node>,
        includes: SymbolFlags,
        excludes: SymbolFlags,
        is_replaceable_by_method: bool,
        is_computed_name: bool,
    ) -> P<Symbol> {
        assert!(is_computed_name || !ast::has_dynamic_name(node));
        let is_default_export = ast::has_syntactic_modifier(node, ModifierFlags::Default)
            || ast::is_export_specifier(node)
                && ast::module_export_name_is_default(node.name().unwrap());
        // The exported symbol for an export default function/class node is always named "default"
        let name: &'static str = if is_computed_name {
            ast::INTERNAL_SYMBOL_NAME_COMPUTED
        } else if is_default_export && parent.is_some() {
            ast::INTERNAL_SYMBOL_NAME_DEFAULT
        } else {
            Self::get_declaration_name(node)
        };
        let mut symbol: P<Symbol>;
        if name == ast::INTERNAL_SYMBOL_NAME_MISSING {
            symbol = self.new_symbol(SymbolFlags::None, ast::INTERNAL_SYMBOL_NAME_MISSING);
        } else {
            // Check and see if the symbol table already has a symbol with this name.  If not,
            // create a new symbol with this name and add it to the table.  Note that we don't
            // give the new symbol any flags *yet*.  This ensures that it will not conflict
            // with the 'excludes' flags we pass in.
            //
            // If we do get an existing symbol, see if it conflicts with the new symbol we're
            // creating.  For example, a 'var' symbol and a 'class' symbol will conflict within
            // the same symbol table.  If we have a conflict, report the issue on each
            // declaration we have for this symbol, and then create a new symbol for this
            // declaration.
            //
            // Note that when properties declared in Javascript constructors
            // (marked by isReplaceableByMethod) conflict with another symbol, the property loses.
            // Always. This allows the common Javascript pattern of overwriting a prototype method
            // with an bound instance method of the same type: `this.method = this.method.bind(this)`
            //
            // If we created a new symbol, either because we didn't have a symbol with this name
            // in the symbol table, or we conflicted with an existing symbol, then just add this
            // node as the sole declaration of the new symbol.
            //
            // Otherwise, we'll be merging into a compatible existing symbol (for example when
            // you have multiple 'vars' with the same name in the same container).  In this case
            // just add this node into the declarations list of the symbol.
            match (*symbol_table).get(name) {
                None => {
                    symbol = self.new_symbol(SymbolFlags::None, name);
                    symbol_table.set(name, symbol);
                    if is_replaceable_by_method {
                        symbol.flags.set(symbol.flags.get() | SymbolFlags::ReplaceableByMethod);
                    }
                }
                Some(existing) => {
                    symbol = existing;
                    if is_replaceable_by_method
                        && !symbol.flags.get().intersects(SymbolFlags::ReplaceableByMethod)
                    {
                        // A symbol already exists, so don't add this as a declaration.
                        return symbol;
                    } else if symbol.flags.get().intersects(excludes) {
                        if symbol.flags.get().intersects(SymbolFlags::ReplaceableByMethod) {
                            // Javascript constructor-declared symbols can be discarded in favor of
                            // prototype symbols like methods.
                            symbol = self.new_symbol(SymbolFlags::None, name);
                            symbol_table.set(name, symbol);
                        } else if !(includes.intersects(SymbolFlags::Variable)
                            && symbol.flags.get().intersects(SymbolFlags::Assignment)
                            || includes.intersects(SymbolFlags::Assignment)
                                && symbol.flags.get().intersects(SymbolFlags::Variable))
                        {
                            // Assignment declarations are allowed to merge with variables, no matter what other flags they have.
                            // Report errors every position with duplicate declaration
                            // Report errors on previous encountered declarations
                            symbol = self.report_duplicate_declaration(
                                node,
                                symbol,
                                includes,
                                name,
                                is_default_export,
                            );
                        }
                    }
                }
            }
        }
        self.add_declaration_to_symbol(symbol, node, includes);
        match symbol.parent() {
            None => symbol.set_parent(parent),
            Some(existing_parent) => {
                assert!(
                    Some(existing_parent) == parent,
                    "Existing symbol parent should match new one"
                );
            }
        }
        symbol
    }

    #[cold]
    fn report_duplicate_declaration(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
        includes: SymbolFlags,
        name: &'static str,
        is_default_export: bool,
    ) -> P<Symbol> {
        let mut message: &'static Message =
            if symbol.flags.get().intersects(SymbolFlags::BlockScopedVariable) {
                &diagnostics::CANNOT_REDECLARE_BLOCK_SCOPED_VARIABLE_0
            } else {
                &diagnostics::DUPLICATE_IDENTIFIER_0
            };
        let mut message_needs_name = true;
        if symbol.flags.get().intersects(SymbolFlags::Enum)
            || includes.intersects(SymbolFlags::Enum)
        {
            message = &diagnostics::ENUM_DECLARATIONS_CAN_ONLY_MERGE_WITH_NAMESPACE_OR_OTHER_ENUM_DECLARATIONS;
            message_needs_name = false;
        }
        let mut multiple_default_exports = false;
        let declarations_len = symbol.declarations().len();
        if declarations_len != 0 {
            // If the current node is a default export of some sort, then check if
            // there are any other default exports that we need to error on.
            // We'll know whether we have other default exports depending on if `symbol` already has a declaration list set.
            if is_default_export {
                message = &diagnostics::A_MODULE_CANNOT_HAVE_MULTIPLE_DEFAULT_EXPORTS;
                message_needs_name = false;
                multiple_default_exports = true;
            } else {
                // This is to properly report an error in the case "export default { }" is after export default of class declaration or function declaration.
                // Error on multiple export default in the following case:
                // 1. multiple export default of class declaration or function declaration by checking NodeFlags.Default
                // 2. multiple export default of export assignment. This one doesn't have NodeFlags.Default on (as export default doesn't considered as modifiers)
                if declarations_len != 0
                    && ast::is_export_assignment(node)
                    && !node.as_export_assignment().is_export_equals()
                {
                    message = &diagnostics::A_MODULE_CANNOT_HAVE_MULTIPLE_DEFAULT_EXPORTS;
                    message_needs_name = false;
                    multiple_default_exports = true;
                }
            }
        }
        let declaration_name = ast::get_name_of_declaration(Some(node)).unwrap_or(node);
        let diag = if message_needs_name {
            let display_name = Self::get_display_name(node);
            self.create_diagnostic_for_node(declaration_name, message, &[&display_name])
        } else {
            self.create_diagnostic_for_node(declaration_name, message, &[])
        };
        if ast::is_type_alias_declaration(node)
            && ast::node_is_missing(node.type_node())
            && ast::has_syntactic_modifier(node, ModifierFlags::Export)
            && symbol
                .flags
                .get()
                .intersects(SymbolFlags::Alias | SymbolFlags::Type | SymbolFlags::Namespace)
        {
            // export type T; - may have meant export type { T }?
            let suggestion = format!("export type {{ {} }}", node.name().unwrap().text());
            diag.add_related_info(self.create_diagnostic_for_node(
                node,
                &diagnostics::DID_YOU_MEAN_0,
                &[&suggestion],
            ));
        }
        let declarations = symbol.declarations();
        for (index, declaration) in declarations.iter().copied().enumerate() {
            let decl = ast::get_name_of_declaration(Some(declaration)).unwrap_or(declaration);
            let d = if message_needs_name {
                let display_name = Self::get_display_name(declaration);
                self.create_diagnostic_for_node(decl, message, &[&display_name])
            } else {
                self.create_diagnostic_for_node(decl, message, &[])
            };
            if multiple_default_exports {
                let related_message: &'static Message = if index == 0 {
                    &diagnostics::ANOTHER_EXPORT_DEFAULT_IS_HERE
                } else {
                    &diagnostics::X_AND_HERE
                };
                d.add_related_info(self.create_diagnostic_for_node(
                    declaration_name,
                    related_message,
                    &[],
                ));
            }
            self.add_diagnostic(d);
            if multiple_default_exports {
                diag.add_related_info(self.create_diagnostic_for_node(
                    decl,
                    &diagnostics::THE_FIRST_EXPORT_DEFAULT_IS_HERE,
                    &[],
                ));
            }
        }
        self.add_diagnostic(diag);
        // When get or set accessor conflicts with a non-accessor or an accessor of a different kind, we mark
        // the symbol as a full accessor such that all subsequent declarations are considered conflicting. This
        // for example ensures that a get accessor followed by a non-accessor followed by a set accessor with the
        // same name are all marked as duplicates.
        if symbol.flags.get().intersects(SymbolFlags::Accessor)
            && (symbol.flags.get() & SymbolFlags::Accessor) != (includes & SymbolFlags::Accessor)
        {
            symbol.flags.set(symbol.flags.get() | SymbolFlags::Accessor);
        }
        self.new_symbol(SymbolFlags::None, name)
    }

    // Should not be called on a declaration with a computed property name,
    // unless it is a well known Symbol.
    fn get_declaration_name(node: P<Node>) -> &'static str {
        if ast::is_export_assignment(node) {
            return if node.as_export_assignment().is_export_equals() {
                ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
            } else {
                ast::INTERNAL_SYMBOL_NAME_DEFAULT
            };
        }
        let name = ast::get_name_of_declaration(Some(node));
        if let Some(name) = name {
            if ast::is_ambient_module(node) {
                let module_name = name.text();
                if ast::is_global_scope_augmentation(node) {
                    return ast::INTERNAL_SYMBOL_NAME_GLOBAL;
                }
                let pattern = tsrs_core::try_parse_pattern(module_name);
                if matches!(pattern.kind, PatternKind::Wildcard(_))
                    && let Some(attributes) = node.as_module_declaration().attributes()
                {
                    return alloc_str(&format!(
                        "{}\"{}\"pattern@{}",
                        ast::INTERNAL_SYMBOL_NAME_PREFIX,
                        module_name,
                        ast::get_node_id(attributes).0
                    ));
                }
                return alloc_str(&format!("\"{module_name}\""));
            }
            if ast::is_private_identifier(name) {
                // containingClass exists because private names only allowed inside classes
                let Some(containing_class) = ast::get_containing_class(node) else {
                    // we can get here in cases where there is already a parse error.
                    return ast::INTERNAL_SYMBOL_NAME_MISSING;
                };
                return alloc_str(&get_symbol_name_for_private_identifier(
                    containing_class.symbol().unwrap(),
                    name.text(),
                ));
            }
            if ast::is_property_name_literal(name) || ast::is_jsx_namespaced_name(name) {
                return name.text();
            }
            if ast::is_computed_property_name(name) {
                let name_expression = name.expression().unwrap();
                // treat computed property names where expression is string/numeric literal as just string/numeric literal
                if ast::is_string_or_numeric_literal_like(name_expression) {
                    return name_expression.text();
                }
                if ast::is_signed_numeric_literal(name_expression) {
                    let unary_expression = name_expression.as_prefix_unary_expression();
                    return alloc_str(&format!(
                        "{}{}",
                        scanner::token_to_string(unary_expression.operator()),
                        unary_expression.operand().text()
                    ));
                }
                panic!("Only computed properties with literal names have declaration names");
            }
            return ast::INTERNAL_SYMBOL_NAME_MISSING;
        }
        match node.kind() {
            Kind::Constructor => return ast::INTERNAL_SYMBOL_NAME_CONSTRUCTOR,
            Kind::FunctionType | Kind::CallSignature => return ast::INTERNAL_SYMBOL_NAME_CALL,
            Kind::ConstructorType | Kind::ConstructSignature => {
                return ast::INTERNAL_SYMBOL_NAME_NEW;
            }
            Kind::IndexSignature => return ast::INTERNAL_SYMBOL_NAME_INDEX,
            Kind::ExportDeclaration => return ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR,
            Kind::SourceFile | Kind::BinaryExpression => {
                return ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS;
            }
            _ => {}
        }
        ast::INTERNAL_SYMBOL_NAME_MISSING
    }

    fn get_display_name(node: P<Node>) -> String {
        if let Some(name_node) = node.name() {
            return scanner::declaration_name_to_string(Some(name_node));
        }
        let name = Self::get_declaration_name(node);
        if name != ast::INTERNAL_SYMBOL_NAME_MISSING {
            return name.to_string();
        }
        "(Missing)".to_string()
    }
}

pub fn get_symbol_name_for_private_identifier(
    containing_class_symbol: P<Symbol>,
    description: &str,
) -> String {
    format!(
        "{}#{}@{}",
        ast::INTERNAL_SYMBOL_NAME_PREFIX,
        ast::get_symbol_id(containing_class_symbol).0,
        description
    )
}

impl Binder {
    fn declare_module_member(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) -> P<Symbol> {
        let container = self.container();
        let has_export_modifier = ast::get_combined_modifier_flags(node)
            .intersects(ModifierFlags::Export)
            || ast::is_implicitly_exported_jsdoc_declaration(node);
        if symbol_flags.intersects(SymbolFlags::Alias) {
            if node.kind() == Kind::ExportSpecifier
                || (node.kind() == Kind::ImportEqualsDeclaration && has_export_modifier)
            {
                return self.declare_symbol(
                    ast::get_exports(container.symbol().unwrap()),
                    container.symbol(),
                    node,
                    symbol_flags,
                    symbol_excludes,
                );
            }
            return self.declare_symbol(
                ast::get_locals(container),
                None, /*parent*/
                node,
                symbol_flags,
                symbol_excludes,
            );
        }
        // Exported module members are given 2 symbols: A local symbol that is classified with an ExportValue flag,
        // and an associated export symbol with all the correct flags set on it. There are 2 main reasons:
        //
        //   1. We treat locals and exports of the same name as mutually exclusive within a container.
        //      That means the binder will issue a Duplicate Identifier error if you mix locals and exports
        //      with the same name in the same container.
        //      TODO: Make this a more specific error and decouple it from the exclusion logic.
        //   2. When we checkIdentifier in the checker, we set its resolved symbol to the local symbol,
        //      but return the export symbol (by calling getExportSymbolOfValueSymbolIfExported). That way
        //      when the emitter comes back to it, it knows not to qualify the name if it was found in a containing scope.
        //
        // NOTE: Nested ambient modules always should go to to 'locals' table to prevent their automatic merge
        //       during global merging in the checker. Why? The only case when ambient module is permitted inside another module is module augmentation
        //       and this case is specially handled. Module augmentations should only be merged with original module definition
        //       and should never be merged directly with other augmentation, and the latter case would be possible if automatic merge is allowed.
        if !ast::is_ambient_module(node)
            && (has_export_modifier || container.flags().intersects(NodeFlags::ExportContext))
        {
            if !ast::is_locals_container(container)
                || (ast::has_syntactic_modifier(node, ModifierFlags::Default)
                    && Self::get_declaration_name(node) == ast::INTERNAL_SYMBOL_NAME_MISSING)
            {
                return self.declare_symbol(
                    ast::get_exports(container.symbol().unwrap()),
                    container.symbol(),
                    node,
                    symbol_flags,
                    symbol_excludes,
                );
                // No local symbol for an unnamed default!
            }
            let export_kind = if symbol_flags.intersects(SymbolFlags::Value) {
                SymbolFlags::ExportValue
            } else {
                SymbolFlags::None
            };
            let local = self.declare_symbol(
                ast::get_locals(container),
                None, /*parent*/
                node,
                export_kind,
                symbol_excludes,
            );
            let export_symbol = self.declare_symbol(
                ast::get_exports(container.symbol().unwrap()),
                container.symbol(),
                node,
                symbol_flags,
                symbol_excludes,
            );
            local.set_export_symbol(Some(export_symbol));
            node.exportable_data().unwrap().local_symbol.set(Some(local));
            return local;
        }
        self.declare_symbol(
            ast::get_locals(container),
            None, /*parent*/
            node,
            symbol_flags,
            symbol_excludes,
        )
    }

    fn declare_class_member(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) -> P<Symbol> {
        let container_symbol = self.container().symbol();
        if ast::is_static(node) {
            return self.declare_symbol(
                ast::get_exports(container_symbol.unwrap()),
                container_symbol,
                node,
                symbol_flags,
                symbol_excludes,
            );
        }
        self.declare_symbol(
            ast::get_members(container_symbol.unwrap()),
            container_symbol,
            node,
            symbol_flags,
            symbol_excludes,
        )
    }

    fn declare_source_file_member(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) -> P<Symbol> {
        if ast::is_external_module(self.file) {
            return self.declare_module_member(node, symbol_flags, symbol_excludes);
        }
        self.declare_symbol(
            ast::get_locals(self.file.as_node()),
            None, /*parent*/
            node,
            symbol_flags,
            symbol_excludes,
        )
    }

    fn declare_symbol_and_add_to_symbol_table(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) -> P<Symbol> {
        let container = self.container();
        match container.kind() {
            Kind::ModuleDeclaration => {
                self.declare_module_member(node, symbol_flags, symbol_excludes)
            }
            Kind::SourceFile => {
                self.declare_source_file_member(node, symbol_flags, symbol_excludes)
            }
            Kind::ClassExpression | Kind::ClassDeclaration => {
                self.declare_class_member(node, symbol_flags, symbol_excludes)
            }
            Kind::EnumDeclaration => self.declare_symbol(
                ast::get_exports(container.symbol().unwrap()),
                container.symbol(),
                node,
                symbol_flags,
                symbol_excludes,
            ),
            Kind::TypeLiteral
            | Kind::ObjectLiteralExpression
            | Kind::InterfaceDeclaration
            | Kind::JsxAttributes => self.declare_symbol(
                ast::get_members(container.symbol().unwrap()),
                container.symbol(),
                node,
                symbol_flags,
                symbol_excludes,
            ),
            Kind::FunctionType
            | Kind::ConstructorType
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::IndexSignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::ClassStaticBlockDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::MappedType => self.declare_symbol(
                ast::get_locals(container),
                None, /*parent*/
                node,
                symbol_flags,
                symbol_excludes,
            ),
            _ => panic!("Unhandled case in declareSymbolAndAddToSymbolTable"),
        }
    }

    fn new_flow_node(&self, flags: FlowFlags) -> P<FlowNode> {
        new_flow_node_value(flags, None, None, self.file.text_index.get())
    }

    fn new_flow_node_ex(
        &self,
        flags: FlowFlags,
        node: Option<P<Node>>,
        antecedent: Option<P<FlowNode>>,
    ) -> P<FlowNode> {
        new_flow_node_value(flags, node, antecedent, self.file.text_index.get())
    }

    fn create_loop_label(&self) -> P<FlowNode> {
        self.new_flow_node(FlowFlags::LoopLabel)
    }

    fn create_branch_label(&self) -> P<FlowNode> {
        self.new_flow_node(FlowFlags::BranchLabel)
    }

    fn create_reduce_label(
        &self,
        target: P<FlowNode>,
        antecedents: Option<P<FlowList>>,
        antecedent: P<FlowNode>,
    ) -> P<FlowNode> {
        self.new_flow_node_ex(
            FlowFlags::ReduceLabel,
            Some(ast::new_flow_reduce_label_data(target, antecedents)),
            Some(antecedent),
        )
    }

    fn create_flow_condition(
        &self,
        flags: FlowFlags,
        antecedent: P<FlowNode>,
        expression: Option<P<Node>>,
    ) -> P<FlowNode> {
        if antecedent.flags.get().intersects(FlowFlags::Unreachable) {
            return antecedent;
        }
        let Some(expression) = expression else {
            if flags.intersects(FlowFlags::TrueCondition) {
                return antecedent;
            }
            return self.unreachable_flow;
        };
        if (expression.kind() == Kind::TrueKeyword && flags.intersects(FlowFlags::FalseCondition)
            || expression.kind() == Kind::FalseKeyword
                && flags.intersects(FlowFlags::TrueCondition))
            && !ast::is_expression_of_optional_chain_root(expression)
            && !ast::is_nullish_coalesce(expression.parent().unwrap())
        {
            return self.unreachable_flow;
        }
        if !is_narrowing_expression(expression) {
            return antecedent;
        }
        set_flow_node_referenced(antecedent);
        self.new_flow_node_ex(flags, Some(expression), Some(antecedent))
    }

    fn create_flow_mutation(
        &mut self,
        flags: FlowFlags,
        antecedent: P<FlowNode>,
        node: P<Node>,
    ) -> P<FlowNode> {
        set_flow_node_referenced(antecedent);
        self.has_flow_effects = true;
        let result = self.new_flow_node_ex(flags, Some(node), Some(antecedent));
        if self.current_exception_target.is_some() {
            Self::add_antecedent(self.current_exception_target, result);
        }
        result
    }

    fn create_flow_switch_clause(
        &self,
        antecedent: P<FlowNode>,
        switch_statement: P<Node>,
        clause_start: usize,
        clause_end: usize,
    ) -> P<FlowNode> {
        set_flow_node_referenced(antecedent);
        self.new_flow_node_ex(
            FlowFlags::SwitchClause,
            Some(ast::new_flow_switch_clause_data(switch_statement, clause_start, clause_end)),
            Some(antecedent),
        )
    }

    fn create_flow_call(&mut self, antecedent: P<FlowNode>, node: P<Node>) -> P<FlowNode> {
        set_flow_node_referenced(antecedent);
        self.has_flow_effects = true;
        self.new_flow_node_ex(FlowFlags::Call, Some(node), Some(antecedent))
    }

    fn new_flow_list(head: P<FlowNode>, tail: Option<P<FlowList>>) -> P<FlowList> {
        P::new_recycled(FlowList { flow: head, next: OwnedCell::new(tail) })
    }

    fn combine_flow_lists(
        head: Option<P<FlowList>>,
        tail: Option<P<FlowList>>,
    ) -> Option<P<FlowList>> {
        let Some(head) = head else {
            return tail;
        };
        let rest = Self::combine_flow_lists(head.next.get(), tail);
        Some(Self::new_flow_list(head.flow, rest))
    }
}

fn set_flow_node_referenced(flow: P<FlowNode>) {
    // On first reference we set the Referenced flag, thereafter we set the Shared flag
    if flow.flags.get().intersects(FlowFlags::Referenced) {
        flow.flags.set(flow.flags.get() | FlowFlags::Shared);
    } else {
        flow.flags.set(flow.flags.get() | FlowFlags::Referenced);
    }
}

impl Binder {
    fn add_antecedent(label: Option<P<FlowNode>>, antecedent: P<FlowNode>) {
        if antecedent.flags.get().intersects(FlowFlags::Unreachable) {
            return;
        }
        let label = label.unwrap();
        // If antecedent isn't already on the Antecedents list, add it to the end of the list
        let mut last: Option<P<FlowList>> = None;
        let mut list = label.antecedents();
        while let Some(l) = list {
            if l.flow == antecedent {
                return;
            }
            last = Some(l);
            list = l.next.get();
        }
        let new_list = Self::new_flow_list(antecedent, None);
        match last {
            None => label.set_antecedents(Some(new_list)),
            Some(last) => last.next.set(Some(new_list)),
        }
        set_flow_node_referenced(antecedent);
    }

    fn finish_flow_label(&self, label: P<FlowNode>) -> P<FlowNode> {
        let Some(antecedents) = label.antecedents() else {
            return self.unreachable_flow;
        };
        if antecedents.next.get().is_none() {
            return antecedents.flow;
        }
        label
    }

    /// `finishFlowLabel` for a branch label that only its creator and the binder's target fields have seen (if
    /// statements, loops, conditional and logical expressions): a dropped label is recycled (notes/mem-recycle.md).
    fn finish_local_flow_label(&mut self, label: P<FlowNode>) -> P<FlowNode> {
        let result = self.finish_flow_label(label);
        if result != label {
            self.recycle_flow_label(label);
        }
        result
    }

    /// Gives a branch label that nothing references any more (and its antecedent list cells, not the antecedents)
    /// back to the arena. Kept when it was ever used as an antecedent or a binder field still holds it.
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "Recycling frees binder-owned flow nodes and requires exclusive access to the binder."
    )]
    fn recycle_flow_label(&mut self, label: P<FlowNode>) {
        debug_assert!(label.flags.get().intersects(FlowFlags::BranchLabel));
        let held = |t: Option<P<FlowNode>>| t == Some(label);
        if label.flags.get().intersects(FlowFlags::Referenced)
            || label == self.unreachable_flow
            || held(self.current_flow)
            || held(self.current_break_target)
            || held(self.current_continue_target)
            || held(self.current_return_target)
            || held(self.current_true_target)
            || held(self.current_false_target)
            || held(self.current_exception_target)
            || held(self.pre_switch_case_flow)
            || self
                .active_label_list
                .iter()
                .any(|l| held(l.break_target) || held(l.continue_target))
        {
            return;
        }
        let mut list = label.antecedents();
        while let Some(l) = list {
            list = l.next.get();
            // SAFETY: a label's antecedent list cells are referenced only by the label (combineFlowLists copies
            // them; only try/finally labels, which are never recycled, hand their lists to reduce labels).
            unsafe { tsrs_core::free!(l) };
        }
        // SAFETY: never an antecedent (no Referenced flag), not held by the binder, and its creator dropped it.
        unsafe { tsrs_core::free!(label) };
    }

    fn bind(&mut self, node: impl Into<Option<P<Node>>>) -> bool {
        let Some(node) = node.into() else {
            return false;
        };
        // Even though in the AST the jsdoc @typedef node belongs to the current node,
        // its symbol might be in the same scope with the current node's symbol. Consider:
        //
        //     /** @typedef {string | number} MyType */
        //     function foo();
        //
        // Here the current node is "foo", which is a container, but the scope of "MyType" should
        // not be inside "foo". Therefore we always bind @typedef before bind the parent node,
        // and skip binding this tag later when binding all the other jsdoc tags.

        // First we bind declaration nodes to a symbol if possible. We'll both create a symbol
        // and then potentially add the symbol to an appropriate symbol table. Possible
        // destination symbol tables are:
        //
        //  1) The 'exports' table of the current container's symbol.
        //  2) The 'members' table of the current container's symbol.
        //  3) The 'locals' table of the current container.
        //
        // However, not all symbols will end up in any of these tables. 'Anonymous' symbols
        // (like TypeLiterals for example) will not be put in any table.
        match node.kind() {
            Kind::Identifier => {
                node.set_flow_node(self.current_flow);
                self.check_contextual_identifier(node);
            }
            Kind::ThisKeyword | Kind::SuperKeyword => {
                if node.kind() == Kind::ThisKeyword {
                    self.seen_this_keyword = true;
                }
                node.set_flow_node(self.current_flow);
            }
            Kind::QualifiedName => {
                if self.current_flow.is_some() && ast::is_part_of_type_query(node) {
                    node.set_flow_node(self.current_flow);
                }
            }
            Kind::MetaProperty => {
                node.set_flow_node(self.current_flow);
            }
            Kind::PrivateIdentifier => {
                self.check_private_identifier(node);
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                if self.current_flow.is_some() && is_narrowable_reference(node) {
                    set_flow_node(node, self.current_flow);
                }
            }
            Kind::BinaryExpression => {
                match ast::get_assignment_declaration_kind(node) {
                    ast::JSDeclarationKind::ModuleExports => {
                        self.bind_module_exports_assignment(node);
                    }
                    ast::JSDeclarationKind::ExportsProperty => {
                        self.bind_exports_or_object_define_property(node);
                    }
                    ast::JSDeclarationKind::Property => self.bind_expando_property_assignment(node),
                    ast::JSDeclarationKind::ThisProperty => {
                        self.bind_this_property_assignment(node);
                    }
                    _ => {}
                }
                self.check_strict_mode_binary_expression(node);
            }
            Kind::CatchClause => self.check_strict_mode_catch_clause(node),
            Kind::DeleteExpression => self.check_strict_mode_delete_expression(node),
            Kind::PostfixUnaryExpression => self.check_strict_mode_postfix_unary_expression(node),
            Kind::PrefixUnaryExpression => self.check_strict_mode_prefix_unary_expression(node),
            Kind::WithStatement => self.check_strict_mode_with_statement(node),
            Kind::LabeledStatement => self.check_strict_mode_labeled_statement(node),
            Kind::ThisType => {
                self.seen_this_keyword = true;
            }
            Kind::TypeParameter => self.bind_type_parameter(node),
            Kind::Parameter => self.bind_parameter(node),
            Kind::VariableDeclaration => self.bind_variable_declaration_or_binding_element(node),
            Kind::BindingElement => {
                node.set_flow_node(self.current_flow);
                self.bind_variable_declaration_or_binding_element(node);
            }
            Kind::PropertyDeclaration | Kind::PropertySignature => self.bind_property_worker(node),
            Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment => {
                self.bind_property_or_method_or_accessor(
                    node,
                    SymbolFlags::Property,
                    SymbolFlags::PropertyExcludes,
                );
            }
            Kind::EnumMember => self.bind_property_or_method_or_accessor(
                node,
                SymbolFlags::EnumMember,
                SymbolFlags::EnumMemberExcludes,
            ),
            Kind::CallSignature | Kind::ConstructSignature | Kind::IndexSignature => {
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::Signature,
                    SymbolFlags::None,
                );
            }
            Kind::MethodDeclaration | Kind::MethodSignature => self
                .bind_property_or_method_or_accessor(
                    node,
                    SymbolFlags::Method | get_optional_symbol_flag_for_node(node),
                    if ast::is_object_literal_method(node) {
                        SymbolFlags::Value
                    } else {
                        SymbolFlags::MethodExcludes
                    },
                ),
            Kind::FunctionDeclaration => self.bind_function_declaration(node),
            Kind::Constructor => {
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::Constructor,
                    SymbolFlags::None,
                );
            }
            Kind::GetAccessor => self.bind_property_or_method_or_accessor(
                node,
                SymbolFlags::GetAccessor,
                SymbolFlags::GetAccessorExcludes,
            ),
            Kind::SetAccessor => self.bind_property_or_method_or_accessor(
                node,
                SymbolFlags::SetAccessor,
                SymbolFlags::SetAccessorExcludes,
            ),
            Kind::FunctionType | Kind::ConstructorType => {
                self.bind_function_or_constructor_type(node);
            }
            Kind::TypeLiteral | Kind::MappedType => self.bind_anonymous_declaration(
                node,
                SymbolFlags::TypeLiteral,
                ast::INTERNAL_SYMBOL_NAME_TYPE,
            ),
            Kind::ObjectLiteralExpression => self.bind_anonymous_declaration(
                node,
                SymbolFlags::ObjectLiteral,
                ast::INTERNAL_SYMBOL_NAME_OBJECT,
            ),
            Kind::FunctionExpression | Kind::ArrowFunction => self.bind_function_expression(node),
            Kind::ClassExpression | Kind::ClassDeclaration => {
                self.bind_class_like_declaration(node);
            }
            Kind::InterfaceDeclaration => self.bind_block_scoped_declaration(
                node,
                SymbolFlags::Interface,
                SymbolFlags::InterfaceExcludes,
            ),
            Kind::CallExpression => {
                match ast::get_assignment_declaration_kind(node) {
                    ast::JSDeclarationKind::ObjectDefinePropertyValue => {
                        self.bind_expando_property_assignment(node);
                    }
                    ast::JSDeclarationKind::ObjectDefinePropertyExports => {
                        self.bind_exports_or_object_define_property(node);
                    }
                    _ => {}
                }
                if ast::is_in_js_file(Some(node)) {
                    self.bind_call_expression(node);
                }
            }
            Kind::TypeAliasDeclaration => self.bind_block_scoped_declaration(
                node,
                SymbolFlags::TypeAlias,
                SymbolFlags::TypeAliasExcludes,
            ),
            Kind::JSTypeAliasDeclaration => {
                // Top-level JSTypeAliasDeclaration nodes are processed in bindContainer
                if !ast::is_source_file(self.block_scope_container.unwrap()) {
                    self.bind_block_scoped_declaration(
                        node,
                        SymbolFlags::TypeAlias,
                        SymbolFlags::TypeAliasExcludes,
                    );
                }
            }
            Kind::EnumDeclaration => self.bind_enum_declaration(node),
            Kind::ModuleDeclaration => self.bind_module_declaration(node),
            Kind::ImportEqualsDeclaration
            | Kind::NamespaceImport
            | Kind::ImportSpecifier
            | Kind::ExportSpecifier => {
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::Alias,
                    SymbolFlags::AliasExcludes,
                );
            }
            Kind::NamespaceExportDeclaration => self.bind_namespace_export_declaration(node),
            Kind::ImportClause => self.bind_import_clause(node),
            Kind::ExportDeclaration => self.bind_export_declaration(node),
            Kind::ExportAssignment => self.bind_export_assignment(node),
            Kind::SourceFile => self.bind_source_file_if_external_module(),
            Kind::JsxAttributes => self.bind_jsx_attributes(node),
            Kind::JsxAttribute => {
                self.bind_jsx_attribute(node, SymbolFlags::Property, SymbolFlags::PropertyExcludes);
            }
            _ => {}
        }
        // Then we recurse into the children of the node to bind them as well. For certain
        // symbols we do specialized work when we recurse. For example, we'll keep track of
        // the current 'container' node when it changes. This helps us know which symbol table
        // a local should go into for example. Since terminal nodes are known not to have
        // children, as an optimization we don't process those.
        let mut this_node_or_any_subnodes_has_error =
            node.flags().intersects(NodeFlags::ThisNodeHasError);
        if node.kind() > Kind::LAST_TOKEN {
            let save_seen_parse_error = self.seen_parse_error;
            self.seen_parse_error = false;
            let container_flags = get_container_flags(node);
            if container_flags == ContainerFlags::None {
                self.bind_children(node);
            } else {
                self.bind_container(node, container_flags);
            }
            if self.seen_parse_error {
                this_node_or_any_subnodes_has_error = true;
            }
            self.seen_parse_error = save_seen_parse_error;
        }
        if this_node_or_any_subnodes_has_error {
            node.set_flags(node.flags() | NodeFlags::ThisNodeOrAnySubNodesHasError);
            self.seen_parse_error = true;
        }
        false
    }

    fn bind_property_worker(&mut self, node: P<Node>) {
        let is_auto_accessor = ast::is_auto_accessor_property_declaration(node);
        let includes = if is_auto_accessor { SymbolFlags::Accessor } else { SymbolFlags::Property };
        let excludes = if is_auto_accessor {
            SymbolFlags::AccessorExcludes
        } else {
            SymbolFlags::PropertyExcludes
        };
        self.bind_property_or_method_or_accessor(
            node,
            includes | get_optional_symbol_flag_for_node(node),
            excludes,
        );
    }

    fn bind_source_file_if_external_module(&mut self) {
        Self::set_export_context_flag(self.file.as_node());
        if ast::is_external_or_common_js_module(self.file) {
            self.bind_source_file_as_external_module();
        } else if ast::is_json_source_file(self.file) {
            self.bind_source_file_as_external_module();
            // Create symbol equivalent for the module.exports = {}
            let file_node = self.file.as_node();
            let original_symbol = file_node.symbol();
            self.declare_symbol(
                ast::get_exports(original_symbol.unwrap()),
                original_symbol,
                file_node,
                SymbolFlags::Property,
                SymbolFlags::All,
            );
            file_node.declaration_data().unwrap().symbol.set(original_symbol);
        }
    }

    fn bind_source_file_as_external_module(&mut self) {
        let name =
            alloc_str(&format!("\"{}\"", tspath::remove_file_extension(self.file.file_name())));
        self.bind_anonymous_declaration(self.file.as_node(), SymbolFlags::ValueModule, name);
    }

    fn bind_module_declaration(&mut self, node: P<Node>) {
        Self::set_export_context_flag(node);
        if ast::is_ambient_module(node) {
            if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
                self.error_on_first_token(
                    node,
                    &diagnostics::X_EXPORT_MODIFIER_CANNOT_BE_APPLIED_TO_AMBIENT_MODULES_AND_MODULE_AUGMENTATIONS_SINCE_THEY_ARE_ALWAYS_VISIBLE,
                    &[],
                );
            }
            if ast::is_module_augmentation_external(node) {
                self.declare_module_symbol(node);
            } else {
                let name = node.name().unwrap();
                let symbol = self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::ValueModule,
                    SymbolFlags::ValueModuleExcludes,
                );

                if ast::is_string_literal(name) {
                    let attributes = node.as_module_declaration().attributes();
                    let pattern = tsrs_core::try_parse_pattern(name.text());
                    if !pattern.is_valid() {
                        // An invalid pattern - must have multiple wildcards.
                        self.error_on_first_token(
                            name,
                            &diagnostics::PATTERN_0_CAN_HAVE_AT_MOST_ONE_ASTERISK_CHARACTER,
                            &[&name.text()],
                        );
                    } else if matches!(pattern.kind, PatternKind::Wildcard(_)) {
                        let mut pattern_ambient_modules =
                            self.file.pattern_ambient_modules.get().to_vec();
                        pattern_ambient_modules
                            .push(P::new(ast::PatternAmbientModule { pattern, symbol }));
                        self.file
                            .pattern_ambient_modules
                            .set(tsrs_core::alloc_slice(&pattern_ambient_modules));
                    } else if attributes.is_some() {
                        self.error_on_node(
                            name,
                            &diagnostics::AN_AMBIENT_MODULE_DECLARATION_WITH_IMPORT_ATTRIBUTES_MUST_USE_A_PATTERN_NAME_WITH_AN_ASTERISK_CHARACTER,
                            &[],
                        );
                    }
                }
            }
        } else {
            let state = self.declare_module_symbol(node);
            if state != ast::ModuleInstanceState::NonInstantiated {
                let symbol = node.symbol().unwrap();
                // if module was already merged with some function, class or non-const enum, treat it as non-const-enum-only
                let const_enum_only_module = !symbol.flags.get().intersects(SymbolFlags::Function | SymbolFlags::Class | SymbolFlags::RegularEnum)
                    // Current must be `const enum` only
                    && state == ast::ModuleInstanceState::ConstEnumOnly
                    // Can't have been set to 'false' in a previous merged symbol. ('undefined' OK)
                    && !self.not_const_enum_only_modules.contains(&symbol);
                if const_enum_only_module {
                    symbol.flags.set(symbol.flags.get() | SymbolFlags::ConstEnumOnlyModule);
                } else {
                    symbol.flags.set(symbol.flags.get() & !SymbolFlags::ConstEnumOnlyModule);
                    self.not_const_enum_only_modules.insert(symbol);
                }
            }
        }
    }

    fn declare_module_symbol(&mut self, node: P<Node>) -> ast::ModuleInstanceState {
        let state = ast::get_module_instance_state(node);
        let instantiated = state != ast::ModuleInstanceState::NonInstantiated;
        self.declare_symbol_and_add_to_symbol_table(
            node,
            if instantiated { SymbolFlags::ValueModule } else { SymbolFlags::NamespaceModule },
            if instantiated {
                SymbolFlags::ValueModuleExcludes
            } else {
                SymbolFlags::NamespaceModuleExcludes
            },
        );
        state
    }

    fn bind_namespace_export_declaration(&mut self, node: P<Node>) {
        if node.modifiers().is_some() {
            self.error_on_node(node, &diagnostics::MODIFIERS_CANNOT_APPEAR_HERE, &[]);
        }
        let parent = node.parent().unwrap();
        if !ast::is_source_file(parent) {
            self.error_on_node(
                node,
                &diagnostics::GLOBAL_MODULE_EXPORTS_MAY_ONLY_APPEAR_AT_TOP_LEVEL,
                &[],
            );
        } else if !ast::is_external_module(parent.as_source_file().as_p()) {
            self.error_on_node(
                node,
                &diagnostics::GLOBAL_MODULE_EXPORTS_MAY_ONLY_APPEAR_IN_MODULE_FILES,
                &[],
            );
        } else if !parent.as_source_file().is_declaration_file() {
            self.error_on_node(
                node,
                &diagnostics::GLOBAL_MODULE_EXPORTS_MAY_ONLY_APPEAR_IN_DECLARATION_FILES,
                &[],
            );
        } else {
            let table = ast::get_symbol_table(&self.file.global_exports);
            self.declare_symbol(
                table,
                self.file.as_node().symbol(),
                node,
                SymbolFlags::Alias,
                SymbolFlags::AliasExcludes,
            );
        }
    }

    fn bind_import_clause(&mut self, node: P<Node>) {
        if node.name().is_some() {
            self.declare_symbol_and_add_to_symbol_table(
                node,
                SymbolFlags::Alias,
                SymbolFlags::AliasExcludes,
            );
        }
    }

    fn bind_export_declaration(&mut self, node: P<Node>) {
        let decl = node.as_export_declaration();
        let container_symbol = self.container().symbol();
        match container_symbol {
            None => {
                // Export * in some sort of block construct
                let name = Self::get_declaration_name(node);
                self.bind_anonymous_declaration(node, SymbolFlags::ExportStar, name);
            }
            Some(container_symbol) => {
                if let Some(export_clause) = decl.export_clause() {
                    if ast::is_namespace_export(export_clause) {
                        self.declare_symbol(
                            ast::get_exports(container_symbol),
                            Some(container_symbol),
                            export_clause,
                            SymbolFlags::Alias,
                            SymbolFlags::AliasExcludes,
                        );
                    }
                } else {
                    // All export * declarations are collected in an __export symbol
                    self.declare_symbol(
                        ast::get_exports(container_symbol),
                        Some(container_symbol),
                        node,
                        SymbolFlags::ExportStar,
                        SymbolFlags::None,
                    );
                }
            }
        }
    }

    fn bind_export_assignment(&mut self, node: P<Node>) {
        let container = self.container();
        if container.symbol().is_none() && ast::is_export_assignment(node) {
            // Incorrect export assignment in some sort of block construct
            let name = Self::get_declaration_name(node);
            self.bind_anonymous_declaration(node, SymbolFlags::Value, name);
        } else {
            // If there is an `export default x;` alias declaration, can't `export default` anything else.
            // (In contrast, you can still have `export default function f() {}` and `export default interface I {}`.)
            let flags = if ast::expression_is_alias(node.expression().unwrap()) {
                SymbolFlags::Alias
            } else {
                SymbolFlags::Property
            };
            let symbol = self.declare_symbol(
                ast::get_exports(container.symbol().unwrap()),
                container.symbol(),
                node,
                flags,
                SymbolFlags::All,
            );
            if node.as_export_assignment().is_export_equals() {
                // Ensure export assignments have a ValueDeclaration set.
                set_value_declaration(symbol, node);
            }
        }
    }

    fn bind_jsx_attributes(&mut self, node: P<Node>) {
        self.bind_anonymous_declaration(
            node,
            SymbolFlags::ObjectLiteral,
            ast::INTERNAL_SYMBOL_NAME_JSX_ATTRIBUTES,
        );
    }

    fn bind_jsx_attribute(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) {
        self.declare_symbol_and_add_to_symbol_table(node, symbol_flags, symbol_excludes);
    }

    fn set_export_context_flag(node: P<Node>) {
        // A declaration source file or ambient module declaration that contains no export declarations (but possibly regular
        // declarations with export modifiers) is an export context in which declarations are implicitly exported.
        if node.flags().intersects(NodeFlags::Ambient) && !Self::has_export_declarations(node) {
            node.set_flags(node.flags() | NodeFlags::ExportContext);
        } else {
            node.set_flags(node.flags() & !NodeFlags::ExportContext);
        }
    }

    fn has_export_declarations(node: P<Node>) -> bool {
        let mut statements: &[P<Node>] = &[];
        match node.kind() {
            Kind::SourceFile => {
                statements = node.statements();
            }
            Kind::ModuleDeclaration => {
                if let Some(body) = node.body()
                    && ast::is_module_block(body)
                {
                    statements = body.statements();
                }
            }
            _ => {}
        }
        statements.iter().any(|&s| ast::is_export_declaration(s) || ast::is_export_assignment(s))
    }

    fn bind_function_expression(&mut self, node: P<Node>) {
        if !self.file.is_declaration_file()
            && !node.flags().intersects(NodeFlags::Ambient)
            && ast::is_async_function(node)
        {
            self.emit_flags |= NodeFlags::HasAsyncFunctions;
        }
        set_flow_node(node, self.current_flow);
        let mut binding_name = ast::INTERNAL_SYMBOL_NAME_FUNCTION;
        if ast::is_function_expression(node)
            && let Some(name) = node.name()
        {
            self.check_strict_mode_function_name(node);
            binding_name = name.text();
        }
        self.bind_anonymous_declaration(node, SymbolFlags::Function, binding_name);
    }

    fn bind_call_expression(&mut self, node: P<Node>) {
        // We're only inspecting call expressions to detect CommonJS modules, so we can skip
        // this check if we've already seen the module indicator
        if self.file.common_js_module_indicator.get().is_none()
            && ast::is_require_call(node, false /*requireStringLiteralLikeArgument*/)
        {
            self.set_common_js_module_indicator(node);
        }
    }

    fn set_common_js_module_indicator(&mut self, node: P<Node>) -> bool {
        if let Some(external_module_indicator) = self.file.external_module_indicator.get()
            && external_module_indicator != self.file.as_node()
        {
            return false;
        }
        if self.file.common_js_module_indicator.get().is_none() {
            self.file.common_js_module_indicator.set(Some(node));
            if self.file.external_module_indicator.get().is_none() {
                self.bind_source_file_as_external_module();
            }
        }
        true
    }

    fn bind_class_like_declaration(&mut self, node: P<Node>) {
        let name = node.name();
        match node.kind() {
            Kind::ClassDeclaration => {
                self.bind_block_scoped_declaration(
                    node,
                    SymbolFlags::Class,
                    SymbolFlags::ClassExcludes,
                );
            }
            Kind::ClassExpression => {
                let name_text = match name {
                    Some(name) => name.text(),
                    None => ast::INTERNAL_SYMBOL_NAME_CLASS,
                };
                self.bind_anonymous_declaration(node, SymbolFlags::Class, name_text);
            }
            _ => {}
        }
        let symbol = node.symbol().unwrap();
        // TypeScript 1.0 spec (April 2014): 8.4
        // Every class automatically contains a static property member named 'prototype', the
        // type of which is an instantiation of the class type with type Any supplied as a type
        // argument for each type parameter. It is an error to explicitly declare a static
        // property member with the name 'prototype'.
        //
        // Note: we check for this here because this class may be merging into a module.  The
        // module might have an exported variable called 'prototype'.  We can't allow that as
        // that would clash with the built-in 'prototype' for the class.
        let prototype_symbol =
            self.new_symbol(SymbolFlags::Property | SymbolFlags::Prototype, "prototype");
        let symbol_export = (*ast::get_exports(symbol)).get(prototype_symbol.name());
        if let Some(symbol_export) = symbol_export {
            let first_declaration = symbol_export.declarations()[0];
            self.error_on_node(
                first_declaration,
                &diagnostics::DUPLICATE_IDENTIFIER_0,
                &[&ast::symbol_name(prototype_symbol)],
            );
        }
        ast::get_exports(symbol).set(prototype_symbol.name(), prototype_symbol);
        prototype_symbol.set_parent(Some(symbol));
    }

    fn bind_property_or_method_or_accessor(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) {
        if !self.file.is_declaration_file()
            && !node.flags().intersects(NodeFlags::Ambient)
            && ast::is_async_function(node)
        {
            self.emit_flags |= NodeFlags::HasAsyncFunctions;
        }
        if self.current_flow.is_some()
            && ast::is_object_literal_or_class_expression_method_or_accessor(node)
        {
            set_flow_node(node, self.current_flow);
        }
        if ast::has_dynamic_name(node) {
            self.bind_anonymous_declaration(node, symbol_flags, ast::INTERNAL_SYMBOL_NAME_COMPUTED);
        } else {
            self.declare_symbol_and_add_to_symbol_table(node, symbol_flags, symbol_excludes);
        }
    }

    fn bind_function_or_constructor_type(&mut self, node: P<Node>) {
        // For a given function symbol "<...>(...) => T" we want to generate a symbol identical
        // to the one we would get for: { <...>(...): T }
        //
        // We do that by making an anonymous type literal symbol, and then setting the function
        // symbol as its sole member. To the rest of the system, this symbol will be indistinguishable
        // from an actual type literal symbol you would have gotten had you used the long form.
        let name = Self::get_declaration_name(node);
        let symbol = self.new_symbol(SymbolFlags::Signature, name);
        self.add_declaration_to_symbol(symbol, node, SymbolFlags::Signature);
        let type_literal_symbol =
            self.new_symbol(SymbolFlags::TypeLiteral, ast::INTERNAL_SYMBOL_NAME_TYPE);
        self.add_declaration_to_symbol(type_literal_symbol, node, SymbolFlags::TypeLiteral);
        let members = SymbolTable::new();
        members.set(symbol.name(), symbol);
        type_literal_symbol.set_members(Some(members));
    }

    fn add_late_bound_assignment_declaration_to_symbol(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
    ) {
        let exports = ast::get_exports(symbol);
        let assignment_symbol = if let Some(s) =
            (*exports).get(ast::INTERNAL_SYMBOL_NAME_ASSIGNMENT_DECLARATION)
        {
            s
        } else {
            let s = self
                .new_symbol(SymbolFlags::None, ast::INTERNAL_SYMBOL_NAME_ASSIGNMENT_DECLARATION);
            exports.set(ast::INTERNAL_SYMBOL_NAME_ASSIGNMENT_DECLARATION, s);
            s
        };
        assignment_symbol.append_declarations(&[node]);
    }

    fn bind_module_exports_assignment(&mut self, node: P<Node>) {
        if self.set_common_js_module_indicator(node) {
            let container = self.file.as_node();
            let flags = if ast::expression_is_alias(node.as_binary_expression().right()) {
                SymbolFlags::Alias
            } else {
                SymbolFlags::Property
            };
            let symbol = self.declare_symbol(
                ast::get_exports(container.symbol().unwrap()),
                container.symbol(),
                node,
                flags,
                SymbolFlags::None,
            );
            set_value_declaration(symbol, node);
        }
    }

    fn bind_expando_property_assignment(&mut self, node: P<Node>) {
        self.expando_assignments.push(ExpandoAssignmentInfo {
            node,
            container: self.container,
            block_scope_container: self.block_scope_container,
        });
    }

    fn bind_deferred_expando_assignments(&mut self) {
        let assignments = std::mem::take(&mut self.expando_assignments);
        for info in &assignments {
            self.container = info.container;
            self.block_scope_container = info.block_scope_container;
            self.bind_deferred_expando_assignment(info.node);
        }
        self.expando_assignments = assignments;
    }

    // If the given module symbol has an export= symbol, promote exports with a type or namespace meaning
    // from the module symbol onto the export= symbol and, if any such exports exist, mark the export=
    // symbol as a namespace module.
    fn bind_common_js_type_exports(module_symbol: P<Symbol>) {
        let Some(module_exports) = module_symbol.exports() else {
            return;
        };
        if let Some(export_equals) = (*module_exports).get(ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS)
        {
            for symbol in module_exports.values() {
                if symbol.name() != ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                    && symbol.flags.get().intersects(SymbolFlags::Type | SymbolFlags::Namespace)
                {
                    ast::get_exports(export_equals).set(symbol.name(), symbol);
                    export_equals
                        .flags
                        .set(export_equals.flags.get() | SymbolFlags::NamespaceModule);
                }
            }
        }
    }

    fn bind_deferred_expando_assignment(&mut self, node: P<Node>) {
        let parent = get_parent_of_property_assignment(node);
        let mut symbol = self.lookup_entity(parent, self.block_scope_container.unwrap());
        if symbol.is_none() {
            symbol = self.lookup_entity(parent, self.container());
        }
        if let Some(symbol) = get_initializer_symbol(symbol) {
            if ast::has_dynamic_name(node) {
                self.bind_anonymous_declaration(
                    node,
                    SymbolFlags::Property | SymbolFlags::Assignment,
                    ast::INTERNAL_SYMBOL_NAME_COMPUTED,
                );
                self.add_late_bound_assignment_declaration_to_symbol(node, symbol);
            } else {
                // We declare expandos only when there are no non-expando declarations for that name.
                let exports = ast::get_exports(symbol);
                let existing = (*exports).get(Self::get_declaration_name(node));
                if existing.is_none()
                    || existing.unwrap().flags.get().intersects(SymbolFlags::Assignment)
                {
                    self.declare_symbol(
                        exports,
                        Some(symbol),
                        node,
                        SymbolFlags::Property | SymbolFlags::Assignment,
                        SymbolFlags::PropertyExcludes,
                    );
                }
            }
        }
    }
}

fn get_parent_of_property_assignment(node: P<Node>) -> P<Node> {
    match node.kind() {
        Kind::BinaryExpression => node.as_binary_expression().left().expression().unwrap(),
        Kind::CallExpression => node.arguments()[0],
        _ => panic!("Unhandled case in getParentOfPropertyAssignment"),
    }
}

impl Binder {
    fn bind_exports_or_object_define_property(&mut self, node: P<Node>) {
        if self.set_common_js_module_indicator(node) {
            let container = self.file.as_node();
            let flags = if ast::is_binary_expression(node)
                && ast::expression_is_alias(node.as_binary_expression().right())
            {
                SymbolFlags::Alias
            } else {
                SymbolFlags::FunctionScopedVariable
            };
            self.declare_symbol(
                ast::get_exports(container.symbol().unwrap()),
                container.symbol(),
                node,
                flags,
                SymbolFlags::FunctionScopedVariableExcludes,
            );
        }
    }
}

fn get_initializer_symbol(symbol: Option<P<Symbol>>) -> Option<P<Symbol>> {
    let symbol = symbol?;
    let declaration = symbol.value_declaration()?;
    // For an assignment 'fn.xxx = ...', where 'fn' is a previously declared function or a previously
    // declared const variable initialized with a function expression or arrow function, we add expando
    // property declarations to the function's symbol. This also applies to class expressions in JS files,
    // and empty object literals in JS files when the declaration doesn't have a type annotation.
    if ast::is_function_declaration(declaration)
        || ast::is_in_js_file(Some(declaration)) && ast::is_class_declaration(declaration)
    {
        return Some(symbol);
    } else if ast::is_variable_declaration(declaration)
        && (declaration.parent().unwrap().flags().intersects(NodeFlags::Const)
            || ast::is_in_js_file(Some(declaration)))
    {
        let initializer = declaration.initializer();
        if ast::is_expando_initializer(declaration, initializer) {
            return initializer.unwrap().symbol();
        }
    } else if ast::is_binary_expression(declaration) && ast::is_in_js_file(Some(declaration)) {
        let initializer = declaration.as_binary_expression().right();
        if ast::is_expando_initializer(declaration, Some(initializer)) {
            return initializer.symbol();
        }
    }
    None
}

impl Binder {
    fn bind_this_property_assignment(&mut self, node: P<Node>) {
        if !ast::is_in_js_file(Some(node)) {
            return;
        }
        let bin = node.as_binary_expression();
        if ast::is_property_access_expression(bin.left())
            && ast::is_private_identifier(bin.left().name().unwrap())
            || self.this_container.is_none()
        {
            return;
        }
        let (class_symbol, symbol_table) = self.get_this_class_and_symbol_table();
        if let Some(symbol_table) = symbol_table {
            if ast::has_dynamic_name(node) {
                self.declare_symbol_ex(
                    symbol_table,
                    class_symbol,
                    node,
                    SymbolFlags::Property,
                    SymbolFlags::None,
                    true, /*isReplaceableByMethod*/
                    true, /*isComputedName*/
                );
                self.add_late_bound_assignment_declaration_to_symbol(node, class_symbol.unwrap());
            } else {
                self.declare_symbol_ex(
                    symbol_table,
                    class_symbol,
                    node,
                    SymbolFlags::Property | SymbolFlags::Assignment,
                    SymbolFlags::None,
                    true,  /*isReplaceableByMethod*/
                    false, /*isComputedName*/
                );
            }
        } else {
            let this_container = self.this_container.unwrap();
            // !!! constructor functions
            assert!(
                !(this_container.kind() != Kind::FunctionDeclaration
                    && this_container.kind() != Kind::FunctionExpression),
                "Unhandled case in bindThisPropertyAssignment: {:?}",
                this_container.kind()
            );
        }
    }

    fn get_this_class_and_symbol_table(&self) -> (Option<P<Symbol>>, Option<P<SymbolTable>>) {
        let Some(this_container) = self.this_container else {
            return (None, None);
        };
        let mut class_symbol = None;
        let mut symbol_table = None;
        match this_container.kind() {
            Kind::Constructor
            | Kind::PropertyDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassStaticBlockDeclaration => {
                // this.property assignment in class member -- bind to the containing class
                class_symbol = this_container.parent().unwrap().symbol();
                if ast::is_static(this_container) {
                    symbol_table = Some(ast::get_exports(class_symbol.unwrap()));
                } else {
                    symbol_table = Some(ast::get_members(class_symbol.unwrap()));
                }
            }
            // !!! constructor functions (FunctionDeclaration and FunctionExpression)
            _ => {}
        }
        (class_symbol, symbol_table)
    }

    fn bind_enum_declaration(&mut self, node: P<Node>) {
        if ast::is_enum_const(node) {
            self.bind_block_scoped_declaration(
                node,
                SymbolFlags::ConstEnum,
                SymbolFlags::ConstEnumExcludes,
            );
        } else {
            self.bind_block_scoped_declaration(
                node,
                SymbolFlags::RegularEnum,
                SymbolFlags::RegularEnumExcludes,
            );
        }
    }

    fn bind_variable_declaration_or_binding_element(&mut self, node: P<Node>) {
        self.check_strict_mode_eval_or_arguments(node, node.name());
        if let Some(name) = node.name()
            && !ast::is_binding_pattern(name)
        {
            if ast::is_variable_declaration_initialized_to_require(node) {
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::Alias,
                    SymbolFlags::AliasExcludes,
                );
            } else if ast::is_block_or_catch_scoped(node) {
                self.bind_block_scoped_declaration(
                    node,
                    SymbolFlags::BlockScopedVariable,
                    SymbolFlags::BlockScopedVariableExcludes,
                );
            } else if ast::is_part_of_parameter_declaration(node) {
                // It is safe to walk up parent chain to find whether the node is a destructuring parameter declaration
                // because its parent chain has already been set up, since parents are set before descending into children.
                //
                // If node is a binding element in parameter declaration, we need to use ParameterExcludes.
                // Using ParameterExcludes flag allows the compiler to report an error on duplicate identifiers in Parameter Declaration
                // For example:
                //      function foo([a,a]) {} // Duplicate Identifier error
                //      function bar(a,a) {}   // Duplicate Identifier error, parameter declaration in this case is handled in bindParameter
                //                             // which correctly set excluded symbols
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::FunctionScopedVariable,
                    SymbolFlags::ParameterExcludes,
                );
            } else {
                self.declare_symbol_and_add_to_symbol_table(
                    node,
                    SymbolFlags::FunctionScopedVariable,
                    SymbolFlags::FunctionScopedVariableExcludes,
                );
            }
        }
    }

    fn bind_parameter(&mut self, node: P<Node>) {
        let decl = node.as_parameter_declaration();
        if !node.flags().intersects(NodeFlags::Ambient) {
            // It is a SyntaxError if the identifier eval or arguments appears within a FormalParameterList of a
            // strict mode FunctionLikeDeclaration or FunctionExpression(13.1)
            self.check_strict_mode_eval_or_arguments(node, node.name());
        }
        if ast::is_binding_pattern(node.name().unwrap()) {
            let index = node
                .parent()
                .unwrap()
                .parameters()
                .iter()
                .position(|&p| p == node)
                .expect("parameter belongs to its parent");
            self.bind_anonymous_declaration(
                node,
                SymbolFlags::FunctionScopedVariable,
                alloc_str(&format!("__{index}")),
            );
        } else {
            self.declare_symbol_and_add_to_symbol_table(
                node,
                SymbolFlags::FunctionScopedVariable,
                SymbolFlags::ParameterExcludes,
            );
        }
        // If this is a property-parameter, then also declare the property symbol into the
        // containing class.
        if ast::is_parameter_property_declaration(node, node.parent().unwrap()) {
            let class_declaration = node.parent().unwrap().parent().unwrap();
            let flags = SymbolFlags::Property
                | if decl.question_token().is_some() {
                    SymbolFlags::Optional
                } else {
                    SymbolFlags::None
                };
            self.declare_symbol(
                ast::get_members(class_declaration.symbol().unwrap()),
                class_declaration.symbol(),
                node,
                flags,
                SymbolFlags::PropertyExcludes,
            );
        }
    }

    fn bind_function_declaration(&mut self, node: P<Node>) {
        if !self.file.is_declaration_file()
            && !node.flags().intersects(NodeFlags::Ambient)
            && ast::is_async_function(node)
        {
            self.emit_flags |= NodeFlags::HasAsyncFunctions;
        }
        self.check_strict_mode_function_name(node);
        self.bind_block_scoped_declaration(
            node,
            SymbolFlags::Function,
            SymbolFlags::FunctionExcludes,
        );
    }

    fn get_infer_type_container(node: P<Node>) -> Option<P<Node>> {
        let extends_type = ast::find_ancestor(Some(node), |n| {
            let parent = n.parent();
            match parent {
                Some(parent) => {
                    ast::is_conditional_type_node(parent)
                        && parent.as_conditional_type_node().extends_type() == n
                }
                None => false,
            }
        });
        extends_type.map(|e| e.parent().unwrap())
    }

    fn bind_anonymous_declaration(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        name: &'static str,
    ) {
        let symbol = self.new_symbol(symbol_flags, name);
        if symbol_flags.intersects(SymbolFlags::EnumMember | SymbolFlags::ClassMember) {
            symbol.set_parent(self.container().symbol());
        }
        self.add_declaration_to_symbol(symbol, node, symbol_flags);
    }

    fn bind_block_scoped_declaration(
        &mut self,
        node: P<Node>,
        symbol_flags: SymbolFlags,
        symbol_excludes: SymbolFlags,
    ) {
        let block_scope_container = self.block_scope_container.unwrap();
        match block_scope_container.kind() {
            Kind::ModuleDeclaration => {
                self.declare_module_member(node, symbol_flags, symbol_excludes);
            }
            Kind::SourceFile
                if ast::is_external_or_common_js_module(
                    self.container().as_source_file().as_p(),
                ) =>
            {
                self.declare_module_member(node, symbol_flags, symbol_excludes);
            }
            _ => {
                self.declare_symbol(
                    ast::get_locals(block_scope_container),
                    None, /*parent*/
                    node,
                    symbol_flags,
                    symbol_excludes,
                );
            }
        }
    }

    fn bind_type_parameter(&mut self, node: P<Node>) {
        if node.parent().unwrap().kind() == Kind::InferType {
            let container = Self::get_infer_type_container(node.parent().unwrap());
            if let Some(container) = container {
                self.declare_symbol(
                    ast::get_locals(container),
                    None, /*parent*/
                    node,
                    SymbolFlags::TypeParameter,
                    SymbolFlags::TypeParameterExcludes,
                );
            } else {
                let name = Self::get_declaration_name(node);
                self.bind_anonymous_declaration(node, SymbolFlags::TypeParameter, name);
            }
        } else {
            self.declare_symbol_and_add_to_symbol_table(
                node,
                SymbolFlags::TypeParameter,
                SymbolFlags::TypeParameterExcludes,
            );
        }
    }

    fn lookup_entity(&self, node: P<Node>, container: P<Node>) -> Option<P<Symbol>> {
        if ast::is_identifier(node) {
            return Self::lookup_name(node.text(), container);
        }
        if node.expression().unwrap().kind() == Kind::ThisKeyword {
            let (_, symbol_table) = self.get_this_class_and_symbol_table();
            if let Some(symbol_table) = symbol_table
                && let Some(name) = ast::get_element_or_property_access_name(node)
            {
                return (*symbol_table).get(name.text());
            }
            return None;
        }
        if let Some(symbol) =
            get_initializer_symbol(self.lookup_entity(node.expression().unwrap(), container))
            && let Some(exports) = symbol.exports()
            && let Some(name) = ast::get_element_or_property_access_name(node)
        {
            return (*exports).get(name.text());
        }
        None
    }

    fn lookup_name(name: &str, container: P<Node>) -> Option<P<Symbol>> {
        if let Some(locals_container) = container.locals_container_data()
            && let Some(locals) = locals_container.locals.get()
            && let Some(local) = (*locals).get(name)
        {
            return Some(local.export_symbol().unwrap_or(local));
        }
        if let Some(declaration) = container.declaration_data()
            && let Some(symbol) = declaration.symbol.get()
        {
            return (*symbol.exports()?).get(name);
        }
        None
    }

    // The binder visits every node in the syntax tree so it is a convenient place to perform a single localized
    // check for reserved words used as identifiers in strict mode code, as well as `yield` or `await` in
    // [Yield] or [Await] contexts, respectively.
    fn check_contextual_identifier(&mut self, node: P<Node>) {
        // Report error only if there are no parse errors in file
        if self.file.diagnostics().is_empty()
            && !node.flags().intersects(NodeFlags::Ambient)
            && !node.flags().intersects(NodeFlags::JSDoc)
            && !ast::is_identifier_name(node)
        {
            // strict mode identifiers
            let original_keyword_kind = scanner::get_identifier_token(node.text());
            if original_keyword_kind == Kind::Identifier {
                return;
            }
            self.check_contextual_keyword(node, original_keyword_kind);
        }
    }

    #[cold]
    #[inline(never)]
    fn check_contextual_keyword(&mut self, node: P<Node>, original_keyword_kind: Kind) {
        if original_keyword_kind >= Kind::FIRST_FUTURE_RESERVED_WORD
            && original_keyword_kind <= Kind::LAST_FUTURE_RESERVED_WORD
        {
            let message = self.get_strict_mode_identifier_message(node);
            self.error_on_node(node, message, &[&scanner::declaration_name_to_string(Some(node))]);
        } else if original_keyword_kind == Kind::AwaitKeyword {
            if ast::is_external_module(self.file) && ast::is_in_top_level_context(node) {
                self.error_on_node(
                    node,
                    &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_AT_THE_TOP_LEVEL_OF_A_MODULE,
                    &[&scanner::declaration_name_to_string(Some(node))],
                );
            } else if node.flags().intersects(NodeFlags::AwaitContext) {
                self.error_on_node(
                    node,
                    &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_THAT_CANNOT_BE_USED_HERE,
                    &[&scanner::declaration_name_to_string(Some(node))],
                );
            }
        } else if original_keyword_kind == Kind::YieldKeyword
            && node.flags().intersects(NodeFlags::YieldContext)
        {
            self.error_on_node(
                node,
                &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_THAT_CANNOT_BE_USED_HERE,
                &[&scanner::declaration_name_to_string(Some(node))],
            );
        }
    }

    fn check_private_identifier(&mut self, node: P<Node>) {
        if node.text() == "#constructor" {
            // Report error only if there are no parse errors in file
            if self.file.diagnostics().is_empty() {
                self.error_on_node(
                    node,
                    &diagnostics::X_CONSTRUCTOR_IS_A_RESERVED_WORD,
                    &[&scanner::declaration_name_to_string(Some(node))],
                );
            }
        }
    }

    fn get_strict_mode_identifier_message(&self, node: P<Node>) -> &'static Message {
        // Provide specialized messages to help the user understand why we think they're in
        // strict mode.
        if ast::get_containing_class(node).is_some() {
            return &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_IN_STRICT_MODE_CLASS_DEFINITIONS_ARE_AUTOMATICALLY_IN_STRICT_MODE;
        }
        if self.file.external_module_indicator.get().is_some() {
            return &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_IN_STRICT_MODE_MODULES_ARE_AUTOMATICALLY_IN_STRICT_MODE;
        }
        &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_IN_STRICT_MODE
    }
}

// Should be called only on prologue directives (ast.IsPrologueDirective(node) should be true)
fn is_use_strict_prologue_directive(source_file: P<SourceFile>, node: P<Node>) -> bool {
    let node_text = scanner::get_source_text_of_node_from_source_file(
        source_file,
        node.expression().unwrap(),
        false, /*includeTrivia*/
    );
    // Note: the node text must be exactly "use strict" or 'use strict'.  It is not ok for the
    // string to contain unicode escapes (as per ES5).
    node_text == "\"use strict\"" || node_text == "'use strict'"
}

pub fn find_use_strict_prologue(
    source_file: P<SourceFile>,
    statements: &[P<Node>],
) -> Option<P<Node>> {
    if let Some(result) = statements.iter().find_map(|&statement| {
        if ast::is_prologue_directive(statement) {
            if is_use_strict_prologue_directive(source_file, statement) {
                return Some(Some(statement));
            }
        } else {
            return Some(None);
        }

        None
    }) {
        return result;
    }
    None
}

impl Binder {
    fn check_strict_mode_function_name(&mut self, node: P<Node>) {
        if !node.flags().intersects(NodeFlags::Ambient) {
            // It is a SyntaxError if the identifier eval or arguments appears within a FormalParameterList of a strict mode FunctionDeclaration or FunctionExpression (13.1))
            self.check_strict_mode_eval_or_arguments(node, node.name());
        }
    }

    fn check_strict_mode_binary_expression(&mut self, node: P<Node>) {
        let expr = node.as_binary_expression();
        if ast::is_left_hand_side_expression(expr.left())
            && ast::is_assignment_operator(expr.operator_token().kind())
        {
            // ECMA 262 (Annex C) The identifier eval or arguments may not appear as the LeftHandSideExpression of an
            // Assignment operator(11.13) or of a PostfixExpression(11.3)
            self.check_strict_mode_eval_or_arguments(node, Some(expr.left()));
        }
    }

    fn check_strict_mode_catch_clause(&mut self, node: P<Node>) {
        // It is a SyntaxError if a TryStatement with a Catch occurs within strict code and the Identifier of the
        // Catch production is eval or arguments
        let clause = node.as_catch_clause();
        if let Some(variable_declaration) = clause.variable_declaration() {
            self.check_strict_mode_eval_or_arguments(node, variable_declaration.name());
        }
    }

    fn check_strict_mode_delete_expression(&mut self, node: P<Node>) {
        // Grammar checking
        let expr = node.as_delete_expression();
        if expr.expression().kind() == Kind::Identifier {
            // When a delete operator occurs within strict mode code, a SyntaxError is thrown if its
            // UnaryExpression is a direct reference to a variable, function argument, or function name
            self.error_on_node(
                expr.expression(),
                &diagnostics::X_DELETE_CANNOT_BE_CALLED_ON_AN_IDENTIFIER_IN_STRICT_MODE,
                &[],
            );
        }
    }

    fn check_strict_mode_postfix_unary_expression(&mut self, node: P<Node>) {
        // Grammar checking
        // The identifier eval or arguments may not appear as the LeftHandSideExpression of an
        // Assignment operator(11.13) or of a PostfixExpression(11.3) or as the UnaryExpression
        // operated upon by a Prefix Increment(11.4.4) or a Prefix Decrement(11.4.5) operator.
        self.check_strict_mode_eval_or_arguments(
            node,
            Some(node.as_postfix_unary_expression().operand()),
        );
    }

    fn check_strict_mode_prefix_unary_expression(&mut self, node: P<Node>) {
        // Grammar checking
        let expr = node.as_prefix_unary_expression();
        if expr.operator() == Kind::PlusPlusToken || expr.operator() == Kind::MinusMinusToken {
            self.check_strict_mode_eval_or_arguments(node, Some(expr.operand()));
        }
    }

    fn check_strict_mode_with_statement(&mut self, node: P<Node>) {
        // Grammar checking for withStatement
        self.error_on_first_token(
            node,
            &diagnostics::X_WITH_STATEMENTS_ARE_NOT_ALLOWED_IN_STRICT_MODE,
            &[],
        );
    }

    fn check_strict_mode_labeled_statement(&mut self, node: P<Node>) {
        // Grammar checking for labeledStatement
        let data = node.as_labeled_statement();
        if ast::is_declaration_statement(data.statement())
            || ast::is_variable_statement(data.statement())
        {
            self.error_on_first_token(data.label(), &diagnostics::A_LABEL_IS_NOT_ALLOWED_HERE, &[]);
        }
    }
}

fn is_eval_or_arguments_identifier(node: P<Node>) -> bool {
    if ast::is_identifier(node) {
        let text = node.text();
        return text == "eval" || text == "arguments";
    }
    false
}

impl Binder {
    fn check_strict_mode_eval_or_arguments(
        &mut self,
        context_node: P<Node>,
        name: Option<P<Node>>,
    ) {
        if let Some(name) = name
            && is_eval_or_arguments_identifier(name)
        {
            // We check first if the name is inside class declaration or class expression; if so give explicit message
            // otherwise report generic error message.
            let message = self.get_strict_mode_eval_or_arguments_message(context_node);
            self.error_on_node(name, message, &[&name.text()]);
        }
    }

    fn get_strict_mode_eval_or_arguments_message(&self, node: P<Node>) -> &'static Message {
        // Provide specialized messages to help the user understand why we think they're in strict mode
        if ast::get_containing_class(node).is_some() {
            return &diagnostics::CODE_CONTAINED_IN_A_CLASS_IS_EVALUATED_IN_JAVA_SCRIPT_S_STRICT_MODE_WHICH_DOES_NOT_ALLOW_THIS_USE_OF_0_FOR_MORE_INFORMATION_SEE_HTTPS_COLON_SLASH_SLASHDEVELOPER_MOZILLA_ORG_SLASHEN_US_SLASHDOCS_SLASH_WEB_SLASH_JAVA_SCRIPT_SLASH_REFERENCE_SLASH_STRICT_MODE;
        }
        if self.file.external_module_indicator.get().is_some() {
            return &diagnostics::INVALID_USE_OF_0_MODULES_ARE_AUTOMATICALLY_IN_STRICT_MODE;
        }
        &diagnostics::INVALID_USE_OF_0_IN_STRICT_MODE
    }

    // All container nodes are kept on a linked list in declaration order. This list is used by
    // the getLocalNameOfContainer function in the type checker to validate that the local name
    // used for a container is unique.
    fn bind_container(&mut self, node: P<Node>, container_flags: ContainerFlags) {
        // Before we recurse into a node's children, we first save the existing parent, container
        // and block-container.  Then after we pop out of processing the children, we restore
        // these saved values.
        let save_container = self.container;
        let save_this_container = self.this_container;
        let saved_block_scope_container = self.block_scope_container;
        // Depending on what kind of node this is, we may have to adjust the current container
        // and block-container.   If the current node is a container, then it is automatically
        // considered the current block-container as well.  Also, for containers that we know
        // may contain locals, we eagerly initialize the .locals field. We do this because
        // it's highly likely that the .locals will be needed to place some child in (for example,
        // a parameter, or variable declaration).
        //
        // However, we do not proactively create the .locals for block-containers because it's
        // totally normal and common for block-containers to never actually have a block-scoped
        // variable in them.  We don't want to end up allocating an object for every 'block' we
        // run into when most of them won't be necessary.
        //
        // Finally, if this is a block-container, then we clear out any existing .locals object
        // it may contain within it.  This happens in incremental scenarios.  Because we can be
        // reusing a node from a previous compilation, that node may have had 'locals' created
        // for it.  We must clear this so we don't accidentally move any stale data forward from
        // a previous compilation.
        if container_flags.intersects(ContainerFlags::IsContainer) {
            self.container = Some(node);
            self.block_scope_container = Some(node);
            if container_flags.intersects(ContainerFlags::HasLocals) {
                // localsContainer := node
                // localsContainer.LocalsContainerData().locals = make(SymbolTable)
                self.add_to_container_chain(node);
            }
        } else if container_flags.intersects(ContainerFlags::IsBlockScopedContainer) {
            self.block_scope_container = Some(node);
            self.add_to_container_chain(node);
        }
        if container_flags.intersects(ContainerFlags::IsThisContainer) {
            self.this_container = Some(node);
        }
        if container_flags.intersects(ContainerFlags::IsControlFlowContainer) {
            let save_current_flow = self.current_flow;
            let save_break_target = self.current_break_target;
            let save_continue_target = self.current_continue_target;
            let save_return_target = self.current_return_target;
            let save_exception_target = self.current_exception_target;
            let save_active_label_list = std::mem::take(&mut self.active_label_list);
            let save_has_explicit_return = self.has_explicit_return;
            let save_seen_this_keyword = self.seen_this_keyword;
            let is_immediately_invoked = (container_flags
                .intersects(ContainerFlags::IsFunctionExpression)
                && !ast::has_syntactic_modifier(node, ModifierFlags::Async)
                && !is_generator_function_expression(node)
                && ast::get_immediately_invoked_function_expression(node).is_some())
                || node.kind() == Kind::ClassStaticBlockDeclaration;
            // A non-async, non-generator IIFE is considered part of the containing control flow. Return statements behave
            // similarly to break statements that exit to a label just past the statement body.
            if !is_immediately_invoked {
                let flow_start = self.new_flow_node(FlowFlags::Start);
                self.current_flow = Some(flow_start);
                if container_flags.intersects(
                    ContainerFlags::IsFunctionExpression
                        | ContainerFlags::IsObjectLiteralOrClassExpressionMethodOrAccessor,
                ) {
                    flow_start.node.set(Some(node));
                }
            }
            // We create a return control flow graph for IIFEs and constructors. For constructors
            // we use the return control flow graph in strict property initialization checks.
            if is_immediately_invoked || node.kind() == Kind::Constructor {
                self.current_return_target = Some(self.new_flow_node(FlowFlags::BranchLabel));
            } else {
                self.current_return_target = None;
            }
            self.current_exception_target = None;
            self.current_break_target = None;
            self.current_continue_target = None;
            self.has_explicit_return = false;
            self.seen_this_keyword = false;
            self.bind_children(node);
            // Reset flags (for incremental scenarios)
            node.set_flags(
                node.flags() & !(NodeFlags::ReachabilityAndEmitFlags | NodeFlags::ContainsThis),
            );
            if !self.current_flow().flags.get().intersects(FlowFlags::Unreachable)
                && container_flags.intersects(ContainerFlags::IsFunctionLike)
                && let Some(body_data) = node.body_data()
                && ast::node_is_present(body_data.body)
            {
                node.set_flags(node.flags() | NodeFlags::HasImplicitReturn);
                if self.has_explicit_return {
                    node.set_flags(node.flags() | NodeFlags::HasExplicitReturn);
                }
                body_data.end_flow_node.set(self.current_flow);
            }
            if self.seen_this_keyword {
                node.set_flags(node.flags() | NodeFlags::ContainsThis);
            }
            if node.kind() == Kind::SourceFile {
                node.set_flags(node.flags() | self.emit_flags);
            }
            if let Some(return_target) = self.current_return_target {
                Self::add_antecedent(self.current_return_target, self.current_flow());
                self.current_flow = Some(self.finish_flow_label(return_target));
                if node.kind() == Kind::Constructor
                    || node.kind() == Kind::ClassStaticBlockDeclaration
                {
                    set_return_flow_node(node, self.current_flow);
                }
            }
            if !is_immediately_invoked {
                self.current_flow = save_current_flow;
            }
            self.current_break_target = save_break_target;
            self.current_continue_target = save_continue_target;
            self.current_return_target = save_return_target;
            self.current_exception_target = save_exception_target;
            self.active_label_list = save_active_label_list;
            self.has_explicit_return = save_has_explicit_return;
            if container_flags.intersects(ContainerFlags::PropagatesThisKeyword) {
                self.seen_this_keyword = save_seen_this_keyword || self.seen_this_keyword;
            } else {
                self.seen_this_keyword = save_seen_this_keyword;
            }
        } else if container_flags.intersects(ContainerFlags::IsInterface) {
            let save_seen_this_keyword = self.seen_this_keyword;
            self.seen_this_keyword = false;
            self.bind_children(node);
            // ContainsThis cannot overlap with HasExtendedUnicodeEscape on Identifier
            if self.seen_this_keyword {
                node.set_flags(node.flags() | NodeFlags::ContainsThis);
            } else {
                node.set_flags(node.flags() & !NodeFlags::ContainsThis);
            }
            self.seen_this_keyword = save_seen_this_keyword;
        } else {
            self.bind_children(node);
        }
        if ast::is_source_file(node) && ast::is_in_js_file(Some(node)) {
            // Binding of top-level JSTypeAliasDeclaration nodes is deferred to ensure CommonJS module
            // indicators, if any, are processed first.
            for &statement in node.statements() {
                if ast::is_js_type_alias_declaration(statement) {
                    self.bind_block_scoped_declaration(
                        statement,
                        SymbolFlags::TypeAlias,
                        SymbolFlags::TypeAliasExcludes,
                    );
                }
            }
            if self.file.common_js_module_indicator.get().is_some() {
                self.declare_common_js_variable("module");
                self.declare_common_js_variable("exports");
            }
        }
        if ast::is_source_file(node)
            && ast::is_external_or_common_js_module(node.as_source_file().as_p())
            || ast::is_ambient_module(node)
        {
            Self::bind_common_js_type_exports(node.symbol().unwrap());
        }
        self.container = save_container;
        self.this_container = save_this_container;
        self.block_scope_container = saved_block_scope_container;
    }

    fn declare_common_js_variable(&mut self, name: &'static str) {
        let locals = ast::get_locals(self.file.as_node());
        if (*locals).get(name).is_none() {
            let symbol = self
                .new_symbol(SymbolFlags::FunctionScopedVariable | SymbolFlags::ModuleExports, name);
            symbol.set_declarations(&[self.file.as_node()]);
            symbol.set_value_declaration(Some(self.file.as_node()));
            if name == "module" {
                let exports_property =
                    self.new_symbol(SymbolFlags::ModuleExports | SymbolFlags::Property, "exports");
                exports_property.set_declarations_static(symbol.declarations());
                exports_property.set_value_declaration(symbol.value_declaration());
                exports_property.set_parent(Some(symbol));
                let members = SymbolTable::new();
                members.set("exports", exports_property);
                symbol.set_members(Some(members));
            }
            locals.set(name, symbol);
        }
    }

    fn bind_children(&mut self, node: P<Node>) {
        let save_in_assignment_pattern = self.in_assignment_pattern;
        // Most nodes aren't valid in an assignment pattern, so we clear the value here
        // and set it before we descend into nodes that could actually be part of an assignment pattern.
        self.in_assignment_pattern = false;

        if self.current_flow == Some(self.unreachable_flow) {
            if node.has_flow_node_data() {
                node.set_flow_node(None);
            }
            if ast::is_potentially_executable_node(node) {
                node.set_flags(node.flags() | NodeFlags::Unreachable);
            }
            self.bind_each_child(node);
            self.bind_lazy_member_list(node);
            self.in_assignment_pattern = save_in_assignment_pattern;
            return;
        }

        if Kind::FIRST_STATEMENT <= node.kind()
            && node.kind() <= Kind::LAST_STATEMENT
            && node.has_flow_node_data()
        {
            node.set_flow_node(self.current_flow);
        }

        match node.kind() {
            Kind::WhileStatement => self.bind_while_statement(node),
            Kind::DoStatement => self.bind_do_statement(node),
            Kind::ForStatement => self.bind_for_statement(node),
            Kind::ForInStatement | Kind::ForOfStatement => {
                self.bind_for_in_or_for_of_statement(node);
            }
            Kind::IfStatement => self.bind_if_statement(node),
            Kind::ReturnStatement => self.bind_return_statement(node),
            Kind::ThrowStatement => self.bind_throw_statement(node),
            Kind::BreakStatement => self.bind_break_statement(node),
            Kind::ContinueStatement => self.bind_continue_statement(node),
            Kind::TryStatement => self.bind_try_statement(node),
            Kind::SwitchStatement => self.bind_switch_statement(node),
            Kind::CaseBlock => self.bind_case_block(node),
            Kind::CaseClause | Kind::DefaultClause => self.bind_case_or_default_clause(node),
            Kind::ExpressionStatement => self.bind_expression_statement(node),
            Kind::LabeledStatement => self.bind_labeled_statement(node),
            Kind::PrefixUnaryExpression => self.bind_prefix_unary_expression_flow(node),
            Kind::PostfixUnaryExpression => self.bind_postfix_unary_expression_flow(node),
            Kind::BinaryExpression => {
                if ast::is_destructuring_assignment(node) {
                    // Carry over whether we are in an assignment pattern to
                    // binary expressions that could actually be an initializer
                    self.in_assignment_pattern = save_in_assignment_pattern;
                    self.bind_destructuring_assignment_flow(node);
                    return;
                }
                self.bind_binary_expression_flow(node);
            }
            Kind::DeleteExpression => self.bind_delete_expression_flow(node),
            Kind::ConditionalExpression => self.bind_conditional_expression_flow(node),
            Kind::VariableDeclaration => self.bind_variable_declaration_flow(node),
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                self.bind_access_expression_flow(node);
            }
            Kind::CallExpression => self.bind_call_expression_flow(node),
            Kind::NonNullExpression => self.bind_non_null_expression_flow(node),
            Kind::SourceFile => {
                let source_file = node.as_source_file();
                self.bind_each_statement_functions_first(source_file.statements);
                self.bind(source_file.end_of_file_token);
            }
            Kind::Block | Kind::ModuleBlock => {
                self.bind_each_statement_functions_first(node.statement_list().unwrap());
            }
            Kind::BindingElement => self.bind_binding_element_flow(node),
            Kind::Parameter => self.bind_parameter_flow(node),
            Kind::ObjectLiteralExpression
            | Kind::ArrayLiteralExpression
            | Kind::PropertyAssignment
            | Kind::SpreadElement => {
                self.in_assignment_pattern = save_in_assignment_pattern;
                self.bind_each_child(node);
            }
            Kind::InterfaceDeclaration
            | Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::TypeLiteral => {
                self.bind_each_child(node);
                self.bind_lazy_member_list(node);
            }
            _ => self.bind_each_child(node),
        }
        self.in_assignment_pattern = save_in_assignment_pattern;
    }

    /// tsrs-only (`tsrs_ast` `lazylist`, notes/mem-lazy-dts-members.md): `node`'s member list, if the parser
    /// made it lazy, was just skipped by `bind_each_child` (an unparsed lazy list has no members yet), at the
    /// point where the members would have been bound. In the usual state (no flow targets or labels,
    /// reachable, an owner symbol) the binder state is recorded on the list and the owner symbol, and the
    /// first reader binds the members with it; otherwise the list is parsed now and bound here, as if it had
    /// never been lazy.
    fn bind_lazy_member_list(&mut self, node: P<Node>) {
        if !matches!(
            node.kind(),
            Kind::InterfaceDeclaration
                | Kind::ClassDeclaration
                | Kind::ClassExpression
                | Kind::TypeLiteral
        ) {
            return;
        }
        let Some(record) = node.member_list().and_then(|l| l.lazy_record()) else {
            return;
        };
        if record.state() != lazylist::PENDING {
            return;
        }
        let symbol = node.symbol();
        let plain = symbol.is_some()
            && self.current_flow != Some(self.unreachable_flow)
            && self.current_break_target.is_none()
            && self.current_continue_target.is_none()
            && self.current_return_target.is_none()
            && self.current_true_target.is_none()
            && self.current_false_target.is_none()
            && self.current_exception_target.is_none()
            && self.pre_switch_case_flow.is_none()
            && self.active_label_list.is_empty()
            && !self.in_assignment_pattern;
        if !plain {
            let nodes = record.parse_now(self.file);
            self.bind_each(nodes);
            return;
        }
        let ctx = P::new(LazyBindContext {
            container: self.container,
            this_container: self.this_container,
            block_scope_container: self.block_scope_container,
            last_container: self.last_container,
            current_flow: self.current_flow,
            unreachable_flow: self.unreachable_flow,
        });
        record.defer(ctx);
        symbol.unwrap().set_lazy_list(Some(P::from_static(record)));
    }

    /// A declaration is about to merge into a symbol whose earlier declaration has a deferred member list: bind that
    /// list first, so the symbol's tables get their entries in source order (as if it had never been lazy).
    fn force_pending_lazy_list(&mut self, record: &'static LazyNodeList) {
        let file = self.file;
        record.force_inline(file, &mut |nodes| {
            let ctx = record.bind_context().expect("a deferred list's binder state");
            self.bind_lazy_members(*ctx, nodes, true);
        });
    }

    /// Binds the members of a deferred lazy list with the binder state recorded at the list, then puts this binder's
    /// state back. With `splice` (the file's own binder, `force_pending_lazy_list`), containers the members add to the
    /// container chain are linked in where the list is, before the containers bound after it.
    ///
    /// Without it (a list forced after its file was bound, on any thread: `bind_lazy_list`), the members' containers
    /// are chained among themselves but not into the file's chain. Lists deferred one after another record the same
    /// `last_container`, so two threads forcing two of them would write that node's `next_container` at once (a data
    /// race that `ThreadSanitizer` reported in `fileregions::force_shared_lists`). Only declaration files have lazy lists, and
    /// the chain is read only for emitted files (`tsrs_printer` name generator), so nothing reads these links.
    fn bind_lazy_members(&mut self, ctx: LazyBindContext, nodes: &'static [P<Node>], splice: bool) {
        let saved = (
            self.container,
            self.this_container,
            self.block_scope_container,
            self.last_container,
            self.current_flow,
            self.unreachable_flow,
            self.current_break_target,
            self.current_continue_target,
            self.current_return_target,
            self.current_true_target,
            self.current_false_target,
            self.current_exception_target,
            self.pre_switch_case_flow,
        );
        let saved_labels = std::mem::take(&mut self.active_label_list);
        let saved_in_assignment_pattern = self.in_assignment_pattern;
        self.container = ctx.container;
        self.this_container = ctx.this_container;
        self.block_scope_container = ctx.block_scope_container;
        self.last_container = if splice { ctx.last_container } else { None };
        self.current_flow = ctx.current_flow;
        self.unreachable_flow = ctx.unreachable_flow;
        self.current_break_target = None;
        self.current_continue_target = None;
        self.current_return_target = None;
        self.current_true_target = None;
        self.current_false_target = None;
        self.current_exception_target = None;
        self.pre_switch_case_flow = None;
        self.in_assignment_pattern = false;
        let next = if splice {
            ctx.last_container.and_then(|c| c.locals_container_data().unwrap().next_container.get())
        } else {
            None
        };
        self.bind_each(nodes);
        let last = self.last_container;
        if splice
            && last != ctx.last_container
            && let Some(last) = last
        {
            last.locals_container_data().unwrap().next_container.set(next);
        }
        (
            self.container,
            self.this_container,
            self.block_scope_container,
            self.last_container,
            self.current_flow,
            self.unreachable_flow,
            self.current_break_target,
            self.current_continue_target,
            self.current_return_target,
            self.current_true_target,
            self.current_false_target,
            self.current_exception_target,
            self.pre_switch_case_flow,
        ) = saved;
        self.active_label_list = saved_labels;
        self.in_assignment_pattern = saved_in_assignment_pattern;
        // The list was the end of the chain so far: what it added is the end now.
        if splice && saved.3 == ctx.last_container && next.is_none() {
            self.last_container = last;
        }
    }

    fn bind_each_child(&mut self, node: P<Node>) {
        node.for_each_child(&mut |child| self.bind(child));
    }

    fn bind_each(&mut self, nodes: &[P<Node>]) {
        for &node in nodes {
            self.bind(node);
        }
    }

    fn bind_node_list(&mut self, node_list: Option<P<NodeList>>) {
        if let Some(node_list) = node_list {
            self.bind_each(node_list.nodes());
        }
    }

    fn bind_modifiers(&mut self, modifiers: Option<P<ast::ModifierList>>) {
        if let Some(modifiers) = modifiers {
            self.bind_each(modifiers.nodes());
        }
    }

    fn bind_each_statement_functions_first(&mut self, statements: P<NodeList>) {
        for &node in statements.nodes() {
            if node.kind() == Kind::FunctionDeclaration {
                self.bind(node);
            }
        }
        for &node in statements.nodes() {
            if node.kind() != Kind::FunctionDeclaration {
                self.bind(node);
            }
        }
    }

    fn set_continue_target(&mut self, node: P<Node>, target: P<FlowNode>) -> P<FlowNode> {
        let mut node = node;
        let mut index = self.active_label_list.len();
        while index > 0 && node.parent().unwrap().kind() == Kind::LabeledStatement {
            index -= 1;
            self.active_label_list[index].continue_target = Some(target);
            node = node.parent().unwrap();
        }
        target
    }

    fn do_with_conditional_branches(
        &mut self,
        action: fn(&mut Binder, Option<P<Node>>) -> bool,
        value: Option<P<Node>>,
        true_target: Option<P<FlowNode>>,
        false_target: Option<P<FlowNode>>,
    ) {
        let saved_true_target = self.current_true_target;
        let saved_false_target = self.current_false_target;
        self.current_true_target = true_target;
        self.current_false_target = false_target;
        action(self, value);
        self.current_true_target = saved_true_target;
        self.current_false_target = saved_false_target;
    }

    fn bind_condition(
        &mut self,
        node: Option<P<Node>>,
        true_target: Option<P<FlowNode>>,
        false_target: Option<P<FlowNode>>,
    ) {
        self.do_with_conditional_branches(Binder::bind, node, true_target, false_target);
        let is_logical_like = match node {
            None => false,
            Some(node) => {
                is_logical_assignment_expression(node)
                    || ast::is_logical_expression(node)
                    || (ast::is_optional_chain(node) && ast::is_outermost_optional_chain(node))
            }
        };
        if node.is_none() || !is_logical_like {
            let true_condition =
                self.create_flow_condition(FlowFlags::TrueCondition, self.current_flow(), node);
            Self::add_antecedent(true_target, true_condition);
            let false_condition =
                self.create_flow_condition(FlowFlags::FalseCondition, self.current_flow(), node);
            Self::add_antecedent(false_target, false_condition);
        }
    }

    fn bind_iterative_statement(
        &mut self,
        node: P<Node>,
        break_target: Option<P<FlowNode>>,
        continue_target: Option<P<FlowNode>>,
    ) {
        let save_break_target = self.current_break_target;
        let save_continue_target = self.current_continue_target;
        self.current_break_target = break_target;
        self.current_continue_target = continue_target;
        self.bind(node);
        self.current_break_target = save_break_target;
        self.current_continue_target = save_continue_target;
    }
}

fn is_logical_assignment_expression(node: P<Node>) -> bool {
    ast::is_logical_or_coalescing_assignment_expression(ast::skip_parentheses(node))
}

impl Binder {
    fn bind_assignment_target_flow(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::ArrayLiteralExpression => {
                for &e in node.elements() {
                    if e.kind() == Kind::SpreadElement {
                        self.bind_assignment_target_flow(e.expression().unwrap());
                    } else {
                        self.bind_destructuring_target_flow(e);
                    }
                }
            }
            Kind::ObjectLiteralExpression => {
                for &p in node.properties() {
                    match p.kind() {
                        Kind::PropertyAssignment => {
                            self.bind_destructuring_target_flow(p.initializer().unwrap());
                        }
                        Kind::ShorthandPropertyAssignment => {
                            self.bind_assignment_target_flow(p.name().unwrap());
                        }
                        Kind::SpreadAssignment => {
                            self.bind_assignment_target_flow(p.expression().unwrap());
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if is_narrowable_reference(node) {
                    self.current_flow = Some(self.create_flow_mutation(
                        FlowFlags::Assignment,
                        self.current_flow(),
                        node,
                    ));
                }
            }
        }
    }

    fn bind_destructuring_target_flow(&mut self, node: P<Node>) {
        if ast::is_binary_expression(node)
            && node.as_binary_expression().operator_token().kind() == Kind::EqualsToken
        {
            self.bind_assignment_target_flow(node.as_binary_expression().left());
        } else {
            self.bind_assignment_target_flow(node);
        }
    }

    fn bind_while_statement(&mut self, node: P<Node>) {
        let stmt = node.as_while_statement();
        let loop_label = self.create_loop_label();
        let pre_while_label = self.set_continue_target(node, loop_label);
        let pre_body_label = self.create_branch_label();
        let post_while_label = self.create_branch_label();
        Self::add_antecedent(Some(pre_while_label), self.current_flow());
        self.current_flow = Some(pre_while_label);
        self.bind_condition(Some(stmt.expression()), Some(pre_body_label), Some(post_while_label));
        self.current_flow = Some(self.finish_local_flow_label(pre_body_label));
        self.bind_iterative_statement(
            stmt.statement(),
            Some(post_while_label),
            Some(pre_while_label),
        );
        Self::add_antecedent(Some(pre_while_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(post_while_label));
    }

    fn bind_do_statement(&mut self, node: P<Node>) {
        let stmt = node.as_do_statement();
        let pre_do_label = self.create_loop_label();
        let branch_label = self.create_branch_label();
        let pre_condition_label = self.set_continue_target(node, branch_label);
        let post_do_label = self.create_branch_label();
        Self::add_antecedent(Some(pre_do_label), self.current_flow());
        self.current_flow = Some(pre_do_label);
        self.bind_iterative_statement(
            stmt.statement(),
            Some(post_do_label),
            Some(pre_condition_label),
        );
        Self::add_antecedent(Some(pre_condition_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(pre_condition_label));
        self.bind_condition(Some(stmt.expression()), Some(pre_do_label), Some(post_do_label));
        self.current_flow = Some(self.finish_local_flow_label(post_do_label));
    }

    fn bind_for_statement(&mut self, node: P<Node>) {
        let stmt = node.as_for_statement();
        self.bind(stmt.initializer());
        if self.current_flow == Some(self.unreachable_flow) {
            // Unlike while/do, the for-loop initializer is bound inside this function before the loop's
            // flow graph is constructed. If it makes flow unreachable (e.g. a throwing IIFE), addAntecedent
            // will filter out the unreachable entry to preLoopLabel, leaving only the back-edge from the
            // incrementor. This creates a cycle with no exit that crashes isReachableFlowNodeWorker.
            // Bail out early and just bind the remaining children with unreachable flow.
            self.bind(stmt.condition());
            self.bind(stmt.statement());
            self.bind(stmt.incrementor());
            return;
        }
        let loop_label = self.create_loop_label();
        let pre_loop_label = self.set_continue_target(node, loop_label);
        let pre_body_label = self.create_branch_label();
        let pre_incrementor_label = self.create_branch_label();
        let post_loop_label = self.create_branch_label();
        Self::add_antecedent(Some(pre_loop_label), self.current_flow());
        self.current_flow = Some(pre_loop_label);
        self.bind_condition(stmt.condition(), Some(pre_body_label), Some(post_loop_label));
        self.current_flow = Some(self.finish_local_flow_label(pre_body_label));
        self.bind_iterative_statement(
            stmt.statement(),
            Some(post_loop_label),
            Some(pre_incrementor_label),
        );
        Self::add_antecedent(Some(pre_incrementor_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(pre_incrementor_label));
        self.bind(stmt.incrementor());
        Self::add_antecedent(Some(pre_loop_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(post_loop_label));
    }

    fn bind_for_in_or_for_of_statement(&mut self, node: P<Node>) {
        let stmt = node.as_for_in_or_of_statement();
        self.bind(stmt.expression());
        if self.current_flow == Some(self.unreachable_flow) {
            // Like the for-loop initializer, the for-in/for-of expression is bound before the loop's
            // flow graph is constructed. If it makes flow unreachable (e.g. a throwing IIFE), addAntecedent
            // will filter out the unreachable entry to preLoopLabel, leaving only the back-edge from the
            // loop body. This creates a cycle with no exit that crashes isReachableFlowNodeWorker.
            // Bail out early and just bind the remaining children with unreachable flow.
            self.bind(stmt.initializer());
            self.bind(stmt.statement());
            return;
        }
        let loop_label = self.create_loop_label();
        let pre_loop_label = self.set_continue_target(node, loop_label);
        let post_loop_label = self.create_branch_label();
        Self::add_antecedent(Some(pre_loop_label), self.current_flow());
        self.current_flow = Some(pre_loop_label);
        if node.kind() == Kind::ForOfStatement {
            self.bind(stmt.await_modifier());
        }
        Self::add_antecedent(Some(post_loop_label), self.current_flow());
        self.bind(stmt.initializer());
        if stmt.initializer().kind() != Kind::VariableDeclarationList {
            self.bind_assignment_target_flow(stmt.initializer());
        }
        self.bind_iterative_statement(
            stmt.statement(),
            Some(post_loop_label),
            Some(pre_loop_label),
        );
        Self::add_antecedent(Some(pre_loop_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(post_loop_label));
    }

    fn bind_if_statement(&mut self, node: P<Node>) {
        let stmt = node.as_if_statement();
        let then_label = self.create_branch_label();
        let else_label = self.create_branch_label();
        let post_if_label = self.create_branch_label();
        self.bind_condition(Some(stmt.expression()), Some(then_label), Some(else_label));
        self.current_flow = Some(self.finish_local_flow_label(then_label));
        self.bind(stmt.then_statement());
        Self::add_antecedent(Some(post_if_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(else_label));
        self.bind(stmt.else_statement());
        Self::add_antecedent(Some(post_if_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(post_if_label));
    }

    fn bind_return_statement(&mut self, node: P<Node>) {
        self.bind(node.expression());
        if self.current_return_target.is_some() {
            Self::add_antecedent(self.current_return_target, self.current_flow());
        }
        self.current_flow = Some(self.unreachable_flow);
        self.has_explicit_return = true;
        self.has_flow_effects = true;
    }

    fn bind_throw_statement(&mut self, node: P<Node>) {
        self.bind(node.expression());
        self.current_flow = Some(self.unreachable_flow);
        self.has_flow_effects = true;
    }

    fn bind_break_statement(&mut self, node: P<Node>) {
        self.bind_break_or_continue_statement(
            node.label(),
            self.current_break_target,
            ActiveLabel::break_target,
        );
    }

    fn bind_continue_statement(&mut self, node: P<Node>) {
        self.bind_break_or_continue_statement(
            node.label(),
            self.current_continue_target,
            ActiveLabel::continue_target,
        );
    }

    fn bind_break_or_continue_statement(
        &mut self,
        label: Option<P<Node>>,
        current_target: Option<P<FlowNode>>,
        get_target: fn(&ActiveLabel) -> Option<P<FlowNode>>,
    ) {
        self.bind(label);
        if let Some(label) = label {
            if let Some(index) = self.find_active_label(label.text()) {
                self.active_label_list[index].referenced = true;
                let target = get_target(&self.active_label_list[index]);
                self.bind_break_or_continue_flow(target);
            }
        } else {
            self.bind_break_or_continue_flow(current_target);
        }
    }

    // Returns the index of the active label in `active_label_list`.
    fn find_active_label(&self, name: &str) -> Option<usize> {
        self.active_label_list.iter().rposition(|label| label.name == name)
    }

    fn bind_break_or_continue_flow(&mut self, flow_label: Option<P<FlowNode>>) {
        if flow_label.is_some() {
            Self::add_antecedent(flow_label, self.current_flow());
            self.current_flow = Some(self.unreachable_flow);
            self.has_flow_effects = true;
        }
    }

    fn bind_try_statement(&mut self, node: P<Node>) {
        // We conservatively assume that *any* code in the try block can cause an exception, but we only need
        // to track code that causes mutations (because only mutations widen the possible control flow type of
        // a variable). The exceptionLabel is the target label for control flows that result from exceptions.
        // We add all mutation flow nodes as antecedents of this label such that we can analyze them as possible
        // antecedents of the start of catch or finally blocks. Furthermore, we add the current control flow to
        // represent exceptions that occur before any mutations.
        let stmt = node.as_try_statement();
        let save_return_target = self.current_return_target;
        let save_exception_target = self.current_exception_target;
        let normal_exit_label = self.create_branch_label();
        let return_label = self.create_branch_label();
        let mut exception_label = self.create_branch_label();
        if stmt.finally_block().is_some() {
            self.current_return_target = Some(return_label);
        }
        Self::add_antecedent(Some(exception_label), self.current_flow());
        self.current_exception_target = Some(exception_label);
        self.bind(stmt.try_block());
        Self::add_antecedent(Some(normal_exit_label), self.current_flow());
        if stmt.catch_clause().is_some() {
            // Start of catch clause is the target of exceptions from try block.
            self.current_flow = Some(self.finish_flow_label(exception_label));
            // The currentExceptionTarget now represents control flows from exceptions in the catch clause.
            // Effectively, in a try-catch-finally, if an exception occurs in the try block, the catch block
            // acts like a second try block.
            exception_label = self.create_branch_label();
            Self::add_antecedent(Some(exception_label), self.current_flow());
            self.current_exception_target = Some(exception_label);
            self.bind(stmt.catch_clause());
            Self::add_antecedent(Some(normal_exit_label), self.current_flow());
        }
        self.current_return_target = save_return_target;
        self.current_exception_target = save_exception_target;
        if stmt.finally_block().is_some() {
            // Possible ways control can reach the finally block:
            // 1) Normal completion of try block of a try-finally or try-catch-finally
            // 2) Normal completion of catch block (following exception in try block) of a try-catch-finally
            // 3) Return in try or catch block of a try-finally or try-catch-finally
            // 4) Exception in try block of a try-finally
            // 5) Exception in catch block of a try-catch-finally
            // When analyzing a control flow graph that starts inside a finally block we want to consider all
            // five possibilities above. However, when analyzing a control flow graph that starts outside (past)
            // the finally block, we only want to consider the first two (if we're past a finally block then it
            // must have completed normally). Likewise, when analyzing a control flow graph from return statements
            // in try or catch blocks in an IIFE, we only want to consider the third. To make this possible, we
            // inject a ReduceLabel node into the control flow graph. This node contains an alternate reduced
            // set of antecedents for the pre-finally label. As control flow analysis passes by a ReduceLabel
            // node, the pre-finally label is temporarily switched to the reduced antecedent set.
            let finally_label = self.create_branch_label();
            let rest =
                Self::combine_flow_lists(exception_label.antecedents(), return_label.antecedents());
            let combined = Self::combine_flow_lists(normal_exit_label.antecedents(), rest);
            finally_label.set_antecedents(combined);
            self.current_flow = Some(finally_label);
            self.bind(stmt.finally_block());
            if self.current_flow().flags.get().intersects(FlowFlags::Unreachable) {
                // If the end of the finally block is unreachable, the end of the entire try statement is unreachable.
                self.current_flow = Some(self.unreachable_flow);
            } else {
                // If we have an IIFE return target and return statements in the try or catch blocks, add a control
                // flow that goes back through the finally block and back through only the return statements.
                if self.current_return_target.is_some() && return_label.antecedents().is_some() {
                    let reduce = self.create_reduce_label(
                        finally_label,
                        return_label.antecedents(),
                        self.current_flow(),
                    );
                    Self::add_antecedent(self.current_return_target, reduce);
                }
                // If we have an outer exception target (i.e. a containing try-finally or try-catch-finally), add a
                // control flow that goes back through the finally block and back through each possible exception source.
                if self.current_exception_target.is_some()
                    && exception_label.antecedents().is_some()
                {
                    let reduce = self.create_reduce_label(
                        finally_label,
                        exception_label.antecedents(),
                        self.current_flow(),
                    );
                    Self::add_antecedent(self.current_exception_target, reduce);
                }
                // If the end of the finally block is reachable, but the end of the try and catch blocks are not,
                // convert the current flow to unreachable. For example, 'try { return 1; } finally { ... }' should
                // result in an unreachable current control flow.
                if normal_exit_label.antecedents().is_some() {
                    self.current_flow = Some(self.create_reduce_label(
                        finally_label,
                        normal_exit_label.antecedents(),
                        self.current_flow(),
                    ));
                } else {
                    self.current_flow = Some(self.unreachable_flow);
                }
            }
        } else {
            self.current_flow = Some(self.finish_flow_label(normal_exit_label));
        }
    }

    fn bind_switch_statement(&mut self, node: P<Node>) {
        let stmt = node.as_switch_statement();
        let post_switch_label = self.create_branch_label();
        self.bind(stmt.expression());
        let save_break_target = self.current_break_target;
        let save_pre_switch_case_flow = self.pre_switch_case_flow;
        self.current_break_target = Some(post_switch_label);
        self.pre_switch_case_flow = self.current_flow;
        self.bind(stmt.case_block());
        Self::add_antecedent(Some(post_switch_label), self.current_flow());
        let has_default = stmt
            .case_block()
            .as_case_block()
            .clauses()
            .nodes()
            .iter()
            .any(|c| c.kind() == Kind::DefaultClause);
        if !has_default {
            let clause =
                self.create_flow_switch_clause(self.pre_switch_case_flow.unwrap(), node, 0, 0);
            Self::add_antecedent(Some(post_switch_label), clause);
        }
        self.current_break_target = save_break_target;
        self.pre_switch_case_flow = save_pre_switch_case_flow;
        self.current_flow = Some(self.finish_flow_label(post_switch_label));
    }

    fn bind_case_block(&mut self, node: P<Node>) {
        let switch_statement = node.parent().unwrap();
        let clauses = node.as_case_block().clauses().nodes();
        let is_narrowing_switch = switch_statement.expression().unwrap().kind()
            == Kind::TrueKeyword
            || is_narrowing_expression(switch_statement.expression().unwrap());
        let mut fallthrough_flow: P<FlowNode> = self.unreachable_flow;
        let mut i = 0;
        while i < clauses.len() {
            let clause_start = i;
            while clauses[i].statements().is_empty() && i + 1 < clauses.len() {
                if fallthrough_flow == self.unreachable_flow {
                    self.current_flow = self.pre_switch_case_flow;
                }
                self.bind(clauses[i]);
                i += 1;
            }
            let pre_case_label = self.create_branch_label();
            let mut pre_case_flow = self.pre_switch_case_flow.unwrap();
            if is_narrowing_switch {
                pre_case_flow = self.create_flow_switch_clause(
                    self.pre_switch_case_flow.unwrap(),
                    switch_statement,
                    clause_start,
                    i + 1,
                );
            }
            Self::add_antecedent(Some(pre_case_label), pre_case_flow);
            Self::add_antecedent(Some(pre_case_label), fallthrough_flow);
            self.current_flow = Some(self.finish_flow_label(pre_case_label));
            let clause = clauses[i];
            self.bind(clause);
            fallthrough_flow = self.current_flow();
            if !self.current_flow().flags.get().intersects(FlowFlags::Unreachable)
                && i != clauses.len() - 1
            {
                clause.as_case_or_default_clause().set_fallthrough_flow_node(self.current_flow);
            }
            i += 1;
        }
    }

    fn bind_case_or_default_clause(&mut self, node: P<Node>) {
        let clause = node.as_case_or_default_clause();
        if let Some(expression) = clause.expression() {
            let save_current_flow = self.current_flow;
            self.current_flow = self.pre_switch_case_flow;
            self.bind(expression);
            self.current_flow = save_current_flow;
        }
        self.bind_each(clause.statements().nodes());
    }

    fn bind_expression_statement(&mut self, node: P<Node>) {
        let stmt = node.as_expression_statement();
        self.bind(stmt.expression());
        self.maybe_bind_expression_flow_if_call(stmt.expression());
    }

    fn maybe_bind_expression_flow_if_call(&mut self, node: P<Node>) {
        // A top level or comma expression call expression with a dotted function name and at least one argument
        // is potentially an assertion and is therefore included in the control flow.
        if ast::is_call_expression(node) {
            let expression = node.expression().unwrap();
            if expression.kind() != Kind::SuperKeyword && ast::is_dotted_name(expression) {
                self.current_flow = Some(self.create_flow_call(self.current_flow(), node));
            }
        }
    }

    fn bind_labeled_statement(&mut self, node: P<Node>) {
        let stmt = node.as_labeled_statement();
        let post_statement_label = self.create_branch_label();
        self.active_label_list.push(ActiveLabel {
            name: stmt.label().text(),
            break_target: Some(post_statement_label),
            continue_target: None,
            referenced: false,
        });
        self.bind(stmt.label());
        self.bind(stmt.statement());
        if !self.active_label_list.last().unwrap().referenced {
            // Mark the label as unused; the checker will decide whether to report it
            stmt.label().set_flags(stmt.label().flags() | NodeFlags::Unreachable);
        }
        self.active_label_list.pop();
        Self::add_antecedent(Some(post_statement_label), self.current_flow());
        self.current_flow = Some(self.finish_flow_label(post_statement_label));
    }

    fn bind_prefix_unary_expression_flow(&mut self, node: P<Node>) {
        let expr = node.as_prefix_unary_expression();
        if expr.operator() == Kind::ExclamationToken {
            std::mem::swap(&mut self.current_true_target, &mut self.current_false_target);
            self.bind_each_child(node);
            std::mem::swap(&mut self.current_true_target, &mut self.current_false_target);
        } else {
            self.bind_each_child(node);
            if expr.operator() == Kind::PlusPlusToken || expr.operator() == Kind::MinusMinusToken {
                self.bind_assignment_target_flow(expr.operand());
            }
        }
    }

    fn bind_postfix_unary_expression_flow(&mut self, node: P<Node>) {
        let expr = node.as_postfix_unary_expression();
        self.bind_each_child(node);
        if expr.operator() == Kind::PlusPlusToken || expr.operator() == Kind::MinusMinusToken {
            self.bind_assignment_target_flow(expr.operand());
        }
    }

    fn bind_destructuring_assignment_flow(&mut self, node: P<Node>) {
        let expr = node.as_binary_expression();
        if self.in_assignment_pattern {
            self.in_assignment_pattern = false;
            self.bind(expr.operator_token());
            self.bind(expr.right());
            self.in_assignment_pattern = true;
            self.bind(expr.left());
            self.bind(expr.type_());
        } else {
            self.in_assignment_pattern = true;
            self.bind(expr.left());
            self.bind(expr.type_());
            self.in_assignment_pattern = false;
            self.bind(expr.operator_token());
            self.bind(expr.right());
        }
        self.bind_assignment_target_flow(expr.left());
    }

    fn bind_binary_expression_flow(&mut self, node: P<Node>) {
        let expr = node.as_binary_expression();
        let operator = expr.operator_token().kind();
        if ast::is_logical_or_coalescing_binary_operator(operator)
            || ast::is_logical_or_coalescing_assignment_operator(operator)
        {
            if is_top_level_logical_expression(node) {
                let post_expression_label = self.create_branch_label();
                let save_current_flow = self.current_flow;
                let save_has_flow_effects = self.has_flow_effects;
                self.has_flow_effects = false;
                self.bind_logical_like_expression(
                    node,
                    Some(post_expression_label),
                    Some(post_expression_label),
                );
                if self.has_flow_effects {
                    self.current_flow = Some(self.finish_local_flow_label(post_expression_label));
                } else {
                    self.current_flow = save_current_flow;
                    self.recycle_flow_label(post_expression_label);
                }
                self.has_flow_effects = self.has_flow_effects || save_has_flow_effects;
            } else {
                self.bind_logical_like_expression(
                    node,
                    self.current_true_target,
                    self.current_false_target,
                );
            }
        } else {
            self.bind(expr.left());
            self.bind(expr.type_());
            if operator == Kind::CommaToken {
                self.maybe_bind_expression_flow_if_call(expr.left());
            }
            self.bind(expr.operator_token());
            self.bind(expr.right());
            if operator == Kind::CommaToken {
                self.maybe_bind_expression_flow_if_call(expr.right());
            }
            if ast::is_assignment_operator(operator) && !ast::is_assignment_target(node) {
                self.bind_assignment_target_flow(expr.left());
                if operator == Kind::EqualsToken
                    && expr.left().kind() == Kind::ElementAccessExpression
                {
                    let element_access = expr.left().as_element_access_expression();
                    if is_narrowable_operand(element_access.expression()) {
                        self.current_flow = Some(self.create_flow_mutation(
                            FlowFlags::ArrayMutation,
                            self.current_flow(),
                            node,
                        ));
                    }
                }
            }
        }
    }

    fn bind_logical_like_expression(
        &mut self,
        node: P<Node>,
        true_target: Option<P<FlowNode>>,
        false_target: Option<P<FlowNode>>,
    ) {
        let expr = node.as_binary_expression();
        let pre_right_label = self.create_branch_label();
        if expr.operator_token().kind() == Kind::AmpersandAmpersandToken
            || expr.operator_token().kind() == Kind::AmpersandAmpersandEqualsToken
        {
            self.bind_condition(Some(expr.left()), Some(pre_right_label), false_target);
        } else {
            self.bind_condition(Some(expr.left()), true_target, Some(pre_right_label));
        }
        self.current_flow = Some(self.finish_local_flow_label(pre_right_label));
        self.bind(expr.operator_token());
        if ast::is_logical_or_coalescing_assignment_operator(expr.operator_token().kind()) {
            self.do_with_conditional_branches(
                Binder::bind,
                Some(expr.right()),
                true_target,
                false_target,
            );
            self.bind_assignment_target_flow(expr.left());
            let true_condition = self.create_flow_condition(
                FlowFlags::TrueCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(true_target, true_condition);
            let false_condition = self.create_flow_condition(
                FlowFlags::FalseCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(false_target, false_condition);
        } else {
            self.bind_condition(Some(expr.right()), true_target, false_target);
        }
    }

    fn bind_delete_expression_flow(&mut self, node: P<Node>) {
        let expr = node.as_delete_expression();
        self.bind_each_child(node);
        if expr.expression().kind() == Kind::PropertyAccessExpression {
            self.bind_assignment_target_flow(expr.expression());
        }
    }

    fn bind_conditional_expression_flow(&mut self, node: P<Node>) {
        let expr = node.as_conditional_expression();
        let true_label = self.create_branch_label();
        let false_label = self.create_branch_label();
        let post_expression_label = self.create_branch_label();
        let save_current_flow = self.current_flow;
        let save_has_flow_effects = self.has_flow_effects;
        self.has_flow_effects = false;
        self.bind_condition(Some(expr.condition()), Some(true_label), Some(false_label));
        self.current_flow = Some(self.finish_local_flow_label(true_label));
        self.bind(expr.question_token());
        self.bind(expr.when_true());
        Self::add_antecedent(Some(post_expression_label), self.current_flow());
        self.current_flow = Some(self.finish_local_flow_label(false_label));
        self.bind(expr.colon_token());
        self.bind(expr.when_false());
        Self::add_antecedent(Some(post_expression_label), self.current_flow());
        if self.has_flow_effects {
            self.current_flow = Some(self.finish_local_flow_label(post_expression_label));
        } else {
            self.current_flow = save_current_flow;
            self.recycle_flow_label(post_expression_label);
        }
        self.has_flow_effects = self.has_flow_effects || save_has_flow_effects;
    }

    fn bind_variable_declaration_flow(&mut self, node: P<Node>) {
        self.bind_each_child(node);
        if node.initializer().is_some()
            || ast::is_for_in_or_of_statement(node.parent().unwrap().parent().unwrap())
        {
            self.bind_initialized_variable_flow(node);
        }
    }

    fn bind_initialized_variable_flow(&mut self, node: P<Node>) {
        let name = match node.kind() {
            Kind::VariableDeclaration | Kind::BindingElement => node.name(),
            _ => None,
        };
        if let Some(name) = name.filter(|&n| ast::is_binding_pattern(n)) {
            for &child in name.elements() {
                self.bind_initialized_variable_flow(child);
            }
        } else {
            self.current_flow =
                Some(self.create_flow_mutation(FlowFlags::Assignment, self.current_flow(), node));
        }
    }

    fn bind_access_expression_flow(&mut self, node: P<Node>) {
        if ast::is_optional_chain(node) {
            self.bind_optional_chain_flow(node);
        } else {
            self.bind_each_child(node);
        }
    }

    fn bind_optional_chain_flow(&mut self, node: P<Node>) {
        if is_top_level_logical_expression(node) {
            let post_expression_label = self.create_branch_label();
            let save_current_flow = self.current_flow;
            let save_has_flow_effects = self.has_flow_effects;
            self.bind_optional_chain(
                node,
                Some(post_expression_label),
                Some(post_expression_label),
            );
            if self.has_flow_effects {
                self.current_flow = Some(self.finish_local_flow_label(post_expression_label));
            } else {
                self.current_flow = save_current_flow;
                self.recycle_flow_label(post_expression_label);
            }
            self.has_flow_effects = self.has_flow_effects || save_has_flow_effects;
        } else {
            self.bind_optional_chain(node, self.current_true_target, self.current_false_target);
        }
    }

    fn bind_optional_chain(
        &mut self,
        node: P<Node>,
        true_target: Option<P<FlowNode>>,
        false_target: Option<P<FlowNode>>,
    ) {
        // For an optional chain, we emulate the behavior of a logical expression:
        //
        // a?.b         -> a && a.b
        // a?.b.c       -> a && a.b.c
        // a?.b?.c      -> a && a.b && a.b.c
        // a?.[x = 1]   -> a && a[x = 1]
        //
        // To do this we descend through the chain until we reach the root of a chain (the expression with a `?.`)
        // and build it's CFA graph as if it were the first condition (`a && ...`). Then we bind the rest
        // of the node as part of the "true" branch, and continue to do so as we ascend back up to the outermost
        // chain node. We then treat the entire node as the right side of the expression.
        let mut pre_chain_label: Option<P<FlowNode>> = None;
        if ast::is_optional_chain_root(node) {
            pre_chain_label = Some(self.create_branch_label());
        }
        self.bind_optional_expression(
            node.expression().unwrap(),
            if pre_chain_label.is_some() { pre_chain_label } else { true_target },
            false_target,
        );
        if let Some(pre_chain_label) = pre_chain_label {
            self.current_flow = Some(self.finish_local_flow_label(pre_chain_label));
        }
        self.do_with_conditional_branches(
            |b, n| b.bind_optional_chain_rest(n.unwrap()),
            Some(node),
            true_target,
            false_target,
        );
        if ast::is_outermost_optional_chain(node) {
            let true_condition = self.create_flow_condition(
                FlowFlags::TrueCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(true_target, true_condition);
            let false_condition = self.create_flow_condition(
                FlowFlags::FalseCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(false_target, false_condition);
        }
    }

    fn bind_optional_expression(
        &mut self,
        node: P<Node>,
        true_target: Option<P<FlowNode>>,
        false_target: Option<P<FlowNode>>,
    ) {
        self.do_with_conditional_branches(Binder::bind, Some(node), true_target, false_target);
        if !ast::is_optional_chain(node) || ast::is_outermost_optional_chain(node) {
            let true_condition = self.create_flow_condition(
                FlowFlags::TrueCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(true_target, true_condition);
            let false_condition = self.create_flow_condition(
                FlowFlags::FalseCondition,
                self.current_flow(),
                Some(node),
            );
            Self::add_antecedent(false_target, false_condition);
        }
    }

    fn bind_optional_chain_rest(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::PropertyAccessExpression => {
                self.bind(node.question_dot_token());
                self.bind(node.name());
            }
            Kind::ElementAccessExpression => {
                self.bind(node.question_dot_token());
                self.bind(node.as_element_access_expression().argument_expression());
            }
            Kind::CallExpression => {
                self.bind(node.question_dot_token());
                self.bind_node_list(node.type_argument_list());
                self.bind_each(node.arguments());
            }
            _ => {}
        }
        false
    }

    fn bind_call_expression_flow(&mut self, node: P<Node>) {
        let call = node.as_call_expression();
        if ast::is_optional_chain(node) {
            self.bind_optional_chain_flow(node);
        } else {
            // If the target of the call expression is a function expression or arrow function we have
            // an immediately invoked function expression (IIFE). Initialize the flowNode property to
            // the current control flow (which includes evaluation of the IIFE arguments).
            let expr = ast::skip_parentheses(call.expression());
            if expr.kind() == Kind::FunctionExpression || expr.kind() == Kind::ArrowFunction {
                self.bind_node_list(call.type_arguments());
                self.bind_each(call.arguments().nodes());
                self.bind(call.expression());
            } else {
                self.bind_each_child(node);
                if call.expression().kind() == Kind::SuperKeyword {
                    self.current_flow = Some(self.create_flow_call(self.current_flow(), node));
                }
            }
        }
        if ast::is_property_access_expression(call.expression()) {
            let access = call.expression().as_property_access_expression();
            let access_name = call.expression().name().unwrap();
            if ast::is_identifier(access_name)
                && is_narrowable_operand(access.expression())
                && ast::is_push_or_unshift_identifier(access_name)
            {
                self.current_flow = Some(self.create_flow_mutation(
                    FlowFlags::ArrayMutation,
                    self.current_flow(),
                    node,
                ));
            }
        }
    }

    fn bind_non_null_expression_flow(&mut self, node: P<Node>) {
        if ast::is_optional_chain(node) {
            self.bind_optional_chain_flow(node);
        } else {
            self.bind_each_child(node);
        }
    }

    fn bind_binding_element_flow(&mut self, node: P<Node>) {
        // When evaluating a binding pattern, the initializer is evaluated before the binding pattern, per:
        // - https://tc39.es/ecma262/#sec-destructuring-binding-patterns-runtime-semantics-iteratorbindinginitialization
        //   - `BindingElement: BindingPattern Initializer?`
        // - https://tc39.es/ecma262/#sec-runtime-semantics-keyedbindinginitialization
        //   - `BindingElement: BindingPattern Initializer?`
        let elem = node.as_binding_element();
        self.bind(elem.dot_dot_dot_token());
        self.bind(elem.property_name());
        self.bind_initializer(elem.initializer());
        self.bind(node.name());
    }

    fn bind_parameter_flow(&mut self, node: P<Node>) {
        let param = node.as_parameter_declaration();
        self.bind_modifiers(node.modifiers());
        self.bind(param.dot_dot_dot_token());
        self.bind(param.question_token());
        self.bind(param.type_());
        self.bind_initializer(param.initializer());
        self.bind(node.name());
    }

    // a BindingElement/Parameter does not have side effects if initializers are not evaluated and used. (see GH#49759)
    fn bind_initializer(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        let entry_flow = self.current_flow;
        self.bind(node);
        if entry_flow == Some(self.unreachable_flow) || entry_flow == self.current_flow {
            return;
        }
        let exit_flow = self.create_branch_label();
        Self::add_antecedent(Some(exit_flow), entry_flow.unwrap());
        Self::add_antecedent(Some(exit_flow), self.current_flow());
        self.current_flow = Some(self.finish_flow_label(exit_flow));
    }
}

fn set_flow_node(node: P<Node>, flow_node: Option<P<FlowNode>>) {
    if node.has_flow_node_data() {
        node.set_flow_node(flow_node);
    }
}

fn set_return_flow_node(node: P<Node>, return_flow_node: Option<P<FlowNode>>) {
    match node.kind() {
        Kind::Constructor => {
            node.as_constructor_declaration().set_return_flow_node(return_flow_node);
        }
        Kind::FunctionDeclaration => {
            node.as_function_declaration().set_return_flow_node(return_flow_node);
        }
        Kind::FunctionExpression => {
            node.as_function_expression().set_return_flow_node(return_flow_node);
        }
        Kind::ClassStaticBlockDeclaration => {
            node.as_class_static_block_declaration().set_return_flow_node(return_flow_node);
        }
        _ => {}
    }
}

fn is_generator_function_expression(node: P<Node>) -> bool {
    ast::is_function_expression(node) && node.body_data().unwrap().asterisk_token.is_some()
}

impl Binder {
    fn add_to_container_chain(&mut self, next: P<Node>) {
        if let Some(last_container) = self.last_container {
            last_container.locals_container_data().unwrap().next_container.set(Some(next));
        }
        self.last_container = Some(next);
    }

    fn add_declaration_to_symbol(
        &mut self,
        symbol: P<Symbol>,
        node: P<Node>,
        symbol_flags: SymbolFlags,
    ) {
        // tsrs-only (`bind_lazy_member_list`): a declaration that fills a table the deferred list fills merges into the
        // symbol: an interface or class fills `members` (type parameters, members), a namespace or enum fills
        // `exports`, which only a class's list fills too (static members). Value and function merges fill neither.
        if let Some(lazy) = symbol.lazy_list()
            && lazy.state() == lazylist::DEFERRED
            && match node.kind() {
                Kind::InterfaceDeclaration | Kind::ClassDeclaration | Kind::ClassExpression => true,
                Kind::ModuleDeclaration | Kind::EnumDeclaration => lazy.fills_exports(),
                _ => false,
            }
        {
            self.force_pending_lazy_list(lazy.get());
        }
        symbol.flags.set(symbol.flags.get() | symbol_flags);
        node.declaration_data().unwrap().symbol.set(Some(symbol));
        if !symbol.declarations().contains(&node) {
            symbol.append_declarations(&[node]);
        }
        // On merge of const enum module with class or function, reset const enum only flag (namespaces will already recalculate)
        if symbol.flags.get().intersects(SymbolFlags::ConstEnumOnlyModule)
            && symbol
                .flags
                .get()
                .intersects(SymbolFlags::Function | SymbolFlags::Class | SymbolFlags::RegularEnum)
        {
            symbol.flags.set(symbol.flags.get() & !SymbolFlags::ConstEnumOnlyModule);
            self.not_const_enum_only_modules.insert(symbol);
        }
        if symbol_flags.intersects(SymbolFlags::Value) {
            set_value_declaration(symbol, node);
        }
    }
}

pub fn set_value_declaration(symbol: P<Symbol>, node: P<Node>) {
    let value_declaration = symbol.value_declaration();
    let replace = match value_declaration {
        None => true,
        Some(value_declaration) => {
            is_assignment_declaration(value_declaration) && !is_assignment_declaration(node)
                || value_declaration.kind() != node.kind()
                    && is_effective_module_declaration(value_declaration)
        }
    };
    if replace {
        // Non-assignment declarations take precedence over assignment declarations and
        // non-namespace declarations take precedence over namespace declarations.
        symbol.set_value_declaration(Some(node));
    }
}

#[expect(
    clippy::inline_always,
    reason = "Measured fewer binder instructions by inlining container classification"
)]
#[inline(always)]
pub fn get_container_flags(node: P<Node>) -> ContainerFlags {
    match node.kind() {
        Kind::ClassExpression
        | Kind::ClassDeclaration
        | Kind::EnumDeclaration
        | Kind::ObjectLiteralExpression
        | Kind::TypeLiteral
        | Kind::JsxAttributes => ContainerFlags::IsContainer,
        Kind::InterfaceDeclaration => ContainerFlags::IsContainer | ContainerFlags::IsInterface,
        Kind::ModuleDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::MappedType
        | Kind::IndexSignature => ContainerFlags::IsContainer | ContainerFlags::HasLocals,
        Kind::SourceFile => {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
        }
        Kind::GetAccessor | Kind::SetAccessor | Kind::MethodDeclaration
            if ast::is_object_literal_or_class_expression_method_or_accessor(node) =>
        {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
                | ContainerFlags::IsFunctionLike
                | ContainerFlags::IsObjectLiteralOrClassExpressionMethodOrAccessor
                | ContainerFlags::IsThisContainer
        }
        Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::MethodDeclaration
        | Kind::Constructor
        | Kind::FunctionDeclaration
        | Kind::ClassStaticBlockDeclaration => {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
                | ContainerFlags::IsFunctionLike
                | ContainerFlags::IsThisContainer
        }
        Kind::MethodSignature
        | Kind::CallSignature
        | Kind::FunctionType
        | Kind::ConstructSignature
        | Kind::ConstructorType => {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
                | ContainerFlags::IsFunctionLike
                | ContainerFlags::PropagatesThisKeyword
        }
        Kind::FunctionExpression => {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
                | ContainerFlags::IsFunctionLike
                | ContainerFlags::IsFunctionExpression
                | ContainerFlags::IsThisContainer
        }
        Kind::ArrowFunction => {
            ContainerFlags::IsContainer
                | ContainerFlags::IsControlFlowContainer
                | ContainerFlags::HasLocals
                | ContainerFlags::IsFunctionLike
                | ContainerFlags::IsFunctionExpression
                | ContainerFlags::PropagatesThisKeyword
        }
        Kind::ModuleBlock => ContainerFlags::IsControlFlowContainer,
        Kind::PropertyDeclaration => {
            if node.initializer().is_some() {
                ContainerFlags::IsControlFlowContainer | ContainerFlags::IsThisContainer
            } else {
                ContainerFlags::None
            }
        }
        Kind::CatchClause
        | Kind::ForStatement
        | Kind::ForInStatement
        | Kind::ForOfStatement
        | Kind::CaseBlock => ContainerFlags::IsBlockScopedContainer | ContainerFlags::HasLocals,
        Kind::Block => {
            let parent = node.parent().unwrap();
            if ast::is_function_like(Some(parent)) || ast::is_class_static_block_declaration(parent)
            {
                ContainerFlags::None
            } else {
                ContainerFlags::IsBlockScopedContainer | ContainerFlags::HasLocals
            }
        }
        _ => ContainerFlags::None,
    }
}

fn is_narrowing_expression(expr: P<Node>) -> bool {
    match expr.kind() {
        Kind::Identifier | Kind::ThisKeyword => true,
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
            contains_narrowable_reference(expr)
        }
        Kind::CallExpression => has_narrowable_argument(expr),
        Kind::ParenthesizedExpression | Kind::NonNullExpression | Kind::TypeOfExpression => {
            is_narrowing_expression(expr.expression().unwrap())
        }
        Kind::BinaryExpression => is_narrowing_binary_expression(expr),
        Kind::PrefixUnaryExpression => {
            let prefix = expr.as_prefix_unary_expression();
            prefix.operator() == Kind::ExclamationToken && is_narrowing_expression(prefix.operand())
        }
        _ => false,
    }
}

fn contains_narrowable_reference(expr: P<Node>) -> bool {
    if is_narrowable_reference(expr) {
        return true;
    }
    if expr.flags().intersects(NodeFlags::OptionalChain) {
        match expr.kind() {
            Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::CallExpression
            | Kind::NonNullExpression => {
                return contains_narrowable_reference(expr.expression().unwrap());
            }
            _ => {}
        }
    }
    false
}

fn is_narrowable_reference(node: P<Node>) -> bool {
    match node.kind() {
        Kind::Identifier | Kind::ThisKeyword | Kind::SuperKeyword | Kind::MetaProperty => true,
        Kind::PropertyAccessExpression
        | Kind::ParenthesizedExpression
        | Kind::NonNullExpression => is_narrowable_reference(node.expression().unwrap()),
        Kind::ElementAccessExpression => {
            let expr = node.as_element_access_expression();
            ast::is_string_or_numeric_literal_like(expr.argument_expression())
                || ast::is_entity_name_expression(expr.argument_expression())
                    && is_narrowable_reference(expr.expression())
        }
        Kind::BinaryExpression => {
            let expr = node.as_binary_expression();
            expr.operator_token().kind() == Kind::CommaToken
                && is_narrowable_reference(expr.right())
                || ast::is_assignment_operator(expr.operator_token().kind())
                    && ast::is_left_hand_side_expression(expr.left())
        }
        _ => false,
    }
}

fn has_narrowable_argument(expr: P<Node>) -> bool {
    let call = expr.as_call_expression();
    if call.arguments().nodes().iter().any(|&argument| contains_narrowable_reference(argument)) {
        return true;
    }
    if ast::is_property_access_expression(call.expression())
        && contains_narrowable_reference(call.expression().expression().unwrap())
    {
        return true;
    }
    false
}

fn is_narrowing_binary_expression(node: P<Node>) -> bool {
    let expr = node.as_binary_expression();
    match expr.operator_token().kind() {
        Kind::EqualsToken
        | Kind::BarBarEqualsToken
        | Kind::AmpersandAmpersandEqualsToken
        | Kind::QuestionQuestionEqualsToken => contains_narrowable_reference(expr.left()),
        Kind::EqualsEqualsToken
        | Kind::ExclamationEqualsToken
        | Kind::EqualsEqualsEqualsToken
        | Kind::ExclamationEqualsEqualsToken => {
            let left = ast::skip_parentheses(expr.left());
            let right = ast::skip_parentheses(expr.right());
            is_narrowable_operand(left)
                || is_narrowable_operand(right)
                || is_narrowing_type_of_operands(right, left)
                || is_narrowing_type_of_operands(left, right)
                || (ast::is_boolean_literal(right) && is_narrowing_expression(left)
                    || ast::is_boolean_literal(left) && is_narrowing_expression(right))
        }
        Kind::InstanceOfKeyword => is_narrowable_operand(expr.left()),
        Kind::InKeyword | Kind::CommaToken => is_narrowing_expression(expr.right()),
        _ => false,
    }
}

fn is_narrowable_operand(expr: P<Node>) -> bool {
    match expr.kind() {
        Kind::ParenthesizedExpression => return is_narrowable_operand(expr.expression().unwrap()),
        Kind::BinaryExpression => {
            let binary = expr.as_binary_expression();
            match binary.operator_token().kind() {
                Kind::EqualsToken => return is_narrowable_operand(binary.left()),
                Kind::CommaToken => return is_narrowable_operand(binary.right()),
                _ => {}
            }
        }
        _ => {}
    }
    contains_narrowable_reference(expr)
}

fn is_narrowing_type_of_operands(expr1: P<Node>, expr2: P<Node>) -> bool {
    ast::is_type_of_expression(expr1)
        && is_narrowable_operand(expr1.expression().unwrap())
        && ast::is_string_literal_like(expr2)
}

impl Binder {
    fn error_on_node(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn std::fmt::Display],
    ) {
        let diagnostic = self.create_diagnostic_for_node(node, message, args);
        self.add_diagnostic(diagnostic);
    }

    fn error_on_first_token(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn std::fmt::Display],
    ) {
        let span = scanner::get_range_of_token_at_position(
            self.file,
            node.pos().expect_source("parsed nodes have source positions"),
        );
        self.add_diagnostic(ast::new_diagnostic(Some(self.file), span, message, args));
    }

    // Inside the binder, we may create a diagnostic for an as-yet unbound node (with potentially no parent pointers, implying no accessible source file)
    // If so, the node _must_ be in the current file (as that's the only way anything could have traversed to it to yield it as the error node)
    // This version of `createDiagnosticForNode` uses the binder's context to account for this, and always yields correct diagnostics even in these situations.
    fn create_diagnostic_for_node(
        &self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn std::fmt::Display],
    ) -> P<Diagnostic> {
        ast::new_diagnostic(
            Some(self.file),
            scanner::get_error_range_for_node(self.file, node),
            message,
            args,
        )
    }

    fn add_diagnostic(&mut self, diagnostic: P<Diagnostic>) {
        self.bind_diagnostics.push(diagnostic);
    }
}

fn get_optional_symbol_flag_for_node(node: P<Node>) -> SymbolFlags {
    match node.postfix_token() {
        Some(postfix_token) if postfix_token.kind() == Kind::QuestionToken => SymbolFlags::Optional,
        _ => SymbolFlags::None,
    }
}

fn is_statement_condition(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::IfStatement | Kind::WhileStatement | Kind::DoStatement => {
            parent.expression() == Some(node)
        }
        Kind::ForStatement => parent.as_for_statement().condition() == Some(node),
        Kind::ConditionalExpression => parent.as_conditional_expression().condition() == node,
        _ => false,
    }
}

fn is_top_level_logical_expression(node: P<Node>) -> bool {
    let mut node = node;
    loop {
        let parent = node.parent().unwrap();
        if ast::is_parenthesized_expression(parent)
            || ast::is_prefix_unary_expression(parent)
                && parent.as_prefix_unary_expression().operator() == Kind::ExclamationToken
        {
            node = parent;
        } else {
            break;
        }
    }
    let parent = node.parent().unwrap();
    !(is_statement_condition(node)
        || ast::is_logical_expression(parent)
        || (ast::is_optional_chain(parent) && parent.expression() == Some(node)))
}

fn is_assignment_declaration(decl: P<Node>) -> bool {
    ast::is_binary_expression(decl)
        || ast::is_access_expression(decl)
        || ast::is_identifier(decl)
        || ast::is_call_expression(decl)
}

fn is_effective_module_declaration(node: P<Node>) -> bool {
    ast::is_module_declaration(node) || ast::is_identifier(node)
}
