// Go `getSourceFileSymbolIndex` (tsc/internal/api/session.go): the file-owned (non-transient) binder
// symbols reachable from a source file's nodes, locals, global exports and pattern ambient modules,
// plus their parents, export symbols, members and exports. Used to resolve file-owned symbol
// references (`SymbolOwnerKindFile`) by id. Like Go, the index is computed once per source file.

use rustc_hash::{FxBuildHasher, FxHashMap};
use tsrs_ast::{Node, SourceFile, Symbol, SymbolFlags};
use tsrs_core::P;

struct Index {
    file: P<SourceFile>,
    symbols: FxHashMap<u64, P<Symbol>>,
}

impl Index {
    fn add(&mut self, symbol: Option<P<Symbol>>) {
        let Some(symbol) = symbol else { return };
        if symbol.flags().intersects(SymbolFlags::Transient) {
            return;
        }
        if tsrs_ast::get_source_file_of_symbol(symbol) != Some(self.file) {
            // Go asserts this; a foreign symbol is never part of this file's index.
            return;
        }
        let id = tsrs_ast::get_symbol_id(symbol).0;
        if self.symbols.insert(id, symbol).is_some() {
            return;
        }
        self.add(symbol.parent());
        self.add(symbol.export_symbol());
        for table in [symbol.members(), symbol.exports()].into_iter().flatten() {
            for child in table.values() {
                self.add(Some(child));
            }
        }
    }

    fn visit(&mut self, node: P<Node>) {
        self.add(node.symbol());
        self.add(node.local_symbol());
        if let Some(locals) = node.locals() {
            for symbol in locals.values() {
                self.add(Some(symbol));
            }
        }
        node.for_each_child(&mut |child: P<Node>| {
            self.visit(child);
            false
        });
        for jsdoc in node.jsdoc(Some(self.file.get())) {
            self.visit(*jsdoc);
        }
    }
}

/// The file-owned symbol with `id` in `file`, if any.
pub fn source_file_symbol(file: P<SourceFile>, id: u64) -> Option<P<Symbol>> {
    file.get_or_compute_api_symbol_index(|| compute_index(file)).get(&id).copied()
}

fn compute_index(file: P<SourceFile>) -> FxHashMap<u64, P<Symbol>> {
    let capacity = file.symbol_count.get();
    let mut index =
        Index { file, symbols: FxHashMap::with_capacity_and_hasher(capacity, FxBuildHasher) };
    index.visit(file.as_node());
    if let Some(globals) = file.global_exports() {
        for symbol in globals.values() {
            index.add(Some(symbol));
        }
    }
    for module in file.pattern_ambient_modules() {
        index.add(Some(module.symbol));
    }
    index.symbols
}
