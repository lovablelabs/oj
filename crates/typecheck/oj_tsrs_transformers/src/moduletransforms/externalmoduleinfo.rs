use super::*;

#[derive(Default)]
pub struct ExternalModuleInfo {
    external_imports: Vec<P<Node>>, // ImportDeclaration | ImportEqualsDeclaration | ExportDeclaration. imports and reexports of other external modules
    pub(crate) export_specifiers: MultiMap<&'static str, P<Node>>, // Maps local names to their associated export specifiers (excludes reexports)
    pub(crate) exported_bindings: MultiMap<P<Node>, P<Node>>, // Maps local declarations to their associated export aliases
    pub(crate) exported_names: Vec<P<Node>>, // all exported names in the module, both local and re-exported, excluding the names of locally exported function declarations
    pub(crate) exported_functions: OrderedSet<P<Node>>, // all of the top-level exported function declarations
    pub(crate) export_equals: Option<P<Node>>, // an export=/module.exports= declaration if one was present
    has_export_stars_to_export_values: bool,   // whether this module contains export*
}

struct ExternalModuleInfoCollector {
    source_file: P<SourceFile>,
    emit_context: P<EmitContext>,
    resolver: ReferenceResolverRef,
    unique_exports: Set<&'static str>,
    has_export_default: bool,
    output: ExternalModuleInfo,
}

// externalmoduleinfo.go:35
pub fn collect_external_module_info(
    source_file: P<SourceFile>,
    _compiler_options: P<CompilerOptions>,
    emit_context: P<EmitContext>,
    resolver: ReferenceResolverRef,
) -> ExternalModuleInfo {
    let c = ExternalModuleInfoCollector {
        source_file,
        emit_context,
        resolver,
        unique_exports: Set::default(),
        has_export_default: false,
        output: ExternalModuleInfo::default(),
    };
    c.collect()
}

impl ExternalModuleInfoCollector {
    // externalmoduleinfo.go:46
    fn collect(mut self) -> ExternalModuleInfo {
        let mut has_import_star = false;
        let mut has_import_default = false;
        for &node in self.source_file.statements.nodes() {
            // Look through NotEmittedStatement to find elided export= declarations
            // (e.g., `declare export = x` is elided by the type eraser but must still be collected)
            if ast::is_not_emitted_statement(node) {
                let original = self.emit_context.most_original(Some(node));
                if let Some(original) = original
                    && ast::is_export_assignment(original)
                {
                    let n = original.as_export_assignment();
                    if n.is_export_equals && self.output.export_equals.is_none() {
                        self.output.export_equals = Some(original);
                    }
                }
                continue;
            }
            match node.kind() {
                Kind::ImportDeclaration => {
                    // import "mod"
                    // import x from "mod"
                    // import * as x from "mod"
                    // import { x, y } from "mod"
                    self.add_external_import(node);
                    if !has_import_star && get_import_needs_import_star_helper(node) {
                        has_import_star = true;
                    }
                    if !has_import_default && get_import_needs_import_default_helper(node) {
                        has_import_default = true;
                    }
                }

                Kind::ImportEqualsDeclaration => {
                    let n = node.as_import_equals_declaration();
                    if ast::is_external_module_reference(n.module_reference) {
                        // import x = require("mod")
                        self.add_external_import(node);
                    }
                }

                Kind::ExportDeclaration => {
                    let n = node.as_export_declaration();
                    if n.module_specifier.is_some() {
                        // export * from "mod"
                        // export * as ns from "mod"
                        // export { x, y } from "mod"
                        self.add_external_import(node);
                        match n.export_clause {
                            None => {
                                // export * from "mod"
                                self.output.has_export_stars_to_export_values = true;
                            }
                            Some(export_clause) if ast::is_named_exports(export_clause) => {
                                // export { x, y } from "mod"
                                self.add_exported_names_for_export_declaration(node);
                                if !has_import_default {
                                    has_import_default =
                                        contains_default_reference(Some(export_clause));
                                }
                            }
                            Some(export_clause) => {
                                // export * as ns from "mod"
                                let name = export_clause.as_namespace_export().name();
                                let name_text = name.text();
                                if self.add_unique_export(name_text) {
                                    self.add_exported_binding(node, name);
                                    self.add_exported_name(name);
                                }
                                // we use the same helpers for `export * as ns` as we do for `import * as ns`
                                has_import_star = true;
                            }
                        }
                    } else {
                        // export { x, y }
                        self.add_exported_names_for_export_declaration(node);
                    }
                }

                Kind::ExportAssignment => {
                    let n = node.as_export_assignment();
                    if n.is_export_equals && self.output.export_equals.is_none() {
                        // export = x
                        self.output.export_equals = Some(node);
                    }
                }

                Kind::VariableStatement => {
                    let n = node.as_variable_statement();
                    if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
                        for &decl in
                            n.declaration_list.as_variable_declaration_list().declarations.nodes()
                        {
                            self.collect_exported_variable_info(decl);
                        }
                    }
                }

                Kind::FunctionDeclaration => {
                    if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
                        self.add_exported_function_declaration(
                            node,
                            None, /*name*/
                            ast::has_syntactic_modifier(node, ModifierFlags::Default),
                        );
                    }
                }

                Kind::ClassDeclaration
                    if ast::has_syntactic_modifier(node, ModifierFlags::Export) =>
                {
                    if ast::has_syntactic_modifier(node, ModifierFlags::Default) {
                        // export default class { }
                        if !self.has_export_default {
                            let name = node.name().unwrap_or_else(|| {
                                self.emit_context.factory.new_generated_name_for_node(node)
                            });
                            self.add_exported_binding(node, name);
                            self.has_export_default = true;
                        }
                    } else {
                        // export class x { }
                        if let Some(name) = node.name()
                            && self.add_unique_export(name.text())
                        {
                            self.add_exported_binding(node, name);
                            self.add_exported_name(name);
                        }
                    }
                }
                _ => {}
            }
        }

        self.output
    }

    // externalmoduleinfo.go:162
    fn add_unique_export(&mut self, name: &'static str) -> bool {
        if !self.unique_exports.has(&name) {
            self.unique_exports.add(name);
            return true;
        }
        false
    }

    // externalmoduleinfo.go:170
    fn add_exported_binding(&mut self, decl: P<Node>, name: P<Node>) {
        self.output
            .exported_bindings
            .add(self.emit_context.most_original(Some(decl)).unwrap(), name);
    }

    // externalmoduleinfo.go:174
    fn add_external_import(
        &mut self,
        node: P<Node>, /*ImportDeclaration | ImportEqualsDeclaration | ExportDeclaration*/
    ) {
        self.output.external_imports.push(node);
    }

    // externalmoduleinfo.go:178
    fn add_exported_name(&mut self, name: P<Node>) {
        self.output.exported_names.push(name);
    }

    // externalmoduleinfo.go:182
    fn add_exported_names_for_export_declaration(&mut self, node: P<Node>) {
        let n = node.as_export_declaration();
        for &specifier in n.export_clause.unwrap().elements() {
            let specifier_name_text = specifier.name().unwrap().text();
            if self.add_unique_export(specifier_name_text) {
                let name = specifier.property_name_or_name().unwrap();
                if name.kind() != Kind::StringLiteral {
                    if n.module_specifier.is_none() {
                        self.output.export_specifiers.add(name.text(), specifier);
                    }

                    let mut decl = self.resolver.get_referenced_import_declaration(
                        self.emit_context.most_original(Some(name)).unwrap(),
                    );
                    if decl.is_none() {
                        decl = self.resolver.get_referenced_value_declaration(
                            self.emit_context.most_original(Some(name)).unwrap(),
                        );
                    }
                    if let Some(decl) = decl {
                        if decl.kind() == Kind::FunctionDeclaration {
                            self.unique_exports.delete(&specifier_name_text);
                            self.add_exported_function_declaration(
                                decl,
                                specifier.name(),
                                ast::module_export_name_is_default(specifier.name().unwrap()),
                            );
                            continue;
                        }
                        self.add_exported_binding(decl, specifier.name().unwrap());
                    }
                }

                self.add_exported_name(specifier.name().unwrap());
            }
        }
    }

    // externalmoduleinfo.go:211
    fn add_exported_function_declaration(
        &mut self,
        node: P<Node>,
        name: Option<P<Node>>,
        is_default: bool,
    ) {
        self.output.exported_functions.insert(self.emit_context.most_original(Some(node)).unwrap());
        if is_default {
            // export default function() { }
            // function x() { } + export { x as default };
            if !self.has_export_default {
                let name = name
                    .unwrap_or_else(|| self.emit_context.factory.new_generated_name_for_node(node));
                self.add_exported_binding(node, name);
                self.has_export_default = true;
            }
        } else {
            // export function x() { }
            // function x() { } + export { x }
            let name = name.unwrap_or_else(|| node.name().unwrap());
            let name_text = name.text();
            if self.add_unique_export(name_text) {
                self.add_exported_binding(node, name);
            }
        }
    }

    // externalmoduleinfo.go:235
    fn collect_exported_variable_info(
        &mut self,
        decl: P<Node>, /*VariableDeclaration | BindingElement*/
    ) {
        let name = decl.name().unwrap();
        if ast::is_binding_pattern(name) {
            for &element in name.elements() {
                if element.name().is_some() {
                    self.collect_exported_variable_info(element);
                }
            }
        } else if !self.emit_context.has_auto_generate_info(Some(name)) {
            let text = name.text();
            if self.add_unique_export(text) {
                self.add_exported_name(name);
                if is_local_name(self.emit_context, name) {
                    self.add_exported_binding(decl, name);
                }
            }
        }
    }
}

const EXTERNAL_HELPERS_MODULE_NAME_TEXT: &str = "tslib";

// externalmoduleinfo.go:257
pub fn create_external_helpers_import_declaration_if_needed(
    emit_context: P<EmitContext>,
    source_file: P<SourceFile>,
    compiler_options: &CompilerOptions,
    file_module_kind: ModuleKind,
    has_export_stars_to_export_values: bool,
    has_import_star: bool,
    has_import_default: bool,
) -> Option<P<Node>> /*ImportDeclaration | ImportEqualsDeclaration*/ {
    if compiler_options.import_helpers.is_true()
        && ast::is_effective_external_module(source_file, compiler_options)
    {
        let f = &emit_context.get().factory;
        let module_kind = compiler_options.get_emit_module_kind();
        let helpers = get_imported_helpers(emit_context, source_file);
        if file_module_kind == ModuleKind::COMMON_JS
            || file_module_kind == ModuleKind::NONE && module_kind == ModuleKind::COMMON_JS
        {
            // When we emit to a non-ES module, generate a synthetic `import tslib = require("tslib")` to be further transformed.
            let external_helpers_module_name = get_or_create_external_helpers_module_name_if_needed(
                emit_context,
                source_file,
                compiler_options,
                &helpers,
                has_export_stars_to_export_values,
                has_import_star || has_import_default,
                file_module_kind,
            );
            if let Some(external_helpers_module_name) = external_helpers_module_name {
                let external_helpers_import_declaration = f.new_import_equals_declaration(
                    None,  /*modifiers*/
                    false, /*isTypeOnly*/
                    external_helpers_module_name,
                    f.new_external_module_reference(
                        f.new_string_literal(EXTERNAL_HELPERS_MODULE_NAME_TEXT, TokenFlags::None),
                    ),
                );
                emit_context
                    .add_emit_flags(external_helpers_import_declaration, EmitFlags::CustomPrologue);
                return Some(external_helpers_import_declaration);
            }
        } else {
            // When we emit as an ES module, generate an `import` declaration that uses named imports for helpers.
            // If we cannot determine the implied module kind under `module: preserve` we assume ESM.
            let mut helper_names: Vec<&'static str> = Vec::new();
            for helper in &helpers {
                let import_name = helper.import_name;
                if !import_name.is_empty() && !helper_names.contains(&import_name) {
                    helper_names.push(import_name);
                }
            }
            if !helper_names.is_empty() {
                helper_names
                    .sort_by(|a, b| tsrs_core::stringutil::compare_strings_case_sensitive(a, b));
                // Alias the imports if the names are used somewhere in the file.
                // NOTE: We don't need to care about global import collisions as this is a module.

                let import_specifiers: Vec<P<Node>> = helper_names
                    .iter()
                    .map(|&name| {
                        if emit_context.is_file_level_unique_name(
                            source_file,
                            name,
                            None, /*hasGlobalName*/
                        ) {
                            f.new_import_specifier(
                                false, /*isTypeOnly*/
                                None,  /*propertyName*/
                                f.new_identifier(name),
                            )
                        } else {
                            f.new_import_specifier(
                                false, /*isTypeOnly*/
                                Some(f.new_identifier(name)),
                                f.new_unscoped_helper_name(name),
                            )
                        }
                    })
                    .collect();
                let named_bindings = f.new_named_imports(f.new_node_list(import_specifiers));
                let parse_node = emit_context.most_original(Some(source_file.as_node())).unwrap();
                emit_context.add_emit_flags(parse_node, EmitFlags::ExternalHelpers);

                let external_helpers_import_declaration = f.new_import_declaration(
                    None, /*modifiers*/
                    Some(f.new_import_clause(
                        Kind::Unknown, /*phaseModifier*/
                        None,          /*name*/
                        Some(named_bindings),
                    )),
                    f.new_string_literal(EXTERNAL_HELPERS_MODULE_NAME_TEXT, TokenFlags::None),
                    None, /*attributes*/
                );

                emit_context
                    .add_emit_flags(external_helpers_import_declaration, EmitFlags::CustomPrologue);
                return Some(external_helpers_import_declaration);
            }
        }
    }
    None
}

// externalmoduleinfo.go:318
fn get_imported_helpers(
    emit_context: P<EmitContext>,
    source_file: P<SourceFile>,
) -> Vec<SP<EmitHelper>> {
    let mut helpers = Vec::new();
    helpers.extend(
        emit_context
            .get_emit_helpers(source_file.as_node())
            .into_iter()
            .filter(|helper| !helper.scoped),
    );
    helpers
}

// externalmoduleinfo.go:328
fn get_or_create_external_helpers_module_name_if_needed(
    emit_context: P<EmitContext>,
    node: P<SourceFile>,
    _compiler_options: &CompilerOptions,
    helpers: &[SP<EmitHelper>],
    has_export_stars_to_export_values: bool,
    has_import_star_or_import_default: bool,
    file_module_kind: ModuleKind,
) -> Option<P<Node>> {
    let external_helpers_module_name = emit_context.get_external_helpers_module_name(node);
    if external_helpers_module_name.is_some() {
        return external_helpers_module_name;
    }

    let create = !helpers.is_empty()
        || (has_export_stars_to_export_values || has_import_star_or_import_default)
            && file_module_kind < ModuleKind::SYSTEM;

    if create {
        let external_helpers_module_name =
            emit_context.factory.new_unique_name(EXTERNAL_HELPERS_MODULE_NAME_TEXT);
        emit_context.set_external_helpers_module_name(node, external_helpers_module_name);
        return Some(external_helpers_module_name);
    }

    None
}

// externalmoduleinfo.go:349
fn is_named_default_reference(e: P<Node> /*ImportSpecifier | ExportSpecifier*/) -> bool {
    ast::module_export_name_is_default(e.property_name_or_name().unwrap())
}

// externalmoduleinfo.go:353
pub fn contains_default_reference(
    node: Option<P<Node>>, /*NamedImportBindings | NamedExportBindings*/
) -> bool {
    matches!(node, Some(node) if (ast::is_named_imports(node) || ast::is_named_exports(node)) && node.elements().iter().any(|&e| is_named_default_reference(e)))
}

// externalmoduleinfo.go:357
pub fn get_export_needs_import_star_helper(node: P<Node>) -> bool {
    ast::get_namespace_declaration_node(node).is_some()
}

// externalmoduleinfo.go:361
pub fn get_import_needs_import_star_helper(node: P<Node>) -> bool {
    if ast::get_namespace_declaration_node(node).is_some() {
        return true;
    }
    let Some(import_clause) = node.as_import_declaration().import_clause else {
        return false;
    };
    let Some(bindings) = import_clause.as_import_clause().named_bindings else {
        return false;
    };
    if !ast::is_named_imports(bindings) {
        return false;
    }
    let named_imports = bindings.as_named_imports();
    let mut default_ref_count = 0;
    default_ref_count += named_imports
        .elements
        .nodes()
        .iter()
        .filter(|&&binding| is_named_default_reference(binding))
        .count();
    let len = named_imports.elements.nodes().len();
    // Import star is required if there's default named refs mixed with non-default refs, or if theres non-default refs and it has a default import
    (default_ref_count > 0 && default_ref_count != len)
        || ((len - default_ref_count) != 0 && ast::is_default_import(node))
}

// externalmoduleinfo.go:385
pub fn get_import_needs_import_default_helper(node: P<Node>) -> bool {
    // Import default is needed if there's a default import or a default ref and no other refs (meaning an import star helper wasn't requested)
    let import_clause = node.as_import_declaration().import_clause;
    !get_import_needs_import_star_helper(node)
        && (ast::is_default_import(node)
            || import_clause.is_some_and(|import_clause| {
                contains_default_reference(import_clause.as_import_clause().named_bindings)
            }))
}
