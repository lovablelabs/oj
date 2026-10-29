use crate::*;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:30510-end (hand-ported by checker-foundation):
//   type ObjectLiteralDiscriminator (checker.go:31202)

impl Checker {
    // In a contextually typed conditional expression, the true/false expressions are contextually typed by the same type.
    // checker.go:30511
    pub(crate) fn get_contextual_type_for_conditional_operand(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let parent = node.parent().unwrap();
        let conditional = parent.as_conditional_expression();
        if node == conditional.when_true || node == conditional.when_false {
            return self.get_contextual_type(parent, context_flags);
        }
        None
    }

    // checker.go:30519
    pub(crate) fn get_contextual_type_for_substitution_expression(
        &mut self,
        template: P<Node>,
        substitution_expression: P<Node>,
    ) -> Option<P<Type>> {
        let parent = template.parent().unwrap();
        if ast::is_tagged_template_expression(parent) {
            return self.get_contextual_type_for_argument(parent, substitution_expression);
        }
        None
    }

    // checker.go:30526
    pub(crate) fn get_contextual_import_attribute_type(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Type>> {
        let t = self.get_global_import_attributes_type();
        self.get_type_of_property_of_contextual_type(t, node.name().unwrap().text())
    }

    // Returns the effective arguments for an expression that works like a function invocation.
    // checker.go:30531
    pub(crate) fn get_effective_call_arguments(&mut self, node: P<Node>) -> Vec<P<Node>> {
        if ast::is_jsx_opening_fragment(node) {
            // This attributes Type does not include a children property yet, the same way a fragment created with <React.Fragment> does not at this stage
            let t = self.empty_fresh_jsx_object_type;
            return vec![self.create_synthetic_expression(node, t, false, None)];
        } else if ast::is_tagged_template_expression(node) {
            let template = node.as_tagged_template_expression().template;
            let t = self.get_global_template_strings_array_type();
            let first_arg = self.create_synthetic_expression(template, t, false, None);
            if !ast::is_template_expression(template) {
                return vec![first_arg];
            }
            let spans = template.as_template_expression().template_spans.nodes();
            let mut args = Vec::with_capacity(spans.len() + 1);
            args.push(first_arg);
            args.extend(spans.iter().map(|span| span.expression().unwrap()));
            return args;
        } else if ast::is_decorator(node) {
            return self.get_effective_decorator_arguments(node);
        } else if ast::is_binary_expression(node) {
            // Handles instanceof operator
            return vec![node.as_binary_expression().left];
        } else if ast::is_jsx_opening_like_element(node) {
            if !node.attributes().unwrap().properties().is_empty()
                || (ast::is_jsx_opening_element(node)
                    && !node.parent().unwrap().children().nodes().is_empty())
            {
                return vec![node.attributes().unwrap()];
            }
            return Vec::new();
        }
        let args = node.arguments();
        let spread_index = Self::get_spread_argument_index(args);
        if let Some(spread_index) = spread_index {
            // Create synthetic arguments from spreads of tuple types.
            let mut effective_args = args[..spread_index].to_vec();
            for (_i, element) in args.iter().enumerate().skip(spread_index) {
                let arg = *element;
                let mut spread_type = None;
                // We can call checkExpressionCached because spread expressions never have a contextual type.
                if ast::is_spread_element(arg) {
                    if self.flow_loop_stack.is_empty() {
                        spread_type = Some(self.check_expression_cached(arg.expression().unwrap()));
                    } else {
                        spread_type = Some(self.check_expression(arg.expression().unwrap()));
                    }
                }
                if let Some(spread_type) = spread_type.filter(|&t| is_tuple_type(t)) {
                    effective_args.extend(
                        self.get_element_types(spread_type).iter().copied().enumerate().map(
                            |(i, t)| {
                                let element_infos = spread_type.target_tuple_type().element_infos();
                                let flags = element_infos[i].flags;
                                let mut synthetic_type = t;
                                if flags.intersects(ElementFlags::Rest) {
                                    synthetic_type = self.create_array_type(t);
                                }

                                self.create_synthetic_expression(
                                    arg,
                                    synthetic_type,
                                    flags.intersects(ElementFlags::Variable),
                                    element_infos[i].labeled_declaration,
                                )
                            },
                        ),
                    );
                } else {
                    effective_args.push(arg);
                }
            }
            return effective_args;
        }
        args.to_vec()
    }

    // checker.go:30597
    pub(crate) fn get_spread_argument_index(args: &[P<Node>]) -> Option<usize> {
        find_index(args, |&arg| is_spread_argument(arg))
    }
}

// checker.go:30601
pub(crate) fn is_spread_argument(arg: P<Node>) -> bool {
    ast::is_spread_element(arg)
        || ast::is_synthetic_expression(arg) && arg.as_synthetic_expression().is_spread
}

// Go `node.AsSyntheticExpression().Type.(*Type)`. The Go `any` field holds the checker type as a `&'static P<Type>`;
// `create_synthetic_expression` is the only writer.
pub(crate) fn synthetic_expression_type(node: P<Node>) -> P<Type> {
    *node
        .as_synthetic_expression()
        .type_
        .downcast_ref::<P<Type>>()
        .expect("SyntheticExpression.Type must hold a checker *Type")
}

impl Checker {
    // checker.go:30605
    pub(crate) fn create_synthetic_expression(
        &self,
        parent: P<Node>,
        t: P<Type>,
        is_spread: bool,
        tuple_name_source: Option<P<Node>>,
    ) -> P<Node> {
        let result = self.factory.new_synthetic_expression(alloc(t), is_spread, tuple_name_source);
        result.set_loc(parent.loc());
        result.set_parent(Some(parent));
        result
    }

    // checker.go:30612
    pub(crate) fn get_spread_indices(&mut self, node: P<Node>) -> (Option<usize>, Option<usize>) {
        let links = self.array_literal_links.get(node);
        if !links.indices_computed.get() {
            let (mut first, mut last) = (None, None);
            for (i, &element) in node.elements().iter().enumerate() {
                if ast::is_spread_element(element) {
                    if first.is_none() {
                        first = Some(i);
                    }
                    last = Some(i);
                }
            }
            links.first_spread_index.set(first);
            links.last_spread_index.set(last);
            links.indices_computed.set(true);
        }
        (links.first_spread_index.get(), links.last_spread_index.get())
    }

    // Returns the synthetic argument list for a decorator invocation.
    // checker.go:30631
    fn get_effective_decorator_arguments(&mut self, node: P<Node>) -> Vec<P<Node>> {
        let expr = node.expression().unwrap();
        let signature = self.get_decorator_call_signature(node);
        if let Some(signature) = signature {
            let params = signature.parameters.get();
            let mut args = Vec::with_capacity(params.len());
            args.extend(params.iter().map(|&param| {
                let t = self.get_type_of_symbol(param);
                self.create_synthetic_expression(expr, t, false, None)
            }));
            return args;
        }
        panic!("Decorator signature not found")
    }

    // checker.go:30644
    pub(crate) fn get_decorator_call_signature(
        &mut self,
        decorator: P<Node>,
    ) -> Option<P<Signature>> {
        if self.legacy_decorators {
            return self.get_legacy_decorator_call_signature(decorator);
        }
        self.get_es_decorator_call_signature(decorator)
    }

    // checker.go:30651
    fn get_legacy_decorator_call_signature(&mut self, decorator: P<Node>) -> Option<P<Signature>> {
        let node = decorator.parent().unwrap();
        let links = self.signature_links.get(node);
        let decorator_signature = links.decorator_signature(node);
        if decorator_signature.get().is_none() {
            decorator_signature.set(Some(self.any_signature));
            match node.kind() {
                Kind::ClassDeclaration | Kind::ClassExpression => {
                    // For a class decorator, the `target` is the type of the class (e.g. the
                    // "static" or "constructor" side of the class).
                    let sym = self.get_symbol_of_declaration(node).unwrap();
                    let target_type = self.get_type_of_symbol(sym);
                    let target_param = self.new_parameter("target", target_type);
                    let void_type = self.void_type;
                    let return_type = self.get_union_type(&[target_type, void_type]);
                    let sig = self.new_call_signature(&[], None, &[target_param], return_type);
                    decorator_signature.set(Some(sig));
                }
                Kind::Parameter => 'case: {
                    let parent = node.parent().unwrap();
                    if !(ast::is_constructor_declaration(parent)
                        || ast::is_method_declaration(parent)
                        || ast::is_set_accessor_declaration(parent)
                            && ast::is_class_like(parent.parent().unwrap()))
                    {
                        break 'case;
                    }
                    if ast::get_this_parameter(parent) == Some(node) {
                        break 'case;
                    }
                    let index = parent
                        .parameters()
                        .iter()
                        .position(|&p| p == node)
                        .expect("parameter belongs to its declaration")
                        - usize::from(ast::get_this_parameter(parent).is_some());
                    // A parameter declaration decorator will have three arguments (see `ParameterDecorator` in
                    // core.d.ts).

                    let (target_type, key_type) = if ast::is_constructor_declaration(parent) {
                        let sym = self.get_symbol_of_declaration(parent.parent().unwrap()).unwrap();
                        (self.get_type_of_symbol(sym), self.undefined_type)
                    } else {
                        (
                            self.get_parent_type_of_class_element(parent),
                            self.get_class_element_property_key_type(parent),
                        )
                    };
                    let index_type = self.get_number_literal_type(jsnum::Number(index as f64));
                    let target_param = self.new_parameter("target", target_type);
                    let key_param = self.new_parameter("propertyKey", key_type);
                    let index_param = self.new_parameter("parameterIndex", index_type);
                    let void_type = self.void_type;
                    let sig = self.new_call_signature(
                        &[],
                        None,
                        &[target_param, key_param, index_param],
                        void_type,
                    );
                    decorator_signature.set(Some(sig));
                }
                Kind::MethodDeclaration
                | Kind::GetAccessor
                | Kind::SetAccessor
                | Kind::PropertyDeclaration => 'case: {
                    if !ast::is_class_like(node.parent().unwrap()) {
                        break 'case;
                    }
                    // A method or accessor declaration decorator will have either two or three arguments (see
                    // `PropertyDecorator` and `MethodDecorator` in core.d.ts).
                    let target_type = self.get_parent_type_of_class_element(node);
                    let target_param = self.new_parameter("target", target_type);
                    let key_type = self.get_class_element_property_key_type(node);
                    let key_param = self.new_parameter("propertyKey", key_type);
                    let mut return_type = self.void_type;
                    if !ast::is_property_declaration(node) {
                        let t = self.get_type_of_node(node);
                        return_type = self.new_typed_property_descriptor_type(t);
                    }
                    let has_prop_desc =
                        !ast::is_property_declaration(node) || ast::has_accessor_modifier(node);
                    if has_prop_desc {
                        let t = self.get_type_of_node(node);
                        let descriptor_type = self.new_typed_property_descriptor_type(t);
                        let descriptor_param = self.new_parameter("descriptor", descriptor_type);
                        let void_type = self.void_type;
                        let union = self.get_union_type(&[return_type, void_type]);
                        let sig = self.new_call_signature(
                            &[],
                            None,
                            &[target_param, key_param, descriptor_param],
                            union,
                        );
                        decorator_signature.set(Some(sig));
                    } else {
                        let void_type = self.void_type;
                        let union = self.get_union_type(&[return_type, void_type]);
                        let sig =
                            self.new_call_signature(&[], None, &[target_param, key_param], union);
                        decorator_signature.set(Some(sig));
                    }
                }
                _ => {}
            }
        }
        if decorator_signature.get() == Some(self.any_signature) {
            return None;
        }
        decorator_signature.get()
    }

    // checker.go:30718
    fn get_es_decorator_call_signature(&mut self, decorator: P<Node>) -> Option<P<Signature>> {
        // We are considering a future change that would allow the type of a decorator to affect the type of the
        // class and its members, such as a `@Stringify` decorator changing the type of a `number` field to `string`, or
        // a `@Callable` decorator adding a call signature to a `class`. The type arguments for the various context
        // types may eventually change to reflect such mutations.
        //
        // In some cases we describe such potential mutations as coming from a "prior decorator application". It is
        // important to note that, while decorators are *evaluated* left to right, they are *applied* right to left
        // to preserve f ৹ g -> f(g(x)) application order. In these cases, a "prior" decorator usually means the
        // next decorator following this one in document order.
        //
        // The "original type" of a class or member is the type it was declared as, or the type we infer from
        // initializers, before _any_ decorators are applied.
        //
        // The type of a class or member that is a result of a prior decorator application represents the
        // "current type", i.e., the type for the declaration at the time the decorator is _applied_.
        //
        // The type of a class or member that is the result of the application of *all* relevant decorators is the
        // "final type".
        //
        // Any decorator that allows mutation or replacement will also refer to an "input type" and an
        // "output type". The "input type" corresponds to the "current type" of the declaration, while the
        // "output type" will become either the "input type/current type" for a subsequent decorator application,
        // or the "final type" for the decorated declaration.
        //
        // It is important to understand decorator application order as it relates to how the "current", "input",
        // "output", and "final" types will be determined:
        //
        //  @E2 @E1 class SomeClass {
        //      @A2 @A1 static f() {}
        //      @B2 @B1 g() {}
        //      @C2 @C1 static x;
        //      @D2 @D1 y;
        //  }
        //
        // Per [the specification][1], decorators are applied in the following order:
        //
        // 1. For each static method (incl. get/set methods and `accessor` fields), in document order:
        //    a. Apply each decorator for that method, in reverse order (`A1`, `A2`).
        // 2. For each instance method (incl. get/set methods and `accessor` fields), in document order:
        //    a. Apply each decorator for that method, in reverse order (`B1`, `B2`).
        // 3. For each static field (excl. auto-accessors), in document order:
        //    a. Apply each decorator for that field, in reverse order (`C1`, `C2`).
        // 4. For each instance field (excl. auto-accessors), in document order:
        //    a. Apply each decorator for that field, in reverse order (`D1`, `D2`).
        // 5. Apply each decorator for the class, in reverse order (`E1`, `E2`).
        //
        // As a result, "current" types at each decorator application are as follows:
        // - For `A1`, the "current" types of the class and method are their "original" types.
        // - For `A2`, the "current type" of the method is the "output type" of `A1`, and the "current type" of the
        //   class is the type of `SomeClass` where `f` is the "output type" of `A1`. This becomes the "final type"
        //   of `f`.
        // - For `B1`, the "current type" of the method is its "original type", and the "current type" of the class
        //   is the type of `SomeClass` where `f` now has its "final type".
        // - etc.
        //
        // [1]: https://arai-a.github.io/ecma262-compare/?pr=2417&id=sec-runtime-semantics-classdefinitionevaluation
        //
        // This seems complicated at first glance, but is not unlike our existing inference for functions:
        //
        //  declare function pipe<Original, A1, A2, B1, B2, C1, C2, D1, D2, E1, E2>(
        //      original: Original,
        //      a1: (input: Original, context: Context<E2>) => A1,
        //      a2: (input: A1, context: Context<E2>) => A2,
        //      b1: (input: A2, context: Context<E2>) => B1,
        //      b2: (input: B1, context: Context<E2>) => B2,
        //      c1: (input: B2, context: Context<E2>) => C1,
        //      c2: (input: C1, context: Context<E2>) => C2,
        //      d1: (input: C2, context: Context<E2>) => D1,
        //      d2: (input: D1, context: Context<E2>) => D2,
        //      e1: (input: D2, context: Context<E2>) => E1,
        //      e2: (input: E1, context: Context<E2>) => E2,
        //  ): E2;

        // When a decorator is applied, it is passed two arguments: "target", which is a value representing the
        // thing being decorated (constructors for classes, functions for methods/accessors, `undefined` for fields,
        // and a `{ get, set }` object for auto-accessors), and "context", which is an object that provides
        // reflection information about the decorated element, as well as the ability to add additional "extra"
        // initializers. In most cases, the "target" argument corresponds to the "input type" in some way, and the
        // return value similarly corresponds to the "output type" (though if the "output type" is `void` or
        // `undefined` then the "output type" is the "input type").
        let node = decorator.parent().unwrap();
        let links = self.signature_links.get(node);
        let decorator_signature = links.decorator_signature(node);
        if decorator_signature.get().is_none() {
            decorator_signature.set(Some(self.any_signature));
            match node.kind() {
                Kind::ClassDeclaration | Kind::ClassExpression => {
                    // Class decorators have a `context` of `ClassDecoratorContext<Class>`, where the `Class` type
                    // argument will be the "final type" of the class after all decorators are applied.
                    let sym = self.get_symbol_of_declaration(node).unwrap();
                    let target_type = self.get_type_of_symbol(sym);
                    let context_type = self.new_class_decorator_context_type(target_type);
                    let sig = self.new_es_decorator_call_signature(
                        target_type,
                        context_type,
                        target_type,
                    );
                    decorator_signature.set(Some(sig));
                }
                Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => 'case: {
                    if !ast::is_class_like(node.parent().unwrap()) {
                        break 'case;
                    }
                    // Method decorators have a `context` of `ClassMethodDecoratorContext<This, Value>`, where the
                    // `Value` type argument corresponds to the "final type" of the method.
                    //
                    // Getter decorators have a `context` of `ClassGetterDecoratorContext<This, Value>`, where the
                    // `Value` type argument corresponds to the "final type" of the value returned by the getter.
                    //
                    // Setter decorators have a `context` of `ClassSetterDecoratorContext<This, Value>`, where the
                    // `Value` type argument corresponds to the "final type" of the parameter of the setter.
                    //
                    // In all three cases, the `This` type argument is the "final type" of either the class or
                    // instance, depending on whether the member was `static`.

                    let value_type = if ast::is_method_declaration(node) {
                        let sig = self.get_signature_from_declaration(node);
                        self.get_or_create_type_from_signature(sig)
                    } else {
                        self.get_type_of_node(node)
                    };

                    let class_symbol =
                        self.get_symbol_of_declaration(node.parent().unwrap()).unwrap();
                    let this_type = if ast::has_static_modifier(node) {
                        self.get_type_of_symbol(class_symbol)
                    } else {
                        self.get_declared_type_of_class_or_interface(class_symbol)
                    };
                    // We wrap the "input type", if necessary, to match the decoration target. For getters this is
                    // something like `() => inputType`, for setters it's `(value: inputType) => void` and for
                    // methods it is just the input type.
                    let target_type = if ast::is_get_accessor_declaration(node) {
                        self.new_getter_function_type(value_type)
                    } else if ast::is_set_accessor_declaration(node) {
                        self.new_setter_function_type(value_type)
                    } else {
                        value_type
                    };
                    let context_type = self.new_class_member_decorator_context_type_for_node(
                        node, this_type, value_type,
                    );
                    let sig = self.new_es_decorator_call_signature(
                        target_type,
                        context_type,
                        target_type,
                    );
                    decorator_signature.set(Some(sig));
                }
                Kind::PropertyDeclaration => 'case: {
                    if !ast::is_class_like(node.parent().unwrap()) {
                        break 'case;
                    }
                    // Field decorators have a `context` of `ClassFieldDecoratorContext<This, Value>` and
                    // auto-accessor decorators have a `context` of `ClassAccessorDecoratorContext<This, Value>. In
                    // both cases, the `This` type argument is the "final type" of either the class or instance,
                    // depending on whether the member was `static`, and the `Value` type argument corresponds to
                    // the "final type" of the value stored in the field.
                    let value_type = self.get_type_of_node(node);

                    let class_symbol =
                        self.get_symbol_of_declaration(node.parent().unwrap()).unwrap();
                    let this_type = if ast::has_static_modifier(node) {
                        self.get_type_of_symbol(class_symbol)
                    } else {
                        self.get_declared_type_of_class_or_interface(class_symbol)
                    };
                    // The `target` of an auto-accessor decorator is a `{ get, set }` object, representing the
                    // runtime-generated getter and setter that are added to the class/prototype. The `target` of a
                    // regular field decorator is always `undefined` as it isn't installed until it is initialized.
                    let target_type = if ast::has_accessor_modifier(node) {
                        self.new_class_accessor_decorator_target_type(this_type, value_type)
                    } else {
                        self.undefined_type
                    };
                    // We wrap the "output type" depending on the declaration. For auto-accessors, we wrap the
                    // "output type" in a `ClassAccessorDecoratorResult<This, In, Out>` type, which allows for
                    // mutation of the runtime-generated getter and setter, as well as the injection of an
                    // initializer mutator. For regular fields, we wrap the "output type" in an initializer mutator.
                    let return_type = if ast::has_accessor_modifier(node) {
                        self.new_class_accessor_decorator_result_type(this_type, value_type)
                    } else {
                        self.new_class_field_decorator_initializer_mutator_type(
                            this_type, value_type,
                        )
                    };
                    let context_type = self.new_class_member_decorator_context_type_for_node(
                        node, this_type, value_type,
                    );
                    let sig = self.new_es_decorator_call_signature(
                        target_type,
                        context_type,
                        return_type,
                    );
                    decorator_signature.set(Some(sig));
                }
                _ => {}
            }
        }
        if decorator_signature.get() == Some(self.any_signature) {
            return None;
        }
        decorator_signature.get()
    }

    // checker.go:30896
    fn new_class_decorator_context_type(&mut self, class_type: P<Type>) -> P<Type> {
        let target = self.get_global_class_decorator_context_type();
        self.try_create_type_reference(target, &[class_type])
    }

    // checker.go:30900
    fn new_class_method_decorator_context_type(
        &mut self,
        class_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_method_decorator_context_type();
        self.try_create_type_reference(target, &[class_type, value_type])
    }

    // checker.go:30904
    fn new_class_getter_decorator_context_type(
        &mut self,
        class_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_getter_decorator_context_type();
        self.try_create_type_reference(target, &[class_type, value_type])
    }

    // checker.go:30908
    fn new_class_setter_decorator_context_type(
        &mut self,
        class_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_setter_decorator_context_type();
        self.try_create_type_reference(target, &[class_type, value_type])
    }

    // checker.go:30912
    fn new_class_accessor_decorator_context_type(
        &mut self,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_accessor_decorator_context_type();
        self.try_create_type_reference(target, &[this_type, value_type])
    }

    // checker.go:30916
    fn new_class_field_decorator_context_type(
        &mut self,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_field_decorator_context_type();
        self.try_create_type_reference(target, &[this_type, value_type])
    }

    // Gets a type like `{ name: "foo", private: false, static: true }` that is used to provided member-specific
    // details that will be intersected with a decorator context type.
    // checker.go:30922
    fn get_class_member_decorator_context_override_type(
        &mut self,
        name_type: P<Type>,
        is_private: bool,
        is_static: bool,
    ) -> P<Type> {
        let kind = if is_private {
            if is_static {
                CachedTypeKind::DecoratorContextPrivateStatic
            } else {
                CachedTypeKind::DecoratorContextPrivate
            }
        } else if is_static {
            CachedTypeKind::DecoratorContextStatic
        } else {
            CachedTypeKind::DecoratorContext
        };
        let key = CachedTypeKey { kind, type_id: name_type.id };
        if let Some(&override_type) = self.cached_types.get(&key) {
            return override_type;
        }
        let members = SymbolTable::new();
        let name_prop = self.new_property("name", name_type);
        members.set("name", name_prop);
        let private_type = if is_private { self.true_type } else { self.false_type };
        let private_prop = self.new_property("private", private_type);
        members.set("private", private_prop);
        let static_type = if is_static { self.true_type } else { self.false_type };
        let static_prop = self.new_property("static", static_type);
        members.set("static", static_prop);
        let override_type = self.new_anonymous_type(None, Some(members), &[], &[], &[]);
        self.cached_types.insert(key, override_type);
        override_type
    }

    // checker.go:30941
    fn new_class_member_decorator_context_type_for_node(
        &mut self,
        node: P<Node>,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let is_static = ast::has_static_modifier(node);
        let name = node.name().unwrap();
        let is_private = ast::is_private_identifier(name);
        let name_type = if is_private {
            self.get_string_literal_type(name.text())
        } else {
            self.get_literal_type_from_property_name(name)
        };
        let context_type = if ast::is_method_declaration(node) {
            self.new_class_method_decorator_context_type(this_type, value_type)
        } else if ast::is_get_accessor_declaration(node) {
            self.new_class_getter_decorator_context_type(this_type, value_type)
        } else if ast::is_set_accessor_declaration(node) {
            self.new_class_setter_decorator_context_type(this_type, value_type)
        } else if ast::is_auto_accessor_property_declaration(node) {
            self.new_class_accessor_decorator_context_type(this_type, value_type)
        } else if ast::is_property_declaration(node) {
            self.new_class_field_decorator_context_type(this_type, value_type)
        } else {
            panic!("Unhandled case in createClassMemberDecoratorContextTypeForNode")
        };
        let override_type =
            self.get_class_member_decorator_context_override_type(name_type, is_private, is_static);
        self.get_intersection_type(&[context_type, override_type])
    }

    // checker.go:30969
    fn new_class_accessor_decorator_target_type(
        &mut self,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_accessor_decorator_target_type();
        self.try_create_type_reference(target, &[this_type, value_type])
    }

    // checker.go:30973
    fn new_class_accessor_decorator_result_type(
        &mut self,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let target = self.get_global_class_accessor_decorator_result_type();
        self.try_create_type_reference(target, &[this_type, value_type])
    }

    // checker.go:30977
    fn new_class_field_decorator_initializer_mutator_type(
        &mut self,
        this_type: P<Type>,
        value_type: P<Type>,
    ) -> P<Type> {
        let this_param = self.new_parameter("this", this_type);
        let value_param = self.new_parameter("value", value_type);
        self.new_function_type(&[], Some(this_param), &[value_param], value_type)
    }

    // Creates a call signature for an ES Decorator. This method is used by the semantics of
    // `getESDecoratorCallSignature`, which you should probably be using instead.
    // checker.go:30985
    fn new_es_decorator_call_signature(
        &mut self,
        target_type: P<Type>,
        context_type: P<Type>,
        non_optional_return_type: P<Type>,
    ) -> P<Signature> {
        let target_param = self.new_parameter("target", target_type);
        let context_param = self.new_parameter("context", context_type);
        let void_type = self.void_type;
        let return_type = self.get_union_type(&[non_optional_return_type, void_type]);
        self.new_call_signature(
            &[],
            None, /*thisParameter*/
            &[target_param, context_param],
            return_type,
        )
    }

    // Creates a synthetic `FunctionType`
    // checker.go:30993
    fn new_function_type(
        &mut self,
        type_parameters: &[P<Type>],
        this_parameter: Option<P<Symbol>>,
        parameters: &[P<Symbol>],
        return_type: P<Type>,
    ) -> P<Type> {
        let signature =
            self.new_call_signature(type_parameters, this_parameter, parameters, return_type);
        self.get_or_create_type_from_signature(signature)
    }

    // checker.go:30998
    fn new_getter_function_type(&mut self, t: P<Type>) -> P<Type> {
        self.new_function_type(&[], None /*thisParameter*/, &[], t)
    }

    // checker.go:31002
    fn new_setter_function_type(&mut self, t: P<Type>) -> P<Type> {
        let value_param = self.new_parameter("value", t);
        let void_type = self.void_type;
        self.new_function_type(&[], None /*thisParameter*/, &[value_param], void_type)
    }

    // Creates a synthetic `Signature` corresponding to a call signature.
    // checker.go:31008
    fn new_call_signature(
        &mut self,
        type_parameters: &[P<Type>],
        this_parameter: Option<P<Symbol>>,
        parameters: &[P<Symbol>],
        return_type: P<Type>,
    ) -> P<Signature> {
        let any_keyword = self.factory.new_keyword_type_node(Kind::AnyKeyword);
        let decl = self.factory.new_function_type_node(None, None, Some(any_keyword));
        self.new_signature(
            SignatureFlags::None,
            Some(decl),
            type_parameters,
            this_parameter,
            parameters,
            Some(return_type),
            None,
            parameters.len(),
        )
    }

    // checker.go:31013
    fn new_typed_property_descriptor_type(&mut self, property_type: P<Type>) -> P<Type> {
        let global = self.get_global_typed_property_descriptor_type();
        self.create_type_from_generic_global_type(global, &[property_type])
    }

    // checker.go:31017
    fn get_parent_type_of_class_element(&mut self, node: P<Node>) -> P<Type> {
        let class_symbol = self.get_symbol_of_node(node.parent().unwrap()).unwrap();
        if ast::is_static(node) {
            return self.get_type_of_symbol(class_symbol);
        }
        self.get_declared_type_of_symbol(class_symbol)
    }

    // checker.go:31025
    fn get_class_element_property_key_type(&mut self, element: P<Node>) -> P<Type> {
        let name = element.name().unwrap();
        match name.kind() {
            Kind::Identifier | Kind::NumericLiteral | Kind::StringLiteral => {
                return self.get_string_literal_type(name.text());
            }
            Kind::ComputedPropertyName => {
                let name_type = self.check_computed_property_name(name);
                if self.is_type_assignable_to_kind(name_type, TypeFlags::ESSymbolLike) {
                    return name_type;
                }
                return self.string_type;
            }
            _ => {}
        }
        self.error_type
    }

    // checker.go:31040
    pub(crate) fn get_type_of_property_of_contextual_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        self.get_type_of_property_of_contextual_type_ex(t, name, None)
    }

    // checker.go:31044
    pub(crate) fn get_type_of_property_of_contextual_type_ex(
        &mut self,
        t: P<Type>,
        name: &str,
        name_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        if t.flags().intersects(TypeFlags::Union)
            && let Some(result) = self.contextual_property_type_of_large_union(t, name, name_type)
        {
            return result;
        }
        self.map_type_ex(
            t,
            |c, t| c.contextual_property_type_of_constituent(t, name, name_type),
            true, /*noReductions*/
        )
    }

    /// The `mapType` callback of `getTypeOfPropertyOfContextualType`.
    fn contextual_property_type_of_constituent(
        &mut self,
        t: P<Type>,
        name: &str,
        name_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        if t.flags().intersects(TypeFlags::Intersection) {
            let mut types: Vec<P<Type>> = Vec::new();
            let mut ignore_index_infos = false;
            for &constituent_type in t.types() {
                if !constituent_type.flags().intersects(TypeFlags::Object) {
                    continue;
                }
                if self.is_generic_mapped_type(constituent_type)
                    && self.get_mapped_type_name_type_kind(constituent_type)
                        != MappedTypeNameTypeKind::Remapping
                {
                    let substituted_type = self
                        .get_indexed_mapped_type_substituted_type_of_contextual_type(
                            constituent_type,
                            name,
                            name_type,
                        );
                    self.append_contextual_property_type_constituent(&mut types, substituted_type);
                    continue;
                }
                let property_type =
                    self.get_type_of_concrete_property_of_contextual_type(constituent_type, name);
                if property_type.is_none() {
                    continue;
                }
                ignore_index_infos = true;
                self.append_contextual_property_type_constituent(&mut types, property_type);
            }
            // Go collects the index info candidates (the constituents above without the property) and drops them at
            // the first constituent with it, so they are used only when no constituent has it: all of them, in order.
            if !ignore_index_infos {
                for &candidate in t.types() {
                    if !candidate.flags().intersects(TypeFlags::Object)
                        || self.is_generic_mapped_type(candidate)
                            && self.get_mapped_type_name_type_kind(candidate)
                                != MappedTypeNameTypeKind::Remapping
                    {
                        continue;
                    }
                    let index_info_type = self
                        .get_type_from_index_infos_of_contextual_type(candidate, name, name_type);
                    self.append_contextual_property_type_constituent(&mut types, index_info_type);
                }
            }
            if types.is_empty() {
                return None;
            }
            if types.len() == 1 {
                return Some(types[0]);
            }
            return Some(self.get_intersection_type(&types));
        }
        if !t.flags().intersects(TypeFlags::Object) {
            return None;
        }
        if self.is_generic_mapped_type(t)
            && self.get_mapped_type_name_type_kind(t) != MappedTypeNameTypeKind::Remapping
        {
            return self
                .get_indexed_mapped_type_substituted_type_of_contextual_type(t, name, name_type);
        }
        let result = self.get_type_of_concrete_property_of_contextual_type(t, name);
        if result.is_some() {
            return result;
        }
        self.get_type_from_index_infos_of_contextual_type(t, name, name_type)
    }

    /// tsrs-only: `get_type_of_property_of_contextual_type_ex` of a large union, visiting only the constituents
    /// that can give a type (None when that is all of them, or the union is small). A contextual union of object
    /// types is asked about each property of an object literal three or four times; on the 38k-file codebase such
    /// unions average over 100 constituents, nearly all without the property, and most (union, name) pairs repeat.
    /// A constituent is skipped when the callback returns None for it without side effects whenever it is asked:
    /// a plain object type with resolved members, no such member, no index infos, and a name that no `Object` or
    /// `Function` member has. That holds while those members stay resolved, so the positions are computed only
    /// with no member resolution in progress and dropped when any type's members are reset. The visited
    /// constituents are mapped in the same order and combined as `map_type_ex` does (a skipped one maps to None,
    /// so the result is never the union itself).
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn contextual_property_type_of_large_union(
        &mut self,
        t: P<Type>,
        name: &str,
        name_type: Option<P<Type>>,
    ) -> Option<Option<P<Type>>> {
        let u = t.as_union_type();
        let types = match u.origin() {
            Some(origin) if origin.flags().intersects(TypeFlags::Union) => origin.types(),
            _ => u.types.get(),
        };
        if types.len() < 8 {
            return None;
        }
        let key = HashedName::new(name);
        let epoch = self.members_reset_epoch;
        let cached = self
            .contextual_relevant
            .get(&(t, key.hash()))
            .and_then(|entries| entries.iter().find(|e| e.0 == name))
            .map(|e| (e.1, std::rc::Rc::clone(&e.2)));
        let relevant = match cached {
            Some((e, relevant)) if e == epoch => relevant,
            _ => {
                if self.members_resolving != 0 {
                    return None;
                }
                let relevant: std::rc::Rc<[u32]> = (0..types.len() as u32)
                    .filter(|&i| !self.is_skippable_contextual_constituent(types[i as usize], key))
                    .collect();
                let entries = self.contextual_relevant.entry((t, key.hash())).or_default();
                entries.retain(|e| e.0 != name);
                entries.push((alloc_str(name), epoch, std::rc::Rc::clone(&relevant)));
                relevant
            }
        };
        if relevant.len() == types.len() {
            return None;
        }
        let mut mapped_types = self.free_type_lists.pop().unwrap_or_default();
        mapped_types.extend(relevant.iter().filter_map(|&i| {
            let s = types[i as usize];

            if s.flags().intersects(TypeFlags::Union) {
                self.map_type_ex(
                    s,
                    |c, t| c.contextual_property_type_of_constituent(t, name, name_type),
                    true, /*noReductions*/
                )
            } else {
                self.contextual_property_type_of_constituent(s, name, name_type)
            }
        }));
        let result = if mapped_types.is_empty() {
            None
        } else {
            Some(self.get_union_type_ex(&mapped_types, UnionReduction::None, AliasArg::None, None))
        };
        self.free_type_list(mapped_types);
        Some(result)
    }

    /// See `contextual_property_type_of_large_union`.
    fn is_skippable_contextual_constituent(&mut self, t: P<Type>, key: HashedName<'_>) -> bool {
        t.flags() == TypeFlags::Object
            && t.object_flags_lazy() & (ObjectFlags::MembersResolved | ObjectFlags::Mapped)
                == ObjectFlags::MembersResolved
            && !is_tuple_type(t)
            && t.as_structured_type().members().is_none_or(|m| m.lookup_hashed(key).is_none())
            && self.index_infos_of_structured_type(t).is_empty()
            && !self.may_be_augment_member(key)
    }

    // checker.go:31096
    fn get_indexed_mapped_type_substituted_type_of_contextual_type(
        &mut self,
        t: P<Type>,
        name: &str,
        name_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        let property_name_type = match name_type {
            Some(name_type) => name_type,
            None => self.get_string_literal_type(name),
        };
        let constraint = self.get_constraint_type_from_mapped_type(t);
        // special case for conditional types pretending to be negated types
        let mapped_name_type = t.as_mapped_type().name_type.get();
        if mapped_name_type.is_some()
            && self.is_excluded_mapped_property_name(mapped_name_type.unwrap(), property_name_type)
            || self.is_excluded_mapped_property_name(constraint, property_name_type)
        {
            return None;
        }
        let constraint_of_constraint = self.get_base_constraint_or_type(constraint);
        if !self.is_type_assignable_to(property_name_type, constraint_of_constraint) {
            return None;
        }
        Some(self.substitute_indexed_mapped_type(t, property_name_type))
    }

    // checker.go:31113
    fn is_excluded_mapped_property_name(
        &mut self,
        t: P<Type>,
        property_name_type: P<Type>,
    ) -> bool {
        if t.flags().intersects(TypeFlags::Conditional) {
            let true_type = self.get_true_type_from_conditional_type(t);
            if !self.get_reduced_type(true_type).flags().intersects(TypeFlags::Never) {
                return false;
            }
            let false_type = self.get_false_type_from_conditional_type(t);
            let a = self.get_actual_type_variable(false_type);
            let check_type = t.as_conditional_type().check_type.get().unwrap();
            let b = self.get_actual_type_variable(check_type);
            return a == b
                && self.is_type_assignable_to(
                    property_name_type,
                    t.as_conditional_type().extends_type.get().unwrap(),
                );
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            return t
                .types()
                .iter()
                .any(|&t| self.is_excluded_mapped_property_name(t, property_name_type));
        }
        false
    }

    // checker.go:31127
    fn get_type_of_concrete_property_of_contextual_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        let prop = self.get_property_of_type(t, name);
        let prop = prop?;
        if self.is_circular_mapped_property(prop) {
            return None;
        }
        let prop_type = self.get_type_of_symbol(prop);
        Some(self.remove_missing_type(prop_type, prop.flags().intersects(SymbolFlags::Optional)))
    }

    // checker.go:31135
    fn get_type_from_index_infos_of_contextual_type(
        &mut self,
        t: P<Type>,
        name: &str,
        name_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        if is_tuple_type(t) && is_numeric_literal_name(name) && jsnum::from_string(name).0 >= 0.0 {
            let rest_type = self.get_element_type_of_slice_of_tuple_type(
                t,
                t.target_tuple_type().fixed_length.get(),
                0,     /*endSkipCount*/
                false, /*writing*/
                true,  /*noReductions*/
            );
            if rest_type.is_some() {
                return rest_type;
            }
        }
        let index_infos = self.get_index_infos_of_structured_type(t);
        if index_infos.is_empty() {
            return None; // most contextual types: no index signature applies to any name
        }
        let name_type = match name_type {
            Some(name_type) => name_type,
            None => self.get_string_literal_type(name),
        };
        let index_info = self.find_applicable_index_info(index_infos, name_type);
        let index_info = index_info?;
        index_info.value_type.get()
    }

    // checker.go:31152
    fn is_circular_mapped_property(&mut self, symbol: P<Symbol>) -> bool {
        if symbol.check_flags().intersects(CheckFlags::Mapped) {
            let links = self.value_symbol_links.get(symbol);
            return links.resolved_type.get().is_none()
                && self
                    .find_resolution_cycle_start_index(
                        TypeSystemEntity::Symbol(symbol),
                        TypeSystemPropertyName::Type,
                    )
                    .is_some();
        }
        false
    }

    // checker.go:31160
    /// tsrs: appends to `types` in place (Go returns the appended slice).
    fn append_contextual_property_type_constituent(
        &self,
        types: &mut Vec<P<Type>>,
        t: Option<P<Type>>,
    ) {
        // any doesn't provide any contextual information but could spoil the overall result by nullifying contextual information
        // provided by other intersection constituents so it gets replaced with `unknown` as `T & unknown` is just `T` and all
        // types computed based on the contextual information provided by other constituens are still assignable to any
        let Some(t) = t else {
            return;
        };
        types.push(if t.flags().intersects(TypeFlags::Any) { self.unknown_type } else { t });
    }

    // Return the contextual type for a given expression node. During overload resolution, a contextual type may temporarily
    // be "pushed" onto a node using the contextualType property.
    // checker.go:31175
    pub(crate) fn get_apparent_type_of_contextual_type(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let contextual_type = if ast::is_object_literal_method(node) {
            self.get_contextual_type_for_object_literal_method(node, context_flags)
        } else {
            self.get_contextual_type(node, context_flags)
        };
        let instantiated_type =
            self.instantiate_contextual_type(contextual_type, node, context_flags);
        if let Some(instantiated_type) = instantiated_type
            && !(context_flags.intersects(ContextFlags::NoConstraints)
                && instantiated_type.flags().intersects(TypeFlags::TypeVariable))
        {
            let apparent_type = self
                .map_type_ex(
                    instantiated_type,
                    |c, t| {
                        if t.object_flags().intersects(ObjectFlags::Mapped) {
                            return Some(t);
                        }
                        Some(c.get_apparent_type(t))
                    },
                    true,
                )
                .unwrap();
            if apparent_type.flags().intersects(TypeFlags::Union)
                && ast::is_object_literal_expression(node)
            {
                return Some(
                    self.discriminate_contextual_type_by_object_members(node, apparent_type),
                );
            } else if apparent_type.flags().intersects(TypeFlags::Union)
                && ast::is_jsx_attributes(node)
            {
                return Some(
                    self.discriminate_contextual_type_by_jsx_attributes(node, apparent_type),
                );
            }
            return Some(apparent_type);
        }
        None
    }
}

impl ObjectLiteralDiscriminator {
    // checker.go:31208
    pub(crate) fn len(&self, _c: &mut Checker) -> usize {
        self.props.len() + self.members.len()
    }

    // checker.go:31212
    pub(crate) fn name(&self, _c: &mut Checker, index: usize) -> &'static str {
        if index < self.props.len() {
            return self.props[index].symbol().unwrap().name();
        }
        self.members[index - self.props.len()].name()
    }

    // checker.go:31219
    pub(crate) fn matches(&self, c: &mut Checker, index: usize, t: P<Type>) -> bool {
        let prop_type;
        if index < self.props.len() {
            let prop = self.props[index];
            if ast::is_property_assignment(prop) || ast::is_jsx_attribute(prop) {
                let initializer = prop.initializer();
                if initializer.is_some() {
                    prop_type = c.get_context_free_type_of_expression(prop.initializer().unwrap());
                } else {
                    prop_type = c.true_type; // JsxAttribute without initializer is always true
                }
            } else {
                prop_type = c.get_context_free_type_of_expression(prop.name().unwrap());
            }
        } else {
            prop_type = c.undefined_type;
        }
        prop_type.distributed_iter().into_iter().any(|s| c.is_type_assignable_to(s, t))
    }
}

impl Checker {
    // checker.go:31244
    fn discriminate_contextual_type_by_object_members(
        &mut self,
        node: P<Node>,
        contextual_type: P<Type>,
    ) -> P<Type> {
        let key = DiscriminatedContextualTypeKey {
            node_id: get_node_id(node),
            type_id: contextual_type.id,
        };
        if let Some(&discriminated) = self.discriminated_contextual_types.get(&key) {
            return discriminated;
        }
        let discriminated =
            self.get_matching_union_constituent_for_object_literal(contextual_type, node);
        let discriminated = if let Some(discriminated) = discriminated {
            discriminated
        } else {
            let mut discriminant_properties = Vec::new();
            discriminant_properties.extend(node.properties().iter().filter_map(|&p| {
                let keep = 'keep: {
                    let Some(symbol) = p.symbol() else {
                        break 'keep false;
                    };
                    if ast::is_property_assignment(p) {
                        break 'keep Self::is_possibly_discriminant_value(p.initializer().unwrap())
                            && self.is_discriminant_property(Some(contextual_type), symbol.name());
                    }
                    if ast::is_shorthand_property_assignment(p) {
                        break 'keep self
                            .is_discriminant_property(Some(contextual_type), symbol.name());
                    }
                    false
                };
                if keep { Some(p) } else { None }
            }));
            let mut discriminant_members = Vec::new();
            discriminant_members.extend(
                self.get_properties_of_type(contextual_type).iter().copied().filter(|&s| {
                    let node_members = node.symbol().unwrap().members();
                    s.flags().intersects(SymbolFlags::Optional)
                        && node_members.is_none_or(|m| m.lookup(s.name()).is_none())
                        && self.is_discriminant_property(Some(contextual_type), s.name())
                }),
            );
            let mut discriminator = ObjectLiteralDiscriminator {
                props: discriminant_properties,
                members: discriminant_members,
            };
            self.discriminate_type_by_discriminable_items(contextual_type, &mut discriminator)
        };
        self.discriminated_contextual_types.insert(key, discriminated);
        discriminated
    }

    // checker.go:31274
    fn get_matching_union_constituent_for_object_literal(
        &mut self,
        union_type: P<Type>,
        node: P<Node>,
    ) -> Option<P<Type>> {
        let key_property_name = self.get_key_property_name(union_type);
        if !key_property_name.is_empty() {
            let mut prop_node = None;
            if let Some(p) = node.properties().iter().find_map(|&p| {
                if p.symbol().is_some()
                    && ast::is_property_assignment(p)
                    && p.symbol().unwrap().name() == key_property_name
                    && Self::is_possibly_discriminant_value(p.initializer().unwrap())
                {
                    Some(p)
                } else {
                    None
                }
            }) {
                prop_node = Some(p);
            }
            if let Some(prop_node) = prop_node {
                let prop_type =
                    self.get_context_free_type_of_expression(prop_node.initializer().unwrap());
                return self.get_constituent_type_for_key_type(union_type, prop_type);
            }
        }
        None
    }

    // Return true if the given expression is possibly a discriminant value. We limit the kinds of
    // expressions we check to those that don't depend on their contextual type in order not to cause
    // recursive (and possibly infinite) invocations of getContextualType.
    // checker.go:31291
    pub(crate) fn is_possibly_discriminant_value(node: P<Node>) -> bool {
        match node.kind() {
            Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateExpression
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NullKeyword
            | Kind::Identifier
            | Kind::UndefinedKeyword => {
                return true;
            }
            Kind::PropertyAccessExpression | Kind::ParenthesizedExpression => {
                return Self::is_possibly_discriminant_value(node.expression().unwrap());
            }
            Kind::JsxExpression => {
                return node.expression().is_none()
                    || Self::is_possibly_discriminant_value(node.expression().unwrap());
            }
            _ => {}
        }
        false
    }

    // If the given contextual type contains instantiable types and if a mapper representing
    // return type inferences is available, instantiate those types using that mapper.
    // contextual_type and the result are nil-able in Go (P<Type> -> Option<P<Type>>).
    // checker.go:31306
    pub(crate) fn instantiate_contextual_type(
        &mut self,
        contextual_type: Option<P<Type>>,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if let Some(contextual_type) = contextual_type
            && self.maybe_type_of_kind(contextual_type, TypeFlags::Instantiable)
        {
            let inference_context = self.get_inference_context(node);
            // If no inferences have been made, and none of the type parameters for which we are inferring
            // specify default types, nothing is gained from instantiating as type parameters would just be
            // replaced with their constraints similar to the apparent type.
            if let Some(inference_context) = inference_context {
                if context_flags.intersects(ContextFlags::Signature)
                    && inference_context
                        .inferences
                        .get()
                        .iter()
                        .any(|&info| has_inference_candidates_or_default(info))
                {
                    // For contextual signatures we incorporate all inferences made so far, e.g. from return
                    // types as well as arguments to the left in a function call.
                    let t = self.instantiate_instantiable_types(
                        contextual_type,
                        inference_context.non_fixing_mapper().unwrap(),
                    );
                    if !t.flags().intersects(TypeFlags::AnyOrUnknown) {
                        return Some(t);
                    }
                }
                if let Some(return_mapper) = inference_context.return_mapper() {
                    // For other purposes (e.g. determining whether to produce literal types) we only
                    // incorporate inferences made from the return type in a function call. We remove
                    // the 'boolean' type from the contextual type such that contextually typed boolean
                    // literals actually end up widening to 'boolean' (see #48363).
                    let t = self.instantiate_instantiable_types(contextual_type, return_mapper);
                    if !t.flags().intersects(TypeFlags::AnyOrUnknown) {
                        if t.flags().intersects(TypeFlags::Union)
                            && contains_type(self, t.types(), self.regular_false_type)
                            && contains_type(self, t.types(), self.regular_true_type)
                        {
                            return Some(self.filter_type(t, |c, t| {
                                t != c.regular_false_type && t != c.regular_true_type
                            }));
                        }
                        return Some(t);
                    }
                }
            }
        }
        contextual_type
    }

    // This function is similar to instantiateType, except that (a) it only instantiates types that
    // are classified as instantiable (i.e. it doesn't instantiate object types), and (b) it performs
    // no reductions on instantiated union types.
    // checker.go:31344
    fn instantiate_instantiable_types(&mut self, t: P<Type>, mapper: P<TypeMapper>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Instantiable) {
            return self.instantiate_type(t, Some(mapper));
        }
        if t.flags().intersects(TypeFlags::Union) {
            let types: Vec<P<Type>> =
                t.types().iter().map(|&t| self.instantiate_instantiable_types(t, mapper)).collect();
            return self.get_union_type_ex(&types, UnionReduction::None, AliasArg::None, None);
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            let types: Vec<P<Type>> =
                t.types().iter().map(|&t| self.instantiate_instantiable_types(t, mapper)).collect();
            return self.get_intersection_type(&types);
        }
        t
    }

    // checker.go:31361
    pub(crate) fn push_cached_contextual_type(&mut self, node: P<Node>) {
        let t = self.get_contextual_type(node, ContextFlags::None);
        self.push_contextual_type(node, t, true /*isCache*/);
    }

    // checker.go:31365
    pub(crate) fn push_contextual_type(
        &mut self,
        node: P<Node>,
        t: Option<P<Type>>,
        is_cache: bool,
    ) {
        self.contextual_infos.push(ContextualInfo { node, t, is_cache });
    }

    // checker.go:31369
    pub(crate) fn pop_contextual_type(&mut self) {
        self.contextual_infos.pop();
    }

    // checker.go:31375
    pub(crate) fn find_contextual_node(
        &self,
        node: P<Node>,
        include_caches: bool,
    ) -> Option<usize> {
        for (i, info) in self.contextual_infos.iter().enumerate() {
            if node == info.node && (include_caches || !info.is_cache) {
                return Some(i);
            }
        }
        None
    }

    // Returns true if the given expression contains (at any level of nesting) a function or arrow expression
    // that is subject to contextual typing.
    // checker.go:31386
    pub(crate) fn is_context_sensitive(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::MethodDeclaration
            | Kind::FunctionDeclaration => {
                return self.is_context_sensitive_function_like_declaration(node);
            }
            Kind::ObjectLiteralExpression => {
                return node.properties().iter().any(|&p| self.is_context_sensitive(p));
            }
            Kind::ArrayLiteralExpression => {
                return node.elements().iter().any(|&e| self.is_context_sensitive(e));
            }
            Kind::ConditionalExpression => {
                let conditional = node.as_conditional_expression();
                return self.is_context_sensitive(conditional.when_true)
                    || self.is_context_sensitive(conditional.when_false);
            }
            Kind::BinaryExpression => {
                let binary = node.as_binary_expression();
                return ast::node_kind_is(
                    binary.operator_token,
                    &[Kind::BarBarToken, Kind::QuestionQuestionToken],
                ) && (self.is_context_sensitive(binary.left)
                    || self.is_context_sensitive(binary.right()));
            }
            Kind::PropertyAssignment => {
                return self.is_context_sensitive(node.initializer().unwrap());
            }
            Kind::ParenthesizedExpression => {
                return self.is_context_sensitive(node.expression().unwrap());
            }
            Kind::JsxAttributes => {
                return node.properties().iter().any(|&p| self.is_context_sensitive(p))
                    || ast::is_jsx_opening_element(node.parent().unwrap())
                        && node
                            .parent()
                            .unwrap()
                            .parent()
                            .unwrap()
                            .children()
                            .nodes()
                            .iter()
                            .any(|&c| self.is_context_sensitive(c));
            }
            Kind::JsxAttribute => {
                // If there is no initializer, JSX attribute has a boolean value of true which is not context sensitive.
                let initializer = node.initializer();
                return initializer.is_some() && self.is_context_sensitive(initializer.unwrap());
            }
            Kind::JsxExpression => {
                // It is possible to that node.expression is undefined (e.g <div x={} />)
                let expression = node.expression();
                return expression.is_some() && self.is_context_sensitive(expression.unwrap());
            }
            Kind::YieldExpression => {
                let expression = node.expression();
                return expression.is_some() && self.is_context_sensitive(expression.unwrap());
            }
            _ => {}
        }
        false
    }

    // checker.go:31420
    pub(crate) fn is_context_sensitive_function_like_declaration(&mut self, node: P<Node>) -> bool {
        ast::has_context_sensitive_parameters(node)
            || self.has_context_sensitive_return_expression(node)
            || self.has_context_sensitive_yield_expression(node)
    }

    // checker.go:31424
    fn has_context_sensitive_return_expression(&mut self, node: P<Node>) -> bool {
        if node.type_parameter_list().is_some() || node.type_node().is_some() {
            return false;
        }
        let Some(body) = node.body() else {
            return false;
        };
        if !ast::is_block(body) {
            return self.is_context_sensitive(body);
        }
        ast::for_each_return_statement(body, |statement| {
            statement.expression().is_some()
                && self.is_context_sensitive(statement.expression().unwrap())
        })
    }

    // checker.go:31440
    fn has_context_sensitive_yield_expression(&mut self, node: P<Node>) -> bool {
        ast::get_function_flags(Some(node)).intersects(FunctionFlags::Generator)
            && node.body().is_some()
            && for_each_yield_expression(node.body().unwrap(), |n| self.is_context_sensitive(n))
    }

    // checker.go:31444
    pub(crate) fn push_inference_context(
        &mut self,
        node: P<Node>,
        context: Option<P<InferenceContext>>,
    ) {
        self.inference_context_infos.push(InferenceContextInfo { node, context });
    }

    // checker.go:31448
    pub(crate) fn pop_inference_context(&mut self) {
        self.inference_context_infos.pop();
    }

    // checker.go:31454
    pub(crate) fn get_inference_context(&self, node: P<Node>) -> Option<P<InferenceContext>> {
        if let Some(result) = self.inference_context_infos.iter().rev().into_iter().find_map(|v| {
            if crate::is_node_descendant_of(Some(node), v.node) { Some(v.context) } else { None }
        }) {
            return result;
        }
        None
    }

    // checker.go:31463
    pub(crate) fn get_type_facts(&mut self, t: P<Type>, mask: TypeFacts) -> TypeFacts {
        self.get_type_facts_worker(t, mask) & mask
    }

    // checker.go:31467
    pub(crate) fn has_type_facts(&mut self, t: P<Type>, mask: TypeFacts) -> bool {
        self.get_type_facts(t, mask) != TypeFacts::None
    }

    // checker.go:31471
    fn get_type_facts_worker(&mut self, t: P<Type>, caller_only_needs: TypeFacts) -> TypeFacts {
        let mut t = t;
        if t.flags().intersects(TypeFlags::Intersection | TypeFlags::Instantiable) {
            t = match self.get_base_constraint_of_type(t) {
                Some(t) => t,
                None => self.unknown_type,
            };
        }
        let flags = t.flags();
        if flags.intersects(TypeFlags::String | TypeFlags::StringMapping) {
            if self.strict_null_checks {
                return TypeFacts::StringStrictFacts;
            }
            return TypeFacts::StringFacts;
        } else if flags.intersects(TypeFlags::StringLiteral | TypeFlags::TemplateLiteral) {
            let is_empty = flags.intersects(TypeFlags::StringLiteral)
                && get_string_literal_value(t).is_empty();
            if self.strict_null_checks {
                if is_empty {
                    return TypeFacts::EmptyStringStrictFacts;
                }
                return TypeFacts::NonEmptyStringStrictFacts;
            }
            if is_empty {
                return TypeFacts::EmptyStringFacts;
            }
            return TypeFacts::NonEmptyStringFacts;
        } else if flags.intersects(TypeFlags::Number | TypeFlags::Enum) {
            if self.strict_null_checks {
                return TypeFacts::NumberStrictFacts;
            }
            return TypeFacts::NumberFacts;
        } else if flags.intersects(TypeFlags::NumberLiteral) {
            let is_zero = get_number_literal_value(t).0 == 0.0;
            if self.strict_null_checks {
                if is_zero {
                    return TypeFacts::ZeroNumberStrictFacts;
                }
                return TypeFacts::NonZeroNumberStrictFacts;
            }
            if is_zero {
                return TypeFacts::ZeroNumberFacts;
            }
            return TypeFacts::NonZeroNumberFacts;
        } else if flags.intersects(TypeFlags::BigInt) {
            if self.strict_null_checks {
                return TypeFacts::BigIntStrictFacts;
            }
            return TypeFacts::BigIntFacts;
        } else if flags.intersects(TypeFlags::BigIntLiteral) {
            let is_zero = is_zero_big_int(t);
            if self.strict_null_checks {
                if is_zero {
                    return TypeFacts::ZeroBigIntStrictFacts;
                }
                return TypeFacts::NonZeroBigIntStrictFacts;
            }
            if is_zero {
                return TypeFacts::ZeroBigIntFacts;
            }
            return TypeFacts::NonZeroBigIntFacts;
        } else if flags.intersects(TypeFlags::Boolean) {
            if self.strict_null_checks {
                return TypeFacts::BooleanStrictFacts;
            }
            return TypeFacts::BooleanFacts;
        } else if flags.intersects(TypeFlags::BooleanLike) {
            let is_false = t == self.false_type || t == self.regular_false_type;
            if self.strict_null_checks {
                if is_false {
                    return TypeFacts::FalseStrictFacts;
                }
                return TypeFacts::TrueStrictFacts;
            }
            if is_false {
                return TypeFacts::FalseFacts;
            }
            return TypeFacts::TrueFacts;
        } else if flags.intersects(TypeFlags::Object) {
            if (caller_only_needs & self.possible_object_type_facts()) == TypeFacts::None {
                // If the caller doesn't care about any of the facts that we could possibly produce,
                // return zero so we can skip resolving members.
                return TypeFacts::None;
            }
            if t.object_flags().intersects(ObjectFlags::Anonymous) && self.is_empty_object_type(t) {
                if self.strict_null_checks {
                    return TypeFacts::EmptyObjectStrictFacts;
                }
                return TypeFacts::EmptyObjectFacts;
            } else if self.is_function_object_type(t) {
                if self.strict_null_checks {
                    return TypeFacts::FunctionStrictFacts;
                }
                return TypeFacts::FunctionFacts;
            } else if self.strict_null_checks {
                return TypeFacts::ObjectStrictFacts;
            }
            return TypeFacts::ObjectFacts;
        } else if flags.intersects(TypeFlags::Void) {
            return TypeFacts::VoidFacts;
        } else if flags.intersects(TypeFlags::Undefined) {
            return TypeFacts::UndefinedFacts;
        } else if flags.intersects(TypeFlags::Null) {
            return TypeFacts::NullFacts;
        } else if flags.intersects(TypeFlags::ESSymbolLike) {
            if self.strict_null_checks {
                return TypeFacts::SymbolStrictFacts;
            }
            return TypeFacts::SymbolFacts;
        } else if flags.intersects(TypeFlags::NonPrimitive) {
            if self.strict_null_checks {
                return TypeFacts::ObjectStrictFacts;
            }
            return TypeFacts::ObjectFacts;
        } else if flags.intersects(TypeFlags::Never) {
            return TypeFacts::None;
        } else if flags.intersects(TypeFlags::Union) {
            // tsrs-only: a union of non-nullable kinds (`ObjectFlags::NonNullableUnion`, set by the union builder)
            // has none of the nullability facts, and when the caller needs no object fact every constituent would
            // return a constant without the facts asked for and resolve nothing, so the walk is skipped. The
            // caller masks the result with `caller_only_needs`, as it does the object case's early return.
            if t.object_flags().intersects(ObjectFlags::NonNullableUnion)
                && (caller_only_needs & self.possible_object_type_facts()) == TypeFacts::None
            {
                if cfg!(debug_assertions) {
                    let walked = self.get_union_type_facts(t, caller_only_needs);
                    assert!(
                        (walked & caller_only_needs) == TypeFacts::None,
                        "a union of non-nullable kinds has a fact its summary rules out"
                    );
                }
                return TypeFacts::None;
            }
            return self.get_union_type_facts(t, caller_only_needs);
        } else if flags.intersects(TypeFlags::Intersection) {
            return self.get_intersection_type_facts(t, caller_only_needs);
        }
        TypeFacts::UnknownFacts
    }

    /// The facts `get_type_facts_worker` can return for an object type (Go's `possibleFacts`).
    #[inline]
    fn possible_object_type_facts(&self) -> TypeFacts {
        if self.strict_null_checks {
            TypeFacts::EmptyObjectStrictFacts
                | TypeFacts::FunctionStrictFacts
                | TypeFacts::ObjectStrictFacts
        } else {
            TypeFacts::EmptyObjectFacts | TypeFacts::FunctionFacts | TypeFacts::ObjectFacts
        }
    }

    /// The union case of `getTypeFactsWorker`: the facts of the constituents, or-ed.
    fn get_union_type_facts(&mut self, t: P<Type>, caller_only_needs: TypeFacts) -> TypeFacts {
        t.types().iter().fold(TypeFacts::None, |facts, &t| {
            facts | self.get_type_facts_worker(t, caller_only_needs)
        })
    }

    // checker.go:31607
    fn get_intersection_type_facts(
        &mut self,
        t: P<Type>,
        caller_only_needs: TypeFacts,
    ) -> TypeFacts {
        // When an intersection contains a primitive type we ignore object type constituents as they are
        // presumably type tags. For example, in string & { __kind__: "name" } we ignore the object type.
        let ignore_objects = self.maybe_type_of_kind(t, TypeFlags::Primitive);
        // When computing the type facts of an intersection type, certain type facts are computed as `and`
        // and others are computed as `or`.
        let mut ored_facts = TypeFacts::None;
        let mut anded_facts = TypeFacts::All;
        for &t in t.types() {
            if !(ignore_objects && t.flags().intersects(TypeFlags::Object)) {
                let f = self.get_type_facts_worker(t, caller_only_needs);
                ored_facts |= f;
                anded_facts &= f;
            }
        }
        ored_facts & TypeFacts::OrFactsMask | anded_facts & TypeFacts::AndFactsMask
    }
}

// checker.go:31625
pub(crate) fn is_zero_big_int(t: P<Type>) -> bool {
    get_big_int_literal_value(t) == jsnum::PseudoBigInt::default()
}

impl Checker {
    // checker.go:31629
    pub(crate) fn is_function_object_type(&mut self, t: P<Type>) -> bool {
        if t.object_flags().intersects(ObjectFlags::EvolvingArray) {
            return false;
        }
        // We do a quick check for a "bind" property before performing the more expensive subtype
        // check. This gives us a quicker out in the common case where an object type is not a function.
        !self.signatures_of_structured_type(t, SignatureKind::Call).is_empty()
            || !self.signatures_of_structured_type(t, SignatureKind::Construct).is_empty()
            || {
                let global_function_type = self.global_function_type;
                self.get_member_of_structured_type(t, "bind").is_some()
                    && self.is_type_subtype_of(t, global_function_type)
            }
    }

    // checker.go:31639
    pub(crate) fn get_type_with_facts(&mut self, t: P<Type>, include: TypeFacts) -> P<Type> {
        self.filter_type(t, |c, t| c.has_type_facts(t, include))
    }

    // This function is similar to getTypeWithFacts, except that in strictNullChecks mode it replaces type
    // unknown with the union {} | null | undefined (and reduces that accordingly), and it intersects remaining
    // instantiable types with {}, {} | null, or {} | undefined in order to remove null and/or undefined.
    // checker.go:31648
    pub(crate) fn get_adjusted_type_with_facts(&mut self, t: P<Type>, facts: TypeFacts) -> P<Type> {
        let source = if self.strict_null_checks && t.flags().intersects(TypeFlags::Unknown) {
            self.unknown_union_type
        } else {
            t
        };
        let with_facts = self.get_type_with_facts(source, facts);
        let reduced = self.recombine_unknown_type(with_facts);
        if self.strict_null_checks {
            if facts == TypeFacts::NEUndefined {
                let null_type = self.null_type;
                return self.remove_nullable_by_intersection(
                    reduced,
                    TypeFacts::EQUndefined,
                    TypeFacts::EQNull,
                    TypeFacts::IsNull,
                    null_type,
                );
            } else if facts == TypeFacts::NENull {
                let undefined_type = self.undefined_type;
                return self.remove_nullable_by_intersection(
                    reduced,
                    TypeFacts::EQNull,
                    TypeFacts::EQUndefined,
                    TypeFacts::IsUndefined,
                    undefined_type,
                );
            } else if facts == TypeFacts::NEUndefinedOrNull || facts == TypeFacts::Truthy {
                return self
                    .map_type(reduced, |c, t| {
                        if c.has_type_facts(t, TypeFacts::EQUndefinedOrNull) {
                            return Some(c.get_global_non_nullable_type_instantiation(t));
                        }
                        Some(t)
                    })
                    .unwrap();
            }
        }
        reduced
    }

    // checker.go:31668
    fn remove_nullable_by_intersection(
        &mut self,
        t: P<Type>,
        target_facts: TypeFacts,
        other_facts: TypeFacts,
        other_includes_facts: TypeFacts,
        other_type: P<Type>,
    ) -> P<Type> {
        let facts = self.get_type_facts(
            t,
            TypeFacts::EQUndefined | TypeFacts::EQNull | TypeFacts::IsUndefined | TypeFacts::IsNull,
        );
        // Simply return the type if it never compares equal to the target nullable.
        if !facts.intersects(target_facts) {
            return t;
        }
        // By default we intersect with a union of {} and the opposite nullable.
        let empty_object_type = self.empty_object_type;
        let empty_and_other_union = self.get_union_type(&[empty_object_type, other_type]);
        // For each constituent type that can compare equal to the target nullable, intersect with the above union
        // if the type doesn't already include the opposite nullable and the constituent can compare equal to the
        // opposite nullable; otherwise, just intersect with {}.
        self.map_type(t, |c, t| {
            if c.has_type_facts(t, target_facts) {
                if !facts.intersects(other_includes_facts) && c.has_type_facts(t, other_facts) {
                    return Some(c.get_intersection_type(&[t, empty_and_other_union]));
                }
                let empty_object_type = c.empty_object_type;
                return Some(c.get_intersection_type(&[t, empty_object_type]));
            }
            Some(t)
        })
        .unwrap()
    }

    // checker.go:31690
    pub(crate) fn recombine_unknown_type(&self, t: P<Type>) -> P<Type> {
        if t == self.unknown_union_type {
            return self.unknown_type;
        }
        t
    }

    // checker.go:31697
    fn get_global_non_nullable_type_instantiation(&mut self, t: P<Type>) -> P<Type> {
        let alias = self.get_global_non_nullable_type_alias_or_nil();
        if let Some(alias) = alias {
            return self.get_type_alias_instantiation(alias, &[t], None);
        }
        let empty_object_type = self.empty_object_type;
        self.get_intersection_type(&[t, empty_object_type])
    }

    // checker.go:31705
    pub(crate) fn convert_auto_to_any(&self, t: P<Type>) -> P<Type> {
        if t == self.auto_type {
            return self.any_type;
        } else if t == self.auto_array_type {
            return self.any_array_type;
        }
        t
    }

    // Gets the "awaited type" of a type.
    // @param type The type to await.
    // @param withAlias When `true`, wraps the "awaited type" in `Awaited<T>` if needed.
    // @remarks The "awaited type" of an expression is its "promised type" if the expression is a
    // Promise-like type; otherwise, it is the type of the expression. This is used to reflect
    // The runtime behavior of the `await` keyword.
    // checker.go:31721
    pub(crate) fn check_awaited_type(
        &mut self,
        t: P<Type>,
        with_alias: bool,
        error_node: P<Node>,
        diagnostic_message: &'static Message,
    ) -> P<Type> {
        let awaited_type = if with_alias {
            self.get_awaited_type_ex(t, Some(error_node), Some(diagnostic_message), &[])
        } else {
            self.get_awaited_type_no_alias_ex(t, Some(error_node), Some(diagnostic_message), &[])
        };
        if let Some(awaited_type) = awaited_type {
            return awaited_type;
        }
        self.error_type
    }

    // Gets the "awaited type" of a type.
    //
    // The "awaited type" of an expression is its "promised type" if the expression is a
    // Promise-like type; otherwise, it is the type of the expression. If the "promised
    // type" is itself a Promise-like, the "promised type" is recursively unwrapped until a
    // non-promise type is found.
    //
    // This is used to reflect the runtime behavior of the `await` keyword.
    // checker.go:31742
    pub(crate) fn get_awaited_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_awaited_type_ex(t, None, None, &[])
    }

    // checker.go:31746
    pub(crate) fn get_awaited_type_ex(
        &mut self,
        t: P<Type>,
        error_node: Option<P<Node>>,
        diagnostic_message: Option<&'static Message>,
        args: &[&dyn Display],
    ) -> Option<P<Type>> {
        let awaited_type =
            self.get_awaited_type_no_alias_ex(t, error_node, diagnostic_message, args);
        if let Some(awaited_type) = awaited_type {
            return Some(self.create_awaited_type_if_needed(awaited_type));
        }
        None
    }

    // Gets the "awaited type" of a type without introducing an `Awaited<T>` wrapper.
    // checker.go:31755
    pub(crate) fn get_awaited_type_no_alias(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_awaited_type_no_alias_ex(t, None, None, &[])
    }

    // checker.go:31759
    fn get_awaited_type_no_alias_ex(
        &mut self,
        t: P<Type>,
        error_node: Option<P<Node>>,
        diagnostic_message: Option<&'static Message>,
        args: &[&dyn Display],
    ) -> Option<P<Type>> {
        if is_type_any(Some(t)) {
            return Some(t);
        }
        // If this is already an `Awaited<T>`, just return it. This avoids `Awaited<Awaited<T>>` in higher-order
        if self.is_awaited_type_instantiation(t) {
            return Some(t);
        }
        // If we've already cached an awaited type, return a possible `Awaited<T>` for it.
        let key = CachedTypeKey { kind: CachedTypeKind::AwaitedType, type_id: t.id };
        if let Some(&awaited_type) = self.cached_types.get(&key) {
            return Some(awaited_type);
        }
        // For a union, get a union of the awaited types of each constituent.
        if t.flags().intersects(TypeFlags::Union) {
            if self.awaited_type_stack.contains(&t) {
                if error_node.is_some() {
                    self.error(error_node, &diagnostics::TYPE_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_THE_FULFILLMENT_CALLBACK_OF_ITS_OWN_THEN_METHOD, &[]);
                }
                return None;
            }
            self.awaited_type_stack.push(t);
            let mapped = self.map_type(t, |c, t| {
                c.get_awaited_type_no_alias_ex(t, error_node, diagnostic_message, args)
            });
            self.awaited_type_stack.pop();
            // Go stores the (possibly nil) result; a nil map entry reads back as a cache miss.
            match mapped {
                Some(mapped) => {
                    self.cached_types.insert(key, mapped);
                }
                None => {
                    self.cached_types.remove(&key);
                }
            }
            return mapped;
        }
        // If `type` is generic and should be wrapped in `Awaited<T>`, return it.
        if self.is_awaited_type_needed(t) {
            self.cached_types.insert(key, t);
            return Some(t);
        }
        let mut this_type_for_error: Option<P<Type>> = None;
        let promised_type = self.get_promised_type_of_promise_ex(
            t,
            None, /*errorNode*/
            Some(&mut this_type_for_error),
        );
        if let Some(promised_type) = promised_type {
            if t == promised_type || self.awaited_type_stack.contains(&promised_type) {
                // Verify that we don't have a bad actor in the form of a promise whose
                // promised type is the same as the promise type, or a mutually recursive
                // promise. If so, we return undefined as we cannot guess the shape. If this
                // were the actual case in the JavaScript, this Promise would never resolve.
                //
                // An example of a bad actor with a singly-recursive promise type might
                // be:
                //
                //  interface BadPromise {
                //      then(
                //          onfulfilled: (value: BadPromise) => any,
                //          onrejected: (error: any) => any): BadPromise;
                //  }
                //
                // The above interface will pass the PromiseLike check, and return a
                // promised type of `BadPromise`. Since this is a self reference, we
                // don't want to keep recursing ad infinitum.
                //
                // An example of a bad actor in the form of a mutually-recursive
                // promise type might be:
                //
                //  interface BadPromiseA {
                //      then(
                //          onfulfilled: (value: BadPromiseB) => any,
                //          onrejected: (error: any) => any): BadPromiseB;
                //  }
                //
                //  interface BadPromiseB {
                //      then(
                //          onfulfilled: (value: BadPromiseA) => any,
                //          onrejected: (error: any) => any): BadPromiseA;
                //  }
                //
                if error_node.is_some() {
                    self.error(error_node, &diagnostics::TYPE_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_THE_FULFILLMENT_CALLBACK_OF_ITS_OWN_THEN_METHOD, &[]);
                }
                return None;
            }
            // Keep track of the type we're about to unwrap to avoid bad recursive promise types.
            // See the comments above for more information.
            self.awaited_type_stack.push(t);
            let awaited_type = self.get_awaited_type_no_alias_ex(
                promised_type,
                error_node,
                diagnostic_message,
                args,
            );
            self.awaited_type_stack.pop();
            let awaited_type = awaited_type?;
            self.cached_types.insert(key, awaited_type);
            return Some(awaited_type);
        }
        // The type was not a promise, so it could not be unwrapped any further.
        // As long as the type does not have a callable "then" property, it is
        // safe to return the type; otherwise, an error is reported and we return
        // undefined.
        //
        // An example of a non-promise "thenable" might be:
        //
        //  await { then(): void {} }
        //
        // The "thenable" does not match the minimal definition for a promise. When
        // a Promise/A+-compatible or ES6 promise tries to adopt this value, the promise
        // will never settle. We treat this as an error to help flag an early indicator
        // of a runtime problem. If the user wants to return this value from an async
        // function, they would need to wrap it in some other value. If they want it to
        // be treated as a promise, they can cast to <any>.
        if self.is_thenable_type(t) {
            if let Some(error_node) = error_node {
                let mut diagnostic = None;
                if let Some(this_type_for_error) = this_type_for_error {
                    let t_str = self.type_to_string_exported(t);
                    let this_str = self.type_to_string_exported(this_type_for_error);
                    diagnostic = Some(new_diagnostic_for_node(Some(error_node), Some(&diagnostics::THE_THIS_CONTEXT_OF_TYPE_0_IS_NOT_ASSIGNABLE_TO_METHOD_S_THIS_OF_TYPE_1), &[&t_str, &this_str]));
                }
                self.add_diagnostic(new_diagnostic_chain_for_node(
                    diagnostic,
                    error_node,
                    diagnostic_message,
                    args,
                ));
            }
            return None;
        }
        self.cached_types.insert(key, t);
        Some(t)
    }

    // checker.go:31873
    fn is_awaited_type_instantiation(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Conditional) {
            let awaited_symbol = self.get_global_awaited_symbol_or_nil();
            return awaited_symbol.is_some()
                && t.alias().is_some_and(|alias| {
                    alias.symbol.get() == awaited_symbol && alias.type_arguments.get().len() == 1
                });
        }
        false
    }

    // checker.go:31881
    fn is_awaited_type_needed(&mut self, t: P<Type>) -> bool {
        // If this is already an `Awaited<T>`, we shouldn't wrap it. This helps to avoid `Awaited<Awaited<T>>` in higher-order.
        if is_type_any(Some(t)) || self.is_awaited_type_instantiation(t) {
            return false;
        }
        // We only need `Awaited<T>` if `T` contains possibly non-primitive types.
        if self.is_generic_object_type(t) {
            let base_constraint = self.get_base_constraint_of_type(t);
            // We only need `Awaited<T>` if `T` is a type variable that has no base constraint, or the base constraint of `T` is `any`, `unknown`, `{}`, `object`,
            // or is promise-like.
            if let Some(base_constraint) = base_constraint {
                return base_constraint.flags().intersects(TypeFlags::AnyOrUnknown)
                    || self.is_empty_object_type(base_constraint)
                    || some_type(self, base_constraint, super::checker::Checker::is_thenable_type);
            }
            return self.maybe_type_of_kind(t, TypeFlags::TypeVariable);
        }
        false
    }

    // checker.go:31899
    fn create_awaited_type_if_needed(&mut self, t: P<Type>) -> P<Type> {
        // We wrap type `T` in `Awaited<T>` based on the following conditions:
        // - `T` is not already an `Awaited<U>`, and
        // - `T` is generic, and
        // - One of the following applies:
        //   - `T` has no base constraint, or
        //   - The base constraint of `T` is `any`, `unknown`, `object`, or `{}`, or
        //   - The base constraint of `T` is an object type with a callable `then` method.
        if self.is_awaited_type_needed(t) {
            let awaited_type = self.try_create_awaited_type(t);
            if let Some(awaited_type) = awaited_type {
                return awaited_type;
            }
        }
        t
    }

    // checker.go:31916
    fn try_create_awaited_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        // Nothing to do if `Awaited<T>` doesn't exist
        let awaited_symbol = self.get_global_awaited_symbol();
        if let Some(awaited_symbol) = awaited_symbol {
            // Unwrap unions that may contain `Awaited<T>`, otherwise its possible to manufacture an `Awaited<Awaited<T> | U>` where
            // an `Awaited<T | U>` would suffice.
            let unwrapped = self.unwrap_awaited_type(t);
            return Some(self.get_type_alias_instantiation(awaited_symbol, &[unwrapped], None));
        }
        None
    }

    // For a generic `Awaited<T>`, gets `T`.
    // checker.go:31928
    pub(crate) fn unwrap_awaited_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Union) {
            return self.map_type(t, |c, t| Some(c.unwrap_awaited_type(t))).unwrap();
        } else if self.is_awaited_type_instantiation(t) {
            return t.alias().unwrap().type_arguments.get()[0];
        }
        t
    }

    // checker.go:31938
    fn is_thenable_type(&mut self, t: P<Type>) -> bool {
        let constraint = self.get_base_constraint_or_type(t);
        if self.all_types_assignable_to_kind(constraint, TypeFlags::Primitive | TypeFlags::Never) {
            // primitive types cannot be considered "thenable" since they are not objects.
            return false;
        }
        let then_function = self.get_type_of_property_of_type(t, "then");
        let Some(then_function) = then_function else {
            return false;
        };
        let then_type = self.get_type_with_facts(then_function, TypeFacts::NEUndefinedOrNull);
        !self.get_signatures_of_type(then_type, SignatureKind::Call).is_empty()
    }

    // checker.go:31947
    pub(crate) fn get_awaited_type_of_promise(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_awaited_type_of_promise_ex(t, None, None, &[])
    }

    // checker.go:31951
    fn get_awaited_type_of_promise_ex(
        &mut self,
        t: P<Type>,
        error_node: Option<P<Node>>,
        diagnostic_message: Option<&'static Message>,
        args: &[&dyn Display],
    ) -> Option<P<Type>> {
        let promised_type = self.get_promised_type_of_promise_ex(t, error_node, None);
        if let Some(promised_type) = promised_type {
            return self.get_awaited_type_ex(promised_type, error_node, diagnostic_message, args);
        }
        None
    }

    // Check if a parameter or catch variable (or their bindings elements) is assigned anywhere
    // checker.go:31960
    pub(crate) fn is_some_symbol_assigned(&mut self, root_declaration: P<Node>) -> bool {
        self.is_some_symbol_assigned_worker(root_declaration.name().unwrap())
    }

    // checker.go:31964
    fn is_some_symbol_assigned_worker(&mut self, node: P<Node>) -> bool {
        if node.kind() == Kind::Identifier {
            let symbol = self.get_symbol_of_declaration(node.parent().unwrap()).unwrap();
            return self.is_symbol_assigned(symbol);
        }
        node.elements()
            .iter()
            .any(|&e| e.name().is_some() && self.is_some_symbol_assigned_worker(e.name().unwrap()))
    }

    // checker.go:31973
    pub(crate) fn get_target_type(t: P<Type>) -> Option<P<Type>> {
        if t.object_flags().intersects(ObjectFlags::Reference) {
            return t.as_type_reference().target.get();
        }
        Some(t)
    }

    // checker.go:31980
    pub(crate) fn get_narrowable_type_for_reference(
        &mut self,
        t: P<Type>,
        reference: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let mut t = t;
        if Self::is_no_infer_type(t) {
            t = t.as_substitution_type().base_type.get().unwrap();
        }
        // When the type of a reference is or contains an instantiable type with a union type constraint, and
        // when the reference is in a constraint position (where it is known we'll obtain the apparent type) or
        // has a contextual type containing no top-level instantiables (meaning constraints will determine
        // assignability), we substitute constraints for all instantiables in the type of the reference to give
        // control flow analysis an opportunity to narrow it further. For example, for a reference of a type
        // parameter type 'T extends string | undefined' with a contextual type 'string', we substitute
        // 'string | undefined' to give control flow analysis the opportunity to narrow to type 'string'.
        let substitute_constraints = !check_mode.intersects(CheckMode::Inferential)
            && some_type(self, t, super::checker::Checker::is_generic_type_with_union_constraint)
            && (self.is_constraint_position(t, reference)
                || self.has_contextual_type_with_no_generic_types(reference, check_mode));
        if substitute_constraints {
            return self.map_type(t, |c, t| Some(c.get_base_constraint_or_type(t))).unwrap();
        }
        t
    }

    /// `get_narrowable_type_for_reference(t, reference, CheckMode::Normal)` for a flow walk: also says whether the
    /// answer read the reference's position, which the flow memo's key does not have.
    pub(crate) fn get_narrowable_type_for_flow_reference(
        &mut self,
        t: P<Type>,
        reference: P<Node>,
    ) -> (P<Type>, bool) {
        let mut t = t;
        if Self::is_no_infer_type(t) {
            t = t.as_substitution_type().base_type.get().unwrap();
        }
        let position_read =
            some_type(self, t, super::checker::Checker::is_generic_type_with_union_constraint);
        let substitute_constraints = position_read
            && (self.is_constraint_position(t, reference)
                || self.has_contextual_type_with_no_generic_types(reference, CheckMode::Normal));
        if substitute_constraints {
            return (
                self.map_type(t, |c, t| Some(c.get_base_constraint_or_type(t))).unwrap(),
                true,
            );
        }
        (t, position_read)
    }

    // checker.go:31998
    fn is_constraint_position(&mut self, t: P<Type>, node: P<Node>) -> bool {
        let parent = node.parent().unwrap();
        // In an element access obj[x], we consider obj to be in a constraint position, except when obj is of
        // a generic type without a nullable constraint and x is a generic type. This is because when both obj
        // and x are of generic types T and K, we want the resulting type to be T[K].
        ast::is_property_access_expression(parent)
            || ast::is_qualified_name(parent)
            || (ast::is_call_expression(parent) || ast::is_new_expression(parent))
                && parent.expression() == Some(node)
            || ast::is_element_access_expression(parent)
                && parent.expression() == Some(node)
                && !(some_type(
                    self,
                    t,
                    super::checker::Checker::is_generic_type_without_nullable_constraint,
                ) && {
                    let index_type = self.get_type_of_expression(
                        parent.as_element_access_expression().argument_expression,
                    );
                    self.is_generic_index_type(index_type)
                })
    }

    // checker.go:32008
    fn is_generic_type_with_union_constraint(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Intersection) {
            return t.types().iter().any(|&t| self.is_generic_type_with_union_constraint(t));
        }
        t.flags().intersects(TypeFlags::Instantiable)
            && self
                .get_base_constraint_or_type(t)
                .flags()
                .intersects(TypeFlags::Nullable | TypeFlags::Union)
    }

    // checker.go:32015
    fn is_generic_type_without_nullable_constraint(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Intersection) {
            return t.types().iter().any(|&t| self.is_generic_type_without_nullable_constraint(t));
        }
        t.flags().intersects(TypeFlags::Instantiable) && {
            let constraint = self.get_base_constraint_or_type(t);
            !self.maybe_type_of_kind(constraint, TypeFlags::Nullable)
        }
    }

    // checker.go:32022
    fn has_contextual_type_with_no_generic_types(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> bool {
        // Computing the contextual type for a child of a JSX element involves resolving the type of the
        // element's tag name, so we exclude that here to avoid circularities.
        // If check mode has `CheckMode.RestBindingElement`, we skip binding pattern contextual types,
        // as we want the type of a rest element to be generic when possible.
        let parent = node.parent().unwrap();
        if (ast::is_identifier(node)
            || ast::is_property_access_expression(node)
            || ast::is_element_access_expression(node))
            && !((ast::is_jsx_opening_element(parent) || ast::is_jsx_self_closing_element(parent))
                && parent.tag_name() == node)
        {
            let context_flags = if check_mode.intersects(CheckMode::RestBindingElement) {
                ContextFlags::SkipBindingPatterns
            } else {
                ContextFlags::None
            };
            let contextual_type = self.get_contextual_type(node, context_flags);
            if let Some(contextual_type) = contextual_type {
                return !self.is_generic_type(contextual_type);
            }
        }
        false
    }

    // checker.go:32037
    pub(crate) fn get_non_undefined_type(&mut self, t: P<Type>) -> P<Type> {
        let mut type_or_constraint = t;
        if some_type(self, t, super::checker::Checker::is_generic_type_with_undefined_constraint) {
            type_or_constraint = self
                .map_type(t, |c, t| {
                    if t.flags().intersects(TypeFlags::Instantiable) {
                        return Some(c.get_base_constraint_or_type(t));
                    }
                    Some(t)
                })
                .unwrap();
        }
        self.get_type_with_facts(type_or_constraint, TypeFacts::NEUndefined)
    }

    // checker.go:32050
    fn is_generic_type_with_undefined_constraint(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Instantiable) {
            let constraint = self.get_base_constraint_of_type(t);
            if let Some(constraint) = constraint {
                return self.maybe_type_of_kind(constraint, TypeFlags::Undefined);
            }
        }
        false
    }

    // checker.go:32060
    pub(crate) fn get_actual_type_variable(&mut self, t: P<Type>) -> Option<P<Type>> {
        if t.flags().intersects(TypeFlags::Substitution) {
            return self
                .get_actual_type_variable(t.as_substitution_type().base_type.get().unwrap());
        }
        if t.flags().intersects(TypeFlags::IndexedAccess) {
            let indexed_access = t.as_indexed_access_type();
            let object_type =
                self.get_actual_type_variable(indexed_access.object_type.get().unwrap());
            let index_type =
                self.get_actual_type_variable(indexed_access.index_type.get().unwrap());
            if object_type != indexed_access.object_type.get()
                || index_type != indexed_access.index_type.get()
            {
                return Some(
                    self.get_indexed_access_type(object_type.unwrap(), index_type.unwrap()),
                );
            }
        }
        get_non_distributed_type_parameter(t)
    }

    // checker.go:32074
    pub fn get_symbol_at_location_exported(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        // !!!
        // const node = getParseTreeNode(nodeIn);

        // set ignoreErrors: true because any lookups invoked by the API shouldn't cause any new errors
        self.get_symbol_at_location(
            ast::get_reparsed_node_for_node(node).unwrap(),
            true, /*ignoreErrors*/
        )
    }

    // Returns the symbol associated with a given AST node. Do *not* use this function in the checker itself! It should
    // be used only by the language service and external tools. The semantics of the function are deliberately "fuzzy"
    // and aim to just return *some* symbol for the node. To obtain the symbol associated with a node for type checking
    // purposes, use appropriate function for the context, e.g. `getResolvedSymbol` for an expression identifier,
    // `getSymbolOfDeclaration` for a declaration, etc.
    // checker.go:32087
    pub(crate) fn get_symbol_at_location(
        &mut self,
        node: P<Node>,
        ignore_errors: bool,
    ) -> Option<P<Symbol>> {
        if ast::is_source_file(node) {
            if ast::is_external_or_common_js_module(node.as_source_file_p()) {
                return Some(self.get_merged_symbol(node.symbol().unwrap()));
            }
            return None;
        }
        let parent = node.parent().unwrap();
        let grand_parent = parent.parent();

        if node.flags().intersects(NodeFlags::InWithStatement) {
            // We cannot answer semantic questions within a with block, do not proceed any further
            return None;
        }

        if ast::is_declaration_name_or_import_property_name(node) {
            // This is a declaration, call getSymbolOfNode
            let parent_symbol = self.get_symbol_of_declaration(parent);
            if ast::is_import_or_export_specifier(parent) && parent.property_name() == Some(node) {
                return self.get_immediate_aliased_symbol(parent_symbol.unwrap());
            }
            return parent_symbol;
        } else if ast::is_literal_computed_property_declaration_name(node) {
            return self.get_symbol_of_declaration(grand_parent.unwrap());
        }

        if ast::is_identifier(node) {
            if is_in_right_side_of_import_or_export_assignment(node) {
                return self.get_symbol_of_name_or_property_access_expression(node);
            } else if ast::is_binding_element(parent)
                && ast::is_object_binding_pattern(grand_parent.unwrap())
                && Some(node) == parent.property_name()
            {
                let type_of_pattern = self.get_type_of_node(grand_parent.unwrap());
                if let Some(property_declaration) =
                    self.get_property_of_type(type_of_pattern, node.text())
                {
                    return Some(property_declaration);
                }
            } else if ast::is_meta_property(parent) && parent.name() == Some(node) {
                let meta_prop = parent.as_meta_property();
                if meta_prop.keyword_token == Kind::NewKeyword && node.text() == "target" {
                    // `target` in `new.target`
                    return self.check_new_target_meta_property(parent).symbol();
                }
                // The `meta` in `import.meta` could be given `getTypeOfNode(parent).symbol` (the `ImportMeta` interface symbol), but
                // we have a fake expression type made for other reasons already, whose transient `meta`
                // member should more exactly be the kind of (declarationless) symbol we want.
                // (See #44364 and #45031 for relevant implementation PRs)
                if meta_prop.keyword_token == Kind::ImportKeyword && node.text() == "meta" {
                    let import_meta_type = self.get_global_import_meta_expression_type();
                    return import_meta_type.as_object_type().members()?.lookup("meta");
                }
                // no other meta properties are valid syntax, thus no others should have symbols
                return None;
            } else if ast::is_jsdoc_parameter_tag(parent) && parent.name() == Some(node) {
                let func = ast::get_node_at_position(
                    ast::get_source_file_of_node(node).unwrap(),
                    node.pos().expect_source("parsed nodes have source positions"),
                    false,
                );
                if ast::is_function_like(func)
                    && let Some(result) = func.parameters().iter().find_map(|&param| {
                        if ast::is_identifier(param.name().unwrap())
                            && param.name().unwrap().text() == node.text()
                        {
                            Some(self.get_symbol_of_node(param))
                        } else {
                            None
                        }
                    })
                {
                    return result;
                }
            }
        }

        match node.kind() {
            Kind::Identifier
            | Kind::PrivateIdentifier
            | Kind::PropertyAccessExpression
            | Kind::QualifiedName
            | Kind::ThisKeyword
            | Kind::ThisType => {
                if node.kind() != Kind::ThisKeyword
                    && node.kind() != Kind::ThisType
                    && !ast::is_this_in_type_query(node)
                {
                    return self.get_symbol_of_name_or_property_access_expression(node);
                }
                // fallthrough (case ast.KindThisKeyword)
                if node.kind() != Kind::ThisType {
                    let container = Self::get_this_container(
                        node, false, /*includeArrowFunctions*/
                        false, /*includeClassComputedPropertyName*/
                    );
                    if ast::is_function_like(container) {
                        let sig = self.get_signature_from_declaration(container.unwrap());
                        if sig.this_parameter().is_some() {
                            return sig.this_parameter();
                        }
                    }
                    if ast::is_in_expression_context(node) {
                        return self.check_expression(node).symbol();
                    }
                }
                // fallthrough (case ast.KindThisType)
                self.get_type_from_this_type_node(node).symbol()
            }
            Kind::SuperKeyword | Kind::MetaProperty => self.check_expression(node).symbol(),
            Kind::ConstructorKeyword => {
                // constructor keyword for an overload, should take us to the definition if it exist
                let constructor_declaration = parent;
                if constructor_declaration.kind() == Kind::Constructor {
                    return constructor_declaration.parent().unwrap().symbol();
                }
                None
            }
            Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral | Kind::NumericLiteral => {
                if node.kind() != Kind::NumericLiteral {
                    // 1). import x = require("./mo/*gotToDefinitionHere*/d")
                    // 2). External module name in an import declaration
                    // 3). Require in Javascript
                    // 4). type A = import("./f/*gotToDefinitionHere*/oo")
                    let gp_is = |f: fn(P<Node>) -> bool| grand_parent.is_some_and(f);
                    if (gp_is(ast::is_external_module_import_equals_declaration)
                        && ast::get_external_module_import_equals_declaration_expression(
                            grand_parent.unwrap(),
                        ) == Some(node))
                        || ((parent.kind() == Kind::ImportDeclaration
                            || parent.kind() == Kind::JSImportDeclaration
                            || parent.kind() == Kind::ExportDeclaration)
                            && ast::get_external_module_name(parent) == Some(node))
                        || gp_is(ast::is_variable_declaration_initialized_to_require)
                        || ast::is_import_call(parent)
                        || (ast::is_literal_type_node(parent)
                            && gp_is(ast::is_literal_import_type_node)
                            && grand_parent.unwrap().as_import_type_node().argument == parent)
                    {
                        let import_attributes_type =
                            self.get_import_attributes_type_for_module_specifier(node);
                        return self.resolve_external_module_name(
                            node,
                            node,
                            ignore_errors,
                            import_attributes_type,
                        );
                    }
                    if ast::is_call_expression(parent)
                        && ast::is_bindable_object_define_property_call(parent)
                        && parent.arguments()[1] == node
                    {
                        return self.get_symbol_of_declaration(parent);
                    }
                }
                // fallthrough (case ast.KindNumericLiteral)
                // index access
                let mut object_type = None;
                if ast::is_element_access_expression(parent) {
                    if parent.as_element_access_expression().argument_expression == node {
                        object_type =
                            Some(self.get_type_of_expression(parent.expression().unwrap()));
                    }
                } else if ast::is_literal_type_node(parent)
                    && grand_parent.is_some_and(ast::is_indexed_access_type_node)
                {
                    object_type = Some(self.get_type_from_type_node(
                        grand_parent.unwrap().as_indexed_access_type_node().object_type,
                    ));
                }

                if let Some(object_type) = object_type {
                    return self.get_property_of_type(object_type, node.text());
                }
                None
            }
            Kind::DefaultKeyword
            | Kind::FunctionKeyword
            | Kind::EqualsGreaterThanToken
            | Kind::ClassKeyword => self.get_symbol_of_node(parent),
            Kind::ImportType => {
                if ast::is_literal_import_type_node(node) {
                    return self.get_symbol_at_location(
                        node.as_import_type_node().argument.as_literal_type_node().literal,
                        ignore_errors,
                    );
                }
                None
            }
            Kind::ExportKeyword => {
                if ast::is_export_assignment(parent) {
                    assert!(parent.symbol().is_some(), "Symbol should be defined");
                    return parent.symbol();
                }
                None
            }
            Kind::ImportKeyword | Kind::NewKeyword => {
                if node.kind() == Kind::ImportKeyword && ast::is_import_phase_meta_property(parent)
                {
                    return None;
                }
                // fallthrough (case ast.KindNewKeyword)
                if ast::is_meta_property(parent) {
                    return self.check_meta_property_keyword(parent).symbol();
                }
                None
            }
            Kind::InstanceOfKeyword => {
                if ast::is_binary_expression(parent) {
                    let t = self.get_type_of_expression(parent.as_binary_expression().right());
                    let has_instance_method_type =
                        self.get_symbol_has_instance_method_of_object_type(t);
                    if let Some(has_instance_method_type) = has_instance_method_type
                        && has_instance_method_type.symbol().is_some()
                    {
                        return has_instance_method_type.symbol();
                    }
                    return t.symbol();
                }
                None
            }
            Kind::JsxNamespacedName => {
                if ast::is_jsx_tag_name(node) && is_jsx_intrinsic_tag_name(node) {
                    let symbol = self.get_intrinsic_tag_symbol(parent);
                    if symbol == Some(self.unknown_symbol) {
                        return None;
                    }
                    return symbol;
                }
                // fallthrough (default)
                None
            }
            _ => None,
        }
    }

    // checker.go:32257
    pub(crate) fn get_index_signatures_at_location(&mut self, node: P<Node>) -> Vec<P<Node>> {
        let mut signatures = Vec::new();
        if ast::is_identifier(node)
            && ast::is_property_access_expression(node.parent().unwrap())
            && node.parent().unwrap().name() == Some(node)
        {
            let key_type = self.get_literal_type_from_property_name(node);
            let object_type =
                self.get_type_of_expression(node.parent().unwrap().expression().unwrap());
            for t in object_type.distributed_iter() {
                for info in self.get_applicable_index_infos(t, key_type) {
                    if let Some(declaration) = info.declaration.get() {
                        append_if_unique(&mut signatures, declaration);
                    }
                }
            }
        }
        signatures
    }

    // checker.go:32273
    fn get_symbol_of_name_or_property_access_expression(
        &mut self,
        name: P<Node>,
    ) -> Option<P<Symbol>> {
        let mut name = name;
        if ast::is_declaration_name(name) {
            return self.get_symbol_of_node(name.parent().unwrap());
        }
        if name.parent().unwrap().kind() == Kind::ExportAssignment
            && ast::is_entity_name_expression(name)
        {
            // Even an entity name expression that doesn't resolve as an entityname may still typecheck as a property access expression
            let success = self.resolve_entity_name(
                name,
                /*all meanings*/
                SymbolFlags::Value
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace
                    | SymbolFlags::Alias,
                true,  /*ignoreErrors*/
                false, /*dontResolveAlias*/
                None,  /*location*/
            );
            if success.is_some() && success != Some(self.unknown_symbol) {
                return success;
            }
        } else if ast::is_entity_name(name) && is_in_right_side_of_import_or_export_assignment(name)
        {
            // Since we already checked for ExportAssignment, this really could only be an Import
            let import_equals_declaration =
                ast::find_ancestor_kind(name, Kind::ImportEqualsDeclaration);
            assert!(
                import_equals_declaration.is_some(),
                "ImportEqualsDeclaration should be defined"
            );
            return self.get_symbol_of_part_of_right_hand_side_of_import_equals(name);
        }

        if ast::is_entity_name(name) {
            let possible_import_node = is_import_type_qualifier_part(name);
            if let Some(possible_import_node) = possible_import_node {
                self.get_type_from_type_node(possible_import_node);
                let sym = self.get_resolved_symbol_or_nil(name);
                return if sym == Some(self.unknown_symbol) { None } else { sym };
            }
        }

        while ast::is_right_side_of_qualified_name_or_property_access(name) {
            name = name.parent().unwrap();
        }

        if is_in_name_of_expression_with_type_arguments_or_heritage_type_reference(name) {
            let mut meaning;
            let name_parent = name.parent().unwrap();
            if name_parent.kind() == Kind::ExpressionWithTypeArguments
                || name_parent.kind() == Kind::TypeReference
            {
                // A heritage element name may appear in type space, value space, or both;
                // ensure the meaning matches its context.
                meaning = if ast::is_part_of_type_node(name) {
                    SymbolFlags::Type
                } else {
                    SymbolFlags::Value
                };

                // In a class 'extends' clause we are also looking for a value.
                if ast::is_expression_with_type_arguments_in_class_extends_clause(name_parent) {
                    meaning |= SymbolFlags::Value;
                }
            } else {
                meaning = SymbolFlags::Namespace;
            }

            meaning |= SymbolFlags::Alias;
            let mut entity_name_symbol = None;
            if ast::is_entity_name_expression(name) {
                entity_name_symbol = self.resolve_entity_name(
                    name, meaning, true,  /*ignoreErrors*/
                    false, /*dontResolveAlias*/
                    None,  /*location*/
                );
            }
            if entity_name_symbol.is_some() {
                return entity_name_symbol;
            }
        }

        if ast::is_expression_node(name) {
            if ast::node_is_missing(name) {
                // Missing entity name.
                return None;
            }
            let is_jsdoc = ast::is_jsdoc_name_reference_context(name);
            if ast::is_identifier(name) {
                if ast::is_jsx_tag_name(name) && is_jsx_intrinsic_tag_name(name) {
                    let symbol = self.get_intrinsic_tag_symbol(name.parent().unwrap());
                    return if symbol == Some(self.unknown_symbol) { None } else { symbol };
                }
                let meaning = if is_jsdoc {
                    SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace
                } else {
                    SymbolFlags::Value
                };
                let mut location = None;
                if is_jsdoc {
                    location = ast::get_host_signature_from_jsdoc(name);
                }
                let mut result = self.resolve_entity_name(
                    name, meaning, true, /*ignoreErrors*/
                    true, /*dontResolveAlias*/
                    location,
                );
                if result.is_none()
                    && is_jsdoc
                    && let Some(container) =
                        ast::find_ancestor(name, ast::is_class_or_interface_like)
                {
                    let symbol = self.get_symbol_of_declaration(container).unwrap();
                    // Handle unqualified references to class static members and class or interface instance members
                    let exports = self.get_exports_of_symbol(symbol);
                    let found = self.get_symbol(exports, name.text(), meaning);
                    result = found.map(|s| self.get_merged_symbol(s));
                    if result.is_none() {
                        let declared_type = self.get_declared_type_of_symbol(symbol);
                        result = self.get_property_of_type(declared_type, name.text());
                    }
                }
                return result;
            } else if ast::is_private_identifier(name) {
                return self.get_symbol_for_private_identifier_expression(name);
            } else if ast::is_property_access_expression(name) || ast::is_qualified_name(name) {
                let links = self.symbol_node_links.get(name);
                if links.resolved_symbol.get().is_some() {
                    return links.resolved_symbol.get();
                }
                if ast::is_property_access_expression(name) {
                    self.check_property_access_expression(
                        name,
                        CheckMode::Normal,
                        false, /*writeOnly*/
                    );
                    if links.resolved_symbol.get().is_none()
                        && !ast::is_private_identifier(name.name().unwrap())
                    {
                        let expression_type =
                            self.check_expression_cached(name.expression().unwrap());
                        let key_type =
                            self.get_literal_type_from_property_name(name.name().unwrap());
                        let symbol = self.get_applicable_index_symbol(expression_type, key_type);
                        links.resolved_symbol.set(symbol);
                    }
                } else {
                    self.check_qualified_name(name, CheckMode::Normal);
                }
                if links.resolved_symbol.get().is_none() && is_jsdoc && ast::is_qualified_name(name)
                {
                    return self.resolve_jsdoc_member_name(Some(name));
                }
                return links.resolved_symbol.get();
            }
        } else if ast::is_entity_name(name) && is_type_reference_identifier(name) {
            let meaning = if name.parent().unwrap().kind() == Kind::TypeReference {
                SymbolFlags::Type
            } else {
                SymbolFlags::Namespace
            };
            let symbol = self.resolve_entity_name(
                name, meaning, true, /*ignoreErrors*/
                true, /*dontResolveAlias*/
                None, /*location*/
            );
            if symbol.is_some() && symbol != Some(self.unknown_symbol) {
                return symbol;
            }
            if ast::is_name_of_heritage_clause_type_reference(name) {
                return None;
            }
            return Some(self.get_unresolved_symbol_for_entity_name(name));
        }

        if name.parent().unwrap().kind() == Kind::TypePredicate {
            return self.resolve_entity_name(
                name,
                SymbolFlags::FunctionScopedVariable, /*meaning*/
                true,                                /*ignoreErrors*/
                false,                               /*dontResolveAlias*/
                None,                                /*location*/
            );
        }
        None
    }

    // checker.go:32423
    pub(crate) fn get_type_of_node(&mut self, node: P<Node>) -> P<Type> {
        if ast::is_source_file(node)
            && !ast::is_external_or_common_js_module(node.as_source_file_p())
        {
            return self.error_type;
        }

        if node.flags().intersects(NodeFlags::InWithStatement) {
            // We cannot answer semantic questions within a with block, do not proceed any further
            return self.error_type;
        }

        let (class_decl, is_implements) =
            ast::try_get_class_implementing_or_extending_heritage_clause_element(node);
        let mut class_type = None;
        if let Some(class_decl) = class_decl {
            let symbol = self.get_symbol_of_declaration(class_decl).unwrap();
            class_type = Some(self.get_declared_type_of_class_or_interface(symbol));
        }

        if ast::is_part_of_type_node(node) {
            let type_from_type_node = self.get_type_from_type_node(node);
            if let Some(class_type) = class_type {
                return self.get_type_with_this_argument(
                    type_from_type_node,
                    class_type.as_interface_type().this_type.get(),
                    false, /*needApparentType*/
                );
            }

            return type_from_type_node;
        }

        if ast::is_expression_node(node) {
            return self.get_regular_type_of_expression(node);
        }

        if let Some(class_type) = class_type
            && !is_implements
        {
            // A SyntaxKind.ExpressionWithTypeArguments is considered a type node, except when it occurs in the
            // extends clause of a class. We handle that case here.
            let base_type = first_or_nil(self.get_base_types(class_type));
            if let Some(base_type) = base_type {
                return self.get_type_with_this_argument(
                    base_type,
                    class_type.as_interface_type().this_type.get(),
                    false, /*needApparentType*/
                );
            }
            return self.error_type;
        }

        if ast::is_type_declaration(node) {
            // In this case, we call getSymbolOfDeclaration instead of getSymbolAtLocation because it is a declaration
            let symbol = self.get_symbol_of_declaration(node).unwrap();
            return self.get_declared_type_of_symbol(symbol);
        }

        if ast::is_type_declaration_name(node) {
            let symbol = self.get_symbol_at_location(node, false /*ignoreErrors*/);
            if let Some(symbol) = symbol {
                return self.get_declared_type_of_symbol(symbol);
            }
            return self.error_type;
        }

        if ast::is_binding_element(node) {
            let t = self.get_type_for_variable_like_declaration(
                node,
                true, /*includeOptionality*/
                CheckMode::Normal,
            );
            if let Some(t) = t {
                return t;
            }
            return self.error_type;
        }

        if ast::is_declaration(node) {
            // In this case, we call getSymbolOfDeclaration instead of getSymbolLAtocation because it is a declaration
            let symbol = self.get_symbol_of_declaration(node);
            if let Some(symbol) = symbol {
                return self.get_type_of_symbol(symbol);
            }
            return self.error_type;
        }

        if ast::is_declaration_name_or_import_property_name(node) {
            let symbol = self.get_symbol_at_location(node, false /*ignoreErrors*/);
            if let Some(symbol) = symbol {
                return self.get_type_of_symbol(symbol);
            }
            return self.error_type;
        }

        if ast::is_binding_pattern(node) {
            let t = self.get_type_for_variable_like_declaration(
                node.parent().unwrap(),
                true, /*includeOptionality*/
                CheckMode::Normal,
            );
            if let Some(t) = t {
                return t;
            }
            return self.error_type;
        }

        if is_in_right_side_of_import_or_export_assignment(node) {
            let symbol = self.get_symbol_at_location(node, false /*ignoreErrors*/);
            if let Some(symbol) = symbol {
                let declared_type = self.get_declared_type_of_symbol(symbol);
                if !self.is_error_type(declared_type) {
                    return declared_type;
                }
                return self.get_type_of_symbol(symbol);
            }
        }

        let parent = node.parent().unwrap();
        if ast::is_meta_property(parent) && parent.as_meta_property().keyword_token == node.kind() {
            return self.check_meta_property_keyword(parent);
        }

        if ast::is_import_attributes(node) {
            return self.check_import_attributes_expression(node);
        }

        self.error_type
    }

    // contextual_type is nil-able in Go (callers pass getApparentTypeOfContextualType's result): P<Type> -> Option<P<Type>>.
    // checker.go:32535
    pub(crate) fn get_this_type_of_object_literal_from_contextual_type(
        &mut self,
        containing_literal: P<Node>,
        contextual_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        let mut literal = containing_literal;
        let mut t = contextual_type;
        while let Some(current) = t {
            let this_type = self.get_this_type_from_contextual_type(current);
            if this_type.is_some() {
                return this_type;
            }
            if literal.parent().unwrap().kind() != Kind::PropertyAssignment {
                break;
            }
            literal = literal.parent().unwrap().parent().unwrap();
            t = self.get_apparent_type_of_contextual_type(literal, ContextFlags::None);
        }
        None
    }

    // checker.go:32552
    fn get_this_type_from_contextual_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.map_type(t, |c, t| {
            if t.flags().intersects(TypeFlags::Intersection) {
                if let Some(result) = t.types().iter().find_map(|&t| {
                    let type_arg = c.get_this_type_argument(t);
                    if type_arg.is_some() {
                        return Some(type_arg);
                    }

                    None
                }) {
                    return result;
                }
                None
            } else {
                c.get_this_type_argument(t)
            }
        })
    }

    // checker.go:32568
    fn get_this_type_argument(&mut self, t: P<Type>) -> Option<P<Type>> {
        if t.object_flags().intersects(ObjectFlags::Reference)
            && t.as_type_reference().target.get() == Some(self.global_this_type)
        {
            return Some(self.get_type_arguments(t)[0]);
        }
        None
    }

    // checker.go:32575
    pub(crate) fn get_applicable_index_infos(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Vec<P<IndexInfo>> {
        let index_infos = self.get_index_infos_of_type(t);
        let mut result = Vec::new();
        result.extend(index_infos.iter().filter_map(|&info| {
            if self.is_applicable_index_type(key_type, info.key_type.get().unwrap()) {
                Some(info)
            } else {
                None
            }
        }));
        result
    }

    // checker.go:32579
    pub(crate) fn get_applicable_index_symbol(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<Symbol>> {
        if let Some(info) = self.get_applicable_index_info(t, key_type)
            && info != self.any_base_type_index_info
        {
            if info.index_symbol.get().is_none() {
                let mut declarations = Vec::new();
                if let Some(declaration) = info.declaration.get() {
                    declarations = vec![declaration];
                } else {
                    declarations.extend(
                        self.get_index_infos_of_type(t).iter().copied().filter_map(|info| {
                            info.declaration.get().filter(|&_declaration| {
                                self.is_applicable_index_type(
                                    key_type,
                                    info.key_type.get().unwrap(),
                                )
                            })
                        }),
                    );
                }
                if !declarations.is_empty() {
                    let symbol = self.new_symbol(SymbolFlags::Property, INTERNAL_SYMBOL_NAME_INDEX);
                    symbol.set_check_flags(symbol.check_flags() | CheckFlags::IndexSymbol);
                    symbol.set_value_declaration(Some(declarations[0]));
                    symbol.set_declarations(&declarations);
                    symbol.set_parent(t.symbol());
                    let links = self.value_symbol_links.get(symbol);
                    links.resolved_type.set(info.value_type.get());
                    info.index_symbol.set(Some(symbol));
                }
            }
            return info.index_symbol.get();
        }
        None
    }

    // checker.go:32608
    pub(crate) fn get_regular_type_of_expression(&mut self, expr: P<Node>) -> P<Type> {
        let mut expr = expr;
        if ast::is_right_side_of_qualified_name_or_property_access(expr) {
            expr = expr.parent().unwrap();
        }
        let t = self.get_type_of_expression(expr);
        self.get_regular_type_of_literal_type(t)
    }

    // checker.go:32615
    pub(crate) fn contains_arguments_reference(&mut self, node: P<Node>) -> bool {
        fn visit(c: &mut Checker, node: Option<P<Node>>) -> bool {
            let Some(node) = node else {
                return false;
            };
            match node.kind() {
                Kind::Identifier => {
                    // Go: c.IsArgumentsSymbol(symbol) is `symbol == c.argumentsSymbol` (services.go).
                    return node.text() == c.arguments_symbol.name()
                        && c.get_resolved_symbol(node) == c.arguments_symbol;
                }
                Kind::PropertyDeclaration
                | Kind::MethodDeclaration
                | Kind::GetAccessor
                | Kind::SetAccessor => {
                    if ast::is_computed_property_name(node.name().unwrap()) {
                        return visit(c, node.name());
                    }
                }
                Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                    return visit(c, node.expression());
                }
                Kind::PropertyAssignment => {
                    return visit(c, node.initializer());
                }
                _ => {}
            }
            if node_starts_new_lexical_environment(node) || ast::is_part_of_type_node(node) {
                return false;
            }
            node.for_each_child(&mut |child| visit(c, Some(child)))
        }

        let Some(body) = node.body() else {
            return false;
        };

        if let Some(&contains_arguments) = self.cached_arguments_referenced.get(&node) {
            return contains_arguments;
        }

        let contains_arguments = visit(self, Some(body));
        self.cached_arguments_referenced.insert(node, contains_arguments);
        contains_arguments
    }

    // checker.go:32652
    pub fn get_type_at_location(&mut self, node: P<Node>) -> P<Type> {
        self.get_type_of_node(ast::get_reparsed_node_for_node(node).unwrap())
    }

    // checker.go:32656
    pub(crate) fn get_emit_resolver(&mut self) -> P<EmitResolver> {
        if self.emit_resolver.is_none() {
            self.emit_resolver = Some(new_emit_resolver(self, tsrs_printer::new_emit_context()));
        }
        self.emit_resolver.unwrap()
    }

    // checker.go:32710 (source 41). The caller owns the emit context and this resolver's lifetime.
    pub fn new_emit_resolver(&self, emit_context: P<EmitContext>) -> P<EmitResolver> {
        new_emit_resolver(self, emit_context)
    }

    /// tsrs-only: called when a file's emit transform ends in a scratch region (notes/mem-emit-regions.md). Forgets
    /// what the checker keeps keyed by nodes of that region: once it is freed, a handle may name a new node and
    /// would hit a stale entry. Go keys these maps by pointer and its GC keeps the nodes alive, so such an entry is
    /// never hit again there either: the referenced-import links of synthesized JSX factory names (every key of
    /// `EmitResolver::jsx_links` is one), and accessible-chain cache entries whose scope is a synthesized node.
    pub fn forget_scratch_keyed_caches(&mut self) {
        self.emit_resolver_links.jsx_links.clear();
        for (symbol, key) in std::mem::take(&mut self.scratch_keyed_chain_cache) {
            let links = self.symbol_container_links.get(symbol);
            let mut cache = links.accessible_chain_cache.borrow_mut();
            cache.remove(&key);
        }
    }

    // checker.go:32664
    pub fn get_aliased_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        self.resolve_alias(symbol)
    }
}
