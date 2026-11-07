use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_ast::{
    self as ast, Node, NodeFactory, NodeFactoryHooks, NodeVisitor, NodeVisitorHooks, SourceFile,
    Symbol,
};
use tsrs_checker::{Checker, Flags, Type};
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_lsproto as lsproto;

use super::export::{Export, symbol_to_export};
use super::fix::{
    Fix, NewImportBinding, add_import_type, add_namespace_qualifier, add_to_existing_import,
    get_add_to_existing_import_fix, get_new_imports, get_new_requires, insert_imports,
};
use super::util::get_default_like_export_name_from_declaration;
use super::view::View;
use crate::change;
use crate::lsconv::Converters;
use crate::lsutil::{self, FormatCodeSettings, UserPreferences};

pub(crate) type IdToSymbol = P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>;

// import_adder.go:24
pub trait ImportAdder {
    fn has_fixes(&self) -> bool;
    fn add_import_from_exported_symbol(
        &mut self,
        symbol: P<Symbol>,
        is_valid_type_only_use_site: bool,
    );
    fn add_import_fix(&mut self, fix: &Fix);
    fn edits(&mut self) -> Vec<lsproto::TextEdit>;
}

// import_adder.go:31 (Go's importClauseOrBindingPattern field is never read; it is the map key)
// addToExistingState tracks modifications to an existing import clause or binding pattern
struct AddToExistingState {
    default_import: Option<NewImportBinding>,
    named_imports: FxHashMap<String, NewImportBinding>,
}

// import_adder.go:38
// importsCollection tracks new imports to be created for a given module specifier
#[derive(Default)]
struct ImportsCollection {
    default_import: Option<NewImportBinding>,
    named_imports: FxHashMap<String, NewImportBinding>,
    namespace_like_import: Option<NewImportBinding>,
    use_require: bool,
}

// import_adder.go:45
fn new_imports_key(module_specifier: &str, top_level_type_only: bool) -> String {
    if top_level_type_only {
        return format!("1|{module_specifier}");
    }
    format!("0|{module_specifier}")
}

// import_adder.go:52
struct ImportAdderImpl {
    // Context
    ctx: Context,
    view: View,
    format_options: FormatCodeSettings,
    converters: Arc<Converters>,
    preferences: UserPreferences,

    // State
    add_to_namespace: Vec<Fix>, // Namespace fixes don't conflict, so just build a list
    import_type: Vec<Fix>,      // JSDoc type import fixes
    // Go maps (random iteration order); insertion order here.
    add_to_existing: OrderedMap<P<Node>, AddToExistingState>, // importClauseOrBindingPattern -> default or named bindings
    new_imports: OrderedMap<String, ImportsCollection>, // module specifier + type only -> imports
                                                        // !!! removeExisting, verbatimImports?
}

// import_adder.go:70
// Go keeps the checker in the adder; the view built from the same checker already holds it (`View::checker`).
pub fn new_import_adder(
    ctx: &Context,
    _program: &'static Program,
    _checker: &mut Checker,
    _file: P<SourceFile>,
    view: View,
    format_options: FormatCodeSettings,
    converters: Arc<Converters>,
    preferences: UserPreferences,
) -> Box<dyn ImportAdder> {
    Box::new(ImportAdderImpl {
        ctx: ctx.clone(),
        view,
        format_options,
        converters,
        preferences,
        add_to_namespace: Vec::new(),
        import_type: Vec::new(),
        add_to_existing: OrderedMap::default(),
        new_imports: OrderedMap::default(),
    })
}

impl ImportAdder for ImportAdderImpl {
    // import_adder.go:93
    fn has_fixes(&self) -> bool {
        !self.add_to_namespace.is_empty()
            || !self.import_type.is_empty()
            || !self.add_to_existing.is_empty()
            || !self.new_imports.is_empty()
    }

    // import_adder.go:101
    // !!! referenceImport
    fn add_import_from_exported_symbol(
        &mut self,
        exported_symbol: P<Symbol>,
        is_valid_type_only_use_site: bool,
    ) {
        let checker = self.view.checker();
        let skipped = checker.skip_alias(exported_symbol);
        let symbol = checker.get_merged_symbol(skipped);
        let export_infos = self.get_all_exports_for_symbol(symbol);
        if export_infos.is_empty() {
            // If no exportInfo is found, this means export could not be resolved when we have filtered for autoImportFileExcludePatterns,
            //     so we should not generate an import.
            // debug.Assert(len(adder.ls.UserPreferences().AutoImportFileExcludePatterns) > 0)
            return;
        }
        let fix = self.get_import_fix_for_symbol(&export_infos, is_valid_type_only_use_site);
        if let Some(fix) = fix {
            // !!! referenceImport -> propertyName
            self.add_import_fix(&fix);
        }
    }

    // import_adder.go:181
    // AddImportFix adds a fix to the import adder, accumulating it with other fixes
    // so that multiple imports from the same module are coalesced into a single import statement.
    fn add_import_fix(&mut self, fix: &Fix) {
        let symbol_name = fix.name.clone();
        let compiler_options = self.view.program.options();

        match fix.kind {
            lsproto::AutoImportFixKind::USE_NAMESPACE => self.add_to_namespace.push(fix.clone()),
            lsproto::AutoImportFixKind::JSDOC_TYPE_IMPORT => self.import_type.push(fix.clone()),
            lsproto::AutoImportFixKind::ADD_TO_EXISTING => {
                let existing_fix = get_add_to_existing_import_fix(self.view.importing_file, fix);
                let entry = self
                    .add_to_existing
                    .entry(existing_fix.import_clause_or_binding_pattern)
                    .or_insert_with(|| AddToExistingState {
                        default_import: None,
                        named_imports: FxHashMap::default(),
                    });

                if fix.import_kind == lsproto::ImportKind::NAMED {
                    let binding = entry.named_imports.entry(symbol_name.clone()).or_default();
                    *binding = NewImportBinding {
                        kind: lsproto::ImportKind::NAMED,
                        name: symbol_name,
                        add_as_type_only: reduce_add_as_type_only_values(
                            binding.add_as_type_only,
                            fix.add_as_type_only,
                        ),
                        property_name: existing_fix
                            .named_import
                            .as_ref()
                            .unwrap()
                            .property_name
                            .clone(),
                    };
                } else {
                    // Default import
                    assert!(
                        entry.default_import.as_ref().is_none_or(|d| d.name == symbol_name),
                        "(Add to Existing) Default import should be missing or match symbolName"
                    );
                    let prev_type_only = entry
                        .default_import
                        .as_ref()
                        .map(|d| d.add_as_type_only)
                        .unwrap_or_default();
                    entry.default_import = Some(NewImportBinding {
                        kind: lsproto::ImportKind::DEFAULT,
                        name: symbol_name,
                        add_as_type_only: reduce_add_as_type_only_values(
                            prev_type_only,
                            fix.add_as_type_only,
                        ),
                        ..Default::default()
                    });
                }
            }

            lsproto::AutoImportFixKind::ADD_NEW => {
                let entry = self.get_new_import_entry(
                    &fix.module_specifier,
                    fix.import_kind,
                    fix.use_require,
                    fix.add_as_type_only,
                );
                assert!(
                    entry.use_require == fix.use_require,
                    "(Add new) Tried to add an `import` and a `require` for the same module"
                );

                match fix.import_kind {
                    lsproto::ImportKind::DEFAULT => {
                        assert!(
                            entry.default_import.as_ref().is_none_or(|d| d.name == symbol_name),
                            "(Add new) Default import should be missing or match symbolName"
                        );
                        let prev_type_only = entry
                            .default_import
                            .as_ref()
                            .map(|d| d.add_as_type_only)
                            .unwrap_or_default();
                        entry.default_import = Some(NewImportBinding {
                            kind: lsproto::ImportKind::DEFAULT,
                            name: symbol_name,
                            add_as_type_only: reduce_add_as_type_only_values(
                                prev_type_only,
                                fix.add_as_type_only,
                            ),
                            ..Default::default()
                        });
                    }

                    lsproto::ImportKind::NAMED => {
                        let binding = entry.named_imports.entry(symbol_name.clone()).or_default();
                        *binding = NewImportBinding {
                            kind: lsproto::ImportKind::NAMED,
                            name: symbol_name,
                            add_as_type_only: reduce_add_as_type_only_values(
                                binding.add_as_type_only,
                                fix.add_as_type_only,
                            ),
                            // !!! propertyName
                            ..Default::default()
                        };
                    }

                    lsproto::ImportKind::COMMON_JS => {
                        if compiler_options.verbatim_module_syntax == tsrs_core::Tristate::True {
                            let binding =
                                entry.named_imports.entry(symbol_name.clone()).or_default();
                            *binding = NewImportBinding {
                                kind: lsproto::ImportKind::COMMON_JS,
                                name: symbol_name,
                                add_as_type_only: reduce_add_as_type_only_values(
                                    binding.add_as_type_only,
                                    fix.add_as_type_only,
                                ),
                                // !!! propertyName
                                ..Default::default()
                            };
                        } else {
                            assert!(
                                entry
                                    .namespace_like_import
                                    .as_ref()
                                    .is_none_or(|n| n.name == symbol_name),
                                "Namespacelike import should be missing or match symbolName"
                            );
                            entry.namespace_like_import = Some(NewImportBinding {
                                kind: lsproto::ImportKind::COMMON_JS,
                                name: symbol_name,
                                add_as_type_only: fix.add_as_type_only,
                                ..Default::default()
                            });
                        }
                    }

                    lsproto::ImportKind::NAMESPACE => {
                        assert!(
                            entry
                                .namespace_like_import
                                .as_ref()
                                .is_none_or(|n| n.name == symbol_name),
                            "Namespacelike import should be missing or match symbolName"
                        );
                        entry.namespace_like_import = Some(NewImportBinding {
                            kind: lsproto::ImportKind::NAMESPACE,
                            name: symbol_name,
                            add_as_type_only: fix.add_as_type_only,
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
            }

            lsproto::AutoImportFixKind::PROMOTE_TYPE_ONLY => {
                // Excluding from fix-all
            }
            _ => panic!("Unexpected fix kind: {:?}", fix.kind),
        }
    }

    // import_adder.go:118
    fn edits(&mut self) -> Vec<lsproto::TextEdit> {
        // !!! organize imports?
        let mut tracker = change::new_tracker(
            &self.ctx,
            &self.view.program.options(),
            self.format_options.clone(),
            Arc::clone(&self.converters),
        );
        let quote_preference =
            lsutil::get_quote_preference(self.view.importing_file, &self.preferences);
        for fix in &self.add_to_namespace {
            add_namespace_qualifier(fix, &mut tracker, self.view.importing_file);
        }
        for fix in &self.import_type {
            add_import_type(fix, self.view.importing_file, &self.preferences, &mut tracker);
        }
        for (&clause_or_pattern, entry) in &self.add_to_existing {
            add_to_existing_import(
                &mut tracker,
                self.view.importing_file,
                clause_or_pattern,
                entry.default_import.as_ref(),
                &sorted_named_imports(&entry.named_imports),
                &self.preferences,
            );
        }

        let mut new_declarations: Vec<P<Node>> = Vec::new();
        new_declarations.extend(self.new_imports.iter().flat_map(|(key, new_import)| {
            let module_specifier = &key[2..]; // From `${0 | 1}|${moduleSpecifier}` format

            if new_import.use_require {
                get_new_requires(
                    &tracker,
                    module_specifier,
                    quote_preference,
                    new_import.default_import.as_ref(),
                    &sorted_named_imports(&new_import.named_imports),
                    new_import.namespace_like_import.as_ref(),
                    &self.view.program.options(),
                )
            } else {
                get_new_imports(
                    &tracker,
                    module_specifier,
                    quote_preference,
                    new_import.default_import.as_ref(),
                    &sorted_named_imports(&new_import.named_imports),
                    new_import.namespace_like_import.as_ref(),
                    &self.view.program.options(),
                    &self.preferences,
                )
            }
        }));

        if !new_declarations.is_empty() {
            insert_imports(
                &mut tracker,
                self.view.importing_file,
                &new_declarations,
                true, /*blankLineBetween*/
                &self.preferences,
            );
        }

        // Unmappable files are dropped by GetChanges, so a content-mapped importing file that cannot be
        // faithfully rewritten yields no edits rather than a corrupting one.
        let (mut changes, _) = tracker.get_changes();
        changes
            .shift_remove(crate::lsconv::Script::original_file_name(&self.view.importing_file))
            .unwrap_or_default()
    }
}

// import_adder.go:172
fn sorted_named_imports(m: &FxHashMap<String, NewImportBinding>) -> Vec<NewImportBinding> {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    keys.into_iter().map(|k| m[k].clone()).collect()
}

// import_adder.go:325
// `NotAllowed` overrides `Required` because one addition of a new import might be required to be type-only
// because of `--importsNotUsedAsValues=error`, but if a second addition of the same import is `NotAllowed`
// to be type-only, the reason the first one was `Required` - the unused runtime dependency - is now moot.
// Alternatively, if one addition is `Required` because it has no value meaning under `--preserveValueImports`
// and `--isolatedModules`, it should be impossible for another addition to be `NotAllowed` since that would
// mean a type is being referenced in a value location.
fn reduce_add_as_type_only_values(
    prev_value: lsproto::AddAsTypeOnly,
    new_value: lsproto::AddAsTypeOnly,
) -> lsproto::AddAsTypeOnly {
    if new_value.0 > prev_value.0 {
        return new_value;
    }
    prev_value
}

impl ImportAdderImpl {
    // import_adder.go:332
    fn get_new_import_entry(
        &mut self,
        module_specifier: &str,
        import_kind: lsproto::ImportKind,
        use_require: bool,
        add_as_type_only: lsproto::AddAsTypeOnly,
    ) -> &mut ImportsCollection {
        // A default import that requires type-only makes the whole import type-only.
        // (We could add `default` as a named import, but that style seems undesirable.)
        // Under `--preserveValueImports` and `--importsNotUsedAsValues=error`, if a
        // module default-exports a type but named-exports some values (weird), you would
        // have to use a type-only default import and non-type-only named imports. These
        // require two separate import declarations, so we build this into the map key.
        let type_only_key = new_imports_key(module_specifier, true /*topLevelTypeOnly*/);
        let non_type_only_key = new_imports_key(module_specifier, false /*topLevelTypeOnly*/);
        let key = if (import_kind == lsproto::ImportKind::DEFAULT
            && add_as_type_only == lsproto::AddAsTypeOnly::REQUIRED)
            || (add_as_type_only == lsproto::AddAsTypeOnly::ALLOWED
                && self.new_imports.contains_key(&type_only_key))
        {
            type_only_key
        } else {
            non_type_only_key
        };
        self.new_imports
            .entry(key)
            .or_insert_with(|| ImportsCollection { use_require, ..Default::default() })
    }

    // import_adder.go:374
    fn get_all_exports_for_symbol(&self, symbol: P<Symbol>) -> Vec<Arc<Export>> {
        if let Some(export) = symbol_to_export(symbol, self.view.checker()) {
            return self.view.search_by_export_id(&export.export_id);
        }
        Vec::new()
    }

    // import_adder.go:490
    fn get_import_fix_for_symbol(
        &self,
        exports: &[Arc<Export>],
        is_valid_type_only_use_site: bool,
    ) -> Option<Fix> {
        let view = &self.view;
        let mut fixes: Vec<Arc<Fix>> = exports
            .iter()
            .flat_map(|export| {
                view.get_fixes(
                    export,
                    false, /*forJSX*/
                    is_valid_type_only_use_site,
                    None, /*usagePosition*/
                )
            })
            .collect();
        tsrs_core::goslices::sort_func(&mut fixes, |a, b| view.compare_fixes_for_ranking(a, b));
        fixes.first().map(|f| (**f).clone())
    }
}

// import_adder.go:382
pub(crate) fn type_to_auto_importable_type_node(
    c: &mut Checker,
    import_adder: Option<&mut (dyn ImportAdder + 'static)>,
    t: P<Type>,
    context_node: P<Node>, // !!! flags
) -> Option<P<Node>> {
    let id_to_symbol: IdToSymbol = P::new(RefCell::new(FxHashMap::default()));
    let type_node = c.type_to_type_node(t, Some(context_node), Flags::None, Some(id_to_symbol))?;
    Some(type_node_to_auto_importable_type_node(type_node, import_adder, id_to_symbol))
}

// import_adder.go:398
// TypeNodeToAutoImportableTypeNode converts import type references in a type node to
// simple type references and registers needed imports with the import adder.
fn type_node_to_auto_importable_type_node(
    mut type_node: P<Node>,
    import_adder: Option<&mut (dyn ImportAdder + 'static)>,
    id_to_symbol: IdToSymbol,
) -> P<Node> {
    let (reference_type_node, importable_symbols) =
        try_get_auto_importable_reference_from_type_node(Some(type_node), id_to_symbol);
    if let Some(reference_type_node) = reference_type_node {
        if let Some(import_adder) = import_adder {
            import_symbols(import_adder, &importable_symbols);
        }
        type_node = reference_type_node;
    }

    // !!! handle type node reuse: nodes needs to be fresh here but also preserve symbols
    type_node
}

// import_adder.go:415
fn import_symbols(import_adder: &mut dyn ImportAdder, symbols: &[P<Symbol>]) {
    for &symbol in symbols {
        import_adder.add_import_from_exported_symbol(symbol, true /*isValidTypeOnlyUseSite*/);
    }
}

// import_adder.go:427
// Given a type node containing 'import("./a").SomeType<import("./b").OtherType<...>>',
// returns an equivalent type reference node with any nested ImportTypeNodes also replaced
// with type references, and a list of symbols that must be imported to use the type reference.
// TryGetAutoImportableReferenceFromTypeNode converts import type references in a type node
// to simple type references and returns the transformed type node and the symbols that need
// to be imported.
pub(crate) fn try_get_auto_importable_reference_from_type_node(
    import_type_node: Option<P<Node>>,
    id_to_symbol: IdToSymbol,
) -> (Option<P<Node>>, Vec<P<Symbol>>) {
    let symbols: Rc<RefCell<Vec<P<Symbol>>>> = Rc::new(RefCell::new(Vec::new()));
    let factory = NodeFactory::new(NodeFactoryHooks::default());
    let visit_symbols = Rc::clone(&symbols);
    let visit_factory = factory.clone();
    let visit = move |visitor: &mut NodeVisitor, node: P<Node>| -> Option<P<Node>> {
        if ast::is_literal_import_type_node(node) && node.as_import_type_node().qualifier.is_some()
        {
            let import_type_node = node.as_import_type_node();
            let qualifier_node = import_type_node.qualifier.unwrap();
            // Symbol for the left-most thing after the dot
            let first_identifier = ast::get_first_identifier(qualifier_node);
            let symbol = id_to_symbol.borrow().get(&first_identifier).copied();
            let Some(symbol) = symbol else {
                // if symbol is missing then this doesn't come from a synthesized import type node
                // it has to be an import type node authored by the user and thus it has to be valid
                // it can't refer to reserved internal symbol names and such
                return Some(node.visit_each_child(visitor));
            };
            let name = get_name_for_exported_symbol(symbol, false /*preferCapitalized*/);
            let qualifier = if name == first_identifier.text() {
                qualifier_node
            } else {
                replace_first_identifier_of_entity_name(
                    &visit_factory,
                    qualifier_node,
                    visit_factory.new_identifier(tsrs_core::alloc_str(&name)),
                )
            };
            visit_symbols.borrow_mut().push(symbol);
            let type_arguments = visitor.visit_nodes(import_type_node.type_arguments());
            return Some(visit_factory.new_type_reference_node(qualifier, type_arguments));
        }
        visitor.visit_each_child(Some(node))
    };
    let mut visitor =
        ast::new_node_visitor(Some(Rc::new(visit)), Some(factory), NodeVisitorHooks::default());

    let type_node = visitor.visit_node(import_type_node);
    assert!(type_node.is_none() || ast::is_type_node(type_node.unwrap()), "expected a type node");
    let symbols = symbols.borrow().clone();
    (type_node, symbols)
}

// import_adder.go:465
// If a type checker and multiple files are available, consider using `forEachNameOfDefaultExport`
// instead, which searches for names of re-exported defaults/namespaces in target files.
fn get_name_for_exported_symbol(symbol: P<Symbol>, prefer_capitalized: bool) -> String {
    if symbol.name() == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
        || symbol.name() == ast::INTERNAL_SYMBOL_NAME_DEFAULT
    {
        // Names for default exports:
        // - export default foo => foo
        // - export { foo as default } => foo
        // - export default 0 => filename converted to camelCase
        let name = get_default_like_export_name_from_declaration(symbol);
        if !name.is_empty() {
            return name;
        }
        assert!(
            symbol.parent().is_some(),
            "Expected exported symbol to have module symbol as parent"
        );
        return lsutil::module_symbol_to_valid_identifier(
            symbol.parent().unwrap(),
            prefer_capitalized,
        );
    }
    symbol.name().to_string()
}

// import_adder.go:481
fn replace_first_identifier_of_entity_name(
    factory: &NodeFactory,
    name: P<Node>,
    new_identifier: P<Node>,
) -> P<Node> {
    if name.kind() == ast::Kind::Identifier {
        return new_identifier;
    }
    factory.new_qualified_name(
        replace_first_identifier_of_entity_name(
            factory,
            name.as_qualified_name().left,
            new_identifier,
        ),
        name.as_qualified_name().right,
    )
}
