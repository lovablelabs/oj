use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use tsrs_ast::{self as ast, Kind, Node, NodeFactory, NodeFactoryHooks, SourceFile};
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_core::stringutil::compare_strings_case_sensitive;
use tsrs_core::{JsxEmit, P, goslices};
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, EmitFlags};
use tsrs_scanner as scanner;

use crate::change::{self, LeadingTriviaOption, NodeOptions, TrailingTriviaOption};
use crate::languageservice::LanguageService;
use crate::lsutil::{
    self, NodeComparer, OrganizeImportsSort, OrganizeImportsTypeOrder, StringComparer,
};

impl LanguageService {
    // organizeimports.go:23
    // OrganizeImports organizes imports by:
    //  1. Removing unused imports
    //  2. Coalescing imports from the same module
    //  3. Sorting imports
    pub(crate) fn organize_imports(
        &self,
        ctx: &Context,
        source_file: P<SourceFile>,
        program: &'static Program,
        kind: lsproto::CodeActionKind,
    ) -> OrderedMap<String, Vec<lsproto::TextEdit>> {
        let mut change_tracker = change::new_tracker(
            ctx,
            &program.options(),
            self.format_options(),
            Arc::clone(&self.converters),
        );
        let should_sort = kind == lsproto::CodeActionKind::SOURCE_SORT_IMPORTS_TS
            || kind == lsproto::CodeActionKind::SOURCE_ORGANIZE_IMPORTS_TS;
        let should_combine = should_sort;
        let should_remove = kind == lsproto::CodeActionKind::SOURCE_REMOVE_UNUSED_IMPORTS_TS
            || kind == lsproto::CodeActionKind::SOURCE_ORGANIZE_IMPORTS_TS;
        let top_level_import_decls =
            lsutil::filter_import_declarations(source_file.statements.nodes());
        let top_level_import_group_decls =
            group_by_newline_contiguous(source_file, &top_level_import_decls);

        let preferences = self.user_preferences();
        let (comparers_to_test, type_orders_to_test) = lsutil::get_detection_lists(preferences);
        let default_comparer = Rc::clone(&comparers_to_test[0]);
        let sort = lsutil::resolve_organize_imports_sort(preferences);

        let mut module_specifier_comparer: Option<StringComparer> = None;
        let mut named_import_comparer: Option<StringComparer> = None;
        if sort != OrganizeImportsSort::Auto {
            module_specifier_comparer = Some(Rc::clone(&default_comparer));
            named_import_comparer = Some(Rc::clone(&default_comparer));
        }
        let mut type_order = preferences.organize_imports_type_order;

        if sort == OrganizeImportsSort::Auto {
            let (result, _) = lsutil::detect_module_specifier_case_by_sort(
                &top_level_import_group_decls,
                &comparers_to_test,
            );
            module_specifier_comparer = result;
        }

        if type_order == OrganizeImportsTypeOrder::Auto || sort == OrganizeImportsSort::Auto {
            let (named_import_comparer2, type_order2, found) =
                lsutil::detect_named_import_organization_by_sort(
                    &top_level_import_decls,
                    &comparers_to_test,
                    &type_orders_to_test,
                );
            if found {
                if named_import_comparer.is_none() || sort == OrganizeImportsSort::Auto {
                    named_import_comparer = named_import_comparer2;
                }
                if type_order == OrganizeImportsTypeOrder::Auto {
                    type_order = type_order2;
                }
            }
        }

        let comparer = OrganizeImportsComparerSettings {
            module_specifier_comparer,
            named_import_comparer,
            type_order,
        };

        for import_group_decl in &top_level_import_group_decls {
            organize_imports_worker(
                import_group_decl,
                &comparer,
                should_sort,
                should_combine,
                should_remove,
                source_file,
                program,
                &mut change_tracker,
                ctx,
            );
        }

        if kind != lsproto::CodeActionKind::SOURCE_REMOVE_UNUSED_IMPORTS_TS {
            let top_level_export_group_decls = get_top_level_export_groups(source_file);
            for export_group_decl in &top_level_export_group_decls {
                organize_exports_worker(
                    export_group_decl,
                    &comparer,
                    source_file,
                    &mut change_tracker,
                );
            }
        }

        for &stmt in source_file.statements.nodes() {
            if !ast::is_ambient_module(stmt) {
                continue;
            }

            let ambient_module = stmt.as_module_declaration();
            let Some(body) = ambient_module.body() else {
                continue;
            };

            let module_body = body.as_module_block();

            let ambient_module_import_decls =
                lsutil::filter_import_declarations(module_body.statements.nodes());
            let ambient_module_import_group_decls =
                group_by_newline_contiguous(source_file, &ambient_module_import_decls);

            for import_group_decl in &ambient_module_import_group_decls {
                organize_imports_worker(
                    import_group_decl,
                    &comparer,
                    should_sort,
                    should_combine,
                    should_remove,
                    source_file,
                    program,
                    &mut change_tracker,
                    ctx,
                );
            }

            if kind != lsproto::CodeActionKind::SOURCE_REMOVE_UNUSED_IMPORTS_TS {
                let ambient_module_export_decls: Vec<P<Node>> = module_body
                    .statements
                    .nodes()
                    .iter()
                    .copied()
                    .filter(|s| s.kind() == Kind::ExportDeclaration)
                    .collect();
                organize_exports_worker(
                    &ambient_module_export_decls,
                    &comparer,
                    source_file,
                    &mut change_tracker,
                );
            }
        }

        // Unmappable files are dropped by GetChanges, so a content-mapped file whose imports cannot be
        // faithfully rewritten yields no edits rather than a corrupting one.
        let (changes, _) = change_tracker.get_changes();
        changes
    }
}

// organizeimports.go:116
struct OrganizeImportsComparerSettings {
    module_specifier_comparer: Option<StringComparer>,
    named_import_comparer: Option<StringComparer>,
    type_order: OrganizeImportsTypeOrder,
}

impl OrganizeImportsComparerSettings {
    // Go calls the (possibly nil) func value directly; a nil comparer is only reachable when detection found
    // nothing, where Go would panic on the call too.
    fn module_specifier_comparer(&self) -> &dyn Fn(&str, &str) -> Ordering {
        &**self.module_specifier_comparer.as_ref().expect("nil moduleSpecifierComparer")
    }
}

// organizeimports.go:122
fn organize_imports_worker(
    old_import_decls: &[P<Node>],
    comparer: &OrganizeImportsComparerSettings,
    should_sort: bool,
    should_combine: bool,
    should_remove: bool,
    source_file: P<SourceFile>,
    program: &'static Program,
    change_tracker: &mut change::Tracker,
    ctx: &Context,
) {
    if old_import_decls.is_empty() {
        return;
    }

    // Header comment preservation is handled via LeadingTriviaOptionExclude in the change tracker below

    let mut processed_imports = old_import_decls.to_vec();
    if should_remove {
        let mut type_checker = program.get_type_checker_for_file(ctx, source_file);
        processed_imports = remove_unused_imports(
            &processed_imports,
            source_file,
            &mut type_checker,
            program,
            change_tracker,
        );
    }

    let mut new_import_decls: Vec<P<Node>> = Vec::new();
    if should_combine {
        let mut grouped = group_by_module_specifier(&processed_imports);
        if should_sort {
            goslices::sort_func(&mut grouped, |a, b| {
                if a.is_empty() || b.is_empty() {
                    return Ordering::Equal;
                }
                lsutil::compare_module_specifiers(
                    a[0].module_specifier(),
                    b[0].module_specifier(),
                    comparer.module_specifier_comparer(),
                )
            });
        }

        let specifier_comparer = lsutil::get_named_import_specifier_comparer(
            &lsutil::UserPreferences {
                organize_imports_type_order: comparer.type_order,
                ..Default::default()
            },
            comparer.named_import_comparer.clone(),
        );

        new_import_decls.extend(grouped.iter().flat_map(|import_group| {
            let mut coalesced = coalesce_imports_worker(
                import_group,
                comparer.module_specifier_comparer(),
                &specifier_comparer,
                Some(source_file),
                change_tracker,
            );
            if should_sort {
                goslices::sort_stable_func(&mut coalesced, |&a, &b| {
                    lsutil::compare_imports_or_require_statements(
                        a,
                        b,
                        comparer.module_specifier_comparer(),
                    )
                });
            }
            coalesced
        }));
    } else {
        new_import_decls = processed_imports;
    }

    if should_sort && !should_combine {
        goslices::sort_func(&mut new_import_decls, |&a, &b| {
            lsutil::compare_imports_or_require_statements(
                a,
                b,
                comparer.module_specifier_comparer(),
            )
        });
    }

    if new_import_decls.is_empty() {
        change_tracker.delete_node_range(
            source_file,
            old_import_decls[0],
            old_import_decls[old_import_decls.len() - 1],
            LeadingTriviaOption::Exclude, // Preserve header comment
            TrailingTriviaOption::Include,
        );
    } else {
        for &imp in &new_import_decls {
            change_tracker.emit_context.set_emit_flags(imp, EmitFlags::NoLeadingComments);
        }

        let options = NodeOptions {
            leading_trivia_option: LeadingTriviaOption::Exclude, // Preserve header comment
            trailing_trivia_option: TrailingTriviaOption::Include,
            suffix: "\n".to_string(),
            ..Default::default()
        };

        change_tracker.replace_node_with_nodes(
            source_file,
            old_import_decls[0],
            new_import_decls,
            Some(options),
        );

        if old_import_decls.len() > 1 {
            for &decl in &old_import_decls[1..] {
                change_tracker.delete(source_file, decl);
            }
        }
    }
}

// organizeimports.go:215
fn group_by_module_specifier(imports: &[P<Node>]) -> Vec<Vec<P<Node>>> {
    let mut groups: FxHashMap<&'static str, Vec<P<Node>>> = FxHashMap::default();
    let mut order: Vec<&'static str> = Vec::new();

    for &imp in imports {
        let specifier = lsutil::get_external_module_name(imp.module_specifier());
        if !groups.contains_key(specifier) {
            order.push(specifier);
        }
        groups.entry(specifier).or_default().push(imp);
    }

    let mut result = Vec::with_capacity(order.len());
    result.extend(order.into_iter().map(|key| groups.remove(key).unwrap()));
    result
}

// organizeimports.go:234
fn remove_unused_imports(
    old_imports: &[P<Node>],
    source_file: P<SourceFile>,
    type_checker: &mut Checker,
    program: &'static Program,
    change_tracker: &change::Tracker,
) -> Vec<P<Node>> {
    let compiler_options = program.options();
    let jsx_elements_present =
        source_file.as_node().subtree_facts().intersects(ast::SubtreeFacts::ContainsJsx);
    let jsx_mode_needs_explicit_import =
        compiler_options.jsx == JsxEmit::REACT || compiler_options.jsx == JsxEmit::REACT_NATIVE;

    let factory = NodeFactory::new(NodeFactoryHooks::default());
    let mut used_imports: Vec<P<Node>> = Vec::with_capacity(old_imports.len());

    for &import_decl in old_imports {
        let Some(import_clause) = import_decl.as_import_declaration().import_clause else {
            used_imports.push(import_decl);
            continue;
        };

        let clause = import_clause.as_import_clause();
        let mut name = clause.name();
        let mut named_bindings = clause.named_bindings;

        if let Some(n) = name
            && !type_checker.is_declaration_used(
                source_file,
                n,
                jsx_elements_present,
                jsx_mode_needs_explicit_import,
            )
        {
            name = None;
        }

        if let Some(nb) = named_bindings {
            match nb.kind() {
                Kind::NamespaceImport => {
                    let ns_name = nb.as_namespace_import().name();
                    if !type_checker.is_declaration_used(
                        source_file,
                        ns_name,
                        jsx_elements_present,
                        jsx_mode_needs_explicit_import,
                    ) {
                        named_bindings = None;
                    }
                }
                Kind::NamedImports => {
                    let named_imports = nb.as_named_imports();
                    let original_bindings = nb;
                    let new_elements = filter_used_import_specifiers(
                        named_imports.elements.nodes(),
                        type_checker,
                        source_file,
                        jsx_elements_present,
                        jsx_mode_needs_explicit_import,
                    );
                    if new_elements.is_empty() {
                        named_bindings = None;
                    } else if new_elements.len() < named_imports.elements.nodes().len() {
                        let new_list = factory.new_node_list(new_elements);
                        let updated_named_imports = factory.update_named_imports(nb, new_list);
                        named_bindings = Some(updated_named_imports);
                    }
                    if let Some(nb2) = named_bindings
                        && !ast::node_is_synthesized(original_bindings)
                        && !printer::range_is_on_single_line(original_bindings.loc(), source_file)
                    {
                        change_tracker.emit_context.set_emit_flags(nb2, EmitFlags::MultiLine);
                    }
                }
                _ => {}
            }
        }

        if name.is_some() || named_bindings.is_some() {
            let import_decl_node = import_decl.as_import_declaration();
            let new_clause = factory.update_import_clause(
                import_clause,
                clause.phase_modifier(),
                name,
                named_bindings,
            );
            let new_import_decl = factory.update_import_declaration(
                import_decl,
                import_decl.modifiers(),
                Some(new_clause),
                import_decl_node.module_specifier,
                import_decl_node.attributes(),
            );
            used_imports.push(new_import_decl);
        } else {
            let module_specifier = import_decl.module_specifier();
            if has_module_declaration_matching_specifier(source_file, module_specifier) {
                if source_file.is_declaration_file() {
                    let import_decl_node = import_decl.as_import_declaration();
                    let new_import_decl = factory.update_import_declaration(
                        import_decl,
                        import_decl.modifiers(),
                        None, // no import clause
                        import_decl_node.module_specifier,
                        import_decl_node.attributes(),
                    );
                    used_imports.push(new_import_decl);
                } else {
                    used_imports.push(import_decl);
                }
            }
        }
    }

    used_imports
}

// organizeimports.go:322
fn filter_used_import_specifiers(
    elements: &[P<Node>],
    type_checker: &mut Checker,
    source_file: P<SourceFile>,
    jsx_elements_present: bool,
    jsx_mode_needs_explicit_import: bool,
) -> Vec<P<Node>> {
    let mut result = Vec::new();
    result.extend(elements.iter().filter_map(|&elem| {
        let spec_name = elem.as_import_specifier().name();
        if type_checker.is_declaration_used(
            source_file,
            spec_name,
            jsx_elements_present,
            jsx_mode_needs_explicit_import,
        ) {
            Some(elem)
        } else {
            None
        }
    }));
    result
}

// organizeimports.go:337
fn has_module_declaration_matching_specifier(
    source_file: P<SourceFile>,
    module_specifier: Option<P<Node>>,
) -> bool {
    let Some(module_specifier) = module_specifier else {
        return false;
    };
    if !ast::is_string_literal(module_specifier) {
        return false;
    }
    let module_specifier_text = module_specifier.text();

    source_file.module_augmentations().iter().any(|&module_name| {
        ast::is_string_literal(module_name) && module_name.text() == module_specifier_text
    })
}

// organizeimports.go:353
// getImportAttributesKey returns a key for grouping imports by their attributes.
fn get_import_attributes_key(attributes: Option<P<Node>>) -> String {
    let Some(attributes) = attributes else {
        return String::new();
    };

    let import_attrs = attributes.as_import_attributes();
    let mut key = String::new();
    // Go writes `Kind.String()`; the key only groups imports, so any injective rendering of the kind works.
    let _ = write!(key, "{:?}", import_attrs.token);
    key.push(' ');

    let mut attr_nodes: Vec<P<Node>> = import_attrs.attributes.nodes().to_vec();
    goslices::sort_func(&mut attr_nodes, |a, b| {
        let a_name = a.as_import_attribute().name().unwrap().text();
        let b_name = b.as_import_attribute().name().unwrap().text();
        compare_strings_case_sensitive(a_name, b_name)
    });

    for attr_node in attr_nodes {
        let attr = attr_node.as_import_attribute();
        key.push_str(attr.name().unwrap().text());
        key.push(':');
        if ast::is_string_literal_like(attr.value) {
            key.push('"');
            key.push_str(attr.value.text());
            key.push('"');
        } else {
            key.push_str(attr.value.text());
        }
        key.push(' ');
    }

    key
}

// organizeimports.go:388
// groupByNewlineContiguous groups declarations by blank lines between them.
pub fn group_by_newline_contiguous(
    source_file: P<SourceFile>,
    decls: &[P<Node>],
) -> Vec<Vec<P<Node>>> {
    let mut s = scanner::Scanner::new();
    s.set_skip_trivia(false); // Must not skip trivia to detect newlines
    let mut groups: Vec<Vec<P<Node>>> = Vec::new();
    let mut current_group: Vec<P<Node>> = Vec::new();

    for &decl in decls {
        if !current_group.is_empty() && is_new_group(source_file, decl, &mut s) {
            groups.push(std::mem::take(&mut current_group));
        }
        current_group.push(decl);
    }

    if !current_group.is_empty() {
        groups.push(current_group);
    }

    groups
}

// organizeimports.go:409
fn is_new_group(source_file: P<SourceFile>, decl: P<Node>, s: &mut scanner::Scanner) -> bool {
    let Some(full_start) = decl.pos().source_offset() else {
        return false;
    };

    let text = source_file.text();
    let text_len = u32::try_from(text.len()).expect("source text length exceeds u32");

    if full_start >= text_len {
        return false;
    }

    let start_pos = scanner::skip_trivia(text, full_start);
    if start_pos <= full_start {
        return false;
    }

    let trivia_len = start_pos - full_start;
    s.set_text(&text[full_start as usize..start_pos as usize]);

    let mut number_of_new_lines = 0;
    while s.token_start() < trivia_len {
        let token_kind = s.scan();
        if token_kind == Kind::NewLineTrivia {
            number_of_new_lines += 1;
            if number_of_new_lines >= 2 {
                return true;
            }
        }
    }

    false
}

// organizeimports.go:445
fn coalesce_imports_worker(
    import_decls: &[P<Node>],
    comparer: &dyn Fn(&str, &str) -> Ordering,
    specifier_comparer: &NodeComparer,
    source_file: Option<P<SourceFile>>,
    change_tracker: &change::Tracker,
) -> Vec<P<Node>> {
    if import_decls.is_empty() {
        return import_decls.to_vec();
    }

    let mut import_groups_by_attributes: FxHashMap<String, Vec<P<Node>>> = FxHashMap::default();
    let mut attribute_keys: Vec<String> = Vec::new();

    for &import_decl in import_decls {
        let key = get_import_attributes_key(import_decl.as_import_declaration().attributes());
        if !import_groups_by_attributes.contains_key(&key) {
            attribute_keys.push(key.clone());
        }
        import_groups_by_attributes.entry(key).or_default().push(import_decl);
    }

    let mut coalesced_imports: Vec<P<Node>> = Vec::new();

    for attribute_key in &attribute_keys {
        let import_group_same_attrs = &import_groups_by_attributes[attribute_key];
        let categorized = get_categorized_imports(import_group_same_attrs);

        if let Some(i) = categorized.import_without_clause {
            coalesced_imports.push(i);
        }

        let mut source_phase_imports = categorized.source_phase_imports;
        goslices::sort_stable_func(&mut source_phase_imports, |&a, &b| {
            let a = a.as_import_declaration().import_clause.unwrap();
            let b = b.as_import_declaration().import_clause.unwrap();
            match (a.as_import_clause().name(), b.as_import_clause().name()) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(_), Some(_)) => specifier_comparer(a, b),
            }
        });
        coalesced_imports.extend(source_phase_imports);

        let factory = NodeFactory::new(NodeFactoryHooks::default());

        for (i, group) in
            [categorized.regular_imports, categorized.type_only_imports].into_iter().enumerate()
        {
            let mut group = group;
            if group.is_empty() {
                continue;
            }

            let is_type_only = i == 1;

            if !is_type_only
                && group.default_imports.len() == 1
                && group.namespace_imports.len() == 1
                && group.named_imports.is_empty()
            {
                let default_import = group.default_imports[0];
                let namespace_import = group.namespace_imports[0];

                let default_clause_node =
                    default_import.as_import_declaration().import_clause.unwrap();
                let default_clause = default_clause_node.as_import_clause();
                let namespace_bindings = namespace_import
                    .as_import_declaration()
                    .import_clause
                    .unwrap()
                    .as_import_clause()
                    .named_bindings;

                let new_clause = factory.update_import_clause(
                    default_clause_node,
                    default_clause.phase_modifier(),
                    default_clause.name(),
                    namespace_bindings,
                );
                let default_decl_node = default_import.as_import_declaration();
                let new_import_decl = factory.update_import_declaration(
                    default_import,
                    default_import.modifiers(),
                    Some(new_clause),
                    default_decl_node.module_specifier,
                    default_decl_node.attributes(),
                );
                coalesced_imports.push(new_import_decl);
                continue;
            }

            goslices::sort_func(&mut group.namespace_imports, |a, b| {
                let n1 = a
                    .as_import_declaration()
                    .import_clause
                    .unwrap()
                    .as_import_clause()
                    .named_bindings
                    .unwrap()
                    .as_namespace_import()
                    .name();
                let n2 = b
                    .as_import_declaration()
                    .import_clause
                    .unwrap()
                    .as_import_clause()
                    .named_bindings
                    .unwrap()
                    .as_namespace_import()
                    .name();
                comparer(n1.text(), n2.text())
            });

            coalesced_imports.extend(group.namespace_imports.iter().map(|&ns_import| {
                let ns_import_decl = ns_import.as_import_declaration();
                let clause_node = ns_import_decl.import_clause.unwrap();
                let clause = clause_node.as_import_clause();
                let new_clause = factory.update_import_clause(
                    clause_node,
                    clause.phase_modifier(),
                    None,
                    clause.named_bindings,
                );

                factory.update_import_declaration(
                    ns_import,
                    ns_import.modifiers(),
                    Some(new_clause),
                    ns_import_decl.module_specifier,
                    ns_import_decl.attributes(),
                )
            }));

            let first_default_import = group.default_imports.first().copied();
            let first_named_import = group.named_imports.first().copied();

            let Some(import_decl) = first_default_import.or(first_named_import) else {
                continue;
            };

            let mut new_default_import: Option<P<Node>> = None;
            let mut new_import_specifiers: Vec<P<Node>> = Vec::new();

            if group.default_imports.len() == 1 {
                new_default_import = group.default_imports[0]
                    .as_import_declaration()
                    .import_clause
                    .unwrap()
                    .as_import_clause()
                    .name();
            } else {
                new_import_specifiers.extend(group.default_imports.iter().map(|&default_import| {
                    let default_clause = default_import
                        .as_import_declaration()
                        .import_clause
                        .unwrap()
                        .as_import_clause();
                    let default_name = default_clause.name().unwrap();
                    let property_name = factory.new_identifier("default");

                    factory.new_import_specifier(false, Some(property_name), default_name)
                }));
            }

            new_import_specifiers.extend(get_new_import_specifiers(&group.named_imports, &factory));
            new_import_specifiers.sort_by(|&a, &b| specifier_comparer(a, b));

            let new_named_imports: Option<P<Node>> = if new_import_specifiers.is_empty() {
                if new_default_import.is_some() {
                    None
                } else {
                    Some(factory.new_named_imports(factory.new_node_list(vec![])))
                }
            } else {
                let sorted_list = factory.new_node_list(new_import_specifiers);
                if let Some(first_named_import) = first_named_import {
                    let first_named_bindings = first_named_import
                        .as_import_declaration()
                        .import_clause
                        .unwrap()
                        .as_import_clause()
                        .named_bindings
                        .unwrap();
                    let original_elements = first_named_bindings.as_named_imports().elements;
                    if original_elements.has_trailing_comma() {
                        sorted_list.loc.set(original_elements.loc.get());
                    }
                    Some(factory.update_named_imports(first_named_bindings, sorted_list))
                } else {
                    Some(factory.new_named_imports(sorted_list))
                }
            };

            if let (Some(source_file), Some(new_named_imports), Some(first_named_import)) =
                (source_file, new_named_imports, first_named_import)
            {
                let first_named_bindings = first_named_import
                    .as_import_declaration()
                    .import_clause
                    .unwrap()
                    .as_import_clause()
                    .named_bindings
                    .unwrap();
                if !ast::node_is_synthesized(first_named_bindings)
                    && !printer::range_is_on_single_line(first_named_bindings.loc(), source_file)
                {
                    change_tracker
                        .emit_context
                        .set_emit_flags(new_named_imports, EmitFlags::MultiLine);
                }
            }

            if is_type_only && new_default_import.is_some() && new_named_imports.is_some() {
                let import_decl_node = import_decl.as_import_declaration();

                let default_clause = factory.new_import_clause(
                    import_decl_node.import_clause.unwrap().as_import_clause().phase_modifier(),
                    new_default_import,
                    None,
                );
                let default_import_decl = factory.update_import_declaration(
                    import_decl,
                    import_decl.modifiers(),
                    Some(default_clause),
                    import_decl_node.module_specifier,
                    import_decl_node.attributes(),
                );
                coalesced_imports.push(default_import_decl);

                let named_decl_node = first_named_import.unwrap_or(import_decl);
                let named_import_decl_node = named_decl_node.as_import_declaration();
                let named_clause = factory.new_import_clause(
                    named_import_decl_node
                        .import_clause
                        .unwrap()
                        .as_import_clause()
                        .phase_modifier(),
                    None,
                    new_named_imports,
                );
                let named_import_decl = factory.update_import_declaration(
                    named_decl_node,
                    named_decl_node.modifiers(),
                    Some(named_clause),
                    named_import_decl_node.module_specifier,
                    named_import_decl_node.attributes(),
                );
                coalesced_imports.push(named_import_decl);
            } else {
                let import_decl_node = import_decl.as_import_declaration();
                let clause_node = import_decl_node.import_clause.unwrap();
                let new_clause = factory.update_import_clause(
                    clause_node,
                    clause_node.as_import_clause().phase_modifier(),
                    new_default_import,
                    new_named_imports,
                );
                let new_import_decl = factory.update_import_declaration(
                    import_decl,
                    import_decl.modifiers(),
                    Some(new_clause),
                    import_decl_node.module_specifier,
                    import_decl_node.attributes(),
                );
                coalesced_imports.push(new_import_decl);
            }
        }
    }
    coalesced_imports
}

// organizeimports.go:644
struct CategorizedImports {
    import_without_clause: Option<P<Node>>,
    source_phase_imports: Vec<P<Node>>,
    type_only_imports: ImportGroup,
    regular_imports: ImportGroup,
}

// organizeimports.go:650
#[derive(Default)]
struct ImportGroup {
    default_imports: Vec<P<Node>>,
    namespace_imports: Vec<P<Node>>,
    named_imports: Vec<P<Node>>,
}

impl ImportGroup {
    // organizeimports.go:656
    fn is_empty(&self) -> bool {
        self.default_imports.is_empty()
            && self.namespace_imports.is_empty()
            && self.named_imports.is_empty()
    }
}

// organizeimports.go:660
fn get_categorized_imports(import_decls: &[P<Node>]) -> CategorizedImports {
    let mut import_without_clause: Option<P<Node>> = None;
    let mut source_phase_imports = Vec::new();
    let mut type_only_imports = ImportGroup::default();
    let mut regular_imports = ImportGroup::default();

    for &import_decl in import_decls {
        let Some(clause_node) = import_decl.as_import_declaration().import_clause else {
            if import_without_clause.is_none() {
                import_without_clause = Some(import_decl);
            }
            continue;
        };

        let clause = clause_node.as_import_clause();
        if clause.phase_modifier() == Kind::SourceKeyword {
            source_phase_imports.push(import_decl);
            continue;
        }
        let group =
            if clause_node.is_type_only() { &mut type_only_imports } else { &mut regular_imports };

        let name = clause.name();
        let named_bindings = clause.named_bindings;

        if name.is_some() {
            group.default_imports.push(import_decl);
        }

        if let Some(nb) = named_bindings {
            match nb.kind() {
                Kind::NamespaceImport => group.namespace_imports.push(import_decl),
                Kind::NamedImports => group.named_imports.push(import_decl),
                _ => {}
            }
        }
    }

    CategorizedImports {
        import_without_clause,
        source_phase_imports,
        type_only_imports,
        regular_imports,
    }
}

// organizeimports.go:700
fn get_new_import_specifiers(named_imports: &[P<Node>], factory: &NodeFactory) -> Vec<P<Node>> {
    let mut result = Vec::new();

    for &named_import in named_imports {
        let Some(elements) = try_get_named_binding_elements(named_import) else {
            continue;
        };

        result.extend(elements.iter().map(|&elem| {
            let spec = elem.as_import_specifier();

            if let Some(property_name) = spec.property_name() {
                let property_text = property_name.text();
                let name_text = spec.name().text();

                if property_text == name_text {
                    let normalized =
                        factory.update_import_specifier(elem, spec.is_type_only, None, spec.name());
                    return normalized;
                }
            }

            elem
        }));
    }

    result
}

// organizeimports.go:729
fn try_get_named_binding_elements(named_import: P<Node>) -> Option<&'static [P<Node>]> {
    if named_import.kind() != Kind::ImportDeclaration {
        return None;
    }

    let import_decl = named_import.as_import_declaration();
    let clause_node = import_decl.import_clause?;

    let clause = clause_node.as_import_clause();
    let named_bindings = clause.named_bindings;

    if let Some(nb) = named_bindings
        && nb.kind() == Kind::NamedImports
    {
        let named_imports_node = nb.as_named_imports();
        return Some(named_imports_node.elements.nodes());
    }

    None
}

// organizeimports.go:751
fn get_top_level_export_groups(source_file: P<SourceFile>) -> Vec<Vec<P<Node>>> {
    let mut top_level_export_groups: Vec<Vec<P<Node>>> = Vec::new();
    let statements = source_file.statements.nodes();
    let statements_len = statements.len();

    let mut i = 0;
    let mut group_index = 0;
    while i < statements_len {
        if statements[i].kind() == Kind::ExportDeclaration {
            if group_index >= top_level_export_groups.len() {
                top_level_export_groups.push(Vec::new());
            }
            let export_decl = statements[i].as_export_declaration();
            if export_decl.module_specifier.is_some() {
                top_level_export_groups[group_index].push(statements[i]);
                i += 1;
            } else {
                while i < statements_len && statements[i].kind() == Kind::ExportDeclaration {
                    top_level_export_groups[group_index].push(statements[i]);
                    i += 1;
                }
                group_index += 1;
            }
        } else {
            i += 1;
            if group_index < top_level_export_groups.len()
                && !top_level_export_groups[group_index].is_empty()
            {
                group_index += 1;
            }
        }
    }

    let mut result = Vec::new();
    result.extend(
        top_level_export_groups
            .iter()
            .flat_map(|export_group| group_by_newline_contiguous(source_file, export_group)),
    );

    result
}

// organizeimports.go:792
fn organize_exports_worker(
    old_export_decls: &[P<Node>],
    comparer: &OrganizeImportsComparerSettings,
    source_file: P<SourceFile>,
    change_tracker: &mut change::Tracker,
) {
    if old_export_decls.is_empty() {
        return;
    }

    let specifier_comparer_func = lsutil::get_named_import_specifier_comparer(
        &lsutil::UserPreferences {
            organize_imports_type_order: comparer.type_order,
            ..Default::default()
        },
        comparer.named_import_comparer.clone(),
    );

    let new_export_decls = coalesce_exports_worker(
        old_export_decls,
        &specifier_comparer_func,
        comparer.module_specifier_comparer(),
        Some(source_file),
        change_tracker,
    );

    if !old_export_decls.is_empty() {
        if new_export_decls.is_empty() {
            change_tracker.delete_node_range(
                source_file,
                old_export_decls[0],
                old_export_decls[old_export_decls.len() - 1],
                LeadingTriviaOption::Exclude,
                TrailingTriviaOption::Include,
            );
        } else {
            for &exp in &new_export_decls {
                change_tracker.emit_context.add_emit_flags(exp, EmitFlags::NoLeadingComments);
            }

            let options = NodeOptions {
                leading_trivia_option: LeadingTriviaOption::Exclude,
                trailing_trivia_option: TrailingTriviaOption::Include,
                suffix: "\n".to_string(),
                ..Default::default()
            };

            change_tracker.replace_node_with_nodes(
                source_file,
                old_export_decls[0],
                new_export_decls,
                Some(options),
            );

            if old_export_decls.len() > 1 {
                for &decl in &old_export_decls[1..] {
                    change_tracker.delete(source_file, decl);
                }
            }
        }
    }
}

// organizeimports.go:844
fn coalesce_exports_worker(
    export_group: &[P<Node>],
    specifier_comparer: &NodeComparer,
    module_specifier_comparer: &dyn Fn(&str, &str) -> Ordering,
    source_file: Option<P<SourceFile>>,
    change_tracker: &change::Tracker,
) -> Vec<P<Node>> {
    if export_group.is_empty() {
        return export_group.to_vec();
    }

    let mut exports_by_module_specifier: FxHashMap<&'static str, Vec<P<Node>>> =
        FxHashMap::default();
    let mut module_specifier_order: Vec<&'static str> = Vec::new();

    for &export_decl in export_group {
        let export = export_decl.as_export_declaration();
        let module_specifier = export.module_specifier.map_or("", |m| m.text());
        if !exports_by_module_specifier.contains_key(module_specifier) {
            module_specifier_order.push(module_specifier);
        }
        exports_by_module_specifier.entry(module_specifier).or_default().push(export_decl);
    }

    module_specifier_order.sort_by(|a, b| {
        if a.is_empty() && !b.is_empty() {
            return Ordering::Greater;
        }
        if !a.is_empty() && b.is_empty() {
            return Ordering::Less;
        }
        module_specifier_comparer(a, b)
    });

    let mut coalesced_exports: Vec<P<Node>> = Vec::new();
    let factory = NodeFactory::new(NodeFactoryHooks::default());

    for module_specifier in &module_specifier_order {
        let group = &exports_by_module_specifier[module_specifier];

        let categorized = get_categorized_exports(group);

        if let Some(e) = categorized.export_without_clause {
            coalesced_exports.push(e);
        }

        for sub_group in [&categorized.named_exports, &categorized.type_only_exports] {
            if sub_group.is_empty() {
                continue;
            }

            let mut new_export_specifiers: Vec<P<Node>> = Vec::new();
            new_export_specifiers.extend(
                sub_group
                    .iter()
                    .filter_map(|&export_decl| {
                        if let Some(export_clause) =
                            export_decl.as_export_declaration().export_clause
                        {
                            if export_clause.kind() == Kind::NamedExports {
                                let named_exports = export_clause.as_named_exports();
                                Some(named_exports.elements.nodes().iter().copied())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .flatten(),
            );

            new_export_specifiers.sort_by(|&a, &b| specifier_comparer(a, b));

            let export_decl_node = sub_group[0];
            let export_decl = export_decl_node.as_export_declaration();

            let mut updated_export_clause: Option<P<Node>> = None;
            if let Some(export_clause) = export_decl.export_clause {
                if export_clause.kind() == Kind::NamedExports {
                    let sorted_list = factory.new_node_list(new_export_specifiers);
                    let updated = factory.update_named_exports(export_clause, sorted_list);
                    updated_export_clause = Some(updated);

                    if let Some(source_file) = source_file
                        && !ast::node_is_synthesized(export_clause)
                        && !printer::range_is_on_single_line(export_clause.loc(), source_file)
                    {
                        change_tracker.emit_context.set_emit_flags(updated, EmitFlags::MultiLine);
                    }
                } else {
                    updated_export_clause = Some(export_clause);
                }
            }

            let new_export_decl = factory.update_export_declaration(
                export_decl_node,
                export_decl_node.modifiers(),
                export_decl.is_type_only,
                updated_export_clause,
                export_decl.module_specifier,
                export_decl.attributes,
            );
            coalesced_exports.push(new_export_decl);
        }
    }

    coalesced_exports
}

// organizeimports.go:928
struct CategorizedExports {
    export_without_clause: Option<P<Node>>,
    named_exports: Vec<P<Node>>,
    type_only_exports: Vec<P<Node>>,
}

// organizeimports.go:934
fn get_categorized_exports(export_group: &[P<Node>]) -> CategorizedExports {
    let mut export_without_clause: Option<P<Node>> = None;
    let mut named_exports = Vec::new();
    let mut type_only_exports = Vec::new();

    for &export_decl in export_group {
        let export = export_decl.as_export_declaration();
        if export.export_clause.is_none() {
            if export_without_clause.is_none() {
                export_without_clause = Some(export_decl);
            }
        } else if export.is_type_only {
            type_only_exports.push(export_decl);
        } else {
            named_exports.push(export_decl);
        }
    }

    CategorizedExports { export_without_clause, named_exports, type_only_exports }
}
