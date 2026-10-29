// Symbol-centric handlers (Go tsc/internal/api/session.go: handleGetSymbol*, handleGetTypeOfSymbol*,
// handleResolveName, handleGetSymbolsInScope, symbol property handlers, alias/module/JSDoc handlers).

use std::cmp::Ordering;

use tsrs_ast::{Symbol, SymbolFlags};
use tsrs_core::P;
use tsrs_core::json::Value;

use super::host::{CachedFileScope, CheckerError, CheckerHost, CheckerResult};
use super::params::{Params, SYMBOL_OWNER_KIND_FILE, SYMBOL_OWNER_KIND_SNAPSHOT, SymbolReference};
use super::setup::{
    Setup, SnapshotCtx, file_symbol_response, symbol_owner_file, touching_property_name_at_utf16,
};
use super::symbol_index::source_file_symbol;

fn setup<'h>(host: &'h dyn CheckerHost, p: &Params) -> CheckerResult<Setup<'h>> {
    Setup::new(host, p.u64("snapshot")?, p.project()?)
}

pub fn get_symbol_at_position(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let node = touching_property_name_at_utf16(file, p.u32("position")?);
    let symbol = s.c().get_symbol_at_location_exported(node);
    s.opt_symbol_response(symbol)
}

pub fn get_symbols_at_positions(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let positions = p.u32_array("positions")?;
    let mut out = Vec::with_capacity(positions.len());
    for pos in positions {
        let node = touching_property_name_at_utf16(file, pos);
        let symbol = s.c().get_symbol_at_location_exported(node);
        out.push(s.opt_symbol_response(symbol)?);
    }
    Ok(Value::Array(out))
}

pub fn get_symbol_at_location(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let symbol = s.c().get_symbol_at_location_exported(node);
    s.opt_symbol_response(symbol)
}

pub fn get_symbols_at_locations(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let locations = p.string_array("locations")?;
    let mut out = Vec::with_capacity(locations.len());
    for loc in locations {
        let node = s.resolve_node(loc)?;
        let symbol = s.c().get_symbol_at_location_exported(node);
        out.push(s.opt_symbol_response(symbol)?);
    }
    Ok(Value::Array(out))
}

pub fn get_symbol_of_source_file(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let symbol = s.c().get_symbol_at_location_exported(file.as_node());
    s.opt_symbol_response(symbol)
}

pub fn get_symbols_of_source_files(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let files = p.documents("files")?;
    let mut out = Vec::with_capacity(files.len());
    for f in &files {
        let file = s.source_file(f)?;
        let symbol = s.c().get_symbol_at_location_exported(file.as_node());
        out.push(s.opt_symbol_response(symbol)?);
    }
    Ok(Value::Array(out))
}

#[derive(Clone, Copy)]
pub enum SymbolTypeQuery {
    TypeOf,
    DeclaredType,
    NonMissingType,
}

pub fn get_type_of_symbol(
    host: &dyn CheckerHost,
    p: &Params,
    query: SymbolTypeQuery,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let t = match query {
        SymbolTypeQuery::TypeOf => s.c().get_type_of_symbol_exported(symbol),
        SymbolTypeQuery::DeclaredType => s.c().get_declared_type_of_symbol_exported(symbol),
        SymbolTypeQuery::NonMissingType => s.c().get_non_missing_type_of_symbol_exported(symbol),
    };
    s.type_response(t)
}

pub fn get_types_of_symbols(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let refs = p.symbol_refs("symbols")?;
    let mut out = Vec::with_capacity(refs.len());
    for r in &refs {
        let symbol = s.resolve_symbol(r)?;
        let t = s.c().get_type_of_symbol_exported(symbol);
        out.push(s.type_response(t)?);
    }
    Ok(Value::Array(out))
}

pub fn resolve_name(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = p.opt_document("file")?;
    let location =
        s.resolve_location(p.string("location")?, file.as_ref(), p.opt_u32("position")?)?;
    let meaning = SymbolFlags::from_bits_retain(p.u32("meaning")?);
    let name = p.string("name")?;
    let exclude_globals = p.bool("excludeGlobals")?;
    // Go `Checker.ResolveName(name, location, meaning, excludeGlobals)` accepts a nil location.
    let symbol = s.c().resolve_name_exported(name, location, meaning, exclude_globals);
    s.opt_symbol_response(symbol)
}

pub fn get_symbols_in_scope(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = p.opt_document("file")?;
    let location =
        s.resolve_location(p.string("location")?, file.as_ref(), p.opt_u32("position")?)?;
    let Some(location) = location else {
        return Err(CheckerError::client("getSymbolsInScope requires a location"));
    };
    let meaning = SymbolFlags::from_bits_retain(p.u32("meaning")?);
    let symbols = s.c().get_symbols_in_scope_exported(location, meaning);
    s.symbols_response(&symbols)
}

pub fn get_shorthand_assignment_value_symbol(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let symbol = s.c().get_shorthand_assignment_value_symbol(Some(node));
    s.opt_symbol_response(symbol)
}

pub fn get_export_specifier_local_target_symbol(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let symbol = s.c().get_export_specifier_local_target_symbol(node);
    s.opt_symbol_response(symbol)
}

#[derive(Clone, Copy)]
pub enum AliasQuery {
    Aliased,
    ImmediateAliased,
    Target,
    Merged,
    Parent,
    ExportSymbol,
}

pub fn alias_query(host: &dyn CheckerHost, p: &Params, query: AliasQuery) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let result = match query {
        AliasQuery::Aliased => Some(s.c().get_aliased_symbol(symbol)),
        AliasQuery::ImmediateAliased => s.c().get_immediate_aliased_symbol_exported(symbol),
        AliasQuery::Target => s.c().get_target_symbol_exported(symbol),
        AliasQuery::Merged => Some(s.c().get_merged_symbol_exported(symbol)),
        AliasQuery::Parent => s.c().get_parent_of_symbol_exported(symbol),
        AliasQuery::ExportSymbol => Some(s.c().get_export_symbol_of_symbol(symbol)),
    };
    s.opt_symbol_response(result)
}

pub fn get_symbol_of_node(
    host: &dyn CheckerHost,
    p: &Params,
    declaration: bool,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let symbol = if declaration {
        s.c().get_symbol_of_declaration_exported(node)
    } else {
        s.c().get_symbol_of_node_exported(node)
    };
    s.opt_symbol_response(symbol)
}

pub fn get_fully_qualified_name(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    Ok(Value::String(s.c().get_fully_qualified_name_exported(symbol)))
}

pub fn get_exports_of_module(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let mut exports = s.c().get_exports_of_module_exported(symbol);
    if exports.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    sort_by_checker(&mut s, &mut exports);
    s.symbols_response(&exports)
}

pub fn get_member_in_module_exports(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let member = s.c().try_get_member_in_module_exports(p.string("name")?, symbol);
    s.opt_symbol_response(member)
}

pub fn get_jsdoc_tags(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let tags = tsrs_ls::get_symbol_jsdoc_tags(Some(symbol));
    if tags.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    let mut out = Vec::with_capacity(tags.len());
    out.extend(tags.into_iter().map(|tag| {
        let mut o = super::json::obj();
        o.set("name", Value::String(tag.name));
        o.str_nonempty("text", &tag.text);
        o.build()
    }));
    Ok(Value::Array(out))
}

pub fn get_documentation_comment(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    Ok(Value::String(tsrs_ls::get_symbol_documentation_comment(s.c(), Some(symbol))))
}

pub fn is_readonly_symbol(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    Ok(Value::Bool(s.c().is_readonly_symbol_exported(symbol)))
}

/// Go `handleGetReferencesToSymbolInFile`.
pub fn get_references_to_symbol_in_file(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let refs = s.c().get_references_to_symbol_in_file(file, symbol);
    Ok(Value::Array(
        refs.into_iter()
            .map(|n| host.node_handle(n).map(Value::String))
            .collect::<CheckerResult<_>>()?,
    ))
}

/// `slices.SortFunc(symbols, checker.CompareSymbols)`.
fn sort_by_checker(s: &mut Setup, symbols: &mut [P<Symbol>]) {
    let c = s.c();
    symbols.sort_by(|a, b| c.compare_symbols_exported(Some(*a), Some(*b)));
}

// --- Symbol property handlers that Go serves without a checker (resolveSymbolReference) ---

/// The owner a context-free symbol reference resolved through (Go `resolveSymbolReference`).
enum Owner<'h> {
    File(
        #[expect(
            dead_code,
            reason = "held for its Drop: the lease keeps the cached file retained while the symbol is used"
        )]
        CachedFileScope,
    ),
    Snapshot(SnapshotCtx<'h>),
}

impl Owner<'_> {
    fn respond(&self, host: &dyn CheckerHost, symbol: P<Symbol>) -> CheckerResult<Value> {
        match self {
            Owner::File(_) => file_owned_response(host, symbol),
            Owner::Snapshot(sd) => sd.symbol_response(symbol, &sd.project),
        }
    }
}

fn file_owned_response(host: &dyn CheckerHost, symbol: P<Symbol>) -> CheckerResult<Value> {
    if symbol_owner_file(symbol).is_none() {
        return Err(CheckerError::client(
            "symbol related to a file-owned symbol is not file-owned",
        ));
    }
    file_symbol_response(host, symbol)
}

fn resolve_symbol_reference<'h>(
    host: &'h dyn CheckerHost,
    r: &SymbolReference,
) -> CheckerResult<(P<Symbol>, Owner<'h>)> {
    match r.kind {
        SYMBOL_OWNER_KIND_FILE => {
            let Some(descriptor) =
                r.file.as_ref().filter(|_| r.snapshot == 0 && r.project.is_empty())
            else {
                return Err(CheckerError::client("invalid file symbol reference"));
            };
            let lease = host.acquire_cached_source_file(descriptor)?;
            let symbol = source_file_symbol(lease.file, r.id).ok_or_else(|| {
                CheckerError::client(format!("symbol {} not found in source file", r.id))
            })?;
            Ok((symbol, Owner::File(lease)))
        }
        SYMBOL_OWNER_KIND_SNAPSHOT => {
            if r.file.is_some() || r.snapshot == 0 || r.project.is_empty() {
                return Err(CheckerError::client("invalid snapshot symbol reference"));
            }
            let sd = SnapshotCtx::new(host, r.snapshot, r.project)?;
            let symbol = sd.scope.registry.resolve_symbol(r.id)?;
            Ok((symbol, Owner::Snapshot(sd)))
        }
        kind => Err(CheckerError::client(format!("invalid symbol reference kind {kind}"))),
    }
}

#[derive(Clone, Copy)]
pub enum SymbolProperty {
    Parent,
    ExportSymbol,
}

/// Go `resolveSymbolPropertyOfSymbol`.
pub fn symbol_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: SymbolProperty,
) -> CheckerResult<Value> {
    let (symbol, owner) = resolve_symbol_reference(host, &p.symbol_ref("symbol")?)?;
    let result = match property {
        SymbolProperty::Parent => symbol.parent(),
        SymbolProperty::ExportSymbol => symbol.export_symbol(),
    };
    match result {
        Some(result) => owner.respond(host, result),
        None => Ok(Value::Null),
    }
}

#[derive(Clone, Copy)]
pub enum SymbolTableProperty {
    Members,
    Exports,
}

/// Go `resolveSymbolTablePropertyOfSymbol`.
pub fn symbol_table_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: SymbolTableProperty,
) -> CheckerResult<Value> {
    let r = p.symbol_ref("symbol")?;
    let (symbol, owner) = resolve_symbol_reference(host, &r)?;
    let table = match property {
        SymbolTableProperty::Members => symbol.members(),
        SymbolTableProperty::Exports => symbol.exports(),
    };
    let Some(table) = table.filter(|t| !t.is_empty()) else {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    };
    let mut symbols = table.values();
    if symbols.len() == 1 {
        return Ok(Value::Array(vec![owner.respond(host, symbols[0])?]));
    }
    match owner {
        Owner::File(_) => {
            // Binder tables of a file-owned symbol only contain symbols from the same file, so they can
            // be ordered by declaration position without a checker.
            symbols.sort_by(compare_file_symbols);
            Ok(Value::Array(
                symbols
                    .into_iter()
                    .map(|s| file_owned_response(host, s))
                    .collect::<CheckerResult<_>>()?,
            ))
        }
        Owner::Snapshot(sd) => {
            drop(sd);
            // Tables of snapshot-owned symbols may contain symbols from several files: checker ordering.
            let mut s = Setup::new(host, r.snapshot, r.project)?;
            sort_by_checker(&mut s, &mut symbols);
            s.symbols_response(&symbols)
        }
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "passed to sort_by, which hands the comparator references"
)]
fn compare_file_symbols(left: &P<Symbol>, right: &P<Symbol>) -> Ordering {
    let (ld, rd) = (left.declarations(), right.declarations());
    if ld.is_empty() != rd.is_empty() {
        return if ld.is_empty() { Ordering::Greater } else { Ordering::Less };
    }
    if !ld.is_empty() {
        let order = ld[0].pos().cmp(&rd[0].pos());
        if order != Ordering::Equal {
            return order;
        }
    }
    // Go compares the (escaped-name) strings bytewise.
    left.name()
        .cmp(right.name())
        .then_with(|| tsrs_ast::get_symbol_id(*left).0.cmp(&tsrs_ast::get_symbol_id(*right).0))
}
