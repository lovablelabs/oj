//! Port of tsgolint's `internal/rules/no_unnecessary_type_parameters/no_unnecessary_type_parameters.go`.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node, Symbol};
use tsrs_checker::{ObjectFlags, Signature, SignatureKind, Type, TypeFlags};
use tsrs_core::{P, goslices};

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils;

pub struct NoUnnecessaryTypeParameters;

fn source_pos(node: P<Node>) -> u32 {
    node.pos().expect_source("parsed signature node")
}

fn source_end(node: P<Node>) -> u32 {
    node.end().expect_source("parsed signature node")
}

fn collect_references(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    symbol: P<Symbol>,
    name: P<Node>,
    references: &mut Vec<P<Node>>,
) {
    if node.kind() == Kind::Identifier
        && node != name
        && node.parent().is_some_and(ast::is_type_node)
        && context.checker.get_symbol_at_location_exported(node) == Some(symbol)
    {
        references.push(node);
    }
    ast::for_each_child_and_jsdoc(node, context.source_file.get(), &mut |child| {
        collect_references(context, child, symbol, name, references);
        false
    });
}

fn start_of_body(node: P<Node>) -> u32 {
    match node.kind() {
        Kind::ClassDeclaration | Kind::ClassExpression => {
            node.member_list().unwrap().pos().expect_source("parsed class members")
        }
        Kind::ArrowFunction
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::MethodDeclaration => {
            if let Some(body) = node.body() {
                if node.kind() == Kind::ArrowFunction && ast::is_function_like(body) {
                    return u32::MAX;
                }
                return source_pos(body);
            }
            node.type_node().map_or(u32::MAX, source_end)
        }
        Kind::CallSignature
        | Kind::ConstructSignature
        | Kind::ConstructorType
        | Kind::FunctionType
        | Kind::MethodSignature => node.type_node().map_or(u32::MAX, source_end),
        _ => u32::MAX,
    }
}

fn repeated_in_ast(
    context: &mut RuleContext<'_>,
    parameter: P<Node>,
    references: &[P<Node>],
    body_start: u32,
) -> bool {
    let mut total = 0;
    for &reference in references {
        // References inside the type parameter declaration itself don't count.
        if source_pos(reference) < source_end(parameter)
            && source_end(reference) > source_pos(parameter)
        {
            continue;
        }
        // References in the body don't count for fast-path usage checks.
        if source_pos(reference) > body_start {
            continue;
        }
        // A type argument can be reused by the generic type even when it occurs
        // only once in this signature. Leave arrays to the type-aware check.
        if let Some(mut argument) =
            reference.parent().filter(|&parent| ast::is_type_reference_node(parent))
        {
            while let Some(parent) =
                argument.parent().filter(|&parent| ast::is_parenthesized_type_node(parent))
            {
                argument = parent;
            }
            if let Some(parent) = argument.parent()
                && matches!(
                    parent.kind(),
                    Kind::TypeReference
                        | Kind::ExpressionWithTypeArguments
                        | Kind::ImportType
                        | Kind::TypeQuery
                )
            {
                let is_array = ast::is_type_reference_node(parent) && {
                    let t = context.checker.get_type_at_location(parent);
                    context.checker.is_array_type(t)
                };
                if !is_array && parent.type_arguments().contains(&argument) {
                    return true;
                }
            }
        }
        total += 1;
        if total >= 2 {
            return true;
        }
    }
    false
}

fn removal_range(context: &RuleContext<'_>, parameters: &[P<Node>], target: P<Node>) -> Range {
    let Some(index) = parameters.iter().position(|&parameter| parameter == target) else {
        return Range { pos: source_pos(target), end: source_end(target) };
    };
    let text = context.source_file.text().as_bytes();
    if parameters.len() == 1 {
        let first = parameters[0];
        let start = source_pos(first).saturating_sub(1);
        let mut end = source_end(first) as usize;
        while end < text.len() && text[end] != b'>' {
            end += 1;
        }
        if end < text.len() && text[end] == b'>' {
            end += 1;
        }
        if end <= start as usize {
            end = source_end(first) as usize;
        }
        return Range { pos: start, end: end as u32 };
    }
    if index == 0 {
        let mut end = source_pos(parameters[1]);
        let token = tsrs_scanner::get_range_of_token_at_position(context.source_file, end)
            .source_pos()
            .unwrap();
        if token > source_pos(target) {
            end = token;
        }
        return Range { pos: source_pos(target), end };
    }
    let mut start = source_end(parameters[index - 1]);
    for (i, &byte) in text.iter().enumerate().take(source_pos(target) as usize).skip(start as usize)
    {
        if byte == b',' {
            start = i as u32;
            break;
        }
    }
    Range { pos: start, end: source_end(target) }
}

struct UsageCounts<'a> {
    counts: &'a mut FxHashMap<P<Symbol>, u32>,
    targets: &'a FxHashSet<P<Symbol>>,
    remaining: usize,
    from_class: bool,
    type_usages: FxHashMap<P<Type>, u32>,
    visited_constraints: FxHashSet<P<Node>>,
    visited_default: bool,
    function_like_type: bool,
}

impl UsageCounts<'_> {
    fn increment_identifier(&mut self, symbol: P<Symbol>, multiple: bool) {
        if self.remaining == 0 || !self.targets.contains(&symbol) {
            return;
        }
        let count = self.counts.entry(symbol).or_default();
        if *count > 2 {
            return;
        }
        *count += if multiple { 2 } else { 1 };
        if *count > 2 {
            self.remaining -= 1;
        }
    }

    fn visit_types(&mut self, context: &mut RuleContext<'_>, types: &[P<Type>], multiple: bool) {
        for &ty in types {
            if self.remaining == 0 {
                return;
            }
            self.visit_type(context, Some(ty), multiple, false);
        }
    }

    fn visit_symbols(
        &mut self,
        context: &mut RuleContext<'_>,
        symbols: &[P<Symbol>],
        multiple: bool,
    ) {
        for &symbol in symbols {
            if self.remaining == 0 {
                return;
            }
            let ty = context.checker.get_type_of_symbol_exported(symbol);
            self.visit_type(context, Some(ty), multiple, false);
        }
    }

    fn visit_signature(&mut self, context: &mut RuleContext<'_>, signature: P<Signature>) {
        if self.remaining == 0 {
            return;
        }
        if let Some(this) = signature.this_parameter() {
            let ty = context.checker.get_type_of_symbol_exported(this);
            self.visit_type(context, Some(ty), false, false);
        }
        for &parameter in signature.parameters() {
            if self.remaining == 0 {
                return;
            }
            let ty = context.checker.get_type_of_symbol_exported(parameter);
            self.visit_type(context, Some(ty), false, false);
        }
        self.visit_types(context, signature.type_parameters(), false);
        let mut return_type = context.checker.get_return_type_of_signature_exported(signature);
        if let Some(predicate) = context.checker.get_type_predicate_of_signature_exported(signature)
            && let Some(ty) = predicate.type_()
        {
            return_type = ty;
        }
        self.visit_type(context, Some(return_type), false, true);
    }

    fn visit_type(
        &mut self,
        context: &mut RuleContext<'_>,
        ty: Option<P<Type>>,
        multiple: bool,
        return_type: bool,
    ) {
        let Some(ty) = ty else {
            return;
        };
        if self.remaining == 0 {
            return;
        }
        let uses = self.type_usages.entry(ty).or_default();
        *uses += 1;
        if *uses > 9 {
            return;
        }
        if ty.flags().intersects(TypeFlags::TypeParameter) {
            if let Some(symbol) = ty.symbol()
                && let Some(&declaration) = symbol.declarations().first()
                && declaration.kind() == Kind::TypeParameter
            {
                self.increment_identifier(symbol, multiple);
                let parameter = declaration.as_type_parameter_declaration();
                if let Some(constraint) = parameter.constraint()
                    && self.visited_constraints.insert(constraint)
                {
                    let constraint_type = context.checker.get_type_at_location(constraint);
                    self.visit_type(context, Some(constraint_type), false, false);
                }
                if let Some(default) = parameter.default_type()
                    && !self.visited_default
                {
                    self.visited_default = true;
                    let default_type = context.checker.get_type_at_location(default);
                    self.visit_type(context, Some(default_type), false, false);
                }
            }
            return;
        }
        if let Some(alias) = ty.alias()
            && !alias.type_arguments().is_empty()
        {
            self.visit_types(context, alias.type_arguments(), true);
            return;
        }
        if ty.flags().intersects(TypeFlags::Union | TypeFlags::Intersection) {
            self.visit_types(context, ty.types(), multiple);
            return;
        }
        if ty.flags().intersects(TypeFlags::IndexedAccess) {
            let indexed = ty.as_indexed_access_type();
            self.visit_type(context, indexed.object_type(), multiple, false);
            self.visit_type(context, indexed.index_type(), multiple, false);
            return;
        }
        if ty.flags().intersects(TypeFlags::Object)
            && ty.object_flags().intersects(ObjectFlags::Reference)
        {
            let arguments = context.checker.get_type_arguments(ty);
            if !arguments.is_empty() {
                let target = ty.target().expect("reference type target");
                for &argument in arguments {
                    let mut assume_multiple = self.from_class || multiple;
                    if target.is_tuple_type() {
                        assume_multiple |= return_type && !target.as_tuple_type().is_readonly();
                    } else if context.checker.is_array_type(target) {
                        assume_multiple |= return_type
                            && ty.symbol().is_some_and(|symbol| symbol.name() == "Array");
                    } else {
                        assume_multiple = true;
                    }
                    self.visit_type(context, Some(argument), assume_multiple, return_type);
                }
            }
            return;
        }
        if ty.flags().intersects(TypeFlags::TemplateLiteral) {
            self.visit_types(context, ty.types(), multiple);
            return;
        }
        if ty.flags().intersects(TypeFlags::Conditional) {
            let conditional = ty.as_conditional_type();
            self.visit_type(context, conditional.check_type(), multiple, false);
            self.visit_type(context, conditional.extends_type(), multiple, false);
            return;
        }
        if ty.flags().intersects(TypeFlags::Object) {
            let properties = context.checker.get_properties_of_type(ty);
            self.visit_symbols(context, properties, false);
            if ty.object_flags().intersects(ObjectFlags::Mapped) {
                let mapped = ty.as_mapped_type();
                self.visit_type(context, mapped.type_parameter(), false, false);
                if properties.is_empty() {
                    self.visit_type(
                        context,
                        mapped.template_type().or_else(|| mapped.constraint_type()),
                        false,
                        false,
                    );
                }
            }
            let number_index = context.checker.get_number_index_type(ty);
            self.visit_type(context, number_index, true, false);
            let string_index = context.checker.get_string_index_type(ty);
            self.visit_type(context, string_index, true, false);
            for kind in [SignatureKind::Call, SignatureKind::Construct] {
                for &signature in context.checker.get_signatures_of_type(ty, kind) {
                    if self.remaining == 0 {
                        return;
                    }
                    self.function_like_type = true;
                    self.visit_signature(context, signature);
                }
            }
            return;
        }
        if ty.flags().intersects(TypeFlags::Index | TypeFlags::StringMapping) {
            self.visit_type(context, ty.target(), multiple, false);
        }
    }
}

fn collect_usage_counts(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    counts: &mut FxHashMap<P<Symbol>, u32>,
    targets: &FxHashSet<P<Symbol>>,
    remaining: usize,
    from_class: bool,
) -> usize {
    let mut usage = UsageCounts {
        counts,
        targets,
        remaining,
        from_class,
        type_usages: FxHashMap::default(),
        visited_constraints: FxHashSet::default(),
        visited_default: false,
        function_like_type: false,
    };
    if matches!(node.kind(), Kind::CallSignature | Kind::Constructor) {
        usage.function_like_type = true;
        let signature = context.checker.get_signature_from_declaration_exported(node);
        usage.visit_signature(context, signature);
    }
    if !usage.function_like_type {
        let ty = context.checker.get_type_at_location(node);
        usage.visit_type(context, Some(ty), false, false);
    }
    usage.remaining
}

fn count_usages(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    targets: &FxHashSet<P<Symbol>>,
) -> FxHashMap<P<Symbol>, u32> {
    let mut counts = FxHashMap::default();
    let mut remaining = targets.len();
    if matches!(node.kind(), Kind::ClassDeclaration | Kind::ClassExpression) {
        for &parameter in node.type_parameters() {
            if remaining == 0 {
                break;
            }
            remaining =
                collect_usage_counts(context, parameter, &mut counts, targets, remaining, true);
        }
        if let Some(clauses) = utils::get_heritage_clauses(node) {
            for &clause in clauses.nodes() {
                for &ty in clause.as_heritage_clause().types().nodes() {
                    if remaining == 0 {
                        break;
                    }
                    remaining =
                        collect_usage_counts(context, ty, &mut counts, targets, remaining, true);
                }
            }
        }
        for &member in node.members() {
            if remaining == 0 {
                break;
            }
            remaining =
                collect_usage_counts(context, member, &mut counts, targets, remaining, true);
        }
    } else {
        collect_usage_counts(context, node, &mut counts, targets, remaining, false);
    }
    counts
}

struct Candidate {
    node: P<Node>,
    name: P<Node>,
    symbol: P<Symbol>,
    references: Vec<P<Node>>,
}

fn check_node(context: &mut RuleContext<'_>, node: P<Node>, descriptor: &str) {
    let parameters = node.type_parameters();
    if parameters.is_empty() {
        return;
    }
    let body_start = start_of_body(node);
    let mut candidates = Vec::new();
    for &parameter in parameters {
        let name = parameter.as_type_parameter_declaration().name();
        let Some(symbol) = context.checker.get_symbol_at_location_exported(name) else {
            continue;
        };
        let mut references = Vec::with_capacity(8);
        collect_references(context, node, symbol, name, &mut references);
        goslices::sort_func(&mut references, |a, b| source_pos(*a).cmp(&source_pos(*b)));
        if !repeated_in_ast(context, parameter, &references, body_start) {
            candidates.push(Candidate { node: parameter, name, symbol, references });
        }
    }
    if candidates.is_empty() {
        return;
    }
    let targets = candidates.iter().map(|candidate| candidate.symbol).collect();
    let counts = count_usages(context, node, &targets);
    for candidate in candidates {
        let Some(&count) = counts.get(&candidate.symbol).filter(|&&count| count <= 2) else {
            continue;
        };
        let uses = if count == 1 { "never used" } else { "used only once" };
        let name = candidate.name.text();
        let constraint = candidate.node.as_type_parameter_declaration().constraint();
        let constraint_text = match constraint {
            None => "unknown".into(),
            Some(constraint) if constraint.kind() == Kind::AnyKeyword => "unknown".into(),
            Some(constraint) => tsrs_scanner::get_source_text_of_node_from_source_file(
                context.source_file,
                constraint,
                false,
            ),
        };
        let mut diagnostic = Diagnostic {
            message: RuleMessage {
                id: "sole".into(),
                description: format!(
                    "Type parameter {name} is {uses} in the {descriptor} signature."
                ),
                ..Default::default()
            },
            range: Some(trim_node_range(context.source_file, candidate.node)),
            labeled_ranges: vec![LabeledRange {
                label: format!("This is the only usage of type parameter {name} in the signature."),
                range: trim_node_range(
                    context.source_file,
                    candidate.references.first().copied().unwrap_or(candidate.name),
                ),
            }],
            ..Default::default()
        };
        if context.options.fix_suggestions {
            let removal = removal_range(context, parameters, candidate.node);
            let complex = constraint.is_some_and(|constraint| {
                matches!(
                    constraint.kind(),
                    Kind::UnionType
                        | Kind::IntersectionType
                        | Kind::ConditionalType
                        | Kind::TypeOperator
                        | Kind::FunctionType
                        | Kind::ConstructorType
                )
            });
            let mut fixes = Vec::with_capacity(candidate.references.len() + 1);
            for reference in candidate.references {
                let range = trim_node_range(context.source_file, reference);
                if range.pos < removal.end && range.end > removal.pos {
                    continue;
                }
                let matching_ancestor =
                    reference.parent().and_then(|parent| parent.parent()).is_some_and(|ancestor| {
                        matches!(
                            ancestor.kind(),
                            Kind::ArrayType
                                | Kind::IndexedAccessType
                                | Kind::IntersectionType
                                | Kind::UnionType
                                | Kind::ConditionalType
                                | Kind::TypeOperator
                                | Kind::OptionalType
                        )
                    });
                let replacement = if complex && matching_ancestor {
                    format!("({constraint_text})")
                } else {
                    constraint_text.clone()
                };
                fixes.push(replace_node(
                    context.source_file,
                    reference.parent().unwrap(),
                    replacement,
                ));
            }
            fixes.push(Fix { range: removal, text: String::new() });
            diagnostic.suggestions.push(Suggestion {
                message: RuleMessage {
                    id: "replaceUsagesWithConstraint".into(),
                    description: "Replace all usages of type parameter with its constraint.".into(),
                    ..Default::default()
                },
                fixes,
            });
        }
        context.report_diagnostic(diagnostic);
    }
}

impl Rule for NoUnnecessaryTypeParameters {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ArrowFunction
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::CallSignature
                    | Kind::ConstructSignature
                    | Kind::ConstructorType
                    | Kind::FunctionType
                    | Kind::MethodSignature
                    | Kind::MethodDeclaration
                    | Kind::ClassDeclaration
                    | Kind::ClassExpression
            )
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let descriptor = match listener {
            Listener::Enter(Kind::ClassDeclaration | Kind::ClassExpression) => "class",
            Listener::Enter(_) => "function",
            _ => unreachable!("no-unnecessary-type-parameters listener"),
        };
        check_node(context, node, descriptor);
    }
}
