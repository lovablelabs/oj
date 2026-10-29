use std::collections::hash_map::Entry;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

use crate::*;

// TODO: Memoize once per checker to retain threadsafety
// printer.go:13
fn create_printer_with_defaults(emit_context: P<EmitContext>) -> Printer {
    new_printer(PrinterOptions::default(), PrintHandlers::default(), Some(emit_context))
}

// printer.go:17
fn create_printer_with_remove_comments(emit_context: P<EmitContext>) -> Printer {
    new_printer(
        PrinterOptions { remove_comments: true, ..Default::default() },
        PrintHandlers::default(),
        Some(emit_context),
    )
}

// printer.go:21
fn create_printer_with_remove_comments_omit_trailing_semicolon(
    emit_context: P<EmitContext>,
) -> Printer {
    new_printer(
        PrinterOptions {
            remove_comments: true,
            omit_trailing_semicolon: true,
            ..Default::default()
        },
        PrintHandlers::default(),
        Some(emit_context),
    )
}

// printer.go:28
fn create_printer_with_remove_comments_omit_trailing_semicolon_never_ascii_escape(
    emit_context: P<EmitContext>,
) -> Printer {
    new_printer(
        PrinterOptions {
            remove_comments: true,
            omit_trailing_semicolon: true,
            never_ascii_escape: true,
            ..Default::default()
        },
        PrintHandlers::default(),
        Some(emit_context),
    )
}

impl Checker {
    // printer.go:43
    pub fn type_to_string_exported(&mut self, t: P<Type>) -> String {
        self.type_to_string(t, None)
    }

    // printer.go:47
    pub(crate) fn type_to_string(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
    ) -> String {
        self.type_to_string_ex(
            t,
            enclosing_declaration,
            TypeFormatFlags::AllowUniqueESSymbolType
                | TypeFormatFlags::UseAliasDefinedOutsideCurrentScope,
            None,
        )
    }
}

// printer.go:51
fn to_node_builder_flags(flags: TypeFormatFlags) -> Flags {
    Flags::from_bits_retain((flags & TypeFormatFlags::NodeBuilderFlagsMask).bits())
}

impl Checker {
    // printer.go:55
    pub fn type_to_string_ex_exported(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: TypeFormatFlags,
        vc: Option<P<VerbosityContext>>,
    ) -> String {
        self.type_to_string_ex(t, enclosing_declaration, flags, vc)
    }

    // printer.go:59
    pub fn type_to_string_ex(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: TypeFormatFlags,
        vc: Option<P<VerbosityContext>>,
    ) -> String {
        // Serialization of types can lead to (lazy) resolution of members, which can cause diagnostics that again require
        // serialization of types. This can potentially result in infinite recursion and stack overflows. To prevent that,
        // after a certain number of recursive invocations the function simply returns "?".
        if self.serialization_level >= MAX_SERIALIZATION_LEVEL {
            return "?".to_string();
        }
        let mut new_line = "";
        if flags.intersects(TypeFormatFlags::MultilineObjectLiterals) {
            new_line = "\n";
        }
        let mut writer = new_text_writer(new_line, 0);
        let no_truncation = ((vc.is_none() || vc.unwrap().max_truncation_length.get() == 0)
            && self.compiler_options.no_error_truncation == Tristate::True)
            || flags.intersects(TypeFormatFlags::NoTruncation);
        let mut combined_flags = to_node_builder_flags(flags) | Flags::IgnoreErrors;
        if no_truncation {
            combined_flags |= Flags::NoTruncation;
        }
        let (node_builder, mut release) = self.get_node_builder();
        let old_verbosity = node_builder.verbosity.get();
        node_builder.verbosity.set(vc);
        self.serialization_level += 1;
        let type_node = node_builder.type_to_type_node(
            self,
            t,
            enclosing_declaration,
            combined_flags,
            InternalFlags::None,
            None,
        );
        self.serialization_level -= 1;
        let Some(type_node) = type_node else {
            panic!("should always get typenode");
        };
        // The unresolved type gets a synthesized comment on `any` to hint to users that it's not a plain `any`.
        // Otherwise, we always strip comments out.
        let mut p = if t == self.unresolved_type {
            create_printer_with_defaults(node_builder.emit_context())
        } else {
            create_printer_with_remove_comments(node_builder.emit_context())
        };
        let mut source_file = None;
        if let Some(enclosing_declaration) = enclosing_declaration {
            source_file = ast::get_source_file_of_node(enclosing_declaration);
        }
        p.write(type_node, source_file, &mut *writer, None);
        let result = writer.string();

        let mut max_length = DEFAULT_MAXIMUM_TRUNCATION_LENGTH * 2;
        if let Some(vc) = vc
            && vc.max_truncation_length.get() > 0
        {
            max_length = vc.max_truncation_length.get() * 10; // hard cutoff matching Strada's absoluteMaximumLength
        }
        if no_truncation {
            max_length = NO_TRUNCATION_MAXIMUM_TRUNCATION_LENGTH * 2;
        }
        // Go's deferred calls: restore the verbosity, then release.
        node_builder.verbosity.set(old_verbosity);
        release(self);
        if max_length > 0 && !result.is_empty() && result.len() >= max_length {
            if let Some(vc) = vc {
                vc.truncated.set(true);
            }
            // Go slices bytes; a cut inside a multi-byte character yields U+FFFD here instead of the raw bytes.
            let cut = &result.as_bytes()[0..max_length - "...".len()];
            return tsrs_core::utf8::from_utf8_lossy(cut).into_owned() + "...";
        }
        result
    }

    // printer.go:120
    pub fn symbol_to_string_exported(&mut self, s: P<Symbol>) -> String {
        self.symbol_to_string(s)
    }

    // printer.go:124
    pub(crate) fn symbol_to_string(&mut self, symbol: P<Symbol>) -> String {
        self.symbol_to_string_ex(
            symbol,
            None,
            SymbolFlags::All,
            SymbolFormatFlags::AllowAnyNodeKind,
        )
    }

    // printer.go:128
    pub fn symbol_to_string_ex_exported(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        flags: SymbolFormatFlags,
    ) -> String {
        self.symbol_to_string_ex(symbol, enclosing_declaration, meaning, flags)
    }

    // printer.go:132
    pub fn symbol_to_string_ex(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        flags: SymbolFormatFlags,
    ) -> String {
        let (mut writer, put_writer) = get_single_line_string_writer();

        let mut node_flags = Flags::IgnoreErrors;
        let mut internal_node_flags = InternalFlags::None;
        if flags.intersects(SymbolFormatFlags::UseOnlyExternalAliasing) {
            node_flags |= Flags::UseOnlyExternalAliasing;
        }
        if flags.intersects(SymbolFormatFlags::WriteTypeParametersOrArguments) {
            node_flags |= Flags::WriteTypeParametersInQualifiedName;
        }
        if flags.intersects(SymbolFormatFlags::UseAliasDefinedOutsideCurrentScope) {
            node_flags |= Flags::UseAliasDefinedOutsideCurrentScope;
        }
        if flags.intersects(SymbolFormatFlags::DoNotIncludeSymbolChain) {
            internal_node_flags |= InternalFlags::DoNotIncludeSymbolChain;
        }
        if flags.intersects(SymbolFormatFlags::WriteComputedProps) {
            internal_node_flags |= InternalFlags::WriteComputedProps;
        }

        let (node_builder, mut release) = self.get_node_builder();
        let mut source_file = None;
        if let Some(enclosing_declaration) = enclosing_declaration {
            source_file = ast::get_source_file_of_node(enclosing_declaration);
        }
        // add neverAsciiEscape for GH#39027
        let mut printer_ = if enclosing_declaration.is_some_and(|e| e.kind() == Kind::SourceFile) {
            create_printer_with_remove_comments_omit_trailing_semicolon_never_ascii_escape(
                node_builder.emit_context(),
            )
        } else {
            create_printer_with_remove_comments_omit_trailing_semicolon(node_builder.emit_context())
        };

        let entity = if flags.intersects(SymbolFormatFlags::AllowAnyNodeKind) {
            node_builder.symbol_to_node(
                self,
                symbol,
                meaning,
                enclosing_declaration,
                node_flags,
                internal_node_flags,
                None,
            ) // TODO: GH#18217
        } else {
            node_builder.symbol_to_entity_name(
                self,
                symbol,
                meaning,
                enclosing_declaration,
                node_flags,
                internal_node_flags,
                None,
            ) // TODO: GH#18217
        };
        printer_.write(entity.unwrap() /*sourceFile*/, source_file, &mut *writer, None); // TODO: GH#18217
        let result = writer.string();
        release(self);
        put_writer();
        result
    }

    // printer.go:179
    pub(crate) fn signature_to_string(&mut self, signature: P<Signature>) -> String {
        self.signature_to_string_ex(signature, None, TypeFormatFlags::None, None)
    }

    // printer.go:187
    pub fn signature_to_string_ex(
        &mut self,
        signature: P<Signature>,
        enclosing_declaration: Option<P<Node>>,
        flags: TypeFormatFlags,
        vc: Option<P<VerbosityContext>>,
    ) -> String {
        let is_constructor = signature.flags.get().intersects(SignatureFlags::Construct)
            && !flags.intersects(TypeFormatFlags::WriteCallStyleSignature);
        let sig_output = if flags.intersects(TypeFormatFlags::WriteArrowStyleSignature) {
            if is_constructor { Kind::ConstructorType } else { Kind::FunctionType }
        } else if is_constructor {
            Kind::ConstructSignature
        } else {
            Kind::CallSignature
        };

        let (node_builder, mut release) = self.get_node_builder();
        let old_verbosity = node_builder.verbosity.get();
        node_builder.verbosity.set(vc);
        let combined_flags = to_node_builder_flags(flags)
            | Flags::IgnoreErrors
            | Flags::WriteTypeParametersInQualifiedName;
        let sig = node_builder.signature_to_signature_declaration(
            self,
            signature,
            sig_output,
            enclosing_declaration,
            combined_flags,
            InternalFlags::None,
            None,
        );
        let mut p = create_printer_with_remove_comments_omit_trailing_semicolon_never_ascii_escape(
            node_builder.emit_context(),
        );
        let mut source_file = None;
        if let Some(enclosing_declaration) = enclosing_declaration {
            source_file = ast::get_source_file_of_node(enclosing_declaration);
        }
        let result = if flags.intersects(TypeFormatFlags::MultilineObjectLiterals) {
            let mut writer = new_text_writer("\n", 0);
            p.write(sig.unwrap(), source_file, &mut *writer, None);
            writer.string()
        } else {
            let (mut writer, put_writer) = get_single_line_string_writer();
            p.write(sig.unwrap(), source_file, &mut *writer, None);
            let result = writer.string();
            put_writer();
            result
        };
        node_builder.verbosity.set(old_verbosity);
        release(self);
        result
    }

    // printer.go:229
    pub(crate) fn type_predicate_to_string(&mut self, type_predicate: P<TypePredicate>) -> String {
        self.type_predicate_to_string_ex(
            type_predicate,
            None,
            TypeFormatFlags::UseAliasDefinedOutsideCurrentScope,
        )
    }

    // printer.go:233
    fn type_predicate_to_string_ex(
        &mut self,
        type_predicate: P<TypePredicate>,
        enclosing_declaration: Option<P<Node>>,
        flags: TypeFormatFlags,
    ) -> String {
        let (mut writer, put_writer) = get_single_line_string_writer();
        let (node_builder, mut release) = self.get_node_builder();
        let combined_flags = to_node_builder_flags(flags)
            | Flags::IgnoreErrors
            | Flags::WriteTypeParametersInQualifiedName;
        let predicate = node_builder.type_predicate_to_type_predicate_node(
            self,
            type_predicate,
            enclosing_declaration,
            combined_flags,
            InternalFlags::None,
            None,
        ); // TODO: GH#18217
        let mut printer_ = create_printer_with_remove_comments(node_builder.emit_context());
        let mut source_file = None;
        if let Some(enclosing_declaration) = enclosing_declaration {
            source_file = ast::get_source_file_of_node(enclosing_declaration);
        }
        printer_.write(predicate.unwrap() /*sourceFile*/, source_file, &mut *writer, None);
        let result = writer.string();
        release(self);
        put_writer();
        result
    }

    // printer.go:249
    pub(crate) fn value_to_string(value: LiteralValue) -> String {
        crate::value_to_string(value)
    }

    // printer.go:253
    pub(crate) fn format_union_types(
        &mut self,
        types: &[P<Type>],
        expanding_enum: bool,
    ) -> Vec<P<Type>> {
        let mut result = Vec::new();
        let mut flags = TypeFlags::None;
        let mut i = 0;
        while i < types.len() {
            let t = types[i];
            flags |= t.flags();
            if !t.flags().intersects(TypeFlags::Nullable) {
                if t.flags().intersects(TypeFlags::BooleanLiteral)
                    || (!expanding_enum && t.flags().intersects(TypeFlags::EnumLike))
                {
                    let base_type = if t.flags().intersects(TypeFlags::BooleanLiteral) {
                        self.boolean_type
                    } else {
                        self.get_base_type_of_enum_like_type(t)
                    };
                    if base_type.flags().intersects(TypeFlags::Union) {
                        let base_types = base_type.as_union_type().types.get();
                        let count = base_types.len();
                        if i + count <= types.len()
                            && self.get_regular_type_of_literal_type(types[i + count - 1])
                                == self.get_regular_type_of_literal_type(base_types[count - 1])
                        {
                            result.push(base_type);
                            i += count - 1;
                            i += 1;
                            continue;
                        }
                    }
                }
                result.push(t);
            }
            i += 1;
        }
        if flags.intersects(TypeFlags::Null) {
            result.push(self.null_type);
        }
        if flags.intersects(TypeFlags::Undefined) {
            result.push(self.undefined_type);
        }
        result
    }

    // printer.go:288
    pub fn type_to_type_node(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
    ) -> Option<P<Node>> {
        let node_builder = self.get_node_builder_ex(id_to_symbol);
        node_builder.type_to_type_node(
            self,
            t,
            enclosing_declaration,
            flags,
            InternalFlags::None,
            None,
        )
    }

    // printer.go:293
    pub fn signature_to_signature_declaration(
        &mut self,
        signature: P<Signature>,
        kind: Kind,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
    ) -> Option<P<Node>> {
        let (node_builder, mut release) = self.get_node_builder();
        let result = node_builder.signature_to_signature_declaration(
            self,
            signature,
            kind,
            enclosing_declaration,
            flags,
            InternalFlags::None,
            None,
        );
        release(self);
        result
    }

    // ExpandSymbolForHover produces declaration strings for a symbol with verbosity support for expandable hover.
    // printer.go:300
    pub fn expand_symbol_for_hover(
        &mut self,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        vc: Option<P<VerbosityContext>>,
    ) -> String {
        let (node_builder, mut release) = self.get_node_builder();
        let old_verbosity = node_builder.verbosity.get();
        node_builder.verbosity.set(vc);
        let nodes = node_builder.expand_symbol_for_hover(self, symbol, meaning);
        if nodes.is_empty() {
            node_builder.verbosity.set(old_verbosity);
            release(self);
            return String::new();
        }
        let mut p = create_printer_with_remove_comments(node_builder.emit_context());
        let mut source_file = None;
        if let Some(value_declaration) = symbol.value_declaration() {
            source_file = ast::get_source_file_of_node(value_declaration);
        }
        let mut b = String::new();
        for (i, &node) in nodes.iter().enumerate() {
            if i > 0 {
                b.push('\n');
            }
            b.push_str(&p.emit(node, source_file));
        }
        node_builder.verbosity.set(old_verbosity);
        release(self);
        b
    }

    // TypeParameterToStringEx renders a type parameter declaration (e.g. "T extends Foo") with optional verbosity support.
    // printer.go:328
    pub fn type_parameter_to_string_ex(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        vc: Option<P<VerbosityContext>>,
    ) -> String {
        let (node_builder, mut release) = self.get_node_builder();
        let old_verbosity = node_builder.verbosity.get();
        node_builder.verbosity.set(vc);
        let type_param_node = node_builder.type_parameter_to_declaration(
            self,
            t,
            enclosing_declaration,
            Flags::IgnoreErrors,
            InternalFlags::None,
            None,
        );
        let result = match type_param_node {
            None => self.type_to_string_exported(t),
            Some(type_param_node) => {
                let mut p = create_printer_with_remove_comments(node_builder.emit_context());
                let mut source_file = None;
                if let Some(enclosing_declaration) = enclosing_declaration {
                    source_file = ast::get_source_file_of_node(enclosing_declaration);
                }
                p.emit(type_param_node, source_file)
            }
        };
        node_builder.verbosity.set(old_verbosity);
        release(self);
        result
    }

    // printer.go:348
    pub fn type_to_type_node_ex(
        &mut self,
        t: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
    ) -> Option<P<Node>> {
        let node_builder = self.get_node_builder_ex(id_to_symbol);
        node_builder.type_to_type_node(self, t, enclosing_declaration, flags, internal_flags, None)
    }

    // printer.go:353
    pub fn type_predicate_to_type_predicate_node(
        &mut self,
        t: P<TypePredicate>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
    ) -> Option<P<Node>> {
        let node_builder = self.get_node_builder_ex(id_to_symbol);
        node_builder.type_predicate_to_type_predicate_node(
            self,
            t,
            enclosing_declaration,
            flags,
            InternalFlags::None,
            None,
        )
    }
}

// ---- symbolaccessibility.go ----

impl Checker {
    // symbolaccessibility.go:11
    pub(crate) fn is_type_symbol_accessible(
        &mut self,
        type_symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        let access = self.is_symbol_accessible_worker(
            Some(type_symbol),
            enclosing_declaration,
            SymbolFlags::Type, /*shouldComputeAliasesToMakeVisible*/
            false,             /*allowModules*/
            true,
        );
        access.accessibility == SymbolAccessibility::Accessible
    }

    // symbolaccessibility.go:16
    pub(crate) fn is_value_symbol_accessible(
        &mut self,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        let access = self.is_symbol_accessible_worker(
            symbol,
            enclosing_declaration,
            SymbolFlags::Value,
            false, /*shouldComputeAliasesToMakeVisible*/
            true,  /*allowModules*/
        );
        access.accessibility == SymbolAccessibility::Accessible
    }

    // symbolaccessibility.go:21
    pub(crate) fn is_symbol_accessible_by_flags(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        flags: SymbolFlags,
    ) -> bool {
        let access = self.is_symbol_accessible_worker(
            Some(symbol),
            enclosing_declaration,
            flags, /*shouldComputeAliasesToMakeVisible*/
            false, /*allowModules*/
            false,
        ); // TODO: Strada bug? Why is this allowModules: false?
        access.accessibility == SymbolAccessibility::Accessible
    }

    // symbolaccessibility.go:26
    fn is_any_symbol_accessible(
        &mut self,
        symbols: &[P<Symbol>],
        enclosing_declaration: Option<P<Node>>,
        initial_symbol: P<Symbol>,
        meaning: SymbolFlags,
        should_compute_aliases_to_make_visible: bool,
        allow_modules: bool,
    ) -> Option<P<SymbolAccessibilityResult>> {
        if symbols.is_empty() {
            return None;
        }

        let mut had_accessible_chain: Option<P<Symbol>> = None;
        let mut early_module_bail = false;
        for &symbol in symbols {
            // Symbol is accessible if it by itself is accessible
            let accessible_symbol_chain = self.get_accessible_symbol_chain(
                symbol,
                enclosing_declaration,
                meaning, /*useOnlyExternalAliasing*/
                false,
            );
            if !accessible_symbol_chain.is_empty() {
                had_accessible_chain = Some(symbol);
                // TODO: going through emit resolver here is weird. Relayer these APIs.
                let r = self.get_emit_resolver();
                let has_accessible_declarations = r.has_visible_declarations(
                    self,
                    accessible_symbol_chain[0],
                    should_compute_aliases_to_make_visible,
                );
                if has_accessible_declarations.is_some() {
                    return has_accessible_declarations;
                }
            }
            if allow_modules {
                let has_module_declaration = symbol
                    .declarations()
                    .iter()
                    .any(|&d| has_non_global_augmentation_external_module_symbol(d));
                if has_module_declaration {
                    if should_compute_aliases_to_make_visible {
                        early_module_bail = true;
                        // Generally speaking, we want to use the aliases that already exist to refer to a module, if present
                        // In order to do so, we need to find those aliases in order to retain them in declaration emit; so
                        // if we are in declaration emit, we cannot use the fast path for module visibility until we've exhausted
                        // all other visibility options (in order to capture the possible aliases used to reference the module)
                        continue;
                    }
                    // Any meaning of a module symbol is always accessible via an `import` type
                    return Some(P::new_scratch(SymbolAccessibilityResult {
                        accessibility: SymbolAccessibility::Accessible,
                        ..Default::default()
                    }));
                }
            }

            // If we haven't got the accessible symbol, it doesn't mean the symbol is actually inaccessible.
            // It could be a qualified symbol and hence verify the path
            // e.g.:
            // module m {
            //     export class c {
            //     }
            // }
            // const x: typeof m.c
            // In the above example when we start with checking if typeof m.c symbol is accessible,
            // we are going to see if c can be accessed in scope directly.
            // But it can't, hence the accessible is going to be undefined, but that doesn't mean m.c is inaccessible
            // It is accessible if the parent m is accessible because then m.c can be accessed through qualification

            let containers = self.get_containers_of_symbol(symbol, enclosing_declaration, meaning);
            let mut next_meaning = meaning;
            if initial_symbol == symbol {
                next_meaning = get_qualified_left_meaning(meaning);
            }
            let parent_result = self.is_any_symbol_accessible(
                &containers,
                enclosing_declaration,
                initial_symbol,
                next_meaning,
                should_compute_aliases_to_make_visible,
                allow_modules,
            );
            if parent_result.is_some() {
                return parent_result;
            }
        }

        if early_module_bail {
            return Some(P::new_scratch(SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::Accessible,
                ..Default::default()
            }));
        }

        if let Some(had_accessible_chain) = had_accessible_chain {
            let mut module_name = String::new();
            if had_accessible_chain != initial_symbol {
                module_name = self.symbol_to_string_ex(
                    had_accessible_chain,
                    enclosing_declaration,
                    SymbolFlags::Namespace,
                    SymbolFormatFlags::AllowAnyNodeKind,
                );
            }
            return Some(P::new_scratch(SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::NotAccessible,
                error_symbol_name: self.symbol_to_string_ex(
                    initial_symbol,
                    enclosing_declaration,
                    meaning,
                    SymbolFormatFlags::AllowAnyNodeKind,
                ),
                error_module_name: module_name,
                ..Default::default()
            }));
        }
        None
    }
}

// symbolaccessibility.go:105
pub(crate) fn has_non_global_augmentation_external_module_symbol(declaration: P<Node>) -> bool {
    ast::is_module_with_string_literal_name(declaration)
        || (declaration.kind() == Kind::SourceFile
            && ast::is_external_or_common_js_module(declaration.as_source_file_p()))
}

// symbolaccessibility.go:109
pub(crate) fn get_qualified_left_meaning(right_meaning: SymbolFlags) -> SymbolFlags {
    // If we are looking in value space, the parent meaning is value, other wise it is namespace
    if right_meaning == SymbolFlags::Value {
        return SymbolFlags::Value;
    }
    SymbolFlags::Namespace
}

impl Checker {
    // symbolaccessibility.go:117
    fn get_with_alternative_containers(
        &mut self,
        container: P<Symbol>,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> Vec<P<Symbol>> {
        let container_declarations = container.declarations().to_vec();
        let mut additional_containers = Vec::new();
        additional_containers.extend(container_declarations.iter().filter_map(|&d| {
            self.get_file_symbol_if_file_symbol_export_equals_container(d, container)
        }));
        let mut reexport_containers = Vec::new();
        if enclosing_declaration.is_some() {
            reexport_containers =
                self.get_alternative_containing_modules(symbol, enclosing_declaration);
        }
        let object_literal_container =
            self.get_variable_declaration_of_object_literal(container, meaning);
        let left_meaning = get_qualified_left_meaning(meaning);
        if enclosing_declaration.is_some()
            && container.flags().intersects(left_meaning)
            && !self
                .get_accessible_symbol_chain(
                    container,
                    enclosing_declaration,
                    SymbolFlags::Namespace, /*useOnlyExternalAliasing*/
                    false,
                )
                .is_empty()
        {
            // This order expresses a preference for the real container if it is in scope
            let mut res = vec![container];
            res.extend(additional_containers);
            res.extend(reexport_containers);
            if let Some(object_literal_container) = object_literal_container {
                res.push(object_literal_container);
            }
            return res;
        }
        // we potentially have a symbol which is a member of the instance side of something - look for a variable in scope with the container's type
        // which may be acting like a namespace (eg, `Symbol` acts like a namespace when looking up `Symbol.toStringTag`)
        let mut variable_matches: Vec<P<Symbol>> = Vec::new();
        if (meaning == SymbolFlags::Value && !container.flags().intersects(left_meaning))
            && container.flags().intersects(SymbolFlags::Type)
            && self.get_declared_type_of_symbol(container).flags().intersects(TypeFlags::Object)
        {
            self.some_symbol_table_in_scope(enclosing_declaration, |c, t, _, _, _, _| {
                let mut found = false;
                if let Some(t) = t {
                    for s in t.values() {
                        if s.flags().intersects(left_meaning)
                            && c.get_type_of_symbol(s) == c.get_declared_type_of_symbol(container)
                        {
                            variable_matches.push(s);
                            found = true;
                        }
                    }
                }
                found
            });
            self.sort_symbols(&mut variable_matches);
        }

        let mut res = Vec::new();
        res.extend(variable_matches);
        res.extend(additional_containers);
        res.push(container);
        if let Some(object_literal_container) = object_literal_container {
            res.push(object_literal_container);
        }
        res.extend(reexport_containers);
        res
    }

    // symbolaccessibility.go:168
    fn get_alternative_containing_modules(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
    ) -> Vec<P<Symbol>> {
        let Some(enclosing_declaration) = enclosing_declaration else {
            return Vec::new();
        };
        let containing_file = ast::get_source_file_of_node(enclosing_declaration).unwrap();
        let id = ast::get_node_id(containing_file.as_node());
        let links = self.symbol_container_links.get(symbol);
        let existing = links.extended_containers_by_file.borrow().get(&id).copied();
        if let Some(existing) = existing {
            return existing.to_vec();
        }
        let mut results: Vec<P<Symbol>> = Vec::new();
        if !containing_file.imports().is_empty() {
            // Try to make an import using an import already in the enclosing file, if possible
            results.extend(containing_file.imports().iter().filter_map(|&import_ref| {
                if ast::node_is_synthesized(import_ref) {
                    // Synthetic names can't be resolved by `resolveExternalModuleName` - they'll cause a debug assert if they error
                    return None;
                }
                let import_attributes_type =
                    self.get_import_attributes_type_for_module_specifier(import_ref);
                let resolved_module = self.resolve_external_module_name(
                    enclosing_declaration,
                    import_ref, /*ignoreErrors*/
                    true,
                    import_attributes_type,
                );
                let resolved_module = resolved_module?;
                let ref_ = self.get_alias_for_symbol_in_container(resolved_module, symbol);
                ref_?;
                Some(resolved_module)
            }));
            if !results.is_empty() {
                links.extended_containers_by_file.borrow_mut().insert(id, alloc_slice(&results));
                return results;
            }
        }

        if let Some(extended_containers) = links.extended_containers.get() {
            return extended_containers.to_vec();
        }
        // No results from files already being imported by this file - expand search (expensive, but not location-specific, so cached)
        if let Some(found) = self.modules_exporting(symbol) {
            results = found;
        } else {
            let other_files = self.program.source_files();
            results.extend(other_files.iter().filter_map(|&file| {
                if !ast::is_external_module(file) {
                    return None;
                }
                // tsrs-only: a check leaf another checker may have freed; its answer is nil for any symbol this
                // checker can name here. Its export table holds only symbols declared in it (no alias, `export *`
                // or `export =`) and nothing outside it refers to it, so `symbol`, declared elsewhere, is neither
                // its export nor re-exported by it (`is_unreadable_check_leaf`).
                if self.is_unreadable_check_leaf(file) {
                    return None;
                }
                let sym = self.get_symbol_of_declaration(file.as_node()).unwrap();
                let ref_ = self.get_alias_for_symbol_in_container(sym, symbol);
                ref_?;
                Some(sym)
            }));
        }
        links.extended_containers.set(Some(alloc_slice(&results)));
        results
    }

    // symbolaccessibility.go:226
    fn get_variable_declaration_of_object_literal(
        &mut self,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        // If we're trying to reference some object literal in, eg `var a = { x: 1 }`, the symbol for the literal, `__object`, is distinct
        // from the symbol of the declaration it is being assigned to. Since we can use the declaration to refer to the literal, however,
        // we'd like to make that connection here - potentially causing us to paint the declaration's visibility, and therefore the literal.
        if !meaning.intersects(SymbolFlags::Value) {
            return None;
        }
        let first_decl = {
            let declarations = symbol.declarations();
            if declarations.is_empty() {
                return None;
            }
            declarations[0]
        };
        let parent = first_decl.parent()?;
        if !ast::is_variable_declaration(parent) {
            return None;
        }
        if ast::is_object_literal_expression(first_decl) && Some(first_decl) == parent.initializer()
            || ast::is_type_literal_node(first_decl) && Some(first_decl) == parent.type_node()
        {
            return self.get_symbol_of_declaration(parent);
        }
        None
    }
}

// symbolaccessibility.go:249
fn has_external_module_symbol(declaration: P<Node>) -> bool {
    ast::is_ambient_module(declaration)
        || (declaration.kind() == Kind::SourceFile
            && ast::is_external_or_common_js_module(declaration.as_source_file_p()))
}

impl Checker {
    // symbolaccessibility.go:253
    fn get_external_module_container(&mut self, declaration: P<Node>) -> Option<P<Symbol>> {
        let node = ast::find_ancestor(Some(declaration), has_external_module_symbol)?;
        self.get_symbol_of_declaration(node)
    }

    // symbolaccessibility.go:261
    pub(crate) fn get_file_symbol_if_file_symbol_export_equals_container(
        &mut self,
        d: P<Node>,
        container: P<Symbol>,
    ) -> Option<P<Symbol>> {
        let file_symbol = self.get_external_module_container(d)?;
        let exports = file_symbol.exports()?;
        let exported = exports.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS)?;
        if self.get_symbol_if_same_reference(exported, container).is_some() {
            return Some(file_symbol);
        }
        None
    }

    /**
     * Attempts to find the symbol corresponding to the container a symbol is in - usually this
     * is just its' `.parent`, but for locals, this value is `undefined`
     */
    // symbolaccessibility.go:280
    pub(crate) fn get_containers_of_symbol(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> Vec<P<Symbol>> {
        let container = self.get_parent_of_symbol(symbol);
        // Type parameters end up in the `members` lists but are not externally visible
        if let Some(container) = container
            && !symbol.flags().intersects(SymbolFlags::TypeParameter)
        {
            return self.get_with_alternative_containers(
                container,
                symbol,
                enclosing_declaration,
                meaning,
            );
        }
        let mut candidates: Vec<P<Symbol>> = Vec::new();
        let declarations = symbol.declarations().to_vec();
        for d in declarations {
            if !ast::is_ambient_module(d)
                && let Some(parent) = d.parent()
            {
                // direct children of a module
                if has_non_global_augmentation_external_module_symbol(parent) {
                    let sym = self.get_symbol_of_declaration(parent);
                    if let Some(sym) = sym
                        && !candidates.contains(&sym)
                    {
                        candidates.push(sym);
                    }
                    continue;
                }
                // export ='d member of an ambient module
                if ast::is_module_block(parent) && parent.parent().is_some() && {
                    let module_symbol = self.get_symbol_of_declaration(parent.parent().unwrap());
                    module_symbol
                        .is_some_and(|m| self.resolve_external_module_symbol(m, false) == symbol)
                } {
                    let sym = self.get_symbol_of_declaration(parent.parent().unwrap());
                    if let Some(sym) = sym
                        && !candidates.contains(&sym)
                    {
                        candidates.push(sym);
                    }
                    continue;
                }
            }
            if ast::is_class_expression(d) && ast::is_binary_expression(d.parent().unwrap()) && {
                let binary = d.parent().unwrap();
                let binary = binary.as_binary_expression();
                binary.operator_token().kind() == Kind::EqualsToken
                    && ast::is_access_expression(binary.left())
                    && ast::is_entity_name_expression(binary.left().expression().unwrap())
            } {
                let left = d.parent().unwrap().as_binary_expression().left();
                if ast::is_module_exports_access_expression(left)
                    || ast::is_exports_identifier(left.expression().unwrap())
                {
                    let sym = self.get_symbol_of_declaration(
                        ast::get_source_file_of_node(d).unwrap().as_node(),
                    );
                    if let Some(sym) = sym
                        && !candidates.contains(&sym)
                    {
                        candidates.push(sym);
                    }
                    continue;
                }
                self.check_expression_cached(left.expression().unwrap());
                let sym =
                    self.symbol_node_links.get(left.expression().unwrap()).resolved_symbol.get();
                if let Some(sym) = sym
                    && !candidates.contains(&sym)
                {
                    candidates.push(sym);
                }
            }
        }
        if candidates.is_empty() {
            return Vec::new();
        }

        let mut best_containers: Vec<P<Symbol>> = Vec::new();
        let mut alternative_containers: Vec<P<Symbol>> = Vec::new();
        for container in candidates {
            if self.get_alias_for_symbol_in_container(container, symbol).is_none() {
                continue;
            }
            let all_alts = self.get_with_alternative_containers(
                container,
                symbol,
                enclosing_declaration,
                meaning,
            );
            if all_alts.is_empty() {
                continue;
            }
            best_containers.push(all_alts[0]);
            alternative_containers.extend_from_slice(&all_alts[1..]);
        }
        best_containers.extend(alternative_containers);
        best_containers
    }

    // symbolaccessibility.go:342
    pub(crate) fn get_alias_for_symbol_in_container(
        &mut self,
        container: P<Symbol>,
        symbol: P<Symbol>,
    ) -> Option<P<Symbol>> {
        if Some(container) == self.get_parent_of_symbol(symbol) {
            // fast path, `symbol` is either already the alias or isn't aliased
            return Some(symbol);
        }
        // Check if container is a thing with an `export=` which points directly at `symbol`, and if so, return
        // the container itself as the alias for the symbol
        if let Some(container_exports) = container.exports()
            && let Some(export_equals) =
                container_exports.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS)
            && self.get_symbol_if_same_reference(export_equals, symbol).is_some()
        {
            return Some(container);
        }
        let exports = self.get_exports_of_symbol(container);
        let quick = exports.and_then(|exports| exports.lookup(symbol.name()));
        if let Some(quick) = quick
            && self.get_symbol_if_same_reference(quick, symbol).is_some()
        {
            return Some(quick);
        }
        let mut candidates: Vec<P<Symbol>> = Vec::new();
        if let Some(exports) = exports
            && !self.exports_by_target(exports, symbol, &mut candidates)
        {
            candidates.extend(
                exports.values().into_iter().filter(|&exported| {
                    self.get_symbol_if_same_reference(exported, symbol).is_some()
                }),
            );
        }
        if !candidates.is_empty() {
            self.sort_symbols(&mut candidates); // _must_ sort exports for stable results - symbol table is randomly iterated
            return Some(candidates[0]);
        }
        None
    }

    // tsrs-only: the loop over every external module of the program in getAlternativeContainingModules: the module
    // symbols `m` (in program order) for which `get_alias_for_symbol_in_container(m, symbol)` is not nil. The cache
    // misses for every fresh (instantiated) symbol, so declaration emit ran that loop thousands of times over the
    // 38k files of the private monorepo (97M calls for one edit of a widely imported file).
    //
    // For a module whose answer no longer has side effects, the answer is decided by data that is fixed: it is not
    // nil iff `m` is `symbol`'s parent, or `m`'s `export=` entry or an entry of its export table resolves (merged) to
    // `symbol`'s merged resolved target. Such a module is "summarized": its targets go into a reverse index. That
    // holds once its export table is final (cached in the module links with no export resolution in progress, so
    // nothing can replace it), `exports_by_target` has indexed that table (every entry resolved) and its `export=`
    // entry is resolved. Modules not summarized yet are asked in program order exactly as the loop does (which may
    // summarize them); summarized ones are pure to ask and are answered from the index. `symbol`'s own target is
    // computed up front, so this is only used when that has no side effects either (not an alias with an unresolved
    // target), and not while an alias resolution is in progress (None: the caller runs the loop).
    fn modules_exporting(&mut self, symbol: P<Symbol>) -> Option<Vec<P<Symbol>>> {
        if self.alias_cache_blockers != 0 {
            return None;
        }
        let merged = self.get_merged_symbol(symbol);
        if merged.flags().intersects(SymbolFlags::Alias)
            && self.alias_symbol_links.get(merged).alias_target.get().is_none()
        {
            return None;
        }
        let target = self.get_symbol_target_for_alias_lookup(symbol);
        let files = self.program.source_files();
        if !self.module_export_index.started {
            self.module_export_index.started = true;
            self.module_export_index.pending = (0..files.len() as u32)
                .filter(|&i| ast::is_external_module(files[i as usize]))
                .collect();
        }
        let mut found: Vec<u32> = Vec::new();
        let pending = std::mem::take(&mut self.module_export_index.pending);
        let mut still_pending = Vec::with_capacity(pending.len());
        for i in pending {
            // tsrs-only: a check leaf another checker may have freed; its answer is nil for any symbol this checker
            // can name here. Its export table holds only symbols declared in it (no alias, `export *` or `export =`)
            // and nothing outside it refers to it, so `symbol`, declared elsewhere, is neither its export nor
            // re-exported by it (`is_unreadable_check_leaf`). It stays pending: if this checker checks it later, it is
            // asked then, as before.
            if self.is_unreadable_check_leaf(files[i as usize]) {
                still_pending.push(i);
                continue;
            }
            let sym = self.get_symbol_of_declaration(files[i as usize].as_node()).unwrap();
            if self.get_alias_for_symbol_in_container(sym, symbol).is_some() {
                found.push(i);
            }
            if !self.summarize_module_exports(i, sym) {
                still_pending.push(i);
            }
        }
        self.module_export_index.pending = still_pending;
        if self.alias_cache_blockers != 0 {
            // Unreachable as long as resolutions finish before returning; keep the loop's answer otherwise.
            return None;
        }
        found.extend(self.module_export_index.files_with_target(target));
        if let Some(parent) = self.get_parent_of_symbol(symbol)
            && let Some(&i) = self.module_export_index.by_symbol.get(&parent)
        {
            found.push(i);
        }
        found.sort_unstable();
        found.dedup();
        Some(
            found
                .into_iter()
                .map(|i| self.get_symbol_of_declaration(files[i as usize].as_node()).unwrap())
                .collect(),
        )
    }

    // tsrs-only (notes/mem-free-leaf-files.md): whether the whole-program loops of getAlternativeContainingModules
    // must pass over `file` without reading it: a check leaf (`SourceFile::is_check_leaf`, CLI `--noEmit` only) other
    // than the file this checker is checking, whose tree the checker that checks it frees when it is done (possibly
    // while this loop runs). Skipping it gives the answer asking it would give, nil:
    // - Nothing outside a leaf refers to it and it declares nothing global, so a symbol this checker names while it
    //   checks another file is declared outside the leaf: the leaf is not its parent.
    // - A leaf's export table holds only symbols declared in it: it has no alias export (`export { x }`, `export ...
    //   from`, `export import`, `export default <name>`), no `export *` and no `export =`. So no entry resolves to a
    //   symbol declared elsewhere, and `getAliasForSymbolInContainer(leaf, symbol)` is nil.
    // - Asking has no effect that reaches output: the leaf's export table needs no `export *` merge (the only part
    //   of `getExportsOfModule` that reports), and the module export index entries it would add are keyed by the
    //   leaf's own symbols.
    // The checker that checks the leaf asks it as before (`checking_file`).
    fn is_unreadable_check_leaf(&self, file: P<SourceFile>) -> bool {
        file.is_check_leaf() && self.checking_file != Some(file)
    }

    // See modules_exporting; true when module `i` (symbol `sym`) is summarized.
    fn summarize_module_exports(&mut self, i: u32, sym: P<Symbol>) -> bool {
        if self.alias_cache_blockers != 0
            || !sym.flags().intersects(SymbolFlags::Module)
            || sym.flags().intersects(SymbolFlags::LateBindingContainer)
        {
            return false;
        }
        let Some(table) = self.module_symbol_links.get(sym).resolved_exports.get() else {
            return false;
        };
        let Some(index) = self.exports_by_target_index.get(&table) else {
            return false;
        };
        if index.len != table.len() {
            return false;
        }
        let mut targets: Vec<P<Symbol>> = index.targets().collect();
        if let Some(export_equals) =
            sym.exports().and_then(|exports| exports.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS))
        {
            let merged = self.get_merged_symbol(export_equals);
            if merged.flags().intersects(SymbolFlags::Alias)
                && self.alias_symbol_links.get(merged).alias_target.get().is_none()
            {
                return false;
            }
            targets.push(self.get_symbol_target_for_alias_lookup(export_equals));
        }
        for t in targets {
            self.module_export_index.add_file_with_target(t, i);
        }
        self.module_export_index.by_symbol.insert(sym, i);
        true
    }

    // tsrs-only: the loop of getAliasForSymbolInContainer, answered from an index of the export table by resolved
    // target (`exported` is a candidate iff `get_symbol_if_same_reference(exported, symbol)`, i.e. their merged
    // resolved targets are equal). Declaration emit runs that loop ~100M times for one edit of a widely imported file
    // in the 38k-file codebase, mostly with fresh (instantiated) symbols over the same few tables. Pushes the
    // candidates in table order, as the loop does, and returns false when the loop has to run instead:
    // - while an alias resolution is in progress or the checker is initializing (`alias_cache_blockers`): a target
    //   seen then may be provisional, and initialization replaces table entries;
    // - for a table whose size changed since it was indexed (late-bound and module export tables are replaced, not
    //   grown, when their resolution finishes; the size check is a guard).
    // The first indexing resolves the entries in the loop's order (first entry, `symbol`, the rest); repeats only
    // read alias targets that are cached by then, as the loop's repeats do.
    fn exports_by_target(
        &mut self,
        exports: P<SymbolTable>,
        symbol: P<Symbol>,
        candidates: &mut Vec<P<Symbol>>,
    ) -> bool {
        if self.alias_cache_blockers != 0 {
            return false;
        }
        let len = exports.len();
        let indexed =
            self.exports_by_target_index.get(&exports).is_some_and(|index| index.len == len);
        let target = if indexed {
            if len == 0 {
                return true;
            }
            self.get_symbol_target_for_alias_lookup(symbol)
        } else {
            let entries = exports.values();
            let mut pairs: Vec<(P<Symbol>, P<Symbol>)> = Vec::with_capacity(entries.len());
            let mut target = None;
            for (i, &exported) in entries.iter().enumerate() {
                let a = self.get_symbol_target_for_alias_lookup(exported);
                if i == 0 {
                    target = Some(self.get_symbol_target_for_alias_lookup(symbol));
                }
                pairs.push((a, exported));
            }
            self.exports_by_target_index.insert(exports, ExportsByTarget::new(len, pairs));
            match target {
                Some(target) => target,
                None => return true,
            }
        };
        if let Some(index) = self.exports_by_target_index.get(&exports) {
            candidates.extend(index.candidates(target));
        }
        true
    }

    // One side of get_symbol_if_same_reference.
    fn get_symbol_target_for_alias_lookup(&mut self, s: P<Symbol>) -> P<Symbol> {
        let m = self.get_merged_symbol(s);
        let r = self.resolve_symbol(m);
        self.get_merged_symbol(r)
    }

    // symbolaccessibility.go:373
    pub(crate) fn get_accessible_symbol_chain(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        use_only_external_aliasing: bool,
    ) -> Vec<P<Symbol>> {
        self.get_accessible_symbol_chain_ex(&AccessibleSymbolChainContext {
            symbol: Some(symbol),
            enclosing_declaration,
            meaning,
            use_only_external_aliasing,
            visited_symbol_tables_map: std::rc::Rc::default(),
        })
    }

    // symbolaccessibility.go:382
    pub fn get_accessible_symbol_chain_exported(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        use_only_external_aliasing: bool,
    ) -> Vec<P<Symbol>> {
        self.get_accessible_symbol_chain(
            symbol,
            enclosing_declaration,
            meaning,
            use_only_external_aliasing,
        )
    }
}

// symbolTableID uniquely identifies a symbol table by encoding its source.
// The high 3 bits encode the kind, and the remaining bits encode the
// NodeId or SymbolId of the source.
const ST_KIND_SHIFT: u64 = 61;

pub const ST_KIND_LOCALS: SymbolTableID = 0 << ST_KIND_SHIFT;
pub const ST_KIND_EXPORTS: SymbolTableID = 1 << ST_KIND_SHIFT;
pub const ST_KIND_MEMBERS: SymbolTableID = 2 << ST_KIND_SHIFT;
pub const ST_KIND_GLOBALS: SymbolTableID = 3 << ST_KIND_SHIFT;
pub const ST_KIND_RESOLVED_EXPORTS: SymbolTableID = 4 << ST_KIND_SHIFT; // resolved/derived exports from getExportsOfSymbol, distinct from raw sym.Exports

// stKindMask extracts the kind bits from a symbolTableID.
pub const ST_KIND_MASK: SymbolTableID = !((1 << ST_KIND_SHIFT) - 1);

// symbolaccessibility.go:417
fn symbol_table_id_from_locals(node: P<Node>) -> SymbolTableID {
    ST_KIND_LOCALS | ast::get_node_id(node).0
}

// symbolaccessibility.go:421
fn symbol_table_id_from_exports(sym: P<Symbol>) -> SymbolTableID {
    ST_KIND_EXPORTS | ast::get_symbol_id(sym).0
}

// symbolTableIDFromResolvedExports returns an ID for resolved/derived export tables
// (e.g. from getExportsOfSymbol/getExportsOfModule which may include export * resolution
// and late-bound members). This is distinct from symbolTableIDFromExports to prevent
// cache collisions with raw sym.Exports tables passed by someSymbolTableInScope.
// symbolaccessibility.go:429
fn symbol_table_id_from_resolved_exports(sym: P<Symbol>) -> SymbolTableID {
    ST_KIND_RESOLVED_EXPORTS | ast::get_symbol_id(sym).0
}

// symbolaccessibility.go:433
fn symbol_table_id_from_members(sym: P<Symbol>) -> SymbolTableID {
    ST_KIND_MEMBERS | ast::get_symbol_id(sym).0
}

// symbolaccessibility.go:437
fn symbol_table_id_from_globals() -> SymbolTableID {
    ST_KIND_GLOBALS
}

impl Checker {
    // symbolaccessibility.go:441
    fn get_accessible_symbol_chain_ex(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
    ) -> Vec<P<Symbol>> {
        let Some(symbol) = ctx.symbol else {
            return Vec::new();
        };
        if is_property_or_method_declaration_symbol(symbol) {
            return Vec::new();
        }
        // Go from enclosingDeclaration to the first scope we check, so the cache is keyed off the scope and thus shared more
        let mut first_relevant_location: Option<P<Node>> = None;
        self.some_symbol_table_in_scope(ctx.enclosing_declaration, |_, _, _, _, _, node| {
            first_relevant_location = node;
            true
        });
        let links = self.symbol_container_links.get(symbol);
        let link_key = AccessibleChainCacheKey {
            use_only_external_aliasing: ctx.use_only_external_aliasing,
            location: first_relevant_location,
            meaning: ctx.meaning,
        };
        let existing = links.accessible_chain_cache.borrow().get(&link_key).copied();
        if let Some(existing) = existing {
            return existing.to_vec();
        }

        let mut result: Vec<P<Symbol>> = Vec::new();

        self.some_symbol_table_in_scope(
            ctx.enclosing_declaration,
            |c, t, table_id, ignore_qualification, is_local_name_lookup, _| {
                let res = c.get_accessible_symbol_chain_from_symbol_table(
                    ctx,
                    t,
                    table_id,
                    ignore_qualification,
                    is_local_name_lookup,
                );
                if !res.is_empty() {
                    result = res;
                    return true;
                }
                false
            },
        );
        links.accessible_chain_cache.borrow_mut().insert(link_key, alloc_slice(&result));
        // A location in a file's emit scratch region (a synthesized node) dies with it; its handle may then name a
        // new node, so the entry is forgotten when the file's transform ends (`forget_scratch_keyed_caches`).
        if let Some(location) = first_relevant_location
            && !ast::is_parse_tree_node(location)
            && tsrs_core::arena::scratch_contains(location.addr())
        {
            self.scratch_keyed_chain_cache.push((symbol, link_key));
        }
        result
    }

    /**
     * @param {ignoreQualification} boolean Set when a symbol is being looked for through the exports of another symbol (meaning we have a route to qualify it already)
     */
    // symbolaccessibility.go:481
    fn get_accessible_symbol_chain_from_symbol_table(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
        t: Option<P<SymbolTable>>,
        table_id: SymbolTableID,
        ignore_qualification: bool,
        is_local_name_lookup: bool,
    ) -> Vec<P<Symbol>> {
        let sym_id = ast::get_symbol_id(ctx.symbol.unwrap());
        {
            let mut visited_symbol_tables_map = ctx.visited_symbol_tables_map.borrow_mut();
            let visited_symbol_tables = visited_symbol_tables_map.entry(sym_id).or_default();

            if !visited_symbol_tables.insert(table_id) {
                return Vec::new();
            }
        }

        let res =
            self.try_symbol_table(ctx, t, table_id, ignore_qualification, is_local_name_lookup);

        ctx.visited_symbol_tables_map.borrow_mut().get_mut(&sym_id).unwrap().remove(&table_id);
        res
    }

    // getSymbolTableAliases returns only the alias symbols from a symbol table,
    // caching the result by tableId to avoid repeated iteration over large tables.
    // Members tables are skipped entirely since someSymbolTableInScope filters them
    // to SymbolFlagsType & ^SymbolFlagsAssignment, which never includes aliases.
    // symbolaccessibility.go:505
    fn get_symbol_table_aliases(
        &mut self,
        symbols: Option<P<SymbolTable>>,
        table_id: SymbolTableID,
    ) -> Vec<P<Symbol>> {
        let kind = table_id & ST_KIND_MASK;
        // Members tables never contain alias symbols; skip entirely.
        if kind == ST_KIND_MEMBERS {
            return Vec::new();
        }
        // Cache globals and exports tables (which are large and revisited often).
        // Locals tables are small and per-scope, so they are filtered but not cached.
        if (kind == ST_KIND_GLOBALS || kind == ST_KIND_EXPORTS || kind == ST_KIND_RESOLVED_EXPORTS)
            && let Some(aliases) = self.symbol_table_alias_cache.get(&table_id)
        {
            return aliases.to_vec();
        }
        let mut aliases: Vec<P<Symbol>> = Vec::new();
        if let Some(symbols) = symbols {
            aliases.extend(
                symbols
                    .values()
                    .into_iter()
                    .filter(|sym| sym.flags().intersects(SymbolFlags::Alias)),
            );
        }
        if kind == ST_KIND_GLOBALS || kind == ST_KIND_EXPORTS || kind == ST_KIND_RESOLVED_EXPORTS {
            self.symbol_table_alias_cache.insert(table_id, alloc_slice(&aliases));
        }
        aliases
    }

    // symbolaccessibility.go:535
    fn try_symbol_table(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
        symbols: Option<P<SymbolTable>>,
        table_id: SymbolTableID,
        ignore_qualification: bool,
        is_local_name_lookup: bool,
    ) -> Vec<P<Symbol>> {
        let ctx_symbol = ctx.symbol.unwrap();
        let is_globals = table_id == ST_KIND_GLOBALS;
        // If symbol is directly available by its name in the symbol table
        let res = symbols.and_then(|symbols| symbols.lookup(ctx_symbol.name()));
        if let Some(res) = res
            && self.is_accessible(ctx, res /*resolvedAliasSymbol*/, None, ignore_qualification)
        {
            return vec![ctx_symbol];
        }

        let mut candidate_chains: Vec<Vec<P<Symbol>>> = Vec::new();

        // Check for ExportSymbol by direct name lookup rather than discovering it during
        // the alias iteration below (where it would never match, since only alias-flagged
        // symbols are iterated).
        if let Some(export_symbol) = res.and_then(|res| res.export_symbol()) {
            let merged = self.get_merged_symbol(export_symbol);
            if self.is_accessible(
                ctx,
                merged, /*resolvedAliasSymbol*/
                None,
                ignore_qualification,
            ) {
                candidate_chains.push(vec![ctx_symbol]);
            }
        }

        // Iterate only alias symbols from the table (cached per tableId).
        // This avoids iterating thousands of non-alias symbols in large tables like globals.
        candidate_chains.extend(self.get_symbol_table_aliases(symbols, table_id).into_iter().filter_map(|symbol_from_symbol_table| {
            let declarations = symbol_from_symbol_table.declarations().to_vec();
            // for every non-default, non-export= alias symbol in scope, check if it refers to or can chain to the target symbol
            if symbol_from_symbol_table.name() != INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                    && symbol_from_symbol_table.name() != INTERNAL_SYMBOL_NAME_DEFAULT
                    && !(is_umd_export_symbol(Some(symbol_from_symbol_table))
                        && ctx.enclosing_declaration.is_some()
                        && ast::is_external_module(ast::get_source_file_of_node(ctx.enclosing_declaration).unwrap()))
                    // If `!useOnlyExternalAliasing`, we can use any type of alias to get the name
                    && (!ctx.use_only_external_aliasing || declarations.iter().any(|&d| ast::is_external_module_import_equals_declaration(d)))
                    // If we're looking up a local name to reference directly, omit namespace reexports, otherwise when we're trawling through an export list to make a dotted name, we can keep it
                    && (!is_local_name_lookup || !declarations.iter().any(|&d| is_namespace_reexport_declaration(d)))
                    // While exports are generally considered to be in scope, export-specifier declared symbols are _not_
                    // See similar comment in `resolveName` for details
                    && (ignore_qualification || get_declarations_of_kind(symbol_from_symbol_table, Kind::ExportSpecifier).is_empty())
            {
                let resolved_imported_symbol = self.resolve_alias(symbol_from_symbol_table);
                let candidate = self.get_candidate_list_for_symbol(ctx, symbol_from_symbol_table, resolved_imported_symbol, ignore_qualification);
                if candidate.is_empty() { None } else { Some(candidate) }
            } else {
                None
            }
        }));

        if !candidate_chains.is_empty() {
            // pick first, shortest
            candidate_chains.sort_by(|a, b| self.compare_symbol_chains(a, b));
            return candidate_chains.swap_remove(0);
        }

        // If there's no result and we're looking at the global symbol table, treat `globalThis` like an alias and try to lookup thru that
        if is_globals {
            let global_this_symbol = self.global_this_symbol;
            return self.get_candidate_list_for_symbol(
                ctx,
                global_this_symbol,
                global_this_symbol,
                ignore_qualification,
            );
        }
        Vec::new()
    }

    // symbolaccessibility.go:595
    pub(crate) fn compare_symbol_chains_worker(
        &mut self,
        a: &[P<Symbol>],
        b: &[P<Symbol>],
    ) -> std::cmp::Ordering {
        let chain_len = a.len().cmp(&b.len());
        if chain_len != std::cmp::Ordering::Equal {
            return chain_len;
        }

        let mut idx = 0;
        while idx < a.len() {
            let comparison = self.compare_symbols(Some(a[idx]), Some(b[idx]));
            if comparison != std::cmp::Ordering::Equal {
                return comparison;
            }
            idx += 1;
        }
        std::cmp::Ordering::Equal
    }
}

// symbolaccessibility.go:612
fn is_umd_export_symbol(symbol: Option<P<Symbol>>) -> bool {
    symbol.is_some_and(|symbol| {
        let declarations = symbol.declarations();
        !declarations.is_empty() && ast::is_namespace_export_declaration(declarations[0])
    })
}

// symbolaccessibility.go:616
fn is_namespace_reexport_declaration(node: P<Node>) -> bool {
    ast::is_namespace_export(node) && node.parent().unwrap().module_specifier().is_some()
}

impl Checker {
    // symbolaccessibility.go:620
    fn get_candidate_list_for_symbol(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
        symbol_from_symbol_table: P<Symbol>,
        resolved_imported_symbol: P<Symbol>,
        ignore_qualification: bool,
    ) -> Vec<P<Symbol>> {
        if self.is_accessible(
            ctx,
            symbol_from_symbol_table,
            Some(resolved_imported_symbol),
            ignore_qualification,
        ) {
            return vec![symbol_from_symbol_table];
        }

        // Look in the exported members, if we can find accessibleSymbolChain, symbol is accessible using this chain
        // but only if the symbolFromSymbolTable can be qualified
        let Some(candidate_table) = self.get_exports_of_symbol(resolved_imported_symbol) else {
            return Vec::new();
        };
        let candidate_table_id = symbol_table_id_from_resolved_exports(resolved_imported_symbol);
        let accessible_symbols_from_exports = self.get_accessible_symbol_chain_from_symbol_table(
            ctx,
            Some(candidate_table),
            candidate_table_id, /*ignoreQualification*/
            true,
            false,
        );
        if accessible_symbols_from_exports.is_empty() {
            return Vec::new();
        }
        if !self.can_qualify_symbol(
            ctx,
            symbol_from_symbol_table,
            get_qualified_left_meaning(ctx.meaning),
        ) {
            return Vec::new();
        }
        let mut result = vec![symbol_from_symbol_table];
        result.extend(accessible_symbols_from_exports);
        result
    }

    // symbolaccessibility.go:647
    fn is_accessible(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
        symbol_from_symbol_table: P<Symbol>,
        resolved_alias_symbol: Option<P<Symbol>>,
        ignore_qualification: bool,
    ) -> bool {
        let mut like_symbols = false;
        if ctx.symbol == resolved_alias_symbol {
            like_symbols = true;
        }
        if ctx.symbol == Some(symbol_from_symbol_table) {
            like_symbols = true;
        }
        let symbol = self.get_merged_symbol(ctx.symbol.unwrap());
        if Some(symbol) == resolved_alias_symbol.map(|s| self.get_merged_symbol(s)) {
            like_symbols = true;
        }
        if symbol == self.get_merged_symbol(symbol_from_symbol_table) {
            like_symbols = true;
        }
        if !like_symbols
            && let Some(mut resolved_alias_symbol) =
                resolved_alias_symbol.filter(|symbol| symbol.flags().intersects(SymbolFlags::Alias))
        {
            let mut seen_aliases = FxHashSet::default();
            while resolved_alias_symbol.flags().intersects(SymbolFlags::Alias)
                && seen_aliases.insert(resolved_alias_symbol)
            {
                let target = self.resolve_alias(resolved_alias_symbol);
                resolved_alias_symbol = self.get_merged_symbol(target);
                if symbol == resolved_alias_symbol {
                    like_symbols = true;
                    break;
                }
            }
        }
        if !like_symbols {
            return false;
        }
        // if the symbolFromSymbolTable is not external module (it could be if it was determined as ambient external module and would be in globals table)
        // and if symbolFromSymbolTable or alias resolution matches the symbol,
        // check the symbol can be qualified, it is only then this symbol is accessible
        let has_module_declaration = symbol_from_symbol_table
            .declarations()
            .iter()
            .any(|&d| has_non_global_augmentation_external_module_symbol(d));
        !has_module_declaration
            && (ignore_qualification || {
                let merged = self.get_merged_symbol(symbol_from_symbol_table);
                self.can_qualify_symbol(ctx, merged, ctx.meaning)
            })
    }

    // symbolaccessibility.go:677
    fn can_qualify_symbol(
        &mut self,
        ctx: &AccessibleSymbolChainContext,
        symbol_from_symbol_table: P<Symbol>,
        meaning: SymbolFlags,
    ) -> bool {
        // If the symbol is equivalent and doesn't need further qualification, this symbol is accessible
        !self.needs_qualification(symbol_from_symbol_table, ctx.enclosing_declaration, meaning) ||
            // If symbol needs qualification, make sure that parent is accessible, if it is then this symbol is accessible too
            !self
                .get_accessible_symbol_chain_ex(&AccessibleSymbolChainContext {
                    symbol: symbol_from_symbol_table.parent(),
                    enclosing_declaration: ctx.enclosing_declaration,
                    meaning: get_qualified_left_meaning(meaning),
                    use_only_external_aliasing: ctx.use_only_external_aliasing,
                    visited_symbol_tables_map: Rc::clone(&ctx.visited_symbol_tables_map),
                })
                .is_empty()
    }

    // symbolaccessibility.go:688
    pub(crate) fn needs_qualification(
        &mut self,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        let mut qualify = false;
        self.some_symbol_table_in_scope(enclosing_declaration, |c, symbol_table, _, _, _, _| {
            // If symbol of this name is not available in the symbol table we are ok
            let Some(res) =
                symbol_table.and_then(|symbol_table| symbol_table.lookup(symbol.name()))
            else {
                return false;
            };
            let mut symbol_from_symbol_table = c.get_merged_symbol(res);
            // If the symbol with this name is present it should refer to the symbol
            if symbol_from_symbol_table == symbol {
                // No need to qualify
                return true;
            }

            // Qualify if the symbol from symbol table has same meaning as expected
            let should_resolve_alias = symbol_from_symbol_table
                .flags()
                .intersects(SymbolFlags::Alias)
                && ast::get_declaration_of_kind(symbol_from_symbol_table, Kind::ExportSpecifier)
                    .is_none();
            if should_resolve_alias {
                symbol_from_symbol_table = c.resolve_alias(symbol_from_symbol_table);
            }
            let mut flags = symbol_from_symbol_table.flags();
            if should_resolve_alias {
                flags = c.get_symbol_flags(symbol_from_symbol_table);
            }
            if flags.intersects(meaning) {
                qualify = true;
                return true;
            }

            // Continue to the next symbol table
            false
        });

        qualify
    }
}

// symbolaccessibility.go:728
fn is_property_or_method_declaration_symbol(symbol: P<Symbol>) -> bool {
    let declarations = symbol.declarations();
    if !declarations.is_empty() {
        if declarations.iter().any(|declaration| {
            !matches!(
                declaration.kind(),
                Kind::PropertyDeclaration
                    | Kind::MethodDeclaration
                    | Kind::GetAccessor
                    | Kind::SetAccessor
            )
        }) {
            return false;
        }
        return true;
    }
    false
}

impl Checker {
    // symbolaccessibility.go:746
    fn some_symbol_table_in_scope(
        &mut self,
        enclosing_declaration: Option<P<Node>>,
        mut callback: impl FnMut(
            &mut Checker,
            Option<P<SymbolTable>>,
            SymbolTableID,
            bool,
            bool,
            Option<P<Node>>,
        ) -> bool,
    ) -> bool {
        let mut location = enclosing_declaration;
        while let Some(loc) = location {
            // Locals of a source file are not in scope (because they get merged into the global symbol table)
            if can_have_locals(loc)
                && loc.locals().is_some()
                && !ast::is_global_source_file(loc)
                && callback(
                    self,
                    loc.locals(),
                    symbol_table_id_from_locals(loc),
                    false,
                    true,
                    Some(loc),
                )
            {
                return true;
            }
            match loc.kind() {
                Kind::SourceFile | Kind::ModuleDeclaration => {
                    if !ast::is_source_file(loc)
                        || ast::is_external_or_common_js_module(loc.as_source_file_p())
                    {
                        let sym = self
                            .get_symbol_of_declaration(
                                ast::get_reparsed_node_for_node(loc).unwrap(),
                            )
                            .unwrap();
                        if callback(
                            self,
                            sym.exports(),
                            symbol_table_id_from_exports(sym),
                            false,
                            true,
                            Some(loc),
                        ) {
                            return true;
                        }
                    }
                }
                Kind::ClassDeclaration | Kind::ClassExpression | Kind::InterfaceDeclaration => {
                    // Type parameters are bound into `members` lists so they can merge across declarations
                    // This is troublesome, since in all other respects, they behave like locals :cries:
                    // TODO: the below is shared with similar code in `resolveName` - in fact, rephrasing all this symbol
                    // lookup logic in terms of `resolveName` would be nice
                    // The below is used to lookup type parameters within a class or interface, as they are added to the class/interface locals
                    // These can never be latebound, so the symbol's raw members are sufficient. `getMembersOfNode` cannot be used, as it would
                    // trigger resolving late-bound names, which we may already be in the process of doing while we're here!
                    let mut table: Option<P<SymbolTable>> = None;
                    let sym = self.get_symbol_of_declaration(loc).unwrap();
                    // TODO: Should this filtered table be cached in some way?
                    if let Some(members) = sym.members() {
                        for (key, member_symbol) in members.entries() {
                            if member_symbol
                                .flags()
                                .intersects(SymbolFlags::Type & !SymbolFlags::Assignment)
                            {
                                if table.is_none() {
                                    table = Some(SymbolTable::new());
                                }
                                table.unwrap().set(key, member_symbol);
                            }
                        }
                    }
                    if table.is_some()
                        && callback(
                            self,
                            table,
                            symbol_table_id_from_members(sym),
                            false,
                            false,
                            Some(loc),
                        )
                    {
                        return true;
                    }
                    // Class expression names (e.g., `B` in `class B {}`) are not stored in any
                    // scope table — the binder uses bindAnonymousDeclaration. Expose the name
                    // binding here so getAccessibleSymbolChain can resolve self-references.
                    // This mirrors the special casing of class expression names in
                    // (*NameResolver).Resolve; if class names are ever bound differently
                    // (e.g., via class-local type aliases), both sites should be updated.
                    if ast::is_class_expression(loc) && loc.as_class_expression().name().is_some() {
                        let name_table = self.get_class_expression_name_table(loc);
                        if name_table.is_some()
                            && callback(
                                self,
                                name_table,
                                symbol_table_id_from_locals(loc),
                                false,
                                true,
                                Some(loc),
                            )
                        {
                            return true;
                        }
                    }
                }
                _ => {}
            }
            location = loc.parent();
        }

        let globals = self.globals;
        callback(self, Some(globals), symbol_table_id_from_globals(), false, true, None)
    }

    // getClassExpressionNameTable returns a cached symbol table containing the class
    // expression's name binding. Class expression names are bound via
    // bindAnonymousDeclaration and aren't stored in any container's locals, so this
    // synthesized table lets someSymbolTableInScope expose them during accessibility checks.
    // symbolaccessibility.go:810
    fn get_class_expression_name_table(&mut self, location: P<Node>) -> Option<P<SymbolTable>> {
        let node_id = ast::get_node_id(location);
        if let Some(&table) = self.class_expression_name_tables.get(&node_id) {
            return Some(table);
        }
        let class_symbol = self.get_symbol_of_declaration(location);
        let name_text = location.as_class_expression().name().unwrap().text();
        if name_text.is_empty() || class_symbol.is_none() {
            return None;
        }
        let table = SymbolTable::new();
        table.set(name_text, class_symbol.unwrap());
        self.class_expression_name_tables.insert(node_id, table);
        Some(table)
    }

    /**
     * Check if the given symbol in given enclosing declaration is accessible and mark all associated alias to be visible if requested
     *
     * @param symbol a Symbol to check if accessible
     * @param enclosingDeclaration a Node containing reference to the symbol
     * @param meaning a `SymbolFlags` to check if such meaning of the symbol is accessible
     * @param shouldComputeAliasToMakeVisible a boolean value to indicate whether to return aliases to be mark visible in case the symbol is accessible
     */
    // symbolaccessibility.go:839
    pub(crate) fn is_symbol_accessible(
        &mut self,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        should_compute_aliases_to_make_visible: bool,
    ) -> SymbolAccessibilityResult {
        self.is_symbol_accessible_worker(
            symbol,
            enclosing_declaration,
            meaning,
            should_compute_aliases_to_make_visible,
            true, /*allowModules*/
        )
    }

    // symbolaccessibility.go:843
    fn is_symbol_accessible_worker(
        &mut self,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        should_compute_aliases_to_make_visible: bool,
        allow_modules: bool,
    ) -> SymbolAccessibilityResult {
        if let (Some(symbol), Some(enclosing)) = (symbol, enclosing_declaration) {
            let result = self.is_any_symbol_accessible(
                &[symbol],
                enclosing_declaration,
                symbol,
                meaning,
                should_compute_aliases_to_make_visible,
                allow_modules,
            );
            if let Some(result) = result {
                return (*result).clone();
            }

            // This could be a symbol that is not exported in the external module
            // or it could be a symbol from different external module that is not aliased and hence cannot be named
            let declarations = symbol.declarations().to_vec();
            let symbol_external_module =
                first_non_nil(&declarations, |&d| self.get_external_module_container(d));
            if let Some(symbol_external_module) = symbol_external_module {
                let enclosing_external_module = self.get_external_module_container(enclosing);
                if Some(symbol_external_module) != enclosing_external_module {
                    // name from different external module that is not visible
                    return SymbolAccessibilityResult {
                        accessibility: SymbolAccessibility::CannotBeNamed,
                        error_symbol_name: self.symbol_to_string_ex(
                            symbol,
                            enclosing_declaration,
                            meaning,
                            SymbolFormatFlags::AllowAnyNodeKind,
                        ),
                        error_module_name: self.symbol_to_string(symbol_external_module),
                        error_node: if ast::is_in_js_file(enclosing) {
                            Some(enclosing)
                        } else {
                            None
                        },
                        ..Default::default()
                    };
                }
            }

            // Just a local name that is not accessible
            return SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::NotAccessible,
                error_symbol_name: self.symbol_to_string_ex(
                    symbol,
                    enclosing_declaration,
                    meaning,
                    SymbolFormatFlags::AllowAnyNodeKind,
                ),
                ..Default::default()
            };
        }

        SymbolAccessibilityResult {
            accessibility: SymbolAccessibility::Accessible,
            ..Default::default()
        }
    }
}

// ---- symboltracker.go ----

// Non-function declarations in symboltracker.go (hand-ported in printer_types.rs):
//   type SymbolTrackerImpl (symboltracker.go:8)

// symboltracker.go:14
pub(crate) fn new_symbol_tracker_impl(
    context: P<NodeBuilderContext>,
    tracker: Option<&'static dyn SymbolTracker>,
) -> P<SymbolTrackerImpl> {
    let mut tracker = tracker;
    while let Some(t) = tracker.and_then(|t| t.as_symbol_tracker_impl()) {
        tracker = t.inner;
    }

    P::new_scratch(SymbolTrackerImpl {
        context,
        inner: tracker,
        disable_track_symbol: Cell::new(false),
    })
}

impl SymbolTrackerImpl {
    // symboltracker.go:28
    pub(crate) fn track_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        if !self.disable_track_symbol.get() {
            if self
                .inner
                .is_some_and(|inner| inner.track_symbol(c, symbol, enclosing_declaration, meaning))
            {
                self.on_diagnostic_reported();
                return true;
            }
            // Skip recording type parameters as they dont contribute to late painted statements
            if !symbol.flags().intersects(SymbolFlags::TypeParameter) {
                self.context.tracked_symbols.borrow_mut().push(TrackedSymbolArgs {
                    symbol,
                    enclosing_declaration,
                    meaning,
                });
            }
        }
        false
    }

    // symboltracker.go:42
    pub(crate) fn report_inaccessible_this_error(&self) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_inaccessible_this_error();
    }

    // symboltracker.go:50
    pub(crate) fn report_private_in_base_of_class_expression(&self, property_name: &str) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_private_in_base_of_class_expression(property_name);
    }

    // symboltracker.go:58
    pub(crate) fn report_inaccessible_unique_symbol_error(&self) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_inaccessible_unique_symbol_error();
    }

    // symboltracker.go:66
    pub(crate) fn report_cyclic_structure_error(&self) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_cyclic_structure_error();
    }

    // symboltracker.go:74
    pub(crate) fn report_likely_unsafe_import_required_error(
        &self,
        specifier: &str,
        symbol_name: &str,
    ) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_likely_unsafe_import_required_error(specifier, symbol_name);
    }

    // symboltracker.go:82
    pub(crate) fn report_truncation_error(&self) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_truncation_error();
    }

    // symboltracker.go:90
    pub(crate) fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    ) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_nonlocal_augmentation(containing_file, parent_symbol, augmenting_symbol);
    }

    // symboltracker.go:98
    pub(crate) fn report_non_serializable_property(&self, property_name: &str) {
        self.on_diagnostic_reported();
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_non_serializable_property(property_name);
    }

    // symboltracker.go:106
    fn on_diagnostic_reported(&self) {
        self.context.reported_diagnostic.set(true);
    }

    // symboltracker.go:110
    pub(crate) fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>) {
        let Some(inner) = self.inner else {
            return;
        };
        inner.report_inference_fallback(c, node);
    }

    // symboltracker.go:117
    pub(crate) fn push_error_fallback_node(&self, node: Option<P<Node>>) {
        let Some(inner) = self.inner else {
            return;
        };
        inner.push_error_fallback_node(node);
    }

    // symboltracker.go:124
    pub(crate) fn pop_error_fallback_node(&self) {
        let Some(inner) = self.inner else {
            return;
        };
        inner.pop_error_fallback_node();
    }
}

/// tsrs-only: which external modules export what (`Checker::modules_exporting`).
#[derive(Default)]
pub struct ModuleExportIndex {
    started: bool,
    /// Program file indices of the external modules not summarized yet, in program order.
    pending: Vec<u32>,
    /// Merged resolved target -> the summarized modules (file indices) with an entry resolving to it: the head of a
    /// list in `files` (most recent first; usually one module, so no `Vec` per target).
    by_target: FxHashMap<P<Symbol>, u32>,
    /// List nodes of `by_target`: (file index, next node or `u32::MAX`).
    files: Vec<(u32, u32)>,
    /// Summarized module symbol -> its file index.
    by_symbol: FxHashMap<P<Symbol>, u32>,
}

impl ModuleExportIndex {
    const END: u32 = u32::MAX;

    /// The summarized modules with an entry resolving to `target`, most recently added first.
    fn files_with_target(&self, target: P<Symbol>) -> impl Iterator<Item = u32> + '_ {
        let mut next = self.by_target.get(&target).copied().unwrap_or(Self::END);
        std::iter::from_fn(move || {
            let (file, after) = *self.files.get(next as usize)?;
            next = after;
            Some(file)
        })
    }

    /// Records that module `file` has an entry resolving to `target` (once per consecutive repeat, like the `Vec`
    /// it replaces: `if files.last() != Some(&i) { files.push(i) }`).
    fn add_file_with_target(&mut self, target: P<Symbol>, file: u32) {
        let node = u32::try_from(self.files.len()).expect("module export index too large");
        match self.by_target.entry(target) {
            Entry::Occupied(mut head) => {
                if self.files[*head.get() as usize].0 != file {
                    self.files.push((file, *head.get()));
                    *head.get_mut() = node;
                }
            }
            Entry::Vacant(head) => {
                self.files.push((file, Self::END));
                head.insert(node);
            }
        }
    }
}

/// An export table indexed by the merged resolved target of each entry (`Checker::exports_by_target`): the
/// (target, entry) pairs sorted by target with a stable sort, so each target's entries are adjacent and in table
/// order. One allocation of 8 bytes per entry instead of a hash map with a `Vec` per target (the index is built in
/// every checker; most tables are small and most targets have one entry).
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub struct ExportsByTarget {
    len: usize,
    entries: Box<[(P<Symbol>, P<Symbol>)]>,
}

impl ExportsByTarget {
    fn new(len: usize, mut pairs: Vec<(P<Symbol>, P<Symbol>)>) -> ExportsByTarget {
        pairs.sort_by_key(|&(target, _)| target.key());
        ExportsByTarget { len, entries: pairs.into_boxed_slice() }
    }

    /// The table's entries whose target is `target`, in table order.
    fn candidates(&self, target: P<Symbol>) -> impl Iterator<Item = P<Symbol>> + '_ {
        let key = target.key();
        let start = self.entries.partition_point(|&(t, _)| t.key() < key);
        self.entries[start..]
            .iter()
            .take_while(move |&&(t, _)| t == target)
            .map(|&(_, exported)| exported)
    }

    /// Every distinct target once.
    fn targets(&self) -> impl Iterator<Item = P<Symbol>> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|&(i, &(t, _))| i == 0 || self.entries[i - 1].0 != t)
            .map(|(_, &(t, _))| t)
    }
}
