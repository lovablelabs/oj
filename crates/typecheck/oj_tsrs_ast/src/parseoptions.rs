use tsrs_core::tspath::{self, Path};
use tsrs_core::{CompilerOptions, JsxEmit, ModuleDetectionKind, ModuleKind, P, ScriptKind};

use crate::*;

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct SourceFileParseOptions {
    pub file_name: String,
    pub path: Path,
    pub external_module_indicator_options: ExternalModuleIndicatorOptions,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ExternalModuleIndicatorOptions {
    pub jsx: bool,
    pub force: bool,
}

pub fn get_external_module_indicator_options(
    file_name: &str,
    options: &CompilerOptions,
    metadata: &SourceFileMetaData,
) -> ExternalModuleIndicatorOptions {
    if tspath::is_declaration_file_name(file_name) {
        return crate::parseoptions::ExternalModuleIndicatorOptions::default();
    }

    match options.get_emit_module_detection_kind() {
        // All non-declaration files are modules, declaration files still do the usual isFileProbablyExternalModule
        ModuleDetectionKind::Force => ExternalModuleIndicatorOptions { jsx: false, force: true },
        // Files are modules if they have imports, exports, or import.meta
        ModuleDetectionKind::Legacy => {
            crate::parseoptions::ExternalModuleIndicatorOptions::default()
        }
        // If module is nodenext or node16, all esm format files are modules
        // If jsx is react-jsx or react-jsxdev then jsx tags force module-ness
        // otherwise, the presence of import or export statments (or import.meta) implies module-ness
        ModuleDetectionKind::Auto => ExternalModuleIndicatorOptions {
            jsx: options.jsx == JsxEmit::REACT_JSX || options.jsx == JsxEmit::REACT_JSX_DEV,
            force: is_file_forced_to_be_module_by_format(file_name, options, metadata),
        },
        ModuleDetectionKind::None => crate::parseoptions::ExternalModuleIndicatorOptions::default(),
    }
}

static IS_FILE_FORCED_TO_BE_MODULE_BY_FORMAT_EXTENSIONS: &[&str] =
    &[tspath::EXTENSION_CJS, tspath::EXTENSION_CTS, tspath::EXTENSION_MJS, tspath::EXTENSION_MTS];

fn is_file_forced_to_be_module_by_format(
    file_name: &str,
    options: &CompilerOptions,
    metadata: &SourceFileMetaData,
) -> bool {
    // Excludes declaration files - they still require an explicit `export {}` or the like
    // for back compat purposes. The only non-declaration files _not_ forced to be a module are `.js` files
    // that aren't esm-mode (meaning not in a `type: module` scope).
    get_implied_node_format_for_emit_worker(file_name, options.get_emit_module_kind(), metadata)
        == ModuleKind::ES_NEXT
        || tspath::file_extension_is_one_of(
            file_name,
            IS_FILE_FORCED_TO_BE_MODULE_BY_FORMAT_EXTENSIONS,
        )
}

pub fn set_external_module_indicator(file: &SourceFile, opts: ExternalModuleIndicatorOptions) {
    file.external_module_indicator.set(get_external_module_indicator(file, opts));
}

fn get_external_module_indicator(
    file: &SourceFile,
    opts: ExternalModuleIndicatorOptions,
) -> Option<P<Node>> {
    if file.script_kind.get() == ScriptKind::JSON {
        return None;
    }

    if let Some(node) = is_file_probably_external_module(file) {
        return Some(node);
    }

    if file.is_declaration_file.get() {
        return None;
    }

    if opts.jsx
        && let Some(node) = is_file_module_from_using_jsx_tag(file)
    {
        return Some(node);
    }

    if opts.force {
        return Some(file.as_node());
    }

    None
}

fn is_file_probably_external_module(source_file: &SourceFile) -> Option<P<Node>> {
    if let Some(result) = source_file.statements.nodes().iter().find_map(|&statement| {
        if is_an_external_module_indicator_node(statement) { Some(Some(statement)) } else { None }
    }) {
        return result;
    }
    get_import_meta_if_necessary(source_file)
}

fn is_an_external_module_indicator_node(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Export)
        || is_import_equals_declaration(node)
            && is_external_module_reference(node.as_import_equals_declaration().module_reference)
        || is_import_declaration(node)
        || is_export_assignment(node)
        || is_export_declaration(node)
}

fn get_import_meta_if_necessary(source_file: &SourceFile) -> Option<P<Node>> {
    if source_file.as_node().flags.get().intersects(NodeFlags::PossiblyContainsImportMeta) {
        return find_child_node(source_file.as_node(), is_import_meta);
    }
    None
}

fn find_child_node(root: P<Node>, check: fn(P<Node>) -> bool) -> Option<P<Node>> {
    fn visit(node: P<Node>, check: fn(P<Node>) -> bool, result: &mut Option<P<Node>>) -> bool {
        if check(node) {
            *result = Some(node);
            return true;
        }
        node.for_each_child(&mut |child| visit(child, check, result))
    }
    let mut result = None;
    visit(root, check, &mut result);
    result
}

fn is_file_module_from_using_jsx_tag(file: &SourceFile) -> Option<P<Node>> {
    walk_tree_for_jsx_tags(file.as_node())
}

// This is a somewhat unavoidable full tree walk to locate a JSX tag - `import.meta` requires the same,
// but we avoid that walk (or parts of it) if at all possible using the `PossiblyContainsImportMeta` node flag.
// Unfortunately, there's no `NodeFlag` space to do the same for JSX.
fn walk_tree_for_jsx_tags(node: P<Node>) -> Option<P<Node>> {
    fn visitor(node: P<Node>, found: &mut Option<P<Node>>) -> bool {
        if found.is_some() {
            return true;
        }
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsJsx) {
            return false;
        }
        if is_jsx_opening_like_element(node) || is_jsx_fragment(node) {
            *found = Some(node);
            return true;
        }
        node.for_each_child(&mut |child| visitor(child, found))
    }
    let mut found = None;
    visitor(node, &mut found);
    found
}
