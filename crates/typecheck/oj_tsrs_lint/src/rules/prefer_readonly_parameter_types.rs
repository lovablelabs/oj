use rustc_hash::FxHashSet;
use tsrs_ast::{CheckFlags, Kind, ModifierFlags, Node, Symbol, SymbolFlags};
use tsrs_checker::{Checker, SignatureKind, Type, TypeFlags, is_tuple_type_exported};
use tsrs_core::{P, json::Value};

use crate::protocol::{Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::type_matches_specifier::{TypeOrValueSpecifier, type_matches_some_specifier};

pub struct PreferReadonlyParameterTypes {
    allow: Vec<TypeOrValueSpecifier>,
    check_parameter_properties: bool,
    ignore_inferred_types: bool,
    treat_methods_as_readonly: bool,
}

fn build_should_be_readonly_message() -> RuleMessage {
    RuleMessage {
        id: "shouldBeReadonly".into(),
        description: "Parameter should be a readonly type.".into(),
        help: String::new(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Readonlyness {
    Unknown,
    Mutable,
    Readonly,
}

fn is_type_readonly_array_or_tuple(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    opts: &PreferReadonlyParameterTypes,
    seen_types: &mut FxHashSet<P<Type>>,
) -> Readonlyness {
    let check_type_arguments =
        |context: &mut RuleContext<'_>, seen_types: &mut FxHashSet<P<Type>>| {
            for &type_arg in context.checker.get_type_arguments(t) {
                if is_type_readonly_recurser(context, type_arg, opts, seen_types)
                    == Readonlyness::Mutable
                {
                    return Readonlyness::Mutable;
                }
            }
            Readonlyness::Readonly
        };

    if context.checker.is_array_type(t) {
        if t.symbol().is_some_and(|symbol| symbol.name() == "Array") {
            return Readonlyness::Mutable;
        }
        return check_type_arguments(context, seen_types);
    }

    if is_tuple_type_exported(t) {
        let tuple_target = t.target().unwrap_or(t);
        if !is_tuple_type_exported(tuple_target) {
            return Readonlyness::Unknown;
        }
        if !tuple_target.as_tuple_type().is_readonly() {
            return Readonlyness::Mutable;
        }
        return check_type_arguments(context, seen_types);
    }

    Readonlyness::Unknown
}

fn property_has_private_identifier_name(property: P<Symbol>) -> bool {
    let has_private_name = |declaration: P<Node>| {
        declaration.name().is_some_and(|name| name.kind() == Kind::PrivateIdentifier)
    };
    if let Some(declaration) = property.value_declaration() {
        return has_private_name(declaration);
    }
    property.declarations().iter().copied().any(has_private_name)
}

fn property_is_private_mapped_property(property: P<Symbol>) -> bool {
    property.check_flags().intersects(CheckFlags::Mapped)
        && tsrs_checker::get_declaration_modifier_flags_from_symbol_exported(property)
            .intersects(ModifierFlags::Private)
}

fn property_is_readonly(checker: &mut Checker, property: P<Symbol>) -> bool {
    if checker.is_readonly_symbol_exported(property) {
        return true;
    }
    if property.check_flags().intersects(CheckFlags::Readonly | CheckFlags::SyntheticMethod) {
        return true;
    }
    if property.flags().intersects(SymbolFlags::Method)
        && property.check_flags().intersects(CheckFlags::Mapped)
    {
        return true;
    }
    if property.value_declaration().is_none()
        && property.check_flags().intersects(CheckFlags::Mapped)
    {
        return true;
    }
    tsrs_checker::get_declaration_modifier_flags_from_symbol_exported(property)
        .intersects(ModifierFlags::Readonly)
}

fn is_type_readonly_object(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    opts: &PreferReadonlyParameterTypes,
    seen_types: &mut FxHashSet<P<Type>>,
) -> Readonlyness {
    let properties = context.checker.get_properties_of_type(t);
    for &property in properties {
        if opts.treat_methods_as_readonly && property.flags().intersects(SymbolFlags::Method) {
            continue;
        }
        if property_is_readonly(context.checker, property) {
            continue;
        }
        if property_has_private_identifier_name(property) {
            continue;
        }
        if property_is_private_mapped_property(property) {
            continue;
        }
        return Readonlyness::Mutable;
    }

    for &property in properties {
        if property.flags().intersects(SymbolFlags::Method) {
            continue;
        }
        if property_has_private_identifier_name(property)
            || property_is_private_mapped_property(property)
        {
            continue;
        }
        let property_type = context
            .checker
            .get_type_of_property_of_type(t, property.name())
            .unwrap_or_else(|| context.checker.get_type_of_symbol(property));
        if seen_types.contains(&property_type) {
            continue;
        }
        if is_type_readonly_recurser(context, property_type, opts, seen_types)
            == Readonlyness::Mutable
        {
            return Readonlyness::Mutable;
        }
    }

    for &info in context.checker.get_index_infos_of_type(t) {
        if !info.is_readonly() {
            return Readonlyness::Mutable;
        }
        let Some(value_type) = info.value_type.get() else {
            continue;
        };
        if value_type == t || seen_types.contains(&value_type) {
            continue;
        }
        if is_type_readonly_recurser(context, value_type, opts, seen_types) == Readonlyness::Mutable
        {
            return Readonlyness::Mutable;
        }
    }

    Readonlyness::Readonly
}

fn is_type_readonly_recurser(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    opts: &PreferReadonlyParameterTypes,
    seen_types: &mut FxHashSet<P<Type>>,
) -> Readonlyness {
    seen_types.insert(t);
    if type_matches_some_specifier(t, &opts.allow, context.program) {
        return Readonlyness::Readonly;
    }
    if t.flags().intersects(TypeFlags::Union) {
        for &subtype in t.types() {
            if seen_types.contains(&subtype) {
                continue;
            }
            if is_type_readonly_recurser(context, subtype, opts, seen_types)
                != Readonlyness::Readonly
            {
                return Readonlyness::Mutable;
            }
        }
        return Readonlyness::Readonly;
    }
    if t.flags().intersects(TypeFlags::Intersection) {
        let has_array_or_tuple = t.types().iter().any(|&subtype| {
            context.checker.is_array_type(subtype) || is_tuple_type_exported(subtype)
        });
        if has_array_or_tuple {
            for &subtype in t.types() {
                if seen_types.contains(&subtype) {
                    continue;
                }
                if is_type_readonly_recurser(context, subtype, opts, seen_types)
                    != Readonlyness::Readonly
                {
                    return Readonlyness::Mutable;
                }
            }
            return Readonlyness::Readonly;
        }
        let readonly_object = is_type_readonly_object(context, t, opts, seen_types);
        if readonly_object != Readonlyness::Unknown {
            return readonly_object;
        }
    }
    if !t.flags().intersects(TypeFlags::Object) {
        return Readonlyness::Readonly;
    }
    if !context.checker.get_signatures_of_type(t, SignatureKind::Call).is_empty()
        && context.checker.get_properties_of_type(t).is_empty()
    {
        return Readonlyness::Readonly;
    }
    let readonly_array = is_type_readonly_array_or_tuple(context, t, opts, seen_types);
    if readonly_array != Readonlyness::Unknown {
        return readonly_array;
    }
    let readonly_object = is_type_readonly_object(context, t, opts, seen_types);
    if readonly_object != Readonlyness::Unknown {
        return readonly_object;
    }
    Readonlyness::Readonly
}

fn is_type_readonly(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    opts: &PreferReadonlyParameterTypes,
) -> bool {
    is_type_readonly_recurser(context, t, opts, &mut FxHashSet::default()) == Readonlyness::Readonly
}

fn is_literal_or_taggable_primitive_like(t: P<Type>) -> bool {
    t.flags().intersects(
        TypeFlags::Literal
            | TypeFlags::BigInt
            | TypeFlags::Number
            | TypeFlags::String
            | TypeFlags::TemplateLiteral,
    )
}

fn is_object_literal_like(checker: &mut Checker, t: P<Type>) -> bool {
    checker.get_signatures_of_type(t, SignatureKind::Call).is_empty()
        && checker.get_signatures_of_type(t, SignatureKind::Construct).is_empty()
        && t.flags().intersects(TypeFlags::Object)
}

fn is_type_branded_literal(checker: &mut Checker, t: P<Type>) -> bool {
    if !t.flags().intersects(TypeFlags::Intersection) {
        return false;
    }
    let mut had_object_like = false;
    let mut had_primitive_like = false;
    for &constituent in t.types() {
        if is_object_literal_like(checker, constituent) {
            had_object_like = true;
        } else if is_literal_or_taggable_primitive_like(constituent) {
            had_primitive_like = true;
        } else {
            return false;
        }
    }
    had_object_like && had_primitive_like
}

fn is_type_branded_literal_like(checker: &mut Checker, t: P<Type>) -> bool {
    if t.flags().intersects(TypeFlags::Union) {
        return t.types().iter().all(|&part| is_type_branded_literal(checker, part));
    }
    is_type_branded_literal(checker, t)
}

fn is_parameter_property(parameter: P<Node>) -> bool {
    parameter.kind() == Kind::Parameter
        && parameter.parent().is_some_and(|parent| parent.kind() == Kind::Constructor)
        && parameter.modifier_flags().intersects(ModifierFlags::ParameterPropertyModifier)
}

fn get_parameter_type(checker: &mut Checker, parameter: P<Node>) -> P<Type> {
    if let Some(type_node) = parameter.type_node() {
        checker.get_type_from_type_node(type_node)
    } else {
        checker.get_type_at_location(parameter)
    }
}

impl PreferReadonlyParameterTypes {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("prefer-readonly-parameter-types: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("prefer-readonly-parameter-types: {name} must be a boolean")),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("prefer-readonly-parameter-types: allow must be an array".into()),
        };
        Ok(Self {
            allow,
            check_parameter_properties: boolean("checkParameterProperties", true)?,
            ignore_inferred_types: boolean("ignoreInferredTypes", false)?,
            treat_methods_as_readonly: boolean("treatMethodsAsReadonly", false)?,
        })
    }
}

impl Rule for PreferReadonlyParameterTypes {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ArrowFunction
                    | Kind::CallSignature
                    | Kind::ConstructSignature
                    | Kind::Constructor
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::FunctionType
                    | Kind::MethodDeclaration
                    | Kind::MethodSignature
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        for &parameter in node.parameters() {
            let parameter_is_property = is_parameter_property(parameter);
            if !self.check_parameter_properties && parameter_is_property {
                continue;
            }
            if self.ignore_inferred_types && parameter.type_node().is_none() {
                continue;
            }
            let t = get_parameter_type(context.checker, parameter);
            if !is_type_readonly(context, t, self)
                && !is_type_branded_literal_like(context.checker, t)
            {
                if parameter_is_property && let Some(name) = parameter.name() {
                    context.report_range(
                        Range {
                            pos: trim_node_range(context.source_file, name).pos,
                            end: parameter
                                .end()
                                .expect_source("lint parameter is a parsed source node"),
                        },
                        build_should_be_readonly_message(),
                    );
                    continue;
                }
                context.report_node(parameter, build_should_be_readonly_message());
            }
        }
    }
}
