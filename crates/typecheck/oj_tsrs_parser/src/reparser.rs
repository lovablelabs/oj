use tsrs_ast::{
    self as ast, ClassLikeBase, Kind, ModifierList, Node, NodeFlags, NodeList, TokenFlags,
};
use tsrs_core::{P, TextRange, alloc_slice, alloc_str};
use tsrs_diagnostics as diagnostics;
use tsrs_scanner as scanner;

use crate::parser_1::{JSDocInfo, Parser, ParsingContext};

impl Parser {
    fn finish_reparsed_node(&mut self, node: P<Node>, location_node: P<Node>) {
        node.set_flags(self.context_flags | NodeFlags::Reparsed);
        node.set_loc(location_node.loc());
        self.override_parent_in_immediate_children(node);
    }

    fn finish_mutated_node(&mut self, node: P<Node>) {
        self.override_parent_in_immediate_children(node);
    }

    // The file's reparse cloner, built on first use.
    fn reparse_cloner(&mut self) -> &mut ast::deepclone::ReparseCloner {
        let factory = &self.factory;
        self.reparse_cloner.get_or_insert_with(|| factory.reparse_cloner())
    }

    // Deep-clone the given node and add the clone to the reparsed clone list. The list is used by ast.GetReparsedNodeForNode
    // to locate reparsed clones of JSDoc nodes. Since the binder attaches symbols to reparsed nodes and not to JSDoc nodes, we
    // need the mapping when obtaining symbols and types from JSDoc nodes.
    fn add_deep_clone_reparse(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let clone = self.reparse_cloner().clone_node(node);
        if let Some(clone) = clone {
            self.reparsed_clones.push(clone);
        }
        clone
    }

    fn add_transformed_reparse(&mut self, new_node: P<Node>, old: P<Node>) -> P<Node> {
        self.finish_reparsed_node(new_node, old);
        new_node.set_flags(new_node.flags() | NodeFlags::ReparserTransformedLiteral);
        self.reparsed_clones.push(new_node);
        new_node
    }

    fn check_non_identifier_name(&mut self, name: Option<P<Node>>) -> Option<P<Node>> {
        // Handles the case of anonymous functions
        let name = name?;
        if ast::is_identifier(name) && !scanner::is_valid_identifier(name.as_identifier().text()) {
            let mut err_loc = name.loc();
            if err_loc.len() == Some(0) {
                // missing name, emit error on the character before the missing name node
                let pos = name.loc().pos().expect_source("parsed nodes have source positions");
                err_loc = TextRange::new(pos - 1, pos);
            }
            self.parse_error_at_range(err_loc, &diagnostics::IDENTIFIER_EXPECTED, &[]);
        }
        Some(name)
    }

    // Hosted tags find a host and add their children to the correct location under the host.
    // Unhosted tags add synthetic nodes to the reparse list.
    pub(crate) fn reparse_tags(&mut self, parent: P<Node>, js_doc: &[P<Node>]) {
        for &j in js_doc {
            let is_last = j == js_doc[js_doc.len() - 1];
            let Some(tags) = j.as_jsdoc().tags else {
                continue;
            };
            for &tag in tags.nodes() {
                self.reparse_unhosted(tag, parent, j);
                if is_last {
                    self.reparse_hosted(tag, parent, j);
                }
            }
        }
    }

    fn reparse_unhosted(&mut self, tag: P<Node>, parent: P<Node>, js_doc: P<Node>) {
        match tag.kind() {
            Kind::JSDocTypedefTag => {
                let Some(type_expression) = tag.type_expression() else {
                    return;
                };
                let full_name = tag.name();
                let is_namespace = full_name.is_some_and(ast::is_module_declaration);
                let mut modifiers = None;
                if is_namespace {
                    modifiers = Some(self.create_export_modifier(tag));
                }
                let innermost = Self::get_innermost_name_of_jsdoc_namespace(full_name);
                let checked = self.check_non_identifier_name(innermost);
                let name = self.add_deep_clone_reparse(checked).unwrap();
                let type_alias = self.factory.new_js_type_alias_declaration(modifiers, name, None, None);
                let type_parameters = self.gather_type_parameters(js_doc, true /*typedefOrCallback*/);
                type_alias.as_type_alias_declaration().type_parameters.set(type_parameters);
                let t = match type_expression.kind() {
                    Kind::JSDocTypeExpression => self.add_deep_clone_reparse(type_expression.type_node()),
                    Kind::JSDocTypeLiteral => self.reparse_jsdoc_type_literal(Some(type_expression)),
                    _ => panic!(
                        "typedef tag type expression should be a name reference or a type expression{:?}",
                        type_expression.kind()
                    ),
                };
                type_alias.as_type_alias_declaration().type_.set(t);
                self.finish_reparsed_node(type_alias, tag);
                self.jsdoc_infos.push(JSDocInfo { parent: type_alias, js_docs: alloc_slice(&[js_doc]) });
                type_alias.set_flags(type_alias.flags() | NodeFlags::HasJSDoc);
                let result = self.wrap_in_jsdoc_namespace(full_name, type_alias, false /*nested*/);
                tsrs_core::arena_pin();
                self.reparse_list.push(result);
            }
            Kind::JSDocCallbackTag => {
                let Some(type_expression) = tag.type_expression() else {
                    return;
                };
                let full_name = tag.name();
                let is_namespace = full_name.is_some_and(ast::is_module_declaration);
                let mut modifiers = None;
                if is_namespace {
                    modifiers = Some(self.create_export_modifier(tag));
                }
                let function_type = self.reparse_jsdoc_signature(type_expression, tag, js_doc, tag, None);
                let innermost = Self::get_innermost_name_of_jsdoc_namespace(full_name);
                let name = self.add_deep_clone_reparse(innermost).unwrap();
                let type_alias = self.factory.new_js_type_alias_declaration(modifiers, name, None, Some(function_type));
                let type_parameters = self.gather_type_parameters(js_doc, true /*typedefOrCallback*/);
                type_alias.as_type_alias_declaration().type_parameters.set(type_parameters);
                self.finish_reparsed_node(type_alias, tag);
                self.jsdoc_infos.push(JSDocInfo { parent: type_alias, js_docs: alloc_slice(&[js_doc]) });
                type_alias.set_flags(type_alias.flags() | NodeFlags::HasJSDoc);
                let result = self.wrap_in_jsdoc_namespace(full_name, type_alias, false /*nested*/);
                tsrs_core::arena_pin();
                self.reparse_list.push(result);
            }
            Kind::JSDocImportTag => {
                let import_tag = tag.as_jsdoc_import_tag();
                if import_tag.import_clause.is_none() {
                    return;
                }
                let import_clause = self.add_deep_clone_reparse(import_tag.import_clause).unwrap();
                import_clause.as_import_clause().phase_modifier.set(Kind::TypeKeyword);
                let modifiers = self.reparse_cloner().clone_modifiers(tag.modifiers());
                let module_specifier = self.add_deep_clone_reparse(Some(import_tag.module_specifier)).unwrap();
                let attributes = self.add_deep_clone_reparse(import_tag.attributes);
                let import_declaration = self.factory.new_js_import_declaration(modifiers, Some(import_clause), module_specifier, attributes);
                self.finish_reparsed_node(import_declaration, tag);
                tsrs_core::arena_pin();
                self.reparse_list.push(import_declaration);
            }
            Kind::JSDocOverloadTag
                // Create overload signatures only for function, method, and constructor declarations outside object literals
                if (ast::is_function_declaration(parent)
                    || ast::is_method_declaration(parent)
                    || ast::is_constructor_declaration(parent))
                    && self.parsing_contexts & (1 << ParsingContext::ObjectLiteralMembers as u32)
                        == 0
                => {
                    let signature = self.reparse_jsdoc_signature(
                        tag.as_jsdoc_overload_tag().type_expression,
                        parent,
                        js_doc,
                        tag,
                        parent.modifiers(),
                    );
                    tsrs_core::arena_pin();
                    self.reparse_list.push(signature);
                }
            _ => {}
        }
    }

    fn reparse_jsdoc_signature(
        &mut self,
        js_signature: P<Node>,
        fun: P<Node>,
        js_doc: P<Node>,
        tag: P<Node>,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let cloned_modifiers = self.reparse_cloner().clone_modifiers(modifiers);
        let signature = match fun.kind() {
            Kind::FunctionDeclaration => {
                let checked = self.check_non_identifier_name(fun.name());
                let name = self.reparse_cloner().clone_node(checked);
                self.factory.new_function_declaration(
                    cloned_modifiers,
                    None,
                    name,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
            Kind::MethodDeclaration => {
                let checked = self.check_non_identifier_name(fun.name());
                let name = self.reparse_cloner().clone_node(checked).unwrap();
                self.factory.new_method_declaration(
                    cloned_modifiers,
                    None,
                    name,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
            Kind::Constructor => self.factory.new_constructor_declaration(
                cloned_modifiers,
                None,
                None,
                None,
                None,
                None,
            ),
            Kind::JSDocCallbackTag => {
                let any = self.factory.new_keyword_type_node(Kind::AnyKeyword);
                self.factory.new_function_type_node(None, None, Some(any))
            }
            _ => panic!("Unexpected kind {:?}", fun.kind()),
        };

        if tag.kind() != Kind::JSDocCallbackTag {
            let type_parameters =
                self.gather_type_parameters(js_doc, false /*typedefOrCallback*/);
            signature.function_like_data().unwrap().type_parameters.set(type_parameters);
        }
        let mut parameters: Vec<P<Node>> = Vec::new();
        for (pi, &param) in js_signature.parameters().iter().enumerate() {
            let parameter;
            if param.kind() == Kind::JSDocThisTag {
                let this_tag = param.as_jsdoc_this_tag();
                let this_ident = self.factory.new_identifier("this");
                this_ident.set_loc(param.loc());
                this_ident.set_flags(self.context_flags | NodeFlags::Reparsed);
                parameter = self
                    .factory
                    .new_parameter_declaration(None, None, this_ident, None, None, None);
                if let Some(type_expression) = Some(this_tag.type_expression) {
                    let t = self.add_deep_clone_reparse(type_expression.type_node());
                    parameter.as_parameter_declaration().type_.set(t);
                }
            } else if param.kind() == Kind::JSDocParameterTag
                || param.kind() == Kind::JSDocPropertyTag
            {
                let jsparam = param.as_jsdoc_parameter_or_property_tag();
                // Skip sub-property parameters (e.g., @param x.y) - these have QualifiedNames
                // and describe properties of a parent parameter, not standalone parameters.
                if ast::is_qualified_name(param.name().unwrap()) {
                    continue;
                }
                let mut dot_dot_dot_token = None;
                let mut param_type = None;

                if let Some(type_expression) = jsparam.type_expression {
                    if type_expression.type_node().unwrap().kind() == Kind::JSDocVariadicType {
                        let token = self.factory.new_token(Kind::DotDotDotToken);
                        token.set_loc(param.loc());
                        token.set_flags(self.context_flags | NodeFlags::Reparsed);
                        dot_dot_dot_token = Some(token);

                        let variadic_type =
                            type_expression.type_node().unwrap().as_jsdoc_variadic_type();
                        param_type = self.reparse_jsdoc_type_literal(Some(variadic_type.type_));
                    } else {
                        param_type = self.reparse_jsdoc_type_literal(type_expression.type_node());
                    }
                }
                let mut name = param.name().unwrap();
                if ast::is_identifier(name)
                    && !scanner::is_valid_identifier(name.as_identifier().text())
                {
                    // drop invalid chars for _, if empty, write _0, etc., so we have a valid param name to emit later
                    let mut result = String::new();
                    result.extend(name.as_identifier().text().char_indices().map(|(i, ch)| {
                        if if i == 0 {
                            scanner::is_identifier_start(ch as u32)
                        } else {
                            scanner::is_identifier_part(ch as u32)
                        } {
                            ch
                        } else {
                            '_'
                        }
                    }));
                    if result.is_empty() {
                        result.push('_');
                        result.push_str(&pi.to_string());
                    }
                    let identifier = self.factory.new_identifier(alloc_str(&result));
                    name = self.add_transformed_reparse(identifier, name);
                } else {
                    name = self.add_deep_clone_reparse(Some(name)).unwrap();
                }
                let question_token = self.make_question_if_optional(param);
                parameter = self.factory.new_parameter_declaration(
                    None,
                    dot_dot_dot_token,
                    name,
                    question_token,
                    param_type,
                    None,
                );
            } else {
                panic!("Unexpected kind {:?}", param.kind());
            }
            self.finish_reparsed_node(parameter, param);
            parameters.push(parameter);
            self.reparse_jsdoc_comment(parameter, param);
        }
        let parameter_list_loc =
            js_signature.as_jsdoc_signature().function_like_base.parameters.get().unwrap().loc();
        let parameter_list = self.new_node_list(parameter_list_loc, &parameters);
        signature.function_like_data().unwrap().parameters.set(Some(parameter_list));

        if let Some(return_tag) = js_signature.type_node()
            && let Some(type_expression) = return_tag.type_expression()
        {
            let t = self.add_deep_clone_reparse(type_expression.type_node());
            signature.function_like_data().unwrap().type_.set(t);
        }
        let mut loc = js_signature;
        if tag.kind() == Kind::JSDocOverloadTag {
            loc = tag.tag_name();
        }
        self.finish_reparsed_node(signature, loc);
        signature
    }

    fn reparse_jsdoc_type_literal(&mut self, t: Option<P<Node>>) -> Option<P<Node>> {
        let t = t?;
        if t.kind() == Kind::JSDocTypeLiteral {
            let jstypeliteral = t.as_jsdoc_type_literal();
            let is_array_type = jstypeliteral.is_array_type;
            let mut properties: Vec<P<Node>> = Vec::new();
            for &prop in jstypeliteral.jsdoc_property_tags {
                if prop.kind() != Kind::JSDocPropertyTag && prop.kind() != Kind::JSDocParameterTag {
                    continue;
                }
                let jsprop = prop.as_jsdoc_parameter_or_property_tag();
                let mut name = prop.name().unwrap();
                if name.kind() == Kind::QualifiedName {
                    name = name.as_qualified_name().right;
                }
                if ast::is_identifier(name)
                    && !scanner::is_valid_identifier(name.as_identifier().text())
                {
                    let literal = self
                        .factory
                        .new_string_literal(name.as_identifier().text(), TokenFlags::None);
                    name = self.add_transformed_reparse(literal, name);
                } else {
                    name = self.add_deep_clone_reparse(Some(name)).unwrap();
                }
                let question_token = self.make_question_if_optional(prop);
                let property = self.factory.new_property_signature_declaration(
                    None,
                    name,
                    question_token,
                    None,
                    None,
                );
                if let Some(type_expression) = jsprop.type_expression {
                    let property_type =
                        self.reparse_jsdoc_type_literal(type_expression.type_node());
                    property.as_property_signature_declaration().type_.set(property_type);
                }
                self.finish_reparsed_node(property, prop);
                properties.push(property);
                self.reparse_jsdoc_comment(property, prop);
            }
            let members = self.new_node_list(t.loc(), &properties);
            let mut result = self.factory.new_type_literal_node(members);
            if is_array_type {
                self.finish_reparsed_node(result, t);
                result = self.factory.new_array_type_node(result);
            }
            self.finish_reparsed_node(result, t);
            return Some(result);
        }
        self.add_deep_clone_reparse(Some(t))
    }

    fn reparse_jsdoc_comment(&mut self, node: P<Node>, tag: P<Node>) {
        if let Some(comment) = tag.comment_list() {
            let cloned: Vec<P<Node>> = comment
                .nodes()
                .iter()
                .map(|&n| self.reparse_cloner().clone_node(Some(n)).unwrap())
                .collect();
            let new_comment = self.factory.new_node_list(cloned);
            new_comment.loc.set(comment.loc.get());
            let prop_jsdoc = self.factory.new_jsdoc(new_comment, None);
            self.finish_reparsed_node(prop_jsdoc, tag);
            prop_jsdoc.set_parent(Some(node));
            self.jsdoc_infos.push(JSDocInfo { parent: node, js_docs: alloc_slice(&[prop_jsdoc]) });
            node.set_flags(node.flags() | NodeFlags::HasJSDoc);
        }
    }

    fn gather_type_parameters(
        &mut self,
        j: P<Node>,
        typedef_or_callback: bool,
    ) -> Option<P<NodeList>> {
        let mut type_parameters: Vec<P<Node>> = Vec::new();
        let mut pos = None;
        let mut end_pos = None;
        for &tag in j.as_jsdoc().tags.unwrap().nodes() {
            // When a JSDoc comment contains an `@typedef` or `@callback` tag, `@template` type parameter
            // declarations apply to the type being defined.
            if !typedef_or_callback
                && (ast::is_jsdoc_typedef_tag(tag) || ast::is_jsdoc_callback_tag(tag))
            {
                return None;
            }
            if !ast::is_jsdoc_template_tag(tag) {
                continue;
            }
            if pos.is_none() {
                pos = Some(tag.pos().expect_source("parsed nodes have source positions"));
            }
            end_pos = Some(tag.end().expect_source("parsed nodes have source positions"));
            let constraint = tag.as_jsdoc_template_tag().constraint;
            let mut first_type_parameter = true;
            for &tp in tag.type_parameters() {
                let reparse;
                if let Some(constraint) = constraint.filter(|_| first_type_parameter) {
                    let modifiers = self.reparse_cloner().clone_modifiers(tp.modifiers());
                    let checked = self.check_non_identifier_name(tp.name());
                    let name = self.add_deep_clone_reparse(checked).unwrap();
                    let constraint_type = self.add_deep_clone_reparse(constraint.type_node());
                    let default_type = self
                        .add_deep_clone_reparse(tp.as_type_parameter_declaration().default_type);
                    reparse = self.factory.new_type_parameter_declaration(
                        modifiers,
                        name,
                        constraint_type,
                        None, // expression
                        default_type,
                    );
                    self.finish_reparsed_node(reparse, tp);
                } else {
                    reparse = self.add_deep_clone_reparse(Some(tp)).unwrap();
                }
                type_parameters.push(reparse);
                first_type_parameter = false;
            }
        }
        if type_parameters.is_empty() {
            None
        } else {
            Some(
                self.new_node_list(
                    TextRange::new(pos.unwrap(), end_pos.unwrap()),
                    &type_parameters,
                ),
            )
        }
    }

    fn reparse_hosted(&mut self, tag: P<Node>, parent: P<Node>, js_doc: P<Node>) {
        let mut parent = parent;
        match tag.kind() {
            Kind::JSDocTypeTag => {
                match parent.kind() {
                    Kind::VariableStatement => {
                        if let Some(declaration_list) =
                            Some(parent.as_variable_statement().declaration_list)
                            && let Some(&declaration) = declaration_list
                                .as_variable_declaration_list()
                                .declarations
                                .nodes()
                                .iter()
                                .find(|declaration| {
                                    declaration.type_node().is_none()
                                        && tag.type_expression().is_some()
                                })
                        {
                            let t = self
                                .add_deep_clone_reparse(tag.type_expression().unwrap().type_node());
                            declaration.as_mutable().set_type(t);
                            self.finish_mutated_node(declaration);
                            return;
                        }
                    }
                    Kind::VariableDeclaration
                    | Kind::ExportAssignment
                    | Kind::PropertyDeclaration
                    | Kind::PropertyAssignment
                    | Kind::ShorthandPropertyAssignment
                    | Kind::GetAccessor => {
                        if parent.type_node().is_none() && tag.type_expression().is_some() {
                            let t = self
                                .add_deep_clone_reparse(tag.type_expression().unwrap().type_node());
                            parent.as_mutable().set_type(t);
                            self.finish_mutated_node(parent);
                            return;
                        }
                    }
                    Kind::Parameter => {
                        if parent.type_node().is_none() && tag.type_expression().is_some() {
                            let t = self.reparse_jsdoc_type_literal(
                                tag.type_expression().unwrap().type_node(),
                            );
                            parent.as_mutable().set_type(t);
                            self.finish_mutated_node(parent);
                            return;
                        }
                    }
                    Kind::ExpressionStatement => {
                        let expression = parent.expression().unwrap();
                        if expression.kind() == Kind::BinaryExpression {
                            let bin = expression;
                            let kind = ast::get_assignment_declaration_kind(bin);
                            if kind != ast::JSDeclarationKind::None
                                && tag.type_expression().is_some()
                            {
                                let t = self.add_deep_clone_reparse(
                                    tag.type_expression().unwrap().type_node(),
                                );
                                bin.as_mutable().set_type(t);
                                self.finish_mutated_node(bin);
                                return;
                            }
                        }
                    }
                    Kind::ReturnStatement | Kind::ParenthesizedExpression
                        if parent.expression().is_some() && tag.type_expression().is_some() =>
                    {
                        let t = self
                            .add_deep_clone_reparse(tag.type_expression().unwrap().type_node())
                            .unwrap();
                        let cast = self.make_new_cast(
                            t,
                            parent.expression().unwrap(),
                            true, /*isAssertion*/
                        );
                        parent.as_mutable().set_expression(cast);
                        self.finish_mutated_node(parent);
                        return;
                    }
                    _ => {}
                }
                if let Some(fun) = get_function_like_host(parent) {
                    let no_typed_params =
                        fun.parameters().iter().all(|param| param.type_node().is_none());
                    if fun.type_parameter_list().is_none()
                        && fun.type_node().is_none()
                        && no_typed_params
                        && tag.type_expression().is_some()
                    {
                        let t =
                            self.add_deep_clone_reparse(tag.type_expression().unwrap().type_node());
                        fun.function_like_data().unwrap().full_signature.set(t);
                        self.finish_mutated_node(fun);
                    }
                }
            }
            Kind::JSDocSatisfiesTag => match parent.kind() {
                Kind::VariableStatement => {
                    if let Some(declaration_list) =
                        Some(parent.as_variable_statement().declaration_list)
                        && let Some(declaration) = declaration_list
                            .as_variable_declaration_list()
                            .declarations
                            .nodes()
                            .iter()
                            .find_map(|&declaration| {
                                if declaration.initializer().is_some()
                                    && tag.type_expression().is_some()
                                {
                                    Some(declaration)
                                } else {
                                    None
                                }
                            })
                    {
                        let t = self
                            .add_deep_clone_reparse(tag.type_expression().unwrap().type_node())
                            .unwrap();
                        let cast = self.make_new_cast(
                            t,
                            declaration.initializer().unwrap(),
                            false, /*isAssertion*/
                        );
                        declaration.as_mutable().set_initializer(cast);
                        self.finish_mutated_node(declaration);
                    }
                }
                Kind::VariableDeclaration
                | Kind::PropertyDeclaration
                | Kind::PropertyAssignment => {
                    if parent.initializer().is_some() && tag.type_expression().is_some() {
                        let t = self
                            .add_deep_clone_reparse(tag.type_expression().unwrap().type_node())
                            .unwrap();
                        let cast = self.make_new_cast(
                            t,
                            parent.initializer().unwrap(),
                            false, /*isAssertion*/
                        );
                        parent.as_mutable().set_initializer(cast);
                        self.finish_mutated_node(parent);
                    }
                }
                Kind::ShorthandPropertyAssignment => {
                    let shorthand = parent.as_shorthand_property_assignment();
                    if let (Some(initializer), Some(type_expression)) = (
                        shorthand.object_assignment_initializer.get(),
                        Some(tag.as_jsdoc_satisfies_tag().type_expression),
                    ) {
                        let t = self.add_deep_clone_reparse(type_expression.type_node()).unwrap();
                        let cast = self.make_new_cast(t, initializer, false /*isAssertion*/);
                        shorthand.object_assignment_initializer.set(Some(cast));
                        self.finish_mutated_node(parent);
                    }
                }
                Kind::ReturnStatement | Kind::ParenthesizedExpression | Kind::ExportAssignment => {
                    if parent.expression().is_some() && tag.type_expression().is_some() {
                        let t = self
                            .add_deep_clone_reparse(tag.type_expression().unwrap().type_node())
                            .unwrap();
                        let cast = self.make_new_cast(
                            t,
                            parent.expression().unwrap(),
                            false, /*isAssertion*/
                        );
                        parent.as_mutable().set_expression(cast);
                        self.finish_mutated_node(parent);
                    }
                }
                Kind::ExpressionStatement => {
                    let expression = parent.expression().unwrap();
                    if expression.kind() == Kind::BinaryExpression {
                        let bin = expression.as_binary_expression();
                        let kind = ast::get_assignment_declaration_kind(expression);
                        if kind != ast::JSDeclarationKind::None && tag.type_expression().is_some() {
                            let t = self
                                .add_deep_clone_reparse(tag.type_expression().unwrap().type_node())
                                .unwrap();
                            let cast =
                                self.make_new_cast(t, bin.right.get(), false /*isAssertion*/);
                            bin.right.set(cast);
                            self.finish_mutated_node(expression);
                        }
                    }
                }
                _ => {}
            },
            Kind::JSDocTemplateTag => {
                if let Some(fun) = get_function_like_host(parent) {
                    if fun.type_parameter_list().is_none()
                        && fun.function_like_data().unwrap().full_signature.get().is_none()
                    {
                        let type_parameters =
                            self.gather_type_parameters(js_doc, false /*typedefOrCallback*/);
                        fun.function_like_data().unwrap().type_parameters.set(type_parameters);
                        self.finish_mutated_node(fun);
                    }
                } else if parent.kind() == Kind::ClassDeclaration
                    || parent.kind() == Kind::ClassExpression
                {
                    let class = parent.class_like_data().unwrap();
                    if class.type_parameters.get().is_none() {
                        let type_parameters =
                            self.gather_type_parameters(js_doc, false /*typedefOrCallback*/);
                        class.type_parameters.set(type_parameters);
                        self.finish_mutated_node(parent);
                    }
                }
            }
            Kind::JSDocParameterTag => {
                if let Some(fun) = get_function_like_host(parent)
                    .filter(|fun| fun.function_like_data().unwrap().full_signature.get().is_none())
                    && let Some(param) = find_matching_parameter(fun, tag, js_doc)
                {
                    let parameter_tag = tag.as_jsdoc_parameter_or_property_tag();
                    let param_data = param.as_parameter_declaration();
                    if param_data.type_.get().is_none()
                        && let Some(type_expression) = parameter_tag.type_expression
                    {
                        let t = self.reparse_jsdoc_type_literal(type_expression.type_node());
                        param_data.type_.set(t);
                    }
                    if param_data.question_token.get().is_none()
                        && let Some(question) = self.make_question_if_optional(tag)
                    {
                        param_data.question_token.set(Some(question));
                    }
                    self.finish_mutated_node(param);
                }
            }
            Kind::JSDocThisTag => {
                if let Some(fun) = get_function_like_host(parent) {
                    let params = fun.parameters();
                    if params.is_empty()
                        || (params[0].name().unwrap().kind() != Kind::ThisKeyword
                            && !ast::is_this_identifier(params[0].name().unwrap()))
                    {
                        let this_identifier = self.factory.new_identifier("this");
                        let this_param = self.factory.new_parameter_declaration(
                            None, /* decorators */
                            None, /* modifiers */
                            this_identifier,
                            None, /* questionToken */
                            None, /* type */
                            None, /* initializer */
                        );
                        if let Some(type_expression) = Some(tag.as_jsdoc_this_tag().type_expression)
                        {
                            let t = self.add_deep_clone_reparse(type_expression.type_node());
                            this_param.as_parameter_declaration().type_.set(t);
                        }
                        self.finish_reparsed_node(this_param, tag.tag_name());

                        let mut new_params: Vec<P<Node>> = Vec::with_capacity(params.len() + 1);
                        new_params.push(this_param);
                        new_params.extend_from_slice(params);

                        let loc = fun.parameter_list().unwrap().loc();
                        let parameter_list = self.new_node_list(loc, &new_params);
                        fun.function_like_data().unwrap().parameters.set(Some(parameter_list));
                        self.finish_mutated_node(fun);
                    }
                }
            }
            Kind::JSDocReturnTag => {
                if let Some(fun) = get_function_like_host(parent)
                    .filter(|fun| fun.function_like_data().unwrap().full_signature.get().is_none())
                    && fun.type_node().is_none()
                    && tag.type_expression().is_some()
                {
                    let t = self.add_deep_clone_reparse(tag.type_expression().unwrap().type_node());
                    fun.function_like_data().unwrap().type_.set(t);
                    self.finish_mutated_node(fun);
                }
            }
            Kind::JSDocReadonlyTag
            | Kind::JSDocPrivateTag
            | Kind::JSDocPublicTag
            | Kind::JSDocProtectedTag
            | Kind::JSDocOverrideTag => {
                if parent.kind() == Kind::ExpressionStatement {
                    parent = parent.expression().unwrap();
                }
                let applies = match parent.kind() {
                    // In object literals these aren't class-like members, so JSDoc modifiers like @override
                    // or @readonly aren't real modifiers there; reparsing them produces spurious grammar errors (#4437).
                    Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => {
                        if self.parsing_contexts
                            & (1 << ParsingContext::ObjectLiteralMembers as u32)
                            != 0
                        {
                            return;
                        }
                        true
                    }
                    Kind::PropertyDeclaration | Kind::Constructor | Kind::BinaryExpression => true,
                    _ => false,
                };
                if applies {
                    let keyword = match tag.kind() {
                        Kind::JSDocReadonlyTag => Kind::ReadonlyKeyword,
                        Kind::JSDocPrivateTag => Kind::PrivateKeyword,
                        Kind::JSDocPublicTag => Kind::PublicKeyword,
                        Kind::JSDocProtectedTag => Kind::ProtectedKeyword,
                        Kind::JSDocOverrideTag => Kind::OverrideKeyword,
                        _ => unreachable!(),
                    };
                    let modifier = self.factory.new_modifier(keyword);
                    modifier.set_loc(tag.loc());
                    modifier.set_flags(self.context_flags | NodeFlags::Reparsed);
                    let nodes: Vec<P<Node>>;
                    let loc;
                    match parent.modifiers() {
                        None => {
                            nodes = vec![modifier];
                            loc = tag.loc();
                        }
                        Some(modifiers) => {
                            let mut n = parent.modifier_nodes().to_vec();
                            n.push(modifier);
                            nodes = n;
                            loc = modifiers.loc();
                        }
                    }
                    let modifier_list = self.new_modifier_list(loc, &nodes);
                    parent.as_mutable().set_modifiers(Some(modifier_list));
                    self.finish_mutated_node(parent);
                }
            }
            Kind::JSDocImplementsTag => {
                if let Some(class) = get_class_like_data(parent) {
                    let implements_tag = tag.as_jsdoc_implements_tag();

                    if let Some(heritage_clauses) = class.heritage_clauses.get()
                        && let Some(implements_clause) =
                            heritage_clauses.nodes().iter().copied().find(|node| {
                                node.as_heritage_clause().token == Kind::ImplementsKeyword
                            })
                    {
                        let heritage_clause = implements_clause.as_heritage_clause();
                        let types = heritage_clause.types();
                        let clone =
                            self.add_deep_clone_reparse(Some(implements_tag.class_name)).unwrap();
                        let mut nodes = types.nodes().to_vec();
                        nodes.push(clone);
                        let new_types = self.new_node_list(types.loc(), &nodes);
                        heritage_clause.set_types(new_types);
                        self.finish_mutated_node(implements_clause);
                        return;
                    }
                    let clone =
                        self.add_deep_clone_reparse(Some(implements_tag.class_name)).unwrap();
                    let types_list = self.new_node_list(implements_tag.class_name.loc(), &[clone]);

                    let heritage_clause =
                        self.factory.new_heritage_clause(Kind::ImplementsKeyword, types_list);
                    self.finish_reparsed_node(heritage_clause, implements_tag.class_name);

                    match class.heritage_clauses.get() {
                        None => {
                            let heritage_clauses = self
                                .new_node_list(implements_tag.class_name.loc(), &[heritage_clause]);
                            class.heritage_clauses.set(Some(heritage_clauses));
                        }
                        Some(heritage_clauses) => {
                            let mut nodes = heritage_clauses.nodes().to_vec();
                            nodes.push(heritage_clause);
                            let new_list = self.new_node_list(heritage_clauses.loc(), &nodes);
                            class.heritage_clauses.set(Some(new_list));
                        }
                    }
                    self.finish_mutated_node(parent);
                }
            }
            Kind::JSDocAugmentsTag => {
                if let Some(class) = get_class_like_data(parent)
                    && let Some(heritage_clauses) = class.heritage_clauses.get()
                    && let Some(extends_clause) = heritage_clauses
                        .nodes()
                        .iter()
                        .copied()
                        .find(|node| node.as_heritage_clause().token == Kind::ExtendsKeyword)
                    && extends_clause.as_heritage_clause().types().nodes().len() == 1
                {
                    let target_node = extends_clause.as_heritage_clause().types().nodes()[0];
                    let target = target_node.as_expression_with_type_arguments();
                    let source = tag.class_name().as_expression_with_type_arguments();
                    if target.type_arguments.get().is_none()
                        && let Some(source_type_arguments) = source.type_arguments.get()
                        && target.expression.kind() != Kind::NullKeyword
                        && (!ast::is_entity_name_expression(target.expression)
                            || ast::has_same_property_access_name(
                                target.expression,
                                source.expression,
                            ))
                    {
                        self.set_reparsed_type_arguments(target_node, source_type_arguments);
                    }
                }
            }
            _ => {}
        }
    }

    fn set_reparsed_type_arguments(&mut self, target: P<Node>, source: P<NodeList>) {
        let type_arguments: Vec<P<Node>> = source
            .nodes()
            .iter()
            .map(|&argument| self.add_deep_clone_reparse(Some(argument)).unwrap())
            .collect();
        let list = self.new_node_list(source.loc.get(), &type_arguments);
        target.as_expression_with_type_arguments().type_arguments.set(Some(list));
        self.finish_mutated_node(target);
    }

    fn make_question_if_optional(&self, parameter: P<Node>) -> Option<P<Node>> {
        let data = parameter.as_jsdoc_parameter_or_property_tag();
        let mut question_token = None;
        if data.is_bracketed
            || data
                .type_expression
                .is_some_and(|t| t.type_node().unwrap().kind() == Kind::JSDocOptionalType)
        {
            let token = self.factory.new_token(Kind::QuestionToken);
            token.set_loc(parameter.loc());
            token.set_flags(self.context_flags | NodeFlags::Reparsed);
            question_token = Some(token);
        }
        question_token
    }

    fn make_new_cast(&mut self, t: P<Node>, e: P<Node>, is_assertion: bool) -> P<Node> {
        let assert = if is_assertion {
            self.factory.new_as_expression(e, t)
        } else {
            self.factory.new_satisfies_expression(e, t)
        };
        self.finish_node_with_end(
            assert,
            e.pos().expect_source("parsed nodes have source positions"),
            e.end().expect_source("parsed nodes have source positions"),
        );
        assert
    }

    fn create_export_modifier(&self, location_node: P<Node>) -> P<ModifierList> {
        let export_modifier = self.factory.new_modifier(Kind::ExportKeyword);
        export_modifier.set_loc(location_node.loc());
        export_modifier.set_flags(self.context_flags | NodeFlags::Reparsed);
        self.new_modifier_list(location_node.loc(), &[export_modifier])
    }

    // getInnermostNameOfJSDocNamespace returns the innermost identifier from a
    // JSDoc namespace chain (ModuleDeclaration). For a simple identifier, it returns
    // the identifier itself. For "A.B.C", it returns the identifier "C".
    fn get_innermost_name_of_jsdoc_namespace(full_name: Option<P<Node>>) -> Option<P<Node>> {
        let mut full_name = full_name?;
        while full_name.kind() == Kind::ModuleDeclaration {
            let Some(body) = full_name.body() else {
                return full_name.name();
            };
            full_name = body;
        }
        Some(full_name)
    }

    // wrapInJSDocNamespace wraps a statement (typically a type alias) in namespace
    // declarations corresponding to a JSDoc dotted name. For example, given name
    // "A.B.C" and a type alias for C, this produces:
    //
    //	namespace A { namespace B { type C = ... } }
    //
    // If the name is a simple identifier (not a ModuleDeclaration), it returns the
    // statement as-is.
    fn wrap_in_jsdoc_namespace(
        &mut self,
        full_name: Option<P<Node>>,
        statement: P<Node>,
        nested: bool,
    ) -> P<Node> {
        let Some(full_name) = full_name.filter(|n| ast::is_module_declaration(*n)) else {
            return statement;
        };
        // Recursively wrap from outermost to innermost. Inner namespaces always get an export modifier
        // so members are accessible via dotted access from outside. The outermost namespace is treated as
        // exported only in module files via IsImplicitlyExportedJSDocDeclaration (in the binder), so it
        // does not get an explicit export modifier here.
        let wrapped =
            self.wrap_in_jsdoc_namespace(full_name.body(), statement, true /*nested*/);
        let statements = self.new_node_list(full_name.loc(), &[wrapped]);
        let block = self.factory.new_module_block(statements);
        self.finish_reparsed_node(block, full_name);
        let mut modifiers = None;
        if nested {
            modifiers = Some(self.create_export_modifier(full_name));
        }
        let name = self.add_deep_clone_reparse(full_name.name()).unwrap();
        let result = self.factory.new_module_declaration(
            modifiers,
            Kind::NamespaceKeyword,
            name,
            None,
            Some(block),
        );
        self.finish_reparsed_node(result, full_name);
        self.reparsed_clones.push(result);
        result
    }
}

fn find_matching_parameter(
    fun: P<Node>,
    parameter_tag: P<Node>,
    js_doc: P<Node>,
) -> Option<P<Node>> {
    let mut tag_index = None;
    let mut param_count = 0;
    for &tag in js_doc.as_jsdoc().tags.unwrap().nodes() {
        if tag.kind() == Kind::JSDocParameterTag {
            if tag == parameter_tag {
                tag_index = Some(param_count);
                break;
            }
            param_count += 1;
        }
    }
    if let Some(result) =
        fun.parameters().iter().enumerate().find_map(|(parameter_index, &parameter)| {
            if parameter.name().unwrap().kind() == Kind::Identifier {
                let tag_name = parameter_tag.name().unwrap();
                if tag_name.kind() == Kind::Identifier
                    && ((parameter.name().unwrap().text() == tag_name.text())
                        || (Some(parameter_index) == tag_index && tag_name.text().is_empty()))
                {
                    return Some(Some(parameter));
                }
            } else if Some(parameter_index) == tag_index {
                return Some(Some(parameter));
            }

            None
        })
    {
        return result;
    }
    None
}

fn skip_satisfies_expressions(node: Option<P<Node>>) -> Option<P<Node>> {
    let mut node = node;
    while let Some(n) = node {
        if n.kind() != Kind::SatisfiesExpression {
            break;
        }
        node = n.expression();
    }
    node
}

fn get_function_like_host(host: P<Node>) -> Option<P<Node>> {
    let mut fun = Some(host);
    match host.kind() {
        Kind::VariableStatement => {
            let nodes = host
                .as_variable_statement()
                .declaration_list
                .as_variable_declaration_list()
                .declarations
                .nodes();
            if !nodes.is_empty() {
                fun = nodes[0].initializer();
            }
        }
        Kind::PropertyAssignment | Kind::PropertyDeclaration => {
            fun = host.initializer();
        }
        Kind::ExportAssignment | Kind::ReturnStatement => {
            fun = host.expression();
        }
        Kind::ExpressionStatement => {
            fun = Some(ast::get_right_most_assigned_expression(host.expression().unwrap()));
        }
        _ => {}
    }
    fun = skip_satisfies_expressions(fun);
    if ast::is_function_like(fun) {
        return fun;
    }
    None
}

fn get_class_like_data(parent: P<Node>) -> Option<&'static ClassLikeBase> {
    let mut class = None;
    match parent.kind() {
        Kind::ClassDeclaration | Kind::ClassExpression => {
            class = parent.class_like_data();
        }
        _ => {}
    }
    class
}
