use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, FileReference, Kind, ModifierFlags, Node, SourceFile, Symbol, SymbolFlags,
};
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::collections::Set;
use tsrs_core::context::Context;

use crate::utilities::{
    get_property_symbol_of_object_binding_pattern_without_property_name,
    is_source_file_with_global_exports,
};

// importTracker.go:15
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImpExpKind {
    Unknown,
    Import,
    Export,
}

// importTracker.go:23
pub struct ImportExportSymbol {
    pub(crate) kind: ImpExpKind,
    pub(crate) symbol: Option<P<Symbol>>,
    pub(crate) export_info: Option<ExportInfo>,
}

// importTracker.go:29 (Go never uses ExportKindUMD and ExportKindModule)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportKind {
    Named = 0,
    Default = 1,
    ExportEquals = 2,
}

// importTracker.go:39
#[derive(Clone, Copy)]
pub struct ExportInfo {
    pub(crate) exporting_module_symbol: P<Symbol>,
    pub(crate) export_kind: ExportKind,
}

// importTracker.go:44
#[derive(Clone, Copy)]
pub struct LocationAndSymbol {
    pub(crate) import_location: P<Node>,
    pub(crate) import_symbol: Option<P<Symbol>>,
}

// importTracker.go:49
pub struct ImportsResult {
    pub(crate) import_searches: Vec<LocationAndSymbol>,
    pub(crate) single_references: Vec<P<Node>>,
    pub(crate) indirect_users: Vec<P<SourceFile>>,
}

// Go `type ImportTracker func(exportSymbol, exportInfo, isForRename) *ImportsResult` (a closure over the direct
// imports map); the port keeps the captured values and calls it with the checker (PORTING.md callback rule).
// importTracker.go:55
pub struct ImportTracker<'a> {
    source_files: &'a [P<SourceFile>],
    source_files_set: &'a Set<String>,
    all_direct_imports: FxHashMap<P<Symbol>, Vec<P<Node>>>,
}

// importTracker.go:57
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleReferenceKind {
    Import,
    Reference,
    Implicit,
}

// ModuleReference represents a reference to a module, either via import, <reference>, or implicit reference
// importTracker.go:66
pub struct ModuleReference {
    pub(crate) kind: ModuleReferenceKind,
    pub(crate) literal: Option<P<Node>>, // for import and implicit kinds (StringLiteralLike)
    pub(crate) referencing_file: Option<P<SourceFile>>,
    pub(crate) ref_: Option<P<FileReference>>, // for reference kind
}

// Creates the imports map and returns an ImportTracker that uses it. Call this lazily to avoid calling `getDirectImportsMap` unnecessarily.
// importTracker.go:74
pub fn create_import_tracker<'a>(
    ctx: &Context,
    program: &'static Program,
    source_files: &'a [P<SourceFile>],
    source_files_set: &'a Set<String>,
    checker: &mut Checker,
) -> ImportTracker<'a> {
    let all_direct_imports = get_direct_imports_map(ctx, program, source_files, checker);
    ImportTracker { source_files, source_files_set, all_direct_imports }
}

impl ImportTracker<'_> {
    // The closure returned by createImportTracker (importTracker.go:76).
    pub(crate) fn call(
        &self,
        checker: &mut Checker,
        export_symbol: P<Symbol>,
        export_info: ExportInfo,
        is_for_rename: bool,
    ) -> ImportsResult {
        let (direct_imports, indirect_users) = get_importers_for_export(
            self.source_files,
            self.source_files_set,
            &self.all_direct_imports,
            export_info,
            checker,
        );
        let (import_searches, single_references) = get_searches_from_direct_imports(
            &direct_imports,
            export_symbol,
            export_info.export_kind,
            checker,
            is_for_rename,
        );
        ImportsResult { import_searches, single_references, indirect_users }
    }
}

// Returns a map from a module symbol to all import statements that directly reference the module
// importTracker.go:84
pub fn get_direct_imports_map(
    ctx: &Context,
    program: &'static Program,
    source_files: &[P<SourceFile>],
    checker: &mut Checker,
) -> FxHashMap<P<Symbol>, Vec<P<Node>>> {
    let mut result: FxHashMap<P<Symbol>, Vec<P<Node>>> = FxHashMap::default();
    for &source_file in source_files {
        if ctx.err().is_some() {
            return result;
        }
        for_each_import(program, source_file, &mut |import_decl, module_specifier| {
            if let Some(module_symbol) = checker.get_symbol_at_location_exported(module_specifier) {
                result.entry(module_symbol).or_default().push(import_decl);
            }
        });
    }
    result
}

// Calls `action` for each import, re-export, or require() in a file
// importTracker.go:100
pub fn for_each_import(
    program: &'static Program,
    source_file: P<SourceFile>,
    action: &mut dyn FnMut(P<Node> /*importStatement*/, P<Node> /*imported*/),
) {
    let mut implicit_imports: Vec<P<Node>> = Vec::new();
    let (_, jsx_specifier) = program.get_jsx_runtime_import_specifier(source_file.path());
    if let Some(jsx_specifier) = jsx_specifier {
        implicit_imports.push(jsx_specifier);
    }
    let import_helpers_specifier = program.get_import_helpers_import_specifier(source_file.path());
    if let Some(import_helpers_specifier) = import_helpers_specifier {
        implicit_imports.push(import_helpers_specifier);
    }
    if source_file.external_module_indicator.get().is_some()
        || source_file.imports().len() + implicit_imports.len() != 0
    {
        for &i in source_file.imports() {
            action(ast::import_from_module_specifier(i), i);
        }
        for &i in &implicit_imports {
            action(ast::import_from_module_specifier(i), i);
        }
    } else {
        for_each_possible_import_or_export_statement(source_file.as_node(), &mut |node| {
            match node.kind() {
                Kind::ExportDeclaration | Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                    if let Some(specifier) = node.module_specifier()
                        && ast::is_string_literal(specifier)
                    {
                        action(node, specifier);
                    }
                }
                Kind::ImportEqualsDeclaration if is_external_module_import_equals(node) => {
                    action(
                        node,
                        node.as_import_equals_declaration().module_reference.expression().unwrap(),
                    );
                }
                _ => {}
            }
            false
        });
    }
}

// importTracker.go:134
pub fn for_each_possible_import_or_export_statement(
    source_file_like: P<Node>,
    action: &mut dyn FnMut(P<Node>) -> bool,
) -> bool {
    get_statements_of_source_file_like(source_file_like).iter().any(|&statement| {
        action(statement)
            || is_ambient_module_declaration(statement)
                && for_each_possible_import_or_export_statement(statement, action)
    })
}

// importTracker.go:143
pub fn get_source_file_like_for_import_declaration(node: P<Node>) -> P<Node> {
    if ast::is_call_expression(node) || ast::is_jsdoc_import_tag(node) {
        return ast::get_source_file_of_node(node).unwrap().as_node();
    }
    let parent = node.parent().unwrap();
    if ast::is_source_file(parent) {
        return parent;
    }
    assert!(
        ast::is_module_block(parent) && is_ambient_module_declaration(parent.parent().unwrap())
    );
    parent.parent().unwrap()
}

// importTracker.go:155
pub fn is_ambient_module_declaration(node: P<Node>) -> bool {
    ast::is_module_declaration(node) && ast::is_string_literal(node.name().unwrap())
}

// importTracker.go:159
pub fn get_statements_of_source_file_like(node: P<Node>) -> &'static [P<Node>] {
    if ast::is_source_file(node) {
        return node.statements();
    }
    if let Some(body) = node.body() {
        return body.statements();
    }
    &[]
}

// The closures of getImportersForExport share the result lists and trackers; they are methods of this struct.
struct ImportersForExport<'a> {
    source_files: &'a [P<SourceFile>],
    source_files_set: &'a Set<String>,
    all_direct_imports: &'a FxHashMap<P<Symbol>, Vec<P<Node>>>,
    export_info: &'a ExportInfo,
    direct_imports: Vec<P<Node>>,
    indirect_user_declarations: Vec<P<Node>>,
    mark_seen_direct_import: FxHashSet<P<Node>>,
    mark_seen_indirect_user: FxHashSet<P<Node>>,
    is_available_through_global: bool,
}

impl ImportersForExport<'_> {
    fn get_direct_imports(&self, module_symbol: P<Symbol>) -> Vec<P<Node>> {
        self.all_direct_imports.get(&module_symbol).cloned().unwrap_or_default()
    }

    // Adds a module and all of its transitive dependencies as possible indirect users
    fn add_indirect_user(
        &mut self,
        checker: &mut Checker,
        source_file_like: P<Node>,
        add_transitive_dependencies: bool,
    ) {
        // When isAvailableThroughGlobal, getIndirectUsers already returns all source files,
        // so indirectUserDeclarations is never consulted. Nothing to do here.
        if self.is_available_through_global {
            return;
        }
        if !self.mark_seen_indirect_user.insert(source_file_like) {
            return;
        }
        self.indirect_user_declarations.push(source_file_like);
        if !add_transitive_dependencies {
            return;
        }
        let Some(module_symbol) =
            source_file_like.symbol().map(|s| checker.get_merged_symbol_exported(s))
        else {
            return;
        };
        assert!(module_symbol.flags().intersects(SymbolFlags::Module));
        for direct_import in self.get_direct_imports(module_symbol) {
            if !ast::is_import_type_node(direct_import) {
                self.add_indirect_user(
                    checker,
                    get_source_file_like_for_import_declaration(direct_import),
                    true, /*addTransitiveDependencies*/
                );
            }
        }
    }

    fn is_exported(node: P<Node>, stop_at_ambient_module: bool) -> bool {
        let mut node = Some(node);
        while let Some(n) = node {
            if stop_at_ambient_module && is_ambient_module_declaration(n) {
                break;
            }
            if ast::has_syntactic_modifier(n, ModifierFlags::Export) {
                return true;
            }
            node = n.parent();
        }
        false
    }

    fn handle_import_call(&mut self, checker: &mut Checker, import_call: P<Node>) {
        let top = ast::find_ancestor(Some(import_call), is_ambient_module_declaration)
            .unwrap_or_else(|| ast::get_source_file_of_node(import_call).unwrap().as_node());
        let exported = Self::is_exported(import_call, true /*stopAtAmbientModule*/);
        self.add_indirect_user(checker, top, exported);
    }

    fn handle_namespace_import(
        &mut self,
        checker: &mut Checker,
        import_declaration: P<Node>,
        name: P<Node>,
        is_re_export: bool,
        already_added_direct: bool,
    ) {
        if self.export_info.export_kind == ExportKind::ExportEquals {
            // This is a direct import, not import-as-namespace.
            if !already_added_direct {
                self.direct_imports.push(import_declaration);
            }
        } else if !self.is_available_through_global {
            let source_file_like = get_source_file_like_for_import_declaration(import_declaration);
            assert!(
                ast::is_source_file(source_file_like)
                    || ast::is_module_declaration(source_file_like)
            );
            let add_transitive_dependencies =
                is_re_export || find_namespace_re_exports(source_file_like, name, checker);
            self.add_indirect_user(checker, source_file_like, add_transitive_dependencies);
        }
    }

    fn handle_direct_imports(&mut self, checker: &mut Checker, exporting_module_symbol: P<Symbol>) {
        let these_direct_imports = self.get_direct_imports(exporting_module_symbol);
        for direct in these_direct_imports {
            if !self.mark_seen_direct_import.insert(direct) {
                continue;
            }
            // !!! cancellation
            match direct.kind() {
                Kind::CallExpression => {
                    if ast::is_import_call(direct) {
                        self.handle_import_call(checker, direct);
                    } else if !self.is_available_through_global {
                        let parent = direct.parent().unwrap();
                        if self.export_info.export_kind == ExportKind::ExportEquals
                            && ast::is_variable_declaration(parent)
                        {
                            let name = parent.name().unwrap();
                            if ast::is_identifier(name) {
                                self.direct_imports.push(name);
                            }
                        }
                    }
                }
                Kind::Identifier => {
                    // Nothing
                }
                Kind::ImportEqualsDeclaration => {
                    self.handle_namespace_import(
                        checker,
                        direct,
                        direct.name().unwrap(),
                        ast::has_syntactic_modifier(direct, ModifierFlags::Export),
                        false, /*alreadyAddedDirect*/
                    );
                }
                Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::JSDocImportTag => {
                    self.direct_imports.push(direct);
                    if let Some(import_clause) = direct.import_clause()
                        && let Some(named_bindings) =
                            import_clause.as_import_clause().named_bindings
                        && ast::is_namespace_import(named_bindings)
                    {
                        self.handle_namespace_import(
                            checker,
                            direct,
                            named_bindings.name().unwrap(),
                            false, /*isReExport*/
                            true,  /*alreadyAddedDirect*/
                        );
                        continue;
                    }
                    if !self.is_available_through_global && ast::is_default_import(direct) {
                        self.add_indirect_user(
                            checker,
                            get_source_file_like_for_import_declaration(direct),
                            false,
                        );
                        // Add a check for indirect uses to handle synthetic default imports
                    }
                }
                Kind::ExportDeclaration => {
                    let export_clause = direct.as_export_declaration().export_clause;
                    match export_clause {
                        None => {
                            // This is `export * from "foo"`, so imports of this module may import the export too.
                            let containing = get_containing_module_symbol(direct, checker);
                            self.handle_direct_imports(checker, containing);
                        }
                        Some(export_clause) if ast::is_namespace_export(export_clause) => {
                            // `export * as foo from "foo"` add to indirect uses
                            self.add_indirect_user(
                                checker,
                                get_source_file_like_for_import_declaration(direct),
                                true, /*addTransitiveDependencies*/
                            );
                        }
                        Some(_) => {
                            // This is `export { foo } from "foo"` and creates an alias symbol, so recursive search will get handle re-exports.
                            self.direct_imports.push(direct);
                        }
                    }
                }
                Kind::ImportType => {
                    // Only check for typeof import('xyz')
                    let import_type = direct.as_import_type_node();
                    if !self.is_available_through_global
                        && import_type.is_type_of
                        && import_type.qualifier.is_none()
                        && Self::is_exported(direct, false)
                    {
                        self.add_indirect_user(
                            checker,
                            ast::get_source_file_of_node(direct).unwrap().as_node(),
                            true, /*addTransitiveDependencies*/
                        );
                    }
                    self.direct_imports.push(direct);
                }
                _ => panic!("Unexpected import kind. Unexpected node kind: {:?}", direct.kind()),
            }
        }
    }

    fn get_indirect_users(&mut self, checker: &mut Checker) -> Vec<P<SourceFile>> {
        if self.is_available_through_global {
            // It has `export as namespace`, so anything could potentially use it.
            return self.source_files.to_vec();
        }
        // Module augmentations may use this module's exports without importing it.
        for &decl in self.export_info.exporting_module_symbol.declarations() {
            if ast::is_external_module_augmentation(decl)
                && self
                    .source_files_set
                    .has(&ast::get_source_file_of_node(decl).unwrap().file_name().to_string())
            {
                self.add_indirect_user(checker, decl, false);
            }
        }
        // This may return duplicates (if there are multiple module declarations in a single source file, all importing the same thing as a namespace), but `State.markSearchedSymbol` will handle that.
        self.indirect_user_declarations
            .iter()
            .map(|&d| ast::get_source_file_of_node(d).unwrap())
            .collect()
    }
}

// importTracker.go:169
pub fn get_importers_for_export(
    source_files: &[P<SourceFile>],
    source_files_set: &Set<String>,
    all_direct_imports: &FxHashMap<P<Symbol>, Vec<P<Node>>>,
    export_info: ExportInfo,
    checker: &mut Checker,
) -> (Vec<P<Node>>, Vec<P<SourceFile>>) {
    let mut state = ImportersForExport {
        source_files,
        source_files_set,
        all_direct_imports,
        export_info: &export_info,
        direct_imports: Vec::new(),
        indirect_user_declarations: Vec::new(),
        mark_seen_direct_import: FxHashSet::default(),
        mark_seen_indirect_user: FxHashSet::default(),
        is_available_through_global: is_source_file_with_global_exports(
            export_info.exporting_module_symbol.value_declaration(),
        ),
    };

    state.handle_direct_imports(checker, export_info.exporting_module_symbol);
    let indirect_users = state.get_indirect_users(checker);
    (state.direct_imports, indirect_users)
}

// importTracker.go:324
pub fn get_containing_module_symbol(importer: P<Node>, checker: &mut Checker) -> P<Symbol> {
    checker.get_merged_symbol_exported(
        get_source_file_like_for_import_declaration(importer).symbol().unwrap(),
    )
}

// Returns 'true' is the namespace 'name' is re-exported from this module, and 'false' if it is only used locally
// importTracker.go:329
pub fn find_namespace_re_exports(
    source_file_like: P<Node>,
    name: P<Node>,
    checker: &mut Checker,
) -> bool {
    let namespace_import_symbol = checker.get_symbol_at_location_exported(name);
    for_each_possible_import_or_export_statement(source_file_like, &mut |statement| {
        if !ast::is_export_declaration(statement) {
            return false;
        }
        let export_clause = statement.as_export_declaration().export_clause;
        let module_specifier = statement.module_specifier();
        module_specifier.is_none()
            && export_clause.is_some_and(|export_clause| {
                ast::is_named_exports(export_clause)
                    && export_clause.elements().iter().any(|&element| {
                        checker.get_export_specifier_local_target_symbol(element)
                            == namespace_import_symbol
                    })
            })
    })
}

// The closures of getSearchesFromDirectImports share the result lists; they are methods of this struct.
struct SearchesFromDirectImports {
    import_searches: Vec<LocationAndSymbol>,
    single_references: Vec<P<Node>>,
    export_symbol: P<Symbol>,
    export_kind: ExportKind,
    is_for_rename: bool,
}

impl SearchesFromDirectImports {
    fn add_search(&mut self, location: P<Node>, symbol: Option<P<Symbol>>) {
        self.import_searches
            .push(LocationAndSymbol { import_location: location, import_symbol: symbol });
    }

    fn is_name_match(&self, name: &str) -> bool {
        // Use name of "default" even in `export =` case because we may have allowSyntheticDefaultImports
        name == self.export_symbol.name()
            || self.export_kind != ExportKind::Named && name == ast::INTERNAL_SYMBOL_NAME_DEFAULT
    }

    // `import x = require("./x")` or `import * as x from "./x"`.
    // An `export =` may be imported by this syntax, so it may be a direct import.
    // If it's not a direct import, it will be in `indirectUsers`, so we don't have to do anything here.
    fn handle_namespace_import_like(&mut self, checker: &mut Checker, import_name: P<Node>) {
        // Don't rename an import that already has a different name than the export.
        if self.export_kind == ExportKind::ExportEquals
            && (!self.is_for_rename || self.is_name_match(import_name.text()))
        {
            let symbol = checker.get_symbol_at_location_exported(import_name);
            self.add_search(import_name, symbol);
        }
    }

    fn search_for_named_import(&mut self, checker: &mut Checker, named_bindings: Option<P<Node>>) {
        let Some(named_bindings) = named_bindings else {
            return;
        };
        for &element in named_bindings.elements() {
            let name = element.name().unwrap();
            let property_name = element.property_name();
            if !self.is_name_match(property_name.unwrap_or(name).text()) {
                continue;
            }
            if let Some(property_name) = property_name {
                // This is `import { foo as bar } from "./a"` or `export { foo as bar } from "./a"`. `foo` isn't a local in the file, so just add it as a single reference.
                self.single_references.push(property_name);
                // If renaming `{ foo as bar }`, don't touch `bar`, just `foo`.
                // But do rename `foo` in ` { default as foo }` if that's the original export name.
                if !self.is_for_rename || name.text() == self.export_symbol.name() {
                    // Search locally for `bar`.
                    let symbol = checker.get_symbol_at_location_exported(name);
                    self.add_search(name, symbol);
                }
            } else {
                let local_symbol =
                    if ast::is_export_specifier(element) && element.property_name().is_some() {
                        checker.get_export_specifier_local_target_symbol(element)
                    } else {
                        checker.get_symbol_at_location_exported(name)
                    };
                self.add_search(name, local_symbol);
            }
        }
    }

    fn handle_import(&mut self, checker: &mut Checker, decl: P<Node>) {
        if ast::is_import_equals_declaration(decl) {
            if is_external_module_import_equals(decl) {
                self.handle_namespace_import_like(checker, decl.name().unwrap());
            }
            return;
        }
        if ast::is_identifier(decl) {
            self.handle_namespace_import_like(checker, decl);
            return;
        }
        if ast::is_import_type_node(decl) {
            if let Some(qualifier) = decl.as_import_type_node().qualifier {
                let first_identifier = ast::get_first_identifier(qualifier);
                if first_identifier.text() == ast::symbol_name(self.export_symbol) {
                    self.single_references.push(first_identifier);
                }
            } else if self.export_kind == ExportKind::ExportEquals {
                self.single_references
                    .push(decl.as_import_type_node().argument.as_literal_type_node().literal);
            }
            return;
        }
        // Ignore if there's a grammar error
        if !decl.module_specifier().is_some_and(ast::is_string_literal) {
            return;
        }
        if ast::is_export_declaration(decl) {
            if let Some(export_clause) = decl.as_export_declaration().export_clause
                && ast::is_named_exports(export_clause)
            {
                self.search_for_named_import(checker, Some(export_clause));
            }
            return;
        }
        if let Some(import_clause) = decl.import_clause() {
            if let Some(named_bindings) = import_clause.as_import_clause().named_bindings {
                match named_bindings.kind() {
                    Kind::NamespaceImport => self.handle_namespace_import_like(checker, named_bindings.name().unwrap()),
                    Kind::NamedImports
                        // 'default' might be accessed as a named import `{ default as foo }`.
                        if (self.export_kind == ExportKind::Named || self.export_kind == ExportKind::Default) => {
                            self.search_for_named_import(checker, Some(named_bindings));
                        }
                    _ => {}
                }
            }
            // `export =` might be imported by a default import if `--allowSyntheticDefaultImports` is on, so this handles both ExportKind.Default and ExportKind.ExportEquals.
            // If a default import has the same name as the default export, allow to rename it.
            // Given `import f` and `export default function f`, we will rename both, but for `import g` we will rename just that.
            if let Some(name) = import_clause.name()
                && (self.export_kind == ExportKind::Default
                    || self.export_kind == ExportKind::ExportEquals)
                && (!self.is_for_rename
                    || name.text() == symbol_name_no_default(self.export_symbol))
            {
                let default_import_alias = checker.get_symbol_at_location_exported(name);
                self.add_search(name, default_import_alias);
            }
        }
    }
}

// importTracker.go:343
pub fn get_searches_from_direct_imports(
    direct_imports: &[P<Node>],
    export_symbol: P<Symbol>,
    export_kind: ExportKind,
    checker: &mut Checker,
    is_for_rename: bool,
) -> (Vec<LocationAndSymbol>, Vec<P<Node>>) {
    let mut state = SearchesFromDirectImports {
        import_searches: Vec::new(),
        single_references: Vec::new(),
        export_symbol,
        export_kind,
        is_for_rename,
    };
    for &decl in direct_imports {
        state.handle_import(checker, decl);
    }
    (state.import_searches, state.single_references)
}

// importTracker.go:462
pub fn get_import_or_export_symbol(
    node: P<Node>,
    symbol: P<Symbol>,
    checker: &mut Checker,
    coming_from_export: bool,
) -> Option<ImportExportSymbol> {
    fn export_info(
        symbol: P<Symbol>,
        kind: ExportKind,
        checker: &mut Checker,
    ) -> Option<ImportExportSymbol> {
        if let Some(export_info) = get_export_info(symbol, kind, checker) {
            return Some(ImportExportSymbol {
                kind: ImpExpKind::Export,
                symbol: Some(symbol),
                export_info: Some(export_info),
            });
        }
        None
    }

    let get_export = |checker: &mut Checker| -> Option<ImportExportSymbol> {
        let get_export_assignment_export = |ex: P<Node>| -> Option<ImportExportSymbol> {
            // Get the symbol for the `export =` node; its parent is the module it's the export of.
            let ex_parent = ex.symbol().unwrap().parent()?;
            let export_kind = if ex.as_export_assignment().is_export_equals {
                ExportKind::ExportEquals
            } else {
                ExportKind::Default
            };
            Some(ImportExportSymbol {
                kind: ImpExpKind::Export,
                symbol: Some(symbol),
                export_info: Some(ExportInfo { exporting_module_symbol: ex_parent, export_kind }),
            })
        };

        // Not meant for use with export specifiers or export assignment.
        let get_export_kind_for_declaration = |node: P<Node>| -> ExportKind {
            if ast::has_syntactic_modifier(node, ModifierFlags::Default) {
                return ExportKind::Default;
            }
            ExportKind::Named
        };

        let get_special_property_export = |checker: &mut Checker,
                                           node: P<Node>,
                                           use_lhs_symbol: bool|
         -> Option<ImportExportSymbol> {
            let kind = match ast::get_assignment_declaration_kind(node) {
                ast::JSDeclarationKind::ExportsProperty => ExportKind::Named,
                ast::JSDeclarationKind::ModuleExports => ExportKind::ExportEquals,
                _ => return None,
            };
            let mut sym = Some(symbol);
            if use_lhs_symbol {
                sym = node.symbol();
            }
            let sym = sym?;
            export_info(sym, kind, checker)
        };

        let parent = node.parent().unwrap();
        let grandparent = parent.parent();
        if let Some(export_symbol) = symbol.export_symbol() {
            if ast::is_property_access_expression(parent) {
                // When accessing an export of a JS module, there's no alias. The symbol will still be flagged as an export even though we're at the use.
                // So check that we are at the declaration.
                if grandparent.is_some_and(ast::is_binary_expression)
                    && symbol.declarations().contains(&parent)
                {
                    return get_special_property_export(
                        checker,
                        grandparent.unwrap(),
                        false, /*useLhsSymbol*/
                    );
                }
                return None;
            }
            return export_info(export_symbol, get_export_kind_for_declaration(parent), checker);
        }
        let export_node = get_export_node(parent, node);
        if let Some(export_node) = export_node.filter(|&export_node| {
            ast::has_syntactic_modifier(export_node, ModifierFlags::Export)
                || ast::is_implicitly_exported_jsdoc_declaration(export_node)
        }) {
            if ast::is_import_equals_declaration(export_node)
                && export_node.as_import_equals_declaration().module_reference == node
            {
                // We're at `Y` in `export import X = Y`. This is not the exported symbol, the left-hand-side is. So treat this as an import statement.
                if coming_from_export {
                    return None;
                }
                let lhs_symbol =
                    checker.get_symbol_at_location_exported(export_node.name().unwrap());
                return Some(ImportExportSymbol {
                    kind: ImpExpKind::Import,
                    symbol: lhs_symbol,
                    export_info: None,
                });
            }
            return export_info(symbol, get_export_kind_for_declaration(export_node), checker);
        } else if ast::is_namespace_export(parent) {
            return export_info(symbol, ExportKind::Named, checker);
        } else if ast::is_export_assignment(parent) {
            return get_export_assignment_export(parent);
        } else if grandparent.is_some_and(ast::is_export_assignment) {
            return get_export_assignment_export(grandparent.unwrap());
        } else if ast::is_binary_expression(parent) {
            return get_special_property_export(checker, parent, true /*useLhsSymbol*/);
        } else if grandparent.is_some_and(ast::is_binary_expression) {
            return get_special_property_export(
                checker,
                grandparent.unwrap(),
                true, /*useLhsSymbol*/
            );
        } else if ast::is_jsdoc_typedef_tag(parent) || ast::is_jsdoc_callback_tag(parent) {
            return export_info(symbol, ExportKind::Named, checker);
        }
        None
    };

    let get_import = |checker: &mut Checker| -> Option<ImportExportSymbol> {
        if !is_node_import(node) {
            return None;
        }
        // JS destructuring from `require(...)` is import-like for references, but the binding element
        // itself is still a local variable symbol rather than an alias.
        let imported_symbol = if symbol.flags().intersects(SymbolFlags::Alias) {
            checker.get_immediate_aliased_symbol_exported(symbol)
        } else {
            get_property_symbol_of_object_binding_pattern_without_property_name(symbol, checker)
        };
        let imported_symbol = imported_symbol?;
        // Search on the local symbol in the exporting module, not the exported symbol.
        let mut imported_symbol = skip_export_specifier_symbol(imported_symbol, checker)?;
        // Similarly, skip past the symbol for 'export ='
        if imported_symbol.name() == "export=" {
            imported_symbol = get_export_equals_local_symbol(imported_symbol, checker)?;
        }
        // If the import has a different name than the export, do not continue searching.
        // If `importedName` is undefined, do continue searching as the export is anonymous.
        // (All imports returned from this function will be ignored anyway if we are in rename and this is a not a named export.)
        let imported_name = symbol_name_no_default(imported_symbol);
        if imported_name.is_empty()
            || imported_name == ast::INTERNAL_SYMBOL_NAME_DEFAULT
            || imported_name == symbol.name()
        {
            return Some(ImportExportSymbol {
                kind: ImpExpKind::Import,
                symbol: Some(imported_symbol),
                export_info: None,
            });
        }
        None
    };

    let mut result = get_export(checker);
    if result.is_none() && !coming_from_export {
        result = get_import(checker);
    }
    result
}

// importTracker.go:611
pub fn get_export_info(
    export_symbol: P<Symbol>,
    export_kind: ExportKind,
    c: &mut Checker,
) -> Option<ExportInfo> {
    // Parent can be nil if an `export` is not at the top-level (which is a compile error).
    if let Some(parent) = export_symbol.parent() {
        let exporting_module_symbol = c.get_merged_symbol_exported(parent);
        // `export` may appear in a namespace. In that case, just rely on global search.
        if tsrs_checker::is_external_module_symbol(exporting_module_symbol) {
            return Some(ExportInfo { exporting_module_symbol, export_kind });
        }
    }
    None
}

// If a reference is a class expression, the exported node would be its parent.
// If a reference is a variable declaration, the exported node would be the variable statement.
// importTracker.go:628
pub fn get_export_node(parent: P<Node>, node: P<Node>) -> Option<P<Node>> {
    let mut declaration: Option<P<Node>> = None;
    if ast::is_variable_declaration(parent) {
        declaration = Some(parent);
    } else if ast::is_binding_element(parent) {
        declaration = Some(ast::walk_up_binding_elements_and_patterns(parent));
    }
    if let Some(declaration) = declaration {
        let declaration_parent = declaration.parent().unwrap();
        if parent.name() == Some(node)
            && !ast::is_catch_clause(declaration_parent)
            && ast::is_variable_statement(declaration_parent.parent().unwrap())
        {
            return declaration_parent.parent();
        }
        return None;
    }
    Some(parent)
}

// importTracker.go:645
pub fn is_node_import(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::ImportEqualsDeclaration => {
            parent.name() == Some(node) && is_external_module_import_equals(parent)
        }
        Kind::ImportSpecifier => {
            // For a rename import `{ foo as bar }`, don't search for the imported symbol. Just find local uses of `bar`.
            parent.property_name().is_none()
        }
        Kind::ImportClause | Kind::NamespaceImport => {
            assert_eq!(parent.name(), Some(node));
            true
        }
        Kind::BindingElement => {
            ast::is_in_js_file(Some(node))
                && ast::is_variable_declaration_initialized_to_bare_or_accessed_require(
                    parent.parent().unwrap().parent().unwrap(),
                )
        }
        _ => false,
    }
}

// importTracker.go:662
pub fn is_external_module_import_equals(node: P<Node>) -> bool {
    let module_reference = node.as_import_equals_declaration().module_reference;
    ast::is_external_module_reference(module_reference)
        && module_reference.expression().unwrap().kind() == Kind::StringLiteral
}

// If at an export specifier, go to the symbol it refers to. */
// importTracker.go:668
pub fn skip_export_specifier_symbol(symbol: P<Symbol>, checker: &mut Checker) -> Option<P<Symbol>> {
    // For `export { foo } from './bar", there's nothing to skip, because it does not create a new alias. But `export { foo } does.
    if let Some(result) = symbol.declarations().iter().find_map(|&declaration| {
        if ast::is_export_specifier(declaration)
            && declaration.property_name().is_none()
            && declaration.parent().unwrap().parent().unwrap().module_specifier().is_none()
        {
            return Some(Some(
                checker.get_export_specifier_local_target_symbol(declaration).unwrap_or(symbol),
            ));
        } else if ast::is_property_access_expression(declaration)
            && ast::is_module_exports_access_expression(declaration.expression().unwrap())
            && !ast::is_private_identifier(declaration.name().unwrap())
        {
            // Export of form 'module.exports.propName = expr';
            return Some(checker.get_symbol_at_location_exported(declaration));
        } else if ast::is_shorthand_property_assignment(declaration)
            && ast::is_binary_expression(declaration.parent().unwrap().parent().unwrap())
            && ast::get_assignment_declaration_kind(declaration.parent().unwrap().parent().unwrap())
                == ast::JSDeclarationKind::ModuleExports
        {
            return Some(
                checker.get_export_specifier_local_target_symbol(declaration.name().unwrap()),
            );
        }

        None
    }) {
        return result;
    }
    Some(symbol)
}

// importTracker.go:684
pub fn get_export_equals_local_symbol(
    imported_symbol: P<Symbol>,
    checker: &mut Checker,
) -> Option<P<Symbol>> {
    if imported_symbol.flags().intersects(SymbolFlags::Alias) {
        return checker.get_immediate_aliased_symbol_exported(imported_symbol);
    }
    let decl = imported_symbol.value_declaration();
    assert!(decl.is_some());
    let decl = decl.unwrap();
    if ast::is_export_assignment(decl) {
        decl.expression().unwrap().symbol()
    } else if ast::is_binary_expression(decl) {
        decl.as_binary_expression().right().symbol()
    } else if ast::is_source_file(decl) {
        decl.symbol()
    } else {
        None
    }
}

// importTracker.go:701
pub fn symbol_name_no_default(symbol: P<Symbol>) -> &'static str {
    if symbol.name() != ast::INTERNAL_SYMBOL_NAME_DEFAULT {
        return symbol.name();
    }
    if let Some(result) = symbol.declarations().iter().find_map(|&decl| {
        let name = ast::get_name_of_declaration(Some(decl));
        if let Some(name) = name
            && ast::is_identifier(name)
        {
            return Some(name.text());
        }

        None
    }) {
        return result;
    }
    ""
}

// findModuleReferences finds all references to a module symbol across the given source files.
// This includes import statements, <reference> directives, and implicit references (e.g., JSX runtime imports).
// importTracker.go:716
pub fn find_module_references(
    program: &'static Program,
    source_files: &[P<SourceFile>],
    search_module_symbol: P<Symbol>,
    checker: &mut Checker,
) -> Vec<ModuleReference> {
    let mut refs: Vec<ModuleReference> = Vec::new();

    for &referencing_file in source_files {
        let search_source_file = search_module_symbol.value_declaration();
        if let Some(search_source_file) =
            search_source_file.filter(|d| d.kind() == Kind::SourceFile)
        {
            // Check <reference path> directives
            refs.extend(referencing_file.referenced_files().iter().filter_map(|&r| {
                if program.get_source_file_from_reference(referencing_file, r)
                    == Some(search_source_file.as_source_file_p())
                {
                    Some(ModuleReference {
                        kind: ModuleReferenceKind::Reference,
                        literal: None,
                        referencing_file: Some(referencing_file),
                        ref_: Some(r),
                    })
                } else {
                    None
                }
            }));

            // Check <reference types> directives
            refs.extend(referencing_file.type_reference_directives().iter().filter_map(|&r| {
                let referenced = program
                    .get_resolved_type_reference_directive_from_type_reference_directive(
                        r,
                        referencing_file,
                    );
                if let Some(referenced) = referenced {
                    if referenced.resolved_file_name
                        == search_source_file.as_source_file().file_name()
                    {
                        Some(ModuleReference {
                            kind: ModuleReferenceKind::Reference,
                            literal: None,
                            referencing_file: Some(referencing_file),
                            ref_: Some(r),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            }));
        }

        // Check all imports (including require() calls)
        for_each_import(program, referencing_file, &mut |import_decl, module_specifier| {
            let module_symbol = checker.get_symbol_at_location_exported(module_specifier);
            if module_symbol == Some(search_module_symbol) {
                if ast::node_is_synthesized(import_decl) {
                    refs.push(ModuleReference {
                        kind: ModuleReferenceKind::Implicit,
                        literal: Some(module_specifier),
                        referencing_file: Some(referencing_file),
                        ref_: None,
                    });
                } else {
                    refs.push(ModuleReference {
                        kind: ModuleReferenceKind::Import,
                        literal: Some(module_specifier),
                        referencing_file: None,
                        ref_: None,
                    });
                }
            }
        });
    }

    refs
}
