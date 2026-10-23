//! Top-level import/require scan feeding the stylex binding tables.
// parity: babel-plugin src/visitors/imports.js + src/index.js:76-91 (top level only)

use std::collections::{BTreeMap, BTreeSet};

use oxc_ast::ast::{
    BindingPattern, Expression, ImportDeclaration, ImportDeclarationSpecifier, ModuleExportName,
    Program, PropertyKey, Statement, VariableDeclaration,
};
use oxc_span::Span;

use crate::errors::StylexError;
use crate::options::ResolvedOptions;

pub const ATOMS_SOURCE: &str = "@stylexjs/atoms";

/// `state.atomImports` marker for a default/namespace/require binding.
pub const ATOM_NAMESPACE: &str = "*";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum StylexNamedImport {
    Create,
    Props,
    Attrs,
    Keyframes,
    PositionTry,
    ViewTransitionClass,
    Include,
    FirstThatWorks,
    DefineVars,
    DefineMarker,
    DefineConsts,
    CreateTheme,
    Types,
    When,
    DefaultMarker,
    Env,
    DefineVarsNested,
    DefineConstsNested,
    CreateThemeNested,
    Conditional,
}

impl StylexNamedImport {
    pub fn from_imported_name(name: &str) -> Option<Self> {
        Some(match name {
            "create" => Self::Create,
            "props" => Self::Props,
            "attrs" => Self::Attrs,
            "keyframes" => Self::Keyframes,
            "positionTry" => Self::PositionTry,
            "viewTransitionClass" => Self::ViewTransitionClass,
            "include" => Self::Include,
            "firstThatWorks" => Self::FirstThatWorks,
            "defineVars" => Self::DefineVars,
            "defineMarker" => Self::DefineMarker,
            "defineConsts" => Self::DefineConsts,
            "createTheme" => Self::CreateTheme,
            "types" => Self::Types,
            "when" => Self::When,
            "defaultMarker" => Self::DefaultMarker,
            "env" => Self::Env,
            "unstable_defineVarsNested" => Self::DefineVarsNested,
            "unstable_defineConstsNested" => Self::DefineConstsNested,
            "unstable_createThemeNested" => Self::CreateThemeNested,
            "unstable_conditional" => Self::Conditional,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedSymbol {
    Named(String),
    Default,
    Namespace,
}

/// One import binding as the evaluator sees it (every import declaration in
/// the file, not just stylex sources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecord {
    pub local: String,
    pub source: String,
    pub imported: ImportedSymbol,
    /// Span of the declaring ImportDeclaration; treeshake compensation inserts
    /// exactly before it (babel `importPath.insertBefore`).
    pub decl_span: Span,
    /// Span of the declaration's source string literal (raw text incl. quotes).
    pub source_span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct ImportTable {
    /// Locals bound to the whole stylex namespace (default/namespace/require).
    pub stylex_namespaces: BTreeSet<String>,
    /// The same locals in source order (upstream state.stylexImport is an
    /// insertion-ordered Set; the sx fallback walks it in order).
    pub stylex_namespace_order: Vec<String>,
    /// Locals bound to individual stylex named imports.
    pub named: BTreeMap<String, StylexNamedImport>,
    /// Matched import-source specifiers (upstream `state.importPaths`).
    pub import_paths: BTreeSet<String>,
    /// Every value import binding in the file, keyed by local name.
    pub all_imports: BTreeMap<String, ImportRecord>,
    /// `state.atomImports`: local name → imported name, or [`ATOM_NAMESPACE`].
    pub atom_imports: BTreeMap<String, String>,
    /// Locals of every `@stylexjs/atoms` specifier, type-only ones included;
    /// the atoms visitor's scope-binding fallback accepts these too.
    pub atom_binding_locals: BTreeSet<String>,
}

impl ImportTable {
    /// No stylex binding of any kind: nothing in the file can compile, so the
    /// caller may skip semantic analysis entirely.
    pub fn is_dormant(&self) -> bool {
        self.stylex_namespaces.is_empty()
            && self.named.is_empty()
            && self.import_paths.is_empty()
            && self.atom_imports.is_empty()
            && self.atom_binding_locals.is_empty()
    }

    pub fn atom_import(&self, name: &str) -> Option<&str> {
        self.atom_imports.get(name).map(String::as_str)
    }

    pub fn is_atom_binding_local(&self, name: &str) -> bool {
        self.atom_binding_locals.contains(name)
    }

    fn add_stylex_namespace(&mut self, local: &str) {
        if self.stylex_namespaces.insert(local.to_string()) {
            self.stylex_namespace_order.push(local.to_string());
        }
    }

    pub fn is_stylex_namespace(&self, name: &str) -> bool {
        self.stylex_namespaces.contains(name)
    }

    pub fn named_binding(&self, name: &str) -> Option<StylexNamedImport> {
        self.named.get(name).copied()
    }

    pub fn import_record(&self, local: &str) -> Option<&ImportRecord> {
        self.all_imports.get(local)
    }

    /// Import records whose specifier passes the theme-file suffix check.
    pub fn theme_file_imports<'t>(
        &'t self,
        theme_extension: &'t str,
    ) -> impl Iterator<Item = &'t ImportRecord> {
        self.all_imports.values().filter(move |r| {
            crate::module_resolution::is_theme_specifier(&r.source, theme_extension)
        })
    }
}

pub fn scan_imports(
    program: &Program<'_>,
    options: &ResolvedOptions,
) -> Result<ImportTable, StylexError> {
    let mut table = ImportTable::default();
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(decl) => {
                scan_import_declaration(decl, options, &mut table)?
            }
            Statement::VariableDeclaration(decl) => scan_requires(decl, options, &mut table)?,
            _ => {}
        }
    }
    Ok(table)
}

fn scan_import_declaration(
    decl: &ImportDeclaration<'_>,
    options: &ResolvedOptions,
    table: &mut ImportTable,
) -> Result<(), StylexError> {
    let source = decl.source.value.as_str();
    if source == ATOMS_SOURCE {
        scan_atoms_declaration(decl, table);
        return Ok(());
    }
    if decl.import_kind.is_type() {
        return Ok(());
    }
    let Some(specifiers) = &decl.specifiers else {
        return Ok(());
    };
    let is_stylex_source = options.is_import_source(source);
    // `as` is subtractive: it turns off default, namespace and every other
    // named import for this source, and turns on exactly one named export.
    let alias = options.import_as(source);
    for specifier in specifiers {
        match specifier {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                let local = spec.local.name.to_string();
                if is_stylex_source && alias.is_none() {
                    table.import_paths.insert(source.to_string());
                    table.add_stylex_namespace(&local);
                }
                table.all_imports.insert(
                    local.clone(),
                    ImportRecord {
                        local,
                        source: source.to_string(),
                        imported: ImportedSymbol::Default,
                        decl_span: decl.span,
                        source_span: decl.source.span,
                    },
                );
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                let local = spec.local.name.to_string();
                if is_stylex_source && alias.is_none() {
                    table.import_paths.insert(source.to_string());
                    table.add_stylex_namespace(&local);
                }
                table.all_imports.insert(
                    local.clone(),
                    ImportRecord {
                        local,
                        source: source.to_string(),
                        imported: ImportedSymbol::Namespace,
                        decl_span: decl.span,
                        source_span: decl.source.span,
                    },
                );
            }
            ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                if spec.import_kind.is_type() {
                    continue;
                }
                let imported = match &spec.imported {
                    ModuleExportName::IdentifierName(id) => id.name.to_string(),
                    ModuleExportName::StringLiteral(lit) => lit.value.to_string(),
                    ModuleExportName::IdentifierReference(id) => id.name.to_string(),
                };
                let local = spec.local.name.to_string();
                if is_stylex_source {
                    match alias {
                        Some(alias) if alias == imported => {
                            table.import_paths.insert(source.to_string());
                            table.add_stylex_namespace(&local);
                        }
                        Some(_) => {}
                        None => {
                            table.import_paths.insert(source.to_string());
                            if let Some(binding) = StylexNamedImport::from_imported_name(&imported)
                            {
                                table.named.insert(local.clone(), binding);
                            }
                        }
                    }
                }
                table.all_imports.insert(
                    local.clone(),
                    ImportRecord {
                        local,
                        source: source.to_string(),
                        imported: ImportedSymbol::Named(imported),
                        decl_span: decl.span,
                        source_span: decl.source.span,
                    },
                );
            }
        }
    }
    Ok(())
}

// parity: imports.js ATOMS_SOURCES branch. `import type` locals are collected
// anyway: the atoms visitor's scope-binding fallback still resolves them.
fn scan_atoms_declaration(decl: &ImportDeclaration<'_>, table: &mut ImportTable) {
    let Some(specifiers) = &decl.specifiers else {
        return;
    };
    let type_only = decl.import_kind.is_type();
    for specifier in specifiers {
        let (local, imported) = match specifier {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                (spec.local.name.as_str(), ATOM_NAMESPACE.to_string())
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                (spec.local.name.as_str(), ATOM_NAMESPACE.to_string())
            }
            // An inline `type` specifier is not filtered out here upstream.
            ImportDeclarationSpecifier::ImportSpecifier(spec) => (
                spec.local.name.as_str(),
                match &spec.imported {
                    ModuleExportName::IdentifierName(id) => id.name.to_string(),
                    ModuleExportName::StringLiteral(lit) => lit.value.to_string(),
                    ModuleExportName::IdentifierReference(id) => id.name.to_string(),
                },
            ),
        };
        table.atom_binding_locals.insert(local.to_string());
        if !type_only {
            table.atom_imports.insert(local.to_string(), imported);
        }
    }
}

// parity: imports.js readRequires — top-level `const x = require('src')` only.
fn scan_requires(
    decl: &VariableDeclaration<'_>,
    options: &ResolvedOptions,
    table: &mut ImportTable,
) -> Result<(), StylexError> {
    for declarator in &decl.declarations {
        let Some(Expression::CallExpression(call)) = &declarator.init else {
            continue;
        };
        let Expression::Identifier(callee) = &call.callee else {
            continue;
        };
        if callee.name != "require" || call.arguments.len() != 1 {
            continue;
        }
        let Some(Expression::StringLiteral(source)) = call.arguments[0].as_expression() else {
            continue;
        };
        let source = source.value.as_str();
        if source == ATOMS_SOURCE {
            scan_atoms_require(&declarator.id, table);
            continue;
        }
        // readRequires never calls importAs: an aliased source still binds its
        // namespace and its individual API names here. Not a bug to "fix".
        if !options.is_import_source(source) {
            continue;
        }
        table.import_paths.insert(source.to_string());
        match &declarator.id {
            BindingPattern::BindingIdentifier(id) => {
                table.add_stylex_namespace(&id.name);
            }
            BindingPattern::ObjectPattern(pattern) => {
                for property in &pattern.properties {
                    let PropertyKey::StaticIdentifier(key) = &property.key else {
                        continue;
                    };
                    let BindingPattern::BindingIdentifier(value) = &property.value else {
                        continue;
                    };
                    if let Some(binding) = StylexNamedImport::from_imported_name(&key.name) {
                        table.named.insert(value.name.to_string(), binding);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// parity: imports.js readRequires ATOMS_SOURCES branch.
fn scan_atoms_require(id: &BindingPattern<'_>, table: &mut ImportTable) {
    match id {
        BindingPattern::BindingIdentifier(name) => {
            table
                .atom_imports
                .insert(name.name.to_string(), ATOM_NAMESPACE.to_string());
        }
        BindingPattern::ObjectPattern(pattern) => {
            for property in &pattern.properties {
                let PropertyKey::StaticIdentifier(key) = &property.key else {
                    continue;
                };
                let BindingPattern::BindingIdentifier(value) = &property.value else {
                    continue;
                };
                table
                    .atom_imports
                    .insert(value.name.to_string(), key.name.to_string());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn scan(source: &str) -> ImportTable {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
        assert!(!ret.fatal_error, "parse failed");
        scan_imports(&ret.program, &ResolvedOptions::default()).unwrap()
    }

    #[test]
    fn namespace_default_and_named_forms() {
        let table = scan(
            "import * as stylex from '@stylexjs/stylex';\n\
             import sx from 'stylex';\n\
             import { create, keyframes as kf, when } from '@stylexjs/stylex';\n\
             import { helper } from './helpers';\n",
        );
        assert!(table.is_stylex_namespace("stylex"));
        assert!(table.is_stylex_namespace("sx"));
        assert_eq!(
            table.named_binding("create"),
            Some(StylexNamedImport::Create)
        );
        assert_eq!(
            table.named_binding("kf"),
            Some(StylexNamedImport::Keyframes)
        );
        assert_eq!(table.named_binding("when"), Some(StylexNamedImport::When));
        assert_eq!(table.named_binding("helper"), None);
        assert!(table.import_record("helper").is_some());
        assert_eq!(
            table.import_record("kf").unwrap().imported,
            ImportedSymbol::Named("keyframes".to_string())
        );
        assert!(table.import_paths.contains("stylex"));
        assert!(table.import_paths.contains("@stylexjs/stylex"));
    }

    #[test]
    fn type_imports_are_skipped() {
        let table = scan(
            "import type { create } from '@stylexjs/stylex';\n\
             import { type props, keyframes } from '@stylexjs/stylex';\n",
        );
        assert_eq!(table.named_binding("create"), None);
        assert_eq!(table.named_binding("props"), None);
        assert_eq!(
            table.named_binding("keyframes"),
            Some(StylexNamedImport::Keyframes)
        );
    }

    #[test]
    fn requires_top_level_only() {
        let table = scan(
            "const stylex = require('@stylexjs/stylex');\n\
             const { create, props: p } = require('stylex');\n\
             function f() { const nested = require('stylex'); }\n",
        );
        assert!(table.is_stylex_namespace("stylex"));
        assert!(!table.is_stylex_namespace("nested"));
        assert_eq!(
            table.named_binding("create"),
            Some(StylexNamedImport::Create)
        );
        assert_eq!(table.named_binding("p"), Some(StylexNamedImport::Props));
    }

    #[test]
    fn custom_import_sources() {
        let allocator = Allocator::default();
        let ret = Parser::new(
            &allocator,
            "import * as css from 'foo-bar';\nimport * as other from 'baz';",
            SourceType::tsx(),
        )
        .parse();
        let options = crate::options::CompilerOptions::from_json(
            &serde_json::json!({ "importSources": ["foo-bar"] }),
        )
        .unwrap()
        .resolve()
        .unwrap();
        let table = scan_imports(&ret.program, &options).unwrap();
        assert!(table.is_stylex_namespace("css"));
        assert!(!table.is_stylex_namespace("other"));
    }

    fn scan_with(source: &str, options: serde_json::Value) -> ImportTable {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
        assert!(!ret.fatal_error, "parse failed");
        let options = crate::options::CompilerOptions::from_json(&options)
            .unwrap()
            .resolve()
            .unwrap();
        scan_imports(&ret.program, &options).unwrap()
    }

    fn aliased(source: &str) -> ImportTable {
        scan_with(
            source,
            serde_json::json!({ "importSources": [{ "from": "my-lib", "as": "css" }] }),
        )
    }

    #[test]
    fn aliased_source_binds_only_the_named_export() {
        for (code, local) in [
            ("import { css } from 'my-lib';", "css"),
            ("import { css as sx } from 'my-lib';", "sx"),
            ("import { 'css' as sx } from 'my-lib';", "sx"),
        ] {
            let table = aliased(code);
            assert!(table.is_stylex_namespace(local), "{code}");
            assert!(table.import_paths.contains("my-lib"), "{code}");
        }
        // `as` is subtractive for everything else on that source.
        for code in [
            "import * as css from 'my-lib';",
            "import css from 'my-lib';",
            "import { create } from 'my-lib';",
            "import type { css } from 'my-lib';",
            "export { css } from 'my-lib';",
        ] {
            let table = aliased(code);
            assert!(table.is_dormant(), "{code}");
        }
        // Mixed declarations register only the matching specifier.
        let table = aliased("import d, { css, create } from 'my-lib';");
        assert!(table.is_stylex_namespace("css"));
        assert!(!table.is_stylex_namespace("d"));
        assert_eq!(table.named_binding("create"), None);
        // The same export under two locals registers both.
        let table = aliased("import { css, css as sx } from 'my-lib';");
        assert!(table.is_stylex_namespace("css"));
        assert!(table.is_stylex_namespace("sx"));
    }

    #[test]
    fn require_ignores_the_alias() {
        let table = aliased("const css = require('my-lib');");
        assert!(table.is_stylex_namespace("css"));
        let table = aliased("const { create } = require('my-lib');");
        assert_eq!(
            table.named_binding("create"),
            Some(StylexNamedImport::Create)
        );
        // `css` is not an API name, so the destructure path drops it.
        let table = aliased("const { css } = require('my-lib');");
        assert!(!table.is_stylex_namespace("css"));
        assert_eq!(table.named_binding("css"), None);
    }

    #[test]
    fn aliased_and_plain_sources_are_independent() {
        let table = scan_with(
            "import { css } from 'my-lib';\nimport * as stylex from 'other-lib';\n",
            serde_json::json!({
                "importSources": [{ "from": "my-lib", "as": "css" }, "other-lib"]
            }),
        );
        assert!(table.is_stylex_namespace("css"));
        assert!(table.is_stylex_namespace("stylex"));
        // Matching is raw-string equality on the specifier.
        let table = aliased("import { css } from 'my-lib/css';");
        assert!(table.is_dormant());
        // Overriding a built-in switches it to the aliased form too.
        let table = scan_with(
            "import * as stylex from '@stylexjs/stylex';\nimport { css } from '@stylexjs/stylex';\n",
            serde_json::json!({
                "importSources": [{ "from": "@stylexjs/stylex", "as": "css" }]
            }),
        );
        assert!(!table.is_stylex_namespace("stylex"));
        assert!(table.is_stylex_namespace("css"));
    }

    #[test]
    fn atom_import_forms() {
        let table = scan(
            "import d from '@stylexjs/atoms';\n\
             import * as ns from '@stylexjs/atoms';\n\
             import { color, 'padding' as p, type gap, default as def } from '@stylexjs/atoms';\n\
             const r = require('@stylexjs/atoms');\n\
             let l = require('@stylexjs/atoms');\n\
             const { color: c2, width } = require('@stylexjs/atoms');\n",
        );
        let got: Vec<(&str, &str)> = table
            .atom_imports
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("c2", "color"),
                ("color", "color"),
                ("d", "*"),
                ("def", "default"),
                ("gap", "gap"),
                ("l", "*"),
                ("ns", "*"),
                ("p", "padding"),
                ("r", "*"),
                ("width", "width"),
            ]
        );
        assert!(!table.is_dormant());
        assert!(table.import_paths.is_empty());
    }

    #[test]
    fn type_atom_imports_bind_without_atom_imports_entries() {
        let table = scan("import type x from '@stylexjs/atoms';\nexport const a = x.display;\n");
        assert!(table.atom_imports.is_empty());
        assert!(table.is_atom_binding_local("x"));
        assert!(!table.is_dormant());
    }

    #[test]
    fn atoms_specifier_is_matched_literally() {
        for source in [
            "import x from '@stylexjs/atoms/';",
            "import x from '@stylexjs/atoms/babel-transform';",
            "export { color } from '@stylexjs/atoms';",
            "const x = require('@stylexjs/atoms').default;",
            "function f() { const x = require('@stylexjs/atoms'); }",
        ] {
            let table = scan(source);
            assert!(table.atom_imports.is_empty(), "{source}");
            assert!(table.atom_binding_locals.is_empty(), "{source}");
        }
    }

    #[test]
    fn theme_file_imports_are_listed() {
        let table = scan(
            "import { colors } from './tokens.stylex';\n\
             import { sizes } from './sizes.stylex.const';\n\
             import { helper } from './helpers';\n",
        );
        let themed: Vec<&str> = table
            .theme_file_imports(crate::module_resolution::THEME_FILE_EXTENSION)
            .map(|r| r.local.as_str())
            .collect();
        assert_eq!(themed, vec!["colors", "sizes"]);
    }
}
