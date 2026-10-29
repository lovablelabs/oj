// Go internal/ls (+ lsconv, lsutil, change), astnav, format, sourcemap (docs/LSP.md).

pub mod astnav;
pub mod autoimport;
mod autoinsert;
mod callhierarchy;
pub mod change;
mod codeactions;
mod codeactions_fixclassincorrectlyimplementsinterface;
mod codeactions_fixmissingtypeannotation;
mod codeactions_importfixes;
mod codeactions_missingmemberfixer;
mod codelens;
pub mod completions;
pub mod completions_2;
pub mod completions_3;
pub mod completions_4;
pub mod crossproject;
mod definition;
mod diagnostics;
mod displaypartswriter;
mod documenthighlights;
mod file_rename;
mod findallreferences;
mod folding;
pub mod format;
pub mod host;
mod hover;
mod hovericon;
mod import_tracker;
mod inlay_hints;
mod jsdoc;
mod jsdoc_snippet;
pub mod languageservice;
mod linkedediting;
pub mod lsconv;
mod lsformat;
pub mod lsutil;
mod organizeimports;
mod rename;
pub use tsrs_sourcemap as sourcemap;
mod selectionranges;
mod semantictokens;
mod signaturehelp;
mod source_map;
mod sourcedefinition;
pub mod spanmap;
pub mod string_completions;
mod symbols;
mod utilities;
mod visitedchildren;
pub use completions::{
    COMPLETION_TRIGGER_CHARACTERS, CompletionItem, CompletionList, is_err_needs_auto_imports,
};
pub use completions_3::compare_completion_entries;
pub use crossproject::*;
pub use findallreferences::{
    Definition, DefinitionKind, ReferenceEntry, SignatureUsage, SymbolAndEntries,
    SymbolAndEntriesData,
};
pub use host::*;
pub use jsdoc::{JSDocTagInfo, get_symbol_documentation_comment, get_symbol_jsdoc_tags};
pub use languageservice::*;
pub use rename::{RenameInfo, client_supports_document_changes, client_supports_will_rename_files};
pub use semantictokens::semantic_tokens_legend;
pub use signaturehelp::{SIGNATURE_HELP_RETRIGGER_CHARACTERS, SIGNATURE_HELP_TRIGGER_CHARACTERS};
pub use symbols::provide_workspace_symbols;
