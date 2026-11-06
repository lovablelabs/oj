use std::cell::RefCell;
use std::collections::hash_map::Entry;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol};
use tsrs_checker::{Checker, ContextFlags, LiteralValue, Type};
use tsrs_compiler::Program;
use tsrs_core::PathSubstitutions;
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_core::stringutil::{self, Comparison};
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{
    CompilerOptions, ModuleResolutionKind, P, PatternKind, RESOLUTION_MODE_NONE, ResolutionMode,
    TextRange, try_parse_pattern,
};
use tsrs_lsproto as lsproto;
use tsrs_module::packagejson::{ExportsOrImports, InfoCacheEntryExt, JSONValueType};
use tsrs_modulespecifiers::{
    self as modulespecifiers, ImportModuleSpecifierEndingPreference, ModuleSpecifierEnding,
};
use tsrs_printer::{self as printer, QuoteChar};
use tsrs_scanner as scanner;
use tsrs_tsoptions as tsoptions;

use crate::astnav;
use crate::completions::*;
use crate::languageservice::LanguageService;
use crate::lsutil::{ScriptElementKind, ScriptElementKindModifier};
use crate::utilities::{
    TrackerValue, get_contextual_type_from_parent, is_in_comment, is_in_string,
    new_case_clause_tracker, skip_constraint,
};

// string_completions.go:30
pub struct CompletionsFromTypes {
    types: Vec<P<Type>>,
    is_new_identifier: bool,
}

// string_completions.go:35
pub struct CompletionsFromProperties {
    symbols: Vec<P<Symbol>>,
    has_index_signature: bool,
}

// string_completions.go:40
pub struct PathCompletion {
    name: String,
    kind: ScriptElementKind,
    extension: String,
}

// string_completions.go:46
pub struct PathCompletions {
    entries: Vec<PathCompletion>,
    replacement_span: Option<lsproto::Range>,
}

// string_completions.go:51
#[derive(Default)]
pub struct StringLiteralCompletions {
    from_types: Option<CompletionsFromTypes>,
    from_properties: Option<CompletionsFromProperties>,
    from_paths: Option<PathCompletions>,
}

// The string value of a checker StringLiteralType (Go `t.AsLiteralType().Value().(string)`).
fn string_literal_value(t: P<Type>) -> &'static str {
    match t.as_literal_type().value() {
        Some(LiteralValue::String(s)) => s,
        _ => panic!("interface conversion: interface {{}} is not string"),
    }
}

impl LanguageService {
    // string_completions.go:57
    pub(crate) fn get_string_literal_completions(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
        position: u32,
        context_token: Option<P<Node>>,
        checker: &mut Checker,
        compiler_options: P<CompilerOptions>,
        include_symbols: bool,
    ) -> Option<CompletionList> {
        if is_in_reference_comment(file, position) {
            let completion = self.get_triple_slash_reference_completions(
                file,
                position,
                self.get_program(),
                checker,
            );
            return self.convert_path_completions(ctx, completion, file, position);
        }
        if is_in_string(file, position, context_token) {
            let context_token = context_token?;
            if !ast::is_string_literal_like(context_token) {
                return None;
            }
            let entries = self.get_string_literal_completion_entries(
                ctx,
                file,
                context_token,
                position,
                checker,
            );
            return self.convert_string_literal_completions(
                ctx,
                entries,
                context_token,
                file,
                position,
                checker,
                compiler_options,
                include_symbols,
            );
        }
        None
    }

    // string_completions.go:95
    fn convert_string_literal_completions(
        &self,
        ctx: &Context,
        completion: Option<StringLiteralCompletions>,
        context_token: P<Node>,
        file: P<SourceFile>,
        position: u32,
        type_checker: &mut Checker,
        options: P<CompilerOptions>,
        include_symbols: bool,
    ) -> Option<CompletionList> {
        let completion = completion?;

        let optional_replacement_range =
            self.create_range_from_string_literal_like_content(file, context_token, position);
        if completion.from_paths.is_some() {
            return self.convert_path_completions(ctx, completion.from_paths, file, position);
        } else if let Some(completion) = completion.from_properties {
            let mut data = CompletionDataData::new(file.as_node());
            data.symbols = completion.symbols;
            data.completion_kind = CompletionKind::String;
            data.is_new_identifier_location = completion.has_index_signature;
            data.context_token = Some(context_token);
            let (_, mut items) = match self.get_completion_entries_from_symbols(
                ctx,
                type_checker,
                &data,
                Some(context_token), /*replacementToken*/
                position,
                file,
                options,
                include_symbols, /*includeSymbols*/
            ) {
                Ok(result) => result,
                Err(err) => panic!("{}", err.message),
            };
            let default_commit_characters =
                get_default_commit_characters(completion.has_index_signature);
            let item_defaults = self.set_item_defaults(
                ctx,
                position,
                file,
                &mut items,
                Some(&default_commit_characters),
                optional_replacement_range,
            );
            return Some(CompletionList {
                is_incomplete: false,
                item_defaults,
                items,
                ..Default::default()
            });
        } else if let Some(completion) = completion.from_types {
            let quote_char = if context_token.kind() == Kind::NoSubstitutionTemplateLiteral {
                QuoteChar::Backtick
            } else if context_token.text().starts_with('\'') {
                QuoteChar::SingleQuote
            } else {
                QuoteChar::DoubleQuote
            };
            let mut items: Vec<CompletionItem> = completion
                .types
                .iter()
                .map(|&t| {
                    let name = printer::escape_string(string_literal_value(t), quote_char);
                    let lsp_item = Self::create_lsp_completion_item(
                        ctx,
                        &name,
                        "", /*insertText*/
                        "", /*filterText*/
                        SORT_TEXT_LOCATION_PRIORITY,
                        ScriptElementKind::String,
                        ScriptElementKindModifier::None,
                        self.get_replacement_range_for_context_token(
                            file,
                            Some(context_token),
                            position,
                        ),
                        None, /*commitCharacters*/
                        None, /*labelDetails*/
                        file,
                        position,
                        false, /*isMemberCompletion*/
                        false, /*isSnippet*/
                        false, /*hasAction*/
                        false, /*preselect*/
                        "",    /*source*/
                        None,  /*autoImportEntryData*/
                        None,  /*additionalTextEdits*/
                        None,  /*detail*/
                    );
                    CompletionItem::new(lsp_item)
                })
                .collect();
            let default_commit_characters =
                get_default_commit_characters(completion.is_new_identifier);
            let item_defaults = self.set_item_defaults(
                ctx,
                position,
                file,
                &mut items,
                Some(&default_commit_characters),
                None, /*optionalReplacementSpan*/
            );
            return Some(CompletionList {
                is_incomplete: false,
                item_defaults,
                items,
                ..Default::default()
            });
        }
        None
    }

    // string_completions.go:206
    fn convert_path_completions(
        &self,
        ctx: &Context,
        completion: Option<PathCompletions>,
        file: P<SourceFile>,
        position: u32,
    ) -> Option<CompletionList> {
        let completion = completion?;
        let is_new_identifier_location = true; // The user may type in a path that doesn't yet exist, creating a "new identifier" with respect to the collection of identifiers the server is aware of.
        let default_commit_characters = get_default_commit_characters(is_new_identifier_location);
        let mut items: Vec<CompletionItem> = completion
            .entries
            .iter()
            .map(|path_completion| {
                let mut detail = path_completion.name.clone();
                if !path_completion.name.ends_with(&path_completion.extension) {
                    detail += &path_completion.extension;
                }
                let lsp_item = Self::create_lsp_completion_item(
                    ctx,
                    &path_completion.name,
                    "", /*insertText*/
                    "", /*filterText*/
                    SORT_TEXT_LOCATION_PRIORITY,
                    path_completion.kind,
                    kind_modifiers_from_extension(&path_completion.extension),
                    completion.replacement_span,
                    None, /*commitCharacters*/
                    None, /*labelDetails*/
                    file,
                    position,
                    false, /*isMemberCompletion*/
                    false, /*isSnippet*/
                    false, /*hasAction*/
                    false, /*preselect*/
                    "",    /*source*/
                    None,  /*autoImportEntryData*/
                    None,  /*additionalTextEdits*/
                    Some(detail),
                );
                CompletionItem::new(lsp_item)
            })
            .collect();
        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut items,
            Some(&default_commit_characters),
            None, /*optionalReplacementSpan*/
        );
        Some(CompletionList { is_incomplete: false, item_defaults, items, ..Default::default() })
    }

    // string_completions.go:263
    fn get_string_literal_completion_entries(
        &self,
        _ctx: &Context,
        file: P<SourceFile>,
        node: P<Node>,
        position: u32,
        type_checker: &mut Checker,
    ) -> Option<StringLiteralCompletions> {
        let parent = walk_up_parentheses(node.parent().unwrap());
        let mut kind = parent.kind();
        // Go's `fallthrough` from the call case into the module-name case.
        if matches!(kind, Kind::CallExpression | Kind::NewExpression | Kind::JsxAttribute) {
            if !is_require_call_argument(node) && !ast::is_import_call(parent) {
                let argument_node = if parent.kind() == Kind::JsxAttribute {
                    parent.parent().unwrap()
                } else {
                    node
                };
                let argument_info =
                    get_argument_info_for_completions(argument_node, position, file, type_checker);
                // Get string literal completions from specialized signatures of the target
                // i.e. declare function f(a: 'A');
                // f("/*completion position*/")
                let argument_info = argument_info?;

                let result = get_string_literal_completions_from_signature(
                    argument_info.invocation,
                    node,
                    &argument_info,
                    type_checker,
                );
                if result.is_some() {
                    return Some(StringLiteralCompletions {
                        from_types: result,
                        ..Default::default()
                    });
                }
                return Some(StringLiteralCompletions {
                    from_types: from_contextual_type(ContextFlags::None, node, type_checker),
                    ..Default::default()
                });
            }
            kind = Kind::ImportDeclaration; // is `require("")` or `require(""` or `import("")`
        }
        match kind {
            Kind::LiteralType => {
                let grandparent = walk_up_parentheses(parent.parent().unwrap());
                if grandparent.kind() == Kind::ImportType {
                    return self.get_string_literal_completions_from_module_names(
                        file,
                        node,
                        self.get_program(),
                        type_checker,
                    );
                }
                from_unionable_literal_type(grandparent, parent, position, type_checker)
            }
            Kind::PropertyAssignment => {
                if ast::is_object_literal_expression(parent.parent().unwrap())
                    && parent.name() == Some(node)
                {
                    // Get quoted name of properties of the object literal expression
                    // i.e. interface ConfigFiles {
                    //          'jspm:dev': string
                    //      }
                    //      let files: ConfigFiles = {
                    //          '/*completion position*/'
                    //      }
                    //
                    //      function foo(c: ConfigFiles) {}
                    //      foo({
                    //          '/*completion position*/'
                    //      });
                    return Some(StringLiteralCompletions {
                        from_properties: string_literal_completions_for_object_literal(
                            type_checker,
                            parent.parent().unwrap(),
                        ),
                        ..Default::default()
                    });
                }
                if ast::find_ancestor(parent.parent(), ast::is_call_like_expression).is_some() {
                    let mut uniques: FxHashSet<String> = FxHashSet::default();
                    let contextual =
                        type_checker.get_contextual_type_exported(node, ContextFlags::None);
                    let mut string_literal_types =
                        get_string_literal_types(contextual, Some(&mut uniques), type_checker);
                    let contextual_ignoring = type_checker
                        .get_contextual_type_exported(node, ContextFlags::IgnoreNodeInferences);
                    string_literal_types.extend(get_string_literal_types(
                        contextual_ignoring,
                        Some(&mut uniques),
                        type_checker,
                    ));
                    return to_string_literal_completions_from_types(string_literal_types);
                }
                Some(StringLiteralCompletions {
                    from_types: from_contextual_type(ContextFlags::None, node, type_checker),
                    ..Default::default()
                })
            }
            Kind::ElementAccessExpression => {
                let expression = parent.expression().unwrap();
                let argument_expression = parent.as_element_access_expression().argument_expression;
                if node == ast::skip_parentheses(argument_expression) {
                    // Get all names of properties on the expression
                    // i.e. interface A {
                    //      'prop1': string
                    // }
                    // let a: A;
                    // a['/*completion position*/']
                    let t = type_checker.get_type_at_location(expression);
                    return Some(StringLiteralCompletions {
                        from_properties: Some(string_literal_completions_from_properties(
                            t,
                            type_checker,
                        )),
                        ..Default::default()
                    });
                }
                None
            }
            Kind::ImportDeclaration
            | Kind::ExportDeclaration
            | Kind::ExternalModuleReference
            | Kind::JSDocImportTag => {
                // Get all known external module names or complete a path to a module
                // i.e. import * as ns from "/*completion position*/";
                //      var y = import("/*completion position*/");
                //      import x = require("/*completion position*/");
                //      var y = require("/*completion position*/");
                //      export * from "/*completion position*/";
                self.get_string_literal_completions_from_module_names(
                    file,
                    node,
                    self.get_program(),
                    type_checker,
                )
            }
            Kind::CaseClause => {
                let tracker = new_case_clause_tracker(
                    type_checker,
                    parent.parent().unwrap().as_case_block().clauses.nodes(),
                );
                let contextual_types =
                    from_contextual_type(ContextFlags::IgnoreNodeInferences, node, type_checker)?;
                let literals: Vec<P<Type>> = contextual_types
                    .types
                    .into_iter()
                    .filter(|&t| {
                        !tracker
                            .has_value(&TrackerValue::String(string_literal_value(t).to_string()))
                    })
                    .collect();
                Some(StringLiteralCompletions {
                    from_types: Some(CompletionsFromTypes {
                        types: literals,
                        is_new_identifier: false,
                    }),
                    ..Default::default()
                })
            }
            Kind::ImportSpecifier | Kind::ExportSpecifier => {
                // Complete string aliases in `import { "|" } from` and `export { "|" } from`
                let specifier = parent;
                if let Some(property_name) = specifier.property_name()
                    && node != property_name
                {
                    return None; // Don't complete in `export { "..." as "|" } from`
                }
                let named_imports_or_exports = specifier.parent().unwrap();
                let module_specifier = if named_imports_or_exports.kind() == Kind::NamedImports {
                    named_imports_or_exports.parent().unwrap().parent()
                } else {
                    named_imports_or_exports.parent()
                };
                let module_specifier = module_specifier?;
                let module_specifier_symbol =
                    type_checker.get_symbol_at_location_exported(module_specifier)?;
                let exports =
                    type_checker.get_exports_and_properties_of_module(module_specifier_symbol);
                let existing: FxHashSet<&'static str> = named_imports_or_exports
                    .elements()
                    .iter()
                    .map(|n| n.property_name_or_name().unwrap().text())
                    .collect();
                let uniques: Vec<P<Symbol>> = exports
                    .into_iter()
                    .filter(|e| {
                        e.name() != ast::INTERNAL_SYMBOL_NAME_DEFAULT
                            && !existing.contains(e.name())
                    })
                    .collect();
                Some(StringLiteralCompletions {
                    from_properties: Some(CompletionsFromProperties {
                        symbols: uniques,
                        has_index_signature: false,
                    }),
                    ..Default::default()
                })
            }
            Kind::BinaryExpression => {
                if parent.as_binary_expression().operator_token.kind() == Kind::InKeyword {
                    let t =
                        type_checker.get_type_at_location(parent.as_binary_expression().right());
                    let properties = get_properties_for_completion(t, type_checker);
                    return Some(StringLiteralCompletions {
                        from_properties: Some(CompletionsFromProperties {
                            symbols: properties
                                .into_iter()
                                .filter(|s| {
                                    s.value_declaration().is_none()
                                        || !ast::is_private_identifier_class_element_declaration(
                                            s.value_declaration().unwrap(),
                                        )
                                })
                                .collect(),
                            has_index_signature: false,
                        }),
                        ..Default::default()
                    });
                }
                Some(StringLiteralCompletions {
                    from_types: from_contextual_type(ContextFlags::None, node, type_checker),
                    ..Default::default()
                })
            }
            _ => {
                let result =
                    from_contextual_type(ContextFlags::IgnoreNodeInferences, node, type_checker);
                if result.is_some() {
                    return Some(StringLiteralCompletions {
                        from_types: result,
                        ..Default::default()
                    });
                }
                Some(StringLiteralCompletions {
                    from_types: from_contextual_type(ContextFlags::None, node, type_checker),
                    ..Default::default()
                })
            }
        }
    }
}

// string_completions.go:440
fn from_contextual_type(
    context_flags: ContextFlags,
    node: P<Node>,
    type_checker: &mut Checker,
) -> Option<CompletionsFromTypes> {
    // Get completion for string literal from string literal type
    // i.e. var x: "hi" | "hello" = "/*completion position*/"
    let contextual = get_contextual_type_from_parent(node, type_checker, context_flags);
    to_completions_from_types(get_string_literal_types(contextual, None, type_checker))
}

// string_completions.go:446
fn to_completions_from_types(types: Vec<P<Type>>) -> Option<CompletionsFromTypes> {
    if types.is_empty() {
        return None;
    }
    Some(CompletionsFromTypes { types, is_new_identifier: false })
}

// string_completions.go:456
fn to_string_literal_completions_from_types(
    types: Vec<P<Type>>,
) -> Option<StringLiteralCompletions> {
    let result = to_completions_from_types(types)?;
    Some(StringLiteralCompletions { from_types: Some(result), ..Default::default() })
}

// string_completions.go:466
fn from_unionable_literal_type(
    grandparent: P<Node>,
    parent: P<Node>,
    position: u32,
    type_checker: &mut Checker,
) -> Option<StringLiteralCompletions> {
    match grandparent.kind() {
        Kind::CallExpression
        | Kind::ExpressionWithTypeArguments
        | Kind::JsxOpeningElement
        | Kind::JsxSelfClosingElement
        | Kind::NewExpression
        | Kind::TaggedTemplateExpression
        | Kind::TypeReference => {
            let type_argument = ast::find_ancestor(parent, |n| n.parent() == Some(grandparent));
            if let Some(type_argument) = type_argument {
                let t = type_checker.get_type_argument_constraint_exported(type_argument);
                return Some(StringLiteralCompletions {
                    from_types: Some(CompletionsFromTypes {
                        types: get_string_literal_types(t, None, type_checker),
                        is_new_identifier: false,
                    }),
                    ..Default::default()
                });
            }
            None
        }
        Kind::IndexedAccessType => {
            // Get all apparent property names
            // i.e. interface Foo {
            //          foo: string;
            //          bar: string;
            //      }
            //      let x: Foo["/*completion position*/"]
            let index_type = grandparent.as_indexed_access_type_node().index_type;
            let object_type = grandparent.as_indexed_access_type_node().object_type;
            if !index_type.loc().contains_inclusive(position.into()) {
                return None;
            }
            let t = type_checker.get_type_from_type_node_exported(object_type);
            Some(StringLiteralCompletions {
                from_properties: Some(string_literal_completions_from_properties(t, type_checker)),
                ..Default::default()
            })
        }
        Kind::UnionType => {
            let result = from_unionable_literal_type(
                walk_up_parentheses(grandparent.parent().unwrap()),
                parent,
                position,
                type_checker,
            )?;
            let already_used_types =
                get_already_used_types_in_string_literal_union(grandparent, parent);
            if let Some(result) = result.from_properties {
                return Some(StringLiteralCompletions {
                    from_properties: Some(CompletionsFromProperties {
                        symbols: result
                            .symbols
                            .into_iter()
                            .filter(|s| !already_used_types.iter().any(|u| u == s.name()))
                            .collect(),
                        has_index_signature: result.has_index_signature,
                    }),
                    ..Default::default()
                });
            } else if let Some(result) = result.from_types {
                return Some(StringLiteralCompletions {
                    from_types: Some(CompletionsFromTypes {
                        types: result
                            .types
                            .into_iter()
                            .filter(|&t| {
                                !already_used_types.iter().any(|u| u == string_literal_value(t))
                            })
                            .collect(),
                        is_new_identifier: false,
                    }),
                    ..Default::default()
                });
            }
            None
        }
        Kind::PropertySignature => {
            let constraint =
                get_constraint_of_type_argument_property(Some(grandparent), type_checker);
            Some(StringLiteralCompletions {
                from_types: Some(CompletionsFromTypes {
                    types: get_string_literal_types(constraint, None, type_checker),
                    is_new_identifier: false,
                }),
                ..Default::default()
            })
        }
        _ => None,
    }
}

// string_completions.go:555
fn string_literal_completions_for_object_literal(
    type_checker: &mut Checker,
    object_literal_expression: P<Node>,
) -> Option<CompletionsFromProperties> {
    let contextual_type =
        type_checker.get_contextual_type_exported(object_literal_expression, ContextFlags::None)?;

    let completions_type = type_checker.get_contextual_type_exported(
        object_literal_expression,
        ContextFlags::IgnoreNodeInferences,
    );
    let symbols = get_properties_for_object_expression(
        contextual_type,
        completions_type,
        object_literal_expression,
        type_checker,
    );

    Some(CompletionsFromProperties {
        symbols,
        has_index_signature: has_index_signature(contextual_type, type_checker),
    })
}

// string_completions.go:578
fn string_literal_completions_from_properties(
    t: P<Type>,
    type_checker: &mut Checker,
) -> CompletionsFromProperties {
    CompletionsFromProperties {
        symbols: type_checker
            .get_apparent_properties(t)
            .into_iter()
            .filter(|s| {
                !(s.value_declaration().is_some()
                    && ast::is_private_identifier_class_element_declaration(
                        s.value_declaration().unwrap(),
                    ))
            })
            .collect(),
        has_index_signature: has_index_signature(t, type_checker),
    }
}

impl LanguageService {
    // string_completions.go:587
    fn get_string_literal_completions_from_module_names(
        &self,
        file: P<SourceFile>,
        node: P<Node>,
        program: &'static Program,
        checker: &mut Checker,
    ) -> Option<StringLiteralCompletions> {
        let text_start = astnav::get_start_of_node(node, file, false /*includeJSDoc*/) + 1;
        let replacement_span = self.path_completion_replacement_span(
            file,
            get_directory_fragment_range(node.text(), text_start),
        )?;
        let name_and_kinds = self
            .get_string_literal_completions_from_module_names_worker(file, node, program, checker);
        Some(StringLiteralCompletions {
            from_paths: Some(PathCompletions {
                entries: to_path_completions(name_and_kinds),
                replacement_span,
            }),
            ..Default::default()
        })
    }
}

// string_completions.go:612
fn to_path_completions(names: Vec<ModuleCompletionNameAndKind>) -> Vec<PathCompletion> {
    names
        .into_iter()
        .map(|name_and_kind| PathCompletion {
            name: name_and_kind.name,
            kind: modulet_to_script_element_kind(name_and_kind.kind),
            extension: name_and_kind.extension,
        })
        .collect()
}

impl LanguageService {
    // string_completions.go:622 (Go `(*lsproto.Range, bool)`: None = not ok, Some(None) = no span)
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn path_completion_replacement_span(
        &self,
        file: P<SourceFile>,
        text_range: Option<TextRange>,
    ) -> Option<Option<lsproto::Range>> {
        let Some(text_range) = text_range else {
            return Some(None);
        };
        let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
            text_range.pos().expect_source("completion ranges have source positions"),
            text_range.end().expect_source("completion ranges have source positions"),
            file,
        );
        if !fidelity.is_exact() {
            return None;
        }
        Some(Some(lsp_range))
    }
}

// string_completions.go:633
fn modulet_to_script_element_kind(kind: ModuleCompletionKind) -> ScriptElementKind {
    match kind {
        ModuleCompletionKind::Directory => ScriptElementKind::Directory,
        ModuleCompletionKind::File => ScriptElementKind::ScriptElement,
        ModuleCompletionKind::ExternalModuleName => ScriptElementKind::ExternalModuleName,
    }
}

// string_completions.go:645
fn is_any_directory_separator(r: char) -> bool {
    r == '/' || r == '\\'
}

// Replace everything after the last directory separator that appears
// string_completions.go:650
fn get_directory_fragment_range(text: &str, text_start: u32) -> Option<TextRange> {
    let index = text.rfind(is_any_directory_separator);
    let mut offset = 0;
    if let Some(index) = index {
        offset = index + 1;
    }
    let length = text.len() - offset;
    if length == 0 {
        return None;
    }
    let offset = u32::try_from(offset).expect("path fragment offset exceeds u32");
    let length = u32::try_from(length).expect("path fragment length exceeds u32");
    Some(TextRange::new(text_start + offset, text_start + offset + length))
}

impl LanguageService {
    // string_completions.go:663
    fn get_string_literal_completions_from_module_names_worker(
        &self,
        file: P<SourceFile>,
        node: P<Node>,
        program: &'static Program,
        checker: &mut Checker,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let literal_value = tspath::normalize_slashes(node.text());
        let mut mode = RESOLUTION_MODE_NONE;
        if ast::is_string_literal_like(node) {
            mode = program.get_mode_for_usage_location(file, node);
        }

        let script_path = file.path().clone();
        let script_directory = script_path.get_directory_path();
        let options = program.options();
        let extension_options = self.get_extension_options(
            &options,
            ReferenceKind::ModuleSpecifier,
            file,
            mode,
            Some(checker),
        );

        let paths_size = options.paths.as_ref().map_or(0, |paths| paths.len());
        if is_path_relative_to_script(&literal_value)
            || (paths_size == 0
                && (tspath::is_rooted_disk_path(&literal_value) || tspath::is_url(&literal_value)))
        {
            self.get_completion_entries_for_relative_modules(
                &literal_value,
                &script_directory,
                program,
                &script_path,
                &extension_options,
            )
        } else {
            self.get_completion_entries_for_non_relative_modules(
                &literal_value,
                &script_directory,
                mode,
                program,
                checker,
                &extension_options,
            )
        }
    }

    // Check all of the declared modules and those in node modules. Possible sources of modules:
    //
    //	Modules that are found by the type checker
    //	Modules found via patterns from "paths" compiler option
    //	Modules from node_modules (i.e. those listed in package.json)
    //	    This includes all files that are found in node_modules/moduleName/ with acceptable file extensions
    // string_completions.go:707
    fn get_completion_entries_for_non_relative_modules(
        &self,
        fragment: &str,
        script_path: &str,
        mode: ResolutionMode,
        program: &'static Program,
        type_checker: &mut Checker,
        extension_options: &ExtensionOptions,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let compiler_options = program.options();
        let paths = &compiler_options.paths;

        let result = RefCell::new(ModuleCompletionNameAndKindSet::default());
        let module_resolution = compiler_options.get_module_resolution_kind();

        if let Some(paths) = paths
            && !paths.is_empty()
        {
            let absolute = compiler_options.get_paths_base_path(program.get_current_directory());
            self.add_completion_entries_from_paths(
                &mut result.borrow_mut(),
                program,
                fragment,
                &absolute,
                extension_options,
                paths,
            );
        }

        let fragment_directory = get_fragment_directory(fragment);
        for ambient_name in
            get_ambient_module_completions(fragment, &fragment_directory, type_checker)
        {
            result.borrow_mut().add(ModuleCompletionNameAndKind {
                name: ambient_name,
                kind: ModuleCompletionKind::ExternalModuleName,
                extension: String::new(),
            });
        }

        self.get_completion_entries_from_typings(
            program,
            script_path,
            &fragment_directory,
            extension_options,
            &mut result.borrow_mut(),
        );

        if module_resolution_uses_node_modules(module_resolution) {
            // If looking for a global package name, don't just include everything in `node_modules` because that includes dependencies' own dependencies.
            // (But do if we didn't find anything, e.g. 'package.json' missing.)
            let mut found_global = false;
            if fragment_directory.is_empty() {
                for module_name in self.enumerate_node_modules_visible_to_script(script_path) {
                    let module_result = ModuleCompletionNameAndKind {
                        name: module_name,
                        kind: ModuleCompletionKind::ExternalModuleName,
                        extension: String::new(),
                    };
                    if !result.borrow().names.contains_key(&module_result.name) {
                        found_global = true;
                        result.borrow_mut().add(module_result);
                    }
                }
            }
            if !found_global {
                let resolve_package_json_exports =
                    compiler_options.get_resolve_package_json_exports();
                let resolve_package_json_imports =
                    compiler_options.get_resolve_package_json_imports();
                let seen_package_scope = std::cell::Cell::new(false);
                let conditions = tsrs_module::get_conditions(&compiler_options, mode);

                // Returns true if the search should stop.
                let exports_or_imports_lookup = |lookup_table: Option<&ExportsOrImports>,
                                                 fragment: &str,
                                                 base_directory: &str,
                                                 is_exports: bool,
                                                 is_imports: bool|
                 -> bool {
                    let Some(lookup_table) =
                        lookup_table.filter(|t| t.type_() == JSONValueType::Object)
                    else {
                        return lookup_table
                            .is_some_and(|t| t.type_() != JSONValueType::NotPresent);
                    };
                    let keys: Vec<String> = lookup_table.as_object().keys().cloned().collect();
                    self.add_completion_entries_from_paths_or_exports_or_imports(
                        &mut result.borrow_mut(),
                        program,
                        is_exports,
                        is_imports,
                        fragment,
                        base_directory,
                        extension_options,
                        &keys,
                        &|key: &str| -> Vec<String> {
                            let Some(key_value) = lookup_table.as_object().get(key) else {
                                return Vec::new();
                            };
                            let pattern =
                                get_pattern_from_first_matching_condition(key_value, &conditions);
                            if pattern.is_empty() {
                                return Vec::new();
                            }
                            if key.ends_with('/') && pattern.ends_with('/') {
                                return vec![pattern + "*"];
                            }
                            vec![pattern]
                        },
                        &tsrs_module::compare_pattern_keys,
                    );
                    true
                };

                let imports_lookup = |directory: &str| {
                    if resolve_package_json_imports && !seen_package_scope.get() {
                        let package_file = tspath::combine_paths(directory, &["package.json"]);
                        let package_json_info = program.get_package_json_info(&package_file);
                        if package_json_info.exists() {
                            seen_package_scope.set(true);
                            exports_or_imports_lookup(
                                Some(&package_json_info.unwrap().contents.unwrap().imports),
                                fragment,
                                directory,
                                false, /*isExports*/
                                true,  /*isImports*/
                            );
                        }
                    }
                };

                let node_modules_directory_or_imports_lookup = |ancestor: &str| -> Option<()> {
                    let node_modules = tspath::combine_paths(ancestor, &["node_modules"]);
                    if self.directory_exists(&node_modules) {
                        self.get_completion_entries_for_directory_fragment(
                            fragment,
                            &node_modules,
                            extension_options,
                            program,
                            false, /* moduleSpecifierIsRelative */
                            "",
                            &mut result.borrow_mut(),
                        );
                    }
                    imports_lookup(ancestor);
                    None
                };

                let ancestor_lookup = |ancestor: &str| -> Option<()> {
                    if fragment_directory.is_empty() || !resolve_package_json_exports {
                        return node_modules_directory_or_imports_lookup(ancestor);
                    }
                    let components = tspath::get_path_components(fragment, "");
                    let mut components: &[String] = &components[1..]; // shift off empty root
                    if components.is_empty() {
                        node_modules_directory_or_imports_lookup(ancestor);
                        return None;
                    }
                    let mut package_path = components[0].clone();
                    components = &components[1..];
                    if package_path.starts_with('@') {
                        if components.is_empty() {
                            node_modules_directory_or_imports_lookup(ancestor);
                            return None;
                        }
                        let sub_name = components[0].clone();
                        components = &components[1..];
                        package_path = tspath::combine_paths(&package_path, &[&sub_name]);
                    }
                    if resolve_package_json_imports && package_path.starts_with('#') {
                        imports_lookup(ancestor);
                        return None;
                    }
                    let package_directory =
                        tspath::combine_paths(ancestor, &["node_modules", &package_path]);
                    let package_file = tspath::combine_paths(&package_directory, &["package.json"]);
                    let package_json_info = program.get_package_json_info(&package_file);
                    if package_json_info.exists() {
                        let mut fragment_subpath = components.join("/");
                        if !components.is_empty()
                            && tspath::has_trailing_directory_separator(fragment)
                        {
                            fragment_subpath += "/";
                        }
                        if exports_or_imports_lookup(
                            Some(&package_json_info.unwrap().contents.unwrap().exports),
                            &fragment_subpath,
                            &package_directory,
                            true,  /*isExports*/
                            false, /*isImports*/
                        ) {
                            return None;
                        }
                    }
                    node_modules_directory_or_imports_lookup(ancestor);
                    None
                };

                let global_cache_location = program.get_global_typings_cache_location();
                tspath::for_each_ancestor_directory_stopping_at_global_cache(
                    global_cache_location,
                    script_path,
                    ancestor_lookup,
                );
            }
        }

        result.into_inner().names.into_values().collect()
    }
}

// string_completions.go:875
fn get_fragment_directory(fragment: &str) -> String {
    if !contains_slash(fragment) {
        return String::new();
    }
    if tspath::has_trailing_directory_separator(fragment) {
        return fragment.to_string();
    }
    tspath::get_directory_path(fragment)
}

// string_completions.go:885
fn get_pattern_from_first_matching_condition(
    target: &ExportsOrImports,
    conditions: &[String],
) -> String {
    if target.type_() == JSONValueType::String {
        return target.as_string().to_string();
    }
    if target.type_() == JSONValueType::Object {
        let obj = target.as_object();
        if let Some(result) = obj.keys().into_iter().find_map(|condition| {
            if (condition == "default"
                || conditions.iter().any(|c| c == condition)
                || (conditions.iter().any(|c| c == "types")
                    && tsrs_module::is_applicable_versioned_types_key(condition)))
                && let Some(pattern) = obj.get(condition)
            {
                return Some(get_pattern_from_first_matching_condition(pattern, conditions));
            }

            None
        }) {
            return result;
        }
    }
    String::new()
}

// string_completions.go:904
fn get_ambient_module_completions(
    fragment: &str,
    fragment_directory: &str,
    type_checker: &mut Checker,
) -> Vec<String> {
    let ambient_modules = type_checker.get_ambient_modules();
    let mut non_relative_module_names: Vec<String> = Vec::new();
    non_relative_module_names.extend(ambient_modules.into_iter().filter_map(|sym| {
        let module_name = get_ambient_module_name(sym);
        if module_name.starts_with(fragment) && !module_name.contains('*') {
            Some(module_name)
        } else {
            None
        }
    }));

    if !fragment_directory.is_empty() {
        let module_name_with_separator =
            tspath::ensure_trailing_directory_separator(fragment_directory);
        for module_name in &mut non_relative_module_names {
            if let Some(stripped) = module_name.strip_prefix(module_name_with_separator.as_str()) {
                *module_name = stripped.to_string();
            }
        }
    }
    non_relative_module_names
}

// string_completions.go:923
fn get_ambient_module_name(symbol: P<Symbol>) -> String {
    let declaration = ast::get_non_augmentation_declaration(symbol);
    if let Some(declaration) = declaration
        && ast::is_module_with_string_literal_name(declaration)
    {
        return declaration.name().unwrap().text().to_string();
    }
    stringutil::strip_quotes(symbol.name()).to_string()
}

impl LanguageService {
    // string_completions.go:931
    fn get_completion_entries_from_typings(
        &self,
        program: &'static Program,
        script_path: &str,
        fragment_directory: &str,
        extension_options: &ExtensionOptions,
        result: &mut ModuleCompletionNameAndKindSet,
    ) {
        let options = program.options();
        let mut seen: FxHashMap<String, bool> = FxHashMap::default();

        let (type_roots, _) = options.get_effective_type_roots(program.get_current_directory());

        for root in &type_roots {
            self.get_completion_entries_from_typings_directories(
                root,
                &options,
                fragment_directory,
                extension_options,
                program,
                &mut seen,
                result,
            );
        }

        let global_cache_location = program.get_global_typings_cache_location();
        tspath::for_each_ancestor_directory_stopping_at_global_cache(
            global_cache_location,
            script_path,
            |directory| -> Option<()> {
                let types_dir = tspath::combine_paths(directory, &["node_modules/@types"]);
                self.get_completion_entries_from_typings_directories(
                    &types_dir,
                    &options,
                    fragment_directory,
                    extension_options,
                    program,
                    &mut seen,
                    result,
                );
                None
            },
        );
    }

    // string_completions.go:955
    fn get_completion_entries_from_typings_directories(
        &self,
        directory: &str,
        options: &CompilerOptions,
        fragment_directory: &str,
        extension_options: &ExtensionOptions,
        program: &'static Program,
        seen: &mut FxHashMap<String, bool>,
        result: &mut ModuleCompletionNameAndKindSet,
    ) {
        if !self.directory_exists(directory) {
            return;
        }

        for type_directory_name in self.get_directories(directory) {
            let package_name = tsrs_module::unmangle_scoped_package_name(&type_directory_name);
            if let Some(types) = &options.types
                && !types.is_empty()
                && !types.contains(&package_name)
            {
                continue;
            }

            if fragment_directory.is_empty() {
                let entry = seen.entry(package_name);
                if matches!(&entry, Entry::Occupied(entry) if *entry.get()) {
                    continue;
                }
                result.add(ModuleCompletionNameAndKind {
                    name: entry.key().clone(),
                    kind: ModuleCompletionKind::ExternalModuleName,
                    extension: String::new(),
                });
                entry.insert_entry(true);
            } else {
                let base_directory = tspath::combine_paths(directory, &[&type_directory_name]);
                let remaining_fragment = try_remove_directory_prefix(
                    fragment_directory,
                    &package_name,
                    program.use_case_sensitive_file_names(),
                );
                if let Some(remaining_fragment) = remaining_fragment {
                    self.get_completion_entries_for_directory_fragment(
                        &remaining_fragment,
                        &base_directory,
                        extension_options,
                        program,
                        false,
                        "",
                        result,
                    );
                }
            }
        }
    }
}

// string_completions.go:1000
pub fn try_remove_directory_prefix(
    path: &str,
    prefix: &str,
    use_case_sensitive_file_names: bool,
) -> Option<String> {
    let without_prefix =
        tspath::trim_file_path_prefix(path, prefix, use_case_sensitive_file_names)?;
    let mut without_prefix = without_prefix;
    if without_prefix.starts_with('/') || without_prefix.starts_with('\\') {
        without_prefix = &without_prefix[1..];
    }
    Some(without_prefix.to_string())
}

impl LanguageService {
    // string_completions.go:1011
    fn enumerate_node_modules_visible_to_script(&self, script_path: &str) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        let global_cache_location = self.get_program().get_global_typings_cache_location();

        tspath::for_each_ancestor_directory_stopping_at_global_cache(
            global_cache_location,
            script_path,
            |directory| -> Option<()> {
                let package_json_path = tspath::combine_paths(directory, &["package.json"]);
                let package_json_info =
                    self.get_program().get_package_json_info(&package_json_path);
                if package_json_info.exists()
                    && let Some(contents) = package_json_info.unwrap().contents
                {
                    contents.range_dependencies(|name, _version, _dependency_field| {
                        if !name.starts_with("@types/") {
                            result.push(name.to_string());
                        }
                        true
                    });
                }
                None
            },
        );

        result
    }

    // string_completions.go:1032
    fn get_extension_options(
        &self,
        options: &CompilerOptions,
        reference_kind: ReferenceKind,
        file: P<SourceFile>,
        mode: ResolutionMode,
        checker: Option<&mut Checker>,
    ) -> ExtensionOptions {
        let extensions_to_search = get_supported_extensions_for_module_resolution(
            options,
            &self.get_program().command_line().content_mapper_extensions(),
            checker,
        );

        ExtensionOptions {
            extensions_to_search,
            reference_kind,
            importing_source_file: file,
            ending_preference: self.user_preferences().import_module_specifier_ending,
            resolution_mode: mode,
        }
    }
}

// string_completions.go:1050
fn get_supported_extensions_for_module_resolution(
    options: &CompilerOptions,
    extra_extensions: &[String],
    checker: Option<&mut Checker>,
) -> Vec<String> {
    // file extensions from ambient modules declarations e.g. *.css
    let mut extensions: Vec<String> = Vec::new();
    if let Some(checker) = checker {
        let ambient_modules = checker.get_ambient_modules();
        extensions.extend(ambient_modules.into_iter().filter_map(|module| {
            let name = get_ambient_module_name(module);
            if !name.starts_with("*.") || name.contains('/') {
                return None;
            }
            Some(name[1..].to_string())
        }));
    }
    let supported_extensions = tsoptions::get_supported_extensions(Some(options), extra_extensions);
    extensions.extend(supported_extensions.into_iter().flatten());
    let module_resolution = options.get_module_resolution_kind();
    if module_resolution_uses_node_modules(module_resolution) {
        return tsoptions::get_supported_extensions_with_json_if_resolve_json_module(
            Some(options),
            &[extensions],
        )
        .into_iter()
        .flatten()
        .collect();
    }
    extensions
}

// string_completions.go:1074
fn module_resolution_uses_node_modules(module_resolution: ModuleResolutionKind) -> bool {
    module_resolution >= ModuleResolutionKind::NODE16
        && module_resolution <= ModuleResolutionKind::NODE_NEXT
        || module_resolution == ModuleResolutionKind::BUNDLER
}

// Returns true if the path is explicitly relative (i.e. relative to . or ..)
// string_completions.go:1080
fn is_path_relative_to_script(path: &str) -> bool {
    path.starts_with("./") || path.starts_with("../")
}

impl LanguageService {
    // string_completions.go:1084
    fn get_completion_entries_for_relative_modules(
        &self,
        literal_value: &str,
        script_directory: &str,
        program: &'static Program,
        script_path: &Path,
        extension_options: &ExtensionOptions,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let options = program.options();
        if let Some(root_dirs) = options.root_dirs.as_ref().filter(|r| !r.is_empty()) {
            self.get_completion_entries_for_directory_fragment_with_root_dirs(
                root_dirs,
                literal_value,
                script_directory,
                program,
                script_path,
                extension_options,
            )
        } else {
            let mut result = ModuleCompletionNameAndKindSet::default();
            self.get_completion_entries_for_directory_fragment(
                literal_value,
                script_directory,
                extension_options,
                program,
                true, /*moduleSpecifierIsRelative*/
                script_path,
                &mut result,
            );
            result.names.into_values().collect()
        }
    }

    // string_completions.go:1115
    fn get_completion_entries_for_directory_fragment_with_root_dirs(
        &self,
        root_dirs: &[String],
        fragment: &str,
        script_directory: &str,
        program: &'static Program,
        exclude: &str,
        extension_options: &ExtensionOptions,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let options = program.options();
        let base_path = if options.project.is_empty() {
            program.get_current_directory().to_string()
        } else {
            options.project.clone()
        };
        let ignore_case = !program.use_case_sensitive_file_names();
        let base_directories = get_base_directories_from_root_dirs(
            root_dirs,
            &base_path,
            script_directory,
            ignore_case,
        );

        let mut all_completions: Vec<ModuleCompletionNameAndKind> = Vec::new();
        for base_directory in base_directories {
            let mut result = ModuleCompletionNameAndKindSet::default();
            self.get_completion_entries_for_directory_fragment(
                fragment,
                &base_directory,
                extension_options,
                program,
                true, /*moduleSpecifierIsRelative*/
                exclude,
                &mut result,
            );
            all_completions.extend(result.names.into_iter().map(|(_, entry)| entry));
        }

        // Deduplicate based on name, kind, and extension
        deduplicate_module_completions(all_completions)
    }
}

// getBaseDirectoriesFromRootDirs takes a script path and returns paths for all potential folders
// that could be merged with its containing folder via the "rootDirs" compiler option.
// string_completions.go:1155
fn get_base_directories_from_root_dirs(
    root_dirs: &[String],
    base_path: &str,
    script_directory: &str,
    ignore_case: bool,
) -> Vec<String> {
    // Make all paths absolute/normalized if they are not already
    let mut normalized_root_dirs: Vec<String> = Vec::with_capacity(root_dirs.len());
    normalized_root_dirs.extend(root_dirs.iter().map(|root_directory| {
        let normalized_path = if tspath::is_rooted_disk_path(root_directory) {
            root_directory.clone()
        } else {
            tspath::combine_paths(base_path, &[root_directory])
        };
        tspath::ensure_trailing_directory_separator(&tspath::normalize_path(&normalized_path))
    }));

    // Determine the path to the directory containing the script relative to the root directory it is contained within
    let mut relative_directory = String::new();
    let compare_paths_options = ComparePathsOptions {
        use_case_sensitive_file_names: !ignore_case,
        current_directory: base_path.to_string(),
    };
    if let Some(root_directory) = normalized_root_dirs.iter().find(|root_directory| {
        tspath::contains_path(root_directory, script_directory, &compare_paths_options)
    }) {
        if root_directory.len() > script_directory.len() {
            relative_directory = String::new();
        } else {
            relative_directory = script_directory[root_directory.len()..].to_string();
        }
    }

    // Now find a path for each potential directory that is to be merged with the one containing the script
    let mut directories: Vec<String> = Vec::new();
    directories.extend(normalized_root_dirs.iter().map(|root_directory| {
        tspath::remove_trailing_directory_separator(&tspath::combine_paths(
            root_directory,
            &[&relative_directory],
        ))
        .to_string()
    }));
    directories.push(tspath::remove_trailing_directory_separator(script_directory).to_string());

    deduplicate_strings(directories)
}

// string_completions.go:1195
fn deduplicate_strings(slice: Vec<String>) -> Vec<String> {
    if slice.len() <= 1 {
        return slice;
    }
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut result: Vec<String> = Vec::new();
    result.extend(
        slice.into_iter().filter_map(|s| if seen.insert(s.clone()) { Some(s) } else { None }),
    );
    result
}

// string_completions.go:1210
fn deduplicate_module_completions(
    completions: Vec<ModuleCompletionNameAndKind>,
) -> Vec<ModuleCompletionNameAndKind> {
    if completions.len() <= 1 {
        return completions;
    }
    let mut seen: FxHashSet<(String, ModuleCompletionKind, String)> = FxHashSet::default();
    let mut result: Vec<ModuleCompletionNameAndKind> = Vec::new();
    result.extend(completions.into_iter().filter_map(|c| {
        let k = (c.name.clone(), c.kind, c.extension.clone());
        if seen.insert(k) { Some(c) } else { None }
    }));
    result
}

// string_completions.go:1231
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum ModuleCompletionKind {
    Directory,
    File,
    ExternalModuleName,
}

// string_completions.go:1239
#[derive(Clone, Debug)]
pub struct ModuleCompletionNameAndKind {
    name: String,
    kind: ModuleCompletionKind,
    extension: String,
}

// string_completions.go:1245 (Go map; insertion order here)
#[derive(Default)]
pub struct ModuleCompletionNameAndKindSet {
    names: OrderedMap<String, ModuleCompletionNameAndKind>,
}

impl ModuleCompletionNameAndKindSet {
    // string_completions.go:1249
    fn add(&mut self, entry: ModuleCompletionNameAndKind) {
        let existing = self.names.get(&entry.name);
        if existing.is_none_or(|existing| existing.kind < entry.kind) {
            self.names.insert(entry.name.clone(), entry);
        }
    }
}

// string_completions.go:1256
struct ExtensionOptions {
    extensions_to_search: Vec<String>,
    reference_kind: ReferenceKind,
    importing_source_file: P<SourceFile>,
    ending_preference: ImportModuleSpecifierEndingPreference,
    resolution_mode: ResolutionMode,
}

// string_completions.go:1264
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    FileName,
    ModuleSpecifier,
}

impl LanguageService {
    // Given a path ending at a directory, gets the completions for the path.
    // string_completions.go:1272
    fn get_completion_entries_for_directory_fragment(
        &self,
        fragment: &str,
        script_directory: &str,
        extension_options: &ExtensionOptions,
        program: &'static Program,
        module_specifier_is_relative: bool,
        exclude: &str,
        result: &mut ModuleCompletionNameAndKindSet,
    ) {
        let mut fragment = tspath::normalize_slashes(fragment);

        // Remove the basename from the path.
        // We don't use the basename to filter completions: the client is responsible for that filtering.
        if !tspath::has_trailing_directory_separator(&fragment) {
            fragment = tspath::get_directory_path(&fragment);
        }

        if fragment.is_empty() {
            fragment = ".".to_string();
        }

        fragment = tspath::ensure_trailing_directory_separator(&fragment);

        let base_directory = tspath::resolve_path(script_directory, &[&fragment]);
        if !module_specifier_is_relative {
            // Check for a version redirect.
            let package_json_directory =
                program.get_nearest_ancestor_directory_with_package_json(&base_directory);
            if !package_json_directory.is_empty() {
                let package_json_path =
                    tspath::combine_paths(&package_json_directory, &["package.json"]);
                let package_json_info = program.get_package_json_info(&package_json_path);
                if let Some(contents) = package_json_info.and_then(|i| i.contents)
                    && contents.types_versions.type_() == JSONValueType::Object
                {
                    let version_paths = contents.get_version_paths(None);
                    let paths = version_paths.get_paths();
                    if let Some(paths) = paths.filter(|p| !p.is_empty()) {
                        let path_in_package = base_directory
                            [tspath::ensure_trailing_directory_separator(
                                &package_json_directory,
                            )
                            .len()..]
                            .to_string();
                        if self.add_completion_entries_from_paths(
                            result,
                            program,
                            &path_in_package,
                            &package_json_directory,
                            extension_options,
                            paths,
                        ) {
                            // One of the `versionPaths` was matched, which will block relative resolution
                            // to files and folders from here.
                            // All reachable paths given the pattern match are already added.
                            return;
                        }
                    }
                }
            }
        }

        if !self.directory_exists(&base_directory) {
            return;
        }

        // Enumerate all available files.
        let files = self.read_directory(
            &base_directory,
            &extension_options.extensions_to_search,
            &["./*".to_string()], /*include*/
        );

        for file_path in files {
            if tspath::compare_paths(
                exclude,
                &file_path,
                &ComparePathsOptions {
                    use_case_sensitive_file_names: program.use_case_sensitive_file_names(),
                    current_directory: program.get_current_directory().to_string(),
                },
            ) == std::cmp::Ordering::Equal
            {
                continue; // Avoid self-imports
            }

            let (name, extension) = get_filename_with_extension_option(
                &tspath::get_base_file_name(&file_path),
                program,
                extension_options,
                false, /*isExportsOrImportsWildcard*/
            );
            result.add(ModuleCompletionNameAndKind {
                name,
                kind: ModuleCompletionKind::File,
                extension,
            });
        }

        // Get folder completion as well.
        let directories = self.get_directories(&base_directory);

        for directory in directories {
            let directory_name = tspath::get_base_file_name(&directory);
            if directory_name != "@types" {
                result.add(ModuleCompletionNameAndKind {
                    name: directory_name.clone(),
                    kind: ModuleCompletionKind::Directory,
                    extension: String::new(),
                });
            }
        }
    }

    // Returns true if `fragment` was a match for any `paths`
    // (which should indicate whether any other path completions should be offered).
    // string_completions.go:1373
    fn add_completion_entries_from_paths(
        &self,
        result: &mut ModuleCompletionNameAndKindSet,
        program: &'static Program,
        fragment: &str,
        base_directory: &str,
        extension_options: &ExtensionOptions,
        paths: &OrderedMap<String, impl PathSubstitutions>,
    ) -> bool {
        let get_patterns_for_keys = |key: &str| -> Vec<String> {
            paths.get(key).map(|v| v.substitutions().to_vec()).unwrap_or_default()
        };
        let compare_paths = |a: &str, b: &str| -> Comparison {
            let length_a = a.find('*').unwrap_or(a.len());
            let length_b = b.find('*').unwrap_or(b.len());
            length_b.cmp(&length_a)
        };
        let keys: Vec<String> = paths.keys().cloned().collect();
        self.add_completion_entries_from_paths_or_exports_or_imports(
            result,
            program,
            false, /*isExports*/
            false, /*isImports*/
            fragment,
            base_directory,
            extension_options,
            &keys,
            &get_patterns_for_keys,
            &compare_paths,
        )
    }

    // Returns true if `fragment` was a match for any `paths`
    // (which should indicate whether any other path completions should be offered).
    // string_completions.go:1413
    fn add_completion_entries_from_paths_or_exports_or_imports(
        &self,
        result: &mut ModuleCompletionNameAndKindSet,
        program: &'static Program,
        is_exports: bool,
        is_imports: bool,
        fragment: &str,
        base_directory: &str,
        extension_options: &ExtensionOptions,
        keys: &[String],
        get_patterns_for_key: &dyn Fn(&str) -> Vec<String>,
        compare_paths: &dyn Fn(&str, &str) -> Comparison,
    ) -> bool {
        struct PathResult {
            results: Vec<ModuleCompletionNameAndKind>,
            matched: bool,
        }
        let mut path_results: Vec<PathResult> = Vec::new();
        let mut matched_path: Option<String> = None;
        for key in keys {
            if key == "." {
                continue;
            }
            let mut normalized_key = key.strip_prefix("./").unwrap_or(key).to_string(); // Remove leading "./"
            if (is_exports || is_imports) && key.ends_with('/') {
                // Normalize trailing "/" to "/*"
                normalized_key += "*";
            }
            let patterns = get_patterns_for_key(key);
            if !patterns.is_empty() {
                let path_pattern = try_parse_pattern(&normalized_key);
                if !path_pattern.is_valid() {
                    continue;
                }
                let is_match = path_pattern.matches(fragment);
                let mut is_longest_match = false;
                if is_match {
                    if let Some(matched) = &matched_path {
                        is_longest_match = compare_paths(&normalized_key, matched)
                            == stringutil::COMPARISON_LESS_THAN;
                    } else {
                        is_longest_match = true;
                    }
                }
                if is_longest_match {
                    // If this is a higher priority match than anything we've seen so far, previous results from matches are invalid, e.g.
                    // for `import {} from "some-package/|"` with a typesVersions:
                    // {
                    //   "bar/*": ["bar/*"], // <-- 1. We add 'bar', but 'bar/*' doesn't match yet.
                    //   "*": ["dist/*"],    // <-- 2. We match here and add files from dist. 'bar' is still ok because it didn't come from a match.
                    //   "foo/*": ["foo/*"]  // <-- 3. We matched '*' earlier and added results from dist, but if 'foo/*' also matched,
                    // }                               results in dist would not be visible. 'bar' still stands because it didn't come from a match.
                    //                                 This is especially important if `dist/foo` is a folder, because if we fail to clear results
                    //                                 added by the '*' match, after typing `"some-package/foo/|"` we would get file results from both
                    //                                 ./dist/foo and ./foo, when only the latter will actually be resolvable.
                    //                                 See pathCompletionsTypesVersionsWildcard6.ts.
                    matched_path = Some(normalized_key.clone());
                    path_results.retain(|pr| !pr.matched);
                }
                if path_pattern.kind == PatternKind::Exact
                    || matched_path.is_none()
                    || compare_paths(&normalized_key, matched_path.as_ref().unwrap())
                        != stringutil::COMPARISON_GREATER_THAN
                {
                    path_results.push(PathResult {
                        matched: is_match,
                        results: self.get_completions_for_path_mapping(
                            &normalized_key,
                            &patterns,
                            fragment,
                            base_directory,
                            is_exports,
                            is_imports,
                            extension_options,
                            program,
                        ),
                    });
                }
            }
        }

        for pr in path_results {
            for res in pr.results {
                result.add(res);
            }
        }

        matched_path.is_some()
    }

    // string_completions.go:1500
    fn get_completions_for_path_mapping(
        &self,
        path: &str,
        patterns: &[String],
        fragment: &str,
        package_directory: &str,
        is_exports: bool,
        is_imports: bool,
        extension_options: &ExtensionOptions,
        program: &'static Program,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let mut fragment_directory = get_fragment_directory(fragment);
        if !fragment_directory.is_empty() {
            fragment_directory = tspath::ensure_trailing_directory_separator(&fragment_directory);
        }
        let just_path_mapping_name = |name: &str,
                                      kind: ModuleCompletionKind,
                                      extension: String|
         -> Vec<ModuleCompletionNameAndKind> {
            if name.starts_with(fragment) {
                let mut name = tspath::remove_trailing_directory_separator(name).to_string();
                if !fragment_directory.is_empty() {
                    name = name
                        .strip_prefix(fragment_directory.as_str())
                        .map(std::string::ToString::to_string)
                        .unwrap_or(name);
                }
                return vec![ModuleCompletionNameAndKind { name, kind, extension }];
            }
            Vec::new()
        };

        let parsed_path = try_parse_pattern(path);
        if !parsed_path.is_valid() {
            return Vec::new();
        }
        // No stars in the pattern.
        if parsed_path.kind == PatternKind::Exact {
            // For a path mapping "foo": ["/x/y/z.ts"], add "foo" itself as a completion.
            let pattern = patterns.first().cloned().unwrap_or_default();
            let extension = get_file_extension(&pattern);
            return just_path_mapping_name(path, ModuleCompletionKind::File, extension);
        }

        let PatternKind::Wildcard(star_index) = parsed_path.kind else {
            unreachable!("valid non-exact path patterns have a wildcard")
        };
        let path_prefix = &parsed_path.text[..star_index];
        let path_suffix = &parsed_path.text[star_index + 1..];
        if !fragment.starts_with(path_prefix) {
            // Fragment doesn't match the path mapping prefix at all:
            // we cannot extend it via this path.
            if !path_prefix.starts_with(fragment) {
                return Vec::new();
            }
            let star_is_full_path_component = path.ends_with("/*");
            if star_is_full_path_component {
                return just_path_mapping_name(
                    path_prefix,
                    ModuleCompletionKind::Directory,
                    String::new(), /*extension*/
                );
            }
            // If path is e.g. `foo/bar/*`, and fragment is `foo/b`, then remaining directory prefix is `bar/`,
            let remaining_directory_prefix = &path_prefix[fragment_directory.len()..];
            let mut completions: Vec<ModuleCompletionNameAndKind> = Vec::new();
            completions.extend(patterns.iter().flat_map(|pattern| {
                let mut modules = self.get_modules_for_paths_pattern(
                    "", /*fragment*/
                    package_directory,
                    pattern,
                    is_exports,
                    is_imports,
                    extension_options,
                    program,
                );
                for module in &mut modules {
                    module.name = format!(
                        "{}{}{}",
                        remaining_directory_prefix,
                        module.name,
                        if module.kind == ModuleCompletionKind::File { path_suffix } else { "" }
                    );
                }
                modules
            }));
            return completions;
        }
        let remaining_fragment = &fragment[path_prefix.len()..];
        let mut remaining_directory_fragment = "";
        if !fragment_directory.starts_with(path_prefix) {
            remaining_directory_fragment = &path_prefix[fragment_directory.len()..];
        }
        let mut result = Vec::new();
        result.extend(patterns.iter().flat_map(|pattern| {
            let mut modules = self.get_modules_for_paths_pattern(
                remaining_fragment,
                package_directory,
                pattern,
                is_exports,
                is_imports,
                extension_options,
                program,
            );
            for module in &mut modules {
                module.name = format!(
                    "{}{}{}",
                    remaining_directory_fragment,
                    module.name,
                    if module.kind == ModuleCompletionKind::File { path_suffix } else { "" }
                );
            }
            modules
        }));
        result
    }
}

// string_completions.go:1598
fn get_file_extension(file_name: &str) -> String {
    let mut extension = tspath::try_get_extension_from_path(file_name).to_string();
    if extension.is_empty() {
        extension = tspath::get_any_extension_from_path(
            file_name,
            &[],   /*extensions*/
            false, /*ignoreCase*/
        );
    }
    extension
}

impl LanguageService {
    // The input fragment is relative to the path pattern's prefix:
    // e.g. if path = "bar/_*/baz", and fragment = "bar/_dir", then fragment is "dir".
    // The names are relative to the path pattern's prefix and fragment directory :
    // e.g. if path = "bar/_*/baz", and fragment = "bar/_dir/a", and we find result "abd",
    // the result should be interpreted as "bar/_dir/abd".
    // string_completions.go:1611
    fn get_modules_for_paths_pattern(
        &self,
        fragment: &str,
        package_directory: &str,
        pattern: &str,
        is_exports: bool,
        is_imports: bool,
        extension_options: &ExtensionOptions,
        program: &'static Program,
    ) -> Vec<ModuleCompletionNameAndKind> {
        let parsed = try_parse_pattern(pattern);
        let PatternKind::Wildcard(star_index) = parsed.kind else {
            return Vec::new();
        };

        let prefix = &parsed.text[..star_index];
        let suffix = &parsed.text[star_index + 1..];

        // The prefix has two effective parts: the directory path and the base component after the filepath that is not a
        // full directory component. For example: directory/path/of/prefix/base*
        let normalized_prefix = tspath::resolve_path(prefix, &[]);
        let (normalized_prefix_directory, normalized_prefix_base) =
            if tspath::has_trailing_directory_separator(prefix) {
                (normalized_prefix, String::new())
            } else {
                (
                    tspath::get_directory_path(&normalized_prefix),
                    tspath::get_base_file_name(&normalized_prefix),
                )
            };

        let fragment_has_path = contains_slash(fragment);
        let mut fragment_directory = String::new();
        if fragment_has_path {
            if tspath::has_trailing_directory_separator(fragment) {
                fragment_directory = fragment.to_string();
            } else {
                fragment_directory = tspath::get_directory_path(fragment);
            }
        }

        let options = program.options();
        let ignore_case = !program.use_case_sensitive_file_names();
        let out_dir = &options.out_dir;
        let declaration_dir = &options.declaration_dir;

        // Try and expand the prefix to include any path from the fragment so that we can limit the readDirectory call
        let expanded_prefix_directory = if fragment_has_path {
            tspath::combine_paths(
                &normalized_prefix_directory,
                &[&(normalized_prefix_base.clone() + &fragment_directory)],
            )
        } else {
            normalized_prefix_directory
        };
        // Need to normalize after combining: If we combinePaths("a", "../b"), we want "b" and not "a/../b".
        let base_directory = tspath::normalize_path(&tspath::combine_paths(
            package_directory,
            &[&expanded_prefix_directory],
        ));

        let mut possible_input_base_directory_for_out_dir = String::new();
        let mut possible_input_base_directory_for_declaration_dir = String::new();
        if is_imports {
            if !out_dir.is_empty() {
                possible_input_base_directory_for_out_dir =
                    get_possible_original_input_path_without_changing_ext(
                        &base_directory,
                        ignore_case,
                        out_dir,
                        &|| program.common_source_directory().to_string(),
                    );
            }
            if !declaration_dir.is_empty() {
                possible_input_base_directory_for_declaration_dir =
                    get_possible_original_input_path_without_changing_ext(
                        &base_directory,
                        ignore_case,
                        declaration_dir,
                        &|| program.common_source_directory().to_string(),
                    );
            }
        }

        let normalized_suffix = tspath::normalize_path(suffix);

        let mut declaration_extension = String::new();
        let mut input_extensions: Vec<String> = Vec::new();
        if !normalized_suffix.is_empty() {
            declaration_extension =
                tspath::get_declaration_emit_extension_for_path(&format!("_{normalized_suffix}"));
            input_extensions = tspath::get_possible_original_input_extension_for_extension(
                &format!("_{normalized_suffix}"),
            );
        }

        let mut matching_suffixes: Vec<String> = Vec::new();
        if !declaration_extension.is_empty() {
            matching_suffixes
                .push(tspath::change_extension(&normalized_suffix, &declaration_extension));
        }
        matching_suffixes.extend(
            input_extensions.iter().map(|ext| tspath::change_extension(&normalized_suffix, ext)),
        );
        matching_suffixes.push(normalized_suffix.clone());

        // If we have a suffix, then we read the directory all the way down to avoid returning completions for
        // directories that don't contain files that would match the suffix. A previous comment here was concerned
        // about the case where `normalizedSuffix` includes a `?` character, which should be interpreted literally,
        // but will match any single character as part of the `include` pattern in `tryReadDirectory`. This is not
        // a problem, because (in the extremely unusual circumstance where the suffix has a `?` in it) a `?`
        // interpreted as "any character" can only return *too many* results as compared to the literal
        // interpretation, so we can filter those superfluous results out via `trimPrefixAndSuffix` as we've always
        // done.
        let include_globs: Vec<String> = if normalized_suffix.is_empty() {
            vec!["./*".to_string()]
        } else {
            matching_suffixes.iter().map(|suffix| format!("**/*{suffix}")).collect()
        };

        let is_exports_or_imports_wildcard = (is_exports || is_imports) && pattern.ends_with("/*");

        let trim_prefix_and_suffix = |path: &str, prefix_str: &str| -> String {
            if let Some(result) = matching_suffixes.iter().find_map(|suffix| {
                let inner =
                    without_start_and_end(&tspath::normalize_path(path), prefix_str, suffix)?;
                Some(remove_leading_directory_separator(&inner).to_string())
            }) {
                return result;
            }
            String::new()
        };

        let get_matches_with_prefix = |directory: &str| -> Vec<ModuleCompletionNameAndKind> {
            let complete_prefix = if fragment_has_path {
                directory.to_string()
            } else {
                tspath::ensure_trailing_directory_separator(directory) + &normalized_prefix_base
            };

            let matches = self.read_directory(
                directory,
                &extension_options.extensions_to_search,
                &include_globs,
            );

            let mut result: Vec<ModuleCompletionNameAndKind> = Vec::new();
            result.extend(matches.into_iter().filter_map(|m| {
                let trimmed_with_pattern = trim_prefix_and_suffix(&m, &complete_prefix);
                if trimmed_with_pattern.is_empty() {
                    None
                } else if contains_slash(&trimmed_with_pattern) {
                    let path_components = tspath::get_path_components(
                        remove_leading_directory_separator(&trimmed_with_pattern),
                        "",
                    );
                    if path_components.len() > 1 {
                        Some(ModuleCompletionNameAndKind {
                            name: path_components[1].clone(),
                            kind: ModuleCompletionKind::Directory,
                            extension: String::new(),
                        })
                    } else {
                        None
                    }
                } else {
                    let (name, mut extension) = get_filename_with_extension_option(
                        &trimmed_with_pattern,
                        program,
                        extension_options,
                        is_exports_or_imports_wildcard,
                    );
                    if extension.is_empty() {
                        extension = get_file_extension(&m);
                    }
                    Some(ModuleCompletionNameAndKind {
                        name,
                        kind: ModuleCompletionKind::File,
                        extension,
                    })
                }
            }));
            result
        };

        let get_directory_matches = |directory_name: &str| -> Vec<ModuleCompletionNameAndKind> {
            let directories = self.get_directories(directory_name);
            let mut result: Vec<ModuleCompletionNameAndKind> = Vec::new();
            result.extend(directories.into_iter().filter_map(|dir| {
                if dir == "node_modules" {
                    None
                } else {
                    Some(ModuleCompletionNameAndKind {
                        name: dir,
                        kind: ModuleCompletionKind::Directory,
                        extension: String::new(),
                    })
                }
            }));
            result
        };

        let mut matches: Vec<ModuleCompletionNameAndKind> = Vec::new();
        matches.extend(get_matches_with_prefix(&base_directory));

        if !possible_input_base_directory_for_out_dir.is_empty() {
            matches.extend(get_matches_with_prefix(&possible_input_base_directory_for_out_dir));
        }
        if !possible_input_base_directory_for_declaration_dir.is_empty() {
            matches.extend(get_matches_with_prefix(
                &possible_input_base_directory_for_declaration_dir,
            ));
        }

        // If we had a suffix, we already recursively searched for all possible files that could match
        // it and returned the directories leading to those files. Otherwise, assume any directory could
        // have something valid to import.
        if normalized_suffix.is_empty() {
            matches.extend(get_directory_matches(&base_directory));
            if !possible_input_base_directory_for_out_dir.is_empty() {
                matches.extend(get_directory_matches(&possible_input_base_directory_for_out_dir));
            }
            if !possible_input_base_directory_for_declaration_dir.is_empty() {
                matches.extend(get_directory_matches(
                    &possible_input_base_directory_for_declaration_dir,
                ));
            }
        }

        matches
    }
}

// string_completions.go:1822
fn contains_slash(fragment: &str) -> bool {
    fragment.contains(tspath::DIRECTORY_SEPARATOR as char)
}

// string_completions.go:1826
fn without_start_and_end(s: &str, start: &str, end: &str) -> Option<String> {
    s.strip_circumfix(start, end).map(str::to_string)
}

// string_completions.go:1834
fn remove_leading_directory_separator(path: &str) -> &str {
    path.strip_prefix(tspath::DIRECTORY_SEPARATOR as char).unwrap_or(path)
}

// string_completions.go:1838
fn get_possible_original_input_path_without_changing_ext(
    file_path: &str,
    ignore_case: bool,
    output_dir: &str,
    get_common_source_directory: &dyn Fn() -> String,
) -> String {
    if !output_dir.is_empty() {
        return tspath::resolve_path(
            &get_common_source_directory(),
            &[&tspath::get_relative_path_from_directory(
                output_dir,
                file_path,
                &ComparePathsOptions {
                    use_case_sensitive_file_names: !ignore_case,
                    ..Default::default()
                },
            )],
        );
    }
    file_path.to_string()
}

// string_completions.go:1855
fn get_filename_with_extension_option(
    name: &str,
    program: &'static Program,
    extension_options: &ExtensionOptions,
    is_exports_or_imports_wildcard: bool,
) -> (String, String) {
    let non_js_result =
        modulespecifiers::try_get_real_file_name_for_non_js_declaration_file_name(name);
    if !non_js_result.is_empty() {
        let ext = tspath::try_get_extension_from_path(&non_js_result).to_string();
        return (non_js_result, ext);
    }
    if extension_options.reference_kind == ReferenceKind::FileName {
        return (name.to_string(), tspath::try_get_extension_from_path(name).to_string());
    }

    let mut allowed_endings = modulespecifiers::get_allowed_endings_in_preferred_order(
        &modulespecifiers::UserPreferences {
            import_module_specifier_ending: extension_options.ending_preference,
            ..Default::default()
        },
        program,
        &program.options(),
        extension_options.importing_source_file,
        "", /*oldImportSpecifier*/
        extension_options.resolution_mode,
    );

    if is_exports_or_imports_wildcard {
        // If we're completing `import {} from "foo/|"` and subpaths are available via `"exports": { "./*": "./src/*" }`,
        // the completion must be a (potentially extension-swapped) file name. Dropping extensions and index files is not allowed.
        allowed_endings
            .retain(|&e| e != ModuleSpecifierEnding::Minimal && e != ModuleSpecifierEnding::Index);
    }

    if !allowed_endings.is_empty() && allowed_endings[0] == ModuleSpecifierEnding::TsExtension {
        if tspath::file_extension_is_one_of(name, tspath::SUPPORTED_TS_IMPLEMENTATION_EXTENSIONS) {
            return (name.to_string(), tspath::try_get_extension_from_path(name).to_string());
        }
        let output_extension = tsrs_module::try_get_js_extension_for_file(name, &program.options());
        if !output_extension.is_empty() {
            return (
                tspath::change_extension(name, output_extension),
                output_extension.to_string(),
            );
        }
        return (name.to_string(), tspath::try_get_extension_from_path(name).to_string());
    }

    if !is_exports_or_imports_wildcard
        && !allowed_endings.is_empty()
        && (allowed_endings[0] == ModuleSpecifierEnding::Minimal
            || allowed_endings[0] == ModuleSpecifierEnding::Index)
        && tspath::file_extension_is_one_of(
            name,
            &[
                tspath::EXTENSION_JS,
                tspath::EXTENSION_JSX,
                tspath::EXTENSION_TS,
                tspath::EXTENSION_TSX,
                tspath::EXTENSION_DTS,
            ],
        )
    {
        return (
            tspath::remove_file_extension(name).to_string(),
            tspath::try_get_extension_from_path(name).to_string(),
        );
    }

    let output_extension = tsrs_module::try_get_js_extension_for_file(name, &program.options());
    if !output_extension.is_empty() {
        return (tspath::change_extension(name, output_extension), output_extension.to_string());
    }
    (name.to_string(), tspath::try_get_extension_from_path(name).to_string())
}

// string_completions.go:1911
fn walk_up_parentheses(node: P<Node>) -> P<Node> {
    match node.kind() {
        Kind::ParenthesizedType => ast::walk_up_parenthesized_types(node).unwrap(),
        Kind::ParenthesizedExpression => ast::walk_up_parenthesized_expressions(node).unwrap(),
        _ => node,
    }
}

// string_completions.go:1922
fn get_string_literal_types(
    t: Option<P<Type>>,
    uniques: Option<&mut FxHashSet<String>>,
    type_checker: &mut Checker,
) -> Vec<P<Type>> {
    let Some(t) = t else {
        return Vec::new();
    };
    let mut local_uniques = FxHashSet::default();
    let uniques = match uniques {
        Some(u) => u,
        None => &mut local_uniques,
    };
    let t = skip_constraint(t, type_checker);
    if t.is_union() {
        let mut types: Vec<P<Type>> = Vec::new();
        types.extend(t.types().iter().flat_map(|&element_type| {
            get_string_literal_types(Some(element_type), Some(uniques), type_checker)
        }));
        return types;
    }
    if t.is_string_literal()
        && !t.is_enum_literal()
        && uniques.insert(string_literal_value(t).to_string())
    {
        return vec![t];
    }
    Vec::new()
}

// string_completions.go:1943
fn get_already_used_types_in_string_literal_union(union: P<Node>, current: P<Node>) -> Vec<String> {
    // Go checks the type list for nil; the parser always creates it.
    let types_list = union.as_union_type_node().types();
    let mut values: Vec<String> = Vec::new();
    values.extend(types_list.nodes().iter().filter_map(|&type_node| {
        if type_node != current
            && ast::is_literal_type_node(type_node)
            && ast::is_string_literal(type_node.as_literal_type_node().literal)
        {
            Some(type_node.as_literal_type_node().literal.text().to_string())
        } else {
            None
        }
    }));
    values
}

// string_completions.go:1958
fn has_index_signature(t: P<Type>, type_checker: &mut Checker) -> bool {
    type_checker.get_string_index_type(t).is_some()
        || type_checker.get_number_index_type(t).is_some()
}

// Matches
//
//	require(""
//	require("")
// string_completions.go:1966
fn is_require_call_argument(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    ast::is_call_expression(parent)
        && !parent.arguments().is_empty()
        && parent.arguments()[0] == node
        && ast::is_identifier(parent.expression().unwrap())
        && parent.expression().unwrap().text() == "require"
}

// string_completions.go:1971
fn kind_modifiers_from_extension(extension: &str) -> ScriptElementKindModifier {
    match extension {
        tspath::EXTENSION_DTS => ScriptElementKindModifier::Dts,
        tspath::EXTENSION_JS => ScriptElementKindModifier::Js,
        tspath::EXTENSION_JSON => ScriptElementKindModifier::Json,
        tspath::EXTENSION_JSX => ScriptElementKindModifier::Jsx,
        tspath::EXTENSION_TS => ScriptElementKindModifier::Ts,
        tspath::EXTENSION_TSX => ScriptElementKindModifier::Tsx,
        tspath::EXTENSION_DMTS => ScriptElementKindModifier::Dmts,
        tspath::EXTENSION_MJS => ScriptElementKindModifier::Mjs,
        tspath::EXTENSION_MTS => ScriptElementKindModifier::Mts,
        tspath::EXTENSION_DCTS => ScriptElementKindModifier::Dcts,
        tspath::EXTENSION_CJS => ScriptElementKindModifier::Cjs,
        tspath::EXTENSION_CTS => ScriptElementKindModifier::Cts,
        tspath::EXTENSION_TS_BUILD_INFO => {
            panic!("Extension {} is unsupported.", tspath::EXTENSION_TS_BUILD_INFO)
        }
        _ => ScriptElementKindModifier::None,
    }
}

// string_completions.go:2004
fn get_string_literal_completions_from_signature(
    call: P<Node>,
    arg: P<Node>,
    argument_info: &ArgumentInfoForCompletions,
    type_checker: &mut Checker,
) -> Option<CompletionsFromTypes> {
    let mut is_new_identifier = false;
    let mut uniques: FxHashSet<String> = FxHashSet::default();
    let editing_argument = if ast::is_jsx_opening_like_element(call) {
        let Some(editing_argument) = ast::find_ancestor(arg.parent(), ast::is_jsx_attribute) else {
            panic!("Expected jsx opening-like element to have a jsx attribute as ancestor.");
        };
        editing_argument
    } else {
        arg
    };
    let candidates = type_checker
        .get_candidate_signatures_for_string_literal_completions(call, editing_argument);
    let mut types: Vec<P<Type>> = Vec::new();
    for candidate in candidates {
        if !candidate.has_rest_parameter()
            && argument_info.argument_count > candidate.parameters().len()
        {
            continue;
        }
        let mut t =
            type_checker.get_type_parameter_at_position(candidate, argument_info.argument_index);
        if ast::is_jsx_opening_like_element(call) {
            let prop_type = type_checker.get_type_of_property_of_type_exported(
                t,
                editing_argument.as_jsx_attribute().name().text(),
            );
            if let Some(prop_type) = prop_type {
                t = prop_type;
            }
        }
        is_new_identifier = is_new_identifier || t.is_string();
        types.extend(get_string_literal_types(Some(t), Some(&mut uniques), type_checker));
    }
    if !types.is_empty() {
        return Some(CompletionsFromTypes { types, is_new_identifier });
    }
    None
}

impl LanguageService {
    // string_completions.go:2046
    pub(crate) fn get_string_literal_completion_details(
        &self,
        ctx: &Context,
        checker: &mut Checker,
        item: lsproto::CompletionItem,
        name: &str,
        file: P<SourceFile>,
        position: u32,
        context_token: Option<P<Node>>,
        doc_format: lsproto::MarkupKind,
    ) -> lsproto::CompletionItem {
        let Some(context_token) = context_token.filter(|&ct| ast::is_string_literal_like(ct))
        else {
            return item;
        };
        let Some(completions) =
            self.get_string_literal_completion_entries(ctx, file, context_token, position, checker)
        else {
            return item;
        };
        self.string_literal_completion_details(
            item,
            name,
            context_token,
            position,
            &completions,
            file,
            checker,
            doc_format,
        )
    }

    // string_completions.go:2072
    fn string_literal_completion_details(
        &self,
        item: lsproto::CompletionItem,
        name: &str,
        location: P<Node>,
        position: u32,
        completion: &StringLiteralCompletions,
        _file: P<SourceFile>,
        checker: &mut Checker,
        doc_format: lsproto::MarkupKind,
    ) -> lsproto::CompletionItem {
        if completion.from_paths.is_some() {
            // Path completions have eagerly-resolved details so the client can show an accurate icon
            // for items of file kind based on the file extension provided in the item detail.
            return item;
        } else if let Some(properties) = &completion.from_properties {
            if let Some(&symbol) = properties.symbols.iter().find(|symbol| symbol.name() == name) {
                return self.create_completion_details_for_symbol(
                    item, symbol, checker, location, position, doc_format,
                );
            }
        } else if let Some(types) = &completion.from_types
            && types.types.iter().any(|&t| string_literal_value(t) == name)
        {
            return create_completion_details(item, name, "" /*documentation*/, doc_format);
        }
        item
    }
}

// string_completions.go:2105
fn is_in_reference_comment(file: P<SourceFile>, position: u32) -> bool {
    let Some(comment_range) =
        is_in_comment(file, position, astnav::get_token_at_position(file, position))
    else {
        return false;
    };
    let comment_text = &file.text()[comment_range.pos() as usize..comment_range.end() as usize];
    has_triple_slash_prefix(comment_text)
}

// string_completions.go:2114
fn has_triple_slash_prefix(comment_text: &str) -> bool {
    comment_text.starts_with("///") && comment_text[3..].trim().starts_with('<')
}

// Matches a triple slash reference directive with an incomplete string literal for its path.
// Used to determine if the caret is currently within the string literal and capture the literal
// fragment for completions.
// For example, this matches
//
// /// <reference path="fragment
//
// but not
//
// /// <reference path="fragment"

// Returns (prefix, kind, toComplete, ok) where:
//   - prefix is everything up to and including the opening quote
//   - kind is either "path" or "types"
//   - toComplete is the fragment after the opening quote
//   - ok indicates whether the match was successful
// string_completions.go:2134
fn parse_triple_slash_directive_fragment(text: &str) -> Option<(String, String, String)> {
    let is_ws = |c: char| stringutil::is_white_space_like(c as u32);
    let mut rest = text;
    if !rest.starts_with("///") {
        return None;
    }

    rest = &rest["///".len()..];
    rest = rest.trim_start_matches(is_ws);

    // <reference
    if !rest.starts_with("<reference") {
        return None;
    }
    rest = &rest["<reference".len()..];

    if rest.is_empty() || !stringutil::is_white_space_like(u32::from(rest.as_bytes()[0])) {
        return None;
    }
    rest = rest.trim_start_matches(is_ws);

    // path or types
    let kind;
    if rest.starts_with("path") {
        kind = "path";
        rest = &rest["path".len()..];
    } else if rest.starts_with("types") {
        kind = "types";
        rest = &rest["types".len()..];
    } else {
        return None;
    }

    // Skip optional whitespace, then must have "="
    rest = rest.trim_start_matches(is_ws);
    if !rest.starts_with('=') {
        return None;
    }
    rest = &rest[1..];

    // Skip optional whitespace, then must have opening quote (' or ")
    rest = rest.trim_start_matches(is_ws);
    if rest.is_empty() || (rest.as_bytes()[0] != b'\'' && rest.as_bytes()[0] != b'"') {
        return None;
    }
    rest = &rest[1..];

    // The toComplete part is everything after the opening quote
    if rest.contains(['\'', '"']) {
        return None;
    }
    let to_complete = rest;
    let prefix = &text[..text.len() - to_complete.len()];
    Some((prefix.to_string(), kind.to_string(), to_complete.to_string()))
}

impl LanguageService {
    // string_completions.go:2188
    fn get_triple_slash_reference_completions(
        &self,
        file: P<SourceFile>,
        position: u32,
        program: &'static Program,
        _checker: &mut Checker,
    ) -> Option<PathCompletions> {
        let compiler_options = program.options();
        let token = astnav::get_token_at_position(file, position);
        let comment_ranges: Vec<ast::CommentRange> = scanner::get_leading_comment_ranges(
            file.text(),
            token.pos().expect_source("parsed tokens have source positions"),
        )
        .collect();

        let found_range = comment_ranges.iter().find(|comment_range| {
            position >= comment_range.pos() && position <= comment_range.end()
        })?;

        let text = &file.text()[found_range.pos() as usize..position as usize];
        let (prefix, kind, to_complete) = parse_triple_slash_directive_fragment(text)?;
        let replacement_span = self.path_completion_replacement_span(
            file,
            get_directory_fragment_range(
                &to_complete,
                found_range.pos()
                    + u32::try_from(prefix.len()).expect("reference prefix exceeds u32"),
            ),
        )?;

        let script_path = tspath::get_directory_path(file.path());

        let mut names: Vec<ModuleCompletionNameAndKind> = Vec::new();
        match kind.as_str() {
            "path" => {
                let extension_options = self.get_extension_options(
                    &compiler_options,
                    ReferenceKind::FileName,
                    file,
                    RESOLUTION_MODE_NONE,
                    None, /*checker*/
                );
                let mut result = ModuleCompletionNameAndKindSet::default();
                self.get_completion_entries_for_directory_fragment(
                    &to_complete,
                    &script_path,
                    &extension_options,
                    program,
                    true, /*moduleSpecifierIsRelative*/
                    file.path(),
                    &mut result,
                );
                names = result.names.into_values().collect();
            }
            "types" => {
                let extension_options = self.get_extension_options(
                    &compiler_options,
                    ReferenceKind::ModuleSpecifier,
                    file,
                    RESOLUTION_MODE_NONE,
                    None, /*checker*/
                );
                let mut result = ModuleCompletionNameAndKindSet::default();
                self.get_completion_entries_from_typings(
                    program,
                    &script_path,
                    &get_fragment_directory(&to_complete),
                    &extension_options,
                    &mut result,
                );
                names = result.names.into_values().collect();
            }
            _ => {}
        }

        Some(PathCompletions { entries: to_path_completions(names), replacement_span })
    }
}
