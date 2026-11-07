use std::cell::RefCell;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, CheckFlags, Kind, ModifierFlags, Node, NodeFactory, NodeFlags, NodeList,
    SourceFile, Symbol, SymbolFlags, TokenFlags,
};
use tsrs_checker::{
    self as checker, Checker, Flags, IndexInfo, InternalFlags, NodeBuilder, Signature, Type,
};
use tsrs_compiler::Program;
use tsrs_core::context::Locale;
use tsrs_core::{P, alloc_str};
use tsrs_diagnostics as diagnostics;

use crate::autoimport::{self, IdToSymbol, ImportAdder};
use crate::change;
use crate::lsutil::{self, QuotePreference, UserPreferences};

bitflags::bitflags! {
    // codeactions_missingmemberfixer.go:18
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub(crate) struct preserveOptionalFlags: u32 {
        const Method = 1 << 0;
        const Property = 1 << 1;
        const All = Self::Method.bits() | Self::Property.bits();
    }
}

// codeactions_missingmemberfixer.go:26
pub struct MissingMemberFixer<'a> {
    change_tracker: &'a change::Tracker,
    type_checker: &'a mut Checker,
    program: &'static Program,
    preferences: UserPreferences,
    import_adder: Option<&'a mut (dyn ImportAdder + 'static)>,
    #[expect(
        dead_code,
        reason = "Go reads it for Method_not_implemented.Localize(f.locale); Message::localize has no locale parameter (English only)"
    )]
    locale: Locale,
}

// codeactions_missingmemberfixer.go:35
pub fn new_missing_member_fixer<'a>(
    change_tracker: &'a change::Tracker,
    program: &'static Program,
    type_checker: &'a mut Checker,
    preferences: UserPreferences,
    import_adder: Option<&'a mut (dyn ImportAdder + 'static)>,
    locale: Locale,
) -> MissingMemberFixer<'a> {
    MissingMemberFixer { change_tracker, type_checker, program, preferences, import_adder, locale }
}

impl<'a> MissingMemberFixer<'a> {
    fn factory(&self) -> &'a NodeFactory {
        &self.change_tracker.node_factory
    }

    // codeactions_missingmemberfixer.go:46
    fn create_node_builder(&self) -> (P<NodeBuilder>, IdToSymbol) {
        let id_to_symbol: IdToSymbol = P::new(RefCell::new(FxHashMap::default()));
        let node_builder = checker::new_node_builder_ex(
            self.type_checker,
            self.change_tracker.emit_context,
            Some(id_to_symbol),
        );
        (node_builder, id_to_symbol)
    }

    // codeactions_missingmemberfixer.go:52
    pub(crate) fn create_member_from_symbol(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: P<Node>,
        source_file: P<SourceFile>,
        body: Option<P<Node>>,
        preserve_optional: preserveOptionalFlags,
        abstract_: bool,
    ) -> Vec<P<Node>> {
        let declarations = symbol.declarations();
        let declaration = declarations.first().copied();

        let quote_preference = lsutil::get_quote_preference(source_file, &self.preferences);
        let ambient = enclosing_declaration.flags().intersects(NodeFlags::Ambient);
        let signature_only = ambient || abstract_;
        let optional = symbol.flags().intersects(SymbolFlags::Optional);
        let mut kind = Kind::PropertySignature;
        if let Some(declaration) = declaration {
            kind = declaration.kind();
        }
        let declaration_name =
            create_declaration_name(self.factory(), self.type_checker, Some(symbol), declaration);
        let modifiers = self.create_modifiers(symbol, declaration);

        let mut flags = Flags::NoTruncation;
        if quote_preference == QuotePreference::Single {
            flags |= Flags::UseSingleQuotesForStringLiteralType;
        }

        let type_of_symbol = self
            .type_checker
            .get_type_of_symbol_at_location(symbol, Some(enclosing_declaration))
            .unwrap();
        let t = self.type_checker.get_widened_type_exported(type_of_symbol);
        let mut nodes: Vec<P<Node>> = Vec::new();

        match kind {
            Kind::PropertySignature | Kind::PropertyDeclaration => {
                let (node_builder, id_to_symbol) = self.create_node_builder();
                let type_node = self.create_type_node(
                    t,
                    enclosing_declaration,
                    flags,
                    node_builder,
                    id_to_symbol,
                );
                let mut question_token: Option<P<Node>> = None;
                if optional && preserve_optional.intersects(preserveOptionalFlags::Property) {
                    question_token = Some(self.factory().new_token(Kind::QuestionToken));
                }
                nodes.push(self.factory().new_property_declaration(
                    modifiers,
                    create_property_name(
                        self.factory(),
                        declaration_name.unwrap(),
                        quote_preference,
                    ),
                    question_token,
                    type_node,
                    None, /*initializer*/
                ));
                nodes
            }

            Kind::GetAccessor | Kind::SetAccessor => {
                let (node_builder, id_to_symbol) = self.create_node_builder();
                let accessors =
                    ast::get_all_accessor_declarations(symbol.declarations(), declaration.unwrap());
                let mut ordered_accessors: Vec<P<Node>> = Vec::new();
                ordered_accessors.push(accessors.first_accessor);
                if let Some(second_accessor) = accessors.second_accessor {
                    ordered_accessors.push(second_accessor);
                }

                nodes = ordered_accessors.into_iter().fold(nodes, |mut nodes, accessor| {
                    if ast::is_get_accessor_declaration(accessor) {
                        let type_node = self.create_type_node(
                            t,
                            enclosing_declaration,
                            flags,
                            node_builder,
                            id_to_symbol,
                        );
                        let body = self.create_body(body, quote_preference, signature_only);
                        nodes.push(self.factory().new_get_accessor_declaration(
                            modifiers,
                            create_property_name(
                                self.factory(),
                                declaration_name.unwrap(),
                                quote_preference,
                            ),
                            None, /*typeParameters*/
                            None, /*parameters*/
                            type_node,
                            None, /*fullSignature*/
                            body,
                        ));
                    }

                    if ast::is_set_accessor_declaration(accessor) {
                        let Some(parameter) = checker::get_set_accessor_value_parameter(accessor)
                        else {
                            panic!("Expected set accessor to have a parameter.");
                        };

                        let type_node = self.create_type_node(
                            t,
                            enclosing_declaration,
                            flags,
                            node_builder,
                            id_to_symbol,
                        );
                        let parameters = create_dummy_parameters(
                            self.factory(),
                            1,
                            &[parameter.name().unwrap().text().to_string()],
                            &[type_node],
                            1,
                            ast::is_in_js_file(enclosing_declaration),
                        );
                        let body = self.create_body(body, quote_preference, signature_only);
                        nodes.push(self.factory().new_set_accessor_declaration(
                            modifiers,
                            create_property_name(
                                self.factory(),
                                declaration_name.unwrap(),
                                quote_preference,
                            ),
                            None, /*typeParameters*/
                            Some(parameters),
                            None, /*type*/
                            None, /*fullSignature*/
                            body,
                        ));
                    }

                    nodes
                });
                nodes
            }

            Kind::MethodSignature | Kind::MethodDeclaration => {
                let signatures = self.get_call_signatures(t);
                let preserve_optional =
                    optional && preserve_optional.intersects(preserveOptionalFlags::Method);
                if signatures.is_empty() {
                    return Vec::new();
                }

                if declarations.len() == 1 {
                    let body = self.create_body(body, quote_preference, signature_only);
                    let method = self.create_signature_declaration_from_signature(
                        signatures[0],
                        Kind::MethodDeclaration,
                        source_file,
                        enclosing_declaration,
                        body,
                        modifiers,
                        declaration_name,
                        preserve_optional,
                    );
                    if let Some(method) = method {
                        nodes.push(method);
                    }
                    return nodes;
                }

                nodes.extend(signatures.iter().filter_map(|&signature| {
                    if let Some(signature_declaration) = signature.declaration()
                        && signature_declaration.flags().intersects(NodeFlags::Ambient)
                    {
                        return None;
                    }

                    self.create_signature_declaration_from_signature(
                        signature,
                        Kind::MethodDeclaration,
                        source_file,
                        enclosing_declaration,
                        None, /*body*/
                        modifiers,
                        declaration_name,
                        preserve_optional,
                    )
                }));

                if signature_only {
                    return nodes;
                }

                if declarations.len() > signatures.len() {
                    let signature = self
                        .type_checker
                        .get_signature_from_declaration_exported(*declarations.last().unwrap());
                    let body =
                        self.create_body(body, quote_preference, false /*signatureOnly*/);
                    let method = self.create_signature_declaration_from_signature(
                        signature,
                        Kind::MethodDeclaration,
                        source_file,
                        enclosing_declaration,
                        body,
                        modifiers,
                        declaration_name,
                        preserve_optional,
                    );
                    if let Some(method) = method {
                        nodes.push(method);
                    }
                } else {
                    let method = self.create_signature_declaration_from_signatures(
                        &signatures,
                        declaration_name,
                        preserve_optional,
                        modifiers,
                        quote_preference,
                        body,
                        enclosing_declaration,
                    );
                    if let Some(method) = method {
                        nodes.push(method);
                    }
                }

                nodes
            }
            _ => Vec::new(),
        }
    }

    // codeactions_missingmemberfixer.go:170
    fn get_call_signatures(&mut self, t: P<Type>) -> Vec<P<Signature>> {
        if t.is_union() {
            let mut result = Vec::new();
            result.extend(
                t.types().iter().flat_map(|&member| {
                    self.type_checker.get_call_signatures(member).iter().copied()
                }),
            );
            return result;
        }
        self.type_checker.get_call_signatures(t).to_vec()
    }

    // codeactions_missingmemberfixer.go:177
    fn create_type_node(
        &mut self,
        t: P<Type>,
        enclosing_declaration: P<Node>,
        flags: Flags,
        node_builder: P<NodeBuilder>,
        id_to_symbol: IdToSymbol,
    ) -> Option<P<Node>> {
        let type_node = node_builder.type_to_type_node(
            self.type_checker,
            t,
            Some(enclosing_declaration),
            flags,
            InternalFlags::None,
            None, /*tracker*/
        );
        self.import_type_node(type_node, id_to_symbol)
    }

    // codeactions_missingmemberfixer.go:181
    fn create_modifiers(
        &self,
        symbol: P<Symbol>,
        declaration: Option<P<Node>>,
    ) -> Option<P<ast::ModifierList>> {
        let mut modifier_flags = ModifierFlags::None;
        if let Some(declaration) = declaration {
            let effective = checker::get_declaration_modifier_flags_from_symbol_exported(symbol);
            modifier_flags = effective & ModifierFlags::Static;
            if effective.intersects(ModifierFlags::Public) {
                modifier_flags |= ModifierFlags::Public;
            } else if effective.intersects(ModifierFlags::Protected) {
                modifier_flags |= ModifierFlags::Protected;
            }
            if ast::is_auto_accessor_property_declaration(declaration) {
                modifier_flags |= ModifierFlags::Accessor;
            }
        }
        if self.should_add_override_keyword(declaration) {
            modifier_flags |= ModifierFlags::Override;
        }
        if modifier_flags == ModifierFlags::None {
            return None;
        }
        let factory = self.factory();
        Some(
            factory.new_modifier_list(ast::create_modifiers_from_modifier_flags(
                modifier_flags,
                |k| factory.new_modifier(k),
            )),
        )
    }

    // codeactions_missingmemberfixer.go:204
    fn should_add_override_keyword(&self, declaration: Option<P<Node>>) -> bool {
        declaration.is_some()
            && self.program.options().no_implicit_override.is_true()
            && ast::has_abstract_modifier(declaration.unwrap())
    }

    // codeactions_missingmemberfixer.go:208
    fn create_signature_declaration_from_signature(
        &mut self,
        signature: P<Signature>,
        kind: Kind,
        source_file: P<SourceFile>,
        enclosing_declaration: P<Node>,
        body: Option<P<Node>>,
        modifiers: Option<P<ast::ModifierList>>,
        name: Option<P<Node>>,
        optional: bool,
    ) -> Option<P<Node>> {
        let quote_preference = lsutil::get_quote_preference(source_file, &self.preferences);
        let mut flags = Flags::NoTruncation | Flags::SuppressAnyReturnType | Flags::AllowEmptyTuple;
        if quote_preference == QuotePreference::Single {
            flags |= Flags::UseSingleQuotesForStringLiteralType;
        }

        let (node_builder, id_to_symbol) = self.create_node_builder();
        let signature_declaration = node_builder.signature_to_signature_declaration(
            self.type_checker,
            signature,
            kind,
            Some(enclosing_declaration),
            flags,
            InternalFlags::AllowUnresolvedNames,
            None, /*tracker*/
        )?;

        let is_js = ast::is_in_js_file(enclosing_declaration);
        let mut parameters = signature_declaration.parameter_list();
        let mut type_parameters =
            if is_js { None } else { signature_declaration.type_parameter_list() };
        let mut type_node = if is_js { None } else { signature_declaration.type_node() };

        if let Some(tps) = type_parameters.filter(|tps| !tps.nodes().is_empty()) {
            let mut nodes = Vec::with_capacity(tps.nodes().len());
            nodes.extend(tps.nodes().iter().map(|&tp| {
                if ast::is_type_parameter_declaration(tp) {
                    let type_parameter = tp.as_type_parameter_declaration();

                    let mut constraint = type_parameter.constraint;
                    if constraint.is_some() {
                        constraint = self.import_type_node(constraint, id_to_symbol);
                    }

                    let mut default_type = type_parameter.default_type;
                    if default_type.is_some() {
                        default_type = self.import_type_node(default_type, id_to_symbol);
                    }

                    self.factory().update_type_parameter_declaration(
                        tp,
                        tp.modifiers(),
                        tp.name().unwrap(),
                        constraint,
                        type_parameter.expression,
                        default_type,
                    )
                } else {
                    tp
                }
            }));
            type_parameters = Some(self.factory().new_node_list(nodes));
        }

        if let Some(ps) = parameters {
            let mut nodes = Vec::with_capacity(ps.nodes().len());
            nodes.extend(ps.nodes().iter().map(|&p| {
                let parameter = p.as_parameter_declaration();
                let mut parameter_type_node = parameter.type_();
                if parameter_type_node.is_some() {
                    parameter_type_node = self.import_type_node(parameter_type_node, id_to_symbol);
                }

                self.factory().update_parameter_declaration(
                    p,
                    p.modifiers(),
                    parameter.dot_dot_dot_token(),
                    p.name().unwrap(),
                    if is_js { None } else { parameter.question_token() },
                    parameter_type_node,
                    p.initializer(),
                )
            }));
            parameters = Some(self.factory().new_node_list(nodes));
        }

        if type_node.is_some() {
            type_node = self.import_type_node(type_node, id_to_symbol);
        }

        let mut question_token: Option<P<Node>> = None;
        if optional {
            question_token = Some(self.factory().new_token(Kind::QuestionToken));
        }

        let factory = self.factory();
        match kind {
            Kind::FunctionExpression => {
                let fn_ = signature_declaration.as_function_expression();
                Some(factory.update_function_expression(
                    signature_declaration,
                    modifiers,
                    fn_.asterisk_token(),
                    if name.is_some() && ast::is_identifier(name.unwrap()) { name } else { None },
                    type_parameters,
                    parameters,
                    type_node,
                    fn_.full_signature(),
                    body.or(fn_.body()),
                ))
            }

            Kind::ArrowFunction => {
                let fn_ = signature_declaration.as_arrow_function();
                Some(factory.update_arrow_function(
                    signature_declaration,
                    modifiers,
                    type_parameters,
                    parameters,
                    type_node,
                    fn_.full_signature(),
                    fn_.equals_greater_than_token,
                    body.or(fn_.body()),
                ))
            }

            Kind::MethodDeclaration => {
                let method = signature_declaration.as_method_declaration();
                let method_name = if let Some(name) = name {
                    create_property_name(factory, name, quote_preference)
                } else {
                    factory.new_identifier("")
                };
                Some(factory.update_method_declaration(
                    signature_declaration,
                    modifiers,
                    method.asterisk_token(),
                    method_name,
                    question_token,
                    type_parameters,
                    parameters,
                    type_node,
                    method.full_signature(),
                    body,
                ))
            }

            Kind::FunctionDeclaration => {
                let fn_ = signature_declaration.as_function_declaration();
                Some(factory.update_function_declaration(
                    signature_declaration,
                    modifiers,
                    fn_.asterisk_token(),
                    if name.is_some() && ast::is_identifier(name.unwrap()) { name } else { None },
                    type_parameters,
                    parameters,
                    type_node,
                    fn_.full_signature(),
                    body.or(fn_.body()),
                ))
            }
            _ => None,
        }
    }

    // codeactions_missingmemberfixer.go:305
    fn create_signature_declaration_from_signatures(
        &mut self,
        signatures: &[P<Signature>],
        name: Option<P<Node>>,
        optional: bool,
        modifiers: Option<P<ast::ModifierList>>,
        quote_preference: QuotePreference,
        body: Option<P<Node>>,
        enclosing_declaration: P<Node>,
    ) -> Option<P<Node>> {
        if signatures.is_empty() {
            return None;
        }

        let (node_builder, id_to_symbol) = self.create_node_builder();
        let mut max_args_signature = signatures[0];
        let mut min_argument_count = signatures[0].min_argument_count();

        let mut has_rest_parameter = false;
        for &signature in signatures {
            min_argument_count = min_argument_count.min(signature.min_argument_count());
            if signature.has_rest_parameter() {
                has_rest_parameter = true;
            }
            if signature.parameters().len() >= max_args_signature.parameters().len()
                && (!signature.has_rest_parameter() || max_args_signature.has_rest_parameter())
            {
                max_args_signature = signature;
            }
        }

        let max_non_rest_args = max_args_signature.parameters().len()
            - usize::from(max_args_signature.has_rest_parameter());
        let mut parameter_names: Vec<String> =
            Vec::with_capacity(max_args_signature.parameters().len());
        parameter_names
            .extend(max_args_signature.parameters().iter().map(|symbol| symbol.name().to_string()));
        let parameters = create_dummy_parameters(
            self.factory(),
            max_non_rest_args,
            &parameter_names,
            &[], /*types*/
            min_argument_count,
            ast::is_in_js_file(enclosing_declaration),
        );
        let mut parameter_nodes = parameters.nodes().to_vec();

        let factory = self.factory();
        if has_rest_parameter {
            let mut rest_parameter_name = "rest".to_string();
            if max_non_rest_args < parameter_names.len()
                && !parameter_names[max_non_rest_args].is_empty()
            {
                rest_parameter_name.clone_from(&parameter_names[max_non_rest_args]);
            }

            let mut question_token: Option<P<Node>> = None;
            if max_non_rest_args >= min_argument_count {
                question_token = Some(factory.new_token(Kind::QuestionToken));
            }

            parameter_nodes.push(
                factory.new_parameter_declaration(
                    None, /*modifiers*/
                    Some(factory.new_token(Kind::DotDotDotToken)),
                    factory.new_identifier(alloc_str(&rest_parameter_name)),
                    question_token,
                    Some(
                        factory.new_array_type_node(
                            factory.new_keyword_type_node(Kind::UnknownKeyword),
                        ),
                    ),
                    None, /*initializer*/
                ),
            );
        }
        // Go appends to `parameters.Nodes` in place; the list's range is kept.
        let parameters = {
            let list = factory.new_node_list(parameter_nodes);
            list.loc.set(parameters.loc.get());
            list
        };

        let method_name = if let Some(name) = name {
            create_property_name(factory, name, quote_preference)
        } else {
            factory.new_identifier("")
        };

        let question_token =
            if optional { Some(factory.new_token(Kind::QuestionToken)) } else { None };
        let return_type = self.get_return_type_from_signatures(
            signatures,
            enclosing_declaration,
            node_builder,
            id_to_symbol,
        );
        let body = self.create_body(body, quote_preference, false /*signatureOnly*/);
        let factory = self.factory();
        Some(factory.new_method_declaration(
            modifiers,
            None, /*asteriskToken*/
            method_name,
            question_token,
            None, /*typeParameters*/
            Some(parameters),
            return_type,
            None, /*fullSignature*/
            body,
        ))
    }

    // codeactions_missingmemberfixer.go:359
    fn get_return_type_from_signatures(
        &mut self,
        signatures: &[P<Signature>],
        enclosing_declaration: P<Node>,
        node_builder: P<NodeBuilder>,
        id_to_symbol: IdToSymbol,
    ) -> Option<P<Node>> {
        if signatures.is_empty() {
            return None;
        }

        let mut return_types: Vec<P<Type>> = Vec::with_capacity(signatures.len());
        return_types.extend(
            signatures.iter().map(|&signature| {
                self.type_checker.get_return_type_of_signature_exported(signature)
            }),
        );

        let union_type = self.type_checker.get_union_type_exported(&return_types);
        let type_node = node_builder.type_to_type_node(
            self.type_checker,
            union_type,
            Some(enclosing_declaration),
            Flags::NoTruncation,
            InternalFlags::AllowUnresolvedNames,
            None, /*typeArguments*/
        );
        self.import_type_node(type_node, id_to_symbol)
    }

    // codeactions_missingmemberfixer.go:373
    fn import_type_node(
        &mut self,
        type_node: Option<P<Node>>,
        id_to_symbol: IdToSymbol,
    ) -> Option<P<Node>> {
        if type_node.is_none() || self.import_adder.is_none() {
            return type_node;
        }

        let (imported_type_node, symbols) =
            autoimport::try_get_auto_importable_reference_from_type_node(type_node, id_to_symbol);
        if imported_type_node.is_some() {
            for symbol in symbols {
                let Some(export_symbol) = self.get_exported_symbol(symbol) else {
                    continue;
                };
                self.import_adder.as_mut().unwrap().add_import_from_exported_symbol(
                    export_symbol,
                    true, /*isValidTypeOnlyUseSite*/
                );
            }
            return imported_type_node;
        }

        // Go iterates the idToSymbol map (random order).
        let mut seen: FxHashSet<P<Symbol>> = FxHashSet::default();
        let symbols: Vec<P<Symbol>> = id_to_symbol.borrow().values().copied().collect();
        for symbol in symbols {
            if !seen.insert(symbol) {
                continue;
            }
            let Some(export_symbol) = self.get_exported_symbol(symbol) else {
                continue;
            };
            self.import_adder.as_mut().unwrap().add_import_from_exported_symbol(
                export_symbol,
                true, /*isValidTypeOnlyUseSite*/
            );
        }
        type_node
    }

    // codeactions_missingmemberfixer.go:405
    fn get_exported_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        let symbol = self.type_checker.get_export_symbol_of_symbol(symbol);
        symbol.parent()?;
        Some(symbol)
    }

    // codeactions_missingmemberfixer.go:413
    pub(crate) fn create_index_signature_declaration_from_type(
        &mut self,
        class_declaration: P<Node>,
        implemented_type: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<Node>> {
        let index_info: P<IndexInfo> =
            self.type_checker.get_index_info_of_type_exported(implemented_type, key_type)?;

        let builder =
            checker::new_node_builder(self.type_checker, self.change_tracker.emit_context);
        builder.index_info_to_index_signature_declaration(
            self.type_checker,
            index_info,
            Some(class_declaration),
            Flags::None,
            InternalFlags::None,
            None,
        )
    }

    // codeactions_missingmemberfixer.go:423
    fn create_body(
        &self,
        body: Option<P<Node>>,
        quote_preference: QuotePreference,
        signature_only: bool,
    ) -> Option<P<Node>> {
        if signature_only {
            return None;
        }
        let body = self.factory().deep_clone_node(body);
        if body.is_none() {
            return Some(self.create_stubbed_method_body(quote_preference));
        }
        body
    }

    // codeactions_missingmemberfixer.go:434
    fn create_stubbed_method_body(&self, quote_preference: QuotePreference) -> P<Node> {
        let mut token_flags = TokenFlags::None;
        if quote_preference == QuotePreference::Single {
            token_flags = TokenFlags::SingleQuote;
        }

        let factory = self.factory();
        factory.new_block(
            factory.new_node_list(vec![factory.new_throw_statement(factory.new_new_expression(
                factory.new_identifier("Error"),
                None, /*typeArguments*/
                Some(factory.new_node_list(vec![factory.new_string_literal(
                    alloc_str(&diagnostics::METHOD_NOT_IMPLEMENTED.localize(&[])),
                    token_flags,
                )])),
            ))]),
            true, /*multiLine*/
        )
    }
}

// codeactions_missingmemberfixer.go:451
pub fn create_dummy_parameters(
    factory: &NodeFactory,
    arg_count: usize,
    names: &[String],
    types: &[Option<P<Node>>],
    min_argument_count: usize,
    in_js: bool,
) -> P<NodeList> {
    let mut parameters = Vec::with_capacity(arg_count);
    let mut parameter_name_counts: FxHashMap<String, usize> = FxHashMap::default();

    for i in 0..arg_count {
        let mut parameter_name = if i < names.len() && !names[i].is_empty() {
            names[i].clone()
        } else {
            format!("arg{i}")
        };

        let count_entry = parameter_name_counts.entry(parameter_name.clone()).or_default();
        let count = *count_entry;
        *count_entry += 1;

        if count > 0 {
            parameter_name += &count.to_string();
        }

        let mut question_token: Option<P<Node>> = None;
        if i >= min_argument_count {
            question_token = Some(factory.new_token(Kind::QuestionToken));
        }

        let type_node = if in_js {
            None
        } else if i < types.len() && types[i].is_some() {
            types[i]
        } else {
            Some(factory.new_keyword_type_node(Kind::UnknownKeyword))
        };
        parameters.push(factory.new_parameter_declaration(
            None, /*modifiers*/
            None, /*dotDotDotToken*/
            factory.new_identifier(alloc_str(&parameter_name)),
            question_token,
            type_node,
            None, /*initializer*/
        ));
    }
    factory.new_node_list(parameters)
}

// codeactions_missingmemberfixer.go:489
pub fn create_declaration_name(
    factory: &NodeFactory,
    type_checker: &Checker,
    symbol: Option<P<Symbol>>,
    declaration: Option<P<Node>>,
) -> Option<P<Node>> {
    if let Some(symbol) = symbol
        && symbol.check_flags().intersects(CheckFlags::Mapped)
    {
        let name_type = type_checker.get_name_type_of_symbol(symbol);
        if let Some(name_type) = name_type
            && checker::is_type_usable_as_property_name_exported(name_type)
        {
            return Some(factory.new_identifier(alloc_str(
                &checker::get_property_name_from_type_exported(name_type),
            )));
        }
    }
    if let Some(declaration) = declaration
        && let Some(name) = declaration.name()
    {
        return Some(name.clone_node(factory));
    }
    if let Some(symbol) = symbol {
        return Some(factory.new_identifier(symbol.name()));
    }
    None
}

// codeactions_missingmemberfixer.go:505
pub fn create_property_name(
    factory: &NodeFactory,
    node: P<Node>,
    quote_preference: QuotePreference,
) -> P<Node> {
    if ast::is_identifier(node) && node.text() == "constructor" {
        let mut token_flags = TokenFlags::None;
        if quote_preference == QuotePreference::Single {
            token_flags = TokenFlags::SingleQuote;
        }
        return factory
            .new_computed_property_name(factory.new_string_literal(node.text(), token_flags));
    }
    factory.deep_clone_node(Some(node)).unwrap()
}
