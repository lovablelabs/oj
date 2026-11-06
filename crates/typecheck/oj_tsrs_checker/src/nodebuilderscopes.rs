use crate::*;
use tsrs_ast as ast;

// Non-function declarations in nodebuilderscopes.go (hand-ported by printer-foundation in nodebuilder_types.rs):
//   type localsRecord (nodebuilderscopes.go:35)

// nodebuilderscopes.go:10
pub fn clone_node_builder_context(context: P<NodeBuilderContext>) -> Box<dyn FnMut()> {
    // Make type parameters created within this context not consume the name outside this context
    // The symbol serializer ends up creating many sibling scopes that all need "separate" contexts when
    // it comes to naming things - within a normal `typeToTypeNode` call, the node builder only ever descends
    // through the type tree, so the only cases where we could have used distinct sibling scopes was when there
    // were multiple generic overloads with similar generated type parameter names
    // The effect:
    // When we write out
    // export const x: <T>(x: T) => T
    // export const y: <T>(x: T) => T
    // we write it out like that, rather than as
    // export const x: <T>(x: T) => T
    // export const y: <T_1>(x: T_1) => T_1
    let restore_names = context.type_parameter_names.borrow_mut().enter_scope();
    let restore_names_by_text = context.type_parameter_names_by_text.borrow_mut().enter_scope();
    let restore_names_by_text_next_name_count =
        context.type_parameter_names_by_text_next_name_count.borrow_mut().enter_scope();
    let restore_symbol_list = context.type_parameter_symbol_list.borrow_mut().enter_scope();
    let mut scopes = Some((
        restore_names,
        restore_names_by_text,
        restore_names_by_text_next_name_count,
        restore_symbol_list,
    ));
    Box::new(move || {
        if let Some((
            restore_names,
            restore_names_by_text,
            restore_names_by_text_next_name_count,
            restore_symbol_list,
        )) = scopes.take()
        {
            context.type_parameter_names.borrow_mut().exit_scope(restore_names);
            context.type_parameter_names_by_text.borrow_mut().exit_scope(restore_names_by_text);
            context
                .type_parameter_names_by_text_next_name_count
                .borrow_mut()
                .exit_scope(restore_names_by_text_next_name_count);
            context.type_parameter_symbol_list.borrow_mut().exit_scope(restore_symbol_list);
        }
    })
}

impl NodeBuilderImpl {
    // nodebuilderscopes.go:40
    pub(crate) fn add_symbol_type_to_context(
        &self,
        _c: &mut Checker,
        symbol: P<Symbol>,
        t: P<Type>,
    ) -> Box<dyn FnMut(&mut Checker)> {
        let id = ast::get_symbol_id(symbol);
        let old_type = self.ctx().enclosing_symbol_types.borrow().get(&id).copied();
        self.ctx().enclosing_symbol_types.borrow_mut().insert(id, t);
        let b = self.as_p();
        Box::new(move |_c: &mut Checker| {
            if let Some(old_type) = old_type {
                b.ctx().enclosing_symbol_types.borrow_mut().insert(id, old_type);
            } else {
                b.ctx().enclosing_symbol_types.borrow_mut().remove(&id);
            }
        })
    }

    // nodebuilderscopes.go:53
    pub(crate) fn enter_signature_scope(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
    ) -> (Vec<P<Symbol>>, Box<dyn FnMut(&mut Checker)>) {
        let expanded_params =
            c.get_expanded_parameters(signature, true /*skipUnionExpanding*/).swap_remove(0);
        let cleanup = self.enter_new_scope(
            c,
            signature.declaration(),
            &expanded_params,
            signature.type_parameters(),
            Some(signature.parameters()),
            signature.mapper.get(),
        );
        (expanded_params, cleanup)
    }

    // nodebuilderscopes.go:59
    pub(crate) fn enter_new_scope(
        &self,
        c: &mut Checker,
        declaration: Option<P<Node>>,
        expanded_params: &[P<Symbol>],
        type_parameters: &[P<Type>],
        original_parameters: Option<&[P<Symbol>]>,
        mapper: Option<P<TypeMapper>>,
    ) -> Box<dyn FnMut(&mut Checker)> {
        let mut cleanup_context = clone_node_builder_context(self.ctx());
        // For regular function/method declarations, the enclosing declaration will already be signature.declaration,
        // so this is a no-op, but for arrow functions and function expressions, the enclosing declaration will be
        // the declaration that the arrow function / function expression is assigned to.
        //
        // If the parameters or return type include "typeof globalThis.paramName", using the wrong scope will lead
        // us to believe that we can emit "typeof paramName" instead, even though that would refer to the parameter,
        // not the global. Make sure we are in the right scope by changing the enclosingDeclaration to the function.
        //
        // We can't use the declaration directly; it may be in another file and so we may lose access to symbols
        // accessible to the current enclosing declaration, or gain access to symbols not accessible to the current
        // enclosing declaration. To keep this chain accurate, insert a fake scope into the chain which makes the
        // function's parameters visible.
        let mut cleanup_params: Option<Box<dyn FnMut()>> = None;
        let mut cleanup_type_params: Option<Box<dyn FnMut()>> = None;
        let old_enclosing_decl = self.ctx().enclosing_declaration.get();
        let old_mapper = self.ctx().mapper.get();
        if mapper.is_some() {
            self.ctx().mapper.set(mapper);
        }
        if self.ctx().enclosing_declaration.get().is_some() && declaration.is_some() {
            // As a performance optimization, reuse the same fake scope within this chain.
            // This is especially needed when we are working on an excessively deep type;
            // if we don't do this, then we spend all of our time adding more and more
            // scopes that need to be searched in isSymbolAccessible later. Since all we
            // really want to do is to mark certain names as unavailable, we can just keep
            // all of the names we're introducing in one large table and push/pop from it as
            // needed; isSymbolAccessible will walk upward and find the closest "fake" scope,
            // which will conveniently report on any and all faked scopes in the chain.
            //
            // It'd likely be better to store this somewhere else for isSymbolAccessible, but
            // since that API _only_ uses the enclosing declaration (and its parents), this is
            // seems like the best way to inject names into that search process.
            //
            // Note that we only check the most immediate enclosingDeclaration; the only place we
            // could potentially add another fake scope into the chain is right here, so we don't
            // traverse all ancestors.

            if expanded_params.is_empty() {
                cleanup_params = None;
            } else {
                cleanup_params = push_fake_scope(self, c, "params", &mut |b, c, add| {
                    for (p_index, &param) in expanded_params.iter().enumerate() {
                        let original_param =
                            original_parameters.and_then(|o| o.get(p_index).copied());
                        if original_parameters.is_some() && original_param != Some(param) {
                            // Can't reference the expanded parameter name, just the original, unless we've expanded the param list for some reason
                            if let Some(original_param) = original_param {
                                add(original_param.name(), original_param);
                            }
                        } else {
                            let declarations = param.declarations().to_vec();
                            let bound = declarations.iter().any(|&d| {
                                if ast::is_parameter_declaration(d)
                                    && d.name().is_some()
                                    && ast::is_binding_pattern(d.name().unwrap())
                                {
                                    bind_pattern(b, c, d.name().unwrap(), add);
                                    return true;
                                }
                                false
                            });
                            if !bound {
                                add(param.name(), param);
                            }
                        }
                    }
                });
            }

            if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
                && !type_parameters.is_empty()
            {
                cleanup_type_params = push_fake_scope(self, c, "typeParams", &mut |b, c, add| {
                    for &type_param in type_parameters {
                        let type_param_name = b.type_parameter_to_name(c, type_param).text();
                        add(type_param_name, type_param.symbol().unwrap());
                    }
                });
            }
        }

        let b = self.as_p();
        Box::new(move |_c: &mut Checker| {
            if let Some(cleanup_params) = cleanup_params.as_mut() {
                cleanup_params();
            }
            if let Some(cleanup_type_params) = cleanup_type_params.as_mut() {
                cleanup_type_params();
            }
            cleanup_context();
            b.ctx().enclosing_declaration.set(old_enclosing_decl);
            b.ctx().mapper.set(old_mapper);
        })
    }
}

type AddSymbol<'a> = &'a mut dyn FnMut(&'static str, P<Symbol>);

// Go's `pushFakeScope` closure in enterNewScope.
fn push_fake_scope(
    b: &NodeBuilderImpl,
    c: &mut Checker,
    kind: &'static str,
    add_all: &mut dyn FnMut(&NodeBuilderImpl, &mut Checker, AddSymbol),
) -> Option<Box<dyn FnMut()>> {
    // We only ever need to look two declarations upward.
    let enclosing_declaration = b.ctx().enclosing_declaration.get();
    assert!(enclosing_declaration.is_some());
    let enclosing_declaration = enclosing_declaration.unwrap();
    let mut existing_fake_scope: Option<P<Node>> = None;
    if b.links.has(enclosing_declaration) {
        let links = b.links.get(enclosing_declaration);
        if links.fake_scope_for_signature_declaration.get() == Some(kind) {
            existing_fake_scope = Some(enclosing_declaration);
        }
    }
    if existing_fake_scope.is_none()
        && let Some(parent) = enclosing_declaration.parent()
        && b.links.has(parent)
    {
        let links = b.links.get(parent);
        if links.fake_scope_for_signature_declaration.get() == Some(kind) {
            existing_fake_scope = Some(parent);
        }
    }
    assert!(existing_fake_scope.is_none() || ast::is_block(existing_fake_scope.unwrap()));

    let mut locals: Option<P<SymbolTable>> = None;
    if let Some(existing_fake_scope) = existing_fake_scope {
        locals = existing_fake_scope.locals();
    }
    let locals = match locals {
        Some(locals) => locals,
        None => SymbolTable::new(),
    };
    let mut new_locals: Vec<String> = vec![];
    let mut old_locals: Vec<LocalsRecord> = vec![];
    add_all(b, c, &mut |name: &'static str, symbol: P<Symbol>| {
        // Add cleanup information only if we don't own the fake scope
        if existing_fake_scope.is_some() {
            match locals.lookup(name) {
                None => new_locals.push(name.to_string()),
                Some(old_symbol) => {
                    old_locals.push(LocalsRecord { name: name.to_string(), old_symbol });
                }
            }
        }
        locals.set(name, symbol);
    });

    if existing_fake_scope.is_none() {
        // Use a Block for this; the type of the node doesn't matter so long as it
        // has locals, and this is cheaper/easier than using a function-ish Node.
        let fake_scope = b.f.new_block(b.f.new_node_list(vec![]), false);
        b.links.get(fake_scope).fake_scope_for_signature_declaration.set(Some(kind));
        let data = fake_scope.locals_container_data().unwrap();
        data.locals.set(Some(locals));
        fake_scope.set_parent(Some(enclosing_declaration));
        b.ctx().enclosing_declaration.set(Some(fake_scope));
        None
    } else {
        // We did not create the current scope, so we have to clean it up
        Some(Box::new(move || {
            for s in &new_locals {
                locals.delete(s);
            }
            for s in &old_locals {
                locals.set(alloc_str(&s.name), s.old_symbol);
            }
        }))
    }
}

// Go's `bindPatternWorker` closure in enterNewScope.
// Go's bindPatternWorker returns after the first element too
fn bind_pattern(b: &NodeBuilderImpl, c: &mut Checker, p: P<Node>, add: AddSymbol) {
    if let Some(&e) = p.elements().first() {
        match e.kind() {
            Kind::OmittedExpression => (),
            Kind::BindingElement => {
                bind_element(b, c, e, add);
            }
            _ => panic!("Unhandled binding element kind"),
        }
    }
}

// Go's `bindElementWorker` closure in enterNewScope.
fn bind_element(b: &NodeBuilderImpl, c: &mut Checker, e: P<Node>, add: AddSymbol) {
    if e.name().is_some() && ast::is_binding_pattern(e.name().unwrap()) {
        bind_pattern(b, c, e.name().unwrap(), add);
        return;
    }
    let symbol = c.get_symbol_of_declaration(e);
    if let Some(symbol) = symbol {
        // omitted expressions are now parsed as nameless binding patterns and also have no symbol
        add(symbol.name(), symbol);
    }
}
