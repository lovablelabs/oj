// Port of binder/referenceresolver.go. Go's `ReferenceResolver` interface has one implementation
// (`referenceResolver`); in Rust it is the struct itself. Like `NameResolver`, the hooks are plain `fn`
// pointers that receive the host (the checker) as their first argument, and methods take the host.

use std::cell::OnceCell;

use tsrs_ast::{self as ast, Kind, Node, Symbol, SymbolFlags};
use tsrs_core::{CompilerOptions, P};
use tsrs_diagnostics::Message;

use crate::NameResolver;

pub struct ReferenceResolverHooks<H: 'static> {
    pub resolve_name: Option<
        fn(
            &mut H,
            Option<P<Node>>,
            &str,
            SymbolFlags,
            Option<&'static Message>,
            bool,
            bool,
        ) -> Option<P<Symbol>>,
    >,
    pub get_resolved_symbol: Option<fn(&mut H, P<Node>) -> Option<P<Symbol>>>,
    pub get_merged_symbol: Option<fn(&mut H, P<Symbol>) -> P<Symbol>>,
    pub get_parent_of_symbol: Option<fn(&mut H, P<Symbol>) -> Option<P<Symbol>>>,
    pub get_symbol_of_declaration: Option<fn(&mut H, P<Node>) -> Option<P<Symbol>>>,
    pub get_type_only_alias_declaration:
        Option<fn(&mut H, P<Symbol>, SymbolFlags) -> Option<P<Node>>>,
    pub get_export_symbol_of_value_symbol_if_exported:
        Option<fn(&mut H, P<Symbol>) -> Option<P<Symbol>>>,
    pub get_element_access_expression_name: Option<fn(&mut H, P<Node>) -> (String, bool)>,
}

impl<H: 'static> Default for ReferenceResolverHooks<H> {
    fn default() -> Self {
        ReferenceResolverHooks {
            resolve_name: None,
            get_resolved_symbol: None,
            get_merged_symbol: None,
            get_parent_of_symbol: None,
            get_symbol_of_declaration: None,
            get_type_only_alias_declaration: None,
            get_export_symbol_of_value_symbol_if_exported: None,
            get_element_access_expression_name: None,
        }
    }
}

pub struct ReferenceResolver<H: 'static> {
    resolver: OnceCell<P<NameResolver<H>>>,
    options: P<CompilerOptions>,
    hooks: ReferenceResolverHooks<H>,
}

pub fn new_reference_resolver<H: 'static>(
    options: P<CompilerOptions>,
    hooks: ReferenceResolverHooks<H>,
) -> P<ReferenceResolver<H>> {
    P::new(ReferenceResolver { resolver: OnceCell::new(), options, hooks })
}

impl<H: 'static> ReferenceResolver<H> {
    fn get_resolved_symbol(&self, h: &mut H, node: Option<P<Node>>) -> Option<P<Symbol>> {
        if let Some(node) = node
            && let Some(hook) = self.hooks.get_resolved_symbol
        {
            return hook(h, node);
        }
        None
    }

    fn get_merged_symbol(&self, h: &mut H, symbol: Option<P<Symbol>>) -> Option<P<Symbol>> {
        if let Some(symbol) = symbol {
            if let Some(hook) = self.hooks.get_merged_symbol {
                return Some(hook(h, symbol));
            }
            return Some(symbol);
        }
        None
    }

    fn get_parent_of_symbol(&self, h: &mut H, symbol: Option<P<Symbol>>) -> Option<P<Symbol>> {
        if let Some(symbol) = symbol {
            if let Some(hook) = self.hooks.get_parent_of_symbol {
                return hook(h, symbol);
            }
            return symbol.parent();
        }
        None
    }

    fn get_symbol_of_declaration(
        &self,
        h: &mut H,
        declaration: Option<P<Node>>,
    ) -> Option<P<Symbol>> {
        if let Some(declaration) = declaration {
            if let Some(hook) = self.hooks.get_symbol_of_declaration {
                return hook(h, declaration);
            }
            return declaration.symbol();
        }
        None
    }

    fn get_referenced_value_symbol(
        &self,
        h: &mut H,
        reference: P<Node>,
        start_in_declaration_container: bool,
    ) -> Option<P<Symbol>> {
        let resolved_symbol = self.get_resolved_symbol(h, Some(reference));
        if resolved_symbol.is_some() {
            return resolved_symbol;
        }

        let mut location = Some(reference);
        if start_in_declaration_container
            && let Some(parent) = reference.parent()
            && ast::is_declaration(parent)
            && parent.name() == Some(reference)
        {
            location = ast::get_declaration_container(parent);
        }

        if let Some(resolve_name) = self.hooks.resolve_name {
            return resolve_name(
                h,
                location,
                reference.text(),
                SymbolFlags::ExportValue | SymbolFlags::Value | SymbolFlags::Alias,
                None,  /*nameNotFoundMessage*/
                false, /*isUse*/
                false, /*excludeGlobals*/
            );
        }

        let resolver = *self.resolver.get_or_init(|| P::new(NameResolver::new(self.options, None)));

        resolver.resolve(
            h,
            location,
            reference.text(),
            SymbolFlags::ExportValue | SymbolFlags::Value | SymbolFlags::Alias,
            None,  /*nameNotFoundMessage*/
            false, /*isUse*/
            false, /*excludeGlobals*/
        )
    }

    fn is_type_only_alias_declaration(&self, h: &mut H, symbol: Option<P<Symbol>>) -> bool {
        if let Some(symbol) = symbol {
            if let Some(hook) = self.hooks.get_type_only_alias_declaration {
                return hook(h, symbol, SymbolFlags::Value).is_some();
            }

            let mut node = Self::get_declaration_of_alias_symbol(symbol);
            while let Some(n) = node {
                match n.kind() {
                    Kind::ImportEqualsDeclaration | Kind::ExportDeclaration => {
                        return n.is_type_only();
                    }
                    Kind::ImportClause | Kind::ImportSpecifier | Kind::ExportSpecifier => {
                        if n.is_type_only() {
                            return true;
                        }
                        node = n.parent();
                        continue;
                    }
                    Kind::NamedImports | Kind::NamedExports => {
                        node = n.parent();
                        continue;
                    }
                    _ => {}
                }
                break;
            }
        }
        false
    }

    fn get_declaration_of_alias_symbol(symbol: P<Symbol>) -> Option<P<Node>> {
        symbol.declarations().iter().rev().copied().find(|&d| ast::is_alias_symbol_declaration(d))
    }

    fn get_export_symbol_of_value_symbol_if_exported(
        &self,
        h: &mut H,
        symbol: Option<P<Symbol>>,
    ) -> Option<P<Symbol>> {
        if let Some(mut symbol) = symbol {
            if let Some(hook) = self.hooks.get_export_symbol_of_value_symbol_if_exported {
                return hook(h, symbol);
            }
            if symbol.flags().intersects(SymbolFlags::ExportValue)
                && let Some(export_symbol) = symbol.export_symbol()
            {
                symbol = export_symbol;
            }
            return self.get_merged_symbol(h, Some(symbol));
        }
        None
    }

    pub fn get_referenced_export_container(
        &self,
        h: &mut H,
        node: P<Node>,
        prefix_locals: bool,
    ) -> Option<P<Node>> /*SourceFile|ModuleDeclaration|EnumDeclaration*/ {
        // When resolving the export for the name of a module or enum
        // declaration, we need to start resolution at the declaration's container.
        // Otherwise, we could incorrectly resolve the export as the
        // declaration if it contains an exported member with the same name.
        let start_in_declaration_container = node.parent().is_some_and(|p| {
            (p.kind() == Kind::ModuleDeclaration || p.kind() == Kind::EnumDeclaration)
                && Some(node) == p.name()
        });
        if let Some(mut symbol) =
            self.get_referenced_value_symbol(h, node, start_in_declaration_container)
        {
            if symbol.flags().intersects(SymbolFlags::ExportValue) {
                // If we reference an exported entity within the same module declaration, then whether
                // we prefix depends on the kind of entity. SymbolFlags.ExportHasLocal encompasses all the
                // kinds that we do NOT prefix.
                let export_symbol = self.get_merged_symbol(h, symbol.export_symbol()).unwrap();
                if !prefix_locals
                    && export_symbol.flags().intersects(SymbolFlags::ExportHasLocal)
                    && !export_symbol.flags().intersects(SymbolFlags::Variable)
                {
                    return None;
                }
                symbol = export_symbol;
            }
            let parent_symbol = self.get_parent_of_symbol(h, Some(symbol));
            if let Some(parent_symbol) = parent_symbol {
                if parent_symbol.flags().intersects(SymbolFlags::ValueModule)
                    && let Some(value_declaration) = parent_symbol.value_declaration()
                    && value_declaration.kind() == Kind::SourceFile
                {
                    let symbol_file = value_declaration; // .AsSourceFile()
                    let reference_file = ast::get_source_file_of_node(Some(node));
                    // If `node` accesses an export and that export isn't in the same file, then symbol is a namespace export, so return nil.
                    let symbol_is_umd_export =
                        Some(symbol_file) != reference_file.map(|f| f.as_node());
                    if symbol_is_umd_export {
                        return None;
                    }
                    return Some(symbol_file);
                }
                let mut n = node.parent();
                while let Some(cur) = n {
                    if (cur.kind() == Kind::ModuleDeclaration
                        || cur.kind() == Kind::EnumDeclaration)
                        && self.get_symbol_of_declaration(h, Some(cur)) == Some(parent_symbol)
                    {
                        return Some(cur);
                    }
                    n = cur.parent();
                }
                return None;
            }
        }

        None
    }

    pub fn get_referenced_import_declaration(&self, h: &mut H, node: P<Node>) -> Option<P<Node>> {
        if let Some(symbol) =
            self.get_referenced_value_symbol(h, node, false /*startInDeclarationContainer*/)
        {
            // We should only get the declaration of an alias if there isn't a local value
            // declaration for the symbol
            if ast::is_non_local_alias(Some(symbol), SymbolFlags::Value /*excludes*/)
                && !self.is_type_only_alias_declaration(h, Some(symbol))
            {
                return Self::get_declaration_of_alias_symbol(symbol);
            }
        }

        None
    }

    pub fn get_referenced_value_declaration(&self, h: &mut H, node: P<Node>) -> Option<P<Node>> {
        if let Some(symbol) =
            self.get_referenced_value_symbol(h, node, false /*startInDeclarationContainer*/)
        {
            return self
                .get_export_symbol_of_value_symbol_if_exported(h, Some(symbol))
                .unwrap()
                .value_declaration();
        }
        None
    }

    pub fn get_referenced_value_declarations(&self, h: &mut H, node: P<Node>) -> Vec<P<Node>> {
        let mut declarations = Vec::new();
        if let Some(symbol) =
            self.get_referenced_value_symbol(h, node, false /*startInDeclarationContainer*/)
        {
            let symbol =
                self.get_export_symbol_of_value_symbol_if_exported(h, Some(symbol)).unwrap();
            declarations.extend(symbol.declarations().iter().filter_map(|&declaration| {
                match declaration.kind() {
                    Kind::VariableDeclaration
                    | Kind::Parameter
                    | Kind::BindingElement
                    | Kind::PropertyDeclaration
                    | Kind::PropertyAssignment
                    | Kind::ShorthandPropertyAssignment
                    | Kind::EnumMember
                    | Kind::ObjectLiteralExpression
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::ArrowFunction
                    | Kind::ClassDeclaration
                    | Kind::ClassExpression
                    | Kind::EnumDeclaration
                    | Kind::MethodDeclaration
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::ModuleDeclaration => Some(declaration),
                    _ => None,
                }
            }));
        }
        declarations
    }

    pub fn get_element_access_expression_name(
        &self,
        h: &mut H,
        expression: Option<P<Node>>,
    ) -> String {
        if let Some(expression) = expression
            && let Some(hook) = self.hooks.get_element_access_expression_name
        {
            let (name, ok) = hook(h, expression);
            if ok {
                return name;
            }
        }
        String::new()
    }

    pub fn get_referenced_member_value_declaration(
        &self,
        h: &mut H,
        node: P<Node>,
    ) -> Option<P<Node>> {
        // member references are `this.something` or `this[something]`, so should always simply have a resolved symbol
        let mut s = self.get_resolved_symbol(h, Some(node));
        if s.is_none() && node.symbol().is_some() {
            // might be a declaration instead of a ref, get the merged declaration symbol
            s = self.get_merged_symbol(h, node.symbol());
        }
        let s = s?;
        self.get_export_symbol_of_value_symbol_if_exported(h, Some(s)).unwrap().value_declaration()
    }
}
