use super::*;

// utilities.go:12
pub fn is_declaration_name_of_enum_or_namespace(
    emit_context: P<EmitContext>,
    node: P<Node>,
) -> bool {
    if let Some(original) = emit_context.most_original(Some(node))
        && let Some(parent) = original.parent()
        && let Kind::EnumDeclaration | Kind::ModuleDeclaration = parent.kind()
    {
        return Some(original) == parent.name();
    }
    false
}

// utilities.go:22
pub fn rewrite_module_specifier(
    emit_context: P<EmitContext>,
    node: Option<P<Node>>,
    compiler_options: &CompilerOptions,
    rewrites: &[tspath::ExtensionRewrite],
    use_case_sensitive_file_names: bool,
) -> Option<P<Node>> {
    let node = node?;
    if !ast::is_string_literal(node)
        || !compiler_options.rewrite_relative_import_extensions.is_true()
    {
        return Some(node);
    }
    if !tspath::should_rewrite_extension(node.text(), rewrites, use_case_sensitive_file_names) {
        return Some(node);
    }
    let (rewritten, has_rewrite) =
        tspath::rewrite_extension(node.text(), rewrites, use_case_sensitive_file_names);
    let updated_text = if has_rewrite {
        rewritten
    } else {
        tspath::change_extension(
            node.text(),
            tsrs_tsoptions::outputpaths::get_output_extension(node.text(), compiler_options.jsx),
        )
    };
    if updated_text != node.text() {
        let updated = emit_context
            .factory
            .new_string_literal(alloc_str(&updated_text), node.as_string_literal().token_flags());
        emit_context.set_original(updated, node);
        emit_context.assign_comment_and_source_map_ranges(updated, node);
        return Some(updated);
    }
    Some(node)
}

// utilities.go:36
pub fn create_empty_imports(factory: &printer::NodeFactory) -> P<Node> {
    factory.new_export_declaration(
        None,  /*modifiers*/
        false, /*isTypeOnly*/
        Some(factory.new_named_exports(factory.new_node_list(Vec::new()))),
        None, /*moduleSpecifier*/
        None, /*attributes*/
    )
}

// Get the name of a target module from an import/export declaration as should be written in the emitted output.
// The emitted output name can be different from the input if:
//  1. The module has a /// <amd-module name="<new name>" />
//  2. --out or --outFile is used, making the name relative to the rootDir
//     3- The containing SourceFile has an entry in renamedDependencies for the import as requested by some module loaders (e.g. System).
//
// Otherwise, a new StringLiteral node representing the module name will be returned.
// utilities.go:54
pub fn get_external_module_name_literal(
    factory: &printer::NodeFactory,
    import_node: P<Node>, /*ImportDeclaration | ExportDeclaration | ImportEqualsDeclaration | ImportCall*/
    source_file: Option<P<SourceFile>>,
    host: Option<()>, /*EmitHost*/
    resolver: Option<Resolver>,
    compiler_options: &CompilerOptions,
) -> Option<P<Node>> {
    let module_name = ast::get_external_module_name(import_node);
    if let Some(module_name) = module_name
        && ast::is_string_literal(module_name)
    {
        let mut name = try_get_module_name_from_declaration(
            import_node,
            host,
            factory,
            resolver,
            compiler_options,
        );
        if name.is_none() {
            name = try_rename_external_module(factory, module_name, source_file);
        }
        if name.is_none() {
            // !!! propagate token flags (will produce new diffs)
            name = Some(factory.new_string_literal(module_name.text(), TokenFlags::None));
        }
        return name;
    }
    None
}

// Get the name of a module as should be written in the emitted output.
// The emitted output name can be different from the input if:
//  1. The module has a /// <amd-module name="<new name>" />
//  2. --out or --outFile is used, making the name relative to the rootDir
//
// Otherwise, a new StringLiteral node representing the module name will be returned.
// utilities.go:76
pub fn try_get_module_name_from_file(
    _factory: &printer::NodeFactory,
    file: Option<P<SourceFile>>,
    _host: Option<()>,
    /*EmitHost*/ _options: &CompilerOptions,
) -> Option<P<Node>> {
    file?;
    // !!!
    // if file.moduleName {
    // 	return factory.createStringLiteral(file.moduleName)
    // }
    None
}

// utilities.go:87
pub fn try_get_module_name_from_declaration(
    declaration: P<Node>, /*ImportEqualsDeclaration | ImportDeclaration | ExportDeclaration | ImportCall*/
    host: Option<()>,     /*EmitHost*/
    factory: &printer::NodeFactory,
    resolver: Option<Resolver>,
    compiler_options: &CompilerOptions,
) -> Option<P<Node>> {
    let resolver = resolver?;
    try_get_module_name_from_file(
        factory,
        resolver.get_external_module_file_from_declaration(declaration),
        host,
        compiler_options,
    )
}

// Some bundlers (SystemJS builder) sometimes want to rename dependencies.
// Here we check if alternative name was provided for a given moduleName and return it if possible.
// utilities.go:102
pub fn try_rename_external_module(
    _factory: &printer::NodeFactory,
    _module_name: P<Node>,
    _source_file: Option<P<SourceFile>>,
) -> Option<P<Node>> {
    // !!!
    None
}

// utilities.go:107
pub fn is_file_level_reserved_generated_identifier(
    emit_context: P<EmitContext>,
    name: P<Node>,
) -> bool {
    let info = emit_context.get_auto_generate_info(Some(name));
    matches!(info, Some(info) if info.flags.is_file_level() && info.flags.is_optimistic() && info.flags.is_reserved_in_nested_scopes())
}

// A simple inlinable expression is an expression which can be copied into multiple locations
// without risk of repeating any sideeffects and whose value could not possibly change between
// any such locations
// utilities.go:117
pub fn is_simple_inlineable_expression(expression: P<Node>) -> bool {
    !ast::is_identifier(expression) && is_simple_copiable_expression(expression)
}
