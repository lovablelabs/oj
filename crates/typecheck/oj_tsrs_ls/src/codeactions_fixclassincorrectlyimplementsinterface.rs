use std::collections::hash_map::Entry;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, ModifierFlags, Node, SourceFile, Symbol};
use tsrs_checker::{self as checker, Checker, Type};
use tsrs_core::collections::Set;
use tsrs_core::context::{Context, locale_from_context};
use tsrs_core::{P, TextRange};
use tsrs_diagnostics as diagnostics;
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport::{self, ImportAdder};
use crate::change;
use crate::codeactions::{
    CodeAction, CodeFixContext, CodeFixProvider, CombinedCodeActions, is_fixable_diagnostic,
};
use crate::codeactions_missingmemberfixer::{new_missing_member_fixer, preserveOptionalFlags};
use crate::diagnostics::get_all_diagnostics;

// codeactions_fixclassincorrectlyimplementsinterface.go:19
const FIX_CLASS_INCORRECTLY_IMPLEMENTS_INTERFACE_FIX_ID: &str =
    "fixClassIncorrectlyImplementsInterface";

// codeactions_fixclassincorrectlyimplementsinterface.go:21
fn fix_class_incorrectly_implements_interface_error_codes() -> &'static [i32] {
    static CODES: std::sync::LazyLock<Vec<i32>> = std::sync::LazyLock::new(|| {
        vec![
            diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_INTERFACE_1.code(),
            diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_CLASS_1_DID_YOU_MEAN_TO_EXTEND_1_AND_INHERIT_ITS_MEMBERS_AS_A_SUBCLASS.code(),
        ]
    });
    &CODES
}

// codeactions_fixclassincorrectlyimplementsinterface.go:26
pub static FIX_CLASS_INCORRECTLY_IMPLEMENTS_INTERFACE_PROVIDER: std::sync::LazyLock<
    CodeFixProvider,
> = std::sync::LazyLock::new(|| CodeFixProvider {
    error_codes: fix_class_incorrectly_implements_interface_error_codes(),
    get_code_actions: get_code_actions_to_fix_class_incorrectly_implements_interface,
    get_all_code_actions: Some(get_all_code_actions_to_fix_class_incorrectly_implements_interface),
});

// codeactions_fixclassincorrectlyimplementsinterface.go:33
fn get_code_actions_to_fix_class_incorrectly_implements_interface(
    context: &Context,
    fix_context: &CodeFixContext,
) -> Result<Vec<CodeAction>, lsproto::Error> {
    let Some(class_declaration) = get_class(fix_context.source_file, fix_context.span) else {
        return Ok(Vec::new());
    };

    let implements_types = ast::get_implements_heritage_clause_elements(class_declaration);

    let mut type_checker =
        fix_context.program.get_type_checker_for_file(context, fix_context.source_file);

    let mut actions = Vec::new();
    for &implemented_type_node in implements_types {
        let mut change_tracker = change::new_tracker(
            context,
            &fix_context.program.options(),
            fix_context.ls.format_options(),
            Arc::clone(&fix_context.ls.converters),
        );
        let mut import_adder = create_import_adder(context, fix_context, &mut type_checker)?;

        add_changes(
            context,
            fix_context,
            &mut change_tracker,
            import_adder.as_deref_mut(),
            &mut type_checker,
            class_declaration,
            implemented_type_node,
        );
        let changes =
            get_changes(&mut change_tracker, import_adder.as_deref_mut(), fix_context.source_file);
        if changes.is_empty() {
            continue;
        }

        actions.push(CodeAction {
            description: diagnostics::IMPLEMENT_INTERFACE_0
                .localize(&[&scanner::get_text_of_node(implemented_type_node)]),
            changes,
            fix_id: FIX_CLASS_INCORRECTLY_IMPLEMENTS_INTERFACE_FIX_ID.to_string(),
        });
    }
    Ok(actions)
}

// codeactions_fixclassincorrectlyimplementsinterface.go:69
fn get_all_code_actions_to_fix_class_incorrectly_implements_interface(
    context: &Context,
    fix_context: &CodeFixContext,
) -> Result<Option<CombinedCodeActions>, lsproto::Error> {
    let all_diags = get_all_diagnostics(context, fix_context.program, fix_context.source_file);

    let mut type_checker =
        fix_context.program.get_type_checker_for_file(context, fix_context.source_file);

    let mut change_tracker = change::new_tracker(
        context,
        &fix_context.program.options(),
        fix_context.ls.format_options(),
        Arc::clone(&fix_context.ls.converters),
    );
    let mut import_adder = create_import_adder(context, fix_context, &mut type_checker)?;

    let mut seen_class_declarations: Set<P<Node>> = Set::new();

    for diag in all_diags {
        if is_fixable_diagnostic(diag, fix_class_incorrectly_implements_interface_error_codes()) {
            let Some(diag_loc) = diag.loc() else {
                continue;
            };
            let Some(class_declaration) = get_class(fix_context.source_file, diag_loc) else {
                continue;
            };
            if seen_class_declarations.add_if_absent(class_declaration) {
                let implements_types =
                    ast::get_implements_heritage_clause_elements(class_declaration);
                for &implemented_type_node in implements_types {
                    add_changes(
                        context,
                        fix_context,
                        &mut change_tracker,
                        import_adder.as_deref_mut(),
                        &mut type_checker,
                        class_declaration,
                        implemented_type_node,
                    );
                }
            }
        }
    }

    let changes =
        get_changes(&mut change_tracker, import_adder.as_deref_mut(), fix_context.source_file);
    if changes.is_empty() {
        return Ok(None);
    }

    Ok(Some(CombinedCodeActions {
        description: diagnostics::IMPLEMENT_ALL_UNIMPLEMENTED_INTERFACES.localize(&[]),
        changes,
    }))
}

// codeactions_fixclassincorrectlyimplementsinterface.go:112
// Go builds one missing member fixer that keeps the change tracker; here a fixer only lives while it creates a
// member (it holds the tracker shared), and the insertions in between borrow the tracker mutably. The fixer has
// no state of its own, so the sequence of creations and insertions is Go's.
fn add_changes(
    context: &Context,
    fix_context: &CodeFixContext,
    change_tracker: &mut change::Tracker,
    mut import_adder: Option<&mut (dyn ImportAdder + 'static)>,
    type_checker: &mut Checker,
    class_declaration: P<Node>,
    implemented_type_node: P<Node>,
) {
    let locale = locale_from_context(context);
    let constructor = get_constructor(Some(class_declaration));
    let implemented_type = type_checker.get_type_at_location(implemented_type_node);
    let class_type = type_checker.get_type_at_location(class_declaration);

    if type_checker.get_number_index_type(class_type).is_none() {
        let number_type = type_checker.get_number_type();
        let member = {
            let mut missing_member_fixer = new_missing_member_fixer(
                change_tracker,
                fix_context.program,
                type_checker,
                fix_context.ls.user_preferences().clone(),
                import_adder.as_deref_mut(),
                locale.clone(),
            );
            missing_member_fixer.create_index_signature_declaration_from_type(
                class_declaration,
                implemented_type,
                number_type,
            )
        };
        if let Some(member) = member {
            insert_interface_member_node(
                change_tracker,
                fix_context.source_file,
                class_declaration,
                constructor,
                member,
            );
        }
    }

    if type_checker.get_string_index_type(class_type).is_none() {
        let string_type = type_checker.get_string_type();
        let member = {
            let mut missing_member_fixer = new_missing_member_fixer(
                change_tracker,
                fix_context.program,
                type_checker,
                fix_context.ls.user_preferences().clone(),
                import_adder.as_deref_mut(),
                locale.clone(),
            );
            missing_member_fixer.create_index_signature_declaration_from_type(
                class_declaration,
                implemented_type,
                string_type,
            )
        };
        if let Some(member) = member {
            insert_interface_member_node(
                change_tracker,
                fix_context.source_file,
                class_declaration,
                constructor,
                member,
            );
        }
    }

    let missing_members = get_missing_members(type_checker, class_declaration, &[implemented_type]);
    for member in missing_members {
        let member_nodes = {
            let mut missing_member_fixer = new_missing_member_fixer(
                change_tracker,
                fix_context.program,
                type_checker,
                fix_context.ls.user_preferences().clone(),
                import_adder.as_deref_mut(),
                locale.clone(),
            );
            missing_member_fixer.create_member_from_symbol(
                member,
                class_declaration,
                fix_context.source_file,
                None, /*body*/
                preserveOptionalFlags::All,
                false, /*abstract*/
            )
        };
        for member_node in member_nodes {
            insert_interface_member_node(
                change_tracker,
                fix_context.source_file,
                class_declaration,
                constructor,
                member_node,
            );
        }
    }
}

// codeactions_fixclassincorrectlyimplementsinterface.go:145
fn get_changes(
    change_tracker: &mut change::Tracker,
    import_adder: Option<&mut (dyn ImportAdder + 'static)>,
    source_file: P<SourceFile>,
) -> Vec<lsproto::TextEdit> {
    let (mut changes, unmappable) = change_tracker.get_changes();
    if !unmappable.is_empty() {
        return Vec::new();
    }
    let mut file_changes = changes
        .shift_remove(crate::lsconv::Script::original_file_name(&source_file))
        .unwrap_or_default();
    if let Some(import_adder) = import_adder
        && import_adder.has_fixes()
    {
        file_changes.extend(import_adder.edits());
    }
    file_changes
}

// codeactions_fixclassincorrectlyimplementsinterface.go:157
fn insert_interface_member_node(
    change_tracker: &mut change::Tracker,
    source_file: P<SourceFile>,
    class_declaration: P<Node>,
    constructor: Option<P<Node>>,
    member: P<Node>,
) {
    match constructor {
        None => change_tracker.insert_member_at_start(source_file, class_declaration, member),
        Some(constructor) => change_tracker.insert_node_after(source_file, constructor, member),
    }
}

// codeactions_fixclassincorrectlyimplementsinterface.go:165
fn get_class(source_file: P<SourceFile>, span: TextRange) -> Option<P<Node>> {
    let token = astnav::get_token_at_position(
        source_file,
        span.pos().expect_source("diagnostics in source files have source positions"),
    );
    ast::get_containing_class(token)
}

// codeactions_fixclassincorrectlyimplementsinterface.go:173
fn get_constructor(class_declaration: Option<P<Node>>) -> Option<P<Node>> {
    let member_list = class_declaration?.member_list()?;
    member_list.nodes().iter().copied().find(|&member| ast::is_constructor_declaration(member))
}

// codeactions_fixclassincorrectlyimplementsinterface.go:185
fn get_missing_members(
    type_checker: &mut Checker,
    class_declaration: P<Node>,
    implemented_types: &[P<Type>],
) -> Vec<P<Symbol>> {
    let inherited_members = get_inherited_members(type_checker, class_declaration);
    let mut seen_members: FxHashMap<&'static str, P<Symbol>> = FxHashMap::default();

    let class_members = class_declaration.symbol().and_then(|s| s.members());

    let mut missing_members = Vec::new();
    for &implemented_type in implemented_types {
        missing_members.extend(
            type_checker.get_properties_of_type(implemented_type).iter().filter_map(|&symbol| {
                if class_members.is_some_and(|m| m.lookup(symbol.name()).is_some()) {
                    return None;
                }
                if inherited_members.contains_key(symbol.name()) {
                    return None;
                }
                let Entry::Vacant(entry) = seen_members.entry(symbol.name()) else {
                    return None;
                };
                let flags = checker::get_declaration_modifier_flags_from_symbol_exported(symbol);
                if flags.intersects(ModifierFlags::Private) {
                    None
                } else {
                    entry.insert(symbol);
                    Some(symbol)
                }
            }),
        );
    }
    missing_members
}

// codeactions_fixclassincorrectlyimplementsinterface.go:216
fn get_inherited_members(
    type_checker: &mut Checker,
    class_declaration: P<Node>,
) -> FxHashMap<&'static str, P<Symbol>> {
    let Some(type_node) = ast::get_class_extends_heritage_element(class_declaration) else {
        return FxHashMap::default();
    };

    let base_type = type_checker.get_type_at_location(type_node);

    let mut inherited_members = FxHashMap::default();
    inherited_members.extend(type_checker.get_properties_of_type(base_type).iter().filter_map(
        |&symbol| {
            let flags = checker::get_declaration_modifier_flags_from_symbol_exported(symbol);
            if flags.intersects(ModifierFlags::Private) {
                None
            } else {
                Some((symbol.name(), symbol))
            }
        },
    ));
    inherited_members
}

// codeactions_fixclassincorrectlyimplementsinterface.go:236
fn create_import_adder(
    context: &Context,
    fix_context: &CodeFixContext,
    type_checker: &mut Checker,
) -> Result<Option<Box<dyn ImportAdder>>, lsproto::Error> {
    let view =
        fix_context.ls.get_prepared_auto_import_view(fix_context.source_file, type_checker)?;
    // Go returns a nil adder for a nil view; a prepared view is never nil.
    Ok(Some(autoimport::new_import_adder(
        context,
        fix_context.program,
        type_checker,
        fix_context.source_file,
        view,
        fix_context.ls.format_options(),
        Arc::clone(&fix_context.ls.converters),
        fix_context.ls.user_preferences().clone(),
    )))
}
