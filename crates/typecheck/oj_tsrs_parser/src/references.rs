use tsrs_ast::{self as ast, ModifierFlags, Node, NodeFlags, SourceFile};
use tsrs_core::{self as core, P, Tristate, alloc_vec, tspath};

// Go appends to the file's slices one element at a time (amortized growth, old backing arrays are garbage). The
// arena never frees, so the appends go to local vectors that are stored once at the end; nothing reads these file
// fields while references are collected.
struct References {
    imports: Vec<P<Node>>,
    module_augmentations: Vec<P<Node>>,
    ambient_module_names: Vec<&'static str>,
}

pub fn collect_external_module_references(file: P<SourceFile>) {
    let mut refs = References {
        imports: file.imports().to_vec(),
        module_augmentations: file.module_augmentations().to_vec(),
        ambient_module_names: file.ambient_module_names().to_vec(),
    };
    for &node in file.statements().nodes() {
        collect_module_references(file, &mut refs, node, false /*inAmbientModule*/);
    }

    if file.as_node().flags().intersects(NodeFlags::PossiblyContainsDynamicImport)
        || ast::is_in_js_file(Some(file.as_node()))
    {
        ast::for_each_dynamic_import_or_require_call(
            file,
            true, /*includeTypeSpaceImports*/
            true, /*requireStringLiteralLikeArgument*/
            |_node: P<Node>, module_specifier: P<Node>| {
                refs.imports.push(module_specifier);
                false
            },
        );
    }
    set_imports_of_source_file(file, refs.imports);
    file.module_augmentations.set(alloc_vec(refs.module_augmentations));
    file.ambient_module_names.set(alloc_vec(refs.ambient_module_names));
}

fn collect_module_references(
    file: P<SourceFile>,
    refs: &mut References,
    node: P<Node>,
    in_ambient_module: bool,
) {
    if ast::is_any_import_or_re_export(node) {
        let module_name_expr = ast::get_external_module_name(node);
        // TypeScript 1.0 spec (April 2014): 12.1.6
        // An ExternalImportDeclaration in an AmbientExternalModuleDeclaration may reference other external modules
        // only through top - level external module names. Relative external module names are not permitted.
        if let Some(module_name_expr) = module_name_expr.filter(|e| ast::is_string_literal(*e)) {
            let module_name = module_name_expr.text();
            if !module_name.is_empty()
                && (!in_ambient_module || !tspath::is_external_module_name_relative(module_name))
            {
                refs.imports.push(module_name_expr);
                // !!! removed `&& p.currentNodeModulesDepth == 0`
                if file.uses_uri_style_node_core_modules() != Tristate::True
                    && !file.is_declaration_file()
                {
                    if module_name.starts_with("node:")
                        && !core::is_exclusively_prefixed_node_core_module(module_name)
                    {
                        // Presence of `node:` prefix takes precedence over unprefixed node core modules
                        file.uses_uri_style_node_core_modules.set(Tristate::True);
                    } else if file.uses_uri_style_node_core_modules() == Tristate::Unknown
                        && core::is_unprefixed_node_core_module(module_name)
                    {
                        // Avoid `unprefixedNodeCoreModules.has` for every import
                        file.uses_uri_style_node_core_modules.set(Tristate::False);
                    }
                }
            }
        }
        return;
    }
    if ast::is_module_declaration(node)
        && ast::is_ambient_module(node)
        && (in_ambient_module
            || ast::has_syntactic_modifier(node, ModifierFlags::Ambient)
            || file.is_declaration_file())
    {
        let name_text = node.as_module_declaration().name().text();
        // Ambient module declarations can be interpreted as augmentations for some existing external modules.
        // This will happen in two cases:
        // - if current file is external module then module augmentation is a ambient module declaration defined in the top level scope
        // - if current file is not external module then module augmentation is an ambient module declaration with non-relative module name
        //   immediately nested in top level ambient module declaration .
        if ast::is_external_module(file)
            || (in_ambient_module && !tspath::is_external_module_name_relative(name_text))
        {
            refs.module_augmentations.push(node.as_module_declaration().name());
        } else if !in_ambient_module {
            refs.ambient_module_names.push(name_text);
            // An AmbientExternalModuleDeclaration declares an external module.
            // This type of declaration is permitted only in the global module.
            // The StringLiteral must specify a top - level external module name.
            // Relative external module names are not permitted
            // NOTE: body of ambient module is always a module block, if it exists
            if let Some(body) = node.body() {
                for &statement in body.statements() {
                    collect_module_references(file, refs, statement, true /*inAmbientModule*/);
                }
            }
        }
    }
}

// Go ast.SetImportsOfSourceFile.
fn set_imports_of_source_file(file: P<SourceFile>, imports: Vec<P<Node>>) {
    file.imports.set(alloc_vec(imports));
}
