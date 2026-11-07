use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::*;
use tsrs_core::*;

use crate::*;

bitflags::bitflags! {
    // Flags enum to track count of temp variables and a few dedicated names
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub(crate) struct tempFlags: u32 {
        const Auto = 0x0000_0000; // No preferred name
        const CountMask = 0x0FFF_FFFF; // Temp variable counter
        const _i = 0x1000_0000; // Use/preference flag for '_i'
    }
}

/// Go `func(string, bool) bool` callback (Printer.isFileLevelUniqueNameInCurrentFile).
pub type IsFileLevelUniqueNameFn = Rc<dyn Fn(&str, bool) -> bool>;
/// Go `func(*ast.Node) string` callback (Printer.getTextOfNode).
///
/// It receives the generator back (PORTING.md callback convention) because the printer's implementation
/// re-enters `generate_name` for generated names.
pub type GetTextOfNodeFn = Rc<dyn Fn(&mut NameGenerator, P<Node>) -> String>;

#[derive(Default)]
pub struct NameGenerator {
    pub(crate) context: Option<P<EmitContext>>,
    pub(crate) is_file_level_unique_name_in_current_file: Option<IsFileLevelUniqueNameFn>, // callback for Printer.isFileLevelUniqueNameInCurrentFile
    pub(crate) get_text_of_node: Option<GetTextOfNodeFn>, // callback for Printer.getTextOfNode
    node_id_to_generated_name: Option<FxHashMap<NodeId, String>>, // Map of generated names for specific nodes
    node_id_to_generated_private_name: Option<FxHashMap<NodeId, String>>, // Map of generated private names for specific nodes
    auto_generated_id_to_generated_name: Option<FxHashMap<AutoGenerateId, String>>, // Map of generated names for temp and loop variables
    name_generation_scope: Option<Box<NameGenerationScope>>,
    private_name_generation_scope: Option<Box<NameGenerationScope>>,
    generated_names: FxHashSet<String>, // NOTE: Used to match Strada, but should be moved to nameGenerationScope after port is complete.
}

#[derive(Default)]
pub struct NameGenerationScope {
    next: Option<Box<NameGenerationScope>>, // The next nameGenerationScope in the stack
    temp_flags: tempFlags,                  // TempFlags for the current name generation scope.
    formatted_name_temp_flags: Option<FxHashMap<String, tempFlags>>, // TempFlags for the current name generation scope.
    reserved_names: FxHashSet<String>, // Names reserved in nested name generation scopes.
                                       // generatedNames         collections.Set[string] // NOTE: generated names should be scoped after Strada port is complete.
}

impl NameGenerator {
    pub(crate) fn push_scope(&mut self, reuse_temp_variable_scope: bool) {
        self.private_name_generation_scope = Some(Box::new(NameGenerationScope {
            next: self.private_name_generation_scope.take(),
            ..Default::default()
        }));
        if !reuse_temp_variable_scope {
            self.name_generation_scope = Some(Box::new(NameGenerationScope {
                next: self.name_generation_scope.take(),
                ..Default::default()
            }));
        }
    }

    pub(crate) fn pop_scope(&mut self, reuse_temp_variable_scope: bool) {
        if let Some(scope) = self.private_name_generation_scope.take() {
            self.private_name_generation_scope = scope.next;
        }
        if !reuse_temp_variable_scope && let Some(scope) = self.name_generation_scope.take() {
            self.name_generation_scope = scope.next;
        }
    }

    fn get_scope(&mut self, private_name: bool) -> &mut Option<Box<NameGenerationScope>> {
        if private_name {
            &mut self.private_name_generation_scope
        } else {
            &mut self.name_generation_scope
        }
    }

    fn get_temp_flags(&mut self, private_name: bool) -> tempFlags {
        let scope = self.get_scope(private_name);
        if let Some(scope) = scope {
            return scope.temp_flags;
        }
        tempFlags::Auto
    }

    fn set_temp_flags(&mut self, private_name: bool, flags: tempFlags) {
        let scope = self.get_scope(private_name);
        scope.get_or_insert_with(Default::default).temp_flags = flags;
    }

    // Gets the TempFlags to use in the current nameGenerationScope for the given key
    fn get_temp_flags_for_formatted_name(
        &mut self,
        private_name: bool,
        formatted_name_key: &str,
    ) -> tempFlags {
        let scope = self.get_scope(private_name);
        if let Some(scope) = scope
            && let Some(flags) =
                scope.formatted_name_temp_flags.as_ref().and_then(|m| m.get(formatted_name_key))
        {
            return *flags;
        }
        tempFlags::Auto
    }

    // Sets the TempFlags to use in the current nameGenerationScope for the given key
    fn set_temp_flags_for_formatted_name(
        &mut self,
        private_name: bool,
        formatted_name_key: &str,
        flags: tempFlags,
    ) {
        let scope = self.get_scope(private_name).get_or_insert_with(Default::default);
        scope
            .formatted_name_temp_flags
            .get_or_insert_with(FxHashMap::default)
            .insert(formatted_name_key.to_string(), flags);
    }

    fn reserve_name(&mut self, name: &str, private_name: bool, scoped: bool, temp: bool) {
        let scope = self.get_scope(private_name).get_or_insert_with(Default::default);
        if private_name || scoped {
            scope.reserved_names.insert(name.to_string());
        } else if !temp {
            self.generated_names.insert(name.to_string()); // NOTE: Matches Strada, but is incorrect.
            // (*scope).generatedNames.Add(name) // TODO: generated names should be scoped after Strada port is complete.
        }
    }

    fn call_get_text_of_node(&mut self, node: P<Node>) -> String {
        let f = self.get_text_of_node.clone().expect("NameGenerator.GetTextOfNode is not set");
        f(self, node)
    }

    // Generate the text for a generated identifier or private identifier
    pub(crate) fn generate_name(&mut self, name: P<Node>) -> String {
        if let Some(context) = self.context
            && let Some(auto_generate) = context.get_auto_generate_info(Some(name))
        {
            if auto_generate.flags.is_node() {
                // Node names generate unique names based on their original node
                // and are cached based on that node's id.
                return self.generate_name_for_node_cached(
                    context.get_node_for_generated_name(name),
                    is_private_identifier(name),
                    auto_generate.flags,
                    auto_generate.prefix,
                    auto_generate.suffix,
                );
            }
            // Auto, Loop, and Unique names are cached based on their unique autoGenerateId.
            if let Some(auto_generated_name) = self
                .auto_generated_id_to_generated_name
                .as_ref()
                .and_then(|m| m.get(&auto_generate.id))
            {
                return auto_generated_name.clone();
            }
            let auto_generated_name = self.make_name(name);
            self.auto_generated_id_to_generated_name
                .get_or_insert_with(FxHashMap::default)
                .insert(auto_generate.id, auto_generated_name.clone());
            return auto_generated_name;
        }
        self.call_get_text_of_node(name)
    }

    fn generate_name_for_node_cached(
        &mut self,
        node: P<Node>,
        private_name: bool,
        flags: GeneratedIdentifierFlags,
        prefix: &str,
        suffix: &str,
    ) -> String {
        let node_id = get_node_id(node);
        {
            let cache = if private_name {
                &mut self.node_id_to_generated_private_name
            } else {
                &mut self.node_id_to_generated_name
            };
            if let Some(name) = cache.get_or_insert_with(FxHashMap::default).get(&node_id) {
                return name.clone();
            }
        }

        let name = self.generate_name_for_node(node, private_name, flags, prefix, suffix);
        let cache = if private_name {
            &mut self.node_id_to_generated_private_name
        } else {
            &mut self.node_id_to_generated_name
        };
        cache.get_or_insert_with(FxHashMap::default).insert(node_id, name.clone());
        name
    }

    fn generate_name_for_node(
        &mut self,
        node: P<Node>,
        private_name: bool,
        flags: GeneratedIdentifierFlags,
        prefix: &str,
        suffix: &str,
    ) -> String {
        match node.kind() {
            Kind::Identifier | Kind::PrivateIdentifier => {
                let text = self.call_get_text_of_node(node);
                self.make_unique_name(
                    &text,
                    None, /*checkFn*/
                    flags.is_optimistic(),
                    flags.is_reserved_in_nested_scopes(),
                    private_name,
                    prefix,
                    suffix,
                )
            }
            Kind::ModuleDeclaration | Kind::EnumDeclaration => {
                assert!(
                    !(private_name || !prefix.is_empty() || !suffix.is_empty()),
                    "Generated name for a module or enum cannot be private and may have neither a prefix nor suffix"
                );
                self.generate_name_for_module_or_enum(node)
            }
            Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::ExportDeclaration => {
                assert!(
                    !(private_name || !prefix.is_empty() || !suffix.is_empty()),
                    "Generated name for an import or export cannot be private and may have neither a prefix nor suffix"
                );
                self.generate_name_for_import_or_export_declaration(node)
            }
            Kind::FunctionDeclaration | Kind::ClassDeclaration => {
                assert!(
                    !(private_name || !prefix.is_empty() || !suffix.is_empty()),
                    "Generated name for a class or function declaration cannot be private and may have neither a prefix nor suffix"
                );
                let name = node.name();
                if let Some(name) = name {
                    // NOTE: Go writes `!(g.Context == nil && g.Context.HasAutoGenerateInfo(name))`, which is always true
                    // when it does not panic.
                    if !(self.context.is_none()
                        && self.context.unwrap().has_auto_generate_info(Some(name)))
                    {
                        return self.generate_name_for_node(
                            name, false, /*privateName*/
                            flags, "", /*prefix*/
                            "", /*suffix*/
                        );
                    }
                }
                self.generate_name_for_export_default()
            }
            Kind::ExportAssignment => {
                assert!(
                    !(private_name || !prefix.is_empty() || !suffix.is_empty()),
                    "Generated name for an export assignment cannot be private and may have neither a prefix nor suffix"
                );
                self.generate_name_for_export_default()
            }
            Kind::ClassExpression => {
                assert!(
                    !(private_name || !prefix.is_empty() || !suffix.is_empty()),
                    "Generated name for a class expression cannot be private and may have neither a prefix nor suffix"
                );
                self.generate_name_for_class_expression()
            }
            Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => {
                self.generate_name_for_method_or_accessor(node, private_name, prefix, suffix)
            }
            Kind::ComputedPropertyName => self.make_temp_variable_name(
                tempFlags::Auto,
                true, /*reservedInNestedScopes*/
                private_name,
                prefix,
                suffix,
            ),
            _ => self.make_temp_variable_name(
                tempFlags::Auto,
                false, /*reservedInNestedScopes*/
                private_name,
                prefix,
                suffix,
            ),
        }
    }

    fn generate_name_for_module_or_enum(
        &mut self,
        node: P<Node>, /* ModuleDeclaration | EnumDeclaration */
    ) -> String {
        let name = self.call_get_text_of_node(node.name().unwrap());
        // Use module/enum name itself if it is unique, otherwise make a unique variation
        if is_unique_local_name(&name, node) {
            name
        } else {
            self.make_unique_name(
                &name, None,  /*checkFn*/
                false, /*optimistic*/
                false, /*scoped*/
                false, /*privateName*/
                "",    /*prefix*/
                "",    /*suffix*/
            )
        }
    }

    fn generate_name_for_import_or_export_declaration(
        &mut self,
        node: P<Node>, /* ImportDeclaration | ExportDeclaration */
    ) -> String {
        let expr = get_external_module_name(node);
        let mut base_name = "module".to_string();
        if let Some(expr) = expr
            && is_string_literal(expr)
        {
            base_name = make_identifier_from_module_name(expr.text());
        }
        self.make_unique_name(
            &base_name, None,  /*checkFn*/
            false, /*optimistic*/
            false, /*scoped*/
            false, /*privateName*/
            "",    /*prefix*/
            "",    /*suffix*/
        )
    }

    fn generate_name_for_export_default(&mut self) -> String {
        self.make_unique_name(
            "default", None,  /*checkFn*/
            false, /*optimistic*/
            false, /*scoped*/
            false, /*privateName*/
            "",    /*prefix*/
            "",    /*suffix*/
        )
    }

    fn generate_name_for_class_expression(&mut self) -> String {
        self.make_unique_name(
            "class", None,  /*checkFn*/
            false, /*optimistic*/
            false, /*scoped*/
            false, /*privateName*/
            "",    /*prefix*/
            "",    /*suffix*/
        )
    }

    fn generate_name_for_method_or_accessor(
        &mut self,
        node: P<Node>, /* MethodDeclaration | AccessorDeclaration */
        private_name: bool,
        prefix: &str,
        suffix: &str,
    ) -> String {
        if is_identifier(node.name().unwrap()) {
            return self.generate_name_for_node_cached(
                node.name().unwrap(),
                private_name,
                GeneratedIdentifierFlags::None,
                prefix,
                suffix,
            );
        }
        self.make_temp_variable_name(
            tempFlags::Auto,
            false, /*reservedInNestedScopes*/
            private_name,
            prefix,
            suffix,
        )
    }

    fn make_name(&mut self, name: P<Node>) -> String {
        if let Some(context) = self.context
            && let Some(auto_generate) = context.get_auto_generate_info(Some(name))
        {
            match auto_generate.flags.kind() {
                GeneratedIdentifierFlags::Auto => {
                    return self.make_temp_variable_name(
                        tempFlags::Auto,
                        auto_generate.flags.is_reserved_in_nested_scopes(),
                        is_private_identifier(name),
                        auto_generate.prefix,
                        auto_generate.suffix,
                    );
                }
                GeneratedIdentifierFlags::Loop => {
                    assert!(is_identifier(name));
                    return self.make_temp_variable_name(
                        tempFlags::_i,
                        auto_generate.flags.is_reserved_in_nested_scopes(),
                        false, /*privateName*/
                        auto_generate.prefix,
                        auto_generate.suffix,
                    );
                }
                GeneratedIdentifierFlags::Unique => {
                    let check_fn = if auto_generate.flags.is_file_level() {
                        self.is_file_level_unique_name_in_current_file.clone()
                    } else {
                        None
                    };
                    return self.make_unique_name(
                        name.text(),
                        check_fn.as_ref(),
                        auto_generate.flags.is_optimistic(),
                        auto_generate.flags.is_reserved_in_nested_scopes(),
                        is_private_identifier(name),
                        auto_generate.prefix,
                        auto_generate.suffix,
                    );
                }
                _ => {}
            }
        }
        self.call_get_text_of_node(name)
    }

    // Return the next available name in the pattern _a ... _z, _0, _1, ...
    // TempFlags._i may be used to express a preference for that dedicated name.
    // Note that names generated by makeTempVariableName and makeUniqueName will never conflict.
    fn make_temp_variable_name(
        &mut self,
        flags: tempFlags,
        reserved_in_nested_scopes: bool,
        private_name: bool,
        prefix: &str,
        suffix: &str,
    ) -> String {
        let mut temp_flags;
        let mut key = String::new();
        let simple = prefix.is_empty() && suffix.is_empty();
        if simple {
            temp_flags = self.get_temp_flags(private_name);
        } else {
            // Generate a key to use to acquire a TempFlags counter based on the fixed portions of the generated name.
            key = format_generated_name(private_name, prefix, "" /*base*/, suffix);
            if private_name {
                key = ensure_leading_hash(&key);
            }
            temp_flags = self.get_temp_flags_for_formatted_name(private_name, &key);
        }

        if !flags.is_empty() && !temp_flags.intersects(flags) {
            let full_name = format_generated_name(private_name, prefix, "_i", suffix);
            if self.is_unique_name(&full_name, private_name) {
                temp_flags |= flags;
                self.reserve_name(
                    &full_name,
                    private_name,
                    reserved_in_nested_scopes,
                    true, /*temp*/
                );
                if simple {
                    self.set_temp_flags(private_name, temp_flags);
                } else {
                    self.set_temp_flags_for_formatted_name(private_name, &key, temp_flags);
                }
                return full_name;
            }
        }

        loop {
            let count = (temp_flags & tempFlags::CountMask).bits();
            temp_flags = tempFlags::from_bits_retain(temp_flags.bits() + 1);
            // Skip over 'i' and 'n'
            if count != 8 && count != 13 {
                let name = if count < 26 {
                    format!("_{}", (b'a' + count as u8) as char)
                } else {
                    format!("_{}", count - 26)
                };
                let full_name = format_generated_name(private_name, prefix, &name, suffix);
                if self.is_unique_name(&full_name, private_name) {
                    self.reserve_name(
                        &full_name,
                        private_name,
                        reserved_in_nested_scopes,
                        true, /*temp*/
                    );
                    if simple {
                        self.set_temp_flags(private_name, temp_flags);
                    } else {
                        self.set_temp_flags_for_formatted_name(private_name, &key, temp_flags);
                    }
                    return full_name;
                }
            }
        }
    }

    // Generate a name that is unique within the current file and doesn't conflict with any names
    // in global scope. The name is formed by adding an '_n' suffix to the specified base name,
    // where n is a positive integer. Note that names generated by makeTempVariableName and
    // makeUniqueName are guaranteed to never conflict.
    // If `optimistic` is set, the first instance will use 'baseName' verbatim instead of 'baseName_1'
    fn make_unique_name(
        &mut self,
        base_name: &str,
        check_fn: Option<&IsFileLevelUniqueNameFn>,
        optimistic: bool,
        scoped: bool,
        private_name: bool,
        prefix: &str,
        suffix: &str,
    ) -> String {
        let mut base_name = remove_leading_hash(base_name).to_string();
        if optimistic {
            let full_name = format_generated_name(private_name, prefix, &base_name, suffix);
            if self.check_unique_name(&full_name, private_name, check_fn) {
                self.reserve_name(&full_name, private_name, scoped, false /*temp*/);
                return full_name;
            }
        }

        // Find the first unique 'name_n', where n is a positive integer
        if !base_name.is_empty() && base_name.as_bytes()[base_name.len() - 1] != b'_' {
            base_name.push('_');
        }

        let mut i = 1;
        loop {
            let full_name =
                format_generated_name(private_name, prefix, &format!("{base_name}{i}"), suffix);
            if self.check_unique_name(&full_name, private_name, check_fn) {
                self.reserve_name(&full_name, private_name, scoped, false /*temp*/);
                return full_name;
            }
            i += 1;
        }
    }

    pub(crate) fn make_file_level_optimistic_unique_name(&mut self, name: &str) -> String {
        let check_fn = self.is_file_level_unique_name_in_current_file.clone();
        self.make_unique_name(
            name,
            check_fn.as_ref(),
            true,  /*optimistic*/
            false, /*scoped*/
            false, /*privateName*/
            "",    /*prefix*/
            "",    /*suffix*/
        )
    }

    fn check_unique_name(
        &mut self,
        name: &str,
        private_name: bool,
        check_fn: Option<&IsFileLevelUniqueNameFn>,
    ) -> bool {
        if let Some(check_fn) = check_fn {
            check_fn(name, private_name)
        } else {
            self.is_unique_name(name, private_name)
        }
    }

    fn is_unique_name(&mut self, name: &str, private_name: bool) -> bool {
        (self.is_file_level_unique_name_in_current_file.is_none()
            || (self.is_file_level_unique_name_in_current_file.as_ref().unwrap())(
                name,
                private_name,
            ))
            && !self.is_reserved_name(name, private_name)
    }

    fn is_reserved_name(&mut self, name: &str, private_name: bool) -> bool {
        // NOTE: The following matches Strada, but is incorrect.
        if self.generated_names.contains(name) {
            return true;
        }

        // TODO: generated names should be scoped after Strada port is complete.
        ////if *scope != nil {
        ////	if (*scope).generatedNames.Has(name) {
        ////		return true
        ////	}
        ////}

        let scope = self.get_scope(private_name).as_deref();
        std::iter::successors(scope, |s| s.next.as_deref()).any(|s| s.reserved_names.contains(name))
    }
}

fn next_container(node: P<Node>) -> Option<P<Node>> {
    let data = node.locals_container_data();
    if let Some(data) = data {
        return data.next_container.get();
    }
    None
}

fn is_unique_local_name(name: &str, container: P<Node>) -> bool {
    let mut node = Some(container);
    while let Some(n) = node {
        if !(is_node_descendant_of(n, container) && n.locals_container_data().is_some()) {
            break;
        }
        let locals = n.locals();
        if let Some(locals) = locals {
            // We conservatively include alias symbols to cover cases where they're emitted as locals
            if let Some(local) = locals.lookup(name)
                && local
                    .flags
                    .get()
                    .intersects(SymbolFlags::Value | SymbolFlags::ExportValue | SymbolFlags::Alias)
            {
                return false;
            }
        }
        node = next_container(n);
    }
    true
}
