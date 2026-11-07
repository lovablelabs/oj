// NameResolver API (for the checker agents)
//
// Go's `NameResolver` is a struct of function-valued fields (nil-able hooks) that the checker fills
// with bound methods (`c.getSymbol`, `c.error`, ...). Rust port: `NameResolver<H>` is generic over
// the host type `H` (the checker) and every hook is an `Option<fn(&mut H, ...) -> ...>` plain function
// pointer that receives the host back as its first argument (PORTING.md callback rule). A nil Go hook is
// `None`. Non-capturing closures coerce to these fn pointers, so adapters are cheap:
//
//     NameResolver::<Checker> {
//         compiler_options: c.compiler_options,
//         get_symbol_of_declaration: Some(|c, node| c.get_symbol_of_declaration(node)),
//         error: Some(|c, location, message, args| c.error(location, message, args)),
//         globals: c.globals,
//         arguments_symbol: Cell::new(Some(c.arguments_symbol)),
//         require_symbol: Some(c.require_symbol),
//         lookup: Some(|c, symbols, name, meaning| c.get_symbol(symbols, name, meaning)),
//         ..NameResolver::new(c.compiler_options, c.globals)
//     }
//
// `resolve` takes `&self` plus `host: &mut H`. The only state Go mutates on the resolver is the lazily
// synthesized `ArgumentsSymbol`, which is a `Cell` here. Go stores `c.createNameResolver().Resolve` in the
// checker; in Rust allocate the resolver once (`P::new(resolver)`), keep the `P<NameResolver<Checker>>` in
// the checker, and call it as `let r = self.name_resolver; r.resolve(self, location, name, ...)` so the
// resolver is not borrowed from `self` while `self` is passed mutably.

use std::cell::Cell;
use std::fmt::Display;

use tsrs_ast as ast;
use tsrs_ast::{
    Diagnostic, Kind, ModifierFlags, Node, NodeFlags, Symbol, SymbolFlags, SymbolTable,
};
use tsrs_core::{CompilerOptions, P, ScriptTarget, Tristate};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

pub struct NameResolver<H: 'static> {
    pub compiler_options: P<CompilerOptions>,
    pub get_symbol_of_declaration: Option<fn(&mut H, P<Node>) -> Option<P<Symbol>>>,
    pub error:
        Option<fn(&mut H, Option<P<Node>>, &'static Message, &[&dyn Display]) -> P<Diagnostic>>,
    pub globals: Option<P<SymbolTable>>,
    pub arguments_symbol: Cell<Option<P<Symbol>>>,
    pub require_symbol: Option<P<Symbol>>,
    pub lookup: Option<fn(&mut H, Option<P<SymbolTable>>, &str, SymbolFlags) -> Option<P<Symbol>>>,
    pub symbol_referenced: Option<fn(&mut H, P<Symbol>, SymbolFlags)>,
    pub set_requires_scope_change_cache: Option<fn(&mut H, P<Node>, Tristate)>,
    pub get_requires_scope_change_cache: Option<fn(&mut H, P<Node>) -> Tristate>,
    pub on_property_with_invalid_initializer:
        Option<fn(&mut H, Option<P<Node>>, &str, P<Node>, Option<P<Symbol>>) -> bool>,
    pub on_failed_to_resolve_symbol:
        Option<fn(&mut H, Option<P<Node>>, &str, SymbolFlags, &'static Message)>,
    pub on_successfully_resolved_symbol: Option<
        fn(&mut H, Option<P<Node>>, P<Symbol>, SymbolFlags, Option<P<Node>>, Option<P<Node>>, bool),
    >,
}

impl<H: 'static> NameResolver<H> {
    // All hooks nil, like a Go `&binder.NameResolver{CompilerOptions: ..., Globals: ...}` literal.
    pub fn new(
        compiler_options: P<CompilerOptions>,
        globals: Option<P<SymbolTable>>,
    ) -> NameResolver<H> {
        NameResolver {
            compiler_options,
            get_symbol_of_declaration: None,
            error: None,
            globals,
            arguments_symbol: Cell::new(None),
            require_symbol: None,
            lookup: None,
            symbol_referenced: None,
            set_requires_scope_change_cache: None,
            get_requires_scope_change_cache: None,
            on_property_with_invalid_initializer: None,
            on_failed_to_resolve_symbol: None,
            on_successfully_resolved_symbol: None,
        }
    }

    pub fn resolve(
        &self,
        host: &mut H,
        location: Option<P<Node>>,
        name: &str,
        meaning: SymbolFlags,
        name_not_found_message: Option<&'static Message>,
        is_use: bool,
        exclude_globals: bool,
    ) -> Option<P<Symbol>> {
        let mut location = location;
        let mut result: Option<P<Symbol>> = None;
        let mut last_location: Option<P<Node>> = None;
        let mut last_self_reference_location: Option<P<Node>> = None;
        let mut property_with_invalid_initializer: Option<P<Node>> = None;
        let mut associated_declaration_for_containing_initializer_or_binding_name: Option<P<Node>> =
            None;
        let mut within_deferred_context = false;
        let original_location = location; // needed for did-you-mean error reporting, which gathers candidates starting from the original location
        let name_is_const = name == "const";
        'loop_: while let Some(mut loc) = location {
            if is_pass_through_scope_kind(loc.kind()) {
                debug_assert!(loc.locals_container_data().is_none());
                last_location = Some(loc);
                location = loc.parent();
                continue;
            }
            if name_is_const && ast::is_const_assertion(loc) {
                // `const` in an `as const` has no symbol, but issues no error because there is no *actual* lookup of the type
                // (it refers to the constant type of the expression instead)
                return None;
            }
            if ast::is_module_or_enum_declaration(loc)
                && last_location.is_some()
                && loc.name() == last_location
            {
                // If lastLocation is the name of a namespace or enum, skip the parent since it will have is own locals that could
                // conflict.
                last_location = Some(loc);
                loc = loc.parent().unwrap();
            }
            let is_module_attributes = ast::is_module_declaration(loc)
                && loc.as_module_declaration().attributes().is_some()
                && last_location == loc.as_module_declaration().attributes();
            let locals = loc.locals();
            // Locals of a source file are not in scope (because they get merged into the global symbol table)
            if locals.is_some() && !ast::is_global_source_file(loc) {
                result = self.lookup(host, locals, name, meaning);
                if let Some(res) = result {
                    let mut use_result = true;
                    if is_module_attributes {
                        use_result = false;
                    } else if ast::is_function_like(Some(loc))
                        && last_location.is_some()
                        && last_location != loc.body()
                    {
                        let last = last_location.unwrap();
                        // symbol lookup restrictions for function-like declarations
                        // - Type parameters of a function are in scope in the entire function declaration, including the parameter
                        //   list and return type. However, local types are only in scope in the function body.
                        // - parameters are only in the scope of function body
                        // This restriction does not apply to JSDoc comment types because they are parented
                        // at a higher level than type parameters would normally be
                        if (meaning & res.flags.get()).intersects(SymbolFlags::Type)
                            && last.kind() != Kind::JSDoc
                        {
                            // type parameters are visible in parameter list, return type and type parameter list.
                            // Synthetic fake scopes are added for signatures so type parameters are accessible from them.
                            use_result = res.flags.get().intersects(SymbolFlags::TypeParameter)
                                && (last.flags().intersects(NodeFlags::Synthesized)
                                    || last_location == loc.type_node()
                                    || last.kind() == Kind::Parameter
                                    || last.kind() == Kind::JSDocParameterTag
                                    || last.kind() == Kind::JSDocReturnTag
                                    || last.kind() == Kind::TypeParameter);
                        }
                        if (meaning & res.flags.get()).intersects(SymbolFlags::Variable) {
                            // expression inside parameter will lookup as normal variable scope when targeting es2015+
                            if self.use_outer_variable_scope_in_parameter(host, res, loc, last) {
                                use_result = false;
                            } else if res
                                .flags
                                .get()
                                .intersects(SymbolFlags::FunctionScopedVariable)
                            {
                                // parameters are visible only inside function body, parameter list and return type
                                // technically for parameter list case here we might mix parameters and variables declared in function,
                                // however it is detected separately when checking initializers of parameters
                                // to make sure that they reference no variables declared after them.
                                use_result = last.kind() == Kind::Parameter
                                    || last.flags().intersects(NodeFlags::Synthesized)
                                    || last_location == loc.type_node()
                                        && ast::find_ancestor(
                                            res.value_declaration(),
                                            ast::is_parameter_declaration,
                                        )
                                        .is_some();
                            }
                        }
                    } else if loc.kind() == Kind::ConditionalType {
                        // A type parameter declared using 'infer T' in a conditional type is visible only in
                        // the true branch of the conditional type.
                        use_result =
                            last_location == Some(loc.as_conditional_type_node().true_type());
                    }
                    if use_result {
                        break 'loop_;
                    }
                    result = None;
                }
            }
            within_deferred_context =
                within_deferred_context || get_is_deferred_context(loc, last_location);
            'switch: {
                match loc.kind() {
                    Kind::SourceFile | Kind::ModuleDeclaration => {
                        if loc.kind() == Kind::SourceFile
                            && !ast::is_external_or_common_js_module(loc.as_source_file().as_p())
                        {
                            break 'switch;
                        }
                        if is_module_attributes {
                            break 'switch;
                        }
                        let Some(module_symbol) = self.get_symbol_of_declaration(host, loc) else {
                            break 'switch;
                        };
                        let module_exports = module_symbol.exports();
                        if ast::is_source_file(loc)
                            || (ast::is_module_declaration(loc)
                                && loc.flags().intersects(NodeFlags::Ambient)
                                && !ast::is_global_scope_augmentation(loc))
                        {
                            // It's an external module. First see if the module has an export default and if the local
                            // name of that export default matches.
                            result = module_exports
                                .and_then(|e| (*e).get(ast::INTERNAL_SYMBOL_NAME_DEFAULT));
                            if let Some(res) = result {
                                let local_symbol = get_local_symbol_for_export_default(res);
                                if let Some(local_symbol) = local_symbol
                                    && res.flags.get().intersects(meaning)
                                    && local_symbol.name() == name
                                {
                                    break 'loop_;
                                }
                                result = None;
                            }
                            // Because of module/namespace merging, a module's exports are in scope,
                            // yet we never want to treat an export specifier as putting a member in scope.
                            // Therefore, if the name we find is purely an export specifier, it is not actually considered in scope.
                            // Two things to note about this:
                            //     1. We have to check this without calling getSymbol. The problem with calling getSymbol
                            //        on an export specifier is that it might find the export specifier itself, and try to
                            //        resolve it as an alias. This will cause the checker to consider the export specifier
                            //        a circular alias reference when it might not be.
                            //     2. We check === SymbolFlags.Alias in order to check that the symbol is *purely*
                            //        an alias. If we used &, we'd be throwing out symbols that have non alias aspects,
                            //        which is not the desired behavior.
                            let module_export = module_exports.and_then(|e| (*e).get(name));
                            if let Some(module_export) = module_export
                                && module_export.flags.get() == SymbolFlags::Alias
                                && (ast::get_declaration_of_kind(
                                    module_export,
                                    Kind::ExportSpecifier,
                                )
                                .is_some()
                                    || ast::get_declaration_of_kind(
                                        module_export,
                                        Kind::NamespaceExport,
                                    )
                                    .is_some())
                            {
                                break 'switch;
                            }
                        }
                        if name != ast::INTERNAL_SYMBOL_NAME_DEFAULT {
                            result = self.lookup(
                                host,
                                module_exports,
                                name,
                                meaning & SymbolFlags::ModuleMember,
                            );
                            if let Some(res) = result {
                                if ast::is_source_file(loc)
                                    && loc
                                        .as_source_file()
                                        .common_js_module_indicator
                                        .get()
                                        .is_some()
                                    && !res.flags.get().intersects(SymbolFlags::Type)
                                {
                                    result = None;
                                } else {
                                    break 'loop_;
                                }
                            }
                        }
                    }
                    Kind::EnumDeclaration => {
                        let Some(enum_symbol) = self.get_symbol_of_declaration(host, loc) else {
                            break 'switch;
                        };
                        result = self.lookup(
                            host,
                            enum_symbol.exports(),
                            name,
                            meaning & SymbolFlags::EnumMember,
                        );
                        if let Some(res) = result {
                            if name_not_found_message.is_some()
                                && self.compiler_options.get_isolated_modules()
                                && !loc.flags().intersects(NodeFlags::Ambient)
                                && ast::get_source_file_of_node(Some(loc))
                                    != ast::get_source_file_of_node(res.value_declaration())
                            {
                                let isolated_modules_like_flag_name =
                                    if self.compiler_options.verbatim_module_syntax
                                        == Tristate::True
                                    {
                                        "verbatimModuleSyntax"
                                    } else {
                                        "isolatedModules"
                                    };
                                let qualified = format!("{}.{}", enum_symbol.name(), name);
                                self.error(
                                    host,
                                    original_location,
                                    &diagnostics::CANNOT_ACCESS_0_FROM_ANOTHER_FILE_WITHOUT_QUALIFICATION_WHEN_1_IS_ENABLED_USE_2_INSTEAD,
                                    &[&name, &isolated_modules_like_flag_name, &qualified],
                                );
                            }
                            break 'loop_;
                        }
                    }
                    Kind::PropertyDeclaration => {
                        if !ast::is_static(loc) {
                            let ctor = ast::find_constructor_declaration(loc.parent().unwrap());
                            if let Some(ctor) = ctor
                                && ctor.locals().is_some()
                                && self
                                    .lookup(host, ctor.locals(), name, meaning & SymbolFlags::Value)
                                    .is_some()
                            {
                                // Remember the property node, it will be used later to report appropriate error
                                property_with_invalid_initializer = Some(loc);
                            }
                        }
                    }
                    Kind::ClassDeclaration | Kind::ClassExpression | Kind::InterfaceDeclaration => {
                        let members = self.get_symbol_of_declaration(host, loc).unwrap().members();
                        result = self.lookup(host, members, name, meaning & SymbolFlags::Type);
                        if let Some(res) = result {
                            if !is_type_parameter_symbol_declared_in_container(res, loc) {
                                // ignore type parameters not declared in this container
                                result = None;
                                break 'switch;
                            }
                            if let Some(last) = last_location
                                && ast::is_static(last)
                            {
                                // TypeScript 1.0 spec (April 2014): 3.4.1
                                // The scope of a type parameter extends over the entire declaration with which the type
                                // parameter list is associated, with the exception of static member declarations in classes.
                                if name_not_found_message.is_some() {
                                    self.error(host, original_location, &diagnostics::STATIC_MEMBERS_CANNOT_REFERENCE_CLASS_TYPE_PARAMETERS, &[]);
                                }
                                return None;
                            }
                            break 'loop_;
                        }
                        if ast::is_class_expression(loc)
                            && meaning.intersects(SymbolFlags::Class)
                            && let Some(class_name) = loc.name()
                            && name == class_name.text()
                        {
                            result = loc.symbol();
                            break 'loop_;
                        }
                    }
                    Kind::ExpressionWithTypeArguments => {
                        let parent = loc.parent().unwrap();
                        if last_location == loc.expression()
                            && ast::is_heritage_clause(parent)
                            && parent.as_heritage_clause().token() == Kind::ExtendsKeyword
                        {
                            let container = parent.parent().unwrap();
                            if ast::is_class_like(container) {
                                let members = self
                                    .get_symbol_of_declaration(host, container)
                                    .unwrap()
                                    .members();
                                result =
                                    self.lookup(host, members, name, meaning & SymbolFlags::Type);
                                if result.is_some() {
                                    if name_not_found_message.is_some() {
                                        self.error(host, original_location, &diagnostics::BASE_CLASS_EXPRESSIONS_CANNOT_REFERENCE_CLASS_TYPE_PARAMETERS, &[]);
                                    }
                                    return None;
                                }
                            }
                        }
                    }
                    // It is not legal to reference a class's own type parameters from a computed property name that
                    // belongs to the class. For example:
                    //
                    //   function foo<T>() { return '' }
                    //   class C<T> { // <-- Class's own type parameter T
                    //       [foo<T>()]() { } // <-- Reference to T from class's own computed property
                    //   }
                    Kind::ComputedPropertyName => {
                        let grandparent = loc.parent().unwrap().parent();
                        let gp = grandparent.unwrap();
                        if ast::is_class_like(gp) || ast::is_interface_declaration(gp) {
                            // A reference to this grandparent's type parameters would be an error
                            let members =
                                self.get_symbol_of_declaration(host, gp).unwrap().members();
                            result = self.lookup(host, members, name, meaning & SymbolFlags::Type);
                            if result.is_some() {
                                if name_not_found_message.is_some() {
                                    self.error(
                                        host,
                                        original_location,
                                        &diagnostics::A_COMPUTED_PROPERTY_NAME_CANNOT_REFERENCE_A_TYPE_PARAMETER_FROM_ITS_CONTAINING_TYPE,
                                        &[],
                                    );
                                }
                                return None;
                            }
                        }
                    }
                    Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::FunctionDeclaration => {
                        if meaning.intersects(SymbolFlags::Variable) && name == "arguments" {
                            result = Some(self.arguments_symbol());
                            break 'loop_;
                        }
                    }
                    Kind::FunctionExpression => {
                        if meaning.intersects(SymbolFlags::Variable) && name == "arguments" {
                            result = Some(self.arguments_symbol());
                            break 'loop_;
                        }
                        if meaning.intersects(SymbolFlags::Function)
                            && let Some(function_name) = loc.name()
                            && name == function_name.text()
                        {
                            result = loc.symbol();
                            break 'loop_;
                        }
                    }
                    Kind::Decorator => {
                        // Decorators are resolved at the class declaration. Resolving at the parameter
                        // or member would result in looking up locals in the method.
                        //
                        //   function y() {}
                        //   class C {
                        //       method(@y x, y) {} // <-- decorator y should be resolved at the class declaration, not the parameter.
                        //   }
                        //
                        if let Some(parent) = loc.parent()
                            && parent.kind() == Kind::Parameter
                        {
                            loc = parent;
                        }
                        //   function y() {}
                        //   class C {
                        //       @y method(x, y) {} // <-- decorator y should be resolved at the class declaration, not the method.
                        //   }
                        //
                        // class Decorators are resolved outside of the class to avoid referencing type parameters of that class.
                        //
                        //   type T = number;
                        //   declare function y(x: T): any;
                        //   @param(1 as T) // <-- T should resolve to the type alias outside of class C
                        //   class C<T> {}
                        if let Some(parent) = loc.parent()
                            && (ast::is_class_element(parent)
                                || parent.kind() == Kind::ClassDeclaration)
                        {
                            loc = parent;
                        }
                    }
                    Kind::Parameter => {
                        let parameter_declaration = loc.as_parameter_declaration();
                        if let Some(last) = last_location
                            && (Some(last) == parameter_declaration.initializer()
                                || Some(last) == loc.name() && ast::is_binding_pattern(last))
                            && associated_declaration_for_containing_initializer_or_binding_name
                                .is_none()
                        {
                            associated_declaration_for_containing_initializer_or_binding_name =
                                Some(loc);
                        }
                    }
                    Kind::BindingElement => {
                        let binding_element = loc.as_binding_element();
                        if let Some(last) = last_location
                            && (Some(last) == binding_element.initializer()
                                || Some(last) == loc.name() && ast::is_binding_pattern(last))
                            && ast::is_part_of_parameter_declaration(loc)
                            && associated_declaration_for_containing_initializer_or_binding_name
                                .is_none()
                        {
                            associated_declaration_for_containing_initializer_or_binding_name =
                                Some(loc);
                        }
                    }
                    Kind::InferType => {
                        if meaning.intersects(SymbolFlags::TypeParameter) {
                            let type_parameter = loc.as_infer_type_node().type_parameter();
                            if let Some(parameter_name) = type_parameter.name()
                                && name == parameter_name.text()
                            {
                                result = type_parameter.symbol();
                                break 'loop_;
                            }
                        }
                    }
                    Kind::ExportSpecifier => {
                        let export_specifier = loc.as_export_specifier();
                        if last_location.is_some()
                            && last_location == export_specifier.property_name()
                            && loc.parent().unwrap().parent().unwrap().module_specifier().is_some()
                        {
                            loc = loc.parent().unwrap().parent().unwrap().parent().unwrap();
                        }
                    }
                    _ => {}
                }
            }
            if is_self_reference_location(loc, last_location) {
                last_self_reference_location = Some(loc);
            }
            last_location = Some(loc);
            // !!! In Strada, JSDocTemplateTag/JSDocParameterTag/JSDocReturnTag locations skip to
            // getEffectiveContainerForJSDocTemplateTag/getHostSignatureFromJSDoc instead of location.parent.
            // This is a no-op currently because JSDoc nodes have no locals and getEffectiveJSDocHost is not
            // fully ported for JS assignment patterns.
            location = loc.parent();
        }
        // We just climbed up parents looking for the name, meaning that we started in a descendant node of `lastLocation`.
        // If `result === lastSelfReferenceLocation.symbol`, that means that we are somewhere inside `lastSelfReferenceLocation` looking up a name, and resolving to `lastLocation` itself.
        // That means that this is a self-reference of `lastLocation`, and shouldn't count this when considering whether `lastLocation` is used.
        if is_use
            && let Some(res) = result
            && (last_self_reference_location.is_none()
                || Some(res) != last_self_reference_location.unwrap().symbol())
            && let Some(symbol_referenced) = self.symbol_referenced
        {
            symbol_referenced(host, res, meaning);
        }
        if result.is_none() && !exclude_globals {
            result = self.lookup(host, self.globals, name, meaning | SymbolFlags::GlobalLookup);
        }
        if result.is_none()
            && let Some(original) = original_location
            && ast::is_in_js_file(Some(original))
            && let Some(parent) = original.parent()
            && ast::is_require_call(parent, false /*requireStringLiteralLikeArgument*/)
        {
            return self.require_symbol;
        }
        if let Some(name_not_found_message) = name_not_found_message {
            if let Some(property_with_invalid_initializer) = property_with_invalid_initializer
                && let Some(on_property_with_invalid_initializer) =
                    self.on_property_with_invalid_initializer
                && on_property_with_invalid_initializer(
                    host,
                    original_location,
                    name,
                    property_with_invalid_initializer,
                    result,
                )
            {
                return None;
            }
            match result {
                None => {
                    if let Some(on_failed_to_resolve_symbol) = self.on_failed_to_resolve_symbol {
                        on_failed_to_resolve_symbol(
                            host,
                            original_location,
                            name,
                            meaning,
                            name_not_found_message,
                        );
                    }
                }
                Some(res) => {
                    if let Some(on_successfully_resolved_symbol) =
                        self.on_successfully_resolved_symbol
                    {
                        on_successfully_resolved_symbol(
                            host,
                            original_location,
                            res,
                            meaning,
                            last_location,
                            associated_declaration_for_containing_initializer_or_binding_name,
                            within_deferred_context,
                        );
                    }
                }
            }
        }
        result
    }

    fn use_outer_variable_scope_in_parameter(
        &self,
        host: &mut H,
        result: P<Symbol>,
        location: P<Node>,
        last_location: P<Node>,
    ) -> bool {
        if ast::is_parameter_declaration(last_location)
            && let (Some(body), Some(value_declaration)) =
                (location.body(), result.value_declaration())
            && value_declaration.pos() >= body.pos()
            && value_declaration.end() <= body.end()
        {
            // check for several cases where we introduce temporaries that require moving the name/initializer of the parameter to the body
            // - static field in a class expression
            // - optional chaining pre-es2020
            // - nullish coalesce pre-es2020
            // - spread assignment in binding pattern pre-es2017
            let function_location = location;
            let mut declaration_requires_scope_change = Tristate::Unknown;
            if let Some(get_requires_scope_change_cache) = self.get_requires_scope_change_cache {
                declaration_requires_scope_change =
                    get_requires_scope_change_cache(host, function_location);
            }
            if declaration_requires_scope_change == Tristate::Unknown {
                declaration_requires_scope_change = if function_location
                    .parameters()
                    .iter()
                    .any(|&p| self.requires_scope_change(p))
                {
                    Tristate::True
                } else {
                    Tristate::False
                };
                if let Some(set_requires_scope_change_cache) = self.set_requires_scope_change_cache
                {
                    set_requires_scope_change_cache(
                        host,
                        function_location,
                        declaration_requires_scope_change,
                    );
                }
            }
            return declaration_requires_scope_change != Tristate::True;
        }
        false
    }

    fn requires_scope_change(&self, node: P<Node>) -> bool {
        let d = node.as_parameter_declaration();
        self.requires_scope_change_worker(node.name().unwrap())
            || d.initializer()
                .is_some_and(|initializer| self.requires_scope_change_worker(initializer))
    }

    fn requires_scope_change_worker(&self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::ArrowFunction
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::Constructor => false,
            Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::PropertyAssignment => self.requires_scope_change_worker(node.name().unwrap()),
            Kind::PropertyDeclaration => {
                if ast::has_static_modifier(node) {
                    return !self.compiler_options.get_emit_standard_class_fields();
                }
                self.requires_scope_change_worker(node.name().unwrap())
            }
            _ => {
                if ast::is_nullish_coalesce(node) || ast::is_optional_chain(node) {
                    return self.compiler_options.get_emit_script_target() < ScriptTarget::ES2020;
                }
                if ast::is_binding_element(node)
                    && node.as_binding_element().dot_dot_dot_token().is_some()
                    && ast::is_object_binding_pattern(node.parent().unwrap())
                {
                    return self.compiler_options.get_emit_script_target() < ScriptTarget::ES2017;
                }
                if ast::is_type_node(node) {
                    return false;
                }
                node.for_each_child(&mut |child| self.requires_scope_change_worker(child))
            }
        }
    }

    fn error(
        &self,
        host: &mut H,
        location: Option<P<Node>>,
        message: &'static Message,
        args: &[&dyn Display],
    ) {
        if let Some(error) = self.error {
            error(host, location, message, args);
        }
        // Default implementation does not report errors
    }

    fn get_symbol_of_declaration(&self, host: &mut H, node: P<Node>) -> Option<P<Symbol>> {
        if let Some(get_symbol_of_declaration) = self.get_symbol_of_declaration {
            return get_symbol_of_declaration(host, node);
        }

        // Default implementation does not support merged symbols
        node.symbol()
    }

    fn lookup(
        &self,
        host: &mut H,
        symbols: Option<P<SymbolTable>>,
        name: &str,
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        if let Some(lookup) = self.lookup {
            return lookup(host, symbols, name, meaning);
        }
        // Default implementation does not support following aliases or merged symbols
        if !meaning.is_empty()
            && let Some(symbol) = symbols.and_then(|s| (*s).get(name))
            && symbol.flags.get().intersects(meaning)
        {
            return Some(symbol);
        }
        None
    }

    fn arguments_symbol(&self) -> P<Symbol> {
        if let Some(arguments_symbol) = self.arguments_symbol.get() {
            return arguments_symbol;
        }
        // Default implementation synthesizes a transient symbol for `arguments`
        let arguments_symbol =
            Symbol::new(SymbolFlags::Property | SymbolFlags::Transient, "arguments");
        self.arguments_symbol.set(Some(arguments_symbol));
        arguments_symbol
    }
}

pub fn get_local_symbol_for_export_default(symbol: P<Symbol>) -> Option<P<Symbol>> {
    if !is_export_default_symbol(Some(symbol)) || symbol.declarations().is_empty() {
        return None;
    }
    let declarations = symbol.declarations();
    if let Some(result) = declarations.iter().find_map(|decl| {
        if let Some(local_symbol) = decl.local_symbol() {
            return Some(Some(local_symbol));
        }

        None
    }) {
        return result;
    }
    None
}

fn is_export_default_symbol(symbol: Option<P<Symbol>>) -> bool {
    let Some(symbol) = symbol else {
        return false;
    };
    let declarations = symbol.declarations();
    !declarations.is_empty() && ast::has_syntactic_modifier(declarations[0], ModifierFlags::Default)
}

/// tsrs-only: kinds of the ancestors a name is most often resolved through, for which `resolve` does nothing but
/// move to the parent: no locals, no case of its kind switch, never a deferred context or a self-reference
/// location, not a const assertion.
#[inline]
fn is_pass_through_scope_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Identifier
            | Kind::QualifiedName
            | Kind::CallExpression
            | Kind::NewExpression
            | Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::ExpressionStatement
            | Kind::ReturnStatement
            | Kind::IfStatement
            | Kind::SwitchStatement
            | Kind::CaseClause
            | Kind::DefaultClause
            | Kind::WhileStatement
            | Kind::DoStatement
            | Kind::TryStatement
            | Kind::ThrowStatement
            | Kind::LabeledStatement
            | Kind::ModuleBlock
            | Kind::VariableDeclaration
            | Kind::VariableDeclarationList
            | Kind::VariableStatement
            | Kind::ObjectLiteralExpression
            | Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::SpreadAssignment
            | Kind::ArrayLiteralExpression
            | Kind::BinaryExpression
            | Kind::ParenthesizedExpression
            | Kind::ConditionalExpression
            | Kind::PrefixUnaryExpression
            | Kind::PostfixUnaryExpression
            | Kind::TypeOfExpression
            | Kind::VoidExpression
            | Kind::DeleteExpression
            | Kind::AwaitExpression
            | Kind::YieldExpression
            | Kind::SpreadElement
            | Kind::SatisfiesExpression
            | Kind::NonNullExpression
            | Kind::TemplateExpression
            | Kind::TemplateSpan
            | Kind::TaggedTemplateExpression
            | Kind::JsxElement
            | Kind::JsxSelfClosingElement
            | Kind::JsxOpeningElement
            | Kind::JsxAttributes
            | Kind::JsxAttribute
            | Kind::JsxExpression
            | Kind::JsxFragment
            | Kind::TypeReference
            | Kind::PropertySignature
            | Kind::TypeLiteral
            | Kind::UnionType
            | Kind::IntersectionType
            | Kind::ArrayType
            | Kind::IndexedAccessType
            | Kind::ParenthesizedType
            | Kind::TupleType
            | Kind::TypeOperator
            | Kind::TypePredicate
            | Kind::TypeParameter
            | Kind::HeritageClause
            | Kind::EnumMember
    )
}

/// tsrs: name resolution asks this of every ancestor of a reference (131M calls on the 38k-file codebase), and only
/// these kinds can answer true; the test itself is out of line.
#[inline]
fn get_is_deferred_context(location: P<Node>, last_location: Option<P<Node>>) -> bool {
    let kind = location.kind();
    if !matches!(
        kind,
        Kind::ArrowFunction
            | Kind::FunctionExpression
            | Kind::TypeQuery
            | Kind::PropertyDeclaration
    ) && !ast::is_function_like_declaration(location)
    {
        return false;
    }
    get_is_deferred_context_worker(location, last_location)
}

#[inline(never)]
fn get_is_deferred_context_worker(location: P<Node>, last_location: Option<P<Node>>) -> bool {
    if location.kind() != Kind::ArrowFunction && location.kind() != Kind::FunctionExpression {
        // initializers in instance property declaration of class like entities are executed in constructor and thus deferred
        // A name is evaluated within the enclosing scope - so it shouldn't count as deferred
        return ast::is_type_query_node(location)
            || (ast::is_function_like_declaration(Some(location))
                || location.kind() == Kind::PropertyDeclaration && !ast::is_static(location))
                && (last_location.is_none() || last_location != location.name());
    }
    if last_location.is_some() && last_location == location.name() {
        return false;
    }
    // generator functions and async functions are not inlined in control flow when immediately invoked
    if location.body_data().unwrap().asterisk_token.is_some()
        || ast::has_syntactic_modifier(location, ModifierFlags::Async)
    {
        return true;
    }
    ast::get_immediately_invoked_function_expression(location).is_none()
}

fn is_type_parameter_symbol_declared_in_container(symbol: P<Symbol>, container: P<Node>) -> bool {
    if symbol.declarations().iter().any(|&decl| {
        if decl.kind() == Kind::TypeParameter {
            let parent = decl.parent();
            if parent == Some(container) {
                return true;
            }
        }

        false
    }) {
        return true;
    }
    false
}

fn is_self_reference_location(node: P<Node>, last_location: Option<P<Node>>) -> bool {
    match node.kind() {
        Kind::Parameter => last_location.is_some() && last_location == node.name(),
        Kind::FunctionDeclaration
        | Kind::ClassDeclaration
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::ModuleDeclaration => true, // For `namespace N { N; }`
        _ => false,
    }
}
