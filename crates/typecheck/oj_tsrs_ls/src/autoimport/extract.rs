use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tsrs_ast::{
    self as ast, CheckFlags, JSDeclarationKind, Kind, ModifierFlags, Node, SourceFile, Symbol,
    SymbolFlags,
};
use tsrs_binder::{self as binder, NameResolver};
use tsrs_checker::Checker;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{ModuleKind, P};
use tsrs_module::DefaultResolver;

use super::export::{Export, ExportID, ExportSyntax, ModuleID};
use super::util::{
    PathFunc, get_default_like_export_name_from_declaration,
    try_get_module_id_and_file_name_of_module_symbol,
};
use crate::lsutil;

pub type ToPathFunc = Arc<dyn Fn(&str) -> Path + Send + Sync>;

// extract.go:16
pub struct SymbolExtractor<'a> {
    package_name: String,
    stats: Arc<ExtractorStats>,

    local_name_resolver: NameResolver<()>,
    checker: &'a mut Checker,
    to_path: Option<ToPathFunc>,
    // realpath, if set, is used to resolve symlinks for ModuleID generation.
    // This ensures that symlinked packages use their realpath as ModuleID,
    // deduplicating exports from files that appear via multiple symlink paths.
    realpath: Option<PathFunc>,
}

// extract.go:29
pub struct ExportExtractor<'a> {
    symbol_extractor: SymbolExtractor<'a>,
    module_resolver: &'a DefaultResolver<'a>,
}

// extract.go:34
#[derive(Default)]
pub struct ExtractorStats {
    pub(crate) exports: AtomicUsize,
    pub(crate) used_checker: AtomicUsize,
}

impl ExportExtractor<'_> {
    // extract.go:39
    pub(crate) fn stats(&self) -> Arc<ExtractorStats> {
        Arc::clone(&self.symbol_extractor.stats)
    }
}

// extract.go:43
// Go keeps the checker pointer in the lease; here the extractor owns the checker borrow and the lease records
// whether it was handed out.
#[derive(Default)]
pub struct CheckerLease {
    used: bool,
}

// extract.go:58
pub fn new_symbol_extractor<'a>(
    package_name: &str,
    checker: &'a mut Checker,
    to_path: Option<ToPathFunc>,
    realpath: Option<PathFunc>,
) -> SymbolExtractor<'a> {
    SymbolExtractor {
        package_name: package_name.to_string(),
        checker,
        local_name_resolver: NameResolver::new(tsrs_core::empty_compiler_options(), None),
        stats: Arc::new(ExtractorStats::default()),
        to_path,
        realpath,
    }
}

// extract.go:71
pub fn new_export_extractor<'a>(
    package_name: &str,
    checker: &'a mut Checker,
    module_resolver: &'a DefaultResolver<'a>,
    to_path: ToPathFunc,
    realpath: Option<PathFunc>,
) -> ExportExtractor<'a> {
    ExportExtractor {
        symbol_extractor: new_symbol_extractor(package_name, checker, Some(to_path), realpath),
        module_resolver,
    }
}

impl SymbolExtractor<'_> {
    // extract.go:50 (checkerLease.GetChecker)
    fn get_checker(&mut self, lease: &mut CheckerLease) -> &mut Checker {
        lease.used = true;
        self.checker
    }

    // extract.go:55 (checkerLease.TryChecker)
    fn try_checker(&mut self, lease: &CheckerLease) -> Option<&mut Checker> {
        if lease.used {
            return Some(self.checker);
        }
        None
    }

    // extract.go:79
    // getModuleID returns the ModuleID for a file, using realpath if available.
    fn get_module_id(&self, file: P<SourceFile>) -> ModuleID {
        if let (Some(realpath), Some(to_path)) = (&self.realpath, &self.to_path) {
            let realpath = realpath(file.file_name());
            return ModuleID::File(to_path(&realpath).to_string());
        }
        ModuleID::File(file.path().to_string())
    }

    // extract.go:89
    // getModuleIDForSymbol returns the ModuleID for a module symbol, using realpath
    // normalization when available for source files.
    fn get_module_id_for_symbol(&self, symbol: P<Symbol>) -> Option<ModuleID> {
        let (module_id, file_name) = try_get_module_id_and_file_name_of_module_symbol(symbol)?;
        // If fileName is set, this is a source file that may need realpath normalization
        if !file_name.is_empty()
            && self.realpath.is_some()
            && let Some(decl) = ast::get_non_augmentation_declaration(symbol)
            && decl.kind() == Kind::SourceFile
        {
            return Some(self.get_module_id(decl.as_source_file_p()));
        }
        Some(module_id)
    }
}

impl ExportExtractor<'_> {
    // extract.go:105
    pub(crate) fn extract_from_file(&mut self, file: P<SourceFile>) -> Vec<Arc<Export>> {
        if file.symbol().is_some() {
            return self.extract_from_module(file);
        }
        if !file.ambient_module_names().is_empty() {
            let mut export_count = 0;
            export_count = file.statements.nodes().iter().fold(
                export_count,
                |mut export_count, &statement| {
                    if ast::is_module_with_string_literal_name(statement)
                        && is_non_pattern_ambient_module_declaration(file, statement)
                    {
                        export_count +=
                            statement.symbol().unwrap().exports().map_or(0, |e| e.len());
                    }

                    export_count
                },
            );
            let mut exports = Vec::with_capacity(export_count);
            for &statement in file.statements.nodes() {
                if ast::is_module_with_string_literal_name(statement)
                    && is_non_pattern_ambient_module_declaration(file, statement)
                {
                    self.extract_from_module_declaration(
                        statement,
                        file,
                        &ModuleID::Ambient(statement.name().unwrap().text().to_string()),
                        "",
                        &mut exports,
                    );
                }
            }
            return exports;
        }
        Vec::new()
    }

    // extract.go:144
    fn extract_from_module(&mut self, file: P<SourceFile>) -> Vec<Arc<Export>> {
        let module_augmentations: Vec<P<Node>> = file
            .module_augmentations()
            .iter()
            .filter_map(|&name| {
                let decl = name.parent().unwrap();
                if ast::is_global_scope_augmentation(decl) {
                    return None;
                }
                Some(decl)
            })
            .collect();
        let mut augmentation_export_count = 0;
        augmentation_export_count = module_augmentations.iter().fold(
            augmentation_export_count,
            |mut augmentation_export_count, decl| {
                augmentation_export_count +=
                    decl.symbol().unwrap().exports().map_or(0, |e| e.len());

                augmentation_export_count
            },
        );
        let module_id = self.symbol_extractor.get_module_id(file);
        let file_exports = file.symbol().unwrap().exports();
        let mut exports =
            Vec::with_capacity(file_exports.map_or(0, |e| e.len()) + augmentation_export_count);
        if let Some(file_exports) = file_exports {
            for (name, symbol) in file_exports.entries() {
                self.symbol_extractor.extract_from_symbol(
                    name,
                    symbol,
                    &module_id,
                    file.file_name(),
                    file,
                    &mut exports,
                );
            }
        }
        for &decl in &module_augmentations {
            let name = decl.name().unwrap().as_string_literal().text();
            let mut module_id = ModuleID::Ambient(name.to_string());
            let mut module_file_name = String::new();
            let mut unresolved_module_specifier = String::new();
            if tspath::is_external_module_name_relative(name) {
                let resolved = self
                    .module_resolver
                    .resolve_module_name(name, file.file_name(), ModuleKind::COMMON_JS, None)
                    .unwrap()
                    .0;
                if resolved.is_resolved() {
                    module_file_name = resolved.resolved_file_name.to_string();
                    module_id = ModuleID::File(
                        (self.symbol_extractor.to_path.as_ref().unwrap())(&module_file_name)
                            .to_string(),
                    );
                } else {
                    module_file_name = tspath::resolve_path(
                        &tspath::get_directory_path(file.file_name()),
                        &[name],
                    );
                    module_id = ModuleID::File(
                        (self.symbol_extractor.to_path.as_ref().unwrap())(&module_file_name)
                            .to_string(),
                    );
                    unresolved_module_specifier = name.to_string();
                }
            }
            let export_start = exports.len();
            self.extract_from_module_declaration(
                decl,
                file,
                &module_id,
                &module_file_name,
                &mut exports,
            );
            for export in &mut exports[export_start..] {
                Arc::make_mut(export)
                    .unresolved_module_specifier
                    .clone_from(&unresolved_module_specifier);
            }
        }
        exports
    }

    // extract.go:179
    fn extract_from_module_declaration(
        &mut self,
        decl: P<Node>,
        file: P<SourceFile>,
        module_id: &ModuleID,
        module_file_name: &str,
        exports: &mut Vec<Arc<Export>>,
    ) {
        if let Some(decl_exports) = decl.symbol().unwrap().exports() {
            for (name, symbol) in decl_exports.entries() {
                self.symbol_extractor.extract_from_symbol(
                    name,
                    symbol,
                    module_id,
                    module_file_name,
                    file,
                    exports,
                );
            }
        }
    }
}

// extract.go:136
fn is_non_pattern_ambient_module_declaration(file: P<SourceFile>, decl: P<Node>) -> bool {
    !(file.pattern_ambient_modules().iter().any(|module| Some(module.symbol) == decl.symbol()))
}

impl SymbolExtractor<'_> {
    // extract.go:185
    pub(crate) fn extract_from_symbol(
        &mut self,
        name: &str,
        symbol: P<Symbol>,
        module_id: &ModuleID,
        module_file_name: &str,
        file: P<SourceFile>,
        exports: &mut Vec<Arc<Export>>,
    ) {
        if should_ignore_symbol(symbol) {
            return;
        }

        if name == ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR {
            let mut checker_lease = CheckerLease::default();
            let parent = symbol.parent().unwrap();
            let mut all_exports = self.checker.get_exports_of_module_exported(parent);
            // allExports includes named exports from the file that will be processed separately;
            // we want to add only the ones that come from the star
            if let Some(parent_exports) = parent.exports() {
                for (name, named_export) in parent_exports.entries() {
                    if name != ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR {
                        let idx = all_exports.iter().position(|&s| s == named_export);
                        if idx.is_some() || should_ignore_symbol(named_export) {
                            // Go's `slices.Delete(allExports, -1, 0)` (an ignored symbol that is not in the list) panics.
                            let idx = idx.expect("slices.Delete: index out of range");
                            all_exports.remove(idx);
                        }
                    }
                }
            }

            exports.reserve(all_exports.len());
            exports.extend(all_exports.into_iter().filter_map(|reexported_symbol| {
                let (export, _) = self.create_export(
                    reexported_symbol,
                    module_id,
                    module_file_name,
                    ExportSyntax::Star,
                    file,
                    &mut checker_lease,
                );
                if let Some(mut export) = export {
                    let reexported_parent = reexported_symbol.parent();
                    let parent = reexported_parent
                        .map(|p| self.get_checker(&mut checker_lease).get_merged_symbol(p));
                    if reexported_parent.is_none() {
                        // Go's GetMergedSymbol(nil) returns nil (the lease is still marked used).
                        self.get_checker(&mut checker_lease);
                    }
                    if let Some(parent) = parent
                        && parent.is_external_module()
                        && let Some(target_module_id) = self.get_module_id_for_symbol(parent)
                    {
                        export.target = ExportID {
                            export_name: reexported_symbol.name().to_string(),
                            module_id: target_module_id,
                        };
                    }
                    export.through = ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR.to_string();
                    Some(Arc::new(export))
                } else {
                    None
                }
            }));
            return;
        }

        let syntax = get_syntax(symbol);
        let mut checker_lease = CheckerLease::default();
        let (export, target) = self.create_export(
            symbol,
            module_id,
            module_file_name,
            syntax,
            file,
            &mut checker_lease,
        );
        let Some(export) = export else {
            return;
        };

        exports.push(Arc::new(export));

        if let Some(target) = target {
            if syntax == ExportSyntax::Equals
                && target.flags().intersects(SymbolFlags::Namespace)
                && let Some(target_exports) = target.exports()
            {
                exports.reserve(target_exports.len());
                exports.extend(target_exports.entries().into_iter().filter_map(
                    |(inner_name, named_export)| {
                        if inner_name == ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR {
                            None
                        } else {
                            let (export, _) = self.create_export(
                                named_export,
                                module_id,
                                module_file_name,
                                syntax,
                                file,
                                &mut checker_lease,
                            );
                            if let Some(mut export) = export {
                                export.through = name.to_string();
                                Some(Arc::new(export))
                            } else {
                                None
                            }
                        }
                    },
                ));
            }
        } else if syntax == ExportSyntax::CommonJSModuleExports {
            let expression = symbol.declarations()[0].as_binary_expression().right();
            if expression.kind() == Kind::ObjectLiteralExpression {
                // what is actually desirable here? I think it would be reasonable to only treat these as exports
                // if *every* property is a shorthand property or identifier: identifier
                // At least, it would be sketchy if there were any methods, computed properties...
                let properties = expression.as_object_literal_expression().properties.nodes();
                exports.reserve(properties.len());
                exports.extend(properties.iter().filter_map(|&prop| {
                    if ast::is_shorthand_property_assignment(prop)
                        || ast::is_property_assignment(prop)
                            && prop.name().unwrap().kind() == Kind::Identifier
                    {
                        // Go indexes the members map, which yields nil for a missing member; createExport then
                        // dereferences it.
                        let member = expression
                            .symbol()
                            .unwrap()
                            .members()
                            .unwrap()
                            .lookup(prop.name().unwrap().text())
                            .expect("nil member symbol");
                        let (export, _) = self.create_export(
                            member,
                            module_id,
                            module_file_name,
                            syntax,
                            file,
                            &mut checker_lease,
                        );
                        if let Some(mut export) = export {
                            export.through = name.to_string();
                            Some(Arc::new(export))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }));
            }
        }
    }

    // extract.go:268
    // createExport creates an Export for the given symbol, returning the Export and the target symbol if the export is an alias.
    fn create_export(
        &mut self,
        symbol: P<Symbol>,
        module_id: &ModuleID,
        module_file_name: &str,
        syntax: ExportSyntax,
        file: P<SourceFile>,
        checker_lease: &mut CheckerLease,
    ) -> (Option<Export>, Option<P<Symbol>>) {
        if should_ignore_symbol(symbol) {
            return (None, None);
        }

        let mut export = Export {
            export_id: ExportID {
                export_name: symbol.name().to_string(),
                module_id: module_id.clone(),
            },
            module_file_name: module_file_name.to_string(),
            syntax,
            flags: symbol.combined_local_and_export_symbol_flags(),
            path: file.path().clone(),
            package_name: self.package_name.clone(),
            ..Default::default()
        };

        if syntax == ExportSyntax::UMD {
            export.export_id.export_name = ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS.to_string();
            export.local_name = symbol.name().to_string();
        }

        let mut target_symbol: Option<P<Symbol>> = None;
        if symbol.flags().intersects(SymbolFlags::Alias) {
            target_symbol = self.try_resolve_symbol(symbol, syntax, checker_lease);
            if let Some(ts) = target_symbol {
                let mut decl: Option<P<Node>> = None;
                if !ts.declarations().is_empty() {
                    decl = Some(ts.declarations()[0]);
                } else if ts.check_flags().intersects(CheckFlags::Mapped)
                    && let Some(mapped_decl) =
                        self.get_checker(checker_lease).get_mapped_type_symbol_of_property(ts)
                    && !mapped_decl.declarations().is_empty()
                {
                    decl = Some(mapped_decl.declarations()[0]);
                }
                if decl.is_none() {
                    // !!! consider GetImmediateAliasedSymbol to go as far as we can
                    decl = Some(symbol.declarations()[0]);
                }
                let Some(decl) = decl else {
                    panic!("no declaration for aliased symbol");
                };

                let mut parent = ts.parent();
                if let Some(checker) = self.try_checker(checker_lease) {
                    export.flags = checker.get_symbol_flags(ts);
                    export.is_type_only = checker.get_type_only_alias_declaration(symbol).is_some();
                    parent = parent.map(|p| checker.get_merged_symbol(p));
                } else {
                    export.flags = ts.flags();
                    export.is_type_only = symbol
                        .declarations()
                        .iter()
                        .any(|&d| ast::is_part_of_type_only_import_or_export_declaration(d));
                }
                export.script_element_kind =
                    lsutil::get_symbol_kind(self.try_checker(checker_lease), ts, decl);
                export.script_element_kind_modifiers =
                    lsutil::get_symbol_modifiers(self.try_checker(checker_lease), Some(ts));
                let mut target_module_id =
                    ModuleID::File(ast::get_source_file_of_node(decl).unwrap().path().to_string());
                if let Some(parent) = parent
                    && parent.is_external_module()
                    && let Some(id) = self.get_module_id_for_symbol(parent)
                {
                    target_module_id = id;
                }
                export.target =
                    ExportID { export_name: ts.name().to_string(), module_id: target_module_id };
            }
        } else {
            export.script_element_kind = lsutil::get_symbol_kind(
                self.try_checker(checker_lease),
                symbol,
                symbol.declarations()[0],
            );
            export.script_element_kind_modifiers =
                lsutil::get_symbol_modifiers(self.try_checker(checker_lease), Some(symbol));
        }

        if symbol.name() == ast::INTERNAL_SYMBOL_NAME_DEFAULT
            || symbol.name() == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
        {
            let mut named_symbol = symbol;
            if let Some(s) = binder::get_local_symbol_for_export_default(symbol) {
                named_symbol = s;
            }
            export.local_name = get_default_like_export_name_from_declaration(named_symbol);
            if is_unusable_name(&export.local_name) {
                export.local_name = export.target.export_name.clone();
            }
            if is_unusable_name(&export.local_name)
                && let Some(ts) = target_symbol
            {
                named_symbol = ts;
                if let Some(s) = binder::get_local_symbol_for_export_default(ts) {
                    named_symbol = s;
                }
                export.local_name = get_default_like_export_name_from_declaration(named_symbol);
            }
            if is_unusable_name(&export.local_name) {
                // Last resort: derive identifier from the file name. Use FileName() (original
                // casing) rather than ModuleID/Path() which is lowercased on case-insensitive
                // file systems, losing PascalCase.
                export.local_name = lsutil::module_specifier_to_valid_identifier(
                    &file_name_for_default_export_name(target_symbol, module_file_name, module_id),
                    false,
                );
            }
        }

        if is_unusable_name(export.name()) {
            return (None, None);
        }

        self.stats.exports.fetch_add(1, Ordering::Relaxed);
        if self.try_checker(checker_lease).is_some() {
            self.stats.used_checker.fetch_add(1, Ordering::Relaxed);
        }

        (Some(export), target_symbol)
    }

    // extract.go:369
    fn try_resolve_symbol(
        &mut self,
        symbol: P<Symbol>,
        syntax: ExportSyntax,
        checker_lease: &mut CheckerLease,
    ) -> Option<P<Symbol>> {
        if !ast::is_non_local_alias(symbol, SymbolFlags::None) {
            return Some(symbol);
        }

        let mut loc: Option<P<Node>> = None;
        let mut name = "";
        let mut default_declaration = false;
        match syntax {
            ExportSyntax::Named => {
                let decl = ast::get_declaration_of_kind(symbol, Kind::ExportSpecifier).unwrap();
                if decl
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .as_export_declaration()
                    .module_specifier
                    .is_none()
                {
                    let n = decl.name().or(decl.property_name()).unwrap();
                    if n.kind() == Kind::Identifier {
                        loc = Some(n);
                        name = n.text();
                    }
                }
            }
            // !!! check if module.exports = foo is marked as an alias
            ExportSyntax::Equals => {
                if symbol.name() == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS {
                    default_declaration = true;
                }
            }
            ExportSyntax::DefaultDeclaration => default_declaration = true,
            _ => {}
        }
        if default_declaration {
            let decl = ast::get_declaration_of_kind(symbol, Kind::ExportAssignment).unwrap();
            let expression = decl.expression().unwrap();
            if expression.kind() == Kind::Identifier {
                loc = Some(expression);
                name = expression.text();
            }
        }

        if let Some(loc) = loc {
            let local = self.local_name_resolver.resolve(
                &mut (),
                Some(loc),
                name,
                SymbolFlags::All,
                None,
                false,
                false,
            );
            if let Some(local) = local
                && !ast::is_non_local_alias(local, SymbolFlags::None)
            {
                return Some(local);
            }
        }

        let checker = self.get_checker(checker_lease);
        let resolved = checker.get_aliased_symbol(symbol);
        if !checker.is_unknown_symbol(resolved) {
            return Some(resolved);
        }
        None
    }
}

// extract.go:410
fn should_ignore_symbol(symbol: P<Symbol>) -> bool {
    if symbol.flags().intersects(SymbolFlags::Prototype) {
        return true;
    }
    false
}

// extract.go:417
fn get_syntax(symbol: P<Symbol>) -> ExportSyntax {
    if let Some(result) = symbol.declarations().iter().find_map(|&decl| {
        match decl.kind() {
            Kind::ExportSpecifier => return Some(ExportSyntax::Named),
            Kind::ExportAssignment => {
                return Some(if decl.as_export_assignment().is_export_equals {
                    ExportSyntax::Equals
                } else {
                    ExportSyntax::DefaultDeclaration
                });
            }
            Kind::NamespaceExportDeclaration => return Some(ExportSyntax::UMD),
            Kind::BinaryExpression => match ast::get_assignment_declaration_kind(decl) {
                JSDeclarationKind::ModuleExports => {
                    return Some(ExportSyntax::CommonJSModuleExports);
                }
                JSDeclarationKind::ExportsProperty => {
                    return Some(ExportSyntax::CommonJSExportsProperty);
                }
                _ => {}
            },
            _ => {
                if ast::get_combined_modifier_flags(decl).intersects(ModifierFlags::Default) {
                    return Some(ExportSyntax::DefaultModifier);
                }
                return Some(ExportSyntax::Modifier);
            }
        }

        None
    }) {
        return result;
    }
    ExportSyntax::None
}

// extract.go:447
fn is_unusable_name(name: &str) -> bool {
    name.is_empty()
        || name == "_default"
        || name == ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR
        || name == ast::INTERNAL_SYMBOL_NAME_DEFAULT
        || name == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
}

// extract.go:460
// fileNameForDefaultExportName returns the best file name to use when deriving
// a fallback identifier for a default-like export. It prefers the target symbol's
// source file (closest to the export origin), falls back to the module's original
// file name, and uses the lowercased moduleID only for ambient modules where no
// original file name is available.
fn file_name_for_default_export_name(
    target_symbol: Option<P<Symbol>>,
    module_file_name: &str,
    module_id: &ModuleID,
) -> String {
    if let Some(ts) = target_symbol
        && !ts.declarations().is_empty()
    {
        let fn_ =
            ast::get_source_file_of_node(ts.declarations()[0]).unwrap().file_name().to_string();
        if !fn_.is_empty() {
            return fn_;
        }
    }
    if !module_file_name.is_empty() {
        return module_file_name.to_string();
    }
    module_id.as_str().to_string()
}
